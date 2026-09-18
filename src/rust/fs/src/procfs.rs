//! procfs: what the kernel says about itself, as files.
//!
//! `/proc/version` and `/proc/cmdline` are written once at mount;
//! `/proc/interrupts` is written again on every lookup, because the VFS
//! looks a file up before it reads it or reports its size, so refreshing
//! there keeps the two consistent with each other.
//!
//! The files themselves are a [`RamFs`] -- procfs is that, with three of
//! them in it and one that keeps changing.

use alloc::boxed::Box;

use kcore::procinfo;
use kcore::trace;

use crate::ramfs::RamFs;
use crate::vfs::FsOps;
use crate::vnode::{VNode, NAME_MAX};

/// What one rendering of /proc/interrupts fits in.
const INTERRUPTS_MAX: usize = 512;

pub struct ProcFs {
    ram: RamFs,
    interrupts: *mut VNode,
}

impl ProcFs {
    pub fn new() -> Option<Box<ProcFs>> {
        let ram = *RamFs::new()?;
        Some(Box::new(ProcFs { ram, interrupts: core::ptr::null_mut() }))
    }

    pub fn mount(&mut self) -> bool {
        let root = self.ram.root();

        let mut buf = [0u8; 128];
        let len = procinfo::version(&mut buf);
        self.put(root, b"version", &buf[..len]);

        let mut buf = [0u8; crate::vfs::MAX_PATH];
        let len = procinfo::cmdline(&mut buf);
        self.put(root, b"cmdline", &buf[..len]);

        self.interrupts = self.ram.create_file(root, b"interrupts");
        if self.interrupts.is_null() {
            trace!(0, "procfs: no memory for /proc/interrupts");
        } else {
            self.refresh_interrupts();
        }

        true
    }

    /// A file with this content, made once at mount.
    fn put(&self, root: *mut VNode, name: &[u8], content: &[u8]) {
        let node = self.ram.create_file(root, name);
        if node.is_null() {
            trace!(0, "procfs: no memory for a file");
            return;
        }
        self.ram.write(node, content, 0);
    }

    /// The interrupt counters as they are now. What does not fit is left
    /// out, as it was when this rendered into a fixed buffer in C++.
    fn refresh_interrupts(&self) {
        let mut out = [0u8; INTERRUPTS_MAX];
        let mut at = 0;

        for index in 0..procinfo::interrupt_source_count() {
            let mut name = [0u8; 32];
            let (len, count) = match procinfo::interrupt_source(index, &mut name) {
                Some(source) => source,
                None => break,
            };

            let written = line(&mut out[at..], &name[..len], count);
            if written == 0 {
                break;
            }
            at += written;
        }

        self.ram.truncate(self.interrupts, 0);
        self.ram.write(self.interrupts, &out[..at], 0);
    }

    pub fn lookup(&self, dir: *mut VNode, name: &[u8]) -> *mut VNode {
        let node = self.ram.lookup(dir, name);
        if !node.is_null() && node == self.interrupts {
            self.refresh_interrupts();
        }
        node
    }
}

/// One "<name padded to 12> <count right-aligned in 10>\n" into `out`; the
/// bytes written, or 0 when the line does not fit.
fn line(out: &mut [u8], name: &[u8], count: i64) -> usize {
    let mut digits = [0u8; 20];
    let mut number = count.unsigned_abs();
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (number % 10) as u8;
        number /= 10;
        if number == 0 {
            break;
        }
    }
    if count < 0 && at > 0 {
        at -= 1;
        digits[at] = b'-';
    }
    let digits = &digits[at..];

    let name_width = name.len().max(12);
    let count_width = digits.len().max(10);
    let total = name_width + 1 + count_width + 1;
    if total > out.len() {
        return 0;
    }

    out[..name.len()].copy_from_slice(name);
    for byte in out[name.len()..name_width].iter_mut() {
        *byte = b' ';
    }
    out[name_width] = b' ';

    let pad = count_width - digits.len();
    for byte in out[name_width + 1..name_width + 1 + pad].iter_mut() {
        *byte = b' ';
    }
    out[name_width + 1 + pad..total - 1].copy_from_slice(digits);
    out[total - 1] = b'\n';
    total
}

/* ---- the ops table the VFS drives it by ---- */

/// # Safety
/// `ctx` is the pointer a mount was made with, and the filesystem is alive.
unsafe fn fs<'a>(ctx: *mut u8) -> &'a mut ProcFs {
    unsafe { &mut *(ctx as *mut ProcFs) }
}

/// # Safety
/// `name` points at a NUL-terminated string.
unsafe fn cstr<'a>(name: *const u8) -> &'a [u8] {
    if name.is_null() {
        return &[];
    }
    let mut len = 0;
    while len < NAME_MAX && unsafe { *name.add(len) } != 0 {
        len += 1;
    }
    unsafe { core::slice::from_raw_parts(name, len) }
}

extern "C" fn op_root(ctx: *mut u8) -> *mut VNode {
    unsafe { fs(ctx) }.ram.root()
}

extern "C" fn op_load_dir(_ctx: *mut u8, _dir: *mut VNode) -> i32 {
    0
}

extern "C" fn op_lookup(ctx: *mut u8, dir: *mut VNode, name: *const u8) -> *mut VNode {
    unsafe { fs(ctx) }.lookup(dir, unsafe { cstr(name) })
}

/* Nothing is made, written, moved or removed in procfs from outside. */

extern "C" fn op_no_node(_ctx: *mut u8, _dir: *mut VNode, _name: *const u8) -> *mut VNode {
    core::ptr::null_mut()
}

extern "C" fn op_read(
    ctx: *mut u8, file: *mut VNode, buf: *mut u8, len: usize, off: usize,
) -> i32 {
    if len == 0 {
        return 0;
    }
    if buf.is_null() {
        return -1;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    if unsafe { fs(ctx) }.ram.read(file, buf, off) { 0 } else { -1 }
}

extern "C" fn op_write(
    _ctx: *mut u8, _file: *mut VNode, _data: *const u8, _len: usize, _off: usize,
) -> i32 {
    -1
}

extern "C" fn op_truncate(_ctx: *mut u8, _file: *mut VNode, _size: usize) -> i32 {
    -1
}

extern "C" fn op_rename(
    _ctx: *mut u8, _node: *mut VNode, _dir: *mut VNode, _name: *const u8,
) -> i32 {
    -1
}

extern "C" fn op_remove(_ctx: *mut u8, _node: *mut VNode) -> i32 {
    -1
}

extern "C" fn op_sync(_ctx: *mut u8) -> i32 {
    0
}

extern "C" fn op_device(_ctx: *mut u8) -> usize {
    0
}

extern "C" fn op_mount(ctx: *mut u8, _read_only: i32) -> i32 {
    let fs = unsafe { fs(ctx) };
    if !fs.mount() {
        return -1;
    }
    /* Read-only whatever the mount asked for: these files are the kernel's */
    1
}

extern "C" fn op_unmount(ctx: *mut u8) {
    unsafe { fs(ctx) }.ram.unmount();
}

extern "C" fn op_destroy(ctx: *mut u8) {
    drop(unsafe { Box::from_raw(ctx as *mut ProcFs) });
}

fn ops_for(fs: *mut ProcFs) -> FsOps {
    FsOps {
        name: b"procfs\0".as_ptr(),
        info: None,
        root: op_root,
        load_dir: op_load_dir,
        lookup: op_lookup,
        create_file: op_no_node,
        create_dir: op_no_node,
        read: op_read,
        write: op_write,
        truncate: op_truncate,
        rename: op_rename,
        remove: op_remove,
        sync: op_sync,
        device: op_device,
        mount: op_mount,
        unmount: op_unmount,
        destroy: Some(op_destroy),
        ctx: fs as *mut u8,
    }
}

/* ---- what the kernel calls ---- */

/// Mount procfs at `path`: 0 mounted, -1 not.
///
/// # Safety
/// `path` points at `path_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_procfs_mount(path: *const u8, path_len: usize) -> i32 {
    let (vfs, at) = match (crate::vfs_instance(), unsafe { crate::path(path, path_len) }) {
        (Some(vfs), Some(at)) => (vfs, at),
        _ => return -1,
    };

    let fs = match ProcFs::new() {
        Some(fs) => Box::into_raw(fs),
        None => return -1,
    };

    let ops = ops_for(fs);
    if !vfs.mount(at, &ops, true) {
        drop(unsafe { Box::from_raw(fs) });
        return -1;
    }
    0
}
