//! The VFS: the mount table, path resolution, and the file API everything
//! else in the kernel reads and writes through.
//!
//! One call at a time, under one mutex -- which is what lets a filesystem
//! below have no locking of its own, and is the contract `FileSystem` is
//! written to. The composed calls (`replace_file`, `locate`) take no lock
//! themselves: they are made of the calls that do.
//!
//! Everything the mutex guards is inside it: the mounts, each owning its
//! filesystem, and the open files. An open file is named by a `Handle` -- a
//! slot in a table and the generation of what is in it -- so a handle from
//! anywhere, the C++ shell's included, is looked up rather than followed: one
//! that was closed, or never opened, reads as no file.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::num::NonZeroUsize;

use kcore::sync::Mutex;
use kcore::trace;

use crate::vnode::{Kind, NodeId, Tree, NAME_MAX};

pub const MAX_MOUNTS: usize = 16;
pub const MAX_PATH: usize = 256;

/// What a filesystem's line about itself, for `mounts`, fits in.
pub const INFO_MAX: usize = 64;

/* Open flags, as the C++ shell passes them too */
pub const OPEN_READ: usize = 1;
pub const OPEN_WRITE: usize = 2;
pub const OPEN_CREATE: usize = 4;
pub const OPEN_TRUNCATE: usize = 8;
pub const OPEN_APPEND: usize = 16;

/// What a filesystem is to the VFS: the tree of what it holds, and the calls
/// that change it.
///
/// Every call arrives with the VFS lock held, so an implementation needs no
/// locking of its own: two calls never overlap on the same filesystem. A
/// `NodeId` it is handed is one of its own tree's, live when the call was
/// made -- the VFS found it there under the same lock.
pub trait FileSystem: Send {
    /// What `mounts` shows the filesystem as
    fn name(&self) -> &'static str;

    /// A line about the filesystem for `mounts`
    fn info(&self, _out: &mut dyn core::fmt::Write) {}

    /// The block device it is on, as a handle, or 0
    fn device(&self) -> usize {
        0
    }

    /// Take the filesystem. `Some(true)` if it may only be read -- asked to
    /// be, or found to be -- `Some(false)` if it may be written, and `None`
    /// if it cannot be mounted at all.
    fn mount(&mut self, read_only: bool) -> Option<bool>;

    fn unmount(&mut self);

    fn tree(&self) -> &Tree;

    /// The VFS keeps the open counts in the nodes, and nothing else.
    fn tree_mut(&mut self) -> &mut Tree;

    fn root(&self) -> Option<NodeId>;

    /// Make a directory's children complete.
    fn load_dir(&mut self, _dir: NodeId) -> bool {
        true
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId>;
    fn create_file(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId>;
    fn create_dir(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId>;

    /// Fill `buf` from `offset`, which is inside the file, with as much of
    /// the file as there is from there.
    fn read(&mut self, file: NodeId, buf: &mut [u8], offset: usize) -> bool;
    fn write(&mut self, file: NodeId, data: &[u8], offset: usize) -> bool;
    fn truncate(&mut self, file: NodeId, size: usize) -> bool;
    fn rename(&mut self, node: NodeId, dir: NodeId, name: &[u8]) -> bool;
    fn remove(&mut self, node: NodeId) -> bool;

    fn sync(&mut self) -> bool {
        true
    }
}

/// What `stat` answers.
pub struct FileStat {
    pub kind: Kind,
    pub size: usize,
    pub ino: usize,
}

/// One entry of a directory.
pub struct DirEntry {
    name: [u8; NAME_MAX],
    name_len: usize,
    pub kind: Kind,
    pub size: usize,
}

impl DirEntry {
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }
}

/// What `mounts` shows of one mount.
pub struct MountInfo {
    path: [u8; MAX_PATH],
    path_len: usize,
    pub fs_name: &'static str,
    info: Line,
    pub read_only: bool,
}

impl MountInfo {
    pub fn path(&self) -> &[u8] {
        &self.path[..self.path_len]
    }

    pub fn info(&self) -> &[u8] {
        &self.info.text[..self.info.len]
    }
}

/// A filesystem's line about itself: what fits, and no more.
struct Line {
    text: [u8; INFO_MAX],
    len: usize,
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let take = s.len().min(INFO_MAX - self.len);
        self.text[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

struct Mount {
    path: [u8; MAX_PATH],
    path_len: usize,
    fs: Box<dyn FileSystem>,
    read_only: bool,
    /// On the filesystem's device, for as long as it is mounted
    claim: usize,
    /// Handles open on it, which is what refuses an unmount
    open_files: usize,
    /// Tells this mount from the one that takes its place
    id: u64,
}

impl Mount {
    fn path(&self) -> &[u8] {
        &self.path[..self.path_len]
    }

    /// Off its device and gone. The filesystem is released with it.
    fn take_down(mut self) {
        self.fs.unmount();
        block::release(self.claim);
    }
}

/* ---- open files ---- */

/// An open file, as everyone outside the VFS knows it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Handle(NonZeroUsize);

/* The slot, plus one, below; the generation above. */
const HANDLE_SLOT_BITS: u32 = 32;
const HANDLE_SLOT_MASK: usize = (1 << HANDLE_SLOT_BITS) - 1;
const _: () = assert!(usize::BITS == 2 * HANDLE_SLOT_BITS, "a handle is a slot and a generation");

impl Handle {
    fn new(slot: usize, generation: u32) -> Option<Handle> {
        if slot >= HANDLE_SLOT_MASK {
            return None;
        }
        NonZeroUsize::new(((generation as usize) << HANDLE_SLOT_BITS) | (slot + 1)).map(Handle)
    }

    fn slot(self) -> usize {
        (self.0.get() & HANDLE_SLOT_MASK) - 1
    }

    fn generation(self) -> u32 {
        (self.0.get() >> HANDLE_SLOT_BITS) as u32
    }

    /// The handle as the word a caller outside Rust holds; 0 is no file.
    pub fn into_raw(handle: Option<Handle>) -> usize {
        handle.map_or(0, |handle| handle.0.get())
    }

    /// Whatever word a caller outside Rust hands back. Any word will do:
    /// one that is no open file's is found to be so when it is looked up.
    pub fn from_raw(raw: usize) -> Option<Handle> {
        let raw = NonZeroUsize::new(raw)?;
        if raw.get() & HANDLE_SLOT_MASK == 0 {
            return None;
        }
        Some(Handle(raw))
    }
}

/// A position over a node, and the mount it belongs to.
struct OpenFile {
    mount_id: u64,
    node: NodeId,
    pos: usize,
    flags: usize,
}

struct FileSlot {
    generation: u32,
    file: Option<OpenFile>,
}

struct Inner {
    mounts: [Option<Mount>; MAX_MOUNTS],
    count: usize,
    next_id: u64,
    files: Vec<FileSlot>,
}

impl Inner {
    fn mount_by_id(&mut self, id: u64) -> Option<&mut Mount> {
        self.mounts.iter_mut().flatten().find(|mount| mount.id == id)
    }

    fn file(&mut self, handle: Handle) -> Option<&mut OpenFile> {
        let slot = self.files.get_mut(handle.slot())?;
        if slot.generation != handle.generation() {
            return None;
        }
        slot.file.as_mut()
    }

    /// A slot for an open file, or None when there is no memory for another.
    fn add_file(&mut self, file: OpenFile) -> Option<Handle> {
        let index = match self.files.iter().position(|slot| slot.file.is_none()) {
            Some(index) => index,
            None => {
                if self.files.try_reserve(1).is_err() {
                    return None;
                }
                self.files.push(FileSlot { generation: 0, file: None });
                self.files.len() - 1
            }
        };

        let slot = &mut self.files[index];
        let handle = Handle::new(index, slot.generation)?;
        slot.file = Some(file);
        Some(handle)
    }

    fn take_file(&mut self, handle: Handle) -> Option<OpenFile> {
        let slot = self.files.get_mut(handle.slot())?;
        if slot.generation != handle.generation() {
            return None;
        }
        let file = slot.file.take()?;
        /* The handle names nothing from here on, whatever takes the slot. */
        slot.generation = slot.generation.wrapping_add(1);
        Some(file)
    }

    /// The files of a mount that is going: abandoned, so that a handle kept
    /// past the unmount finds no file rather than a filesystem that is gone.
    fn drop_files_of(&mut self, mount_id: u64) {
        for slot in self.files.iter_mut() {
            if slot.file.as_ref().is_some_and(|file| file.mount_id == mount_id) {
                slot.file = None;
                slot.generation = slot.generation.wrapping_add(1);
            }
        }
    }

    fn remove_mount(&mut self, index: usize) -> Option<Mount> {
        let mount = self.mounts[index].take()?;
        for at in index..self.count - 1 {
            self.mounts[at] = self.mounts[at + 1].take();
        }
        self.count -= 1;
        self.drop_files_of(mount.id);
        Some(mount)
    }
}

pub struct Vfs {
    inner: Mutex<Inner>,
}

/// What a mount's claim on its device says to whoever is refused it.
const MOUNT_HOLDER: &core::ffi::CStr = c"a mounted filesystem";

/// What a path came to.
struct Resolved {
    mount: usize,
    /// None when the last component does not exist; `parent` and `last` then
    /// say where it would go
    node: Option<NodeId>,
    parent: Option<NodeId>,
    last: [u8; NAME_MAX],
    last_len: usize,
}

impl Resolved {
    fn last(&self) -> &[u8] {
        &self.last[..self.last_len]
    }

    /// Where something new would go, and what it would be called.
    fn new_entry(&self) -> Option<(NodeId, &[u8])> {
        match self.parent {
            Some(parent) if self.last_len != 0 => Some((parent, self.last())),
            _ => None,
        }
    }
}

impl Vfs {
    pub fn new() -> Option<Box<Vfs>> {
        Some(Box::new(Vfs {
            inner: Mutex::new(Inner {
                mounts: [const { None }; MAX_MOUNTS],
                count: 0,
                next_id: 1,
                files: Vec::new(),
            })?,
        }))
    }

    /* ---- mounts ---- */

    /// Mount `fs` at `path`. `Some(read_only)` once it is; None -- and the
    /// filesystem released -- when it cannot be.
    pub fn mount(&self, path: &[u8], mut fs: Box<dyn FileSystem>, read_only: bool) -> Option<bool> {
        if path.is_empty() || path[0] != b'/' || path.len() >= MAX_PATH {
            trace!(0, "vfs: a mount path must start with / and fit {} bytes", MAX_PATH);
            return None;
        }

        let mut inner = self.inner.lock();

        let device = fs.device();

        for mount in inner.mounts.iter().flatten() {
            if mount.path() == path {
                trace!(0, "vfs: something is mounted on that path already");
                return None;
            }
            if device != 0 && mount.fs.device() == device {
                trace!(0, "vfs: that device is mounted already");
                return None;
            }
        }

        if inner.count >= MAX_MOUNTS {
            trace!(0, "vfs: {} mounts is all there is room for", MAX_MOUNTS);
            return None;
        }

        /* The device is the filesystem's while it is mounted: nothing may
         * write to it around the filesystem -- the disk log, a module going
         * direct -- nor another mount take it, or a disk or partition
         * overlapping it. */
        let mut claim = 0;
        if device != 0 {
            claim = match block::claim_as(device, MOUNT_HOLDER) {
                Ok(claim) => claim,
                Err(held_by) => {
                    trace!(0, "vfs: the device is in use by {}", held_by);
                    return None;
                }
            };
        }

        /* The filesystem may find an image it can read but must not write. */
        let read_only = match fs.mount(read_only) {
            Some(found_read_only) => read_only || found_read_only,
            None => {
                block::release(claim);
                return None;
            }
        };

        let mut entry = Mount {
            path: [0; MAX_PATH],
            path_len: path.len(),
            fs,
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
        Some(read_only)
    }

    /// Take a filesystem off its mount point and release it. False if it is
    /// not mounted there, or is busy.
    pub fn unmount(&self, path: &[u8]) -> bool {
        let mut inner = self.inner.lock();

        let index = match inner.mounts.iter().flatten().position(|mount| mount.path() == path) {
            Some(index) => index,
            None => {
                trace!(0, "vfs: nothing is mounted there");
                return false;
            }
        };

        let open_files = inner.mounts[index].as_ref().map_or(0, |mount| mount.open_files);
        if open_files != 0 {
            trace!(0, "vfs: the mount is busy, {} files open", open_files);
            return false;
        }

        if let Some(mount) = inner.remove_mount(index) {
            mount.take_down();
        }
        true
    }

    /// Take everything down, deepest mount first, releasing each filesystem.
    /// This is shutdown: a handle left open is abandoned, not honoured.
    pub fn unmount_all(&self) {
        let mut inner = self.inner.lock();

        while inner.count > 0 {
            /* Deepest first, so a mount inside another goes before it. */
            let mut deepest = 0;
            let mut longest = 0;
            for (index, mount) in inner.mounts.iter().flatten().enumerate() {
                if mount.path_len >= longest {
                    longest = mount.path_len;
                    deepest = index;
                }
            }

            let mount = match inner.remove_mount(deepest) {
                Some(mount) => mount,
                None => return,
            };

            trace!(0, "vfs: unmounting {}", core::str::from_utf8(mount.path()).unwrap_or("?"));
            if mount.open_files != 0 {
                trace!(0, "vfs: unmounting with {} files still open", mount.open_files);
            }
            mount.take_down();
        }
    }

    pub fn mount_count(&self) -> usize {
        self.inner.lock().count
    }

    /// What the index'th mount is, for `mounts` to print.
    pub fn mount_info(&self, index: usize) -> Option<MountInfo> {
        let inner = self.inner.lock();
        let mount = inner.mounts.get(index)?.as_ref()?;

        let mut info = MountInfo {
            path: mount.path,
            path_len: mount.path_len,
            fs_name: mount.fs.name(),
            info: Line { text: [0; INFO_MAX], len: 0 },
            read_only: mount.read_only,
        };
        mount.fs.info(&mut info.info);
        Some(info)
    }

    /* ---- resolution ---- */

    /// The mount a path is on: the longest mount path it starts with, and
    /// what is left of it after that.
    fn find_mount<'a>(inner: &Inner, path: &'a [u8]) -> Option<(usize, &'a [u8])> {
        let mut best: Option<(usize, usize)> = None;

        for (index, mount) in inner.mounts.iter().flatten().enumerate() {
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

    /// Walk a path to what it names.
    fn resolve(inner: &mut Inner, path: &[u8]) -> Option<Resolved> {
        let (index, rest) = match Self::find_mount(inner, path) {
            Some(found) => found,
            None => {
                trace!(0, "vfs: no mount holds that path");
                return None;
            }
        };

        let fs = &mut inner.mounts[index].as_mut()?.fs;
        let mut at = fs.root()?;

        let mut resolved = Resolved {
            mount: index,
            node: None,
            parent: None,
            last: [0; NAME_MAX],
            last_len: 0,
        };

        if rest.is_empty() {
            resolved.node = Some(at);
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
                    resolved.node = Some(at);
                    return Some(resolved);
                }
                continue;
            }
            if component == b".." {
                if let Some(parent) = fs.tree().parent(at) {
                    at = parent;
                }
                if last {
                    resolved.node = Some(at);
                    return Some(resolved);
                }
                continue;
            }

            let child = fs.lookup(at, component);

            if last {
                resolved.parent = Some(at);
                resolved.last[..component.len()].copy_from_slice(component);
                resolved.last_len = component.len();
                resolved.node = child;
                return Some(resolved);
            }

            match child {
                Some(child) if fs.tree().get(child).is_some_and(|node| node.is_dir()) => at = child,
                _ => {
                    trace!(0, "vfs: a component of that path is not a directory");
                    return None;
                }
            }
        }

        resolved.node = Some(at);
        Some(resolved)
    }
}

/* ---- the file API ---- */

impl Vfs {
    pub fn stat(&self, path: &[u8]) -> Option<FileStat> {
        let mut inner = self.inner.lock();

        let resolved = Self::resolve(&mut inner, path)?;
        let mount = inner.mounts[resolved.mount].as_ref()?;
        let node = mount.fs.tree().get(resolved.node?)?;

        Some(FileStat {
            kind: node.kind,
            size: if node.is_file() { node.size } else { 0 },
            ino: node.ino,
        })
    }

    /// The index'th entry of the directory at `path`.
    pub fn read_dir(&self, path: &[u8], index: usize) -> Option<DirEntry> {
        let mut inner = self.inner.lock();

        let resolved = Self::resolve(&mut inner, path)?;
        let dir = resolved.node?;
        let fs = &mut inner.mounts[resolved.mount].as_mut()?.fs;

        if !fs.tree().get(dir)?.is_dir() || !fs.load_dir(dir) {
            return None;
        }

        let tree = fs.tree();
        let child = tree.get(tree.children(dir).nth(index)?)?;

        let mut entry = DirEntry {
            name: [0; NAME_MAX],
            name_len: child.name().len(),
            kind: child.kind,
            size: if child.is_file() { child.size } else { 0 },
        };
        entry.name[..entry.name_len].copy_from_slice(child.name());
        Some(entry)
    }

    pub fn open(&self, path: &[u8], flags: usize) -> Option<Handle> {
        let mut flags = flags;
        if flags & OPEN_APPEND != 0 {
            flags |= OPEN_WRITE;
        }
        if flags & (OPEN_READ | OPEN_WRITE) == 0 {
            trace!(0, "vfs: an open for neither reading nor writing");
            return None;
        }

        let mut guard = self.inner.lock();
        let inner = &mut *guard;

        let writes = flags & (OPEN_WRITE | OPEN_CREATE | OPEN_TRUNCATE) != 0;

        let resolved = Self::resolve(inner, path)?;
        let mount = inner.mounts[resolved.mount].as_mut()?;

        if writes && mount.read_only {
            trace!(0, "vfs: that mount is read-only");
            return None;
        }

        let node = match resolved.node {
            Some(node) => node,
            None => {
                if flags & OPEN_CREATE == 0 {
                    return None;
                }
                let (parent, name) = resolved.new_entry()?;
                match mount.fs.create_file(parent, name) {
                    Some(node) => node,
                    None => {
                        trace!(0, "vfs: the file could not be created");
                        return None;
                    }
                }
            }
        };

        let size = match mount.fs.tree().get(node) {
            Some(found) if found.is_file() => found.size,
            _ => {
                trace!(0, "vfs: that path is not a file");
                return None;
            }
        };

        let mut size = size;
        if flags & OPEN_TRUNCATE != 0 && size != 0 {
            if !mount.fs.truncate(node, 0) {
                return None;
            }
            size = 0;
        }

        let pos = if flags & OPEN_APPEND != 0 { size } else { 0 };
        let mount_id = mount.id;

        let handle = inner.add_file(OpenFile { mount_id, node, pos, flags })?;

        let mount = inner.mounts[resolved.mount].as_mut()?;
        if let Some(found) = mount.fs.tree_mut().get_mut(node) {
            found.open_count += 1;
        }
        mount.open_files += 1;
        Some(handle)
    }

    pub fn close(&self, handle: Handle) {
        let mut inner = self.inner.lock();

        let file = match inner.take_file(handle) {
            Some(file) => file,
            None => return,
        };

        if let Some(mount) = inner.mount_by_id(file.mount_id) {
            if let Some(node) = mount.fs.tree_mut().get_mut(file.node) {
                node.open_count = node.open_count.saturating_sub(1);
            }
            mount.open_files = mount.open_files.saturating_sub(1);
        }
    }

    /// What was read into `buf`: 0 at the end of the file, None on error.
    pub fn read(&self, handle: Handle, buf: &mut [u8]) -> Option<usize> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;

        let (mount_id, node, pos, flags) = {
            let file = inner.file(handle)?;
            (file.mount_id, file.node, file.pos, file.flags)
        };
        if flags & OPEN_READ == 0 {
            trace!(0, "vfs: that handle is not open for reading");
            return None;
        }

        let fs = &mut inner.mount_by_id(mount_id)?.fs;
        let size = fs.tree().get(node)?.size;
        if pos >= size || buf.is_empty() {
            return Some(0);
        }

        let take = buf.len().min(size - pos);
        if !fs.read(node, &mut buf[..take], pos) {
            return None;
        }

        inner.file(handle)?.pos = pos + take;
        Some(take)
    }

    pub fn write(&self, handle: Handle, data: &[u8]) -> bool {
        self.write_at(handle, data).is_some()
    }

    fn write_at(&self, handle: Handle, data: &[u8]) -> Option<()> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;

        let (mount_id, node, pos, flags) = {
            let file = inner.file(handle)?;
            (file.mount_id, file.node, file.pos, file.flags)
        };
        if flags & OPEN_WRITE == 0 {
            trace!(0, "vfs: that handle is not open for writing");
            return None;
        }
        if data.is_empty() {
            return Some(());
        }

        let fs = &mut inner.mount_by_id(mount_id)?.fs;
        let pos = if flags & OPEN_APPEND != 0 { fs.tree().get(node)?.size } else { pos };
        let end = pos.checked_add(data.len())?;

        if !fs.write(node, data, pos) {
            return None;
        }

        inner.file(handle)?.pos = end;
        Some(())
    }

    /// As much of `buf` as the file has from `offset`: the count read, 0 at
    /// or past its end, None on an error. The handle's position is not
    /// moved, so two callers of one handle do not race on it.
    pub fn read_at(&self, handle: Handle, offset: usize, buf: &mut [u8]) -> Option<usize> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;

        let (mount_id, node, flags) = {
            let file = inner.file(handle)?;
            (file.mount_id, file.node, file.flags)
        };
        if flags & OPEN_READ == 0 {
            trace!(0, "vfs: that handle is not open for reading");
            return None;
        }

        let fs = &mut inner.mount_by_id(mount_id)?.fs;
        let size = fs.tree().get(node)?.size;
        if offset >= size || buf.is_empty() {
            return Some(0);
        }

        let take = buf.len().min(size - offset);
        fs.read(node, &mut buf[..take], offset).then_some(take)
    }

    /// `data` into the file at `offset`, all of it inside the size the file
    /// has, or none of it: a disk image, whose size is its disk's, is never
    /// grown by what its guest writes. The handle's position is not moved.
    pub fn write_within(&self, handle: Handle, offset: usize, data: &[u8]) -> bool {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;

        let (mount_id, node, flags) = match inner.file(handle) {
            Some(file) => (file.mount_id, file.node, file.flags),
            None => return false,
        };
        if flags & OPEN_WRITE == 0 {
            trace!(0, "vfs: that handle is not open for writing");
            return false;
        }

        let Some(fs) = inner.mount_by_id(mount_id).map(|mount| &mut mount.fs) else {
            return false;
        };
        let size = match fs.tree().get(node) {
            Some(found) => found.size,
            None => return false,
        };
        match offset.checked_add(data.len()) {
            Some(end) if end <= size => {}
            _ => return false,
        }
        data.is_empty() || fs.write(node, data, offset)
    }

    /// The filesystem the file is on, synced: everything written to it on
    /// its disk's medium.
    pub fn sync_file(&self, handle: Handle) -> bool {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;

        let mount_id = match inner.file(handle) {
            Some(file) => file.mount_id,
            None => return false,
        };
        inner.mount_by_id(mount_id).is_some_and(|mount| mount.fs.sync())
    }

    pub fn seek(&self, handle: Handle, pos: usize) -> bool {
        match self.inner.lock().file(handle) {
            Some(file) => {
                file.pos = pos;
                true
            }
            None => false,
        }
    }

    pub fn tell(&self, handle: Handle) -> usize {
        self.inner.lock().file(handle).map_or(0, |file| file.pos)
    }

    pub fn size(&self, handle: Handle) -> usize {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;

        let (mount_id, node) = match inner.file(handle) {
            Some(file) => (file.mount_id, file.node),
            None => return 0,
        };
        inner.mount_by_id(mount_id)
            .and_then(|mount| mount.fs.tree().get(node))
            .map_or(0, |node| node.size)
    }

    /// The open file's size, or None for a handle that is no open file's.
    pub fn length(&self, handle: Handle) -> Option<usize> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;

        let (mount_id, node) = {
            let file = inner.file(handle)?;
            (file.mount_id, file.node)
        };
        Some(inner.mount_by_id(mount_id)?.fs.tree().get(node)?.size)
    }

    /// The mount a resolved path is on, if it may be written.
    fn writable<'a>(inner: &'a mut Inner, resolved: &Resolved) -> Option<&'a mut Mount> {
        let mount = inner.mounts[resolved.mount].as_mut()?;
        if mount.read_only {
            trace!(0, "vfs: that mount is read-only");
            return None;
        }
        Some(mount)
    }

    pub fn create(&self, path: &[u8], directory: bool) -> bool {
        let mut inner = self.inner.lock();
        Self::create_locked(&mut inner, path, directory).is_some()
    }

    fn create_locked(inner: &mut Inner, path: &[u8], directory: bool) -> Option<NodeId> {
        let resolved = Self::resolve(inner, path)?;
        let mount = Self::writable(inner, &resolved)?;

        if resolved.node.is_some() {
            trace!(0, "vfs: that path exists already");
            return None;
        }

        let (parent, name) = resolved.new_entry()?;
        if directory {
            mount.fs.create_dir(parent, name)
        } else {
            mount.fs.create_file(parent, name)
        }
    }

    pub fn remove(&self, path: &[u8]) -> bool {
        let mut inner = self.inner.lock();
        Self::remove_locked(&mut inner, path).is_some()
    }

    fn remove_locked(inner: &mut Inner, path: &[u8]) -> Option<()> {
        let resolved = Self::resolve(inner, path)?;
        let node = resolved.node?;
        let fs = &mut Self::writable(inner, &resolved)?.fs;

        if fs.tree().get(node)?.is_dir() && !fs.load_dir(node) {
            return None;
        }
        if fs.tree().has_open_files(node) {
            trace!(0, "vfs: something under that path is open");
            return None;
        }

        fs.remove(node).then_some(())
    }

    pub fn truncate(&self, path: &[u8], size: usize) -> bool {
        let mut inner = self.inner.lock();
        Self::truncate_locked(&mut inner, path, size).is_some()
    }

    fn truncate_locked(inner: &mut Inner, path: &[u8], size: usize) -> Option<()> {
        let resolved = Self::resolve(inner, path)?;
        let node = resolved.node?;
        let fs = &mut Self::writable(inner, &resolved)?.fs;

        if !fs.tree().get(node)?.is_file() {
            return None;
        }
        fs.truncate(node, size).then_some(())
    }

    pub fn rename(&self, from: &[u8], to: &[u8]) -> bool {
        let mut inner = self.inner.lock();
        Self::rename_locked(&mut inner, from, to).is_some()
    }

    fn rename_locked(inner: &mut Inner, from: &[u8], to: &[u8]) -> Option<()> {
        let (from_mount, _) = Self::find_mount(inner, from)?;
        let (to_mount, _) = Self::find_mount(inner, to)?;

        if from_mount != to_mount {
            trace!(0, "vfs: a rename across two mounts is not a rename");
            return None;
        }

        let source = Self::resolve(inner, from)?;
        let node = source.node?;
        {
            let fs = &mut Self::writable(inner, &source)?.fs;

            if fs.tree().parent(node).is_none() {
                trace!(0, "vfs: the root of a mount cannot be renamed");
                return None;
            }
            if fs.tree().get(node)?.is_dir() && !fs.load_dir(node) {
                return None;
            }
            if fs.tree().has_open_files(node) {
                trace!(0, "vfs: something under that path is open");
                return None;
            }
        }

        let target = Self::resolve(inner, to)?;
        if target.node.is_some() {
            trace!(0, "vfs: the new path exists already");
            return None;
        }
        let (new_parent, new_name) = target.new_entry()?;

        let fs = &mut inner.mounts[source.mount].as_mut()?.fs;

        /* A directory cannot be moved inside itself. */
        if fs.tree().is_ancestor(node, new_parent) {
            trace!(0, "vfs: that would move a directory inside itself");
            return None;
        }

        fs.rename(node, new_parent, new_name).then_some(())
    }

    pub fn sync(&self) -> bool {
        let mut inner = self.inner.lock();

        let mut ok = true;
        for mount in inner.mounts.iter_mut().flatten() {
            if !mount.fs.sync() {
                ok = false;
            }
        }
        ok
    }

    /// Replace a file's contents, creating it if it is missing.
    pub fn write_file(&self, path: &[u8], data: &[u8]) -> bool {
        let mut inner = self.inner.lock();
        Self::write_file_locked(&mut inner, path, data).is_some()
    }

    fn write_file_locked(inner: &mut Inner, path: &[u8], data: &[u8]) -> Option<()> {
        let resolved = Self::resolve(inner, path)?;
        let fs = &mut Self::writable(inner, &resolved)?.fs;

        let node = match resolved.node {
            Some(node) => node,
            None => {
                let (parent, name) = resolved.new_entry()?;
                fs.create_file(parent, name)?
            }
        };

        let size = match fs.tree().get(node) {
            Some(found) if found.is_file() => found.size,
            _ => return None,
        };

        if size != 0 && !fs.truncate(node, 0) {
            return None;
        }
        if data.is_empty() {
            return Some(());
        }

        fs.write(node, data, 0).then_some(())
    }
}

/// A file open for as long as this is held, and closed when it goes -- on
/// every way out of the function that opened it.
pub struct Open<'a> {
    vfs: &'a Vfs,
    handle: Handle,
}

impl<'a> Open<'a> {
    pub fn new(vfs: &'a Vfs, path: &[u8], flags: usize) -> Option<Open<'a>> {
        vfs.open(path, flags).map(|handle| Open { vfs, handle })
    }

    pub fn read(&self, buf: &mut [u8]) -> Option<usize> {
        self.vfs.read(self.handle, buf)
    }

    pub fn write(&self, data: &[u8]) -> bool {
        self.vfs.write(self.handle, data)
    }

    pub fn seek(&self, pos: usize) -> bool {
        self.vfs.seek(self.handle, pos)
    }

    pub fn size(&self) -> usize {
        self.vfs.size(self.handle)
    }
}

impl Drop for Open<'_> {
    fn drop(&mut self) {
        self.vfs.close(self.handle);
    }
}
