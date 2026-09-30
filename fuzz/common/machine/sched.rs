//! The machine's CPUs, as the fuzzer has them: every task on a thread of its
//! own, and one of them running at a time -- the one with the turn -- so
//! that a run is the same every time, and the order tasks run in is decided
//! here and by the input, where a task sleeps, waits, yields or lets a
//! spin lock go.
//!
//! The kernel's scheduling is what is kept: a task sleeps until a deadline,
//! waits on an event or a mutex, waits for another task to end, yields; a
//! soft IRQ runs in task context, one type at a time; a timer fires from
//! interrupt context. When nobody can run, the clock moves on to the first
//! deadline -- nothing here sleeps on a real clock. The world -- the
//! fuzzer's own thread, which runs the target's script and whatever is
//! around the machine -- is a task too, and the one soft IRQs run on.
//!
//! And the kernel's rules are checked where they are broken, each a finding:
//! a task that sleeps, waits or yields with a spin lock held or interrupts
//! off; a soft IRQ handler or a timer that sleeps (the receive path, above
//! all: nothing on it may resolve through ARP); a spin lock or a mutex taken
//! again by its holder; two locks taken in both orders; a mutex let go of by
//! a task that does not hold it; a task that returns with a lock held; two
//! tasks waiting on one event; a task waiting for itself; every task waiting
//! for good.

use std::cell::Cell;
use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// The world: the fuzzer's thread.
pub const WORLD: usize = 1;

/// Where each input's clock starts: a machine up for a while.
pub const START: u64 = 1_000_000_000_000;
/// The latest it gets: a machine up for 146 years.
pub const END: u64 = 1 << 62;
/// What a read of the clock costs, in time passed: a loop that waits for
/// the clock without sleeping ends, as it would on a machine.
const READ_NS: u64 = 100;
/// The scheduler's tick: every CPU has a scheduling point this often.
const TICK: u64 = 10_000_000;
/// That many reads with no sleep, wait or yield between is a loop that
/// neither gets anywhere nor lets anything else run.
const SPIN_READS: u64 = 1_000_000;
/// A task thread's stack: four times a kernel task's (`Task::StackSize`),
/// for what the host's code generation and the overflow checks add. A task
/// that runs past it is a crash, and a finding.
const STACK: usize = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    Sleep,
    Event(usize),
    Mutex(usize),
    Join(usize),
    /// The world waiting for something to happen: a frame out, a soft IRQ
    /// raised, a task done, or its deadline.
    World,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum St {
    Ready,
    Running,
    Blocked(Wait),
    Done,
}

/// Why a blocked task was let go on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Woke {
    Signalled,
    TimedOut,
    Granted,
    Joined,
    Kicked,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    World,
    /// Spawned by the kernel: a service's task, a module's.
    Kernel,
    /// The fuzzer's stand-in for a program: a caller of a blocking API.
    App,
}

pub struct Task {
    pub name: String,
    pub kind: Kind,
    pub st: St,
    /// Blocked until then at the latest; `u64::MAX` for no deadline.
    pub until: u64,
    pub woke: Woke,
    thread: Option<std::thread::Thread>,
    pub stopping: bool,
    pub cpu: u32,
    /// The spawn's handle, until `kernel_task_put`.
    pub refs: u32,
    /// The kernel's locks it holds -- mutexes and spin locks -- in the
    /// order it took them.
    pub holds: Vec<usize>,
}

struct Event {
    signalled: bool,
    waiter: Option<usize>,
    alive: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LockKind {
    Mutex,
    Spin,
}

struct Lock {
    kind: LockKind,
    /// The task holding it, 0 for none.
    owner: usize,
    waiters: VecDeque<usize>,
    alive: bool,
}

struct Timer {
    handler: extern "C" fn(*mut u8),
    ctx: usize,
    period: u64,
    next: u64,
    alive: bool,
}

pub struct Sched {
    tasks: Vec<Task>,
    current: usize,
    events: Vec<Event>,
    locks: Vec<Lock>,
    /// Lock order seen: (a, b) when b was taken with a held.
    order: BTreeSet<(usize, usize)>,
    timers: Vec<Timer>,
    softirqs: [Option<(extern "C" fn(*mut u8), usize)>; SOFTIRQS],
    pending: u32,
    /// Decides where a task that could go on is made to let another run
    /// first: 0 never, else one time in `preempt_one_in`.
    preempt_one_in: u32,
    chaos: u64,
    pub handoffs: u64,
}

const SOFTIRQS: usize = 8;

static SCHED: Mutex<Option<Sched>> = Mutex::new(None);
static NOW: AtomicU64 = AtomicU64::new(START);
/// Whose turn it is, where a waiting thread can see it without the lock:
/// a thread handed the turn while it is still spinning takes it without a
/// trip through the OS's semaphores, which on macOS cost several
/// microseconds each way and were most of what a run of the tasks spent.
static CURRENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(WORLD);
/// How long a thread that has handed the turn on spins for it to come back
/// before it parks: on a machine with the cores for it, which the parent
/// asks once (`boot`) -- a spinner with no core of its own to spin on
/// keeps the thread with the turn off its core instead.
static SPIN_US: AtomicU64 = AtomicU64::new(0);
/// `Sched::preempt_one_in`, where a lock let go of can see it without the
/// scheduler's lock: most inputs never preempt, and every spin lock's
/// unlock asks.
static PREEMPT_ONE_IN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// What the thread is doing: a task's work, a soft IRQ's handler, or an
/// interrupt's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ctx {
    Task,
    Softirq,
    Irq,
}

thread_local! {
    static ME: Cell<usize> = const { Cell::new(0) };
    static CTX: Cell<Ctx> = const { Cell::new(Ctx::Task) };
    /// Interrupts off, and preemption off: how deep. Only the thread's own
    /// code moves them, so they need no lock -- and the allocator, which
    /// must not take one, can read them.
    static IRQ_OFF: Cell<u32> = const { Cell::new(0) };
    static PREEMPT_OFF: Cell<u32> = const { Cell::new(0) };
    /// The fuzzer's own code is running -- the kernel's C++ half, the NIC
    /// -- and what it allocates is not the kernel's.
    static EXEMPT: Cell<u32> = const { Cell::new(0) };
    static READS: Cell<u64> = const { Cell::new(0) };
}

fn lock() -> MutexGuard<'static, Option<Sched>> {
    /* A panic under the lock is a finding already, reported by the hook
     * and the process gone: nothing reads a poisoned one. */
    SCHED.lock().unwrap_or_else(|e| e.into_inner())
}

macro_rules! sched {
    ($g:expr) => {
        $g.as_mut().expect("the machine is booted")
    };
}

/* ---- the thread's own state ---- */

pub fn me() -> usize {
    ME.with(|m| m.get())
}

pub fn ctx() -> Ctx {
    CTX.with(|c| c.get())
}

pub fn irq_off() -> u32 {
    IRQ_OFF.try_with(|c| c.get()).unwrap_or(0)
}

pub fn preempt_off() -> u32 {
    PREEMPT_OFF.try_with(|c| c.get()).unwrap_or(0)
}

/// Whether what this thread allocates now is the fuzzer's.
pub fn exempt() -> bool {
    EXEMPT.try_with(|c| c.get() != 0).unwrap_or(true)
}

/// `f`, as the fuzzer's own code: the C++ half of the kernel, the NIC.
pub fn harness<R>(f: impl FnOnce() -> R) -> R {
    EXEMPT.with(|c| c.set(c.get() + 1));
    let r = f();
    EXEMPT.with(|c| c.set(c.get() - 1));
    r
}

/// `f`, as the kernel's code, called back from the fuzzer's.
pub fn kernel<R>(f: impl FnOnce() -> R) -> R {
    let saved = EXEMPT.with(|c| c.replace(0));
    let r = f();
    EXEMPT.with(|c| c.set(saved));
    r
}

fn in_ctx<R>(c: Ctx, f: impl FnOnce() -> R) -> R {
    let saved = CTX.with(|x| x.replace(c));
    let r = f();
    CTX.with(|x| x.set(saved));
    r
}

/* ---- the clock ---- */

/// The clock, without the read costing anything: the fuzzer's own look.
pub fn now() -> u64 {
    NOW.load(Ordering::Relaxed)
}

/// The clock as the kernel reads it: each read a little later than the
/// last, and a million with nothing let run between them a spin.
pub fn read_clock() -> u64 {
    let reads = READS.with(|r| {
        r.set(r.get() + 1);
        r.get()
    });
    if reads == SPIN_READS {
        panic!("a spin: task {} read the clock {} times without sleeping, waiting or yielding", me(), reads);
    }
    let now = NOW.load(Ordering::Relaxed);
    NOW.store((now + READ_NS).min(END), Ordering::Relaxed);
    now
}

fn reach(t: u64) {
    let t = t.min(END);
    if t > NOW.load(Ordering::Relaxed) {
        NOW.store(t, Ordering::Relaxed);
    }
}

/// Where the wall clock is when an input starts: a date the net fuzzer's
/// certificates are valid at, unless a fuzzer says otherwise.
static WALL_BASE: AtomicU64 = AtomicU64::new(1_830_000_000);

/// The wall clock at the start of every input from now on: set once, at
/// boot.
pub fn set_wall_base(secs: u64) {
    WALL_BASE.store(secs, Ordering::Relaxed);
}

/// Wall-clock seconds.
pub fn wall_secs() -> u64 {
    WALL_BASE.load(Ordering::Relaxed) + (now() - START.min(now())) / 1_000_000_000
}

/* ---- booting, and each input ---- */

/// The machine, before the first input: the world its only task. In the
/// parent, which forks each input's process -- and which must never make a
/// thread or park one, or the children cannot (macOS's libdispatch is not
/// fork-safe once it has been used).
pub fn boot() {
    let world = Task {
        name: "world".into(),
        kind: Kind::World,
        st: St::Running,
        until: u64::MAX,
        woke: Woke::Kicked,
        thread: None,
        stopping: false,
        cpu: 0,
        refs: 1,
        holds: Vec::new(),
    };
    let placeholder = Task { name: "none".into(), kind: Kind::World, st: St::Done, until: u64::MAX, woke: Woke::Kicked,
                             thread: None, stopping: false, cpu: 0, refs: 0, holds: Vec::new() };
    *lock() = Some(Sched {
        tasks: vec![placeholder, world],
        current: WORLD,
        events: Vec::new(),
        locks: Vec::new(),
        order: BTreeSet::new(),
        timers: Vec::new(),
        softirqs: [None; SOFTIRQS],
        pending: 0,
        preempt_one_in: 0,
        chaos: 0,
        handoffs: 0,
    });
    ME.with(|m| m.set(WORLD));
    CURRENT.store(WORLD, Ordering::Release);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    SPIN_US.store(if cores >= 4 { 30 } else { 0 }, Ordering::Relaxed);
}

/// The world's thread, as the input's process has it: set at the start of
/// each input, since the parent never looked at its own.
pub fn begin_input(preempt_one_in: u32, chaos: u64) {
    let mut g = lock();
    let s = sched!(g);
    s.tasks[WORLD].thread = Some(std::thread::current());
    s.preempt_one_in = preempt_one_in;
    PREEMPT_ONE_IN.store(preempt_one_in, Ordering::Relaxed);
    s.chaos = chaos | 1;
}

/* ---- the turn ---- */

impl Sched {
    fn chaos_next(&mut self) -> u64 {
        /* xorshift: which task goes first, whether one is preempted */
        let mut x = self.chaos;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.chaos = x;
        x
    }

    /// Who runs after `me` (Ready, Blocked or Done now): the world when a
    /// soft IRQ waits for it, else the next that can run round the ring
    /// from `me` -- now and then another of them, the input's chaos choosing.
    fn pick(&mut self, me: usize) -> Option<usize> {
        if self.pending != 0 && self.tasks[WORLD].st == St::Ready {
            return Some(WORLD);
        }
        let n = self.tasks.len();
        let ready: Vec<usize> = (1..=n).map(|k| (me + k) % n).filter(|&i| i != 0 && self.tasks[i].st == St::Ready)
            .collect();
        if ready.is_empty() {
            return None;
        }
        if ready.len() > 1 && self.preempt_one_in != 0 && self.chaos_next() % 4 == 0 {
            let i = (self.chaos_next() % ready.len() as u64) as usize;
            return Some(ready[i]);
        }
        Some(ready[0])
    }

    /// The first deadline: a blocked task's, or a timer's. A task's is
    /// the kernel's: its sleep, or its wait's timeout, ends at the first
    /// scheduling point after it -- the tick, a hundred times a second, if
    /// nothing else happens sooner (docs/scheduler.md) -- so a task that
    /// polls every millisecond on an idle machine polls every tick. The
    /// world's own waits are the fuzzer's, and end when they say.
    fn next_deadline(&self) -> u64 {
        let tasks = self.tasks.iter().enumerate().filter(|(_, t)| matches!(t.st, St::Blocked(_)))
            .map(|(i, t)| if i == WORLD || t.until == u64::MAX { t.until } else { t.until.div_ceil(TICK) * TICK });
        let timers = self.timers.iter().filter(|t| t.alive).map(|t| t.next);
        tasks.chain(timers).min().unwrap_or(u64::MAX)
    }

    /// Every task whose deadline has come, on; the timers due, for the
    /// caller to fire with the lock down.
    fn due(&mut self, now: u64) -> Vec<(extern "C" fn(*mut u8), usize)> {
        for i in 1..self.tasks.len() {
            let t = &mut self.tasks[i];
            if let St::Blocked(wait) = t.st {
                if t.until <= now {
                    t.st = St::Ready;
                    t.woke = Woke::TimedOut;
                    if let Wait::Event(e) = wait {
                        if let Some(ev) = self.events.get_mut(e) {
                            if ev.waiter == Some(i) {
                                ev.waiter = None;
                            }
                        }
                    }
                }
            }
        }
        let mut fire = Vec::new();
        for t in self.timers.iter_mut().filter(|t| t.alive) {
            if t.next <= now {
                fire.push((t.handler, t.ctx));
                /* One tick however late: a timer that fell behind does not
                 * fire the ticks it missed all at once. */
                t.next = now.saturating_add(t.period);
            }
        }
        fire
    }

    fn describe(&self) -> String {
        let mut s = String::new();
        for (i, t) in self.tasks.iter().enumerate().skip(1) {
            if t.st != St::Done {
                s.push_str(&format!("\n  task {} '{}' {:?} until {}", i, t.name, t.st,
                                    if t.until == u64::MAX { "never".to_string() } else { t.until.to_string() }));
            }
        }
        s
    }
}

/// Hands the turn on from `me`, whose state the caller has set, and returns
/// when `me` has it again -- at once, when `me` is the one to run. The
/// clock moves on to the next deadline whenever nobody can run, and the
/// timers due then fire, on this thread, as an interrupt would.
fn switch(mut g: MutexGuard<'static, Option<Sched>>, me: usize) {
    loop {
        let s = sched!(g);
        if let Some(next) = s.pick(me) {
            s.tasks[next].st = St::Running;
            s.current = next;
            CURRENT.store(next, Ordering::Release);
            if next == me {
                return;
            }
            s.handoffs += 1;
            let thread = s.tasks[next].thread.clone().expect("a task that can run has a thread");
            drop(g);
            thread.unpark();
            wait_turn(me);
            return;
        }
        let due = s.next_deadline();
        if due == u64::MAX {
            panic!("invariant: every task waits for good:{}", s.describe());
        }
        reach(due);
        let fire = s.due(now());
        drop(g);
        for (handler, ctx) in fire {
            interrupt(|| handler(ctx as *mut u8));
        }
        g = lock();
    }
}

/// Waits for the turn to be `me`'s: spinning for a while, as it mostly
/// comes back soon, then parked.
fn wait_turn(me: usize) {
    let began = std::time::Instant::now();
    loop {
        if CURRENT.load(Ordering::Acquire) == me {
            let g = lock();
            if let Some(s) = g.as_ref() {
                if s.current == me && s.tasks[me].st == St::Running {
                    return;
                }
            }
        }
        if (began.elapsed().as_micros() as u64) < SPIN_US.load(Ordering::Relaxed) {
            std::hint::spin_loop();
            continue;
        }
        /* An unpark before the park is kept: no turn is missed. */
        std::thread::park();
    }
}

/// Code run as an interrupt handler: interrupts off, no sleeping, and what
/// it allocates is the kernel's with interrupts off.
pub fn interrupt<R>(f: impl FnOnce() -> R) -> R {
    in_ctx(Ctx::Irq, || {
        IRQ_OFF.with(|c| c.set(c.get() + 1));
        PREEMPT_OFF.with(|c| c.set(c.get() + 1));
        let r = kernel(f);
        IRQ_OFF.with(|c| c.set(c.get() - 1));
        PREEMPT_OFF.with(|c| c.set(c.get() - 1));
        r
    })
}

/// That the running task may wait here -- else what it is doing is a
/// finding.
pub fn check_may_wait(what: &str) {
    match ctx() {
        Ctx::Softirq => panic!("invariant: a soft IRQ handler {}: every packet waits for it -- the receive path \
                                must never sleep", what),
        Ctx::Irq => panic!("invariant: an interrupt handler {}", what),
        Ctx::Task => {}
    }
    if irq_off() != 0 || preempt_off() != 0 {
        panic!("invariant: task {} {} with a spin lock held or interrupts off (irq {}, preempt {})", me(), what,
               irq_off(), preempt_off());
    }
}

/// The running task, blocked on `wait` until `until` at the latest: why it
/// was let go on.
pub fn block(wait: Wait, until: u64) -> Woke {
    let me = me();
    READS.with(|r| r.set(0));
    let mut g = lock();
    let s = sched!(g);
    s.tasks[me].st = St::Blocked(wait);
    s.tasks[me].until = until;
    switch(g, me);
    let g = lock();
    g.as_ref().expect("booted").tasks[me].woke
}

/* ---- what a task does ---- */

/// `kernel_sleep_ns`.
pub fn sleep_ns(ns: u64) {
    check_may_wait("sleeps");
    let until = now().saturating_add(ns).min(END);
    block(Wait::Sleep, until);
}

/// `kernel_task_yield_to_runnable`: another task that can run goes first;
/// with none, straight back.
pub fn yield_now() {
    check_may_wait("yields");
    READS.with(|r| r.set(0));
    let me = me();
    let mut g = lock();
    let s = sched!(g);
    s.tasks[me].st = St::Ready;
    switch(g, me);
}

/// Where a task that could go on may be made to let another run first --
/// as the tick or an IPI would, landing just after a lock was let go of:
/// the input's chaos decides. Only where the task could be switched away.
pub fn preempt_point() {
    if PREEMPT_ONE_IN.load(Ordering::Relaxed) == 0 || ctx() != Ctx::Task || irq_off() != 0 || preempt_off() != 0
        || me() == 0
    {
        return;
    }
    let me = me();
    let mut g = lock();
    let s = sched!(g);
    if s.preempt_one_in == 0 || s.chaos_next() % u64::from(s.preempt_one_in) != 0 {
        return;
    }
    if !s.tasks.iter().enumerate().any(|(i, t)| i != me && t.st == St::Ready) {
        return;
    }
    READS.with(|r| r.set(0));
    s.tasks[me].st = St::Ready;
    switch(g, me);
}

/// A task of the kernel's or the fuzzer's, on a thread of its own, run when
/// the turn comes to it: its handle.
pub fn spawn(name: &str, kind: Kind, cpu: u32, entry: Box<dyn FnOnce() + Send>) -> usize {
    let id = {
        let mut g = lock();
        let s = sched!(g);
        s.tasks.push(Task {
            name: name.to_string(),
            kind,
            /* Not one to run until its thread is known */
            st: St::Done,
            until: u64::MAX,
            woke: Woke::Kicked,
            thread: None,
            stopping: false,
            cpu,
            refs: 1,
            holds: Vec::new(),
        });
        s.tasks.len() - 1
    };
    let thread = std::thread::Builder::new()
        .name(format!("{}-{}", name, id))
        .stack_size(STACK)
        .spawn(move || {
            ME.with(|m| m.set(id));
            wait_turn(id);
            match kind {
                Kind::App => harness(entry),
                _ => kernel(entry),
            }
            exit(id);
        })
        .expect("a thread for a task");
    let mut g = lock();
    let s = sched!(g);
    s.tasks[id].thread = Some(thread.thread().clone());
    s.tasks[id].st = St::Ready;
    id
}

/// The task `me` has returned: it must hold nothing, and whoever waits for
/// it goes on.
fn exit(me: usize) {
    if irq_off() != 0 || preempt_off() != 0 {
        panic!("invariant: task {} ended with interrupts or preemption off (irq {}, preempt {})", me, irq_off(),
               preempt_off());
    }
    let mut g = lock();
    let s = sched!(g);
    if !s.tasks[me].holds.is_empty() {
        panic!("invariant: task {} '{}' ended holding locks {:?}", me, s.tasks[me].name, s.tasks[me].holds);
    }
    s.tasks[me].st = St::Done;
    for i in 1..s.tasks.len() {
        if s.tasks[i].st == St::Blocked(Wait::Join(me)) {
            s.tasks[i].st = St::Ready;
            s.tasks[i].woke = Woke::Joined;
        }
    }
    if s.tasks[WORLD].st == St::Blocked(Wait::World) {
        s.tasks[WORLD].st = St::Ready;
        s.tasks[WORLD].woke = Woke::Kicked;
    }
    /* The turn to whoever is next; this thread ends. */
    loop {
        let s = sched!(g);
        if let Some(next) = s.pick(me) {
            s.tasks[next].st = St::Running;
            s.current = next;
            CURRENT.store(next, Ordering::Release);
            s.handoffs += 1;
            let thread = s.tasks[next].thread.clone().expect("a thread");
            drop(g);
            thread.unpark();
            return;
        }
        let due = s.next_deadline();
        if due == u64::MAX {
            panic!("invariant: every task waits for good:{}", s.describe());
        }
        reach(due);
        let fire = s.due(now());
        drop(g);
        for (handler, ctx) in fire {
            interrupt(|| handler(ctx as *mut u8));
        }
        g = lock();
    }
}

/// `kernel_task_wait`.
pub fn join(task: usize) {
    let me = me();
    if task == me {
        panic!("invariant: task {} waits for itself to end", me);
    }
    {
        let g = lock();
        let s = g.as_ref().expect("booted");
        match s.tasks.get(task) {
            None => panic!("invariant: a wait for task {}, which there is not", task),
            Some(t) if t.st == St::Done => return,
            Some(_) => {}
        }
    }
    check_may_wait("waits for a task");
    block(Wait::Join(task), u64::MAX);
}

pub fn set_stopping(task: usize) {
    let mut g = lock();
    if let Some(t) = sched!(g).tasks.get_mut(task) {
        t.stopping = true;
    }
}

pub fn put(task: usize) {
    let mut g = lock();
    if let Some(t) = sched!(g).tasks.get_mut(task) {
        if t.refs == 0 {
            panic!("invariant: task {}'s handle given back twice", task);
        }
        t.refs -= 1;
    }
}

pub fn stopping() -> bool {
    let me = me();
    let g = lock();
    g.as_ref().is_some_and(|s| s.tasks.get(me).is_some_and(|t| t.stopping))
}

pub fn cpu() -> u32 {
    if ctx() == Ctx::Irq {
        return 0;
    }
    let me = me();
    let g = lock();
    g.as_ref().and_then(|s| s.tasks.get(me)).map_or(0, |t| t.cpu)
}

/// Whether task `t` has ended.
pub fn done(t: usize) -> bool {
    let g = lock();
    g.as_ref().is_some_and(|s| s.tasks.get(t).is_some_and(|t| t.st == St::Done))
}

/* ---- interrupts and preemption ---- */

pub fn irq_save() -> usize {
    let prev = IRQ_OFF.with(|c| c.replace(c.get() + 1));
    PREEMPT_OFF.with(|c| c.set(c.get() + 1));
    if prev == 0 { 1 } else { 0 }
}

pub fn irq_restore(flags: usize) {
    let depth = IRQ_OFF.with(|c| c.get());
    if depth == 0 {
        panic!("invariant: interrupts restored that were never saved");
    }
    /* What the save said about the state before it has to be what the
     * nesting says now: a restore out of order turns interrupts on inside
     * somebody else's critical section. */
    if (flags == 1) != (depth == 1) {
        panic!("invariant: interrupts restored out of order: flags {} at depth {}", flags, depth);
    }
    IRQ_OFF.with(|c| c.set(depth - 1));
    PREEMPT_OFF.with(|c| c.set(c.get() - 1));
    if depth == 1 {
        preempt_point();
    }
}

pub fn preempt_disable() {
    PREEMPT_OFF.with(|c| c.set(c.get() + 1));
}

pub fn preempt_enable() {
    let depth = PREEMPT_OFF.with(|c| c.get());
    if depth == 0 {
        panic!("invariant: preemption enabled that was never disabled");
    }
    PREEMPT_OFF.with(|c| c.set(depth - 1));
    if depth == 1 {
        preempt_point();
    }
}

/* ---- the kernel's locks ---- */

impl Sched {
    /// `lock` taken by `me`, with what it holds: the order checked against
    /// every order seen before.
    fn took(&mut self, me: usize, lock: usize) {
        let held = self.tasks[me].holds.clone();
        for &h in &held {
            if h == lock {
                continue;
            }
            if self.reaches(lock, h) {
                panic!("invariant: lock order: lock {} taken with lock {} held, and elsewhere {} with {} held -- two \
                        tasks doing each would wait on each other for good", lock, h, h, lock);
            }
            self.order.insert((h, lock));
        }
        self.tasks[me].holds.push(lock);
    }

    /// Whether lock `b` was ever taken with `a` held, or through a chain.
    fn reaches(&self, a: usize, b: usize) -> bool {
        let mut seen = BTreeSet::new();
        let mut todo = vec![a];
        while let Some(x) = todo.pop() {
            if x == b {
                return true;
            }
            if !seen.insert(x) {
                continue;
            }
            todo.extend(self.order.range((x, 0)..=(x, usize::MAX)).map(|&(_, y)| y));
        }
        false
    }

    fn released(&mut self, me: usize, lock: usize) {
        let holds = &mut self.tasks[me].holds;
        if let Some(at) = holds.iter().rposition(|&h| h == lock) {
            holds.remove(at);
        }
    }
}

pub fn mutex_create() -> usize {
    let mut g = lock();
    let s = sched!(g);
    s.locks.push(Lock { kind: LockKind::Mutex, owner: 0, waiters: VecDeque::new(), alive: true });
    s.locks.len()
}

pub fn spin_create() -> usize {
    let mut g = lock();
    let s = sched!(g);
    s.locks.push(Lock { kind: LockKind::Spin, owner: 0, waiters: VecDeque::new(), alive: true });
    s.locks.len()
}

pub fn lock_destroy(h: usize) {
    let mut g = lock();
    let s = sched!(g);
    let Some(l) = h.checked_sub(1).and_then(|i| s.locks.get_mut(i)) else {
        panic!("invariant: a lock destroyed that was never made: {}", h)
    };
    if l.owner != 0 {
        panic!("invariant: lock {} destroyed while task {} holds it", h, l.owner);
    }
    l.alive = false;
}

pub fn mutex_lock(h: usize) {
    /* It may sleep, taken or not: never under a spin lock. */
    check_may_wait("takes a mutex");
    let me = me();
    let mut g = lock();
    let s = sched!(g);
    let l = s.locks.get_mut(h.wrapping_sub(1)).filter(|l| l.alive && l.kind == LockKind::Mutex)
        .unwrap_or_else(|| panic!("invariant: mutex {} is no mutex", h));
    if l.owner == me {
        panic!("invariant: task {} takes mutex {} it holds already, and waits for itself for good", me, h);
    }
    if l.owner == 0 {
        l.owner = me;
        s.took(me, h);
        return;
    }
    l.waiters.push_back(me);
    s.tasks[me].st = St::Blocked(Wait::Mutex(h));
    s.tasks[me].until = u64::MAX;
    READS.with(|r| r.set(0));
    switch(g, me);
    let mut g = lock();
    let s = sched!(g);
    /* Its unlocker made it the owner. */
    s.took(me, h);
}

pub fn mutex_unlock(h: usize) {
    let me = me();
    {
        let mut g = lock();
        let s = sched!(g);
        let l = s.locks.get_mut(h.wrapping_sub(1)).filter(|l| l.alive && l.kind == LockKind::Mutex)
            .unwrap_or_else(|| panic!("invariant: mutex {} is no mutex", h));
        if l.owner != me {
            panic!("invariant: task {} lets go of mutex {}, which task {} holds", me, h, l.owner);
        }
        match l.waiters.pop_front() {
            Some(next) => {
                l.owner = next;
                s.tasks[next].st = St::Ready;
                s.tasks[next].woke = Woke::Granted;
            }
            None => l.owner = 0,
        }
        s.released(me, h);
    }
    preempt_point();
}

pub fn spin_lock(h: usize) -> u64 {
    let flags = irq_save() as u64;
    let me = me();
    let mut g = lock();
    let s = sched!(g);
    let l = s.locks.get_mut(h.wrapping_sub(1)).filter(|l| l.alive && l.kind == LockKind::Spin)
        .unwrap_or_else(|| panic!("invariant: spin lock {} is no spin lock", h));
    if l.owner == me {
        panic!("invariant: task {} takes spin lock {} it holds already: its CPU spins on itself for good", me, h);
    }
    if l.owner != 0 {
        panic!("invariant: spin lock {} is held by task {}, which is not running: switched away holding it", h,
               l.owner);
    }
    l.owner = me;
    s.took(me, h);
    flags
}

pub fn spin_unlock(h: usize, flags: u64) {
    let me = me();
    {
        let mut g = lock();
        let s = sched!(g);
        let l = s.locks.get_mut(h.wrapping_sub(1)).filter(|l| l.alive && l.kind == LockKind::Spin)
            .unwrap_or_else(|| panic!("invariant: spin lock {} is no spin lock", h));
        if l.owner != me {
            panic!("invariant: task {} lets go of spin lock {}, which task {} holds", me, h, l.owner);
        }
        l.owner = 0;
        s.released(me, h);
    }
    irq_restore(flags as usize);
}

/* ---- events ---- */

pub fn event_create() -> usize {
    let mut g = lock();
    let s = sched!(g);
    s.events.push(Event { signalled: false, waiter: None, alive: true });
    s.events.len()
}

pub fn event_destroy(h: usize) {
    let mut g = lock();
    let s = sched!(g);
    let e = s.events.get_mut(h.wrapping_sub(1)).filter(|e| e.alive)
        .unwrap_or_else(|| panic!("invariant: event {} destroyed that is no event", h));
    if let Some(w) = e.waiter {
        panic!("invariant: event {} destroyed while task {} waits on it", h, w);
    }
    e.alive = false;
}

/// `kernel_event_wait` and `_wait_for`: true when signalled.
pub fn event_wait(h: usize, until: u64) -> bool {
    check_may_wait("waits on an event");
    let me = me();
    let mut g = lock();
    let s = sched!(g);
    let e = s.events.get_mut(h.wrapping_sub(1)).filter(|e| e.alive)
        .unwrap_or_else(|| panic!("invariant: a wait on event {}, which is no event", h));
    if e.signalled {
        e.signalled = false;
        READS.with(|r| r.set(0));
        drop(g);
        /* A wait that did not block is still a place another may run. */
        preempt_point();
        return true;
    }
    if let Some(w) = e.waiter {
        panic!("invariant: tasks {} and {} wait on event {} at once: the kernel's event has one waiter", w, me, h);
    }
    e.waiter = Some(me);
    s.tasks[me].st = St::Blocked(Wait::Event(h - 1));
    s.tasks[me].until = until;
    READS.with(|r| r.set(0));
    switch(g, me);
    let g = lock();
    g.as_ref().expect("booted").tasks[me].woke == Woke::Signalled
}

pub fn event_signal(h: usize) {
    let mut g = lock();
    let s = sched!(g);
    let e = s.events.get_mut(h.wrapping_sub(1)).filter(|e| e.alive)
        .unwrap_or_else(|| panic!("invariant: a signal of event {}, which is no event", h));
    match e.waiter.take() {
        Some(w) => {
            s.tasks[w].st = St::Ready;
            s.tasks[w].woke = Woke::Signalled;
        }
        None => e.signalled = true,
    }
}

/* ---- soft IRQs and timers ---- */

pub fn softirq_register(typ: usize, handler: extern "C" fn(*mut u8), ctx: usize) {
    let mut g = lock();
    let s = sched!(g);
    let slot = s.softirqs.get_mut(typ).unwrap_or_else(|| panic!("invariant: soft IRQ type {} out of range", typ));
    if slot.is_some() {
        panic!("invariant: soft IRQ {} registered twice", typ);
    }
    *slot = Some((handler, ctx));
}

pub fn softirq_raise(typ: usize) {
    let mut g = lock();
    let s = sched!(g);
    if typ >= SOFTIRQS {
        panic!("invariant: soft IRQ type {} raised, out of range", typ);
    }
    s.pending |= 1 << typ;
    if s.tasks[WORLD].st == St::Blocked(Wait::World) {
        s.tasks[WORLD].st = St::Ready;
        s.tasks[WORLD].woke = Woke::Kicked;
    }
}

pub fn softirq_pending(typ: usize) -> bool {
    let g = lock();
    g.as_ref().is_some_and(|s| typ < SOFTIRQS && s.pending & (1 << typ) != 0)
}

/// Whether any soft IRQ waits to run.
pub fn softirq_pending_any() -> bool {
    let g = lock();
    g.as_ref().is_some_and(|s| s.pending != 0)
}

/// The soft IRQs raised, run: the world's, in task context, one type at a
/// time and each to its end -- none of them may sleep. What they raise
/// runs too, before this returns.
pub fn run_softirqs() {
    loop {
        let next = {
            let mut g = lock();
            let s = sched!(g);
            if s.pending == 0 {
                return;
            }
            let typ = s.pending.trailing_zeros() as usize;
            s.pending &= !(1 << typ);
            s.softirqs[typ]
        };
        if let Some((handler, ctx)) = next {
            in_ctx(Ctx::Softirq, || kernel(|| handler(ctx as *mut u8)));
        }
    }
}

pub fn timer_start(handler: extern "C" fn(*mut u8), ctx: usize, period: u64) -> usize {
    if period == 0 {
        return 0;
    }
    let mut g = lock();
    let s = sched!(g);
    let next = now().saturating_add(period);
    s.timers.push(Timer { handler, ctx, period, next, alive: true });
    s.timers.len()
}

pub fn timer_stop(h: usize) {
    let mut g = lock();
    let s = sched!(g);
    match h.checked_sub(1).and_then(|i| s.timers.get_mut(i)) {
        Some(t) if t.alive => t.alive = false,
        _ => panic!("invariant: timer {} stopped that is not running", h),
    }
}

/* ---- the world ---- */

/// The world waits: until something happens -- a frame out of a NIC, a soft
/// IRQ raised, a task done -- or `until`, whichever is first. Soft IRQs
/// raised meanwhile have run by the time it returns.
pub fn world_wait(until: u64) {
    run_softirqs();
    let pending = {
        let g = lock();
        g.as_ref().is_some_and(|s| s.pending != 0)
    };
    if !pending && now() < until {
        block(Wait::World, until);
    }
    run_softirqs();
}

/// Something happened the world will want to see.
pub fn world_kick() {
    let mut g = lock();
    let s = sched!(g);
    if s.tasks[WORLD].st == St::Blocked(Wait::World) {
        s.tasks[WORLD].st = St::Ready;
        s.tasks[WORLD].woke = Woke::Kicked;
    }
}

/// Every task but the world: its name, whether it has ended, and what it
/// holds -- for the audit at an input's end.
pub fn tasks() -> Vec<(usize, String, Kind, St, Vec<usize>)> {
    let g = lock();
    g.as_ref().map_or(Vec::new(), |s| {
        s.tasks.iter().enumerate().skip(2).map(|(i, t)| (i, t.name.clone(), t.kind, t.st, t.holds.clone())).collect()
    })
}

pub fn handoffs() -> u64 {
    let g = lock();
    g.as_ref().map_or(0, |s| s.handoffs)
}
