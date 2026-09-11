//! Shell commands registered at run time -- by a loadable module, above all,
//! which has no other way to put a command in front of whoever runs it.

use alloc::boxed::Box;
use core::ffi::c_void;
use ffi::cmd;

use crate::error::{Error, Result};

/// Where a command's output goes: the console or the UDP shell, whichever
/// ran it.
pub struct Output {
    printer: *mut c_void,
}

impl core::fmt::Write for Output {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        unsafe { cmd::kernel_printer_write(self.printer, s.as_ptr(), s.len()) };
        Ok(())
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

unsafe extern "C" fn trampoline(ctx: *mut c_void, args: *const u8, args_len: usize, out: *mut c_void) {
    let handler = unsafe { &*(ctx as *const Box<Handler>) };
    let bytes = unsafe { core::slice::from_raw_parts(args, args_len) };
    let mut output = Output { printer: out };
    handler(core::str::from_utf8(bytes).unwrap_or(""), &mut output);
}
