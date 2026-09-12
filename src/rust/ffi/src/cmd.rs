use core::ffi::c_void;

/// A shell command's handler: the context it was registered with, the
/// arguments after the command's name, and the printer its output goes to.
pub type CmdHandler =
    unsafe extern "C" fn(ctx: *mut c_void, args: *const u8, args_len: usize, out: *mut c_void);

extern "C" {
    /// Adds a shell command. Returns a handle for `kernel_cmd_unregister`,
    /// or 0 when the name is taken or the table is full.
    pub fn kernel_cmd_register(
        name: *const u8,
        name_len: usize,
        help: *const u8,
        help_len: usize,
        handler: CmdHandler,
        ctx: *mut c_void,
    ) -> usize;

    /// Removes it, returning once no call of it is still running -- it
    /// sleeps until then, so only from a task, and never from the
    /// command's own handler.
    pub fn kernel_cmd_unregister(handle: usize);

    /// Writes to the printer a handler was given.
    pub fn kernel_printer_write(out: *mut c_void, buf: *const u8, len: usize);
}

/// Where kernel_cmd_dispatch hands what a command prints: the ctx it was
/// given, and a piece of the output.
pub type CmdSink = unsafe extern "C" fn(ctx: *mut c_void, buf: *const u8, len: usize);

extern "C" {
    /// Runs a shell command line as the console would, handing what it
    /// prints to the sink, a piece at a time. Sleeps as long as the command
    /// runs.
    pub fn kernel_cmd_dispatch(line: *const u8, len: usize, sink: CmdSink, ctx: *mut c_void);
}
