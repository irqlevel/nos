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
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use kcore::cmd::{Command, Output};
use kcore::mgmt::{Call, Kind, Method, Provider, Status};
use kcore::error::{Error, Result};

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
}

impl kmod::Module for ModTest {}

impl Drop for ModTest {
    fn drop(&mut self) {
        kcore::trace!(0, "modtest: unloaded");
    }
}

fn check(ok: bool, what: &str) -> Result<()> {
    if ok {
        return Ok(());
    }
    kcore::trace!(0, "modtest: {} came out wrong", what);
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
    let cmd = Command::register("modtest", "modtest - the loader self-test module's command", move |args, out| {
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

    kcore::trace!(0, "modtest: loaded, every check passed");
    Ok(Box::new(ModTest { _cmd: cmd, _provider: provider }))
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

kmod::module!(name: "modtest", init_with_params: init);
