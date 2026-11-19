#![no_std]

//! numabench: what a NUMA node's memory costs the CPUs of each node -- the
//! difference the kernel's placement by node is for (`kcore::numa`): a page
//! of the node a task runs on, and a guest's memory on the node of its CPUs.
//!
//!     insmod /lib/modules/numabench.ko
//!     numabench                   256 MiB a node, 4 million loads
//!     numabench mib=64 loads=1000000
//!
//! For each node, a task bound to one of its CPUs takes `mib` of memory from
//! the heap -- its node's, as every allocation is the node of the CPU it
//! runs on first -- in blocks of the heap's largest size, and threads a
//! random cycle through its cache lines in place, a line's first word naming
//! the next. Every node's is taken before any is read, and given back after
//! the last read: a block freed is kept by the heap for its next taker,
//! whatever node that one is on. Then, for each node, a task bound to one of
//! its CPUs reads each node's memory twice: the cycle followed `loads`
//! lines, each load waiting for the one before -- the latency of a line from
//! that memory as a task meets it, TLB misses among it, no prefetcher to
//! hide it -- and every word of it read in order, as many passes as fill a
//! second at least: its bandwidth to one CPU.
//!
//! What it says: a line a pair -- the CPUs' node, the memory's -- and the
//! share of the memory's pages that were its node's, looked up by their
//! physical addresses: what the allocator was asked to do, checked.

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::hint::black_box;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kcore::cmd::{Command, Output};
use kcore::sync::Mutex;
use kcore::time::boot_time_ns;

const HELP: &str = "numabench [mib=256] [loads=4000000] - each node's memory as each node's CPUs reach it";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const PAGE: usize = 4096;
const LINE: usize = 64;
const WORDS_PER_LINE: usize = LINE / 8;
/// A block of the memory: the heap's largest, 128 pages -- it refuses more
const PIECE: usize = 128 * PAGE;
const WORDS_PER_PIECE: usize = PIECE / 8;
const LINES_PER_PIECE: usize = PIECE / LINE;
const MIB: usize = 1 << 20;
/// Past the last-level cache of the parts it was written for: a line read
/// is the memory's, not the cache's
const DEFAULT_MIB: usize = 256;
const MAX_MIB: usize = 4096;
const DEFAULT_LOADS: u64 = 4_000_000;
const MAX_LOADS: u64 = 1_000_000_000;
/// The first lap, before the clock: the loop warm
const WARM_LOADS: u64 = 100_000;
/// The bandwidth's reads go on for this long at least, whole passes
const READ_NS: u64 = 1_000_000_000;
const NS_PER_SEC: u64 = 1_000_000_000;
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// A node's memory: its blocks, a cycle through their lines threaded in
/// them, and how many of its pages were the node's
struct Memory {
    pieces: Vec<Box<[u64]>>,
    pages: u64,
    own_pages: u64,
}

impl Memory {
    /// The line after `line` in the cycle: its first word
    #[inline]
    fn next(&self, line: usize) -> usize {
        self.pieces[line / LINES_PER_PIECE][(line % LINES_PER_PIECE) * WORDS_PER_LINE] as usize
    }
}

/// What a task is handed and hands back: the memory -- made by the node's
/// task, read by every node's -- and the measures
struct Work {
    node: usize,
    mib: usize,
    loads: u64,
    memory: Mutex<Option<Memory>>,
    refused: AtomicBool,
    /// Tenths of a nanosecond
    load_ns: AtomicU64,
    bytes_per_sec: AtomicU64,
}

/// xorshift64
fn next_random(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// The first word of line `line` of `pieces`
fn word(pieces: &mut [Box<[u64]>], line: usize) -> &mut u64 {
    &mut pieces[line / LINES_PER_PIECE][(line % LINES_PER_PIECE) * WORDS_PER_LINE]
}

/// Made on a CPU of the node: the blocks, every page's node looked up, and
/// a random cycle through every line
fn make(work: Arc<Work>) {
    let count = (work.mib * MIB).div_ceil(PIECE).max(1);
    let mut pieces = Vec::new();
    if pieces.try_reserve_exact(count).is_err() {
        work.refused.store(true, Ordering::Release);
        return;
    }
    for _ in 0..count {
        let mut piece = Vec::new();
        if piece.try_reserve_exact(WORDS_PER_PIECE).is_err() {
            work.refused.store(true, Ordering::Release);
            return;
        }
        piece.resize(WORDS_PER_PIECE, 0u64);
        pieces.push(piece.into_boxed_slice());
    }

    let mut pages = 0u64;
    let mut own_pages = 0u64;
    for piece in &pieces {
        let base = piece.as_ptr() as *const u8;
        for page in 0..PIECE / PAGE {
            pages += 1;
            let phys = kcore::dma::virt_to_phys(base.wrapping_add(page * PAGE));
            if phys != 0 && kcore::numa::phys_node(phys) == work.node {
                own_pages += 1;
            }
        }
    }

    /* Sattolo's shuffle, in place: every line naming itself, then each from
     * the last down swapped with one before it -- one cycle of them all, in
     * no order a prefetcher can follow */
    let lines = count * LINES_PER_PIECE;
    for line in 0..lines {
        *word(&mut pieces, line) = line as u64;
    }
    let mut x = SEED;
    for i in (1..lines).rev() {
        let j = (next_random(&mut x) % i as u64) as usize;
        let a = *word(&mut pieces, i);
        let b = *word(&mut pieces, j);
        *word(&mut pieces, i) = b;
        *word(&mut pieces, j) = a;
    }

    *work.memory.lock() = Some(Memory { pieces, pages, own_pages });
}

/// On a CPU of the reading node: the cycle followed, then the words read
fn read(work: Arc<Work>) {
    let guard = work.memory.lock();
    let Some(memory) = guard.as_ref() else {
        return;
    };

    let mut line = 0usize;
    for _ in 0..WARM_LOADS {
        line = memory.next(line);
    }
    let t0 = boot_time_ns();
    for _ in 0..work.loads {
        line = memory.next(line);
    }
    let t1 = boot_time_ns();
    black_box(line);
    work.load_ns.store((t1 - t0) * 10 / work.loads.max(1), Ordering::Release);

    let bytes = (memory.pieces.len() * PIECE) as u64;
    let mut sum = 0u64;
    let mut passes = 0u64;
    let r0 = boot_time_ns();
    let mut now = r0;
    while passes == 0 || now - r0 < READ_NS {
        for piece in &memory.pieces {
            for &w in piece.iter() {
                sum = sum.wrapping_add(w);
            }
        }
        passes += 1;
        now = boot_time_ns();
    }
    black_box(sum);
    let ns = (now - r0).max(1);
    work.bytes_per_sec.store(((bytes * passes) as u128 * NS_PER_SEC as u128 / ns as u128) as u64, Ordering::Release);
}

/// A CPU running of node `node`, its last: None for a node of memory alone
fn cpu_of(node: usize) -> Option<u32> {
    let cpus = kcore::numa::node_cpus(node) & kcore::cpu::online_mask();
    (cpus != 0).then(|| 63 - cpus.leading_zeros())
}

fn parse(args: &str) -> Result<(usize, u64), String> {
    let mut mib = DEFAULT_MIB;
    let mut loads = DEFAULT_LOADS;
    for word in args.split_whitespace() {
        match word.split_once('=') {
            Some(("mib", v)) => {
                mib = v.parse().map_err(|_| format!("{}: not a number", word))?;
                if mib == 0 || mib > MAX_MIB {
                    return Err(format!("mib= is 1 to {}", MAX_MIB));
                }
            }
            Some(("loads", v)) => {
                loads = v.parse().map_err(|_| format!("{}: not a number", word))?;
                if loads == 0 || loads > MAX_LOADS {
                    return Err(format!("loads= is 1 to {}", MAX_LOADS));
                }
            }
            _ => return Err(format!("{}: {}", word, HELP)),
        }
    }
    Ok((mib, loads))
}

/// A task bound to `cpu` on `work`, waited for
fn on(name: &str, cpu: u32, work: &Arc<Work>, entry: fn(Arc<Work>)) -> Result<(), String> {
    match kcore::task::spawn_on_with(name, 1u64 << cpu, work.clone(), entry) {
        /* Dropping the handle waits for the task */
        Some(task) => {
            drop(task);
            Ok(())
        }
        None => Err(format!("no task for {}", name)),
    }
}

fn run(args: &str, out: &mut Output) -> Result<(), String> {
    let (mib, loads) = parse(args)?;
    let nodes = kcore::numa::node_count();
    let _ = writeln!(out, "numabench: {} MiB a node, {} loads a pair, {} node{}", mib, loads, nodes,
                     if nodes == 1 { "" } else { "s" });

    /* Every node's memory first, each on a CPU of its own node */
    let mut works: Vec<Arc<Work>> = Vec::new();
    works.try_reserve_exact(nodes).map_err(|_| String::from("out of memory"))?;
    for node in 0..nodes {
        let Some(maker) = cpu_of(node) else {
            let _ = writeln!(out, "  node {} has no CPU running to take its memory: left out", node);
            continue;
        };
        let work = Arc::new(Work {
            node,
            mib,
            loads,
            memory: Mutex::new(None).ok_or_else(|| String::from("out of memory"))?,
            refused: AtomicBool::new(false),
            load_ns: AtomicU64::new(0),
            bytes_per_sec: AtomicU64::new(0),
        });
        on(&format!("numabench/m{}", node), maker, &work, make)?;
        if work.refused.load(Ordering::Acquire) || work.memory.lock().is_none() {
            return Err(format!("node {}'s CPU {} was refused {} MiB", node, maker, mib));
        }
        works.push(work);
    }

    let _ = writeln!(out, "  {:<10} {:<10} {:>12} {:>12} {:>12}", "CPUs of", "memory of", "load ns", "read GB/s",
                     "its pages");
    for cpu_node in 0..nodes {
        let Some(reader) = cpu_of(cpu_node) else {
            continue;
        };
        for work in &works {
            on(&format!("numabench/r{}", cpu_node), reader, work, read)?;
            let (pages, own) = {
                let m = work.memory.lock();
                m.as_ref().map_or((0, 0), |m| (m.pages, m.own_pages))
            };
            let load = work.load_ns.load(Ordering::Acquire);
            let rate = work.bytes_per_sec.load(Ordering::Acquire);
            let _ = writeln!(out, "  node {:<5} node {:<5} {:>10}.{} {:>9}.{:02} {:>11}%", cpu_node, work.node,
                             load / 10, load % 10, rate / 1_000_000_000, (rate / 10_000_000) % 100,
                             own * 100 / pages.max(1));
        }
    }
    /* Every node's given back once the last read is done */
    drop(works);
    Ok(())
}

struct NumaBench {
    _cmd: Command,
}

impl kmod::Module for NumaBench {}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let cmd = Command::register("numabench", HELP, |args, out| {
        if let Err(problem) = run(args, out) {
            let _ = writeln!(out, "numabench: {}", problem);
        }
    })?;
    Ok(Box::new(NumaBench { _cmd: cmd }))
}

kmod::module!(name: "numabench", init: init);
