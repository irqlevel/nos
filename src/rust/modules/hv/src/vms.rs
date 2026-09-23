//! Guests that run until they are stopped: `hv start`, and the commands that
//! reach one while it runs -- `list`, `console`, `send`, `exec`, `wait`,
//! `stop`.
//!
//! Each VM is its guest on a vCPU task of its own, bound to a CPU the
//! extension is on for, and a [`Shared`] that task and the commands both hold:
//! the console the guest writes to (the last 64 KiB of it), the input waiting
//! to be typed at it, a stop flag, and the loop's counters as it last gave
//! them. The guest itself -- its memory, its CPU, its devices -- belongs to
//! the task alone, and is freed by the task when the guest stops; what is
//! left of a stopped VM is its console and how it ended, until `hv stop`
//! takes it off the list.
//!
//! A command never holds the table's lock while it waits: `stop` takes the
//! VM off the table, and only then joins its task; `exec` and `wait` poll the
//! console a lock at a time.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use hv::run::{Counts, Host, LinuxGuest};
use hv::Machine;
use kcore::cmd::Output;
use kcore::consts::{MAX_CPUS, NS_PER_MS, NS_PER_SEC};
use kcore::sync::Mutex;
use kcore::task::TaskHandle;

use crate::guest::{self, LogLine, Ring};

const START_USAGE: &str =
    "hv start <bzImage> [mem=MiB] [cpu=N] [initrd=path] [input=...] [log] [cmdline=...]";
/// The most VMs at once: each is a task and at least 64 MiB of RAM.
const MAX_VMS: usize = 16;
/// The most input waiting for one guest.
const INPUT_MAX: usize = 4096;
/// What `hv console` shows unless asked for more.
const CONSOLE_DEFAULT: usize = 2048;
/// How long `hv exec` and `hv wait` wait by default, and at most.
const EXEC_DEFAULT_S: u64 = 10;
const WAIT_DEFAULT_S: u64 = 60;
const WAIT_MAX_S: u64 = 600;
/// How often they look.
const POLL_MS: u64 = 50;
/// Room for how a VM ended, taken before it is written.
const REASON_BYTES: usize = 256;
const REPORT_BYTES: usize = 4096;

/// What is typed at a guest: the bytes not yet fed to its UART, and how many
/// ever were queued and fed -- which is how `hv exec` tells that its line has
/// gone in, and where in the console the answer starts.
struct Input {
    queue: VecDeque<u8>,
    queued: u64,
    fed: u64,
    /// The console's total when the last byte was fed: whatever the guest
    /// printed from there on, it printed after that byte was there to read.
    fed_at: u64,
}

/// What a VM's vCPU task and the commands that reach it share.
pub struct Shared {
    id: u32,
    stop: AtomicBool,
    running: AtomicBool,
    console: Mutex<Ring>,
    input: Mutex<Input>,
    /// How many bytes wait in `input`: the vCPU takes the lock only when some
    /// do, rather than on every exit.
    pending: AtomicUsize,
    /// How it ended, in a line and whole; empty while it runs.
    reason: Mutex<String>,
    report: Mutex<String>,
    /// The loop's counters as it last reported them.
    exits: AtomicU64,
    irq: AtomicU64,
    hlt: AtomicU64,
    started_ns: u64,
    ended_ns: AtomicU64,
    log: bool,
}

impl Shared {
    fn new(id: u32, log: bool, input: &[u8]) -> Option<Shared> {
        let mut queue = VecDeque::new();
        queue.try_reserve_exact(INPUT_MAX).ok()?;
        /* `start` refused more than fits. */
        queue.extend(input.iter().take(INPUT_MAX));
        let queued = queue.len();
        Some(Shared {
            id,
            stop: AtomicBool::new(false),
            running: AtomicBool::new(true),
            console: Mutex::new(Ring::new()?)?,
            input: Mutex::new(Input { queue, queued: queued as u64, fed: 0, fed_at: 0 })?,
            pending: AtomicUsize::new(queued),
            reason: Mutex::new(String::new())?,
            report: Mutex::new(String::new())?,
            exits: AtomicU64::new(0),
            irq: AtomicU64::new(0),
            hlt: AtomicU64::new(0),
            started_ns: kcore::time::boot_time_ns(),
            ended_ns: AtomicU64::new(0),
            log,
        })
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Queue `bytes` to be typed at the guest, all of them or -- when they do
    /// not fit -- none. The number the last of them has among all the bytes
    /// ever typed at it.
    fn type_in(&self, bytes: &[u8]) -> Option<u64> {
        let mut input = self.input.lock();
        /* The queue's room was taken when the VM was made, and it is never
         * let grow past it: no push here allocates. */
        if bytes.len() > INPUT_MAX.saturating_sub(input.queue.len()) {
            return None;
        }
        input.queue.extend(bytes.iter());
        input.queued += bytes.len() as u64;
        self.pending.fetch_add(bytes.len(), Ordering::Release);
        Some(input.queued)
    }

    /// Where the console was when byte `seq` of the input -- or one after it
    /// -- was fed to the guest, once it has been.
    fn fed_at(&self, seq: u64) -> Option<u64> {
        let input = self.input.lock();
        (input.fed >= seq).then_some(input.fed_at)
    }

    /// The console from `from` on, at most `max` bytes, made safe to print;
    /// and where the oldest byte it still has is, which is after `from` when
    /// some of what was asked for is gone.
    fn console_since(&self, from: u64, max: usize) -> Option<(Vec<u8>, u64)> {
        let mut raw = Vec::new();
        let oldest = {
            let ring = self.console.lock();
            if !ring.since(from, max, &mut raw) {
                return None;
            }
            ring.oldest()
        };
        let mut text = Vec::new();
        guest::sanitize(&raw, &mut text).then_some((text, oldest))
    }

    fn console_total(&self) -> u64 {
        self.console.lock().total()
    }
}

/// The loop's side of a VM: the guest's console into the ring (and the kernel
/// log, with `log`), what was typed out of the queue, the stop flag, and the
/// counters out to where `hv list` reads them.
struct VmHost {
    shared: Arc<Shared>,
    /// With `log`, the line of the console on its way to the kernel log.
    line: Option<LogLine>,
    /// What the guest has written to its console: the ring's total, which
    /// only this pushes to, kept here so that feeding a byte need not take
    /// the ring's lock to learn it.
    written: u64,
}

impl Host for VmHost {
    fn output(&mut self, byte: u8) {
        self.shared.console.lock().push(byte);
        self.written += 1;
        if let Some(line) = &mut self.line {
            if line.push(byte) {
                kcore::trace!(0, "hvvm{}| {}", self.shared.id, line.text());
                line.clear();
            }
        }
    }

    fn input(&mut self) -> Option<u8> {
        if self.shared.pending.load(Ordering::Acquire) == 0 {
            return None;
        }
        let mut input = self.shared.input.lock();
        let byte = input.queue.pop_front()?;
        input.fed += 1;
        input.fed_at = self.written;
        self.shared.pending.fetch_sub(1, Ordering::Release);
        Some(byte)
    }

    fn stop_requested(&mut self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }

    fn progress(&mut self, counts: &Counts) {
        self.shared.exits.store(counts.exits, Ordering::Relaxed);
        self.shared.irq.store(counts.irq, Ordering::Relaxed);
        self.shared.hlt.store(counts.hlt, Ordering::Relaxed);
    }
}

/// What a vCPU task starts from: the guest, the machine it runs on, and what
/// it shares with the commands. The guest is the task's alone from here.
struct Start {
    shared: Arc<Shared>,
    guest: LinuxGuest,
    machine: Arc<Machine>,
}

/// A VM's vCPU task: the guest, until it stops or is stopped; then how it
/// ended, into what the commands read; then the guest freed, here.
fn vcpu(start: Start) {
    let Start { shared, mut guest, machine } = start;
    let line = if shared.log { LogLine::new() } else { None };
    if shared.log && line.is_none() {
        kcore::trace!(0, "hv: vm {} logs nothing of its console: no memory for a line", shared.id);
    }
    let mut host = VmHost { shared: shared.clone(), line, written: 0 };
    let (stop, counts) = guest.run(&machine, u64::MAX, &mut host);
    if let Some(line) = host.line.as_ref().filter(|l| !l.text().is_empty()) {
        kcore::trace!(0, "hvvm{}| {}", shared.id, line.text());
    }
    host.progress(&counts);

    let ended = kcore::time::boot_time_ns();
    let ran_ns = ended.saturating_sub(shared.started_ns);
    let mut reason = String::new();
    let mut report = String::new();
    if reason.try_reserve(REASON_BYTES).is_ok() && report.try_reserve(REPORT_BYTES).is_ok() {
        let _ = guest::describe(&stop, &mut reason);
        guest::report(&mut report, &guest, &stop, &counts, ran_ns);
    }
    kcore::trace!(0, "hv: vm {} stopped after {} ms -- {}", shared.id, ran_ns / NS_PER_MS, reason);
    *shared.reason.lock() = reason;
    *shared.report.lock() = report;
    shared.ended_ns.store(ended, Ordering::Relaxed);
    /* Last, and with release: whoever sees it stopped sees how. */
    shared.running.store(false, Ordering::Release);
    /* The guest's memory, its CPU and its devices go back here, in the task,
     * with the guest stopped for good. */
    drop(guest);
}

/// A VM as the table keeps it.
struct Vm {
    shared: Arc<Shared>,
    /// Its vCPU task, joined by `stop` -- taken out of the table first, and
    /// dropped outside the lock, since dropping it waits.
    task: Option<TaskHandle>,
    cpu: u32,
    kernel: String,
    mem_mib: u64,
}

struct Table {
    next: u32,
    vms: Vec<Vm>,
    /// The module is on its way out: no VM is added from here on.
    closing: bool,
}

/// Every VM this module has started and not yet taken off the list.
pub struct Vms {
    table: Mutex<Table>,
}

/// The first word of `args`, and the rest after it -- the text of `send`,
/// `exec` and `wait`, spaces and all.
fn after_word(args: &str) -> (&str, &str) {
    let args = args.trim_start();
    let end = args.find(char::is_whitespace).unwrap_or(args.len());
    (&args[..end], args[end..].trim_start())
}

/// `secs=N` at the head of `rest`, if it is there, and what follows it.
fn secs_option(rest: &str, default: u64) -> Result<(u64, &str), String> {
    let (word, tail) = after_word(rest);
    match word.strip_prefix("secs=") {
        Some(v) => match v.parse::<u64>() {
            Ok(s) if (1..=WAIT_MAX_S).contains(&s) => Ok((s, tail)),
            _ => Err(alloc::format!("secs= must be 1..{}", WAIT_MAX_S)),
        },
        None => Ok((default, rest)),
    }
}

/// Stop a VM taken off the table and wait for its vCPU task: the flag, then
/// the join, which returns once the guest has stopped and been freed.
fn finish(mut vm: Vm) {
    vm.shared.stop.store(true, Ordering::Release);
    drop(vm.task.take());
}

/// Whether what a shell printed after a line was fed to it is the line's
/// whole answer: the echo of its newline, then everything up to a prompt.
fn at_prompt(text: &[u8]) -> bool {
    text.contains(&b'\n') && (text.ends_with(b"# ") || text.ends_with(b"$ "))
}

impl Vms {
    pub fn new() -> Option<Vms> {
        let mut vms = Vec::new();
        vms.try_reserve_exact(MAX_VMS).ok()?;
        Some(Vms { table: Mutex::new(Table { next: 0, vms, closing: false })? })
    }

    /// How many running VMs each CPU has: what a new vCPU is placed by.
    pub fn load(&self) -> [u32; MAX_CPUS] {
        let mut load = [0u32; MAX_CPUS];
        for vm in &self.table.lock().vms {
            if vm.shared.running() {
                if let Some(n) = load.get_mut(vm.cpu as usize) {
                    *n += 1;
                }
            }
        }
        load
    }

    /// A running VM on a CPU in `mask`, if there is one, and its CPU: what
    /// `hv off` will not turn the extension off under.
    pub fn running_on(&self, mask: u64) -> Option<(u32, u32)> {
        let table = self.table.lock();
        table.vms.iter()
            .find(|vm| vm.shared.running() && vm.cpu < u64::BITS && mask & (1u64 << vm.cpu) != 0)
            .map(|vm| (vm.shared.id, vm.cpu))
    }

    /// The VM `word` names.
    fn find(&self, word: &str) -> Result<Arc<Shared>, String> {
        let id: u32 = word.parse().map_err(|_| alloc::format!("hv: \"{}\" is not a vm number", word))?;
        let table = self.table.lock();
        table.vms.iter()
            .find(|vm| vm.shared.id == id)
            .map(|vm| vm.shared.clone())
            .ok_or_else(|| alloc::format!("hv: no vm {}", id))
    }

    /// `hv start`: build the guest, put it on a vCPU task, and come back.
    pub fn start(&self, machine: &Arc<Machine>, args: &str, out: &mut Output) {
        if let Err(e) = hv::run::ensure_runnable(machine) {
            let _ = writeln!(out, "hv: no guest can run here -- {}", e);
            return;
        }
        let spec = match guest::parse(args, START_USAGE) {
            Ok(spec) if spec.secs.is_none() => spec,
            Ok(_) => {
                let _ = writeln!(out, "hv: secs= is hv boot's -- a started vm runs until hv stop");
                return;
            }
            Err(usage) => {
                let _ = writeln!(out, "{}", usage);
                return;
            }
        };
        let cpu = match guest::pick_cpu(machine, spec.cpu, &self.load()) {
            Ok(cpu) => cpu,
            Err(why) => {
                let _ = writeln!(out, "hv: {}", why);
                return;
            }
        };
        if spec.input.len() > INPUT_MAX {
            let _ = writeln!(out, "hv: input= is {} bytes, more than the {} a vm queues", spec.input.len(), INPUT_MAX);
            return;
        }
        let id = {
            let mut table = self.table.lock();
            if table.closing {
                let _ = writeln!(out, "hv: the module is being unloaded");
                return;
            }
            if table.vms.len() >= MAX_VMS {
                let _ = writeln!(out, "hv: {} vms already -- hv stop one first", MAX_VMS);
                return;
            }
            let Some(next) = table.next.checked_add(1) else {
                let _ = writeln!(out, "hv: out of vm numbers -- reload the module");
                return;
            };
            core::mem::replace(&mut table.next, next)
        };

        /* The files are read and the guest's memory filled here, with no lock
         * held: it takes as long as the kernel and the initrd take to read. */
        let guest = match guest::build(machine, &spec) {
            Ok(guest) => guest,
            Err(why) => {
                let _ = writeln!(out, "hv: vm {} not started -- {}", id, why);
                return;
            }
        };
        let shared = match Shared::new(id, spec.log, &spec.input) {
            Some(shared) => Arc::new(shared),
            None => {
                let _ = writeln!(out, "hv: vm {} not started -- out of memory for its console", id);
                return;
            }
        };
        let mut kernel = String::new();
        let mut name = String::new();
        if kernel.try_reserve_exact(spec.kernel.len()).is_err() || name.try_reserve(16).is_err() {
            let _ = writeln!(out, "hv: vm {} not started -- out of memory", id);
            return;
        }
        kernel.push_str(&spec.kernel);
        let _ = write!(name, "hv/vm{}", id);

        let start = Start { shared: shared.clone(), guest, machine: machine.clone() };
        let task = match kcore::task::spawn_on_with(&name, 1u64 << cpu, start, vcpu) {
            Some(task) => task,
            None => {
                /* The start -- the guest in it -- went with the spawn that
                 * failed. */
                let _ = writeln!(out, "hv: vm {} not started -- no task for it", id);
                return;
            }
        };

        let vm = Vm { shared, task: Some(task), cpu, kernel, mem_mib: spec.mem_bytes / (1024 * 1024) };
        let refused = {
            let mut table = self.table.lock();
            if table.closing || table.vms.len() >= MAX_VMS {
                Some(vm)
            } else {
                /* Into the room taken when the table was made (`MAX_VMS`). */
                table.vms.push(vm);
                None
            }
        };
        /* Two starts that both found room, or an unload that began while
         * this one read its files: stop the guest again, off the lock. */
        if let Some(vm) = refused {
            finish(vm);
            let _ = writeln!(out, "hv: vm {} stopped again at once -- no room for it, or the module is going", id);
            return;
        }
        let _ = writeln!(out, "hv: vm {} started on cpu {} -- {}, {} MiB, cmdline \"{}\"",
                         id, cpu, spec.kernel, spec.mem_bytes / (1024 * 1024), spec.cmdline);
    }

    /// `hv list`.
    pub fn list(&self, out: &mut Output) {
        let now = kcore::time::boot_time_ns();
        let table = self.table.lock();
        if table.vms.is_empty() {
            let _ = writeln!(out, "hv: no vms");
            return;
        }
        for vm in &table.vms {
            let s = &vm.shared;
            let running = s.running();
            let until = if running { now } else { s.ended_ns.load(Ordering::Relaxed) };
            let _ = write!(out, "vm {}  {}  cpu {}  {} MiB  {} s  exits {}  irq {}  hlt {}  {}",
                s.id, if running { "running" } else { "stopped" }, vm.cpu, vm.mem_mib,
                until.saturating_sub(s.started_ns) / NS_PER_SEC,
                s.exits.load(Ordering::Relaxed), s.irq.load(Ordering::Relaxed),
                s.hlt.load(Ordering::Relaxed), vm.kernel);
            if !running {
                let _ = write!(out, "  -- {}", *s.reason.lock());
            }
            let _ = writeln!(out);
        }
    }

    /// `hv console <id> [bytes=N]`: the tail of the guest's console.
    pub fn console(&self, args: &str, out: &mut Output) {
        let (word, rest) = after_word(args);
        let shared = match self.find(word) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(out, "{}", e);
                return;
            }
        };
        let bytes = match rest.strip_prefix("bytes=") {
            Some(v) => match v.trim().parse::<usize>() {
                Ok(n) if (1..=guest::CONSOLE_BYTES).contains(&n) => n,
                _ => {
                    let _ = writeln!(out, "hv: bytes= must be 1..{}", guest::CONSOLE_BYTES);
                    return;
                }
            },
            None if rest.is_empty() => CONSOLE_DEFAULT,
            None => {
                let _ = writeln!(out, "hv console <id> [bytes=N]");
                return;
            }
        };
        match shared.console_since(0, bytes) {
            Some((text, _)) if text.is_empty() => {
                let _ = writeln!(out, "hv: vm {} has printed nothing yet", shared.id);
            }
            Some((text, _)) => {
                out.write_bytes(&text);
                if !text.ends_with(b"\n") {
                    let _ = writeln!(out);
                }
            }
            None => {
                let _ = writeln!(out, "hv: out of memory");
            }
        }
    }

    /// `hv send <id> <text>`: type at the guest, `\n` for a newline.
    pub fn send(&self, args: &str, out: &mut Output) {
        let (word, text) = after_word(args);
        let shared = match self.find(word) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(out, "{}", e);
                return;
            }
        };
        if !shared.running() {
            let _ = writeln!(out, "hv: vm {} is not running", shared.id);
            return;
        }
        let bytes = match guest::unescape(text) {
            Ok(b) => b,
            Err(e) => {
                let _ = writeln!(out, "hv: {}", e);
                return;
            }
        };
        match shared.type_in(&bytes) {
            Some(_) => {
                let _ = writeln!(out, "hv: vm {}: {} bytes queued", shared.id, bytes.len());
            }
            None => {
                let _ = writeln!(out, "hv: vm {}: its input is full -- nothing typed", shared.id);
            }
        }
    }

    /// `hv exec <id> [secs=N] <line>`: type a line at the guest and print what
    /// it printed back, once its prompt has come back -- or it has stopped,
    /// or the time is up.
    pub fn exec(&self, args: &str, out: &mut Output) {
        let (word, rest) = after_word(args);
        let shared = match self.find(word) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(out, "{}", e);
                return;
            }
        };
        let (secs, line) = match secs_option(rest, EXEC_DEFAULT_S) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(out, "hv: {}", e);
                return;
            }
        };
        if !shared.running() {
            let _ = writeln!(out, "hv: vm {} is not running", shared.id);
            return;
        }
        let mut bytes = match guest::unescape(line) {
            Ok(b) => b,
            Err(e) => {
                let _ = writeln!(out, "hv: {}", e);
                return;
            }
        };
        if bytes.try_reserve(1).is_err() {
            let _ = writeln!(out, "hv: out of memory");
            return;
        }
        bytes.push(b'\n');

        let from = shared.console_total();
        let Some(seq) = shared.type_in(&bytes) else {
            let _ = writeln!(out, "hv: vm {}: its input is full -- nothing typed", shared.id);
            return;
        };

        /* Done at a prompt printed after the line's last byte went in -- not
         * at one the shell printed before it read the line, which is where a
         * line typed at a guest still booting, or at one sitting at its
         * prompt, would otherwise stop. */
        let deadline = kcore::time::boot_time_ns().saturating_add(secs * NS_PER_SEC);
        let mut seen = None;
        let why = loop {
            kcore::task::sleep_ms(POLL_MS);
            if let Some(at) = shared.fed_at(seq) {
                let total = shared.console_total();
                if seen != Some(total) {
                    seen = Some(total);
                    if let Some((text, _)) = shared.console_since(at, guest::CONSOLE_BYTES) {
                        if at_prompt(&text) {
                            break None;
                        }
                    }
                }
            }
            if !shared.running() {
                break Some("the vm stopped");
            }
            if kcore::time::boot_time_ns() >= deadline {
                break Some(if shared.fed_at(seq).is_some() {
                    "no prompt came back in time"
                } else {
                    "the guest has not read the line -- it is not at a prompt; the line stays queued"
                });
            }
        };

        match shared.console_since(from, guest::CONSOLE_BYTES) {
            Some((text, oldest)) => {
                if oldest > from {
                    let _ = writeln!(out, "hv: vm {}: the first {} bytes of its answer are gone from the console",
                                     shared.id, oldest - from);
                }
                out.write_bytes(&text);
                if !text.is_empty() && !text.ends_with(b"\n") {
                    let _ = writeln!(out);
                }
            }
            None => {
                let _ = writeln!(out, "hv: out of memory");
            }
        }
        if let Some(why) = why {
            let _ = writeln!(out, "hv: vm {}: {}", shared.id, why);
        }
    }

    /// `hv wait <id> [secs=N] <text>`: until the guest's console has `text`
    /// in it, the guest has stopped, or the time is up.
    pub fn wait(&self, args: &str, out: &mut Output) {
        let (word, rest) = after_word(args);
        let shared = match self.find(word) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(out, "{}", e);
                return;
            }
        };
        let (secs, text) = match secs_option(rest, WAIT_DEFAULT_S) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(out, "hv: {}", e);
                return;
            }
        };
        if text.is_empty() {
            let _ = writeln!(out, "hv wait <id> [secs=N] <text>");
            return;
        }
        let start = kcore::time::boot_time_ns();
        loop {
            /* Whether it had stopped is read before the console is searched:
             * a guest that printed the text and then stopped is found. */
            let running = shared.running();
            if shared.console.lock().contains(text.as_bytes()) {
                let _ = writeln!(out, "hv: vm {} printed \"{}\", {} ms in", shared.id, text,
                                 kcore::time::boot_time_ns().saturating_sub(start) / NS_PER_MS);
                return;
            }
            if !running {
                let _ = writeln!(out, "hv: vm {} stopped without printing \"{}\" -- {}",
                                 shared.id, text, *shared.reason.lock());
                return;
            }
            if kcore::time::boot_time_ns().saturating_sub(start) >= secs * NS_PER_SEC {
                let _ = writeln!(out, "hv: vm {} has not printed \"{}\" in {} s", shared.id, text, secs);
                return;
            }
            kcore::task::sleep_ms(POLL_MS);
        }
    }

    /// `hv stop <id|all>`: stop it, wait for its vCPU, say how it ended, and
    /// take it off the list.
    pub fn stop(&self, args: &str, out: &mut Output) {
        let (word, _) = after_word(args);
        if word == "all" {
            self.stop_all(Some(out));
            return;
        }
        let id: u32 = match word.parse() {
            Ok(id) => id,
            Err(_) => {
                let _ = writeln!(out, "hv stop <id|all>");
                return;
            }
        };
        let vm = {
            let mut table = self.table.lock();
            match table.vms.iter().position(|vm| vm.shared.id == id) {
                Some(i) => table.vms.remove(i),
                None => {
                    let _ = writeln!(out, "hv: no vm {}", id);
                    return;
                }
            }
        };
        let shared = vm.shared.clone();
        finish(vm);
        let _ = writeln!(out, "hv: vm {} stopped -- {}", id, *shared.reason.lock());
        out.write_bytes(shared.report.lock().as_bytes());
    }

    /// Stop every VM and take it off the list, saying how each ended -- to
    /// `out`, or, with no one to say it to, to the kernel log.
    pub fn stop_all(&self, mut out: Option<&mut Output>) {
        /* Every stop flag first, so that the VMs stop together rather than
         * one join at a time. */
        for vm in &self.table.lock().vms {
            vm.shared.stop.store(true, Ordering::Release);
        }
        let mut stopped = 0usize;
        loop {
            let vm = {
                let mut table = self.table.lock();
                if table.vms.is_empty() {
                    break;
                }
                table.vms.remove(0)
            };
            let shared = vm.shared.clone();
            finish(vm);
            stopped += 1;
            let reason = shared.reason.lock();
            match out.as_deref_mut() {
                Some(out) => {
                    let _ = writeln!(out, "hv: vm {} stopped -- {}", shared.id, *reason);
                }
                None => kcore::trace!(0, "hv: vm {} stopped for the unload -- {}", shared.id, *reason),
            }
        }
        if stopped == 0 {
            if let Some(out) = out {
                let _ = writeln!(out, "hv: no vms");
            }
        }
    }

    /// The module is going: no VM starts from here on, and every one there is
    /// stops.
    pub fn close(&self) {
        self.table.lock().closing = true;
        self.stop_all(None);
    }
}
