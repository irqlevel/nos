//! The filesystem layer: the VFS every read and write in the kernel goes
//! through, the filesystems under it, and the few names the rest of the
//! kernel calls them by.
//!
//! [`vfs`] is the mount table, path resolution and the open files; under it
//! [`ext2`] (the root filesystem), [`nanofs`], [`ramfs`] and [`procfs`], each
//! a [`vfs::FileSystem`] seeing one call at a time under the VFS lock, and
//! each keeping what it holds in a [`vnode::Tree`]. [`rootfs`] is what the
//! kernel command line asks to be mounted at boot. C++ and the modules come
//! in through the C ABI at the bottom of this file and of [`files`], and
//! never see a vnode: an open file is a handle to them, and a handle is
//! looked up, not followed.

#![no_std]

extern crate alloc;

pub mod ext2;
pub mod files;
pub mod nanofs;
pub mod paths;
pub mod procfs;
pub mod rootfs;
pub mod ramfs;
pub mod selftest;
pub mod shell;
pub mod vfs;
pub mod vnode;

use core::ffi::c_void;

use kcore::once::OnceBox;
use kcore::trace;
use vfs::{Handle, Vfs};

static VFS: OnceBox<Vfs> = OnceBox::new();

/// Put the layer's commands in front of whoever runs one. Called from
/// `rust_init`, before the shell starts.
pub fn init() {
    shell::register_all();
}

/// The one VFS, made on first use. That is early in boot, in task context,
/// where its mutex can be allocated; two callers racing here both get the
/// same one.
pub(crate) fn vfs_instance() -> Option<&'static Vfs> {
    let vfs = VFS.get_or_try_init(Vfs::new);
    if vfs.is_none() {
        trace!(0, "vfs: no memory for the mount table");
    }
    vfs
}

/// # Safety
/// `path` points at `len` readable bytes.
pub(crate) unsafe fn path<'a>(path: *const u8, len: usize) -> Option<&'a [u8]> {
    if path.is_null() || len == 0 || len >= vfs::MAX_PATH {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(path, len) })
}

/* ---- mounts ---- */

/// Unmount what is at the path, releasing the filesystem: 0 done, -1 not.
///
/// # Safety
/// `path` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_unmount(path_ptr: *const u8, len: usize) -> i32 {
    match (vfs_instance(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) if vfs.unmount(at) => 0,
        _ => -1,
    }
}

#[no_mangle]
pub extern "C" fn kernel_vfs_unmount_all() {
    if let Some(vfs) = vfs_instance() {
        vfs.unmount_all();
    }
}

/* ---- files ----
 *
 * An open file crosses as a word -- a slot of the VFS's table and the
 * generation of what is in it -- and comes back as one. Whatever comes back
 * is looked up: a word that is no open file's reads as no file, so the calls
 * that take nothing else are not `unsafe`. */

fn handle_of(file: *mut c_void) -> Option<Handle> {
    Handle::from_raw(file as usize)
}

/// # Safety
/// `path` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_open(
    path_ptr: *const u8, len: usize, flags: usize,
) -> *mut c_void {
    let handle = match (vfs_instance(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) => vfs.open(at, flags),
        _ => None,
    };
    Handle::into_raw(handle) as *mut c_void
}

#[no_mangle]
pub extern "C" fn kernel_vfs_close(file: *mut c_void) {
    if let (Some(vfs), Some(handle)) = (vfs_instance(), handle_of(file)) {
        vfs.close(handle);
    }
}

/// 0 with `*out` set to what was read -- 0 at end of file -- or -1.
///
/// # Safety
/// `buf` takes `len` bytes; `out` is writable, or null.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_read(
    file: *mut c_void, buf: *mut u8, len: usize, out: *mut usize,
) -> i32 {
    let (vfs, handle) = match (vfs_instance(), handle_of(file)) {
        (Some(vfs), Some(handle)) if !buf.is_null() => (vfs, handle),
        _ => return -1,
    };

    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    match vfs.read(handle, buf) {
        Some(got) => {
            if let Some(out) = unsafe { out.as_mut() } {
                *out = got;
            }
            0
        }
        None => -1,
    }
}

/// # Safety
/// `data` holds `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_write(file: *mut c_void, data: *const u8, len: usize) -> i32 {
    let (vfs, handle) = match (vfs_instance(), handle_of(file)) {
        (Some(vfs), Some(handle)) if !data.is_null() || len == 0 => (vfs, handle),
        _ => return -1,
    };

    let data = if len == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(data, len) } };
    if vfs.write(handle, data) { 0 } else { -1 }
}

#[no_mangle]
pub extern "C" fn kernel_vfs_size(file: *mut c_void) -> usize {
    match (vfs_instance(), handle_of(file)) {
        (Some(vfs), Some(handle)) => vfs.size(handle),
        _ => 0,
    }
}

/* ---- paths ---- */

/// # Safety
/// `path` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_remove(path_ptr: *const u8, len: usize) -> i32 {
    match (vfs_instance(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) if vfs.remove(at) => 0,
        _ => -1,
    }
}

#[no_mangle]
pub extern "C" fn kernel_vfs_sync() -> i32 {
    match vfs_instance() {
        Some(vfs) if vfs.sync() => 0,
        _ => -1,
    }
}
