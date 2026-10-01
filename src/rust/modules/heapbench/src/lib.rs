#![no_std]

//! heapbench: what the kernel heap costs the Rust code that allocates from
//! it. A `Box`, a `Vec`, a `String` -- in the image and in a module alike --
//! reach `kernel_alloc` through the global allocator, and so does every
//! allocation here.
//!
//!     insmod /heapbench.ko
//!     heapbench                  every workload, on one CPU and on all
//!     heapbench cpus=2 ms=500    on one and on two, half a second a run
//!
//! A workload runs on one CPU, then on N at once, each worker a task bound
//! to a CPU of its own, and the report is the time an operation took as the
//! workers saw it: an allocation and its free for `pair` and `batch`, a
//! vector grown from nothing to 64 KiB in 512-byte pieces and dropped for
//! `grow`. `pass` is a check as much as a measure: each worker fills a
//! batch of blocks with a byte of its own, hands it to the next worker's
//! mailbox and checks and frees the batch in its own -- every block freed on
//! another CPU than the one it was taken on, and one handed to two owners
//! found by the byte it no longer holds. The sizes are the ones the heap treats differently -- its pools'
//! classes, the edge past which a pool block has a page to itself, whole
//! pages and runs of them -- so the line that stands out says which of its
//! paths it was. Nothing here panics on a refusal: an allocation the heap
//! refuses ends the run and says so.

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::hint::black_box;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use kcore::cmd::{Command, Output};
use kcore::sync::PreemptSpinLock;
use kcore::time::boot_time_ns;

const HELP: &str = "heapbench [cpus=N] [ms=200] - time the kernel heap as Rust code reaches it";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const NS_PER_MS: u64 = 1_000_000;
const DEFAULT_MS: u64 = 200;
const MAX_MS: u64 = 10_000;

/// How many allocations a `batch` round holds at once before freeing them
const BATCH: usize = 64;
/// What `grow` grows a vector to, and the piece it adds at a time
const GROW_TO: usize = 64 * 1024;
const GROW_STEP: usize = 512;
/// Rounds between two looks at the clock
const ROUNDS_PER_CHECK: u64 = 16;

#[derive(Clone, Copy)]
enum Workload {
    /// An allocation of this many bytes and its free, one after the other
    Pair(usize),
    /// BATCH allocations of this many bytes, then their frees
    Batch(usize),
    /// A vector grown to GROW_TO bytes, GROW_STEP at a time, and dropped
    Grow,
    /// BATCH blocks of this many bytes, filled and handed to the next
    /// worker; the batch handed to this one checked and freed
    Pass(usize),
}

/* 16 to 1024: pool classes. 1536 and 2032: the largest class, whose block
   takes a page to itself. 2048 and up: the page allocator's runs. */
const WORKLOADS: [Workload; 16] = [
    Workload::Pair(16),
    Workload::Pair(64),
    Workload::Pair(256),
    Workload::Pair(1024),
    Workload::Pair(1536),
    Workload::Pair(2032),
    Workload::Pair(4096),
    Workload::Pair(16 * 1024),
    Workload::Pair(64 * 1024),
    Workload::Pair(256 * 1024),
    Workload::Batch(64),
    Workload::Batch(512),
    Workload::Batch(1536),
    Workload::Batch(4096),
    Workload::Grow,
    Workload::Pass(256),
];

impl Workload {
    fn name(&self) -> String {
        match self {
            Workload::Pair(size) => format!("pair {}", size),
            Workload::Batch(size) => format!("batch {}", size),
            Workload::Grow => format!("grow {}k", GROW_TO / 1024),
            Workload::Pass(size) => format!("pass {}", size),
        }
    }

    /// One round: the operations it did, or None if the heap refused one
    fn round(&self, held: &mut Vec<Vec<u8>>, pass: &mut Pass) -> Option<u64> {
        match *self {
            Workload::Pair(size) => {
                let v = filled(size)?;
                black_box(&v);
                Some(1)
            }
            Workload::Batch(size) => {
                for _ in 0..BATCH {
                    /* Within the capacity reserved before the run: the push
                       itself allocates nothing */
                    held.push(filled(size)?);
                }
                black_box(&*held);
                held.clear();
                Some(BATCH as u64)
            }
            Workload::Grow => {
                let piece = [1u8; GROW_STEP];
                let mut v: Vec<u8> = Vec::new();
                while v.len() < GROW_TO {
                    v.try_reserve(GROW_STEP).ok()?;
                    v.extend_from_slice(&piece);
                }
                black_box(&v);
                Some(1)
            }
            Workload::Pass(size) => pass.round(size, held),
        }
    }
}

/// A worker's side of `pass`: whose mailbox it fills, and the empty batch
/// it swaps for a full one, so that a swap under the mailbox's lock moves
/// two vectors and allocates nothing
struct Pass {
    run: Arc<Run>,
    slot: usize,
    inbox: Vec<Vec<u8>>,
    round: u8,
}

impl Pass {
    fn round(&mut self, size: usize, held: &mut Vec<Vec<u8>>) -> Option<u64> {
        let workers = self.run.mailboxes.len();
        self.round = self.round.wrapping_add(1);
        /* Never 0: a block of zeroes is what a page fresh from the
           allocator holds, and would pass a check it should fail */
        let mark = ((self.slot as u8).wrapping_mul(31) ^ self.round) | 1;
        for _ in 0..BATCH {
            let mut v = Vec::new();
            v.try_reserve_exact(size).ok()?;
            v.resize(size, mark);
            held.push(v);
        }

        let mut freed = 0u64;
        {
            let mut next = self.run.mailboxes[(self.slot + 1) % workers].lock();
            if next.is_empty() {
                core::mem::swap(&mut *next, held);
            }
        }
        /* The next worker had not taken the last batch yet: these go back
           here */
        freed += held.len() as u64;
        held.clear();

        {
            let mut mine = self.run.mailboxes[self.slot].lock();
            core::mem::swap(&mut *mine, &mut self.inbox);
        }
        for v in self.inbox.iter() {
            if v.len() != size || v.iter().any(|&b| b != v[0]) || v[0] == 0 {
                self.run.corrupt.store(true, Ordering::Relaxed);
            }
        }
        freed += self.inbox.len() as u64;
        self.inbox.clear();
        Some(freed)
    }
}

/// A vector of `size` bytes' capacity with a byte written into it: an
/// allocation the optimiser cannot take away, as its address escapes
fn filled(size: usize) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(size).ok()?;
    v.push(1);
    Some(v)
}

/// One run of a workload on some CPUs: what the workers share
struct Run {
    workload: Workload,
    deadline_ms: u64,
    ready: AtomicUsize,
    go: AtomicBool,
    refused: AtomicBool,
    /// A block of `pass` that came back holding other than what was put in
    corrupt: AtomicBool,
    /// `pass`'s, a worker's each; empty for every other workload
    mailboxes: Vec<PreemptSpinLock<Vec<Vec<u8>>>>,
    ops: Vec<AtomicU64>,
    ns: Vec<AtomicU64>,
}

fn worker((run, slot): (Arc<Run>, usize)) {
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut inbox: Vec<Vec<u8>> = Vec::new();
    let reserved = held.try_reserve_exact(BATCH).is_ok() && inbox.try_reserve_exact(BATCH).is_ok();
    let mut pass = Pass { run: run.clone(), slot, inbox, round: 0 };
    run.ready.fetch_add(1, Ordering::AcqRel);
    /* Yielding, not spinning: the task that sets `go` may be waiting for
       this CPU */
    while !run.go.load(Ordering::Acquire) {
        kcore::task::yield_to_runnable();
    }
    if !reserved {
        run.refused.store(true, Ordering::Relaxed);
        return;
    }

    let start = boot_time_ns();
    let deadline = start + run.deadline_ms * NS_PER_MS;
    let mut ops = 0u64;
    'run: loop {
        for _ in 0..ROUNDS_PER_CHECK {
            match run.workload.round(&mut held, &mut pass) {
                Some(n) => ops += n,
                None => {
                    run.refused.store(true, Ordering::Relaxed);
                    break 'run;
                }
            }
        }
        if boot_time_ns() >= deadline {
            break;
        }
    }
    run.ns[slot].store(boot_time_ns() - start, Ordering::Relaxed);
    run.ops[slot].store(ops, Ordering::Relaxed);
}

/// What one run measured
struct Measured {
    /// Nanoseconds an operation took, in tenths, as the workers saw it
    tenths_ns: u64,
    /// Operations a second, all workers together
    per_sec: u64,
}

fn measure(workload: Workload, cpus: &[u32], ms: u64) -> Result<Measured, String> {
    let n = cpus.len();
    let no_memory = |_| String::from("no memory for the run");
    let mut ops = Vec::new();
    let mut ns = Vec::new();
    let mut mailboxes = Vec::new();
    ops.try_reserve_exact(n).map_err(no_memory)?;
    ns.try_reserve_exact(n).map_err(no_memory)?;
    for _ in 0..n {
        ops.push(AtomicU64::new(0));
        ns.push(AtomicU64::new(0));
    }
    if let Workload::Pass(_) = workload {
        mailboxes.try_reserve_exact(n).map_err(no_memory)?;
        for _ in 0..n {
            let mut mailbox = Vec::new();
            mailbox.try_reserve_exact(BATCH).map_err(no_memory)?;
            mailboxes.push(PreemptSpinLock::new(mailbox));
        }
    }
    let run = Arc::new(Run {
        workload,
        deadline_ms: ms,
        ready: AtomicUsize::new(0),
        go: AtomicBool::new(false),
        refused: AtomicBool::new(false),
        corrupt: AtomicBool::new(false),
        mailboxes,
        ops,
        ns,
    });

    /* Dropping a TaskHandle waits for its task, so every worker is done by
       the end of this block */
    let started = {
        let mut tasks = Vec::new();
        for (slot, &cpu) in cpus.iter().enumerate() {
            let name = format!("heapbench/{}", cpu);
            match kcore::task::spawn_on_with(&name, 1u64 << cpu, (run.clone(), slot), worker) {
                Some(task) => tasks.push(task),
                None => break,
            }
        }
        let started = tasks.len();
        /* All of them spinning on `go` before any starts, so the N-CPU runs
           overlap for their whole length */
        while run.ready.load(Ordering::Acquire) < started {
            kcore::task::yield_to_runnable();
        }
        run.go.store(true, Ordering::Release);
        started
    };

    if started != n {
        return Err(format!("could start only {} of {} tasks", started, n));
    }
    if run.refused.load(Ordering::Relaxed) {
        return Err(String::from("the heap refused an allocation"));
    }
    if run.corrupt.load(Ordering::Relaxed) {
        return Err(String::from("CORRUPT: a block passed between CPUs came back changed -- one block, two owners"));
    }

    let total_ops: u64 = run.ops.iter().map(|o| o.load(Ordering::Relaxed)).sum();
    let total_ns: u64 = run.ns.iter().map(|t| t.load(Ordering::Relaxed)).sum();
    let longest = run.ns.iter().map(|t| t.load(Ordering::Relaxed)).max().unwrap_or(0).max(1);
    if total_ops == 0 {
        return Err(String::from("no operation finished"));
    }
    Ok(Measured {
        tenths_ns: (total_ns as u128 * 10 / total_ops as u128) as u64,
        per_sec: (total_ops as u128 * 1_000_000_000 / longest as u128) as u64,
    })
}

struct Options {
    cpus: Option<usize>,
    ms: u64,
}

fn parse(args: &str) -> Result<Options, String> {
    let mut opts = Options { cpus: None, ms: DEFAULT_MS };
    for word in args.split_whitespace() {
        let value = |v: &str| v.parse::<u64>().map_err(|_| format!("{}: not a number", word));
        match word.split_once('=') {
            Some(("cpus", v)) => opts.cpus = Some(value(v)? as usize),
            Some(("ms", v)) => opts.ms = value(v)?,
            _ => return Err(format!("what is '{}'?\nusage: {}", word, HELP)),
        }
    }
    if opts.ms == 0 || opts.ms > MAX_MS {
        return Err(format!("ms={} is outside 1..={}", opts.ms, MAX_MS));
    }
    Ok(opts)
}

fn tenths(value: u64) -> String {
    format!("{}.{}", value / 10, value % 10)
}

fn run(args: &str, out: &mut Output) -> Result<(), String> {
    let opts = parse(args)?;
    let online = kcore::cpu::online_mask();
    let all: Vec<u32> = (0..u64::BITS).filter(|&cpu| online & (1u64 << cpu) != 0).collect();
    let n = opts.cpus.unwrap_or(all.len());
    if n == 0 || n > all.len() {
        return Err(format!("cpus={} with {} online", n, all.len()));
    }
    let many = &all[..n];
    /* The single-CPU run on the last of them: the first takes more of the
       machine's interrupts */
    let one = &all[n - 1..n];

    let _ = writeln!(out, "heapbench: {} ms a run, on 1 CPU and on {}", opts.ms, n);
    let _ = writeln!(out, "  {:<12} {:>12} {:>12} {:>14}", "workload", "ns/op 1cpu",
        format!("ns/op {}cpu", n), format!("Mops/s {}cpu", n));
    for workload in WORKLOADS {
        let alone = measure(workload, one, opts.ms)?;
        let together = if n > 1 { Some(measure(workload, many, opts.ms)?) } else { None };
        let (ns_many, rate) = match &together {
            Some(m) => (tenths(m.tenths_ns), tenths(m.per_sec / 100_000)),
            None => (String::from("-"), tenths(alone.per_sec / 100_000)),
        };
        let _ = writeln!(out, "  {:<12} {:>12} {:>12} {:>14}", workload.name(), tenths(alone.tenths_ns),
            ns_many, rate);
    }
    Ok(())
}

struct HeapBench {
    _cmd: Command,
}

impl kmod::Module for HeapBench {}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let cmd = Command::register("heapbench", HELP, |args, out| {
        if let Err(problem) = run(args, out) {
            let _ = writeln!(out, "heapbench: {}", problem);
        }
    })?;

    Ok(Box::new(HeapBench { _cmd: cmd }))
}

kmod::module!(name: "heapbench", init: init);
