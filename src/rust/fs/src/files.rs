//! What is built out of the file API and nothing else: listing a directory,
//! printing a file, and the two-step replace a configuration file is written
//! with.
//!
//! These were the last of `fs/vfs.cpp`. They hold no state, and the module
//! ABI's `kernel_file_*` -- what `kcore::fs` gives a module -- is built on
//! them here rather than in C++, where it used to go out to the VFS view and
//! straight back into this crate.

use core::fmt::Write;

use kcore::cmd::Output;
use kcore::trace;

use crate::paths::Path;
use crate::vfs::{FileStat, MAX_PATH, OPEN_READ};
use crate::vfs::Vfs;
use crate::vnode::{TYPE_DIR, TYPE_FILE};
use crate::vfs_instance;

/// A file is streamed to a printer through a buffer of this size, so the
/// file itself never has to fit in one allocation.
const READ_CHUNK: usize = 4096;

/// A heap buffer taken fallibly and given back on the way out. `Vec` would
/// panic the kernel on a failed allocation rather than let the caller say so.
pub struct Buffer {
    ptr: *mut u8,
    len: usize,
}

impl Buffer {
    pub fn new(len: usize) -> Option<Self> {
        let layout = core::alloc::Layout::from_size_align(len, 8).ok()?;
        let ptr = unsafe { alloc::alloc::alloc(layout) };
        if ptr.is_null() { None } else { Some(Self { ptr, len }) }
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        let layout = core::alloc::Layout::from_size_align(self.len, 8).unwrap();
        unsafe { alloc::alloc::dealloc(self.ptr, layout) };
    }
}

/// Where `replace_file` puts a file's next content.
const SUFFIX: &str = ".new";

fn stat(vfs: &Vfs, path: &[u8]) -> Option<FileStat> {
    let mut st = FileStat { node_type: 0, size: 0, ino: 0 };
    if vfs.stat(path, &mut st) { Some(st) } else { None }
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

    if st.node_type != TYPE_DIR {
        let _ = writeln!(out, "not a directory");
        return false;
    }

    let mut index = 0;
    loop {
        let mut entry = crate::vfs::DirEntry {
            name: [0; crate::vnode::NAME_MAX], node_type: 0, size: 0,
        };
        if !vfs.read_dir(path.as_bytes(), index, &mut entry) {
            break;
        }
        index += 1;

        let len = entry.name.iter().position(|b| *b == 0).unwrap_or(entry.name.len());
        let name = core::str::from_utf8(&entry.name[..len]).unwrap_or("?");
        if entry.node_type == TYPE_FILE {
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

    let file = vfs.open(path.as_bytes(), OPEN_READ);
    if file.is_null() {
        let _ = writeln!(out, "file not found");
        return false;
    }

    /* From the allocator and not the stack, and fallibly: a task's stack has
     * no room for this, and `Vec` would panic the kernel rather than report
     * that there was no memory. */
    let mut buf = match Buffer::new(READ_CHUNK) {
        Some(buf) => buf,
        None => {
            trace!(0, "fs: read_file: no memory for a {} byte buffer", READ_CHUNK);
            let _ = writeln!(out, "read failed");
            vfs.close(file);
            return false;
        }
    };

    let mut ok = true;
    loop {
        match vfs.read(file, buf.as_mut_ptr(), READ_CHUNK) {
            Some(0) => break,
            Some(got) => out.write_bytes(&buf.as_slice()[..got]),
            None => {
                let _ = writeln!(out, "read failed");
                ok = false;
                break;
            }
        }
    }

    vfs.close(file);
    if ok {
        let _ = writeln!(out);
    }
    ok
}

pub fn dump_mounts(out: &mut Output) {
    let vfs = match vfs_instance() {
        Some(vfs) => vfs,
        None => return,
    };

    for index in 0..vfs.mount_count() {
        let mut path = [0u8; MAX_PATH];
        let mut info = [0u8; 64];
        let mut name: *const u8 = core::ptr::null();

        let read_only = vfs.mount_at(index, &mut path, &mut name, &mut info);
        if read_only < 0 {
            continue;
        }

        let rw = if read_only != 0 { "ro" } else { "rw" };
        let path = cstr(&path);
        let info = cstr(&info);
        let name = name_of(name);
        if info.is_empty() {
            let _ = writeln!(out, "{} on {}  {}", name, path, rw);
        } else {
            let _ = writeln!(out, "{} on {}  {}  {}", name, path, info, rw);
        }
    }
}

fn cstr(buf: &[u8]) -> &str {
    let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..len]).unwrap_or("?")
}

/// The filesystem's name, which it keeps for good.
fn name_of(name: *const u8) -> &'static str {
    if name.is_null() {
        return "?";
    }
    let text = unsafe { core::ffi::CStr::from_ptr(name as *const core::ffi::c_char) };
    text.to_str().unwrap_or("?")
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
        if st.node_type != TYPE_FILE {
            trace!(0, "fs: replace_file: {} is not a file", path);
            return false;
        }
    }

    /* The old content stays where it is until all of the new is on disk. */
    if !vfs.write_file(next.as_bytes(), data.as_ptr(), data.len()) || !vfs.sync() {
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
        Some(st) if st.node_type == TYPE_FILE => st.size as isize,
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
    let file = vfs.open(at.as_bytes(), OPEN_READ);
    if file.is_null() {
        return -1;
    }

    let mut total = 0;
    while total < cap {
        match vfs.read(file, unsafe { buf.add(total) }, cap - total) {
            Some(0) => break,
            Some(got) => total += got,
            None => {
                vfs.close(file);
                return -1;
            }
        }
    }
    vfs.close(file);
    total as isize
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

    let data = unsafe { core::slice::from_raw_parts(data, len) };
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
    if !vfs.write_file(path.as_bytes(), data, len) || !vfs.sync() {
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
        Some(st) if st.node_type == TYPE_DIR => 0,
        Some(_) => -1,
        None => if vfs.create(path.as_bytes(), true) { 0 } else { -1 },
    }
}

/// # Safety
/// `path` points at `len` readable bytes.
unsafe fn ffi_path<'a>(path: *const u8, len: usize) -> Option<&'a str> {
    let bytes = unsafe { crate::path(path, len)? };
    core::str::from_utf8(bytes).ok()
}
