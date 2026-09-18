//! The filesystem layer: the VFS every read and write in the kernel goes
//! through, and the C ABI the rest of the kernel calls it by.
//!
//! The filesystems below it are moving over one at a time; while one is
//! still written in C++ it is wrapped in an [`FsOps`] by the shim in
//! fs/vfs.cpp, and the vnodes the two pass each other are the same struct on
//! both sides (see `vnode`).

#![no_std]

extern crate alloc;

pub mod vfs;
pub mod vnode;

use core::sync::atomic::{AtomicPtr, Ordering};

use kcore::trace;
use vfs::{DirEntry, File, FileStat, FsOps, Vfs};

static VFS: AtomicPtr<Vfs> = AtomicPtr::new(core::ptr::null_mut());

/// Nothing to set up: the layer is called from C++ by the names below, and
/// this is what keeps them in the archive.
pub fn init() {}

/// The one VFS, made on first use. That is early in boot, in task context,
/// where its mutex can be allocated; two callers racing here both get the
/// same one.
fn vfs() -> Option<&'static Vfs> {
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
unsafe fn path<'a>(path: *const u8, len: usize) -> Option<&'a [u8]> {
    if path.is_null() || len == 0 || len >= vfs::MAX_PATH {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(path, len) })
}

/* ---- mounts ---- */

/// # Safety
/// `path` points at `len` bytes; `ops` at a filled ops table whose context
/// outlives the mount.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_mount(
    path_ptr: *const u8, len: usize, ops: *const FsOps, read_only: i32,
) -> i32 {
    let (vfs, at) = match (vfs(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) => (vfs, at),
        _ => return -1,
    };
    if ops.is_null() {
        return -1;
    }

    let ops = unsafe { &*ops };
    if vfs.mount(at, ops, read_only != 0) {
        0
    } else {
        -1
    }
}

/// The filesystem's context, for the caller to release, or null.
///
/// # Safety
/// `path` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_unmount(path_ptr: *const u8, len: usize) -> *mut u8 {
    match (vfs(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) => vfs.unmount(at),
        _ => core::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn kernel_vfs_unmount_all() {
    if let Some(vfs) = vfs() {
        vfs.unmount_all();
    }
}

#[no_mangle]
pub extern "C" fn kernel_vfs_mount_count() -> usize {
    vfs().map_or(0, |vfs| vfs.mount_count())
}

/// What the index'th mount is: 1 read-only, 0 writable, -1 past the end.
///
/// # Safety
/// `path` and `info` point at `path_len` and `info_len` writable bytes;
/// `name` at a place for one pointer.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_mount_at(
    index: usize, path_out: *mut u8, path_len: usize,
    name_out: *mut *const u8, info_out: *mut u8, info_len: usize,
) -> i32 {
    let vfs = match vfs() {
        Some(vfs) => vfs,
        None => return -1,
    };
    if path_out.is_null() || name_out.is_null() || info_out.is_null() {
        return -1;
    }

    let path_buf = unsafe { core::slice::from_raw_parts_mut(path_out, path_len) };
    let info_buf = unsafe { core::slice::from_raw_parts_mut(info_out, info_len) };
    let mut name = core::ptr::null();
    let answer = vfs.mount_at(index, path_buf, &mut name, info_buf);
    unsafe { *name_out = name };
    answer
}

/* ---- files ---- */

/// # Safety
/// `path` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_open(
    path_ptr: *const u8, len: usize, flags: usize,
) -> *mut File {
    match (vfs(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) => vfs.open(at, flags),
        _ => core::ptr::null_mut(),
    }
}

/// # Safety
/// `file` came from `kernel_vfs_open` and is not used again.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_close(file: *mut File) {
    if let Some(vfs) = vfs() {
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
    let vfs = match vfs() {
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
    match vfs() {
        Some(vfs) if vfs.write(file, data, len) => 0,
        _ => -1,
    }
}

/// # Safety
/// `file` came from `kernel_vfs_open`.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_seek(file: *mut File, pos: usize) -> i32 {
    match vfs() {
        Some(vfs) if vfs.seek(file, pos) => 0,
        _ => -1,
    }
}

/// # Safety
/// `file` came from `kernel_vfs_open`.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_tell(file: *mut File) -> usize {
    vfs().map_or(0, |vfs| vfs.tell(file))
}

/// # Safety
/// `file` came from `kernel_vfs_open`.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_size(file: *mut File) -> usize {
    vfs().map_or(0, |vfs| vfs.size(file))
}

/* ---- paths ---- */

/// # Safety
/// `path` points at `len` bytes; `out` at a FileStat.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_stat(
    path_ptr: *const u8, len: usize, out: *mut FileStat,
) -> i32 {
    let (vfs, at) = match (vfs(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) => (vfs, at),
        _ => return -1,
    };
    if out.is_null() {
        return -1;
    }

    if vfs.stat(at, unsafe { &mut *out }) {
        0
    } else {
        -1
    }
}

/// # Safety
/// `path` points at `len` bytes; `out` at a DirEntry.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_readdir(
    path_ptr: *const u8, len: usize, index: usize, out: *mut DirEntry,
) -> i32 {
    let (vfs, at) = match (vfs(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) => (vfs, at),
        _ => return -1,
    };
    if out.is_null() {
        return -1;
    }

    if vfs.read_dir(at, index, unsafe { &mut *out }) {
        0
    } else {
        -1
    }
}

/// # Safety
/// `path` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_create(
    path_ptr: *const u8, len: usize, directory: i32,
) -> i32 {
    match (vfs(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) if vfs.create(at, directory != 0) => 0,
        _ => -1,
    }
}

/// # Safety
/// `path` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_remove(path_ptr: *const u8, len: usize) -> i32 {
    match (vfs(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) if vfs.remove(at) => 0,
        _ => -1,
    }
}

/// # Safety
/// `path` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_truncate(
    path_ptr: *const u8, len: usize, size: usize,
) -> i32 {
    match (vfs(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) if vfs.truncate(at, size) => 0,
        _ => -1,
    }
}

/// # Safety
/// Both paths point at their given lengths.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_rename(
    from_ptr: *const u8, from_len: usize, to_ptr: *const u8, to_len: usize,
) -> i32 {
    let vfs = match vfs() {
        Some(vfs) => vfs,
        None => return -1,
    };

    match (unsafe { path(from_ptr, from_len) }, unsafe { path(to_ptr, to_len) }) {
        (Some(from), Some(to)) if vfs.rename(from, to) => 0,
        _ => -1,
    }
}

#[no_mangle]
pub extern "C" fn kernel_vfs_sync() -> i32 {
    match vfs() {
        Some(vfs) if vfs.sync() => 0,
        _ => -1,
    }
}

/// Replace a file's contents, creating it if it is missing.
///
/// # Safety
/// `path` points at `len` bytes, `data` at `data_len`.
#[no_mangle]
pub unsafe extern "C" fn kernel_vfs_write_file(
    path_ptr: *const u8, len: usize, data: *const u8, data_len: usize,
) -> i32 {
    match (vfs(), unsafe { path(path_ptr, len) }) {
        (Some(vfs), Some(at)) if vfs.write_file(at, data, data_len) => 0,
        _ => -1,
    }
}
