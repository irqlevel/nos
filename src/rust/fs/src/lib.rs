//! The filesystem layer: the VFS every read and write in the kernel goes
//! through, the filesystems under it, and the C ABI the rest of the kernel
//! calls them by.
//!
//! [`vfs`] is the mount table, path resolution and the open handles; under
//! it [`ext2`] (the root filesystem), [`nanofs`], [`ramfs`] and [`procfs`],
//! each giving the VFS an [`FsOps`] and seeing one call at a time under its
//! lock. [`rootfs`] is what the kernel command line asks to be mounted at
//! boot. C++ calls in through fs/vfs.cpp and never sees a vnode.

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

use core::sync::atomic::{AtomicPtr, Ordering};

use kcore::trace;
use vfs::{File, Vfs};

static VFS: AtomicPtr<Vfs> = AtomicPtr::new(core::ptr::null_mut());

/// Put the layer's commands in front of whoever runs one. Called from
/// `rust_init`, before the shell starts.
pub fn init() {
    shell::register_all();
}

/// The one VFS, made on first use. That is early in boot, in task context,
/// where its mutex can be allocated; two callers racing here both get the
/// same one.
pub(crate) fn vfs_instance() -> Option<&'static Vfs> {
    let existing = VFS.load(Ordering::Acquire);
    if !existing.is_null() {
        return Some(unsafe { &*existing });
    }

    let made = match Vfs::new() {
        Some(vfs) => alloc::boxed::Box::into_raw(vfs),
        None => {
            trace!(0, "vfs: no memory for the mount table");
            return None;
        }
    };

    match VFS.compare_exchange(
        core::ptr::null_mut(), made, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => Some(unsafe { &*made }),
        Err(winner) => {
            /* Someone else got there first; theirs is the one. */
            unsafe { drop(alloc::boxed::Box::from_raw(made)) };
            Some(unsafe { &*winner })
        }
    }
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

/* ---- files ---- */

/// # Safety
/// `path` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_open(
    path_ptr: *const u8, len: usize, flags: usize,
) -> *mut File {
    match (vfs_instance(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) => vfs.open(at, flags),
        _ => core::ptr::null_mut(),
    }
}

/// # Safety
/// `file` came from `kernel_vfs_open` and is not used again.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_close(file: *mut File) {
    if let Some(vfs) = vfs_instance() {
        vfs.close(file);
    }
}

/// 0 with `*out` set to what was read -- 0 at end of file -- or -1.
///
/// # Safety
/// `buf` takes `len` bytes; `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_read(
    file: *mut File, buf: *mut u8, len: usize, out: *mut usize,
) -> i32 {
    let vfs = match vfs_instance() {
        Some(vfs) => vfs,
        None => return -1,
    };

    match vfs.read(file, buf, len) {
        Some(got) => {
            if !out.is_null() {
                unsafe { *out = got };
            }
            0
        }
        None => -1,
    }
}

/// # Safety
/// `data` holds `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_write(file: *mut File, data: *const u8, len: usize) -> i32 {
    match vfs_instance() {
        Some(vfs) if vfs.write(file, data, len) => 0,
        _ => -1,
    }
}

/// # Safety
/// `file` came from `kernel_vfs_open`.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_size(file: *mut File) -> usize {
    vfs_instance().map_or(0, |vfs| vfs.size(file))
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

