//! Guests that run until they are stopped: `hv start`, and the commands that
//! reach one while it runs -- `list`, `console`, `send`, `exec`, `wait`,
//! `stop`.
//!
//! Each VM is its guest on a task of its own -- the VM's, which runs its
//! first CPU -- with a task for each of its other CPUs, each bound to a CPU
//! the extension is on for, and a [`Shared`] those tasks and the commands
//! all hold: the console the guest writes to (the last 64 KiB of it), the
//! input waiting to be typed at it, a stop flag, each CPU's doorbell, and
//! the loops' counters as they last gave them. The guest itself -- its
//! memory, its CPUs, its devices -- belongs to the tasks alone, and is freed
//! by the VM's when the guest stops; what is left of a stopped VM is its
//! console and how it ended, until `hv stop` takes it off the list.
//!
//! The task lives as long as the VM does. A guest that stops leaves it parked
//! on the VM's event, its memory given back, for `hv restart` to boot the
//! guest again from its files or `hv stop` to end it; with `restart`, a guest
//! that resets itself -- a reboot -- is booted again straight away.
//!
//! `stop` asks first: it presses the guest's power button, which a kernel
//! with ACPI hears and a distribution's init answers by shutting down -- its
//! services stopped, its disks unmounted -- and turning the machine off, and
//! waits for that a while before it stops the guest where it is. A guest that
//! does not listen to the button is stopped at once, and so is every guest
//! when the module goes.
//!
//! A command never holds the table's lock while it waits: `stop` takes the
//! VM off the table, and only then joins its task; `exec` and `wait` poll the
//! console a lock at a time.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use hv::run::{Counts, Host};
use hv::{Doorbells, Machine};
use kcore::cmd::Output;
use kcore::consts::{MAX_CPUS, NS_PER_MS, NS_PER_SEC};
use kcore::sync::Mutex;
use kcore::task::TaskHandle;

use crate::disk::{self, Runner, Wake};
use crate::guest::{self, Built, LogLine, NicSpec, Ring, Spec, TermFilter};
use crate::net::{self, Switch};

const START_USAGE: &str =
    "hv start <bzImage> [mem=MiB] [cpus=N] [cpu=N] [xapic] [ioapic] [initrd=path] [disk=path[:ro]]... [input=...] [log] [restart] [net] [cmdline=...]";
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
/// How long `hv stop` gives a guest to turn itself off once its power
/// button is pressed, unless told: long enough for a distribution's init to
/// stop its services and unmount its disks.
const STOP_DEFAULT_S: u64 = 30;
/* The power button, as `hv stop` presses it and the first CPU's loop takes
 * it: pressed, taken by the loop, and then whether the guest's OS heard it
 * -- had the button's event enabled -- or not. */
const BUTTON_UP: u8 = 0;
const BUTTON_PRESSED: u8 = 1;
const BUTTON_TAKEN: u8 = 2;
const BUTTON_HEARD: u8 = 3;
const BUTTON_UNHEARD: u8 = 4;
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

/// What a guest CPU's loop last said it had counted.
struct CpuStats {
    exits: AtomicU64,
    irq: AtomicU64,
    hlt: AtomicU64,
}

/// What a VM's tasks and the commands that reach it share.
pub struct Shared {
    id: u32,
    stop: AtomicBool,
    /// `hv restart`: boot the guest again -- the one running, or the one
    /// parked after it stopped.
    reset: AtomicBool,
    /// Its power button: `BUTTON_UP` until `hv stop` presses it.
    button: AtomicU8,
    /// Each of its CPUs' doorbell: what that CPU's task waits on while the
    /// CPU is halted, and what has it leave its guest when something comes
    /// for it while it runs, rather than at the host's next interrupt. The
    /// first CPU's is the one frames, disks and keys ring -- that CPU does
    /// the devices' work -- and the one the VM's task waits on while the
    /// guest is parked, rung with `stop` and with `reset`.
    doorbells: Arc<Doorbells>,
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
    /// The loops' counters as they last reported them, a CPU each.
    stats: Vec<CpuStats>,
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
    fn new(id: u32, log: bool, input: &[u8], cpus: u32) -> Option<Shared> {
        let mut queue = VecDeque::new();
        queue.try_reserve_exact(INPUT_MAX).ok()?;
        /* `start` refused more than fits. */
        queue.extend(input.iter().take(INPUT_MAX));
        let queued = queue.len();
        let mut stats = Vec::new();
        stats.try_reserve_exact(cpus as usize).ok()?;
        for _ in 0..cpus {
            stats.push(CpuStats { exits: AtomicU64::new(0), irq: AtomicU64::new(0), hlt: AtomicU64::new(0) });
        }
        Some(Shared {
            id,
            stop: AtomicBool::new(false),
            reset: AtomicBool::new(false),
            button: AtomicU8::new(BUTTON_UP),
            doorbells: Arc::new(Doorbells::new(cpus as usize)?),
            running: AtomicBool::new(true),
            restarts: AtomicU32::new(0),
            attached: AtomicU32::new(0),
            typed: AtomicBool::new(false),
            console: Mutex::new(Ring::new()?)?,
            input: Mutex::new(Input { queue, queued: queued as u64, fed: 0, fed_at: 0 })?,
            pending: AtomicUsize::new(queued),
            reason: Mutex::new(String::new())?,
            report: Mutex::new(String::new())?,
            stats,
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

    /// Press its power button, when it is running: the first CPU's loop
    /// takes the press, woken to it now. Whether it was pressed.
    fn press(&self) -> bool {
        if !self.running() {
            return false;
        }
        self.button.store(BUTTON_PRESSED, Ordering::Release);
        self.wake_up();
        true
    }

    /// Whether it is still to be waited for after a press: running, and
    /// neither known deaf to the button nor never pressed.
    fn shutting_down(&self) -> bool {
        self.running()
            && matches!(self.button.load(Ordering::Acquire), BUTTON_PRESSED | BUTTON_TAKEN | BUTTON_HEARD)
    }

    /// What became of `hv stop`'s press, just before the guest is stopped:
    /// asked while it may still be running.
    fn asked(&self, secs: u64) -> Asked {
        match self.button.load(Ordering::Acquire) {
            BUTTON_UP => Asked::Nothing,
            _ if !self.running() => Asked::Nothing,
            BUTTON_UNHEARD => Asked::Unheard,
            _ => Asked::Unanswered(secs),
        }
    }

    /// Something waits for the guest -- a frame, what a disk served, handed
    /// over before this: the first CPU's -- which does the devices' work --
    /// halted task is woken, or the CPU kicked out of its guest to take it
    /// now. From any context, interrupts off included.
    pub(crate) fn wake_up(&self) {
        if let Some(d) = self.doorbells.get(0) {
            d.ring();
        }
    }

    /// Every CPU's task woken, and every CPU out of its guest: for a stop
    /// or a restart, which each has to see.
    fn ring_all(&self) {
        self.doorbells.ring_all();
    }

    /// The loops' counters, over every CPU: (exits, interrupts, halts).
    fn totals(&self) -> (u64, u64, u64) {
        self.stats.iter().fold((0, 0, 0), |(e, i, h), s| {
            (e + s.exits.load(Ordering::Relaxed), i + s.irq.load(Ordering::Relaxed), h + s.hlt.load(Ordering::Relaxed))
        })
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
        /* A halted guest takes it now, not at its next timer edge: its
         * first CPU feeds the console. */
        if let Some(d) = self.doorbells.get(0) {
            d.signal();
        }
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

/// What became of `hv stop`'s press of a guest's power button, as it adds
/// to how the guest ended.
#[derive(Clone, Copy)]
enum Asked {
    /// Not pressed -- `secs=0`, or the guest was not running -- or answered:
    /// the guest stopped by itself, and how it ended says how.
    Nothing,
    /// Nothing in the guest listens to it: stopped at once.
    Unheard,
    /// It heard, and was still running after this many seconds.
    Unanswered(u64),
}

impl core::fmt::Display for Asked {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Asked::Nothing => Ok(()),
            Asked::Unheard => write!(f, " -- nothing in it listens to its power button"),
            Asked::Unanswered(secs) => write!(f, " -- its power button went unanswered for {} s", secs),
        }
    }
}

impl Wake for Shared {
    fn wake(&self) {
        self.wake_up();
    }
}

/// The loops' side of a VM, which every CPU's task shares: the guest's
/// console into the ring (and the kernel log, with `log`), what was typed
/// out of the queue, the stop flag, and each CPU's counters out to where
/// `hv list` reads them.
struct VmHost {
    shared: Arc<Shared>,
    /// With `log`, the line of the console on its way to the kernel log.
    line: Mutex<Option<LogLine>>,
    /// What the guest has written to its console: the ring's total, which
    /// only `output` pushes to, kept here so that feeding a byte need not
    /// take the ring's lock to learn it.
    written: AtomicU64,
}

impl Host for VmHost {
    fn output(&self, byte: u8) {
        self.shared.console.lock().push(byte);
        self.written.fetch_add(1, Ordering::Relaxed);
        if self.shared.log {
            if let Some(line) = self.line.lock().as_mut() {
                if line.push(byte) {
                    kcore::trace!(0, "hvvm{}| {}", self.shared.id, line.text());
                    line.clear();
                }
            }
        }
    }

    fn input(&self, at_prompt: bool) -> Option<u8> {
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
        input.fed_at = self.written.load(Ordering::Relaxed);
        self.shared.pending.fetch_sub(1, Ordering::Release);
        Some(byte)
    }

    fn stop_requested(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire) || self.shared.reset.load(Ordering::Acquire)
    }

    fn progress(&self, cpu: u32, counts: &Counts) {
        if let Some(s) = self.shared.stats.get(cpu as usize) {
            s.exits.store(counts.exits, Ordering::Relaxed);
            s.irq.store(counts.irq + counts.apic, Ordering::Relaxed);
            s.hlt.store(counts.hlt, Ordering::Relaxed);
        }
    }

    fn power_button(&self) -> bool {
        /* Asked on every exit of the first CPU: a load, and the locked
         * exchange only for a press. */
        self.shared.button.load(Ordering::Relaxed) == BUTTON_PRESSED
            && self.shared.button
                .compare_exchange(BUTTON_PRESSED, BUTTON_TAKEN, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    fn power_button_heard(&self, heard: bool) {
        self.shared.button.store(if heard { BUTTON_HEARD } else { BUTTON_UNHEARD }, Ordering::Release);
    }
}

impl VmHost {
    /// The line of the console not yet ended, to the kernel log: at the end
    /// of a boot.
    fn flush_line(&self) {
        if let Some(line) = self.line.lock().as_mut().filter(|l| !l.text().is_empty()) {
            kcore::trace!(0, "hvvm{}| {}", self.shared.id, line.text());
            line.clear();
        }
    }
}

/// What a VM's task starts from: the guest, the machine it runs on, what it
/// shares with the commands, the spec it was built from, to build it again,
/// and the host CPUs its CPUs run on. The guest is the task's alone from
/// here -- and its CPUs' tasks'.
struct Start {
    shared: Arc<Shared>,
    built: Built,
    machine: Arc<Machine>,
    spec: Spec,
    /// What its disks wake and where they are served, for every boot.
    runner: Runner,
    placement: Vec<u32>,
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

/// A VM's task, for as long as the VM is on the list: its guest's first CPU
/// -- the other CPUs on tasks it starts and waits for -- until the guest
/// stops or is stopped, then -- the guest's memory given back -- booted
/// again, or parked on the first CPU's doorbell until a command asks for a
/// restart or the end.
fn vcpu(start: Start) {
    let Start { shared, built, machine, spec, runner, placement } = start;
    let line = if shared.log { LogLine::new() } else { None };
    if shared.log && line.is_none() {
        kcore::trace!(0, "hv: vm {} logs nothing of its console: no memory for a line", shared.id);
    }
    /* One host for every boot: the console, and where it has got to, is the
     * VM's and not a boot's. */
    let Some(line) = Mutex::new(line) else {
        stopped(&shared, String::from("not started -- out of memory"), None, kcore::time::boot_time_ns());
        return;
    };
    let host = Arc::new(VmHost { shared: shared.clone(), line, written: AtomicU64::new(0) });
    let mut guest = Some(built);
    let mut burst = Burst { since: 0, count: 0 };
    let Some(bell) = shared.doorbells.get(0) else { return };

    loop {
        let Some(built) = guest.take() else {
            /* Parked: nothing to run until a command says what next. The
             * flags are looked at before the wait, not only after: the
             * ring a stop or a restart came with may have been taken
             * already -- by a halted guest's wait, which the same doorbell
             * ends -- and a park that waited for it would wait for good,
             * and `hv stop` and the unload with it. A ring still there
             * from while the guest ran finds nothing to do. */
            if !shared.stop.load(Ordering::Acquire) && !shared.reset.load(Ordering::Acquire) {
                bell.wait_forever();
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
                guest = reboot(&shared, &machine, &spec, &runner, "on request", None);
            }
            continue;
        };

        let g = Arc::new(built.guest);
        let mut name = String::new();
        let _ = name.try_reserve(16);
        let _ = write!(name, "vm{}", shared.id);
        let ran = guest::run_cpus(&g, built.cpus, &placement, &machine, u64::MAX, &host, &name);
        host.flush_line();

        let ended = kcore::time::boot_time_ns();
        let ran_ns = ended.saturating_sub(shared.boot_ns.load(Ordering::Relaxed));
        let mut reason = String::new();
        let mut report = String::new();
        let ran = match ran {
            Ok(ran) => ran,
            Err(why) => {
                let _ = write!(reason, "not run -- {}", why);
                kcore::trace!(0, "hv: vm {} not run -- {}", shared.id, why);
                drop(g);
                stopped(&shared, reason, None, ended);
                continue;
            }
        };
        for (cpu, counts) in ran.counts.iter().enumerate() {
            host.progress(cpu as u32, counts);
        }
        if reason.try_reserve(REASON_BYTES).is_ok() && report.try_reserve(REPORT_BYTES).is_ok() {
            let _ = guest::describe(&ran.stopped.stop, &mut reason);
            guest::report(&mut report, &g, &ran.stopped, &ran.counts, ran_ns);
        }
        kcore::trace!(0, "hv: vm {} stopped after {} ms -- {}", shared.id, ran_ns / NS_PER_MS, reason);
        /* Its memory, its CPUs and its devices go back before anything else
         * is built: a reboot needs as much again. Every CPU's task is done,
         * so this is the last of the guest. */
        drop(g);

        /* What next: another boot -- asked for, or the guest's own reset
         * with `restart` -- or parked until a command says. */
        let asked = shared.reset.swap(false, Ordering::AcqRel);
        let reset_itself = ran.stopped.stop.is_reset();
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
            Some(why) => guest = reboot(&shared, &machine, &spec, &runner, why, Some(report)),
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
fn reboot(shared: &Shared, machine: &Machine, spec: &Spec, runner: &Runner, why: &str,
          report: Option<String>) -> Option<Built> {
    kcore::trace!(0, "hv: vm {} restarting -- {}", shared.id, why);
    /* The new boot's console starts here, before the build -- which is long
     * for a big guest under TCG -- so that `hv wait` meanwhile does not find
     * what the last boot printed. */
    shared.boot_at.store(shared.console_total(), Ordering::Release);
    match guest::build(machine, spec, runner, shared.doorbells.clone()) {
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
    /// Its task, joined by `stop` -- taken out of the table first, and
    /// dropped outside the lock, since dropping it waits.
    task: Option<TaskHandle>,
    /// The host CPUs its CPUs run on, its first CPU's first.
    cpus: Vec<u32>,
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

/// `hv stop`'s `secs=N` after the VM's word: 0 -- stop it without asking --
/// to `WAIT_MAX_S`, or `STOP_DEFAULT_S` when it is not there.
fn stop_secs(rest: &str) -> Result<u64, String> {
    let (word, tail) = after_word(rest);
    if word.is_empty() {
        return Ok(STOP_DEFAULT_S);
    }
    match word.strip_prefix("secs=").map(|v| v.parse::<u64>()) {
        Some(Ok(s)) if s <= WAIT_MAX_S && tail.is_empty() => Ok(s),
        _ => Err(alloc::format!("hv stop <id|all> [secs=0..{}]", WAIT_MAX_S)),
    }
}

/// Wait until none of the guests `waiting` finds is still shutting down
/// after its power button was pressed, or `secs` have gone by.
fn wait_shut_down(secs: u64, mut waiting: impl FnMut() -> bool) {
    let deadline = kcore::time::boot_time_ns().saturating_add(secs.saturating_mul(NS_PER_SEC));
    while waiting() && kcore::time::boot_time_ns() < deadline {
        kcore::task::sleep_ms(POLL_MS);
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

/// Stop a VM taken off the table and wait for its task: the flag, then the
/// join, which returns once the guest has stopped and been freed.
fn finish(mut vm: Vm) {
    vm.shared.stop.store(true, Ordering::Release);
    vm.shared.ring_all();
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

    /// How many running VMs' CPUs each CPU has: what a new VM's CPUs are
    /// placed by.
    pub fn load(&self) -> [u32; MAX_CPUS] {
        let mut load = [0u32; MAX_CPUS];
        for vm in &self.table.lock().vms {
            if vm.shared.running() {
                for &cpu in &vm.cpus {
                    if let Some(n) = load.get_mut(cpu as usize) {
                        *n += 1;
                    }
                }
            }
        }
        load
    }

    /// A running VM with a CPU on a host CPU in `mask`, if there is one, and
    /// that host CPU: what `hv off` will not turn the extension off under.
    pub fn running_on(&self, mask: u64) -> Option<(u32, u32)> {
        let table = self.table.lock();
        table.vms.iter()
            .filter(|vm| vm.shared.running())
            .find_map(|vm| vm.cpus.iter()
                .find(|&&cpu| cpu < u64::BITS && mask & (1u64 << cpu) != 0)
                .map(|&cpu| (vm.shared.id, cpu)))
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
        let load = self.load();
        let placement = match guest::pick_cpus(machine, spec.cpu, spec.cpus, &load) {
            Ok(p) => p,
            Err(why) => {
                let _ = writeln!(out, "hv: {}", why);
                return;
            }
        };
        let cpu = placement[0];
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

        let shared = match Shared::new(id, spec.log, &spec.input, spec.cpus) {
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
            let port = match switch.claim(&shared) {
                Ok(port) => port,
                Err(why) => {
                    let _ = writeln!(out, "hv: vm {} not started -- {}", id, why);
                    return;
                }
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

        /* Its disks are served off the vCPU's CPU, and wake it. */
        let mut label = String::new();
        if label.try_reserve(16).is_err() {
            let _ = writeln!(out, "hv: vm {} not started -- out of memory", id);
            return;
        }
        let _ = write!(label, "vm{}", id);
        let runner = Runner { wake: shared.clone(), name: label, cpu: disk::disk_cpu(cpu, &load) };

        /* The files are read and the guest's memory filled here, with no lock
         * held: it takes as long as the kernel and the initrd take to read. */
        let built = match guest::build(machine, &spec, &runner, shared.doorbells.clone()) {
            Ok(built) => built,
            Err(why) => {
                let _ = writeln!(out, "hv: vm {} not started -- {}", id, why);
                return;
            }
        };
        let mut cpus = Vec::new();
        if cpus.try_reserve_exact(placement.len()).is_err() {
            let _ = writeln!(out, "hv: vm {} not started -- out of memory", id);
            return;
        }
        cpus.extend_from_slice(&placement);
        let mem_mib = spec.mem_bytes / (1024 * 1024);
        let mut kernel = String::new();
        let mut name = String::new();
        let mut said = String::new();
        let mut host_cpus = String::new();
        if kernel.try_reserve_exact(spec.kernel.len()).is_err() || name.try_reserve(16).is_err()
            || said.try_reserve(SAID_BYTES + spec.kernel.len() + spec.cmdline.len()).is_err()
            || host_cpus.try_reserve(8 + 4 * placement.len()).is_err()
        {
            let _ = writeln!(out, "hv: vm {} not started -- out of memory", id);
            return;
        }
        host_cpus.push_str(if placement.len() == 1 { "cpu " } else { "cpus " });
        for (i, c) in placement.iter().enumerate() {
            let _ = write!(host_cpus, "{}{}", if i == 0 { "" } else { "," }, c);
        }
        kernel.push_str(&spec.kernel);
        let _ = write!(name, "hv/vm{}", id);
        let _ = write!(said, "{}, {} MiB, {} cpu{}, cmdline \"{}\"{}", spec.kernel, mem_mib, spec.cpus,
                       if spec.cpus == 1 { "" } else { "s" }, spec.cmdline,
                       if spec.restart { ", restarted when it resets" } else { "" });

        let start = Start { shared: shared.clone(), built, machine: machine.clone(), spec, runner, placement };
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
        let vm = Vm { shared, task: Some(task), cpus, kernel, mem_mib, port };
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
        let _ = writeln!(out, "hv: vm {} started on {} -- {}", id, host_cpus, said);
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
            let _ = write!(out, "vm {}  {}  cpu{} ", s.id, if running { "running" } else { "stopped" },
                if vm.cpus.len() == 1 { "" } else { "s" });
            for (i, c) in vm.cpus.iter().enumerate() {
                let _ = write!(out, "{}{}", if i == 0 { "" } else { "," }, c);
            }
            let _ = write!(out, "  {} MiB  {} s", vm.mem_mib,
                until.saturating_sub(s.boot_ns.load(Ordering::Relaxed)) / NS_PER_SEC);
            if let Some(port) = vm.port {
                let _ = write!(out, "  {}", net::dotted(net::port_ip(port)));
            }
            let (dropped, spoofed) =
                vm.port.and_then(|p| self.switch.lock().as_ref().map(|s| s.dropped(p))).unwrap_or((0, 0));
            if dropped != 0 {
                let _ = write!(out, "  {} frames dropped for it", dropped);
            }
            if spoofed != 0 {
                let _ = write!(out, "  {} frames it sent as another guest, dropped", spoofed);
            }
            let restarts = s.restarts.load(Ordering::Relaxed);
            if restarts != 0 {
                let _ = write!(out, "  restarts {}", restarts);
            }
            let (exits, irq, hlt) = s.totals();
            let _ = write!(out, "  exits {}  irq {}  hlt {}  kicks {}  {}",
                exits, irq, hlt, s.doorbells.kicks(), vm.kernel);
            if !running {
                let _ = write!(out, "  -- {}", *s.reason.lock());
            }
            let _ = writeln!(out);
        }
        drop(table);
        if let Some(s) = self.switch.lock().as_ref() {
            let (to_host, refused, dhcp, foreign) = s.host_counts();
            let _ = writeln!(out, "hv0 {}/24: {} frames from the guests to nos, {} it would not take, {} DHCP answers, \
                                   {} neither IPv4 nor ARP (dropped)",
                             net::dotted(net::HOST_IP), to_host, refused, dhcp, foreign);
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
        shared.ring_all();

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

    /// `hv stop <id|all> [secs=N]`: take it off the list, press its power
    /// button and give it `secs` to turn itself off; then stop it where it
    /// is, if it is still running, wait for its vCPUs, and say how it ended.
    /// One that does not listen to the button is stopped at once, and
    /// `secs=0` stops it without asking.
    pub fn stop(&self, args: &str, out: &mut Output) {
        let (word, rest) = after_word(args);
        let secs = match stop_secs(rest) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(out, "{}", e);
                return;
            }
        };
        if word == "all" {
            self.stop_all(Some(out), secs);
            return;
        }
        let id: u32 = match word.parse() {
            Ok(id) => id,
            Err(_) => {
                let _ = writeln!(out, "hv stop <id|all> [secs=N]");
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
        if secs != 0 && shared.press() {
            wait_shut_down(secs, || shared.shutting_down());
        }
        let asked = shared.asked(secs);
        finish(vm);
        self.release(port);
        let _ = writeln!(out, "hv: vm {} stopped -- {}{}", id, *shared.reason.lock(), asked);
        out.write_bytes(shared.report.lock().as_bytes());
    }

    /// Stop every VM and take it off the list, saying how each ended -- to
    /// `out`, or, with no one to say it to, to the kernel log: every power
    /// button pressed at once and the guests given `secs` together to turn
    /// themselves off, as `stop` gives one.
    pub fn stop_all(&self, mut out: Option<&mut Output>, secs: u64) {
        if secs != 0 {
            let mut pressed = false;
            for vm in &self.table.lock().vms {
                pressed |= vm.shared.press();
            }
            if pressed {
                wait_shut_down(secs, || self.table.lock().vms.iter().any(|vm| vm.shared.shutting_down()));
            }
        }
        /* Then every stop flag, so that those left stop together rather than
         * one join at a time -- and what became of each press noted first: a
         * guest stopped says nothing of it. */
        let mut asked = [(u32::MAX, Asked::Nothing); MAX_VMS];
        for (note, vm) in asked.iter_mut().zip(self.table.lock().vms.iter()) {
            *note = (vm.shared.id, vm.shared.asked(secs));
            vm.shared.stop.store(true, Ordering::Release);
            vm.shared.ring_all();
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
            let asked = asked.iter().find(|(id, _)| *id == shared.id).map_or(Asked::Nothing, |&(_, a)| a);
            finish(vm);
            self.release(port);
            stopped += 1;
            let reason = shared.reason.lock();
            match out.as_deref_mut() {
                Some(out) => {
                    let _ = writeln!(out, "hv: vm {} stopped -- {}{}", shared.id, *reason, asked);
                }
                None => kcore::trace!(0, "hv: vm {} stopped for the unload -- {}{}", shared.id, *reason, asked),
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
        /* Where they are, without asking: an unload is no place to wait on
         * guests -- rmmod gives a module's exit five seconds before it goes
         * on in the background -- and `hv stop all` first is the way to shut
         * them down. */
        self.stop_all(None, 0);
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
