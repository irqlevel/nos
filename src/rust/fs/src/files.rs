//! What is built out of the file API and nothing else: listing a directory,
//! printing a file, and the two-step replace a configuration file is written
//! with.
//!
//! These were the last of `fs/vfs.cpp`. They hold no state, and the module
//! ABI's `kernel_file_*` -- what `kcore::fs` gives a module -- is built on
//! them here rather than in C++, where it used to go out to the VFS view and
//! straight back into this crate.

use alloc::vec::Vec;
use core::fmt::Write;

use kcore::cmd::Output;
use kcore::trace;

use crate::paths::Path;
use crate::vfs::{FileStat, Open, Vfs, OPEN_READ, OPEN_WRITE};
use crate::vnode::Kind;
use crate::vfs_instance;

/// A file is streamed to a printer through a buffer of this size, so the
/// file itself never has to fit in one allocation.
const READ_CHUNK: usize = 4096;

/// `len` zeroed bytes from the heap, taken fallibly: a `vec![0; len]` would
/// panic the kernel on a failed allocation rather than let the caller say so.
pub fn buffer(len: usize) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).ok()?;
    buf.resize(len, 0);
    Some(buf)
}

/// Where `replace_file` puts a file's next content.
const SUFFIX: &str = ".new";

fn stat(vfs: &Vfs, path: &[u8]) -> Option<FileStat> {
    vfs.stat(path)
}

/* ---- what the shell shows ---- */

pub fn list_dir(path: &str, out: &mut Output) -> bool {
    let vfs = match vfs_instance() {
        Some(vfs) => vfs,
        None => return false,
    };

    let st = match stat(vfs, path.as_bytes()) {
        Some(st) => st,
        None => {
            let _ = writeln!(out, "path not found");
            return false;
        }
    };

    if st.kind != Kind::Dir {
        let _ = writeln!(out, "not a directory");
        return false;
    }

    let mut index = 0;
    while let Some(entry) = vfs.read_dir(path.as_bytes(), index) {
        index += 1;

        let name = core::str::from_utf8(entry.name()).unwrap_or("?");
        if entry.kind == Kind::File {
            let _ = writeln!(out, "f {} {}", entry.size, name);
        } else {
            let _ = writeln!(out, "d   {}", name);
        }
    }

    true
}

pub fn read_file(path: &str, out: &mut Output) -> bool {
    let vfs = match vfs_instance() {
        Some(vfs) => vfs,
        None => return false,
    };

    let file = match Open::new(vfs, path.as_bytes(), OPEN_READ) {
        Some(file) => file,
        None => {
            let _ = writeln!(out, "file not found");
            return false;
        }
    };

    /* From the allocator and not the stack, and fallibly: a task's stack has
     * no room for this, and a failed allocation is to be reported rather
     * than panicked on. */
    let mut buf = match buffer(READ_CHUNK) {
        Some(buf) => buf,
        None => {
            trace!(0, "fs: read_file: no memory for a {} byte buffer", READ_CHUNK);
            let _ = writeln!(out, "read failed");
            return false;
        }
    };

    loop {
        match file.read(&mut buf) {
            Some(0) => break,
            Some(got) => out.write_bytes(&buf[..got]),
            None => {
                let _ = writeln!(out, "read failed");
                return false;
            }
        }
    }

    let _ = writeln!(out);
    true
}

pub fn dump_mounts(out: &mut Output) {
    let vfs = match vfs_instance() {
        Some(vfs) => vfs,
        None => return,
    };

    for index in 0..vfs.mount_count() {
        let mount = match vfs.mount_info(index) {
            Some(mount) => mount,
            None => continue,
        };

        let rw = if mount.read_only { "ro" } else { "rw" };
        let path = core::str::from_utf8(mount.path()).unwrap_or("?");
        let info = core::str::from_utf8(mount.info()).unwrap_or("?");
        if info.is_empty() {
            let _ = writeln!(out, "{} on {}  {}", mount.fs_name, path, rw);
        } else {
            let _ = writeln!(out, "{} on {}  {}  {}", mount.fs_name, path, info, rw);
        }
    }
}

/* ---- the two-step replace ---- */

/// `<path>.new`, or None when that would not fit.
fn replacement(path: &str) -> Option<Path> {
    let mut next = Path::from(path)?;
    next.push(SUFFIX)?;
    Some(next)
}

/// Replace the file's content, never leaving it empty or half written -- the
/// disk filling, or the machine stopping, midway. The new content goes to
/// `<path>.new` and is synced, and only then takes the old file's place. Cut
/// short between those two steps, the content is whole in `<path>.new`, where
/// `locate` finds it.
pub fn replace_file(path: &str, data: &[u8]) -> bool {
    let (vfs, next) = match (vfs_instance(), replacement(path)) {
        (Some(vfs), Some(next)) => (vfs, next),
        _ => return false,
    };

    if let Some(st) = stat(vfs, path.as_bytes()) {
        if st.kind != Kind::File {
            trace!(0, "fs: replace_file: {} is not a file", path);
            return false;
        }
    }

    /* The old content stays where it is until all of the new is on disk. */
    if !vfs.write_file(next.as_bytes(), data) || !vfs.sync() {
        vfs.remove(next.as_bytes());
        return false;
    }
    if stat(vfs, path.as_bytes()).is_some() && !vfs.remove(path.as_bytes()) {
        return false;
    }
    if !vfs.rename(next.as_bytes(), path.as_bytes()) {
        return false;
    }
    vfs.sync()
}

/// Where a file's content is: at `path`, or at `<path>.new` when a
/// `replace_file` of it was cut short. None if at neither.
pub fn locate(path: &str) -> Option<Path> {
    let vfs = vfs_instance()?;
    if stat(vfs, path.as_bytes()).is_some() {
        return Path::from(path);
    }

    let next = replacement(path)?;
    if stat(vfs, next.as_bytes()).is_some() {
        Some(next)
    } else {
        None
    }
}

/* ---- what a module keeps its configuration in (kcore::fs) ---- */

/// The file's size, or -1: no such file, or not a file at all.
///
/// # Safety
/// `path` points at `path_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_file_size(path: *const u8, path_len: usize) -> isize {
    let (vfs, path) = match (vfs_instance(), unsafe { ffi_path(path, path_len) }) {
        (Some(vfs), Some(path)) => (vfs, path),
        _ => return -1,
    };

    let at = match locate(path) {
        Some(at) => at,
        None => return -1,
    };
    match stat(vfs, at.as_bytes()) {
        Some(st) if st.kind == Kind::File => st.size as isize,
        _ => -1,
    }
}

/// Up to `cap` bytes from the start of the file: the count read, or -1.
///
/// # Safety
/// `path` points at `path_len` bytes; `buf` takes `cap`.
#[no_mangle]
pub unsafe extern "C" fn kernel_file_read(
    path: *const u8, path_len: usize, buf: *mut u8, cap: usize,
) -> isize {
    let (vfs, path) = match (vfs_instance(), unsafe { ffi_path(path, path_len) }) {
        (Some(vfs), Some(path)) => (vfs, path),
        _ => return -1,
    };
    if buf.is_null() && cap != 0 {
        return -1;
    }

    let at = match locate(path) {
        Some(at) => at,
        None => return -1,
    };
    let file = match Open::new(vfs, at.as_bytes(), OPEN_READ) {
        Some(file) => file,
        None => return -1,
    };
    if cap == 0 {
        return 0;
    }

    let buf = unsafe { core::slice::from_raw_parts_mut(buf, cap) };
    let mut total = 0;
    while total < cap {
        match file.read(&mut buf[total..]) {
            Some(0) => break,
            Some(got) => total += got,
            None => return -1,
        }
    }
    total as isize
}

/// Up to `cap` bytes of the file from `offset`: the count read, 0 at or past
/// its end, or -1. For a file too large for one buffer -- a guest's kernel --
/// read a piece at a time; the file is opened for each call, so nothing is
/// held open between them.
///
/// # Safety
/// `path` points at `path_len` bytes; `buf` takes `cap`.
#[no_mangle]
pub unsafe extern "C" fn kernel_file_read_at(
    path: *const u8, path_len: usize, offset: u64, buf: *mut u8, cap: usize,
) -> isize {
    let (vfs, path) = match (vfs_instance(), unsafe { ffi_path(path, path_len) }) {
        (Some(vfs), Some(path)) => (vfs, path),
        _ => return -1,
    };
    if buf.is_null() && cap != 0 {
        return -1;
    }

    let at = match locate(path) {
        Some(at) => at,
        None => return -1,
    };
    let file = match Open::new(vfs, at.as_bytes(), OPEN_READ) {
        Some(file) => file,
        None => return -1,
    };
    let offset = match usize::try_from(offset) {
        Ok(offset) => offset,
        Err(_) => return 0,
    };
    if cap == 0 || offset >= file.size() {
        return 0;
    }
    if !file.seek(offset) {
        return -1;
    }

    let buf = unsafe { core::slice::from_raw_parts_mut(buf, cap) };
    let mut total = 0;
    while total < cap {
        match file.read(&mut buf[total..]) {
            Some(0) => break,
            Some(got) => total += got,
            None => return -1,
        }
    }
    total as isize
}

/// Writes `len` bytes into the file at `offset`, within the size it has -- a
/// guest's disk image, whose size is its disk's: `len`, or -1 when the
/// write would reach past the end (refused whole, the file never grown) or
/// the filesystem refuses. Nothing is synced here: `kernel_file_sync` is the
/// flush. The file is opened for each call, as `kernel_file_read_at` opens
/// it.
///
/// # Safety
/// `path` points at `path_len` bytes; `data` at `len`.
#[no_mangle]
pub unsafe extern "C" fn kernel_file_write_at(
    path: *const u8, path_len: usize, offset: u64, data: *const u8, len: usize,
) -> isize {
    let (vfs, path) = match (vfs_instance(), unsafe { ffi_path(path, path_len) }) {
        (Some(vfs), Some(path)) => (vfs, path),
        _ => return -1,
    };
    if data.is_null() && len != 0 {
        return -1;
    }
    let Ok(len_signed) = isize::try_from(len) else {
        return -1;
    };

    let at = match locate(path) {
        Some(at) => at,
        None => return -1,
    };
    let file = match Open::new(vfs, at.as_bytes(), OPEN_WRITE) {
        Some(file) => file,
        None => return -1,
    };
    let end = match usize::try_from(offset).ok().and_then(|o| o.checked_add(len)) {
        Some(end) => end,
        None => return -1,
    };
    if end > file.size() {
        return -1;
    }
    if len == 0 {
        return 0;
    }
    if !file.seek(end - len) || !file.write(unsafe { bytes(data, len) }) {
        return -1;
    }
    len_signed
}

/// Everything written to every filesystem so far, on its disk: what a
/// guest's flush asks of the image under it. 0, or -1 when a filesystem
/// could not.
#[no_mangle]
pub extern "C" fn kernel_file_sync() -> i32 {
    match vfs_instance() {
        Some(vfs) if vfs.sync() => 0,
        _ => -1,
    }
}

/// Replaces the file's content, making the file if it is missing, through
/// `replace_file`: what a module writes is its configuration -- the keys
/// allowed to log in -- which a full disk or a crash midway must not leave
/// empty.
///
/// # Safety
/// `path` points at `path_len` bytes; `data` at `len`.
#[no_mangle]
pub unsafe extern "C" fn kernel_file_write(
    path: *const u8, path_len: usize, data: *const u8, len: usize,
) -> i32 {
    let path = match unsafe { ffi_path(path, path_len) } {
        Some(path) => path,
        None => return -1,
    };
    if data.is_null() && len != 0 {
        return -1;
    }

    let data = unsafe { bytes(data, len) };
    if replace_file(path, data) { 0 } else { -1 }
}

/// A new file with this content: 1, and nothing written, when there is one at
/// the path already -- or the remains of a `replace_file` of it. For a file
/// that must never be written over by mistake, a host key: a stat that failed
/// on a bad block must not read as "there is none" and lose it.
///
/// # Safety
/// `path` points at `path_len` bytes; `data` at `len`.
#[no_mangle]
pub unsafe extern "C" fn kernel_file_create(
    path: *const u8, path_len: usize, data: *const u8, len: usize,
) -> i32 {
    let (vfs, path) = match (vfs_instance(), unsafe { ffi_path(path, path_len) }) {
        (Some(vfs), Some(path)) => (vfs, path),
        _ => return -1,
    };
    if data.is_null() && len != 0 {
        return -1;
    }

    if locate(path).is_some() {
        return 1;
    }
    /* Refused by the filesystem itself when the file is there after all. */
    if !vfs.create(path.as_bytes(), false) {
        return 1;
    }
    if !vfs.write_file(path.as_bytes(), unsafe { bytes(data, len) }) || !vfs.sync() {
        return -1;
    }
    0
}

/// The file wherever it is -- at the path, and at `<path>.new` should a
/// `replace_file` of it have been cut short leaving both. 0 once neither is
/// there, -1 when one could not be removed. The pair is this layer's idea,
/// so taking a file away whole is its job and not the caller's.
///
/// # Safety
/// `path` points at `path_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_file_remove(path: *const u8, path_len: usize) -> i32 {
    let (vfs, path) = match (vfs_instance(), unsafe { ffi_path(path, path_len) }) {
        (Some(vfs), Some(path)) => (vfs, path),
        _ => return -1,
    };

    let mut removed_any = false;
    while let Some(at) = locate(path) {
        if !vfs.remove(at.as_bytes()) {
            return -1;
        }
        removed_any = true;
    }
    if removed_any { 0 } else { -1 }
}

/// A directory, made if there is none by that name: 0 once there is one.
///
/// # Safety
/// `path` points at `path_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_dir_create(path: *const u8, path_len: usize) -> i32 {
    let (vfs, path) = match (vfs_instance(), unsafe { ffi_path(path, path_len) }) {
        (Some(vfs), Some(path)) => (vfs, path),
        _ => return -1,
    };

    match stat(vfs, path.as_bytes()) {
        Some(st) if st.kind == Kind::Dir => 0,
        Some(_) => -1,
        None => if vfs.create(path.as_bytes(), true) { 0 } else { -1 },
    }
}

/// What a caller outside Rust wants written: nothing, for a length of 0,
/// whatever the pointer.
///
/// # Safety
/// `data` points at `len` readable bytes, unless `len` is 0.
unsafe fn bytes<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(data, len) }
    }
}

/// # Safety
/// `path` points at `len` readable bytes.
unsafe fn ffi_path<'a>(path: *const u8, len: usize) -> Option<&'a str> {
    let bytes = unsafe { crate::path(path, len)? };
    core::str::from_utf8(bytes).ok()
}
