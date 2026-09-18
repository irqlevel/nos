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

/* ---- what the root filesystem is to be ---- */

/// What `root=` asked for.
pub enum Root {
    /// `root=` was not given, or said nothing: mount nothing.
    None,
    /// `root=auto`: the ext2 labelled `nos`.
    Auto,
    /// `root=<device>`: that block device, by the name `disks` shows.
    Device,
    /// `root=LABEL=<label>`: the ext2 with that volume label.
    Label,
    /// `root=UUID=<uuid>`: the ext2 with that UUID.
    Uuid,
}

/// The root spec: what to look for, the name or label it was given, and the
/// UUID it was given. The name's length comes back with it.
pub fn root_spec(value: &mut [u8], uuid: &mut [u8; 16]) -> (Root, usize) {
    let mode = unsafe {
        fs::kernel_root_spec(value.as_mut_ptr(), value.len(), uuid.as_mut_ptr(), uuid.len())
    };
    let len = value.iter().position(|b| *b == 0).unwrap_or(value.len());

    let mode = match mode {
        1 => Root::Auto,
        2 => Root::Device,
        3 => Root::Label,
        4 => Root::Uuid,
        _ => Root::None,
    };
    (mode, len)
}

/// `ro`: the root is to be mounted read-only.
pub fn root_read_only() -> bool {
    unsafe { fs::kernel_root_read_only() != 0 }
}

/// `fstest=on`: run the filesystem self-test on / once it is mounted.
pub fn root_fstest() -> bool {
    unsafe { fs::kernel_root_fstest() != 0 }
}

/// The filesystem self-test in `dir`, with a file of `size` bytes.
pub fn fs_selftest(dir: &str, size: usize) -> bool {
    unsafe { fs::kernel_fs_selftest(dir.as_ptr(), dir.len(), size) == 0 }
}
