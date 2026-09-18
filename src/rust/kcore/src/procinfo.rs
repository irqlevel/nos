//! What the kernel knows about itself, for whoever reports it: the version,
//! the command line it was booted with, and the interrupt counters. procfs
//! is the one caller today.

use ffi::fs;

/// The version, into `buf`; the bytes written, without a NUL.
pub fn version(buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    unsafe { fs::kernel_version_string(buf.as_mut_ptr(), buf.len()) }.min(buf.len())
}

/// The command line, into `buf`; the bytes written, without a NUL.
pub fn cmdline(buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    unsafe { fs::kernel_cmdline_string(buf.as_mut_ptr(), buf.len()) }.min(buf.len())
}

/// How many interrupt sources `interrupt_source` will answer for.
pub fn interrupt_source_count() -> usize {
    unsafe { fs::kernel_interrupt_source_count() }
}

/// What the index'th interrupt source is called and has counted, or None
/// past the end. The name goes into `name`, whose used length comes back
/// with it.
pub fn interrupt_source(index: usize, name: &mut [u8]) -> Option<(usize, i64)> {
    let count = unsafe {
        fs::kernel_interrupt_source(index, name.as_mut_ptr(), name.len())
    };
    if count < 0 {
        return None;
    }

    let len = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    Some((len, count as i64))
}
