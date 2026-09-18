//! The VFS: the mount table, path resolution, and the file API everything
//! else in the kernel reads and writes through.
//!
//! One call at a time, under one mutex -- which is what lets a filesystem
//! below have no locking of its own, and is the contract `FileSystem`
//! (fs/filesystem.h) is written to. The composed calls (`replace_file`,
//! `locate`) take no lock themselves: they are made of the calls that do.

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::ffi::c_int;

use kcore::sync::Mutex;
use kcore::trace;

use crate::vnode::{self, VNode, NAME_MAX};

pub const MAX_MOUNTS: usize = 16;
pub const MAX_PATH: usize = 256;

/* Open flags, as Vfs::Open takes them (fs/vfs.h) */
pub const OPEN_READ: usize = 1;
pub const OPEN_WRITE: usize = 2;
pub const OPEN_CREATE: usize = 4;
pub const OPEN_TRUNCATE: usize = 8;
pub const OPEN_APPEND: usize = 16;

/// What a filesystem gives the VFS: the calls it answers, and the context
/// they are about. A filesystem written in C++ is wrapped in one of these by
/// the shim in fs/vfs.cpp; one written in Rust fills it directly.
///
/// Every call arrives with the VFS lock held, so an implementation needs no
/// locking of its own: two calls never overlap on the same filesystem.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FsOps {
    /// NUL-terminated, what `mounts` shows the filesystem as
    pub name: *const u8,
    /// A line about the filesystem for `mounts`, into the buffer given
    pub info: Option<extern "C" fn(ctx: *mut u8, buf: *mut u8, len: usize)>,

    pub root: extern "C" fn(ctx: *mut u8) -> *mut VNode,
    /// Make a directory's children complete; 0 on success
    pub load_dir: extern "C" fn(ctx: *mut u8, dir: *mut VNode) -> i32,
    pub lookup: extern "C" fn(ctx: *mut u8, dir: *mut VNode, name: *const u8) -> *mut VNode,
    pub create_file: extern "C" fn(ctx: *mut u8, dir: *mut VNode, name: *const u8) -> *mut VNode,
    pub create_dir: extern "C" fn(ctx: *mut u8, dir: *mut VNode, name: *const u8) -> *mut VNode,

    pub read: extern "C" fn(ctx: *mut u8, file: *mut VNode, buf: *mut u8, len: usize, off: usize) -> i32,
    pub write: extern "C" fn(ctx: *mut u8, file: *mut VNode, data: *const u8, len: usize, off: usize) -> i32,
    pub truncate: extern "C" fn(ctx: *mut u8, file: *mut VNode, size: usize) -> i32,
    pub rename: extern "C" fn(ctx: *mut u8, node: *mut VNode, dir: *mut VNode, name: *const u8) -> i32,
    pub remove: extern "C" fn(ctx: *mut u8, node: *mut VNode) -> i32,
    pub sync: extern "C" fn(ctx: *mut u8) -> i32,

    /// The block device it is on, as a handle, or 0
    pub device: extern "C" fn(ctx: *mut u8) -> usize,
    /// Take the filesystem: answers 1 if it may only be read, 0 if it may be
    /// written, and -1 if it cannot be mounted at all.
    pub mount: extern "C" fn(ctx: *mut u8, read_only: i32) -> i32,
    pub unmount: extern "C" fn(ctx: *mut u8),
    /// Release the filesystem itself. Only the shutdown path calls it.
    pub destroy: Option<extern "C" fn(ctx: *mut u8)>,

    pub ctx: *mut u8,
}

/// What `stat` answers (FileStat in fs/vfs.h).
#[repr(C)]
pub struct FileStat {
    pub node_type: c_int,
    pub size: usize,
    pub ino: usize,
}

/// One entry of a directory (DirEntry in fs/vfs.h).
#[repr(C)]
pub struct DirEntry {
    pub name: [u8; NAME_MAX],
    pub node_type: c_int,
    pub size: usize,
}

struct Mount {
    path: [u8; MAX_PATH],
    path_len: usize,
    ops: FsOps,
    read_only: bool,
    /// On the filesystem's device, for as long as it is mounted
    claim: usize,
    /// Handles open on it, which is what refuses an unmount
    open_files: usize,
    /// Tells this mount from the one that takes its place
    id: u64,
}

/// An open file: a position over a vnode, and the mount it belongs to.
pub struct File {
    mount_id: u64,
    ops: FsOps,
    node: *mut VNode,
    pos: usize,
    flags: usize,
}

struct Inner {
    mounts: [Option<Mount>; MAX_MOUNTS],
    count: usize,
    next_id: u64,
}

pub struct Vfs {
    lock: Mutex,
    inner: UnsafeCell<Inner>,
}

/* Everything inside is touched with the lock held. */
unsafe impl Sync for Vfs {}
unsafe impl Send for Vfs {}

/// What a mount's claim on its device says to whoever is refused it.
const MOUNT_HOLDER: &[u8] = b"a mounted filesystem\0";

impl Vfs {
    pub fn new() -> Option<Box<Vfs>> {
        let lock = Mutex::new()?;
        Some(Box::new(Vfs {
            lock,
            inner: UnsafeCell::new(Inner {
                mounts: [const { None }; MAX_MOUNTS],
                count: 0,
                next_id: 1,
            }),
        }))
    }

    /* ---- mounts ---- */

    pub fn mount(&self, path: &[u8], ops: &FsOps, read_only: bool) -> bool {
        if path.is_empty() || path[0] != b'/' || path.len() >= MAX_PATH {
            trace!(0, "vfs: a mount path must start with / and fit {} bytes", MAX_PATH);
            return false;
        }

        let _guard = self.lock.lock();
        let inner = unsafe { &mut *self.inner.get() };

        let device = (ops.device)(ops.ctx);

        for mount in inner.mounts.iter().flatten() {
            if mount.path() == path {
                trace!(0, "vfs: something is mounted on that path already");
                return false;
            }
            if device != 0 && (mount.ops.device)(mount.ops.ctx) == device {
                trace!(0, "vfs: that device is mounted already");
                return false;
            }
        }

        if inner.count >= MAX_MOUNTS {
            trace!(0, "vfs: {} mounts is all there is room for", MAX_MOUNTS);
            return false;
        }

        /* The device is the filesystem's while it is mounted: nothing may
         * write to it around the filesystem -- the disk log, a module going
         * direct -- nor another mount take it, or a disk or partition
         * overlapping it. */
        let mut claim = 0;
        if device != 0 {
            claim = kcore::block::claim_as(device, MOUNT_HOLDER.as_ptr());
            if claim == 0 {
                trace!(0, "vfs: the device is in use by someone else");
                return false;
            }
        }

        /* The filesystem may find an image it can read but must not write. */
        let answer = (ops.mount)(ops.ctx, read_only as i32);
        if answer < 0 {
            kcore::block::release(claim);
            return false;
        }
        let read_only = read_only || answer == 1;

        let mut entry = Mount {
            path: [0; MAX_PATH],
            path_len: path.len(),
            ops: *ops,
            read_only,
            claim,
            open_files: 0,
            id: inner.next_id,
        };
        entry.path[..path.len()].copy_from_slice(path);
        inner.next_id += 1;

        let slot = inner.count;
        inner.mounts[slot] = Some(entry);
        inner.count += 1;
        true
    }

    /// Take a filesystem off its mount point and answer with its context,
    /// which is the caller's to release. 0 if it is not mounted or is busy.
    pub fn unmount(&self, path: &[u8]) -> *mut u8 {
        let _guard = self.lock.lock();
        let inner = unsafe { &mut *self.inner.get() };

        for index in 0..inner.count {
            let matches = match &inner.mounts[index] {
                Some(mount) => mount.path() == path,
                None => false,
            };
            if !matches {
                continue;
            }

            let mount = inner.mounts[index].as_ref().unwrap();
            if mount.open_files != 0 {
                trace!(0, "vfs: the mount is busy, {} files open", mount.open_files);
                return core::ptr::null_mut();
            }

            let ops = mount.ops;
            let claim = mount.claim;
            (ops.unmount)(ops.ctx);
            kcore::block::release(claim);

            remove_mount(inner, index);
            return ops.ctx;
        }

        trace!(0, "vfs: nothing is mounted there");
        core::ptr::null_mut()
    }

    /// Take everything down, deepest mount first, releasing each filesystem.
    /// This is shutdown: a handle left open is abandoned, not honoured.
    pub fn unmount_all(&self) {
        let _guard = self.lock.lock();
        let inner = unsafe { &mut *self.inner.get() };

        while inner.count > 0 {
            /* Deepest first, so a mount inside another goes before it. */
            let mut deepest = 0;
            let mut longest = 0;
            for index in 0..inner.count {
                if let Some(mount) = &inner.mounts[index] {
                    if mount.path_len >= longest {
                        longest = mount.path_len;
                        deepest = index;
                    }
                }
            }

            let mount = inner.mounts[deepest].as_ref().unwrap();
            let ops = mount.ops;
            let claim = mount.claim;
            trace!(0, "vfs: unmounting {}",
                core::str::from_utf8(mount.path()).unwrap_or("?"));
            if mount.open_files != 0 {
                trace!(0, "vfs: unmounting with {} files still open", mount.open_files);
            }

            (ops.unmount)(ops.ctx);
            kcore::block::release(claim);
            if let Some(destroy) = ops.destroy {
                destroy(ops.ctx);
            }

            remove_mount(inner, deepest);
        }
    }

    pub fn mount_count(&self) -> usize {
        let _guard = self.lock.lock();
        unsafe { &*self.inner.get() }.count
    }

    /// What the index'th mount is, for `mounts` to print: its path, the
    /// filesystem's name and its line about itself, and whether it is
    /// read-only.
    pub fn mount_at(
        &self, index: usize, path: &mut [u8], name: &mut *const u8, info: &mut [u8],
    ) -> i32 {
        let _guard = self.lock.lock();
        let inner = unsafe { &*self.inner.get() };

        let mount = match inner.mounts.get(index).and_then(|m| m.as_ref()) {
            Some(mount) => mount,
            None => return -1,
        };

        let bytes = mount.path();
        if bytes.len() >= path.len() {
            return -1;
        }
        path[..bytes.len()].copy_from_slice(bytes);
        path[bytes.len()] = 0;

        *name = mount.ops.name;
        info[0] = 0;
        if let Some(get_info) = mount.ops.info {
            get_info(mount.ops.ctx, info.as_mut_ptr(), info.len());
        }

        mount.read_only as i32
    }

    /* ---- resolution ---- */

    /// The mount a path is on: the longest mount path it starts with, and
    /// what is left of it after that.
    fn find_mount<'a>(&self, inner: &'a Inner, path: &'a [u8]) -> Option<(usize, &'a [u8])> {
        let mut best: Option<(usize, usize)> = None;

        for index in 0..inner.count {
            let mount = match &inner.mounts[index] {
                Some(mount) => mount,
                None => continue,
            };

            let mpath = mount.path();
            if mpath.is_empty() || path.len() < mpath.len() || &path[..mpath.len()] != mpath {
                continue;
            }

            /* An exact match, or a '/' after it -- except the root mount,
             * which every absolute path is under. */
            let next = path.get(mpath.len()).copied();
            if next.is_some() && next != Some(b'/') && mpath != b"/" {
                continue;
            }

            if best.map_or(true, |(_, len)| mpath.len() > len) {
                best = Some((index, mpath.len()));
            }
        }

        let (index, len) = best?;
        let mut rest = &path[len..];
        if rest.first() == Some(&b'/') {
            rest = &rest[1..];
        }
        Some((index, rest))
    }

    /// Walk a path to what it names. `node` is null when the last component
    /// does not exist, and `parent` and `last` then say where it would go.
    fn resolve(&self, inner: &Inner, path: &[u8]) -> Option<Resolved> {
        let (index, rest) = match self.find_mount(inner, path) {
            Some(found) => found,
            None => {
                trace!(0, "vfs: no mount holds that path");
                return None;
            }
        };

        let ops = inner.mounts[index].as_ref().unwrap().ops;
        let mut at = (ops.root)(ops.ctx);
        if at.is_null() {
            return None;
        }

        let mut resolved = Resolved {
            mount: index,
            ops,
            node: core::ptr::null_mut(),
            parent: core::ptr::null_mut(),
            last: [0; NAME_MAX],
            last_len: 0,
        };

        if rest.is_empty() {
            resolved.node = at;
            return Some(resolved);
        }

        let mut components = rest.split(|b| *b == b'/').filter(|c| !c.is_empty()).peekable();
        while let Some(component) = components.next() {
            if component.len() >= NAME_MAX {
                trace!(0, "vfs: a name in that path is longer than {} bytes", NAME_MAX - 1);
                return None;
            }

            let last = components.peek().is_none();

            /* "." and ".." are not children of anything; they are answered
             * here, and ".." at a mount root stays put. */
            if component == b"." {
                if last {
                    resolved.node = at;
                    return Some(resolved);
                }
                continue;
            }
            if component == b".." {
                let parent = unsafe { (*at).parent };
                if !parent.is_null() {
                    at = parent;
                }
                if last {
                    resolved.node = at;
                    return Some(resolved);
                }
                continue;
            }

            let mut name = [0u8; NAME_MAX];
            name[..component.len()].copy_from_slice(component);

            let child = (ops.lookup)(ops.ctx, at, name.as_ptr());

            if last {
                resolved.parent = at;
                resolved.last = name;
                resolved.last_len = component.len();
                resolved.node = child;
                return Some(resolved);
            }

            if child.is_null() || !unsafe { (*child).is_dir() } {
                trace!(0, "vfs: a component of that path is not a directory");
                return None;
            }
            at = child;
        }

        resolved.node = at;
        Some(resolved)
    }
}

struct Resolved {
    mount: usize,
    ops: FsOps,
    node: *mut VNode,
    parent: *mut VNode,
    last: [u8; NAME_MAX],
    last_len: usize,
}

impl Mount {
    fn path(&self) -> &[u8] {
        &self.path[..self.path_len]
    }
}

fn remove_mount(inner: &mut Inner, index: usize) {
    for at in index..inner.count - 1 {
        inner.mounts[at] = inner.mounts[at + 1].take();
    }
    inner.mounts[inner.count - 1] = None;
    inner.count -= 1;
}

/* ---- the file API ---- */

impl Vfs {
    pub fn stat(&self, path: &[u8], out: &mut FileStat) -> bool {
        let _guard = self.lock.lock();
        let inner = unsafe { &*self.inner.get() };

        let resolved = match self.resolve(inner, path) {
            Some(resolved) if !resolved.node.is_null() => resolved,
            _ => return false,
        };

        let node = unsafe { &*resolved.node };
        out.node_type = node.node_type;
        out.size = if node.is_file() { node.size } else { 0 };
        out.ino = node.ino;
        true
    }

    pub fn read_dir(&self, path: &[u8], index: usize, out: &mut DirEntry) -> bool {
        let _guard = self.lock.lock();
        let inner = unsafe { &*self.inner.get() };

        let resolved = match self.resolve(inner, path) {
            Some(resolved) if !resolved.node.is_null() => resolved,
            _ => return false,
        };

        if !unsafe { (*resolved.node).is_dir() } {
            return false;
        }
        if (resolved.ops.load_dir)(resolved.ops.ctx, resolved.node) != 0 {
            return false;
        }

        for (at, child) in unsafe { vnode::children(resolved.node) }.enumerate() {
            if at != index {
                continue;
            }

            let child = unsafe { &*child };
            let name = child.name();
            out.name = [0; NAME_MAX];
            out.name[..name.len()].copy_from_slice(name);
            out.node_type = child.node_type;
            out.size = if child.is_file() { child.size } else { 0 };
            return true;
        }

        false
    }

    pub fn open(&self, path: &[u8], flags: usize) -> *mut File {
        let mut flags = flags;
        if flags & OPEN_APPEND != 0 {
            flags |= OPEN_WRITE;
        }
        if flags & (OPEN_READ | OPEN_WRITE) == 0 {
            trace!(0, "vfs: an open for neither reading nor writing");
            return core::ptr::null_mut();
        }

        let _guard = self.lock.lock();
        let inner = unsafe { &mut *self.inner.get() };

        let writes = flags & (OPEN_WRITE | OPEN_CREATE | OPEN_TRUNCATE) != 0;

        let resolved = match self.resolve(inner, path) {
            Some(resolved) => resolved,
            None => return core::ptr::null_mut(),
        };

        if writes && inner.mounts[resolved.mount].as_ref().unwrap().read_only {
            trace!(0, "vfs: that mount is read-only");
            return core::ptr::null_mut();
        }

        let mut node = resolved.node;
        if node.is_null() {
            if flags & OPEN_CREATE == 0 {
                return core::ptr::null_mut();
            }
            if resolved.parent.is_null() || resolved.last_len == 0 {
                return core::ptr::null_mut();
            }

            node = (resolved.ops.create_file)(
                resolved.ops.ctx, resolved.parent, resolved.last.as_ptr());
            if node.is_null() {
                trace!(0, "vfs: the file could not be created");
                return core::ptr::null_mut();
            }
        }

        if !unsafe { (*node).is_file() } {
            trace!(0, "vfs: that path is not a file");
            return core::ptr::null_mut();
        }

        if flags & OPEN_TRUNCATE != 0 && unsafe { (*node).size } != 0 {
            if (resolved.ops.truncate)(resolved.ops.ctx, node, 0) != 0 {
                return core::ptr::null_mut();
            }
        }

        let pos = if flags & OPEN_APPEND != 0 { unsafe { (*node).size } } else { 0 };
        let mount = inner.mounts[resolved.mount].as_mut().unwrap();
        let file = Box::new(File {
            mount_id: mount.id,
            ops: resolved.ops,
            node,
            pos,
            flags,
        });

        unsafe { (*node).open_count += 1 };
        mount.open_files += 1;
        Box::into_raw(file)
    }

    pub fn close(&self, file: *mut File) {
        if file.is_null() {
            return;
        }

        let _guard = self.lock.lock();
        let inner = unsafe { &mut *self.inner.get() };

        let file = unsafe { Box::from_raw(file) };
        unsafe { (*file.node).open_count -= 1 };

        for mount in inner.mounts.iter_mut().flatten() {
            if mount.id == file.mount_id && mount.open_files != 0 {
                mount.open_files -= 1;
                break;
            }
        }
    }

    pub fn read(&self, file: *mut File, buf: *mut u8, len: usize) -> Option<usize> {
        if file.is_null() || buf.is_null() {
            return None;
        }

        let file = unsafe { &mut *file };
        if file.flags & OPEN_READ == 0 {
            trace!(0, "vfs: that handle is not open for reading");
            return None;
        }

        let _guard = self.lock.lock();

        let size = unsafe { (*file.node).size };
        if file.pos >= size || len == 0 {
            return Some(0);
        }

        let take = core::cmp::min(len, size - file.pos);
        if (file.ops.read)(file.ops.ctx, file.node, buf, take, file.pos) != 0 {
            return None;
        }

        file.pos += take;
        Some(take)
    }

    pub fn write(&self, file: *mut File, data: *const u8, len: usize) -> bool {
        if file.is_null() || (data.is_null() && len != 0) {
            return false;
        }

        let file = unsafe { &mut *file };
        if file.flags & OPEN_WRITE == 0 {
            trace!(0, "vfs: that handle is not open for writing");
            return false;
        }
        if len == 0 {
            return true;
        }

        let _guard = self.lock.lock();

        if file.flags & OPEN_APPEND != 0 {
            file.pos = unsafe { (*file.node).size };
        }
        if file.pos.checked_add(len).is_none() {
            return false;
        }

        if (file.ops.write)(file.ops.ctx, file.node, data, len, file.pos) != 0 {
            return false;
        }

        file.pos += len;
        true
    }

    pub fn seek(&self, file: *mut File, pos: usize) -> bool {
        if file.is_null() {
            return false;
        }
        let _guard = self.lock.lock();
        unsafe { (*file).pos = pos };
        true
    }

    pub fn tell(&self, file: *mut File) -> usize {
        if file.is_null() {
            return 0;
        }
        let _guard = self.lock.lock();
        unsafe { (*file).pos }
    }

    pub fn size(&self, file: *mut File) -> usize {
        if file.is_null() {
            return 0;
        }
        let _guard = self.lock.lock();
        unsafe { (*(*file).node).size }
    }

    pub fn create(&self, path: &[u8], directory: bool) -> bool {
        let _guard = self.lock.lock();
        let inner = unsafe { &*self.inner.get() };

        let resolved = match self.resolve(inner, path) {
            Some(resolved) => resolved,
            None => return false,
        };

        if inner.mounts[resolved.mount].as_ref().unwrap().read_only {
            trace!(0, "vfs: that mount is read-only");
            return false;
        }

        if !resolved.node.is_null() {
            trace!(0, "vfs: that path exists already");
            return false;
        }
        if resolved.parent.is_null() || resolved.last_len == 0 {
            return false;
        }

        let made = if directory {
            (resolved.ops.create_dir)(resolved.ops.ctx, resolved.parent, resolved.last.as_ptr())
        } else {
            (resolved.ops.create_file)(resolved.ops.ctx, resolved.parent, resolved.last.as_ptr())
        };
        !made.is_null()
    }

    pub fn remove(&self, path: &[u8]) -> bool {
        let _guard = self.lock.lock();
        let inner = unsafe { &*self.inner.get() };

        let resolved = match self.resolve(inner, path) {
            Some(resolved) if !resolved.node.is_null() => resolved,
            _ => return false,
        };

        if inner.mounts[resolved.mount].as_ref().unwrap().read_only {
            trace!(0, "vfs: that mount is read-only");
            return false;
        }

        if unsafe { (*resolved.node).is_dir() }
            && (resolved.ops.load_dir)(resolved.ops.ctx, resolved.node) != 0
        {
            return false;
        }

        if unsafe { vnode::has_open_files(resolved.node) } {
            trace!(0, "vfs: something under that path is open");
            return false;
        }

        (resolved.ops.remove)(resolved.ops.ctx, resolved.node) == 0
    }

    pub fn truncate(&self, path: &[u8], size: usize) -> bool {
        let _guard = self.lock.lock();
        let inner = unsafe { &*self.inner.get() };

        let resolved = match self.resolve(inner, path) {
            Some(resolved) if !resolved.node.is_null() => resolved,
            _ => return false,
        };

        if inner.mounts[resolved.mount].as_ref().unwrap().read_only {
            trace!(0, "vfs: that mount is read-only");
            return false;
        }
        if !unsafe { (*resolved.node).is_file() } {
            return false;
        }

        (resolved.ops.truncate)(resolved.ops.ctx, resolved.node, size) == 0
    }

    pub fn rename(&self, from: &[u8], to: &[u8]) -> bool {
        let _guard = self.lock.lock();
        let inner = unsafe { &*self.inner.get() };

        let (from_mount, _) = match self.find_mount(inner, from) {
            Some(found) => found,
            None => return false,
        };
        let (to_mount, _) = match self.find_mount(inner, to) {
            Some(found) => found,
            None => return false,
        };

        if from_mount != to_mount {
            trace!(0, "vfs: a rename across two mounts is not a rename");
            return false;
        }
        if inner.mounts[from_mount].as_ref().unwrap().read_only {
            trace!(0, "vfs: that mount is read-only");
            return false;
        }

        let source = match self.resolve(inner, from) {
            Some(resolved) if !resolved.node.is_null() => resolved,
            _ => return false,
        };

        if unsafe { (*source.node).parent }.is_null() {
            trace!(0, "vfs: the root of a mount cannot be renamed");
            return false;
        }

        if unsafe { (*source.node).is_dir() }
            && (source.ops.load_dir)(source.ops.ctx, source.node) != 0
        {
            return false;
        }

        if unsafe { vnode::has_open_files(source.node) } {
            trace!(0, "vfs: something under that path is open");
            return false;
        }

        let target = match self.resolve(inner, to) {
            Some(resolved) => resolved,
            None => return false,
        };

        if !target.node.is_null() {
            trace!(0, "vfs: the new path exists already");
            return false;
        }
        if target.parent.is_null() || target.last_len == 0 {
            return false;
        }

        /* A directory cannot be moved inside itself. */
        if unsafe { vnode::is_ancestor(source.node, target.parent) } {
            trace!(0, "vfs: that would move a directory inside itself");
            return false;
        }

        (source.ops.rename)(source.ops.ctx, source.node, target.parent, target.last.as_ptr()) == 0
    }

    pub fn sync(&self) -> bool {
        let _guard = self.lock.lock();
        let inner = unsafe { &*self.inner.get() };

        let mut ok = true;
        for mount in inner.mounts.iter().flatten() {
            if (mount.ops.sync)(mount.ops.ctx) != 0 {
                ok = false;
            }
        }
        ok
    }

    /// Replace a file's contents, creating it if it is missing.
    pub fn write_file(&self, path: &[u8], data: *const u8, len: usize) -> bool {
        let _guard = self.lock.lock();
        let inner = unsafe { &*self.inner.get() };

        let resolved = match self.resolve(inner, path) {
            Some(resolved) => resolved,
            None => return false,
        };

        if inner.mounts[resolved.mount].as_ref().unwrap().read_only {
            trace!(0, "vfs: that mount is read-only");
            return false;
        }

        let node = if resolved.node.is_null() {
            if resolved.parent.is_null() || resolved.last_len == 0 {
                return false;
            }
            let made = (resolved.ops.create_file)(
                resolved.ops.ctx, resolved.parent, resolved.last.as_ptr());
            if made.is_null() {
                return false;
            }
            made
        } else {
            resolved.node
        };

        if !unsafe { (*node).is_file() } {
            return false;
        }

        if unsafe { (*node).size } != 0 && (resolved.ops.truncate)(resolved.ops.ctx, node, 0) != 0 {
            return false;
        }
        if len == 0 {
            return true;
        }

        (resolved.ops.write)(resolved.ops.ctx, node, data, len, 0) == 0
    }
}
