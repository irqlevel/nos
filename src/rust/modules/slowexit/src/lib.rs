#![no_std]

//! A module whose exit takes its time, the way one waiting out a running
//! command of its own does: longer than the shell waits for an rmmod, so the
//! unload finishes in the background while lsmod shows it going
//! (docs/modules.md). For the end-to-end test; the kernel never loads it.

extern crate alloc;

use alloc::boxed::Box;

/* Well past how long the shell waits for an unload */
const EXIT_MS: u64 = 8000;

struct SlowExit;

impl kmod::Module for SlowExit {}

impl Drop for SlowExit {
    fn drop(&mut self) {
        kcore::trace!(0, "slowexit: exit takes {} ms", EXIT_MS);
        kcore::task::sleep_ms(EXIT_MS);
        kcore::trace!(0, "slowexit: exit done");
    }
}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    kcore::trace!(0, "slowexit: loaded");
    Ok(Box::new(SlowExit))
}

kmod::module!(name: "slowexit", init: init);
