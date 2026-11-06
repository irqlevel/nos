#![no_std]

//! The module the kernel loads at boot, out of its own image, to test the
//! loader (test_modules in kernel/test.cpp). Its init does what a module does
//! and checks every result: a check that fails fails the init, the load and
//! the boot test. What it leaves behind is a shell command, which the test
//! runs through the kernel's dispatcher and reads the answer of. It takes
//! one parameter, `answer=N`, the number the command answers with, so the
//! test can see what `insmod` was given reach the init.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use kcore::cmd::{Command, Output};
use kcore::events::SharedEvent;
use kcore::ipc::Mailbox;
use kcore::mgmt::{Call, Kind, Method, Provider, Status};
use kcore::error::{Error, Result};
use kcore::sync::Mutex;

/* Initialized data, zeroed data, and a table of function pointers. The first
   lands in .data, the second in .bss, and the third needs a relocation per
   entry: each has to come out right for its check below to pass. */
static DATA: [AtomicU32; 4] = [AtomicU32::new(1), AtomicU32::new(2), AtomicU32::new(3), AtomicU32::new(4)];
static ZEROED: AtomicU32 = AtomicU32::new(0);
static TABLE: [fn(u32) -> u32; 3] = [double, square, negate];

/* A megabyte of .bss: twice what a module's image could once be, which it
   can be now that images are mapped from a window of their own. It has to
   come out mapped, writable and zeroed from its first page to its last. */
const BIG_LEN: usize = 1 << 20;
const PAGE: usize = 4096;
static BIG: [AtomicU8; BIG_LEN] = [const { AtomicU8::new(0) }; BIG_LEN];

fn double(x: u32) -> u32 {
    x * 2
}

fn square(x: u32) -> u32 {
    x * x
}

fn negate(x: u32) -> u32 {
    x.wrapping_neg()
}

/* A vtable is a table of function pointers the compiler writes. */
trait Shape {
    fn area(&self) -> u32;
}

struct Rect(u32, u32);

impl Shape for Rect {
    fn area(&self) -> u32 {
        self.0 * self.1
    }
}

struct ModTest {
    _cmd: Command,
    /* Dropped with the module: its unregister waits out a job of it */
    _provider: Provider,
    /* The mailbox `modtest ipc` keeps, which kernel.echo pesters: closed by
     * this drop, under that traffic, after the command is gone */
    _ipc: Arc<Mutex<Option<Mailbox>>>,
}

impl kmod::Module for ModTest {}

impl Drop for ModTest {
    fn drop(&mut self) {
        kcore::klog!(0, "modtest: unloaded");
    }
}

fn check(ok: bool, what: &str) -> Result<()> {
    if ok {
        return Ok(());
    }
    kcore::klog!(0, "modtest: {} came out wrong", what);
    Err(Error::InvalidValue)
}

/// `answer=N`, if it was given; what else was, refused as a module refuses
/// a parameter it has not got.
fn answer_given(params: &kmod::Params, out: &mut Output) -> Result<Option<u32>> {
    let mut answer = None;
    for (key, value) in params.words() {
        match (key, value.map(str::parse::<u32>)) {
            ("answer", Some(Ok(n))) if answer.is_none() => answer = Some(n),
            _ => {
                let _ = writeln!(out, "modtest: \"{}\" is not a parameter of modtest's -- answer=N is, once", key);
                return Err(Error::InvalidValue);
            }
        }
    }
    Ok(answer)
}

fn init(params: &kmod::Params, out: &mut Output) -> Result<Box<dyn kmod::Module>> {
    let given = answer_given(params, out)?;

    let data: u32 = DATA.iter().map(|v| v.load(Ordering::Relaxed)).sum();
    check(data == 10, ".data")?;

    ZEROED.fetch_add(5, Ordering::Relaxed);
    check(ZEROED.load(Ordering::Relaxed) == 5, ".bss")?;

    let zeroed = (0..BIG_LEN).step_by(PAGE).all(|i| BIG[i].load(Ordering::Relaxed) == 0);
    check(zeroed && BIG[BIG_LEN - 1].load(Ordering::Relaxed) == 0, "a megabyte of .bss")?;
    BIG[BIG_LEN - 1].store(0x5a, Ordering::Relaxed);
    check(BIG[BIG_LEN - 1].load(Ordering::Relaxed) == 0x5a, "the last page of .bss")?;

    let table = TABLE.iter().fold(0u32, |acc, f| acc.wrapping_add(f(3)));
    check(table == 12, "a table of function pointers")?;

    let shape: Box<dyn Shape> = Box::new(Rect(6, 7));
    check(shape.area() == 42, "a trait object")?;

    let v: Vec<u32> = (1..=100).collect();
    check(v.iter().sum::<u32>() == 5050, "a Vec from the kernel's allocator")?;

    /* A kernel object through the export table, created and destroyed. */
    let lock = kcore::sync::SpinLock::new(()).ok_or(Error::NoMemory)?;
    {
        let _guard = lock.lock();
    }

    /* Where one of its functions landed, for the test to name through the
       kernel's symbolizer -- the way a backtrace through this module would */
    let answer = given.unwrap_or(shape.area() + data);
    let probe = double as fn(u32) -> u32 as usize;
    let ipc_kept = Arc::new(Mutex::new(None).ok_or(Error::NoMemory)?);
    let ipc_cmd = ipc_kept.clone();
    let cmd = Command::register("modtest", "modtest - the loader self-test module's command", move |args, out| {
        if args.trim() == "ipc" {
            match ipc(&ipc_cmd) {
                Ok(()) => {
                    let _ = writeln!(out, "modtest ipc: ok");
                }
                Err(what) => {
                    let _ = writeln!(out, "modtest ipc: {} went wrong", what);
                }
            }
            return;
        }
        if args.trim() == "bus" {
            match bus() {
                Ok(()) => {
                    let _ = writeln!(out, "modtest bus: ok");
                }
                Err(what) => {
                    let _ = writeln!(out, "modtest bus: {} went wrong", what);
                }
            }
            return;
        }
        let _ = writeln!(out, "modtest: answer {} args '{}' double at {}", answer, args, probe);
    })?;

    /* A provider on the management bus, for the test to call, and to start
       a job of that its unload has to stop and wait for */
    let methods = [Method { name: "echo", kind: Kind::Read }, Method { name: "wait", kind: Kind::Job },
                   Method { name: "nap", kind: Kind::Job }];
    let provider = Provider::register("modtest", &methods, include_str!("modtest.descriptor.json"), provide)
        .map_err(|_| Error::Busy)?;

    kcore::klog!(0, "modtest: loaded, every check passed");
    Ok(Box::new(ModTest { _cmd: cmd, _provider: provider, _ipc: ipc_kept }))
}

/// How long `wait` waits, at most, before it says nobody told it to stop.
const WAIT_LIMIT_MS: u64 = 10_000;

/// The provider's methods: `echo` answers with its parameters; `wait`, a
/// job, runs until it is told to stop -- by the module's unload; `nap`, a
/// job that ends on its own, with a line of its log and a result.
fn provide(method: &str, params: &[u8], call: &mut Call) -> Status {
    match method {
        "echo" => {
            call.reply(params);
            Status::Ok
        }
        "nap" => {
            call.note("modtest: napping");
            kcore::task::sleep_ms(10);
            call.progress(1, 1);
            call.reply(br#"{"napped":true}"#);
            Status::Ok
        }
        "wait" => {
            call.note("modtest: waiting to be told to stop");
            let mut ms = 0;
            while !call.cancelled() {
                call.progress(ms, 0);
                kcore::task::sleep_ms(1);
                ms += 1;
                if ms > WAIT_LIMIT_MS {
                    return Status::Failed;
                }
            }
            Status::Cancelled
        }
        _ => Status::NoMethod,
    }
}

/// What the bus answers into `f`'s sink: its status and every byte.
fn answered(f: impl FnOnce(&mut dyn FnMut(&[u8]) -> bool) -> Status) -> (Status, Vec<u8>) {
    let mut got = Vec::new();
    let status = f(&mut |b| {
        got.extend_from_slice(b);
        true
    });
    (status, got)
}

fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `modtest bus`: the bus as a module asks it -- the host API's way in --
/// gone through once: what a method is registered as, a call of this
/// module's own provider and of the kernel's, the providers a reader and an
/// admin are shown, a descriptor, and a job started, run to its end, its
/// log and result read. What went wrong first, if anything.
fn bus() -> core::result::Result<(), &'static str> {
    use kcore::mgmt as m;
    if m::kind("system", "power_off") != Ok(Kind::Action) || m::kind("system", "info") != Ok(Kind::Read) {
        return Err("a method's kind");
    }
    if m::kind("nobody", "info") != Err(Status::NoProvider) || m::kind("system", "nope") != Err(Status::NoMethod) {
        return Err("a kind there is not");
    }
    let (s, got) = answered(|sink| m::call("modtest", "echo", br#"{"n":7}"#, 64, sink));
    if s != Status::Ok || got != br#"{"n":7}"# {
        return Err("a call of the module's own provider");
    }
    let (s, got) = answered(|sink| m::call("system", "info", b"", 4096, sink));
    if s != Status::Ok || !has(&got, b"\"arch\"") {
        return Err("a call of the kernel's");
    }
    let (s, got) = answered(|sink| m::providers(m::KINDS_READ, sink));
    if s != Status::Ok || !has(&got, b"\"system\"") || has(&got, b"\"power_off\"") {
        return Err("the providers a reader is shown");
    }
    let (s, got) = answered(|sink| m::providers(m::KINDS_ALL, sink));
    if s != Status::Ok || !has(&got, b"\"power_off\"") {
        return Err("the providers an admin is shown");
    }
    let (s, got) = answered(|sink| m::describe("modtest", sink));
    if s != Status::Ok || !has(&got, b"\"provider\": \"modtest\"") {
        return Err("a descriptor");
    }
    if m::job_start("modtest", "echo", b"") != Err(Status::Kind) {
        return Err("a call started as a job");
    }
    let id = m::job_start("modtest", "nap", b"").map_err(|_| "a job started")?;
    let want_id = alloc::format!("\"id\":{}", id);
    for _ in 0..5000 {
        let (s, got) = answered(|sink| m::job(id, 0, sink));
        if s != Status::Ok {
            return Err("a job's record");
        }
        if has(&got, b"\"state\":\"done\"") {
            if !has(&got, b"\"status\":\"ok\"") || !has(&got, b"\"result\":{\"napped\":true}")
                || !has(&got, b"modtest: napping") {
                return Err("a job done: its status, log or result");
            }
            if m::job_cancel(id) != Status::NotFound {
                return Err("a job done told to stop");
            }
            let (s, got) = answered(|sink| m::jobs(sink));
            if s != Status::Ok || !has(&got, want_id.as_bytes()) {
                return Err("the list of jobs");
            }
            return Ok(());
        }
        kcore::task::sleep_ms(1);
    }
    Err("a job that never ended")
}

/// kernel.echo's tag for "monitor me, and keep sending until I am gone"
/// (ipc::echo's TAG_ECHO_PESTER).
const TAG_ECHO_PESTER: u32 = 0x5045_5354;

/// `n` bytes of a pattern.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(13).wrapping_add(5)).collect()
}

/// What a module's test task and the task asleep on its event share.
struct Waiter {
    event: SharedEvent,
    /// How many of its waits a signal ended.
    woke: AtomicU32,
    stop: AtomicBool,
}

/// How long the task sleeps at a time -- far past what a signal takes to
/// wake it -- and how long the test waits for it to have woken.
const NAP_SECS: u64 = 30;
const WITHIN_MS: u64 = 5000;

/// The task: asleep on the event until told to stop, counting the waits a
/// signal ended.
fn nap(w: Arc<Waiter>) {
    while !w.stop.load(Ordering::Acquire) {
        if w.event.wait_for(kcore::time::Duration::from_secs(NAP_SECS)) {
            w.woke.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Whether the task's waits ended past `before` within WITHIN_MS.
fn woken(w: &Waiter, before: u32) -> bool {
    for _ in 0..WITHIN_MS {
        if w.woke.load(Ordering::Acquire) != before {
            return true;
        }
        kcore::task::sleep_ms(1);
    }
    false
}

/// One event to wait on, as a module has it (plans/ipc.md, I2): a mailbox
/// and a device's queue made on an event of the module's, and a task of its
/// asleep on it -- woken by a message and by the queue's wake; the event let
/// go of first, the mailbox and the queue going on as they were, a message
/// and a wake after it harmless. The device is the first nobody holds, for
/// reading, and nothing is read from it: a boot's test does no I/O. What
/// went wrong first, if anything.
fn one_event() -> core::result::Result<(), &'static str> {
    use kcore::block::Disk;
    use kcore::ipc::Limits;
    let event = SharedEvent::new().ok_or("an event")?;
    let mb = Mailbox::new_on(Limits { messages: 4, bytes: 64 }, &event).map_err(|_| "a mailbox on an event")?;
    let q = (0..Disk::count()).filter_map(Disk::at).find_map(|d| {
        let name = d.name();
        d.into_queue_on(&event, false, "modtest", 1, 4096).ok().map(|q| (name, q))
    });
    match &q {
        Some((name, _)) => kcore::klog!(0, "modtest: one event -- a mailbox and {}'s queue made on it", name),
        None => kcore::klog!(0, "modtest: one event -- a mailbox made on it, and no device for a queue"),
    }
    let w = Arc::new(Waiter { event, woke: AtomicU32::new(0), stop: AtomicBool::new(false) });
    let task = kcore::task::spawn_with("modtest-event", w.clone(), nap).ok_or("a task to wait")?;
    let before = w.woke.load(Ordering::Acquire);
    let mut woke = mb.send(mb.addr(), 1, 0, b"x").is_ok() && woken(&w, before)
        && mb.try_recv().is_some_and(|m| m.tag == 1 && m.data == b"x");
    if let Some((_, q)) = &q {
        let before = w.woke.load(Ordering::Acquire);
        q.wake();
        woke &= woken(&w, before);
    }
    w.stop.store(true, Ordering::Release);
    w.event.signal();
    drop(task);
    if !woke {
        return Err("a message or a queue's wake that did not wake the task waiting on their event");
    }
    /* The event let go of first: the kernel holds it for the mailbox and the
     * queue, which go on as they were */
    drop(w);
    if mb.send(mb.addr(), 2, 0, b"late").is_err() || !mb.try_recv().is_some_and(|m| m.tag == 2) {
        return Err("a message after the mailbox's event was let go of");
    }
    if let Some((_, q)) = &q {
        q.wake();
    }
    drop(q);
    drop(mb);
    Ok(())
}

/// `modtest ipc`: the message channel as a module has it, through
/// kernel.echo, which the boot's test has started -- one event to wait on
/// first (`one_event`), then calls of every size, its names held to its
/// namespace, an address gone, a Down, a full mailbox, the names listed;
/// one mailbox left open on purpose, for the sweep after the unload to
/// close; and the one it keeps in `keep`, which it asks the echo to
/// pester, so that the unload closes it under traffic. What went wrong
/// first, if anything.
fn ipc(keep: &Mutex<Option<Mailbox>>) -> core::result::Result<(), &'static str> {
    use kcore::ipc::{list, lookup, Error as E, Limits, Reason, Scope, TAG_DOWN};
    use kcore::time::Duration;
    const ROOMY: Limits = Limits { messages: 64, bytes: 1 << 20 };
    one_event()?;
    let echo = lookup("kernel.echo").ok_or("kernel.echo, not there,")?;
    let mb = Mailbox::new(ROOMY).map_err(|_| "a mailbox")?;
    mb.register("modtest.ipc", Scope::Host).map_err(|_| "a name of the module's")?;
    if mb.register("modtest.again", Scope::Host) != Err(E::Taken) {
        return Err("a second name");
    }
    let other = Mailbox::new(ROOMY).map_err(|_| "a second mailbox")?;
    if other.register("kernel.modtest", Scope::Host) != Err(E::NotYours)
        || other.register("nbs.modtest", Scope::Cluster) != Err(E::NotYours) {
        return Err("a name outside the module's namespace");
    }
    for n in [0, 1, 4095, 4096, 4097, 200_000] {
        let data = pattern(n);
        let reply = mb.call(echo, 7, &data, Duration::from_secs(5)).map_err(|_| "a call of kernel.echo")?;
        if reply.tag != 7 || reply.data != data {
            return Err("an echo's bytes");
        }
    }
    /* An echo received in pieces, as a message larger than one block of
     * the heap has to be: 200 KB in 64 KiB pieces, and one of no bytes */
    for n in [200_000, 0] {
        let data = pattern(n);
        mb.send(echo, 8, 99, &data).map_err(|_| "a send to kernel.echo")?;
        let got = mb.recv_pieces(Duration::from_secs(5), kcore::ipc::PIECE_MAX).ok_or("an echo in pieces")?;
        let mut at = 0;
        for p in &got.pieces {
            if p.len() > kcore::ipc::PIECE_MAX || data.get(at..at + p.len()) != Some(&p[..]) {
                return Err("an echo's pieces");
            }
            at += p.len();
        }
        if got.tag != 8 || got.reference != 99 || got.from != echo || got.len != n || at != n {
            return Err("an echo received in pieces");
        }
    }
    let gone = Mailbox::new(ROOMY).map_err(|_| "a mailbox")?.addr();
    if mb.send(gone, 1, 0, b"x") != Err(E::Gone) {
        return Err("a message to a mailbox closed");
    }
    let t = Mailbox::new(ROOMY).map_err(|_| "a mailbox")?;
    let id = mb.monitor(t.addr()).map_err(|_| "a monitor")?;
    let target = t.addr();
    drop(t);
    match mb.recv(Duration::from_secs(1)) {
        Some(m) if m.tag == TAG_DOWN && m.reference == id && m.from == target && m.down() == Some(Reason::Closed) => {}
        _ => return Err("a Down"),
    }
    let small = Mailbox::new(Limits { messages: 2, bytes: 64 }).map_err(|_| "a small mailbox")?;
    let s = small.addr();
    if mb.send(s, 1, 0, b"") != Ok(()) || mb.send(s, 1, 1, b"") != Ok(()) || mb.send(s, 1, 2, b"") != Err(E::Full) {
        return Err("a full mailbox");
    }
    if !list("modtest.").iter().any(|n| n.name == "modtest.ipc" && n.addr == mb.addr()) {
        return Err("the module's name listed");
    }
    /* Left open on purpose: the sweep after the unload closes it */
    let leaked = Mailbox::new(ROOMY).map_err(|_| "a mailbox to leave open")?;
    leaked.register("modtest.leaked", Scope::Host).map_err(|_| "the left one's name")?;
    core::mem::forget(leaked);
    mb.send(echo, TAG_ECHO_PESTER, 0, b"").map_err(|_| "asking the echo to pester")?;
    *keep.lock() = Some(mb);
    Ok(())
}

kmod::module!(name: "modtest", init_with_params: init);
