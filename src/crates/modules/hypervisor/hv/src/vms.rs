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
//! waits for that a while before it stops the guest where it is, pressing the
//! button again every `REPRESS_S` meanwhile: an OS may decline a press and
//! forget it, and answer only one after. A guest that does not listen to the
//! button is stopped at once. So does the module's
//! going -- `rmmod`, and nos's own poweroff and reboot, which unload every
//! module: every guest's button pressed at once, and the guests given
//! `stop_secs=` together (`UNLOAD_STOP_DEFAULT_S` unless `insmod` said) to
//! turn themselves off before the rest are stopped where they are.
//!
//! A command never holds the table's lock while it waits: `stop` takes the
//! VM off the table, and only then joins its task; `exec` and `wait` poll the
//! console a lock at a time.
//!
//! A VM started from its folder (`store`: `hv start <name>`) has its name on
//! the list beside its number, and every command takes either; there is one
//! of a name on the list at a time. Its console goes to `console.log` in the
//! folder (`vmlog`), with a line for what became of it at every start, boot
//! and stop (`Shared::note`); its port on the switch is the one it asks for
//! (`port=`) -- or, asking for none, the one it was given at its first
//! start and kept there (`store::keep_port`) -- and nos's ports it asks for
//! are relayed to it once it runs (`forward=`) -- as for a guest started by
//! its kernel's path with those words.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use hv::run::{Counts, Host, LinuxGuest, Stop};
use hv::{Doorbells, Machine};
use kcore::cmd::Output;
use kcore::consts::{NS_PER_MS, NS_PER_SEC};
use kcore::sync::Mutex;
use kcore::task::TaskHandle;

use crate::cores::{Cores, Hold};
use crate::disk::{Claims, Holder, Runner, Wake};
use crate::forward::{self, Forwards};
use crate::guest::{self, Built, LogLine, NicSpec, Ring, Spec, TermFilter};
use crate::net::{self, Switch};
use crate::store::{self, Folder};
use crate::vmlog::Log;

const START_USAGE: &str =
    "hv start <name|folder> [force] | <kernel> [mem=MiB] [cpus=N] [cpu=N] [node=N] [xapic] [ioapic] [apicv|noapicv] [rxmod=US] [nooffload] [initrd=path] [disk=<path|device>[:ro]]... [pci=bus:dev.fn]... [unsafe_interrupts] [input=...] [log] [restart] [force] [uuid=U] [net] [network=name] [port=N] [forward=nos:guest]... [cmdline=...]";
/// How many times a `restart` guest is booted again after resetting itself
/// within `RESTART_WINDOW_NS` before that is taken for a loop and it is left
/// stopped: a guest that reboots in its first second would otherwise take
/// its CPU for good.
const RESTART_BURST: u32 = 5;
const RESTART_WINDOW_NS: u64 = 60 * NS_PER_SEC;
/// The most VMs at once: each is a task and at least 64 MiB of RAM.
pub(crate) const MAX_VMS: usize = 16;
/// The most input waiting for one guest.
const INPUT_MAX: usize = 4096;
/// What `hv console` shows unless asked for more.
const CONSOLE_DEFAULT: usize = 2048;
/// How long `hv exec` and `hv wait` wait by default, and at most.
const EXEC_DEFAULT_S: u64 = 10;
const WAIT_DEFAULT_S: u64 = 60;
pub(crate) const WAIT_MAX_S: u64 = 600;
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
pub(crate) const STOP_DEFAULT_S: u64 = 30;
/// How long the module's going gives the guests to turn themselves off once
/// their power buttons are pressed, unless `insmod` said (`stop_secs=`):
/// longer than `hv stop`'s, as nobody is at a prompt waiting for it and every
/// guest shuts down at once, their disks all served by this one machine --
/// and longer than the 90 s systemd gives a service that will not stop
/// before it goes on to unmount the disks anyway.
pub(crate) const UNLOAD_STOP_DEFAULT_S: u64 = 120;
/// How often a guest that heard its power button and runs still has it
/// pressed again while its time lasts: an OS may decline a press and forget
/// it -- systemd's logind refuses one while a shutdown is inhibited, as
/// unattended-upgrades inhibits it while it installs -- so that only a press
/// after that ends is answered. A press of a guest already on its way down
/// is one it does not act on twice.
const REPRESS_S: u64 = 15;
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
/// Room for what `hv start` says, beside the kernel's path and command line:
/// its NUMA node's share of its memory among it.
const SAID_BYTES: usize = 144;
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
    /// The bytes the last new boot threw away unread, by number: after the
    /// first, up to and including the second. Counted as fed, so that the
    /// count goes on in step with what is typed after -- `hv exec` waits for
    /// `fed` to reach its line's number, and a count left behind by bytes
    /// that will never be fed had every later line wait in vain.
    lost: (u64, u64),
}

/// What became of a byte typed at a guest, by its number.
enum Fed {
    /// Still queued.
    Waiting,
    /// Fed, the console then this long.
    At(u64),
    /// Thrown away by a new boot before the guest read it.
    Lost,
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
    /// What the hypervisor's file log calls it: `vm 3 (web)`, or `vm 3` for
    /// one with no name of its own.
    label: String,
    stop: AtomicBool,
    /// `hv stop` has it, or `hv stop all` or the unload: the VM stays on the
    /// list -- its CPUs counted where a new VM is placed, `hv off` and an
    /// unload seeing it -- while its power button is given its time; a
    /// second stop and a restart are refused meanwhile, and a second stop's
    /// press is not made: the button is pressed again every `REPRESS_S`,
    /// whoever waits (`press_again`).
    stopping: AtomicBool,
    /// What holds it for its state to be taken (`Held`, as a `u8`; 0 for
    /// nothing): the hold takes `stopping` too, which keeps a stop off it
    /// meanwhile, but the VM is not going -- a move's rounds copy it as it
    /// runs -- and the list says what it is doing (`held_state`).
    held: AtomicU8,
    /// `hv restart`: boot the guest again -- the one running, or the one
    /// parked after it stopped.
    reset: AtomicBool,
    /// Its power button: `BUTTON_UP` until `hv stop` presses it.
    button: AtomicU8,
    /// When it was pressed, the first time a stop pressed it: how long it
    /// then went unanswered is the time since, whoever's press it was -- an
    /// `hv stop`'s own `secs`, or the unload's.
    pressed_at: AtomicU64,
    /// When it was last pressed, again or not: the next press again is
    /// `REPRESS_S` after it.
    last_press: AtomicU64,
    /// Its OS has heard a press of this stop's: a press it does not hear
    /// after that is of a guest on its way down -- a kernel turns the event
    /// off as it powers off -- and not one deaf to the button, to be stopped
    /// at once.
    heard: AtomicBool,
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
    /// With a console.log: what became of the VM, for the log, each with the
    /// console's total and the wall clock when it happened -- written there
    /// among the guest's own lines (`vmlog`).
    notes: Option<Mutex<VecDeque<(u64, u64, String)>>>,
    /// The guest running, while it runs: what its disks' tasks hand what
    /// they served to (`served`), under this lock for the whole call -- and
    /// what the VM's task takes back under it before it lets the guest go,
    /// so that a disk's task never holds the last of a guest, whose drop
    /// waits for that task.
    serving: Mutex<Option<Arc<hv::run::LinuxGuest>>>,
}

/// The most notes waiting for the log's writer: it takes them every second,
/// and a VM that stops and boots faster than that loses the oldest.
const NOTES_MAX: usize = 32;

/// Room for what the hypervisor's log calls a VM beside its name: `vm
/// 4294967295 ()`.
const LABEL_BYTES: usize = 24;

/// Why a start failed, without the `vm <n> not started -- ` many of the
/// reasons begin with: the log's line says that once, by the VM's name.
fn why_alone(why: &str) -> &str {
    let Some(rest) = why.strip_prefix("vm ") else { return why };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    match rest[digits..].strip_prefix(" not started -- ") {
        Some(alone) if digits != 0 => alone,
        _ => why,
    }
}

impl Shared {
    fn new(id: u32, name: &str, log: bool, input: &[u8], cpus: u32, notes: bool) -> Option<Shared> {
        let mut label = String::new();
        label.try_reserve(name.len() + LABEL_BYTES).ok()?;
        let _ = write!(label, "vm {}", id);
        /* A VM with no folder is called `vm<number>`: said once */
        if name.strip_prefix("vm").and_then(|n| n.parse::<u32>().ok()) != Some(id) {
            let _ = write!(label, " ({})", name);
        }
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
            label,
            stop: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            held: AtomicU8::new(0),
            reset: AtomicBool::new(false),
            button: AtomicU8::new(BUTTON_UP),
            pressed_at: AtomicU64::new(0),
            last_press: AtomicU64::new(0),
            heard: AtomicBool::new(false),
            doorbells: Arc::new(Doorbells::new(cpus as usize)?),
            running: AtomicBool::new(true),
            restarts: AtomicU32::new(0),
            attached: AtomicU32::new(0),
            typed: AtomicBool::new(false),
            console: Mutex::new(Ring::new()?)?,
            input: Mutex::new(Input { queue, queued: queued as u64, fed: 0, fed_at: 0, lost: (0, 0) })?,
            pending: AtomicUsize::new(queued),
            reason: Mutex::new(String::new())?,
            report: Mutex::new(String::new())?,
            stats,
            boot_ns: AtomicU64::new(kcore::time::boot_time_ns()),
            boot_at: AtomicU64::new(0),
            ended_ns: AtomicU64::new(0),
            serving: Mutex::new(None)?,
            log,
            notes: if notes {
                let mut q = VecDeque::new();
                q.try_reserve_exact(NOTES_MAX).ok()?;
                Some(Mutex::new(q)?)
            } else {
                None
            },
        })
    }

    /// What became of the VM, for its console.log when it has one: noted at
    /// the console's end as it is now. In task context only: the line is
    /// the caller's, and the queue's room was taken with the VM.
    pub(crate) fn note(&self, text: String) {
        let Some(notes) = &self.notes else { return };
        /* The console's end, read before the queue's lock is taken: the
         * console's lock is never taken under it. */
        let at = self.console_total();
        let wall = kcore::time::wall_clock_secs();
        let dropped = {
            let mut q = notes.lock();
            let dropped = if q.len() >= NOTES_MAX { q.pop_front() } else { None };
            q.push_back((at, wall, text));
            dropped
        };
        /* Freed off the lock. */
        drop(dropped);
    }

    /// The notes waiting for the log's writer, oldest first.
    pub(crate) fn take_notes(&self) -> Vec<(u64, u64, String)> {
        let Some(notes) = &self.notes else { return Vec::new() };
        let mut taken = Vec::new();
        if taken.try_reserve_exact(NOTES_MAX).is_err() {
            return taken;
        }
        let mut q = notes.lock();
        while let Some(n) = q.pop_front() {
            /* Into the room taken above, NOTES_MAX being the queue's most. */
            taken.push(n);
        }
        taken
    }

    /// The console as the guest wrote it, from `from` up to `upto`, at most
    /// `max` bytes, onto `out`: where what was copied begins -- after `from`
    /// when the ring no longer has what came before it. For the log's
    /// writer; None out of memory.
    pub(crate) fn console_for_log(&self, from: u64, upto: u64, max: usize, out: &mut Vec<u8>) -> Option<u64> {
        let ring = self.console.lock();
        let start = from.max(ring.oldest());
        let end = upto.min(ring.total());
        if start >= end {
            return Some(start);
        }
        let n = usize::try_from(end - start).map_or(max, |n| n.min(max));
        ring.since(start, n, out).then_some(start)
    }

    /// A new boot's input: what `input=` types at its first prompt, in place
    /// of whatever was still waiting for the one before.
    fn requeue(&self, bytes: &[u8]) {
        /* The console's end, read before the input's lock is taken: the
         * console's lock is never taken under it. */
        let end = self.console_total();
        let mut input = self.input.lock();
        self.typed.store(false, Ordering::Release);
        /* What the last boot never read goes with it: counted as fed, as if
         * read at the console's end, so that the numbers of what is typed
         * from here on still meet `fed` -- and noted as lost, for a command
         * waiting on one of them to say so. */
        let dropped = input.queue.len() as u64;
        if dropped != 0 {
            input.lost = (input.fed, input.fed + dropped);
            input.fed += dropped;
            input.fed_at = end;
        }
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

    /// Its state on the list while it runs: what a hold of it is doing
    /// (`Held`), "stopping" where a stop has it, else "running".
    fn running_state(&self) -> &'static str {
        if !self.stopping.load(Ordering::Acquire) {
            return "running";
        }
        match self.held.load(Ordering::Acquire) {
            h if h == Held::Save as u8 => "saving",
            h if h == Held::Move as u8 => "moving",
            h if h == Held::Check as u8 => "running",
            _ => "stopping",
        }
    }

    /// How it ended last, copied out: printed after, with the lock free for
    /// its task -- a VM that restarts writes the next one there.
    fn reason_copy(&self) -> String {
        self.reason.lock().clone()
    }

    /// Press its power button, when it is running: the first CPU's loop
    /// takes the press, woken to it now. Whether it was pressed.
    fn press(&self) -> bool {
        if !self.running() {
            return false;
        }
        let now = kcore::time::boot_time_ns();
        self.pressed_at.store(now, Ordering::Relaxed);
        self.last_press.store(now, Ordering::Relaxed);
        /* Each stop's first press says afresh whether the guest hears: a
         * stop left before, and the guest booted again since, says nothing
         * of it now. */
        self.heard.store(false, Ordering::Relaxed);
        self.button.store(BUTTON_PRESSED, Ordering::Release);
        self.wake_up();
        true
    }

    /// Press its button again, `REPRESS_S` after the last press, when its OS
    /// heard that one and it runs still -- asked by whoever waits for it at
    /// every look. Two waiters, an `hv stop` and an unload's, press once
    /// between them: the one that moves `last_press` on presses.
    fn press_again(&self) {
        let now = kcore::time::boot_time_ns();
        let last = self.last_press.load(Ordering::Relaxed);
        if now.saturating_sub(last) < REPRESS_S * NS_PER_SEC
            || self.button.load(Ordering::Acquire) != BUTTON_HEARD
            || !self.running()
        {
            return;
        }
        if self.last_press.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return;
        }
        /* From heard alone: a press the loop has not yet taken, or one it
         * found unheard, is not made again. */
        if self.button.compare_exchange(BUTTON_HEARD, BUTTON_PRESSED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.wake_up();
        }
    }

    /// Whether it is still to be waited for after a press: running, and
    /// neither known deaf to the button nor never pressed.
    fn shutting_down(&self) -> bool {
        self.running()
            && matches!(self.button.load(Ordering::Acquire), BUTTON_PRESSED | BUTTON_TAKEN | BUTTON_HEARD)
    }

    /// What became of `hv stop`'s press, just before the guest is stopped:
    /// asked while it may still be running. Unanswered for as long as it
    /// has been since the press -- a stop of the unload's once said its own
    /// `secs` of a guest an `hv stop` had given ten minutes.
    fn asked(&self) -> Asked {
        match self.button.load(Ordering::Acquire) {
            BUTTON_UP => Asked::Nothing,
            _ if !self.running() => Asked::Nothing,
            BUTTON_UNHEARD => Asked::Unheard,
            _ => {
                let since = kcore::time::boot_time_ns().saturating_sub(self.pressed_at.load(Ordering::Relaxed));
                Asked::Unanswered(since / NS_PER_SEC)
            }
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

    /// The `len` bytes `type_in` numbered up to `seq` taken back out of the
    /// queue, when none of them has been fed and nothing was typed after
    /// them: an `hv exec` that gave up on a guest not at a prompt, whose
    /// line was otherwise typed at whatever prompt came next -- a login,
    /// minutes later -- with nobody waiting for its answer. Whether it was.
    fn untype(&self, seq: u64, len: usize) -> bool {
        let mut input = self.input.lock();
        let first = seq.saturating_sub(len as u64);
        if input.queued != seq || input.fed > first || input.queue.len() < len {
            return false;
        }
        let keep = input.queue.len() - len;
        input.queue.truncate(keep);
        input.queued = first;
        self.pending.fetch_sub(len, Ordering::Release);
        true
    }

    /// What became of byte `seq` of the input: where the console was when
    /// it -- or one after it -- was fed to the guest, once it has been; or
    /// that a new boot threw it away.
    fn fed_at(&self, seq: u64) -> Fed {
        let input = self.input.lock();
        let (after, upto) = input.lost;
        if seq > after && seq <= upto {
            Fed::Lost
        } else if input.fed >= seq {
            Fed::At(input.fed_at)
        } else {
            Fed::Waiting
        }
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

    pub(crate) fn console_total(&self) -> u64 {
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
pub enum Asked {
    /// Not pressed -- `secs=0`, or the guest was not running -- or answered:
    /// the guest stopped by itself, and how it ended says how.
    Nothing,
    /// Nothing in the guest listens to it: stopped at once.
    Unheard,
    /// It heard, and was still running after this many seconds.
    Unanswered(u64),
}

impl Asked {
    /// What it is, as the host API says it.
    pub fn name(&self) -> &'static str {
        match self {
            Asked::Nothing => "answered",
            Asked::Unheard => "unheard",
            Asked::Unanswered(_) => "unanswered",
        }
    }
}

/// Room a terminal's page may need past its bytes: what `TermFilter` holds
/// of a sequence, and lets out as it ends.
const TERM_SLACK: usize = 64;

/// A page of a VM's console (`Vms::console_page`).
pub struct ConsolePage {
    pub text: Vec<u8>,
    pub start: u64,
    pub end: u64,
    pub total: u64,
}

/// A VM on the list, as the host API reads it (`Vms::listed`).
pub struct Listed {
    pub id: u32,
    /// Its folder's, when it was started from one.
    pub name: Option<String>,
    /// "running", "stopping", "moving", "saving" or "stopped" (`Shared::running_state`).
    pub state: &'static str,
    /// The host CPUs its CPUs run on.
    pub cpus: Vec<u32>,
    pub mem_mib: u64,
    /// On a machine of NUMA nodes, the node its memory was taken from first
    /// and the share of it, in percent, found there.
    pub node: Option<(usize, u8)>,
    /// Its port of the switch, and its address there: the port's own on a
    /// host's network, the one vnet gave it on a cluster's.
    pub port: Option<usize>,
    pub ip: Option<u32>,
    /// The public address mapped one to one to it, while it has one.
    pub public: Option<u32>,
    /// How long its boot has run, or ran.
    pub uptime_ms: u64,
    pub restarts: u32,
    /// How it ended; empty while it runs.
    pub reason: String,
    pub exits: u64,
    pub kernel: String,
    /// What it has written to its console in all.
    pub console_bytes: u64,
}

/// What the switch wakes as a frame is put in the VM's port's inbox.
impl crate::net::Wake for Shared {
    fn wake_up(&self) {
        Shared::wake_up(self);
    }
}

/// How a VM `stop_vm` stopped ended.
pub struct Stopped {
    pub id: u32,
    /// How it ended, and what became of its power button's press.
    pub reason: String,
    pub asked: Asked,
    /// Its forwards taken away with it.
    pub forwarded: usize,
    /// Its last boot's report.
    pub report: String,
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

    /// What disk `disk` served given to the guest from its task, the device's
    /// interrupts posted from there (`LinuxGuest::serve_disk`): the first
    /// CPU woken only when its round has to come anyway, or when no guest
    /// runs.
    fn served(&self, disk: u8) {
        let serving = self.serving.lock();
        let again = match serving.as_ref() {
            Some(guest) => guest.serve_disk(disk),
            None => true,
        };
        drop(serving);
        if again {
            self.wake_up();
        }
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
                    if let Some(dropped) = line.admit(kcore::time::boot_time_ns()) {
                        if dropped != 0 {
                            kcore::klog!(0, "hvvm{}| ... {} lines not logged", self.shared.id, dropped);
                            line.told(kcore::time::boot_time_ns());
                        }
                        kcore::klog!(0, "hvvm{}| {}", self.shared.id, line.text());
                    }
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
        /* A press again that finds the event off is of a guest that heard
         * one before and is on its way down: waited for still. */
        let heard = heard || self.shared.heard.load(Ordering::Relaxed);
        if heard {
            self.shared.heard.store(true, Ordering::Relaxed);
        }
        self.shared.button.store(if heard { BUTTON_HEARD } else { BUTTON_UNHEARD }, Ordering::Release);
    }
}

impl VmHost {
    /// The line of the console not yet ended, to the kernel log, and how many
    /// were left out that no notice has said: at the end of a boot.
    fn flush_line(&self) {
        let mut line = self.line.lock();
        let Some(line) = line.as_mut() else { return };
        let dropped = line.take_dropped();
        if dropped != 0 {
            kcore::klog!(0, "hvvm{}| ... {} lines not logged", self.shared.id, dropped);
        }
        if !line.text().is_empty() {
            kcore::klog!(0, "hvvm{}| {}", self.shared.id, line.text());
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
    /// Its hold on those CPUs and its disks', for as long as the task runs:
    /// through every boot and while it is parked, until `hv stop`.
    hold: Hold,
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
    let Start { shared, built, machine, spec, runner, placement, hold } = start;
    /* Given back when this returns, the VM gone. */
    let _hold = hold;
    let line = if shared.log { LogLine::new() } else { None };
    if shared.log && line.is_none() {
        kcore::klog!(0, "hv: vm {} logs nothing of its console: no memory for a line", shared.id);
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
        /* Its host functions' interrupts to it from here, taken back as it
         * stops, in this task */
        let passed = built.passed;
        for p in &passed {
            p.arm(&g);
        }
        let mut name = String::new();
        let _ = name.try_reserve(16);
        let _ = write!(name, "vm{}", shared.id);
        /* Its disks' tasks serve the guest themselves while it runs; taken
         * back -- under the lock their calls hold -- before the guest goes */
        *shared.serving.lock() = Some(g.clone());
        let ran = guest::run_cpus(&g, built.cpus, &placement, &machine, u64::MAX, &host, &name);
        drop(passed);
        *shared.serving.lock() = None;
        host.flush_line();

        let ended = kcore::time::boot_time_ns();
        let ran_ns = ended.saturating_sub(shared.boot_ns.load(Ordering::Relaxed));
        let mut reason = String::new();
        let mut report = String::new();
        let ran = match ran {
            Ok(ran) => ran,
            Err(why) => {
                let _ = write!(reason, "not run -- {}", why);
                kcore::flog!(crate::HV, "{} not run -- {}", shared.label, why);
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
        kcore::flog!(crate::HV, "{} stopped after {} ms -- {}", shared.label, ran_ns / NS_PER_MS, reason);
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
        } else if spec.restart && reset_itself && !shared.stopping.load(Ordering::Acquire) {
            /* Not while it is being stopped: a guest that resets as its
             * power button is pressed -- it was rebooting, or panicked on
             * the way down -- was booted again, the stop waited its whole
             * time, killed the new boot and said the button went
             * unanswered. The reset ends it there. */
            if burst.allow(ended) {
                Some("it reset itself")
            } else {
                let _ = write!(reason, " -- reset {} times in {} s, left stopped",
                               burst.count, RESTART_WINDOW_NS / NS_PER_SEC);
                kcore::flog!(crate::HV, "{} reset {} times in {} s -- left stopped", shared.label, burst.count,
                             RESTART_WINDOW_NS / NS_PER_SEC);
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
    let mut note = String::new();
    if note.try_reserve(reason.len() + 16).is_ok() {
        let _ = write!(note, "stopped -- {}", reason);
        shared.note(note);
    }
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
    kcore::flog!(crate::HV, "{} restarting -- {}", shared.label, why);
    let mut note = String::new();
    if note.try_reserve(why.len() + 24).is_ok() {
        let _ = write!(note, "booting again -- {}", why);
        shared.note(note);
    }
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
            kcore::flog!(crate::HV, "{} not restarted -- {}", shared.label, e);
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
    /// The one its disks are served on, when it has disks.
    disk_cpu: Option<u32>,
    kernel: String,
    mem_mib: u64,
    /// On a machine of NUMA nodes, the node its memory was taken from first
    /// and how much of it, in percent, the allocator found there.
    node: Option<(usize, u8)>,
    /// Its port on the switch, with `net`.
    port: Option<usize>,
    /// Its name, when it was started from its folder: one of a name on the
    /// list at a time.
    name: Option<String>,
    /// That folder's path -- which may be anywhere (`hv start /path`), not
    /// the VMs' folder's -- what its vm.toml, `protected` above all, is
    /// read from while it runs.
    folder: Option<String>,
    /// Its console.log being written, when it has one: finished after its
    /// task, so that how it ended is in it.
    log: Option<Log>,
    /// Its disks' words as it was started with, its `vda` first: what
    /// `disk_io` says each disk's counters of -- its vm.toml may have
    /// changed since.
    disks: Vec<String>,
}

/// What a disk of a guest's has done since the guest's boot: the virtio
/// disk's own counters.
#[derive(Clone, Copy, Default)]
pub struct DiskCounts {
    pub reads: u64,
    pub writes: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub flushes: u64,
    pub errors: u64,
    pub discards: u64,
    pub zeroings: u64,
    /// What its backend took over the requests answered, in nanoseconds.
    pub wait_ns: u64,
}

/// What `disk_io` says of a VM: whether its guest runs, how many times it
/// was booted again, when the counters were read -- the host's boot time,
/// what a rate is made against -- and each disk's word and counters.
pub struct DiskIo {
    pub running: bool,
    pub restarts: u32,
    pub at_ns: u64,
    pub disks: Vec<(String, DiskCounts)>,
}

/// `s` copied, or None out of memory.
fn copied(s: &str) -> Option<String> {
    let mut c = String::new();
    c.try_reserve_exact(s.len()).ok()?;
    c.push_str(s);
    Some(c)
}

/// A VM as `hv list` shows it, copied out of the table under its lock: what
/// is printed goes to whoever asked -- an SSH session that may be slow to
/// take it -- with the lock free meanwhile for every start and stop. Its
/// counters are the guest's own, read live after.
struct VmView {
    shared: Arc<Shared>,
    cpus: Vec<u32>,
    disk_cpu: Option<u32>,
    kernel: String,
    mem_mib: u64,
    node: Option<(usize, u8)>,
    port: Option<usize>,
    name: Option<String>,
}

struct Table {
    next: u32,
    vms: Vec<Vm>,
    /// The module is on its way out: no VM is added from here on.
    closing: bool,
    /// Names held while their folders are made, cloned, deleted or changed,
    /// or a VM of one is started: no VM of one starts, and nothing else of
    /// the kind is done to it (`Vms::reserve`).
    reserved: Vec<Reservation>,
}

/// A name held (`Table::reserved`): what is being done to its folder -- one
/// of `MADE`, `CLONED`, `DELETED`, `CHANGED`, `STARTED` -- and what a VM
/// being made or cloned has been given before its vm.toml says so: a port
/// of the switch and devices, by their ids, which no other make, clone or
/// change is given meanwhile (`Vms::ports_held`, `Vms::devices_given`). Two
/// clones at once were given one port -- one MAC, one address -- and two
/// makes one device, each checking only the vm.tomls written so far.
struct Reservation {
    name: String,
    doing: &'static str,
    port: Option<u32>,
    devices: Vec<String>,
}

/// What a name is held for (`Vms::reserve`): said as "<name> is being
/// <what>", and `DELETED` looked for by a change of a vm.toml
/// (`store::update`), which a delete under way refuses.
pub(crate) const MADE: &str = "made";
pub(crate) const CLONED: &str = "cloned";
pub(crate) const DELETED: &str = "deleted";
pub(crate) const CHANGED: &str = "changed";
const STARTED: &str = "started";

/// Names held at once, at most: folders made, cloned -- each clone holds
/// two -- and deleted together.
const MAX_RESERVED: usize = 16;

/// Why a VM of a name may not start now.
enum Taken {
    /// A VM of the name is on the list: this one.
    Listed(u32),
    /// Its folder is being made, cloned or deleted.
    Doing(&'static str),
}

impl Taken {
    fn said(&self, name: &str) -> String {
        match self {
            Taken::Listed(id) => alloc::format!("vm {} is {} already -- hv restart {}, or hv stop {} first", id, name,
                                                name, name),
            Taken::Doing(doing) => alloc::format!("{} is being {} -- not started", name, doing),
        }
    }
}

/// A name held while its folder is made, cloned or deleted, given back when
/// this goes (`Vms::reserve`).
pub struct Reserved<'a> {
    vms: &'a Vms,
    name: String,
}

impl Reserved<'_> {
    /// The name it holds.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The hold given up to the VM it was taken for, as that VM goes on the
    /// list -- in `table`, the same step: the name is the listed VM's from
    /// there, and no moment is left between the two for another to take it.
    /// What it held, to be freed off the lock; nothing left for the drop.
    fn hand_over(&mut self, table: &mut Table) -> Option<Reservation> {
        let at = table.reserved.iter().position(|r| r.name == self.name);
        self.name.clear();
        at.map(|i| table.reserved.swap_remove(i))
    }

    /// Port `port` of the switch given to the VM this holds the name of,
    /// until its vm.toml says so -- this goes once it is written: no other
    /// make or clone is given it meanwhile (`Vms::ports_held`). Under the
    /// lock of the vm.toml changes, which the look for a free port is made
    /// under too (`store::take_port`).
    pub fn keep_port(&self, port: u32) {
        let mut table = self.vms.table.lock();
        if let Some(r) = table.reserved.iter_mut().find(|r| r.name == self.name) {
            r.port = Some(port);
        }
    }

    /// The devices, by their ids, given to the VM this holds the name of,
    /// until its vm.toml says so: what `devices::check` refuses another
    /// meanwhile. Under the lock of the vm.toml changes, as the checks were
    /// made (`store::give_devices`).
    pub fn keep_devices(&self, ids: Vec<String>) {
        let had = {
            let mut table = self.vms.table.lock();
            match table.reserved.iter_mut().find(|r| r.name == self.name) {
                Some(r) => core::mem::replace(&mut r.devices, ids),
                None => ids,
            }
        };
        /* Freed off the lock. */
        drop(had);
    }
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        if self.name.is_empty() {
            return;
        }
        let gone = {
            let mut table = self.vms.table.lock();
            table.reserved.iter().position(|r| r.name == self.name).map(|i| table.reserved.swap_remove(i))
        };
        /* Freed off the lock. */
        drop(gone);
    }
}

/// What a VM's guest is made of at its start: a kernel booted, as `hv start`
/// boots one -- or the state of a guest that ran (`hv restore`, a
/// migration's), put into a machine built alike and its memory filled by
/// `fill`; `what` says where it came from, for the note.
pub enum Origin<'a> {
    Boot,
    State { state: &'a vmstate::VmState, fill: &'a mut dyn FnMut(&LinuxGuest) -> Result<(), String>, what: &'a str },
}

/// A VM coming from another host, from its offer here to its run
/// (plans/vm-migration.md): its name held, its CPUs taken, its number, what
/// its tasks share and what its disks are served for -- everything a start
/// takes but the guest, which the migration builds over stand-ins, fills,
/// and gives its volumes and its port at the handover (`Vms::arrived`).
/// Dropped, all of it goes back.
pub struct Coming<'a> {
    held: Reserved<'a>,
    core_hold: Hold,
    pub id: u32,
    shared: Arc<Shared>,
    pub runner: Runner,
    placement: Vec<u32>,
    disk_cpu: u32,
    pub spec: Spec,
}

impl Coming<'_> {
    /// Its guest, built over stand-ins as `disks` and `nic` say the old
    /// host's was (`guest::build_coming`).
    pub fn build(&self, machine: &Machine, disks: &[hv::disk::Shape], nic: Option<([u8; 6], u16)>)
        -> Result<Built, String> {
        guest::build_coming(machine, &self.spec, self.shared.doorbells.clone(), disks, nic)
    }
}

/// A running VM held for its state to be taken (`Vms::take_for_state`): its
/// guest, its name and number -- and, until it is given back or ended, its
/// stop held, so that nothing else stops or restarts it meanwhile.
pub struct StateHold<'a> {
    vms: &'a Vms,
    shared: Arc<Shared>,
    pub guest: Arc<LinuxGuest>,
    pub name: String,
    pub id: u32,
    /// Its folder's path, and its disks' words as it was started with.
    pub folder: String,
    pub disks: Vec<String>,
    /// Its port of the switch, with `net`.
    pub port: Option<usize>,
    /// The host CPU its disks are served on.
    disk_cpu: Option<u32>,
    done: bool,
}

impl StateHold<'_> {
    /// What a disk opened for it now is served by, as its others are: its
    /// task on the VM's disks' CPU, waking its vCPUs -- a migration's
    /// volume taken back (`QueuedDisk::open_handover`).
    pub fn runner(&self) -> Option<Runner> {
        Some(Runner {
            wake: self.shared.clone(),
            name: alloc::format!("vm{}", self.id),
            cpu: self.disk_cpu?,
            claims: self.vms.claims.clone(),
            holder: Holder::Vm(self.id),
            owner: alloc::format!("vm {}'s disk", self.name),
        })
    }

    /// Given back as it was: running, its guest gone on (`LinuxGuest::resume`
    /// is the caller's), and stoppable again.
    pub fn give_back(mut self) {
        self.done = true;
        self.shared.held.store(0, Ordering::Release);
        self.shared.stopping.store(false, Ordering::Release);
    }

    /// Ended -- its state taken -- as `hv stop` ends one: the guest stopped
    /// with `stop` where it is, paused or not, and the VM off the list, its
    /// task joined, its port, forwards and CPUs given back. Its folder is
    /// the caller's.
    pub fn end(mut self, stop: Stop, forwards: &Forwards) -> Result<(), String> {
        self.done = true;
        let id = self.id;
        self.shared.stop.store(true, Ordering::Release);
        self.guest.stop(stop);
        self.shared.ring_all();
        let vm = {
            let mut table = self.vms.table.lock();
            table.vms.iter().position(|vm| vm.shared.id == id).map(|i| table.vms.remove(i))
        };
        let vm = vm.ok_or_else(|| alloc::format!("vm {} was taken off the list meanwhile", id))?;
        let port = vm.port;
        /* The task joined: the guest's memory goes back with this hold's
         * share of it, the last. */
        finish(vm);
        forwards.drop_vm(id);
        self.vms.release(port);
        Ok(())
    }
}

impl Drop for StateHold<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.shared.held.store(0, Ordering::Release);
            self.shared.stopping.store(false, Ordering::Release);
        }
    }
}

/// What holds a running VM for its state to be taken (`Vms::take_for_state`),
/// said as its state on the list -- `hv list`'s, the host API's -- while the
/// hold has it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// `hv save`: paused, written into a file and ended -- "saving".
    Save = 1,
    /// `hv migrate`, the `migrate` job: copied as it runs, then paused and
    /// gone on on another host -- "moving".
    Move = 2,
    /// `hv logcheck`: its dirty log checked as it runs -- "running".
    Check = 3,
}

/// Every VM this module has started and not yet taken off the list.
pub struct Vms {
    table: Mutex<Table>,
    /// The guests' switch, made by the first `net` guest and kept until the
    /// module goes.
    switch: Mutex<Option<Arc<Switch>>>,
    /// The claims the guests' disks hold on their images -- `hv boot`'s
    /// too, which takes them from here.
    claims: Arc<Claims>,
    /// The vm.toml changes, one at a time (`store::Changes`).
    pub changes: store::Changes,
    /// The host's firewalls, and their places in the switch -- the
    /// cluster's too, which the keeper changes as vnet does.
    pub firewalls: Arc<crate::firewalls::Firewalls>,
    /// What hv keeps of vnet, made with the switch, and its task: stopped
    /// and waited out before the switch goes (`close`).
    keeper: Mutex<Option<(Arc<crate::vnet::Keeper>, TaskHandle)>>,
    /// The cluster networks' tunnel to the other hosts, made with the
    /// switch, and its task: stopped first of all (`close`).
    tunnel: Mutex<Option<(Arc<crate::tunnel::Tunnel>, TaskHandle)>>,
    /// The tunnel's keys given anew every this many seconds, and the one
    /// before kept for this many (`insmod hv.ko rekey= keylife=`).
    rekey_s: u64,
    keylife_s: u64,
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
        _ => Err(alloc::format!("hv stop <id|name|all> [secs=0..{}]", WAIT_MAX_S)),
    }
}

/// Wait until none of the guests `waiting` finds is still shutting down
/// after its power button was pressed, or `secs` have gone by: `waiting`
/// asked at every look, and pressing the button again of each it finds
/// (`Shared::press_again`).
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

/// What a start has made by the time its guest is built, for the rest of it
/// (`Vms::run_listed`): its number and name, its folder, what its tasks
/// share, the guest, the spec it boots again from, where its CPUs and disks
/// run and its hold on them, its name's hold, its port of the switch and the
/// port of vnet's it is, and the state it was restored from, if it was.
struct Listing<'a, 'o> {
    machine: &'a Arc<Machine>,
    forwards: &'a Forwards,
    id: u32,
    name: Option<&'a str>,
    stored: &'a Option<(Folder, bool)>,
    logged: bool,
    shared: Arc<Shared>,
    built: Built,
    spec: Spec,
    runner: Runner,
    placement: Vec<u32>,
    disk_cpu: u32,
    has_disks: bool,
    core_hold: Hold,
    held: Option<Reserved<'a>>,
    hold: Option<PortHold>,
    attached: Option<([u8; 16], crate::vnet::Attached)>,
    restored: Option<&'a str>,
    out: &'o mut dyn Write,
}

/// Stop a VM taken off the table and wait for its task: the flag, then the
/// join, which returns once the guest has stopped and been freed.
fn finish(mut vm: Vm) {
    vm.shared.stop.store(true, Ordering::Release);
    vm.shared.ring_all();
    drop(vm.task.take());
    if let Some(log) = vm.log.take() {
        log.finish();
    }
}

/// Whether what a shell printed after a line was fed to it is the line's
/// whole answer: the echo of its newline, then everything up to a prompt.
fn at_prompt(text: &[u8]) -> bool {
    text.contains(&b'\n') && (text.ends_with(b"# ") || text.ends_with(b"$ "))
}

impl Vms {
    pub fn new(rekey_s: u64, keylife_s: u64) -> Option<Vms> {
        let mut vms = Vec::new();
        vms.try_reserve_exact(MAX_VMS).ok()?;
        let mut reserved = Vec::new();
        reserved.try_reserve_exact(MAX_RESERVED).ok()?;
        Some(Vms {
            table: Mutex::new(Table { next: 0, vms, closing: false, reserved })?,
            switch: Mutex::new(None)?,
            claims: Arc::new(Claims::new()?),
            changes: store::Changes::new()?,
            firewalls: Arc::new(crate::firewalls::Firewalls::new()?),
            keeper: Mutex::new(None)?,
            tunnel: Mutex::new(None)?,
            rekey_s,
            keylife_s,
        })
    }

    /// The claims the guests' disks hold on their images (`disk::Claims`).
    pub fn claims(&self) -> Arc<Claims> {
        self.claims.clone()
    }

    /// `name` held for what is done to its folder -- `what`: `MADE`,
    /// `CLONED`, `DELETED`, `CHANGED` -- until the hold goes: no VM of it
    /// starts meanwhile, and nothing else of the kind is done to it. Refused
    /// while a VM of the name is on the list, the name is held already, or
    /// the module is going.
    pub fn reserve(&self, name: &str, what: &'static str) -> Result<Reserved<'_>, String> {
        let mut kept = String::new();
        let mut held = String::new();
        if kept.try_reserve_exact(name.len()).is_err() || held.try_reserve_exact(name.len()).is_err() {
            return Err(String::from("out of memory"));
        }
        kept.push_str(name);
        held.push_str(name);
        /* Asked and taken under the lock, said after it. */
        enum No {
            Closing,
            Listed(u32),
            Doing(&'static str),
            Full,
        }
        let refused = {
            let mut table = self.table.lock();
            if table.closing {
                Some(No::Closing)
            } else if let Some(vm) = table.vms.iter().find(|vm| vm.name.as_deref() == Some(name)) {
                Some(No::Listed(vm.shared.id))
            } else if let Some(r) = table.reserved.iter().find(|r| r.name == name) {
                Some(No::Doing(r.doing))
            } else if table.reserved.len() >= MAX_RESERVED {
                Some(No::Full)
            } else {
                /* Into the room taken when the table was made. */
                table.reserved.push(Reservation { name: kept, doing: what, port: None, devices: Vec::new() });
                None
            }
        };
        match refused {
            None => Ok(Reserved { vms: self, name: held }),
            Some(No::Closing) => Err(String::from("the module is being unloaded")),
            Some(No::Listed(id)) => Err(alloc::format!("vm {} is {} -- hv stop {} first", id, name, name)),
            Some(No::Doing(doing)) => Err(alloc::format!("{} is being {} -- not now", name, doing)),
            Some(No::Full) => Err(alloc::format!("{} vms are being made, cloned or deleted already -- not now",
                                                 MAX_RESERVED)),
        }
    }

    /// The name of a VM about to be started, held from the start's first
    /// look until the VM is on the list -- while its kernel and initrd are
    /// read, which may be long -- so that a second start of it, a delete or
    /// a clone of its folder, refused by the hold, cannot run beside it:
    /// two starts at once once both read a vm.toml with no UUID and wrote
    /// one each, and a delete took away the folder a start was booting.
    fn reserve_start(&self, name: &str) -> Result<Reserved<'_>, String> {
        let mut kept = String::new();
        let mut held = String::new();
        if kept.try_reserve_exact(name.len()).is_err() || held.try_reserve_exact(name.len()).is_err() {
            return Err(String::from("out of memory"));
        }
        kept.push_str(name);
        held.push_str(name);
        let refused = {
            let mut table = self.table.lock();
            match Self::name_taken(&table, Some(name)) {
                Some(taken) => Some(taken.said(name)),
                None if table.reserved.len() >= MAX_RESERVED => {
                    Some(alloc::format!("{} vms are being made, cloned, deleted or started already -- not now",
                                        MAX_RESERVED))
                }
                None => {
                    /* Into the room taken when the table was made. */
                    table.reserved.push(Reservation { name: kept, doing: STARTED, port: None, devices: Vec::new() });
                    None
                }
            }
        };
        match refused {
            None => Ok(Reserved { vms: self, name: held }),
            Some(why) => Err(why),
        }
    }

    /// The VM of `name` on the list, if one is: what a start that holds the
    /// name asks again as it takes its number and goes on the list.
    fn listed_as(table: &Table, name: Option<&str>) -> Option<Taken> {
        let n = name?;
        table.vms.iter().find(|vm| vm.name.as_deref() == Some(n)).map(|vm| Taken::Listed(vm.shared.id))
    }

    /// What is being done to the folder of `name`, if it is held.
    pub fn doing(&self, name: &str) -> Option<&'static str> {
        self.table.lock().reserved.iter().find(|r| r.name == name).map(|r| r.doing)
    }

    /// The devices given to VMs being made, cloned or changed -- all but
    /// `except`'s -- whose vm.tomls do not say so yet (`Reserved::keep_devices`):
    /// each VM's name and a device's id. What fits in memory.
    pub fn devices_given(&self, except: &str) -> Vec<(String, String)> {
        let mut given = Vec::new();
        let table = self.table.lock();
        for r in table.reserved.iter().filter(|r| r.name != except) {
            for id in &r.devices {
                let (mut n, mut i) = (String::new(), String::new());
                if given.try_reserve(1).is_err() || n.try_reserve_exact(r.name.len()).is_err()
                    || i.try_reserve_exact(id.len()).is_err()
                {
                    return given;
                }
                n.push_str(&r.name);
                i.push_str(id);
                given.push((n, i));
            }
        }
        given
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

    /// The VM `word` names: its number, or the name it was started from its
    /// folder by. The two cannot be taken for each other: a name is never
    /// digits alone (`store::reserved`), so a word that reads as a number
    /// is a number -- a VM named `1` was once reached as whichever VM was
    /// number 1.
    fn find(&self, word: &str) -> Result<Arc<Shared>, String> {
        let table = self.table.lock();
        Self::found(&table, word).map(|vm| vm.shared.clone()).ok_or_else(|| alloc::format!("no vm {}", word))
    }

    /// `find`'s lookup, in the table its caller holds locked.
    fn found<'a>(table: &'a Table, word: &str) -> Option<&'a Vm> {
        let by_id = word.parse::<u32>().ok().and_then(|id| table.vms.iter().find(|vm| vm.shared.id == id));
        by_id.or_else(|| table.vms.iter().find(|vm| vm.name.as_deref() == Some(word)))
    }

    /// What each disk of the VM `word` names (`find`) has done since its
    /// guest's boot, its word as the VM was started with -- and when that
    /// was read; no disks while its guest does not run (stopped, or between
    /// boots). The counters are read under `serving`'s lock, the guest
    /// borrowed and never held past it: a guest is let go of by its own
    /// task alone.
    pub fn disk_io(&self, word: &str) -> Result<DiskIo, String> {
        let oom = || String::from("out of memory");
        let (shared, words) = {
            let table = self.table.lock();
            let vm = Self::found(&table, word).ok_or_else(|| alloc::format!("no vm {}", word))?;
            let mut words = Vec::new();
            words.try_reserve_exact(vm.disks.len()).map_err(|_| oom())?;
            for w in &vm.disks {
                words.push(copied(w).ok_or_else(oom)?);
            }
            (vm.shared.clone(), words)
        };
        let at_ns = kcore::time::boot_time_ns();
        let stats = shared.serving.lock().as_ref().map(|guest| guest.disk_stats());
        let restarts = shared.restarts.load(Ordering::Relaxed);
        let Some(stats) = stats else {
            return Ok(DiskIo { running: false, restarts, at_ns, disks: Vec::new() });
        };
        /* One a disk, in the order it was given its disks; none at all when
         * there was no memory to say them in */
        if stats.len() != words.len() {
            return Err(oom());
        }
        let mut disks = Vec::new();
        disks.try_reserve_exact(words.len()).map_err(|_| oom())?;
        for (w, (_, s, _, _)) in words.into_iter().zip(stats) {
            disks.push((w, DiskCounts {
                reads: s.reads, writes: s.writes, read_bytes: s.read_bytes, written_bytes: s.written_bytes,
                flushes: s.flushes, errors: s.errors, discards: s.discards, zeroings: s.zeroings,
                wait_ns: s.wait_ns,
            }));
        }
        Ok(DiskIo { running: true, restarts, at_ns, disks })
    }

    /// The number of the VM `word` names (`find`).
    pub fn id_of(&self, word: &str) -> Result<u32, String> {
        self.find(word).map(|shared| shared.id)
    }

    /// The folder the VM of this name on the list was started from, if one
    /// is: what its vm.toml is read from while it runs -- its path may be
    /// anywhere, not the VMs' folder's.
    pub fn folder_listed(&self, name: &str) -> Option<Folder> {
        let table = self.table.lock();
        let vm = table.vms.iter().find(|vm| vm.name.as_deref() == Some(name))?;
        let path = vm.folder.as_ref()?;
        let (mut n, mut p) = (String::new(), String::new());
        if n.try_reserve_exact(name.len()).is_err() || p.try_reserve_exact(path.len()).is_err() {
            return None;
        }
        n.push_str(name);
        p.push_str(path);
        Some(Folder { name: n, path: p })
    }

    /// The VM of this name on the list, if one is: its number, and whether
    /// it runs.
    pub fn status_of(&self, name: &str) -> Option<(u32, bool)> {
        let table = self.table.lock();
        table.vms.iter().find(|vm| vm.name.as_deref() == Some(name)).map(|vm| (vm.shared.id, vm.shared.running()))
    }

    /// `hv start <kernel> [words]`, or `hv start <name|folder>`: a guest by
    /// its kernel's path and the words after it, or a VM from its folder --
    /// a name's under `root` -- built, put on a vCPU task, and come back.
    pub fn start(&self, machine: &Arc<Machine>, cores: &Cores, forwards: &Forwards, root: &str, args: &str,
                 out: &mut dyn Write) {
        if let Err(e) = hv::run::ensure_runnable(machine) {
            let _ = writeln!(out, "hv: no guest can run here -- {}", e);
            return;
        }
        let (first, rest) = after_word(args);
        let started = match store::find(root, first) {
            Ok(Some(folder)) => {
                /* As its vm.toml says -- but `force`, which no file says */
                let force = match rest.trim() {
                    "" => false,
                    "force" => true,
                    _ => {
                        let _ = writeln!(out, "hv: a vm started from its folder is started as its {} says -- no words \
                                               after its name but force, for its volumes of NBS's", store::CONFIG);
                        return;
                    }
                };
                self.start_forced(machine, cores, forwards, folder, force, out)
            }
            Ok(None) => match guest::parse(args, START_USAGE) {
                Ok(spec) if spec.secs.is_none() => self.launch(machine, cores, forwards, spec, None, Origin::Boot, out),
                Ok(_) => Err(String::from("secs= is hv boot's -- a started vm runs until hv stop")),
                Err(usage) => {
                    let _ = writeln!(out, "{}", usage);
                    return;
                }
            },
            Err(e) => Err(e),
        };
        if let Err(e) = started {
            let _ = writeln!(out, "hv: {}", e);
        }
    }

    /// The VM of `folder` started as its vm.toml says: its number, or why
    /// not -- what a start of it by name, from the shell or the host API,
    /// comes to.
    pub fn start_from(&self, machine: &Arc<Machine>, cores: &Cores, forwards: &Forwards, folder: Folder,
                      out: &mut dyn Write) -> Result<u32, String> {
        self.start_forced(machine, cores, forwards, folder, false, out)
    }

    /// As `start_from`, its volumes of NBS's taken from a writer elsewhere
    /// with `force`.
    fn start_forced(&self, machine: &Arc<Machine>, cores: &Cores, forwards: &Forwards, folder: Folder, force: bool,
                    out: &mut dyn Write) -> Result<u32, String> {
        hv::run::ensure_runnable(machine).map_err(|e| alloc::format!("no guest can run here -- {}", e))?;
        let config = store::load(&folder)?;
        let mut spec = store::options_checked(&folder, &config)
            .map_err(|e| alloc::format!("{}/{}: {}", folder.path, store::CONFIG, e))?;
        spec.force = force;
        let log = config.console_log;
        self.launch(machine, cores, forwards, spec, Some((folder, log)), Origin::Boot, out)
    }

    /// The VM of `folder` started from a state (`origin`): its machine as its
    /// vm.toml describes it, the state put into it -- `hv restore`'s, a
    /// migration's.
    pub fn start_restored(&self, machine: &Arc<Machine>, cores: &Cores, forwards: &Forwards, folder: Folder,
                          origin: Origin<'_>, out: &mut dyn Write) -> Result<u32, String> {
        hv::run::ensure_runnable(machine).map_err(|e| alloc::format!("no guest can run here -- {}", e))?;
        let config = store::load(&folder)?;
        let spec = store::options_checked(&folder, &config)
            .map_err(|e| alloc::format!("{}/{}: {}", folder.path, store::CONFIG, e))?;
        let log = config.console_log;
        self.launch(machine, cores, forwards, spec, Some((folder, log)), origin, out)
    }

    /// The running VM `word` names held for its state to be taken: its guest,
    /// name and number, its stop held until the hold is given back or the VM
    /// ended (`StateHold`), the list saying what holds it (`why`). A VM of a
    /// folder alone -- that is what it is started from again -- running, and
    /// being stopped by nothing else.
    pub fn take_for_state(&self, word: &str, why: Held) -> Result<StateHold<'_>, String> {
        let id = self.id_of(word)?;
        let (shared, name, folder, disks, disk_cpu, port) = {
            let table = self.table.lock();
            let vm = table.vms.iter().find(|vm| vm.shared.id == id).ok_or_else(|| alloc::format!("no vm {}", id))?;
            let mut disks = Vec::new();
            disks.try_reserve_exact(vm.disks.len()).map_err(|_| String::from("out of memory"))?;
            disks.extend(vm.disks.iter().cloned());
            (vm.shared.clone(), vm.name.clone(), vm.folder.clone(), disks, vm.disk_cpu, vm.port)
        };
        let (Some(name), Some(folder)) = (name, folder) else {
            return Err(alloc::format!("vm {} has no folder: only a VM of one -- what it is started from again -- has \
                                       its state taken", id));
        };
        if shared.stopping.swap(true, Ordering::AcqRel) {
            return Err(alloc::format!("vm {} is being stopped", id));
        }
        let guest = shared.serving.lock().clone();
        let Some(guest) = guest else {
            shared.stopping.store(false, Ordering::Release);
            return Err(alloc::format!("vm {} is not running", id));
        };
        shared.held.store(why as u8, Ordering::Release);
        Ok(StateHold { vms: self, shared, guest, name, id, folder, disks, disk_cpu, port, done: false })
    }

    /// Why a VM of `name` may not start now -- one on the list already, the
    /// name held for its folder -- asked under the table's lock.
    fn name_taken(table: &Table, name: Option<&str>) -> Option<Taken> {
        let n = name?;
        if let Some(vm) = table.vms.iter().find(|vm| vm.name.as_deref() == Some(n)) {
            return Some(Taken::Listed(vm.shared.id));
        }
        table.reserved.iter().find(|r| r.name == n).map(|r| Taken::Doing(r.doing))
    }

    /// A VM of `name` coming from another host (`Coming`), as `spec` -- its
    /// vm.toml's -- describes it: its name held, its CPUs taken, its number
    /// and what its tasks share made, before its guest is built.
    pub fn coming(&self, machine: &Arc<Machine>, cores: &Cores, name: &str, mut spec: Spec, logged: bool)
        -> Result<Coming<'_>, String> {
        hv::run::ensure_runnable(machine).map_err(|e| alloc::format!("no guest can run here -- {}", e))?;
        let held = self.reserve_start(name)?;
        let has_disks = !spec.disks.is_empty();
        let (place, core_hold) = cores.take(machine, spec.cpu, spec.numa, spec.cpus, has_disks, None)?;
        spec.placed(&place);
        let id = self.number(Some(name))?;
        core_hold.held_by(Holder::Vm(id));
        spec.name = String::from(name);
        spec.input.clear();
        let shared = Shared::new(id, name, spec.log, &[], spec.cpus, logged)
            .map(Arc::new)
            .ok_or_else(|| alloc::format!("vm {} not made -- out of memory for its console", id))?;
        let runner = Runner {
            wake: shared.clone(),
            name: alloc::format!("vm{}", id),
            cpu: place.disk,
            claims: self.claims.clone(),
            holder: Holder::Vm(id),
            owner: alloc::format!("vm {}'s disk", name),
        };
        Ok(Coming { held, core_hold, id, shared, runner, placement: place.vcpus, disk_cpu: place.disk, spec })
    }

    /// The port of vnet's that VM `uuid`'s NIC is, moved here, had on the
    /// switch for the VM `coming` is: attached as vnet's state now has it,
    /// judged by its firewall of the cluster's. The port of the switch, and
    /// the attach, for the keeper.
    pub fn claim_cluster_port(&self, coming: &Coming<'_>, uuid: &[u8; 16], network: &str)
        -> Result<(usize, Arc<Switch>, crate::vnet::Attached), String> {
        let shared = &coming.shared;
        let switch = self.switch()?;
        let cluster = crate::vnet::cluster_network(network)
            .ok_or_else(|| alloc::format!("{} is no network of the cluster's", network))?;
        let a = crate::vnet::attach(uuid, cluster, None, None, false)?;
        let place = self.firewalls.place_cluster(&switch, &a.firewall)
            .map_err(|why| alloc::format!("its firewall: {}", why))?;
        let port = switch.claim_vnet(shared, &a.identity, place)?;
        Ok((port, switch, a))
    }

    /// A VM come from another host (`coming`), its guest `built` and its
    /// state in, its volumes and its port had -- `nic`, the port of the
    /// switch, the switch, its UUID and vnet's attach of it -- started here:
    /// listed as `launch` lists one, its task running the guest from where
    /// it stood, its folder `folder`. Its number, or why not.
    #[allow(clippy::too_many_arguments)]
    pub fn arrived(&self, machine: &Arc<Machine>, forwards: &Forwards, coming: Coming<'_>, built: Built,
                   folder: Folder, logged: bool, nic: Option<(usize, Arc<Switch>, [u8; 16], crate::vnet::Attached)>,
                   what: &str, out: &mut dyn Write) -> Result<u32, String> {
        let Coming { held, core_hold, id, shared, runner, placement, disk_cpu, mut spec } = coming;
        let has_disks = !spec.disks.is_empty();
        let (hold, attached) = match nic {
            Some((port, switch, uuid, a)) => {
                /* What its next boot here is told, a reset's under `restart`:
                 * its address, as `launch` tells a VM it starts */
                spec.cmdline.push(' ');
                spec.cmdline.push_str(&net::ip_param_at(a.identity.ip, a.identity.gateway(), a.identity.mask(),
                                                        switch.dns()));
                spec.nic = Some(NicSpec { port, switch: switch.clone(), mtu: a.identity.mtu });
                (Some(PortHold { switch, port, kept: false }), Some((uuid, a)))
            }
            None => (None, None),
        };
        let stored = Some((folder, logged));
        let name = stored.as_ref().map(|(f, _)| f.name.as_str());
        self.run_listed(Listing {
            machine, forwards, id, name, stored: &stored, logged, shared, built, spec, runner, placement, disk_cpu,
            has_disks, core_hold, held: Some(held), hold, attached, restored: Some(what), out,
        })
    }

    /// A number for a VM about to start, `name` its name if it has one: none
    /// while the module goes, the list is full, or a VM of that name is on
    /// it.
    fn number(&self, name: Option<&str>) -> Result<u32, String> {
        enum No {
            Closing,
            Full,
            Taken(Taken),
            Numbers,
        }
        let id = {
            let mut table = self.table.lock();
            let no = if table.closing {
                Some(No::Closing)
            } else if table.vms.len() >= MAX_VMS {
                Some(No::Full)
            } else if let Some(taken) = Self::listed_as(&table, name) {
                Some(No::Taken(taken))
            } else {
                None
            };
            match no {
                Some(no) => Err(no),
                None => match table.next.checked_add(1) {
                    Some(next) => Ok(core::mem::replace(&mut table.next, next)),
                    None => Err(No::Numbers),
                },
            }
        };
        id.map_err(|no| match no {
            No::Closing => String::from("the module is being unloaded"),
            No::Full => alloc::format!("{} vms already -- hv stop one first", MAX_VMS),
            No::Taken(taken) => taken.said(name.unwrap_or("")),
            No::Numbers => String::from("out of vm numbers -- reload the module"),
        })
    }

    /// Start the guest `spec` describes -- from its folder, given one, and
    /// whether to keep its console.log there: its number, or why not, which
    /// the hypervisor's log has too.
    fn launch(&self, machine: &Arc<Machine>, cores: &Cores, forwards: &Forwards, spec: Spec,
              stored: Option<(Folder, bool)>, origin: Origin<'_>, out: &mut dyn Write) -> Result<u32, String> {
        let mut who = String::new();
        let _ = who.try_reserve(spec.kernel.len() + LABEL_BYTES);
        match &stored {
            Some((folder, _)) => {
                let _ = write!(who, "vm {}", folder.name);
            }
            None => {
                let _ = write!(who, "a vm of {}", spec.kernel);
            }
        }
        let started = self.launch_one(machine, cores, forwards, spec, stored, origin, out);
        if let Err(why) = &started {
            kcore::flog_limited!(crate::HV, "{} not started -- {}", who, why_alone(why));
        }
        started
    }

    fn launch_one(&self, machine: &Arc<Machine>, cores: &Cores, forwards: &Forwards, mut spec: Spec,
                  stored: Option<(Folder, bool)>, origin: Origin<'_>, out: &mut dyn Write) -> Result<u32, String> {
        /* A guest of a state is typed nothing at: what its vm.toml queues is
         * for its boot, long past. */
        if matches!(origin, Origin::State { .. }) {
            spec.input.clear();
        }
        let name = stored.as_ref().map(|(f, _)| f.name.as_str());
        /* A name on the list already -- or held while its folder is made,
         * cloned, deleted or started -- is said first, and before anything
         * is taken for it: not a want of cores. And from here it is held,
         * until the VM is on the list or the start has failed. */
        let held = match name {
            Some(n) => Some(self.reserve_start(n)?),
            None => None,
        };
        /* Its place, and its hold on it, before anything is made: two
         * starts at once are given cores apart. The hold is named once the
         * VM has a number, and given back with the task -- or here, on the
         * way out of a start that goes no further. */
        let has_disks = !spec.disks.is_empty();
        let (place, core_hold) = cores.take(machine, spec.cpu, spec.numa, spec.cpus, has_disks, None)?;
        spec.placed(&place);
        let placement = place.vcpus;
        let disk_cpu = place.disk;
        if spec.input.len() > INPUT_MAX {
            return Err(alloc::format!("input= is {} bytes, more than the {} a vm queues", spec.input.len(), INPUT_MAX));
        }
        let id = self.number(name)?;
        core_hold.held_by(Holder::Vm(id));

        /* What its SMBIOS calls it, and the UUID it gives it: its vm.toml's
         * -- or, a VM from a folder at its first start since VMs had them,
         * one made now and written into the file, every start after saying
         * the same. One with no folder has a new one at each start; its
         * restarts keep it. */
        spec.name = match name {
            Some(n) => String::from(n),
            None => alloc::format!("vm{}", id),
        };
        if spec.uuid.is_none() {
            let mut uuid = guest::random_uuid();
            if let Some((folder, _)) = &stored {
                match store::keep_uuid(self, folder, uuid) {
                    /* Ours, or one a start at the same moment kept first */
                    Ok(kept) => uuid = kept,
                    Err(e) => {
                        let _ = writeln!(out, "hv: its UUID is not kept -- {}: a new one at its next start", e);
                    }
                }
            }
            spec.uuid = Some(uuid);
        }

        let logged = stored.as_ref().is_some_and(|(_, log)| *log);
        let shared = Shared::new(id, &spec.name, spec.log, &spec.input, spec.cpus, logged)
            .map(Arc::new)
            .ok_or_else(|| alloc::format!("vm {} not started -- out of memory for its console", id))?;
        let not_started = |why: &dyn core::fmt::Display| alloc::format!("vm {} not started -- {}", id, why);

        /* A VM from a folder whose vm.toml names no port is given one now,
         * kept there as its UUID is: its MAC and its address the same at
         * every start after, whatever order the VMs start in -- a guest's
         * own network configuration names them (`store::keep_port`). Not on
         * a network of the cluster's: there its address and MAC are its
         * port of vnet's, the same at every start already, and a vm.toml
         * of one names no port -- `port=` is refused there, below. */
        let on_cluster = spec.network.as_deref().and_then(crate::vnet::cluster_network).is_some();
        if spec.net && spec.port.is_none() && !on_cluster {
            if let Some((folder, _)) = &stored {
                match store::keep_port(self, folder) {
                    Ok(p) => spec.port = Some(p as usize),
                    Err(e) => {
                        let _ = writeln!(out, "hv: its port of the switch is not kept -- {}: a free one now, maybe \
                                               another at its next start", e);
                    }
                }
            }
        }

        /* With `net`, a port on the switch -- made by the first such guest --
         * and the address that goes with it on the guest's command line;
         * given back if the start goes no further. Judged by its firewall:
         * the one it names; a VM from a folder that names none given one at
         * this, its first start since VMs had them (`Firewalls::adopt`);
         * `default` for one with no folder. */
        let mut hold = None;
        let mut attached = None;
        if spec.net {
            let switch = self.switch().map_err(|why| not_started(&why))?;
            let cluster = spec.network.as_deref().and_then(crate::vnet::cluster_network);
            let (port, mtu) = match cluster {
                /* A network of the cluster's: its port vnet's, as its address,
                 * its firewall and its public address are -- read from the
                 * state the leader vouches for, made at its first start where
                 * `hv create` did not make it. */
                Some(network) => {
                    if spec.port.is_some() || spec.public.is_some() {
                        return Err(not_started(&"port= and public= are the host's networks': on a network of the \
                                                  cluster's its port is vnet's, and its public address (`vnet ip`)"));
                    }
                    let uuid = match (&stored, spec.uuid) {
                        (Some(_), Some(uuid)) => uuid,
                        _ => return Err(not_started(&"a VM with no folder is on no network of the cluster's: its \
                                                       UUID, which its port is vnet's by, is new at each start")),
                    };
                    let a = crate::vnet::attach(&uuid, network, None, None, true).map_err(|why| not_started(&why))?;
                    let place = self.firewalls.place_cluster(&switch, &a.firewall)
                        .map_err(|why| not_started(&alloc::format!("its firewall: {}", why)))?;
                    let port = switch.claim_vnet(&shared, &a.identity, place).map_err(|why| not_started(&why))?;
                    if a.made {
                        let _ = writeln!(out, "hv: vm {} given port {} on {}{} at {}", id, a.port, crate::vnet::PREFIX,
                                         network, netwire::Ipv4(a.identity.ip));
                    }
                    if !a.confirmed {
                        let _ = writeln!(out, "hv: vm {}'s port on {}{} is unconfirmed -- vnet has no leader to \
                                               vouch for it, and is asked again until it has", id,
                                         crate::vnet::PREFIX, network);
                    }
                    if let Some(public) = a.public {
                        let _ = writeln!(out, "hv: vm {}'s public address {} is vnet's, its gateway's to carry -- not \
                                               on this host's NAT yet", id, netwire::Ipv4(public));
                    }
                    spec.cmdline.push(' ');
                    spec.cmdline.push_str(&net::ip_param_at(a.identity.ip, a.identity.gateway(), a.identity.mask(),
                                                            switch.dns()));
                    let mtu = a.identity.mtu;
                    attached = Some((uuid, a));
                    (port, mtu)
                }
                None => {
                    if spec.firewall.is_none() {
                        if let Some((folder, _)) = &stored {
                            let (named, said) = self.firewalls.adopt(self, folder, &spec.forwards);
                            let _ = writeln!(out, "hv: {}", said);
                            spec.firewall = Some(named);
                        }
                    }
                    let firewall = spec.firewall.as_deref().unwrap_or(crate::firewalls::DEFAULT);
                    let place = self.firewalls.place(&switch, firewall)
                        .map_err(|why| not_started(&alloc::format!("its firewall: {}", why)))?;
                    let claimed = match spec.port {
                        Some(p) => switch.claim_at(p, &shared, spec.network.as_deref(), place),
                        None => switch.claim(&shared, spec.network.as_deref(), place),
                    };
                    let port = claimed.map_err(|why| not_started(&why))?;
                    spec.cmdline.push(' ');
                    spec.cmdline.push_str(&net::ip_param(port, switch.dns()));
                    (port, 0)
                }
            };
            /* Its way out, and the DNS server it is told of: a guest with no
             * way out still reaches nos and the other guests. */
            if let Err(why) = switch.way_out() {
                let _ = writeln!(out, "hv: vm {} has no way out of the switch -- {}", id, net::nat_why(why));
            }
            spec.nic = Some(NicSpec { port, switch: switch.clone(), mtu });
            hold = Some(PortHold { switch: switch.clone(), port, kept: false });
            /* Its public address, one to one: a start asked for one that
             * cannot have it does not go on without it -- the port given back
             * with the hold. */
            if let Some(public) = spec.public {
                switch.map_public(port, public).map_err(|why| {
                    not_started(&alloc::format!("its public address {}: {}", netwire::Ipv4(public), why))
                })?;
                let _ = writeln!(out, "hv: vm {} is reached at {}, one to one -- what its firewall lets in", id,
                                 netwire::Ipv4(public));
            }
        }

        /* Its disks are served off the vCPU's CPU, and wake it. */
        let mut label = String::new();
        label.try_reserve(16).map_err(|_| not_started(&"out of memory"))?;
        let _ = write!(label, "vm{}", id);
        let owner = match name {
            Some(n) => alloc::format!("vm {}'s disk", n),
            None => alloc::format!("vm {}'s disk", id),
        };
        let runner = Runner {
            wake: shared.clone(),
            name: label,
            cpu: disk_cpu,
            claims: self.claims.clone(),
            holder: Holder::Vm(id),
            owner,
        };

        /* The files are read and the guest's memory filled here, with no lock
         * held: it takes as long as the kernel and the initrd take to read. */
        let (built, restored) = match origin {
            Origin::Boot => (guest::build(machine, &spec, &runner, shared.doorbells.clone()), None),
            Origin::State { state, fill, what } => {
                (guest::build_restored(machine, &spec, &runner, shared.doorbells.clone(), state, fill), Some(what))
            }
        };
        let built = built.map_err(|why| not_started(&why))?;
        /* `force` was this attach's -- an operator's word for a volume whose
         * writer's host is gone -- and is no boot's after it: kept in the
         * spec the task boots again from, a reset under `restart` or an `hv
         * restart` took the volume back by force from whichever host had
         * taken it since, and two hosts fenced each other in turn. A later
         * boot attaches plainly, and where another host writes, it fails and
         * says so. */
        spec.force = false;
        self.run_listed(Listing {
            machine, forwards, id, name, stored: &stored, logged, shared, built, spec, runner, placement, disk_cpu,
            has_disks, core_hold, held, hold, attached, restored, out,
        })
    }

    /// A VM's start from where its guest is built: its task started on its
    /// first CPU's host CPU, running the guest, and the VM put on the list --
    /// its name the listed VM's from then, its port kept by the keeper, its
    /// log begun, nos's ports it asks for relayed. Its number, or why not.
    fn run_listed(&self, l: Listing<'_, '_>) -> Result<u32, String> {
        let Listing {
            machine, forwards, id, name, stored, logged, shared, built, spec, runner, placement, disk_cpu,
            has_disks, core_hold, mut held, mut hold, attached, restored, out,
        } = l;
        let not_started = |why: &dyn core::fmt::Display| alloc::format!("vm {} not started -- {}", id, why);
        let cpu = placement[0];
        let mut cpus = Vec::new();
        cpus.try_reserve_exact(placement.len()).map_err(|_| not_started(&"out of memory"))?;
        cpus.extend_from_slice(&placement);
        let mem_mib = spec.mem_bytes / (1024 * 1024);
        let mut kernel = String::new();
        let mut task_name = String::new();
        let mut said = String::new();
        let mut host_cpus = String::new();
        if kernel.try_reserve_exact(spec.kernel.len()).is_err() || task_name.try_reserve(16).is_err()
            || said.try_reserve(SAID_BYTES + spec.kernel.len() + spec.cmdline.len()).is_err()
            || host_cpus.try_reserve(8 + 4 * placement.len()).is_err()
        {
            return Err(not_started(&"out of memory"));
        }
        host_cpus.push_str(if placement.len() == 1 { "cpu " } else { "cpus " });
        for (i, c) in placement.iter().enumerate() {
            let _ = write!(host_cpus, "{}{}", if i == 0 { "" } else { "," }, c);
        }
        kernel.push_str(&spec.kernel);
        let _ = write!(task_name, "hv/vm{}", id);
        let _ = write!(said, "{}, {} MiB{}, {} cpu{}, cmdline \"{}\"{}", spec.kernel, mem_mib,
                       if guest::from_pool() { " of the huge pool" } else { "" }, spec.cpus,
                       if spec.cpus == 1 { "" } else { "s" }, spec.cmdline,
                       if spec.restart { ", restarted when it resets" } else { "" });
        /* Where its memory is on a machine of nodes: the node its placement
           asked for, and how much of it the allocator found there */
        let mem_node = spec.node.map(|node| {
            let (on, all) = built.guest.memory().pages_on(node);
            (node, (on * 100 / all.max(1)) as u8)
        });
        if let Some((node, pct)) = mem_node {
            let _ = write!(said, ", its memory node {}'s ({}%)", node, pct);
        }

        let mut wanted = Vec::new();
        let mut vm_name = None;
        let mut vm_folder = None;
        wanted.try_reserve_exact(spec.forwards.len()).map_err(|_| not_started(&"out of memory"))?;
        wanted.extend_from_slice(&spec.forwards);
        if let Some((folder, _)) = &stored {
            let mut s = String::new();
            let mut p = String::new();
            s.try_reserve_exact(folder.name.len()).map_err(|_| not_started(&"out of memory"))?;
            p.try_reserve_exact(folder.path.len()).map_err(|_| not_started(&"out of memory"))?;
            s.push_str(&folder.name);
            p.push_str(&folder.path);
            vm_name = Some(s);
            vm_folder = Some(p);
        }
        /* For the log, before the guest has printed a byte: first in it. */
        let mut note = String::new();
        if note.try_reserve(host_cpus.len() + said.len() + 64).is_ok() {
            match restored {
                Some(what) => {
                    let _ = write!(note, "restored on {} from {} -- {}", host_cpus, what, said);
                }
                None => {
                    let _ = write!(note, "started on {} -- {}", host_cpus, said);
                }
            }
            shared.note(note);
        }
        /* Its disks' words, for `disk_io`: the spec goes into its task */
        let mut disk_words = Vec::new();
        disk_words.try_reserve_exact(spec.disks.len()).map_err(|_| not_started(&"out of memory"))?;
        for (word, _) in &spec.disks {
            disk_words.push(copied(word).ok_or_else(|| not_started(&"out of memory"))?);
        }
        let start = Start {
            shared: shared.clone(), built, machine: machine.clone(), spec, runner, placement, hold: core_hold,
        };
        /* The start -- the guest in it -- goes with a spawn that fails. */
        let task = kcore::task::spawn_on_with(&task_name, 1u64 << cpu, start, vcpu)
            .ok_or_else(|| not_started(&"no task for it"))?;

        /* Its log from its first byte on: the ring has everything the guest
         * has printed since its task began. */
        let log = match (&stored, logged) {
            (Some((folder, _)), true) => Log::start(&shared, id, &folder.path),
            _ => None,
        };
        let port = hold.as_ref().map(|h| h.port);
        let vm = Vm {
            shared: shared.clone(), task: Some(task), cpus, disk_cpu: has_disks.then_some(disk_cpu), kernel, mem_mib,
            node: mem_node, port, name: vm_name, folder: vm_folder, log, disks: disk_words,
        };
        let mut handed = None;
        let refused = {
            let mut table = self.table.lock();
            if table.closing || table.vms.len() >= MAX_VMS || Self::listed_as(&table, name).is_some() {
                Some(vm)
            } else {
                /* Into the room taken when the table was made (`MAX_VMS`). */
                table.vms.push(vm);
                /* Its name the listed VM's from here: the hold given up in
                 * the same step */
                if let Some(h) = held.as_mut() {
                    handed = h.hand_over(&mut table);
                }
                None
            }
        };
        /* Freed off the lock. */
        drop(handed);
        /* Two starts that both found room, or an unload that began while
         * this one read its files: stop the guest again, off the lock. */
        if let Some(vm) = refused {
            finish(vm);
            return Err(alloc::format!("vm {} stopped again at once -- no room for it, its name taken meanwhile, or \
                                       the module is going", id));
        }
        /* On the list: its port is given back when it comes off -- and one
         * of a cluster network's kept by the keeper till then. */
        if let Some(h) = hold.as_mut() {
            h.kept = true;
            if let (Some((uuid, a)), Some(keeper)) = (attached.as_ref(), self.keeper()) {
                if let Err(why) = keeper.attached(h.port, uuid, a) {
                    let _ = writeln!(out, "hv: vm {}'s port on {}{} not kept -- {}: what vnet changes of it is not \
                                           seen here", id, crate::vnet::PREFIX, a.network, why);
                }
            }
        }
        match name {
            Some(n) => {
                let _ = writeln!(out, "hv: vm {} ({}) started on {} -- {}", id, n, host_cpus, said);
            }
            None => {
                let _ = writeln!(out, "hv: vm {} started on {} -- {}", id, host_cpus, said);
            }
        }
        /* On the list, its guest running: a start that got this far */
        match restored {
            Some(what) => {
                kcore::flog!(crate::HV, "{} restored on {} from {} -- {}", shared.label, host_cpus, what, said);
            }
            None => kcore::flog!(crate::HV, "{} started on {} -- {}", shared.label, host_cpus, said),
        }
        /* nos's ports it asks for, relayed to its own: each said as it is
         * added, or why not -- the guest runs either way. */
        if let (Some(port), Some(host), Some(switch)) = (port, self.host_nic(), self.switch_now()) {
            let outer = self.outer_nic();
            for (host_port, guest_port) in wanted {
                let listed = || self.on_list(id);
                let to = forward::To { guest_ip: switch.ip_of(port), guest_port, host, outer, switch: switch.clone(), port };
                if let Err(e) = forwards.add(host_port, id, to, &listed, out) {
                    let _ = writeln!(out, "hv: {}", e);
                }
            }
        }
        Ok(id)
    }

    /// `hv list`.
    /// Every VM on the list as it is now, copied out of the table under its
    /// lock; None when there was no memory for the copy.
    fn snapshot(&self) -> Option<Vec<VmView>> {
        let table = self.table.lock();
        let mut views = Vec::new();
        views.try_reserve_exact(table.vms.len()).ok()?;
        for vm in &table.vms {
            views.push(VmView {
                shared: vm.shared.clone(),
                cpus: vm.cpus.clone(),
                disk_cpu: vm.disk_cpu,
                kernel: vm.kernel.clone(),
                mem_mib: vm.mem_mib,
                node: vm.node,
                port: vm.port,
                name: vm.name.clone(),
            });
        }
        Some(views)
    }

    pub fn list(&self, out: &mut Output) {
        let now = kcore::time::boot_time_ns();
        let Some(views) = self.snapshot() else {
            let _ = writeln!(out, "hv: out of memory");
            return;
        };
        /* No VM, and the switch said all the same: probes may be on it */
        if views.is_empty() {
            let _ = writeln!(out, "hv: no vms");
        }
        /* The switch out of its lock too: its counters are atomics, its
         * networks under a lock of its own. */
        let switch = self.switch.lock().clone();
        for vm in &views {
            let s = &vm.shared;
            let running = s.running();
            let until = if running { now } else { s.ended_ns.load(Ordering::Relaxed) };
            let state = if running { s.running_state() } else { "stopped" };
            let _ = write!(out, "vm {}", s.id);
            if let Some(name) = &vm.name {
                let _ = write!(out, " {}", name);
            }
            let _ = write!(out, "  {}  cpu{} ", state, if vm.cpus.len() == 1 { "" } else { "s" });
            for (i, c) in vm.cpus.iter().enumerate() {
                let _ = write!(out, "{}{}", if i == 0 { "" } else { "," }, c);
            }
            if let Some((node, pct)) = vm.node {
                let _ = write!(out, "  node {} ({}% of its memory)", node, pct);
            }
            if let Some(cpu) = vm.disk_cpu {
                let _ = write!(out, "  disks on cpu {}", cpu);
            }
            let _ = write!(out, "  {} MiB  {} s", vm.mem_mib,
                until.saturating_sub(s.boot_ns.load(Ordering::Relaxed)) / NS_PER_SEC);
            if let Some(port) = vm.port {
                let ip = switch.as_ref().map_or(net::port_ip(port), |s| s.ip_of(port));
                let _ = write!(out, "  {}", net::dotted(ip));
                let cluster = self.keeper().and_then(|k| k.said(port));
                match (cluster, switch.as_ref().and_then(|s| s.network_of(port))) {
                    (Some((network, confirmed, shut)), _) => {
                        let _ = write!(out, " on {}{}", crate::vnet::PREFIX, network);
                        if let Some(why) = shut {
                            let _ = write!(out, " (shut: {})", why);
                        } else if !confirmed {
                            let _ = write!(out, " (unconfirmed)");
                        }
                    }
                    (None, Some(network)) => {
                        let _ = write!(out, " on {}", network);
                    }
                    (None, None) if switch.as_ref().is_some_and(|s| s.vnet_of(port).is_some()) => {
                        let _ = write!(out, " on a network of the cluster's");
                        if switch.as_ref().is_some_and(|s| s.is_shut(port)) {
                            let _ = write!(out, " (shut)");
                        }
                    }
                    (None, None) => {
                        let _ = write!(out, " alone");
                    }
                }
                if let Some(public) = switch.as_ref().and_then(|s| s.public_of(port)) {
                    let _ = write!(out, ", public {}", netwire::Ipv4(public));
                }
            }
            let lost = vm.port.and_then(|p| switch.as_ref().map(|s| s.counts(p))).unwrap_or_default();
            if lost.dropped != 0 {
                let _ = write!(out, "  {} frames dropped for it", lost.dropped);
            }
            if lost.spoofed != 0 {
                let _ = write!(out, "  {} frames it sent as another guest, dropped", lost.spoofed);
            }
            if lost.over_share != 0 {
                let _ = write!(out, "  {} frames for hv0 past its share of the backlog, dropped", lost.over_share);
            }
            if lost.flooded != 0 {
                let _ = write!(out, "  {} frames for every port past their rate, dropped", lost.flooded);
            }
            if lost.isolated != 0 {
                let _ = write!(out, "  {} frames for another network's guests, dropped", lost.isolated);
            }
            if vm.port.is_some() {
                let _ = write!(out, "  firewall: {} flows", lost.flows);
                if lost.denied_in != 0 || lost.denied_out != 0 {
                    let _ = write!(out, ", {} packets kept out, {} kept in", lost.denied_in, lost.denied_out);
                }
                if lost.evicted != 0 {
                    let _ = write!(out, ", {} flows given up for room", lost.evicted);
                }
            }
            let restarts = s.restarts.load(Ordering::Relaxed);
            if restarts != 0 {
                let _ = write!(out, "  restarts {}", restarts);
            }
            let (exits, irq, hlt) = s.totals();
            let _ = write!(out, "  exits {}  irq {}  hlt {}  kicks {}  {}",
                exits, irq, hlt, s.doorbells.kicks(), vm.kernel);
            if !running {
                let _ = write!(out, "  -- {}", s.reason_copy());
            }
            let _ = writeln!(out);
        }
        if let Some(s) = switch.as_ref() {
            let (to_host, refused, dhcp, foreign) = s.host_counts();
            let _ = writeln!(out, "hv0 {}/24: {} frames from the guests to nos, {} it would not take, {} DHCP answers, \
                                   {} of nothing the switch carries (dropped)",
                             net::dotted(net::HOST_IP), to_host, refused, dhcp, foreign);
            let (advertised, bound) = s.ipv6_counts();
            let g = |i: usize| u16::from_be_bytes([net::PREFIX6[i], net::PREFIX6[i + 1]]);
            let _ = writeln!(out, "IPv6 between the guests, in {:x}:{:x}:{:x}::/64: {} router advertisements, {} \
                                   addresses bound -- none to nos, which speaks none",
                             g(0), g(2), g(4), advertised, bound);
            let (arps, echoes) = s.vnet_counts();
            if arps + echoes != 0 {
                let _ = writeln!(out, "the cluster's networks: {} ARP questions answered by the switch, {} echoes from \
                                       their gateways", arps, echoes);
            }
            let relayed = s.relayed();
            if relayed != 0 {
                let _ = writeln!(out, "the cluster's networks: {} frames for ports moved to other hosts carried on to \
                                       them", relayed);
            }
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
                let _ = writeln!(out, "hv: {}", e);
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
                let _ = writeln!(out, "hv: {}", e);
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
                let _ = writeln!(out, "hv: {}", e);
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
            match shared.fed_at(seq) {
                Fed::At(at) => {
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
                Fed::Lost => break Some("the vm booted again before it read the line, which went with that boot"),
                Fed::Waiting => {}
            }
            if !shared.running() {
                break Some("the vm stopped");
            }
            if shared.restarts.load(Ordering::Acquire) != restarts {
                break Some("the vm restarted, and the line went with the boot it was typed at");
            }
            if kcore::time::boot_time_ns() >= deadline {
                break Some(if matches!(shared.fed_at(seq), Fed::At(_)) {
                    "no prompt came back in time"
                } else if shared.untype(seq, bytes.len()) {
                    "the guest has not read the line -- it is not at a prompt; the line is taken back"
                } else {
                    "the guest has not read all of the line -- it is not at a prompt; what is left of it, or what \
                     was typed after it, stays queued"
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
                let _ = writeln!(out, "hv: {}", e);
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
                                 shared.id, text, shared.reason_copy());
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
                let _ = writeln!(out, "hv: {}", e);
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
        /* The terminal as nos's shell had it: whatever modes the guest
         * left on -- its alternate screen, mouse reports, a hidden cursor,
         * line-drawing characters -- off again */
        out.write_bytes(guest::TERM_RESET);
        let _ = writeln!(out, "\nhv: {} -- vm {}", why, shared.id);
    }

    /// `hv restart <id>`: boot the guest again from its files -- the running
    /// one, as a reset button would, or one that has stopped. The VM's task
    /// does it; this waits until the new boot is built, or has failed to be,
    /// so that what comes next in a script finds it booting.
    pub fn restart(&self, args: &str, out: &mut dyn Write) {
        let (word, _) = after_word(args);
        match self.restart_vm(word, &|| false) {
            Ok(id) => {
                let _ = writeln!(out, "hv: vm {} restarted", id);
            }
            Err(e) => {
                let _ = writeln!(out, "hv: {}", e);
            }
        }
    }

    /// The VM `word` names booted again (`restart`): its number once the new
    /// boot is built, or why not -- and no longer waited for once `give_up`
    /// says so: a job cancelled, the module going.
    pub fn restart_vm(&self, word: &str, give_up: &dyn Fn() -> bool) -> Result<u32, String> {
        let shared = self.find(word)?;
        if shared.stopping.load(Ordering::Acquire) {
            return Err(alloc::format!("vm {} is being stopped -- not restarted", shared.id));
        }
        let before = shared.restarts.load(Ordering::Acquire);
        shared.reset.store(true, Ordering::Release);
        shared.ring_all();

        let deadline = kcore::time::boot_time_ns().saturating_add(RESTART_WAIT_S * NS_PER_SEC);
        loop {
            kcore::task::sleep_ms(POLL_MS);
            if shared.restarts.load(Ordering::Acquire) != before {
                return Ok(shared.id);
            }
            /* Stopped meanwhile -- by a command, or the unload: its task
             * leaves on the stop flag without taking the reset up, and there
             * is nothing to wait for. */
            if shared.stop.load(Ordering::Acquire) {
                return Err(alloc::format!("vm {} was stopped -- not restarted", shared.id));
            }
            /* Taken up, and not running: its build failed, or it was
             * stopped meanwhile -- the reason says which. */
            if !shared.reset.load(Ordering::Acquire) && !shared.running() {
                return Err(alloc::format!("vm {} not restarted -- {}", shared.id, shared.reason_copy()));
            }
            if kcore::time::boot_time_ns() >= deadline {
                return Err(alloc::format!("vm {} is still not booted again after {} s", shared.id, RESTART_WAIT_S));
            }
            if give_up() {
                return Err(alloc::format!("vm {} is being booted again -- not waited for", shared.id));
            }
        }
    }

    /// `hv stop <id|all> [secs=N]`: press its power button and give it
    /// `secs` to turn itself off -- on the list meanwhile, `stopping`, its
    /// CPUs its own -- then take it off the list, stop it where it is, if it
    /// is still running, wait for its vCPUs, and say how it ended. One that
    /// does not listen to the button is stopped at once, and `secs=0` stops
    /// it without asking.
    pub fn stop(&self, args: &str, out: &mut dyn Write, forwards: &Forwards) {
        let (word, rest) = after_word(args);
        let secs = match stop_secs(rest) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(out, "{}", e);
                return;
            }
        };
        if word == "all" {
            self.stop_all(Some(out), secs, Some(forwards));
            return;
        }
        if word.is_empty() {
            let _ = writeln!(out, "hv stop <id|name|all> [secs=N]");
            return;
        }
        match self.stop_vm(word, secs, forwards, &|| false, &|| false) {
            Ok(s) => {
                let _ = writeln!(out, "hv: vm {} stopped -- {}{}", s.id, s.reason, s.asked);
                if s.forwarded != 0 {
                    let _ = writeln!(out, "hv: its {} forward(s) taken away with it", s.forwarded);
                }
                let _ = out.write_str(&s.report);
            }
            Err(e) => {
                let _ = writeln!(out, "hv: {}", e);
            }
        }
    }

    /// The VM `word` names stopped (`stop`), given `secs` to turn itself off
    /// once its power button is pressed -- 0 to stop it where it is, and
    /// stopped there too once `give_up` says so: how it ended, or why it was
    /// not stopped here. Once `left` says so -- a person's cancel of the
    /// stop, not the module's going -- the wait ends and the guest is left
    /// as it is, turning itself off or not, for another stop to end.
    pub fn stop_vm(&self, word: &str, secs: u64, forwards: &Forwards, give_up: &dyn Fn() -> bool,
                   left: &dyn Fn() -> bool) -> Result<Stopped, String> {
        let id = self.id_of(word)?;
        /* On the list while its power button has its time: its CPUs are
         * still in use, and an unload has to see it. */
        let shared = self.table.lock().vms.iter().find(|vm| vm.shared.id == id).map(|vm| vm.shared.clone());
        let shared = shared.ok_or_else(|| alloc::format!("no vm {}", id))?;
        if shared.stopping.swap(true, Ordering::AcqRel) {
            return Err(alloc::format!("vm {} is being stopped already", id));
        }
        if secs != 0 && shared.press() {
            /* An unload meanwhile gives it its own time, and whichever of
             * the two runs out first takes it off the list: this wait ends
             * there too, the guest no longer running. */
            wait_shut_down(secs, || {
                let waiting = shared.shutting_down() && !give_up() && !left();
                if waiting {
                    shared.press_again();
                }
                waiting
            });
            if left() && !give_up() && shared.running() {
                shared.stopping.store(false, Ordering::Release);
                return Err(crate::work::stopped_leaving(&alloc::format!("vm {} is left running, its power button \
                                                                         pressed", id)));
            }
        }
        let asked = shared.asked();
        let vm = {
            let mut table = self.table.lock();
            table.vms.iter().position(|vm| vm.shared.id == id).map(|i| table.vms.remove(i))
        };
        /* `hv stop all`, or the unload, took it meanwhile, and says how it
         * ended. */
        let vm = vm.ok_or_else(|| alloc::format!("vm {} was stopped meanwhile", id))?;
        let port = vm.port;
        finish(vm);
        /* Its forwards before its switch port: once the port is given back
         * the next VM started may take its address, which they relay to. */
        let forwarded = forwards.drop_vm(id);
        self.release(port);
        let reason = shared.reason_copy();
        let report = shared.report.lock().clone();
        Ok(Stopped { id, reason, asked, forwarded, report })
    }

    /// Stop every VM and take it off the list, saying how each ended -- to
    /// `out`, or, with no one to say it to, to the kernel log: every power
    /// button pressed at once and the guests given `secs` together to turn
    /// themselves off, as `stop` gives one, each pressed again meanwhile as
    /// it gives one. A VM an `hv stop` of its own is giving its time already
    /// is not pressed now, only waited for -- and pressed again as every
    /// other is, once between the two waiters. Each one's forwards go with
    /// it, when there are `forwards` still to take them from.
    pub fn stop_all(&self, mut out: Option<&mut dyn Write>, secs: u64, forwards: Option<&Forwards>) {
        let mut pressed = 0usize;
        for vm in &self.table.lock().vms {
            if !vm.shared.stopping.swap(true, Ordering::AcqRel) && secs != 0 && vm.shared.press() {
                pressed += 1;
            }
        }
        if secs != 0 {
            /* Said where it can be seen: on the way down, a pause of up to
             * `secs` with nothing in the log would look like a wedge. */
            if pressed != 0 && out.is_none() {
                kcore::klog!(0, "hv: unloading -- {} vm(s) asked to turn themselves off, given {} s", pressed, secs);
            }
            wait_shut_down(secs, || {
                let mut waiting = false;
                for vm in self.table.lock().vms.iter().filter(|vm| vm.shared.shutting_down()) {
                    vm.shared.press_again();
                    waiting = true;
                }
                waiting
            });
        }
        /* Then every stop flag, so that those left stop together rather than
         * one join at a time -- and what became of each press noted first: a
         * guest stopped says nothing of it. */
        let mut asked = [(u32::MAX, Asked::Nothing); MAX_VMS];
        for (note, vm) in asked.iter_mut().zip(self.table.lock().vms.iter()) {
            *note = (vm.shared.id, vm.shared.asked());
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
            /* Its forwards before its switch port, as `stop_vm` takes them */
            if let Some(f) = forwards {
                f.drop_vm(shared.id);
            }
            self.release(port);
            stopped += 1;
            let reason = shared.reason.lock();
            match out.as_deref_mut() {
                Some(out) => {
                    let _ = writeln!(out, "hv: vm {} stopped -- {}{}", shared.id, *reason, asked);
                }
                None => kcore::klog!(0, "hv: vm {} stopped for the unload -- {}{}", shared.id, *reason, asked),
            }
        }
        if stopped == 0 {
            if let Some(out) = out {
                let _ = writeln!(out, "hv: no vms");
            }
        }
    }

    /// The module is going: no VM starts from here on, and every one there is
    /// stops as `hv stop all secs=<secs>` stops them -- asked first, its power
    /// button pressed and the guests given `secs` together -- and then the
    /// switch goes, `hv0`'s sink detached. This is how nos's own poweroff and
    /// reboot end the guests too (they unload every module), and asking is
    /// the difference between a guest that unmounted its disks and one whose
    /// writes still in its page cache were lost: `rmmod` goes on in the
    /// background after five seconds, the kernel log saying how each ended,
    /// and the halt waits. The forwards went before this, every one of them.
    pub fn close(&self, secs: u64) {
        self.table.lock().closing = true;
        self.stop_all(None, secs, None);
        /* The tunnel's task stopped and waited out -- it tells the other
         * hosts this one goes -- the switch carrying into it no more, then
         * the keeper's: both are this module's code, and held the switch.
         * Then the switch -- and the tunnel last of all: `hv0`'s sink, which
         * the switch's going waits out, may hold a carrier of it it took a
         * moment before, whose drop must not be the last, there. */
        let tunnel = match self.tunnel.lock().take() {
            Some((tunnel, task)) => {
                task.request_stop();
                drop(task);
                Some(tunnel)
            }
            None => None,
        };
        let switch_now = self.switch.lock().clone();
        if let Some(s) = switch_now {
            drop(s.carrier(None));
        }
        let kept = self.keeper.lock().take();
        if let Some((keeper, task)) = kept {
            task.request_stop();
            keeper.quiet();
            drop(task);
        }
        let switch = self.switch.lock().take();
        drop(switch);
        drop(tunnel);
    }

    /// Every VM on the list as the host API reads it, copied out under the
    /// table's lock and read live after (`snapshot`); None out of memory.
    pub fn listed(&self) -> Option<Vec<Listed>> {
        let now = kcore::time::boot_time_ns();
        let views = self.snapshot()?;
        let switch = self.switch_now();
        let mut all = Vec::new();
        all.try_reserve_exact(views.len()).ok()?;
        for v in views {
            let s = &v.shared;
            let running = s.running();
            let until = if running { now } else { s.ended_ns.load(Ordering::Relaxed) };
            let state = if running { s.running_state() } else { "stopped" };
            let (exits, _, _) = s.totals();
            all.push(Listed {
                id: s.id,
                name: v.name,
                state,
                cpus: v.cpus,
                mem_mib: v.mem_mib,
                node: v.node,
                port: v.port,
                ip: v.port.zip(switch.as_ref()).map(|(p, s)| s.ip_of(p)),
                public: v.port.zip(switch.as_ref()).and_then(|(p, s)| s.public_of(p)),
                uptime_ms: until.saturating_sub(s.boot_ns.load(Ordering::Relaxed)) / NS_PER_MS,
                restarts: s.restarts.load(Ordering::Relaxed),
                reason: if running { String::new() } else { s.reason_copy() },
                exits,
                kernel: v.kernel,
                console_bytes: s.console_total(),
            });
        }
        Some(all)
    }

    /// The console of the VM `word` names from byte `from` on, at most `max`
    /// bytes of it: its control sequences taken out (`guest::printable`) --
    /// or, for a `terminal`, filtered as `hv attach` filters them
    /// (`TermFilter`), a sequence the page would cut in two left for the
    /// next. The text, where what it was made of begins -- after `from` when
    /// the ring no longer has what came before -- and ends, the next page's
    /// `from`; and where the console is now.
    pub fn console_page(&self, word: &str, from: u64, max: usize, terminal: bool) -> Result<ConsolePage, String> {
        let shared = self.find(word)?;
        let mut raw = Vec::new();
        let (start, total) = {
            let ring = shared.console.lock();
            let start = ring.page(from, max, &mut raw).ok_or_else(|| String::from("out of memory"))?;
            (start, ring.total())
        };
        let mut text = Vec::new();
        let taken = if terminal {
            text.try_reserve(raw.len() + 2 * TERM_SLACK).map_err(|_| String::from("out of memory"))?;
            TermFilter::new().filter_whole(&raw, &mut text)
        } else if guest::printable(&raw, &mut text) {
            raw.len()
        } else {
            return Err(String::from("out of memory"));
        };
        Ok(ConsolePage { text, start, end: start + taken as u64, total })
    }

    /// `bytes` typed at the VM `word` names, as `hv send` types them: now,
    /// as at a terminal. How many were queued.
    pub fn type_at(&self, word: &str, bytes: &[u8]) -> Result<usize, String> {
        let shared = self.find(word)?;
        if !shared.running() {
            return Err(alloc::format!("vm {} is not running", shared.id));
        }
        shared.typed.store(true, Ordering::Release);
        shared.type_in(bytes).map(|_| bytes.len())
            .ok_or_else(|| alloc::format!("vm {}'s input is full -- nothing typed", shared.id))
    }

    /// The guests' switch, if one is up.
    pub fn switch_now(&self) -> Option<Arc<Switch>> {
        self.switch.lock().clone()
    }

    /// The guests' switch, made the first time it is asked for: what a
    /// probe's port is claimed of too (`crate::probe`).
    pub(crate) fn switch_made(&self) -> Result<Arc<Switch>, String> {
        self.switch()
    }

    /// The guests' switch, made the first time it is asked for -- and with
    /// it the keeper of what hv has of vnet, and its task (`vnet::Keeper`).
    fn switch(&self) -> Result<Arc<Switch>, String> {
        let mut switch = self.switch.lock();
        if let Some(s) = switch.as_ref() {
            return Ok(s.clone());
        }
        let made = Arc::new(Switch::new()?);
        /* The tunnel first: the switch carries into it, and the keeper gives
         * it the table */
        let tunnel = crate::tunnel::Tunnel::new(made.clone(), &crate::aead::Cryptogams, self.rekey_s, self.keylife_s)
            .ok_or_else(|| String::from("out of memory for the switch's tunnel"))?;
        let Some(tunnel_task) = kcore::task::spawn_with("hv/tunnel", tunnel.clone(), crate::tunnel::Tunnel::run) else {
            return Err(String::from("no task for the switch's tunnel"));
        };
        let carrier: Arc<dyn crate::net::Carrier> = tunnel.clone();
        drop(made.carrier(Some(carrier)));
        let keeper = crate::vnet::Keeper::new(made.clone(), self.firewalls.clone(), tunnel.clone());
        let task = keeper.as_ref().and_then(|k| kcore::task::spawn_with("hv/vnet", k.clone(), crate::vnet::Keeper::run));
        let (Some(keeper), Some(task)) = (keeper, task) else {
            /* Nothing kept: the tunnel's task stopped, the switch let go */
            tunnel_task.request_stop();
            drop(tunnel_task);
            drop(made.carrier(None));
            return Err(String::from("no task for the switch's keeper"));
        };
        *self.tunnel.lock() = Some((tunnel, tunnel_task));
        *self.keeper.lock() = Some((keeper, task));
        *switch = Some(made.clone());
        Ok(made)
    }

    /// The cluster networks' tunnel, while there is a switch.
    pub fn tunnel(&self) -> Option<Arc<crate::tunnel::Tunnel>> {
        self.tunnel.lock().as_ref().map(|(t, _)| t.clone())
    }

    /// What hv keeps of vnet, while there is a switch.
    pub fn keeper(&self) -> Option<Arc<crate::vnet::Keeper>> {
        self.keeper.lock().as_ref().map(|(k, _)| k.clone())
    }

    /// A VM's port given back, once its vCPU task is gone.
    fn release(&self, port: Option<usize>) {
        let Some(port) = port else { return };
        if let Some(k) = self.keeper() {
            k.released(port);
        }
        let switch = self.switch.lock().clone();
        if let Some(s) = switch {
            s.release(port);
        }
    }

    /// Port `port`'s firewall's flows, each with how long it has left (a
    /// VM's, paused for its move): none where there is no switch.
    pub fn port_flows(&self, port: usize) -> Vec<net::Carried> {
        let mut flows = Vec::new();
        let switch = self.switch.lock().clone();
        if let Some(s) = switch {
            if flows.try_reserve_exact(net::FLOWS).is_ok() {
                s.flows_of(port, &mut flows);
            }
        }
        flows
    }

    /// Port `port`'s cluster network and address, while it is on one.
    pub fn port_vnet(&self, port: usize) -> Option<(u32, u32)> {
        let switch = self.switch.lock().clone();
        switch.and_then(|s| s.vnet_address(port))
    }

    /// The port of network `vni` at `addr` moved to host `to`, its frames
    /// still sent here carried on there a while (`Switch::moved_away`):
    /// whether the host was known.
    pub fn moved_away(&self, vni: u32, addr: u32, to: &str) -> bool {
        let switch = self.switch.lock().clone();
        let node = self.tunnel().and_then(|t| t.node_of(to));
        if let (Some(s), Some(node)) = (switch, node) {
            s.moved_away(vni, addr, node);
        }
        node.is_some()
    }

    /// The node of host `name`, as the tunnel has the hosts.
    pub fn node_of(&self, name: &str) -> Option<u64> {
        self.tunnel().and_then(|t| t.node_of(name))
    }

    /// The keeper's table asked for now, where there is one: a port moved
    /// here or away.
    pub fn keeper_now(&self) {
        if let Some(k) = self.keeper() {
            k.turn_now();
        }
    }

    /// The guests' switch, if there is one: for `hv forward`.
    pub fn host_nic(&self) -> Option<kcore::net::Nic> {
        self.switch.lock().as_ref().and_then(|s| s.host_nic())
    }

    /// The device the guests go out through, while NAT is on: what `hv
    /// forward` listens on (`Switch::outer_nic`).
    pub fn outer_nic(&self) -> Option<kcore::net::Nic> {
        self.switch.lock().as_ref().and_then(|s| s.outer_nic())
    }

    /// The ports of the switch the VMs on the list hold, and those given to
    /// VMs being made or cloned whose vm.tomls do not say so yet
    /// (`Reserved::keep_port`). None out of memory: no port is known free.
    pub fn ports_held(&self) -> Option<Vec<usize>> {
        let mut ports = Vec::new();
        ports.try_reserve_exact(MAX_VMS + MAX_RESERVED).ok()?;
        let table = self.table.lock();
        /* Into the room taken above: the table holds MAX_VMS at most, and
         * MAX_RESERVED holds. */
        ports.extend(table.vms.iter().filter_map(|vm| vm.port));
        ports.extend(table.reserved.iter().filter_map(|r| r.port).map(|p| p as usize));
        Some(ports)
    }

    /// The name of VM `id`, when it was started from its folder.
    pub fn name_of(&self, id: u32) -> Option<String> {
        let table = self.table.lock();
        let name = table.vms.iter().find(|vm| vm.shared.id == id)?.name.as_deref()?;
        let mut copy = String::new();
        copy.try_reserve_exact(name.len()).ok()?;
        copy.push_str(name);
        Some(copy)
    }

    /// Whether VM `id` is on the list: what `Forwards::add` asks, under its
    /// own lock, as a forward to it goes in.
    pub fn on_list(&self, id: u32) -> bool {
        self.table.lock().vms.iter().any(|vm| vm.shared.id == id)
    }

    /// The port of the switch the running VM `id` has, when it has a NIC.
    pub fn port_of(&self, id: u32) -> Option<usize> {
        let table = self.table.lock();
        table.vms.iter().find(|vm| vm.shared.id == id && vm.shared.running()).and_then(|vm| vm.port)
    }

    /// The VM on the list on port `port` of the switch, as a listing names
    /// it: its folder's name, or `vm N` where it has none.
    pub fn named_on_port(&self, port: usize) -> Option<String> {
        let table = self.table.lock();
        let vm = table.vms.iter().find(|vm| vm.port == Some(port))?;
        let mut said = String::new();
        said.try_reserve(vm.name.as_ref().map_or(0, |n| n.len()) + 16).ok()?;
        match &vm.name {
            Some(n) => said.push_str(n),
            None => {
                let _ = write!(said, "vm {}", vm.shared.id);
            }
        }
        Some(said)
    }

    /// The VM on the list on port `port` of the switch, as a person knows
    /// it: its name and number, or its number.
    pub fn on_port(&self, port: usize) -> Option<String> {
        let table = self.table.lock();
        let vm = table.vms.iter().find(|vm| vm.port == Some(port))?;
        let mut said = String::new();
        said.try_reserve(vm.name.as_ref().map_or(0, |n| n.len()) + 16).ok()?;
        match &vm.name {
            Some(n) => {
                let _ = write!(said, "{} (vm {})", n, vm.shared.id);
            }
            None => {
                let _ = write!(said, "vm {}", vm.shared.id);
            }
        }
        Some(said)
    }
}
