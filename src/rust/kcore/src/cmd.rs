//! Shell commands registered at run time -- by a loadable module, above all,
//! which has no other way to put a command in front of whoever runs it.

use alloc::boxed::Box;
use core::ffi::c_void;
use ffi::cmd;

use crate::error::{Error, Result};
use crate::time::Duration;

/// Where a command's output goes: the console or the UDP shell, whichever
/// ran it.
pub struct Output {
    printer: *mut c_void,
}

impl core::fmt::Write for Output {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}

impl Output {
    /// A printer the kernel passed in, for code that is not a command
    /// handler but prints where one would -- the boot's DHCP, on the shell's
    /// console.
    ///
    /// # Safety
    /// `printer` is a `Stdlib::Printer*` that outlives the Output.
    pub unsafe fn from_raw(printer: *mut c_void) -> Self {
        Self { printer }
    }

    /// Bytes as they are, for output that is not text -- a file `cat` prints,
    /// which is whatever was written to it.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        unsafe { cmd::kernel_printer_write(self.printer, bytes.as_ptr(), bytes.len()) };
    }

    /// What the person at the other end types while the command runs, for
    /// a command that asks: up to `buf.len()` bytes, waiting up to `timeout`
    /// for some -- after whatever the command has printed has gone out.
    /// `Some(0)` when the time passed with nothing; `None` when nobody can
    /// type here -- the console, the UDP shell, `/etc/rc` -- or no more will
    /// come, the session over. Only an SSH session has anyone to ask.
    /// Sleeps: task context, no lock held.
    pub fn read_input(&mut self, buf: &mut [u8], timeout: Duration) -> Option<usize> {
        let n = unsafe {
            cmd::kernel_printer_read(self.printer, buf.as_mut_ptr(), buf.len(), timeout.as_nanos())
        };
        usize::try_from(n).ok().map(|n| n.min(buf.len()))
    }
}

type Handler = dyn Fn(&str, &mut Output) + Send + Sync;

/// How much of a command's help `help` shows: the kernel keeps this many
/// characters of it (Cmd::DynamicHelpMax) and drops the rest.
pub const HELP_MAX: usize = 95;

/// A registered command. Dropping it takes the command away, after any call
/// of it still running has returned -- so its handler, and a module holding
/// one, can go right after.
pub struct Command {
    handle: usize,
    handler: *mut Box<Handler>,
}

/* The handler behind the pointer is Send + Sync, and only Drop frees it. */
unsafe impl Send for Command {}
unsafe impl Sync for Command {}

impl Command {
    pub fn register<F>(name: &str, help: &str, handler: F) -> Result<Self>
    where
        F: Fn(&str, &mut Output) + Send + Sync + 'static,
    {
        let raw: *mut Box<Handler> = Box::into_raw(Box::new(Box::new(handler)));
        let handle = unsafe {
            cmd::kernel_cmd_register(
                name.as_ptr(),
                name.len(),
                help.as_ptr(),
                help.len(),
                trampoline,
                raw as *mut c_void,
            )
        };

        if handle == 0 {
            drop(unsafe { Box::from_raw(raw) });
            return Err(Error::Busy);
        }

        Ok(Self { handle, handler: raw })
    }
}

impl Drop for Command {
    fn drop(&mut self) {
        unsafe { cmd::kernel_cmd_unregister(self.handle) };
        drop(unsafe { Box::from_raw(self.handler) });
    }
}

/// What a session gives a command it runs: where its output goes, and what is
/// typed at it while it runs, for a command that asks (`Output::read_input`).
pub trait Session {
    fn write(&mut self, bytes: &[u8]);
    /// Up to `buf.len()` bytes typed, waiting up to `timeout_ns` for some:
    /// `Some(0)` when the time passed with nothing, `None` when no more
    /// will come.
    fn read(&mut self, buf: &mut [u8], timeout_ns: u64) -> Option<usize>;
}

/// Runs a shell command line as the console would, its output going to
/// `session` as it prints it and what is typed at it read from `session`
/// when it asks -- how an SSH session runs the shell's commands. Returns when
/// the command does, so task context only.
pub fn dispatch_session(line: &str, session: &mut dyn Session) {
    let mut session = session;
    let ctx = &mut session as *mut &mut dyn Session as *mut c_void;
    unsafe { cmd::kernel_cmd_dispatch_io(line.as_ptr(), line.len(), session_sink, session_source, ctx) };
}

/// # Safety
/// `ctx` is the `&mut &mut dyn Session` `dispatch_session` passed, alive
/// for as long as the command runs, and `buf` is `len` bytes to read.
unsafe extern "C" fn session_sink(ctx: *mut c_void, buf: *const u8, len: usize) {
    let session = unsafe { &mut *(ctx as *mut &mut dyn Session) };
    session.write(unsafe { core::slice::from_raw_parts(buf, len) });
}

/// # Safety
/// As `session_sink`, with `buf` `len` bytes to write. The two are never
/// called at once: both are called by the command, on its own task.
unsafe extern "C" fn session_source(ctx: *mut c_void, buf: *mut u8, len: usize, timeout_ns: u64) -> isize {
    let session = unsafe { &mut *(ctx as *mut &mut dyn Session) };
    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    match session.read(buf, timeout_ns) {
        Some(n) => n.min(len) as isize,
        None => -1,
    }
}

/// Runs a shell command line as the console would, and hands what it
/// prints to `sink` a piece at a time as it prints it -- how an SSH session
/// puts the shell's output on its channel. Returns when the command does,
/// so task context only.
pub fn dispatch(line: &str, sink: &mut dyn FnMut(&[u8])) {
    let mut sink = sink;
    let ctx = &mut sink as *mut &mut dyn FnMut(&[u8]) as *mut c_void;
    unsafe { cmd::kernel_cmd_dispatch(line.as_ptr(), line.len(), dispatch_sink, ctx) };
}

unsafe extern "C" fn dispatch_sink(ctx: *mut c_void, buf: *const u8, len: usize) {
    let sink = unsafe { &mut *(ctx as *mut &mut dyn FnMut(&[u8])) };
    sink(unsafe { core::slice::from_raw_parts(buf, len) });
}

unsafe extern "C" fn trampoline(ctx: *mut c_void, args: *const u8, args_len: usize, out: *mut c_void) {
    let handler = unsafe { &*(ctx as *const Box<Handler>) };
    let bytes = unsafe { core::slice::from_raw_parts(args, args_len) };
    let mut output = Output { printer: out };
    handler(core::str::from_utf8(bytes).unwrap_or(""), &mut output);
}
