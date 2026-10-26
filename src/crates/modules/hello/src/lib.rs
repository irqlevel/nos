#![no_std]

//! A loadable module about as small as one can usefully be: a shell command
//! that lives as long as the module does.
//!
//!     make modules                  -> out/<arch>/modules/hello.ko
//!     insmod /hello.ko; hello nos; rmmod hello

extern crate alloc;

use alloc::boxed::Box;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};
use kcore::cmd::Command;

static CALLS: AtomicU64 = AtomicU64::new(0);

struct Hello {
    _cmd: Command,
}

impl kmod::Module for Hello {}

impl Drop for Hello {
    fn drop(&mut self) {
        kcore::trace!(0, "hello: unloaded after {} calls", CALLS.load(Ordering::Relaxed));
    }
}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let cmd = Command::register("hello", "hello [name] - a command from a loadable module", |args, out| {
        let calls = CALLS.fetch_add(1, Ordering::Relaxed) + 1;
        let who = if args.is_empty() { "world" } else { args };
        let _ = writeln!(out, "hello, {} -- call {} since the module was loaded", who, calls);
    })?;

    kcore::trace!(0, "hello: loaded");
    Ok(Box::new(Hello { _cmd: cmd }))
}

kmod::module!(name: "hello", init: init);
