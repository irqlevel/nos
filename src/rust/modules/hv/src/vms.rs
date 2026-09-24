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
//! The task lives as long as the VM does. A guest that stops leaves it parked
//! on the VM's event, its memory given back, for `hv restart` to boot the
//! guest again from its files or `hv stop` to end it; with `restart`, a guest
//! that resets itself -- a reboot -- is booted again straight away.
//!
//! A command never holds the table's lock while it waits: `stop` takes the
//! VM off the table, and only then joins its task; `exec` and `wait` poll the
//! console a lock at a time.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use hv::run::{Counts, Host, LinuxGuest, Stop};
use hv::Machine;
use kcore::cmd::Output;
use kcore::consts::{MAX_CPUS, NS_PER_MS, NS_PER_SEC};
use kcore::sync::{Event, Mutex};
use kcore::task::TaskHandle;

use crate::guest::{self, LogLine, NicSpec, Ring, Spec, TermFilter};
use crate::net::{self, Switch};

const START_USAGE: &str =
    "hv start <bzImage> [mem=MiB] [cpu=N] [initrd=path] [disk=path[:ro]]... [input=...] [log] [restart] [net] [cmdline=...]";
/// How many times a `restart` guest is booted again after resetting itself
/// within `RESTART_WINDOW_NS` before that is taken for a loop and it is left
/// stopped: a guest that reboots in its first second would otherwise take
/// its CPU for good.
const RESTART_BURST: u32 = 5;
const RESTART_WINDOW_NS: u64 = 60 * NS_PER_SEC;
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
/// How long `hv restart` waits for the guest to be built again: as long as
/// its files take to read.
const RESTART_WAIT_S: u64 = 120;
/// How much of what the console already has `hv attach` shows first: enough
/// to see the prompt it lands at.
const ATTACH_BACKLOG: u64 = 1024;
/// How long `hv attach` waits for typing before it looks at the console
/// again: the most its output lags.
const ATTACH_POLL_MS: u64 = 20;
/// What ends `hv attach`: ^], as it ends telnet's and virsh's console.
const DETACH: u8 = 0x1D;
/// Keys taken from the session at a time.
const ATTACH_KEYS: usize = 256;
/// Room for how a VM ended, taken before it is written.
const REASON_BYTES: usize = 256;
/// Room for what `hv start` says, beside the kernel's path and command line.
const SAID_BYTES: usize = 96;
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
    /// `hv restart`: boot the guest again -- the one running, or the one
    /// parked after it stopped.
    reset: AtomicBool,
    /// What a parked task waits on: signalled with `stop` and with `reset`.
    wake: Event,
    /// What has the vCPU leave its guest when a frame comes for it while
    /// it runs, rather than at the host's next interrupt.
    kick: hv::Kick,
    running: AtomicBool,
    /// How many times it has been booted again.
    restarts: AtomicU32,
    /// How many `hv attach` sessions it has.
    attached: AtomicU32,
    /// `hv send` has typed at this boot: its input goes in from then on,
    /// a shell's prompt or not -- whoever typed took it to be reading.
    typed: AtomicBool,
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
    /// When its current boot began, and when it last stopped.
    boot_ns: AtomicU64,
    /// Where the console was when its current boot began: what `hv wait`
    /// looks from, so that a guest booted again is not found to have printed
    /// what the boot before it did.
    boot_at: AtomicU64,
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
            reset: AtomicBool::new(false),
            wake: Event::new()?,
            kick: hv::Kick::new(),
            running: AtomicBool::new(true),
            restarts: AtomicU32::new(0),
            attached: AtomicU32::new(0),
            typed: AtomicBool::new(false),
            console: Mutex::new(Ring::new()?)?,
            input: Mutex::new(Input { queue, queued: queued as u64, fed: 0, fed_at: 0 })?,
            pending: AtomicUsize::new(queued),
            reason: Mutex::new(String::new())?,
            report: Mutex::new(String::new())?,
            exits: AtomicU64::new(0),
            irq: AtomicU64::new(0),
            hlt: AtomicU64::new(0),
            boot_ns: AtomicU64::new(kcore::time::boot_time_ns()),
            boot_at: AtomicU64::new(0),
            ended_ns: AtomicU64::new(0),
            log,
        })
    }

    /// A new boot's input: what `input=` types at its first prompt, in place
    /// of whatever was still waiting for the one before.
    fn requeue(&self, bytes: &[u8]) {
        let mut input = self.input.lock();
        self.typed.store(false, Ordering::Release);
        input.queue.clear();
        /* `start` refused more than fits. */
        input.queue.extend(bytes.iter().take(INPUT_MAX));
        let n = input.queue.len();
        input.queued += n as u64;
        /* Every change to `pending` is made under this lock, so it is the
         * queue's length whenever the lock is free. */
        self.pending.store(n, Ordering::Release);
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Something waits for the guest -- a frame, handed over before this:
    /// a halted vCPU's task is woken, a vCPU in its guest kicked out of it
    /// to take it now. From any context, interrupts off included.
    pub(crate) fn wake_up(&self) {
        self.wake.signal();
        self.kick.kick();
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
        /* A halted guest takes it now, not at its next timer edge. */
        self.wake.signal();
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

    /// The console as the guest wrote it, from `from` on, onto `out`, at most
    /// `max` bytes; and where it has got to, for the next call's `from`.
    fn console_raw(&self, from: u64, max: usize, out: &mut Vec<u8>) -> Option<u64> {
        let ring = self.console.lock();
        ring.since(from, max, out).then(|| ring.total())
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

    fn input(&mut self, at_prompt: bool) -> Option<u8> {
        if self.shared.pending.load(Ordering::Acquire) == 0 {
            return None;
        }
        /* A script's line waits for the prompt; with someone attached, or
         * once `hv send` has typed, what is typed goes in when it is. */
        if !at_prompt && self.shared.attached.load(Ordering::Acquire) == 0
            && !self.shared.typed.load(Ordering::Acquire)
        {
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
        self.shared.stop.load(Ordering::Acquire) || self.shared.reset.load(Ordering::Acquire)
    }

    /// On the VM's event: a frame for the guest, a key typed at it, a stop
    /// or a restart wake it at once; else the timer edge does.
    fn halt_wait(&mut self, ns: u64) {
        self.shared.wake.wait_for(kcore::time::Duration::from_nanos(ns));
    }

    fn progress(&mut self, counts: &Counts) {
        self.shared.exits.store(counts.exits, Ordering::Relaxed);
        self.shared.irq.store(counts.irq, Ordering::Relaxed);
        self.shared.hlt.store(counts.hlt, Ordering::Relaxed);
    }
}

/// What a vCPU task starts from: the guest, the machine it runs on, what it
/// shares with the commands, and the spec it was built from, to build it
/// again. The guest is the task's alone from here.
struct Start {
    shared: Arc<Shared>,
    guest: LinuxGuest,
    machine: Arc<Machine>,
    spec: Spec,
}

/// How a `restart` guest's reboots are counted: `RESTART_BURST` in a
/// `RESTART_WINDOW_NS` from the first of them, and a window over begins a
/// new one.
struct Burst {
    since: u64,
    count: u32,
}

impl Burst {
    fn allow(&mut self, now: u64) -> bool {
        if self.count == 0 || now.saturating_sub(self.since) >= RESTART_WINDOW_NS {
            self.since = now;
            self.count = 0;
        }
        self.count += 1;
        self.count <= RESTART_BURST
    }
}

/// A VM's vCPU task, for as long as the VM is on the list: its guest until
/// the guest stops or is stopped, then -- the guest's memory given back --
/// booted again, or parked on the VM's event until a command asks for a
/// restart or the end.
fn vcpu(start: Start) {
    let Start { shared, guest, machine, spec } = start;
    let line = if shared.log { LogLine::new() } else { None };
    if shared.log && line.is_none() {
        kcore::trace!(0, "hv: vm {} logs nothing of its console: no memory for a line", shared.id);
    }
    /* One host for every boot: the console, and where it has got to, is the
     * VM's and not a boot's. */
    let mut host = VmHost { shared: shared.clone(), line, written: 0 };
    let mut guest = Some(guest);
    let mut burst = Burst { since: 0, count: 0 };

    loop {
        let Some(mut g) = guest.take() else {
            /* Parked: nothing to run until a command says what next. The
             * flags are looked at before the wait, not only after: the
             * signal a stop or a restart came with may have been taken
             * already -- by a halted guest's wait, which the same event
             * ends -- and a park that waited for it would wait for good,
             * and `hv stop` and the unload with it. A signal still there
             * from while the guest ran finds nothing to do. */
            if !shared.stop.load(Ordering::Acquire) && !shared.reset.load(Ordering::Acquire) {
                shared.wake.wait();
            }
            if shared.stop.load(Ordering::Acquire) {
                break;
            }
            if shared.reset.load(Ordering::Acquire) {
                /* Running again from here, not from when the build is done,
                 * and before the request is taken down: `hv restart` tells a
                 * build under way from one that failed by the two. */
                shared.running.store(true, Ordering::Release);
                shared.reset.store(false, Ordering::Release);
                guest = reboot(&shared, &machine, &spec, "on request", None);
            }
            continue;
        };

        let (stop, counts) = g.run(&machine, u64::MAX, &mut host, Some(&shared.kick));
        if let Some(line) = host.line.as_mut().filter(|l| !l.text().is_empty()) {
            kcore::trace!(0, "hvvm{}| {}", shared.id, line.text());
            line.clear();
        }
        host.progress(&counts);

        let ended = kcore::time::boot_time_ns();
        let ran_ns = ended.saturating_sub(shared.boot_ns.load(Ordering::Relaxed));
        let mut reason = String::new();
        let mut report = String::new();
        if reason.try_reserve(REASON_BYTES).is_ok() && report.try_reserve(REPORT_BYTES).is_ok() {
            let _ = guest::describe(&stop, &mut reason);
            guest::report(&mut report, &g, &stop, &counts, ran_ns);
        }
        kcore::trace!(0, "hv: vm {} stopped after {} ms -- {}", shared.id, ran_ns / NS_PER_MS, reason);
        /* Its memory, its CPU and its devices go back before anything else
         * is built: a reboot needs as much again. */
        drop(g);

        /* What next: another boot -- asked for, or the guest's own reset
         * with `restart` -- or parked until a command says. */
        let asked = shared.reset.swap(false, Ordering::AcqRel);
        let reset_itself = matches!(stop, Stop::Reset { .. } | Stop::Shutdown { .. });
        let again = if shared.stop.load(Ordering::Acquire) {
            None
        } else if asked {
            Some("on request")
        } else if spec.restart && reset_itself {
            if burst.allow(ended) {
                Some("it reset itself")
            } else {
                let _ = write!(reason, " -- reset {} times in {} s, left stopped",
                               burst.count, RESTART_WINDOW_NS / NS_PER_SEC);
                None
            }
        } else {
            None
        };
        match again {
            /* Booted again -- or, when it cannot be built, stopped saying so,
             * with this boot's report kept. */
            Some(why) => guest = reboot(&shared, &machine, &spec, why, Some(report)),
            None => stopped(&shared, reason, Some(report), ended),
        }
    }
}

/// Say how the VM ended -- and, given one, the report of its last boot --
/// and that it is no longer running: last, and with release, so whoever sees
/// it stopped sees how.
fn stopped(shared: &Shared, reason: String, report: Option<String>, ended: u64) {
    *shared.reason.lock() = reason;
    if let Some(report) = report {
        *shared.report.lock() = report;
    }
    shared.ended_ns.store(ended, Ordering::Relaxed);
    shared.running.store(false, Ordering::Release);
}

/// Build the guest again from its files, for another boot -- or, when that
/// fails, say why and leave the VM stopped with `report`, the last boot's,
/// when there is one to keep.
fn reboot(shared: &Shared, machine: &Machine, spec: &Spec, why: &str,
          report: Option<String>) -> Option<LinuxGuest> {
    kcore::trace!(0, "hv: vm {} restarting -- {}", shared.id, why);
    /* The new boot's console starts here, before the build -- which is long
     * for a big guest under TCG -- so that `hv wait` meanwhile does not find
     * what the last boot printed. */
    shared.boot_at.store(shared.console_total(), Ordering::Release);
    match guest::build(machine, spec) {
        Ok(g) => {
            shared.requeue(&spec.input);
            shared.boot_ns.store(kcore::time::boot_time_ns(), Ordering::Relaxed);
            /* Last, with release: whoever sees the count move sees the new
             * boot's input and console start with it. */
            shared.restarts.fetch_add(1, Ordering::Release);
            shared.running.store(true, Ordering::Release);
            Some(g)
        }
        Err(e) => {
            let mut reason = String::new();
            if reason.try_reserve(REASON_BYTES).is_ok() {
                let _ = write!(reason, "restart failed -- {}", e);
            }
            kcore::trace!(0, "hv: vm {} not restarted -- {}", shared.id, e);
            stopped(shared, reason, report, kcore::time::boot_time_ns());
            None
        }
    }
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
    /// Its port on the switch, with `net`.
    port: Option<usize>,
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
    /// The guests' switch, made by the first `net` guest and kept until the
    /// module goes.
    switch: Mutex<Option<Arc<Switch>>>,
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

/// A port claimed for a VM being started, given back if the start goes no
/// further.
struct PortHold {
    switch: Arc<Switch>,
    port: usize,
    kept: bool,
}

impl Drop for PortHold {
    fn drop(&mut self) {
        if !self.kept {
            self.switch.release(self.port);
        }
    }
}

/// Stop a VM taken off the table and wait for its vCPU task: the flag, then
/// the join, which returns once the guest has stopped and been freed.
fn finish(mut vm: Vm) {
    vm.shared.stop.store(true, Ordering::Release);
    vm.shared.wake.signal();
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
        Some(Vms {
            table: Mutex::new(Table { next: 0, vms, closing: false })?,
            switch: Mutex::new(None)?,
        })
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
        let mut spec = match guest::parse(args, START_USAGE) {
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

        let shared = match Shared::new(id, spec.log, &spec.input) {
            Some(shared) => Arc::new(shared),
            None => {
                let _ = writeln!(out, "hv: vm {} not started -- out of memory for its console", id);
                return;
            }
        };

        /* With `net`, a port on the switch -- made by the first such guest --
         * and the address that goes with it on the guest's command line;
         * given back if the start goes no further. */
        let mut hold = None;
        if spec.net {
            let switch = match self.switch() {
                Ok(switch) => switch,
                Err(why) => {
                    let _ = writeln!(out, "hv: vm {} not started -- {}", id, why);
                    return;
                }
            };
            let Some(port) = switch.claim(&shared) else {
                let _ = writeln!(out, "hv: vm {} not started -- every port of the switch is taken", id);
                return;
            };
            /* Its way out, and the DNS server it is told of: a guest with no
             * way out still reaches nos and the other guests. */
            if let Err(why) = switch.way_out() {
                let _ = writeln!(out, "hv: vm {} has no way out of the switch -- {}", id, net::nat_why(why));
            }
            spec.cmdline.push(' ');
            spec.cmdline.push_str(&net::ip_param(port, switch.dns()));
            spec.nic = Some(NicSpec { port, switch: switch.clone() });
            hold = Some(PortHold { switch, port, kept: false });
        }

        /* The files are read and the guest's memory filled here, with no lock
         * held: it takes as long as the kernel and the initrd take to read. */
        let guest = match guest::build(machine, &spec) {
            Ok(guest) => guest,
            Err(why) => {
                let _ = writeln!(out, "hv: vm {} not started -- {}", id, why);
                return;
            }
        };
        let mem_mib = spec.mem_bytes / (1024 * 1024);
        let mut kernel = String::new();
        let mut name = String::new();
        let mut said = String::new();
        if kernel.try_reserve_exact(spec.kernel.len()).is_err() || name.try_reserve(16).is_err()
            || said.try_reserve(SAID_BYTES + spec.kernel.len() + spec.cmdline.len()).is_err()
        {
            let _ = writeln!(out, "hv: vm {} not started -- out of memory", id);
            return;
        }
        kernel.push_str(&spec.kernel);
        let _ = write!(name, "hv/vm{}", id);
        let _ = write!(said, "{}, {} MiB, cmdline \"{}\"{}", spec.kernel, mem_mib, spec.cmdline,
                       if spec.restart { ", restarted when it resets" } else { "" });

        let start = Start { shared: shared.clone(), guest, machine: machine.clone(), spec };
        let task = match kcore::task::spawn_on_with(&name, 1u64 << cpu, start, vcpu) {
            Some(task) => task,
            None => {
                /* The start -- the guest in it -- went with the spawn that
                 * failed. */
                let _ = writeln!(out, "hv: vm {} not started -- no task for it", id);
                return;
            }
        };

        let port = hold.as_ref().map(|h| h.port);
        let vm = Vm { shared, task: Some(task), cpu, kernel, mem_mib, port };
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
        /* On the list: its port is given back when it comes off. */
        if let Some(h) = hold.as_mut() {
            h.kept = true;
        }
        let _ = writeln!(out, "hv: vm {} started on cpu {} -- {}", id, cpu, said);
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
            let _ = write!(out, "vm {}  {}  cpu {}  {} MiB  {} s",
                s.id, if running { "running" } else { "stopped" }, vm.cpu, vm.mem_mib,
                until.saturating_sub(s.boot_ns.load(Ordering::Relaxed)) / NS_PER_SEC);
            if let Some(port) = vm.port {
                let _ = write!(out, "  {}", net::dotted(net::port_ip(port)));
            }
            let dropped = vm.port.and_then(|p| self.switch.lock().as_ref().map(|s| s.dropped(p))).unwrap_or(0);
            if dropped != 0 {
                let _ = write!(out, "  {} frames dropped for it", dropped);
            }
            let restarts = s.restarts.load(Ordering::Relaxed);
            if restarts != 0 {
                let _ = write!(out, "  restarts {}", restarts);
            }
            let _ = write!(out, "  exits {}  irq {}  hlt {}  kicks {}  {}",
                s.exits.load(Ordering::Relaxed), s.irq.load(Ordering::Relaxed),
                s.hlt.load(Ordering::Relaxed), s.kick.sent(), vm.kernel);
            if !running {
                let _ = write!(out, "  -- {}", *s.reason.lock());
            }
            let _ = writeln!(out);
        }
        drop(table);
        if let Some(s) = self.switch.lock().as_ref() {
            let (to_host, refused, dhcp) = s.host_counts();
            let _ = writeln!(out, "hv0 {}/24: {} frames from the guests to nos, {} it would not take, {} DHCP answers",
                             net::dotted(net::HOST_IP), to_host, refused, dhcp);
            match s.nat_address() {
                Some(ip) => {
                    let _ = writeln!(out, "the guests go out through NAT, from {} (nos's nat command)", net::dotted(ip));
                }
                None => {
                    let _ = writeln!(out, "the guests have no way out: NAT is not on");
                }
            }
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

    /// `hv send <id> <text>`: type at the guest, `\n` for a newline -- now,
    /// as at a terminal, and not at a shell's prompt: a login prompt asks
    /// for no cursor, and a getty throws away what was typed before it
    /// printed one, so what to type at is `hv wait`'s to find. From then on
    /// this boot, what `exec` types goes in as it is typed too.
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
        shared.typed.store(true, Ordering::Release);
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

        let restarts = shared.restarts.load(Ordering::Acquire);
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
            if shared.restarts.load(Ordering::Acquire) != restarts {
                break Some("the vm restarted, and the line went with the boot it was typed at");
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

    /// `hv wait <id> [secs=N] [boot=N] <text>`: until the guest's console has `text`
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
        let (secs, rest) = match secs_option(rest, WAIT_DEFAULT_S) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(out, "hv: {}", e);
                return;
            }
        };
        /* `boot=N`: not before its Nth restart -- what a script waits for
         * after typing `reboot`, since the boot going down still has on its
         * console what the next one is to print. */
        let (word, tail) = after_word(rest);
        let (boot, text) = match word.strip_prefix("boot=") {
            Some(v) => match v.parse::<u32>() {
                Ok(n) => (n, tail),
                Err(_) => {
                    let _ = writeln!(out, "hv: boot= wants a number of restarts");
                    return;
                }
            },
            None => (0, rest),
        };
        if text.is_empty() {
            let _ = writeln!(out, "hv wait <id> [secs=N] [boot=N] <text>");
            return;
        }
        let start = kcore::time::boot_time_ns();
        loop {
            /* Whether it had stopped is read before the console is searched:
             * a guest that printed the text and then stopped is found. The
             * restarts before the console's start: the count moves last, so
             * a boot it has reached has its start in `boot_at` already. */
            let running = shared.running();
            let restarts = shared.restarts.load(Ordering::Acquire);
            let from = shared.boot_at.load(Ordering::Acquire);
            if restarts >= boot && shared.console.lock().contains_since(from, text.as_bytes()) {
                let _ = writeln!(out, "hv: vm {} printed \"{}\"{}, {} ms in", shared.id, text,
                                 if boot != 0 { alloc::format!(" in boot {}", restarts) } else { String::new() },
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

    /// `hv attach <id>`: the guest's console, live, for a person at an SSH
    /// session: what it prints goes to the session as it prints it
    /// (`TermFilter`), what is typed goes to it as it is typed -- a line
    /// editor's keys and all -- until ^] is typed, the guest stops, or the
    /// session goes. `ssh -t`, so that the keys come one at a time and the
    /// guest does the echoing.
    pub fn attach(&self, args: &str, out: &mut Output) {
        let (word, _) = after_word(args);
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
        let mut keys = [0u8; ATTACH_KEYS];
        /* Whether anybody can type here at all: a look that does not wait. */
        let Some(mut typed) = out.read_input(&mut keys, kcore::time::Duration::from_nanos(0)) else {
            let _ = writeln!(out, "hv: nobody can type here -- hv attach is for an ssh session: ssh -t <host> hv attach {}",
                             shared.id);
            return;
        };
        let mut raw = Vec::new();
        let mut text = Vec::new();
        if raw.try_reserve_exact(guest::CONSOLE_BYTES).is_err()
            || text.try_reserve_exact(guest::CONSOLE_BYTES + ATTACH_KEYS).is_err()
        {
            let _ = writeln!(out, "hv: out of memory");
            return;
        }

        let _ = writeln!(out, "hv: attached to vm {} -- ^] detaches", shared.id);
        shared.attached.fetch_add(1, Ordering::AcqRel);
        let mut filter = TermFilter::new();
        let mut seen = shared.console_total().saturating_sub(ATTACH_BACKLOG);
        let why = loop {
            if typed != 0 {
                let keys = &keys[..typed];
                let detach = keys.iter().position(|&b| b == DETACH);
                let keys = &keys[..detach.unwrap_or(keys.len())];
                if !keys.is_empty() && shared.type_in(keys).is_none() {
                    let _ = write!(out, "\r\nhv: vm {}'s input is full -- {} keys dropped\r\n", shared.id, keys.len());
                }
                if detach.is_some() {
                    break "detached";
                }
            }

            raw.clear();
            text.clear();
            match shared.console_raw(seen, guest::CONSOLE_BYTES, &mut raw) {
                Some(total) => seen = total,
                None => break "out of memory",
            }
            filter.filter(&raw, &mut text);
            if !text.is_empty() {
                out.write_bytes(&text);
            }
            if !shared.running() {
                break "the vm stopped";
            }

            typed = match out.read_input(&mut keys, kcore::time::Duration::from_millis(ATTACH_POLL_MS)) {
                Some(n) => n,
                None => break "the session ended",
            };
        };
        shared.attached.fetch_sub(1, Ordering::AcqRel);
        let _ = writeln!(out, "\nhv: {} -- vm {}", why, shared.id);
    }

    /// `hv restart <id>`: boot the guest again from its files -- the running
    /// one, as a reset button would, or one that has stopped. The VM's task
    /// does it; this waits until the new boot is built, or has failed to be,
    /// so that what comes next in a script finds it booting.
    pub fn restart(&self, args: &str, out: &mut Output) {
        let (word, _) = after_word(args);
        let shared = match self.find(word) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(out, "{}", e);
                return;
            }
        };
        let before = shared.restarts.load(Ordering::Acquire);
        shared.reset.store(true, Ordering::Release);
        shared.wake.signal();

        let deadline = kcore::time::boot_time_ns().saturating_add(RESTART_WAIT_S * NS_PER_SEC);
        loop {
            kcore::task::sleep_ms(POLL_MS);
            if shared.restarts.load(Ordering::Acquire) != before {
                let _ = writeln!(out, "hv: vm {} restarted", shared.id);
                return;
            }
            /* Taken up, and not running: its build failed, or it was
             * stopped meanwhile -- the reason says which. */
            if !shared.reset.load(Ordering::Acquire) && !shared.running() {
                let _ = writeln!(out, "hv: vm {} not restarted -- {}", shared.id, *shared.reason.lock());
                return;
            }
            if kcore::time::boot_time_ns() >= deadline {
                let _ = writeln!(out, "hv: vm {} is still not booted again after {} s", shared.id, RESTART_WAIT_S);
                return;
            }
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
        let port = vm.port;
        finish(vm);
        self.release(port);
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
            vm.shared.wake.signal();
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
            let port = vm.port;
            finish(vm);
            self.release(port);
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
    /// stops -- and then the switch goes, `hv0`'s sink detached.
    pub fn close(&self) {
        self.table.lock().closing = true;
        self.stop_all(None);
        let switch = self.switch.lock().take();
        drop(switch);
    }

    /// The guests' switch, made the first time it is asked for.
    fn switch(&self) -> Result<Arc<Switch>, String> {
        let mut switch = self.switch.lock();
        if let Some(s) = switch.as_ref() {
            return Ok(s.clone());
        }
        let made = Arc::new(Switch::new()?);
        *switch = Some(made.clone());
        Ok(made)
    }

    /// A VM's port given back, once its vCPU task is gone.
    fn release(&self, port: Option<usize>) {
        let Some(port) = port else { return };
        let switch = self.switch.lock().clone();
        if let Some(s) = switch {
            s.release(port);
        }
    }

    /// The guests' switch, if there is one: for `hv forward`.
    pub fn host_nic(&self) -> Option<kcore::net::Nic> {
        self.switch.lock().as_ref().and_then(|s| s.host_nic())
    }

    /// The address of the running VM `id`, when it has a NIC.
    pub fn address_of(&self, id: u32) -> Option<u32> {
        let table = self.table.lock();
        table.vms.iter().find(|vm| vm.shared.id == id && vm.shared.running())
            .and_then(|vm| vm.port).map(net::port_ip)
    }
}
