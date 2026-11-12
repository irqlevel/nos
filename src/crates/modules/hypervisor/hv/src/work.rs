//! What a long operation of hv's says as it goes, and is asked: the lines a
//! command prints -- on a shell's output, or as a job's log on the
//! management bus -- how far it has got, and whether it is to stop. A
//! download, a conversion and a copy of a VM's disks are each one function,
//! whoever asked for them.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use kcore::cmd::Output;

pub trait Work: Write {
    /// How far it has got: `done` of `total`, 0 when that is not known -- a
    /// job's progress; a shell has the lines.
    fn progress(&mut self, _done: u64, _total: u64) {}

    /// Whether it is to stop where it is, and take back what it made: a job
    /// cancelled, the module going.
    fn cancelled(&self) -> bool;
}

/// A command typed at a shell: its output, and the module's going -- which
/// waits for the command -- to stop it.
pub struct Shell<'a> {
    pub out: &'a mut Output,
    pub unloading: &'a AtomicBool,
}

impl Write for Shell<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.out.write_str(s)
    }
}

impl Work for Shell<'_> {
    fn cancelled(&self) -> bool {
        self.unloading.load(Ordering::Acquire)
    }
}

/// What every text a stop is said as begins with, where an operation says
/// why it stopped: what the bus answers Cancelled for (`is_stop`).
const STOP_HEAD: &str = "stopped, as asked";

/// What a stop is said as, where what the operation made is taken away.
pub const STOPPED: &str = "stopped, as asked -- what it had made taken away";

/// What a stop is said as where what it did is not taken back: `left`, what
/// it leaves -- a device holding part of an image, a guest still running.
#[cfg(target_arch = "x86_64")]
pub fn stopped_leaving(left: &str) -> alloc::string::String {
    alloc::format!("{} -- {}", STOP_HEAD, left)
}

/// Whether `why` an operation ended is a stop (`STOPPED`, `stopped_leaving`).
#[cfg(target_arch = "x86_64")]
pub fn is_stop(why: &str) -> bool {
    why.starts_with(STOP_HEAD)
}

/// Whether `text` begins with `head`, for the compiler to hold `STOPPED` to
/// `STOP_HEAD`.
const fn begins(text: &str, head: &str) -> bool {
    let (t, h) = (text.as_bytes(), head.as_bytes());
    if h.len() > t.len() {
        return false;
    }
    let mut i = 0;
    while i < h.len() {
        if t[i] != h[i] {
            return false;
        }
        i += 1;
    }
    true
}

const _: () = assert!(begins(STOPPED, STOP_HEAD));
