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
        let _ = writeln!(out, "modtest: answer {} args '{}' double at {}", answer, args, probe);
    })?;

    /* A provider on the management bus, for the test to call, and to start
       a job of that its unload has to stop and wait for */
    let methods = [Method { name: "echo", kind: Kind::Read }, Method { name: "wait", kind: Kind::Job }];
    let provider = Provider::register("modtest", &methods, "{}", provide).map_err(|_| Error::Busy)?;

    kcore::trace!(0, "modtest: loaded, every check passed");
    Ok(Box::new(ModTest { _cmd: cmd, _provider: provider }))
}

/// How long `wait` waits, at most, before it says nobody told it to stop.
const WAIT_LIMIT_MS: u64 = 10_000;

/// The provider's methods: `echo` answers with its parameters; `wait`, a
/// job, runs until it is told to stop -- by the module's unload.
fn provide(method: &str, params: &[u8], call: &mut Call) -> Status {
    match method {
        "echo" => {
            call.reply(params);
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

kmod::module!(name: "modtest", init_with_params: init);
