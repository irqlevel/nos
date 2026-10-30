//! The VFS: the mount table, path resolution, and the file API everything
//! else in the kernel reads and writes through.
//!
//! One call at a time on each filesystem, under its mount's mutex -- which
//! is what lets a filesystem below have no locking of its own, and is the
//! contract `FileSystem` is written to -- and calls on two mounts side by
//! side: a read of megabytes off one disk holds up nobody else's. The
//! composed calls (`replace_file`, `locate`) take no lock themselves: they
//! are made of the calls that do.
//!
//! Each lock owns what it guards, and they are taken in this order:
//!
//! - `admin`: a mount, an unmount and `unmount_all`, one at a time and each
//!   for the whole of it -- the filesystem's own mount and unmount, a journal
//!   replayed or written home, among it -- so that what one finds of the
//!   table stays so until it is done.
//! - a mount's slot: its filesystem, and the files open on it with what is
//!   counted of them; held across every call into the filesystem, down to
//!   the device and back.
//! - `table`: the mount points, which a path is resolved through; held only
//!   to read one or to change one, and never across a call into a
//!   filesystem.
//!
//! No two slots are held at once, and none is waited for with the table
//! held: a call waits for its own mount and nothing else, and the table is
//! never held up behind a filesystem.
//!
//! An open file is named by a `Handle` -- its mount's slot, a slot in that
//! mount's table of files and the generation of what is in it -- so a
//! handle from anywhere, the C++ shell's included, is looked up rather than
//! followed, under its mount's lock alone: one that was closed, or never
//! opened, reads as no file.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::num::NonZeroUsize;

use kcore::sync::{Mutex, MutexGuard};
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
/// A disk's image (`image`): the file held while the handle is open --
/// nobody else may write it, cut it or grow it (nor, open, rename or remove
/// it), so that its blocks stay where `map` said they are while the image
/// reads and writes them off the lock. With `OPEN_WRITE` the holder writes
/// it, and has it alone; without, any number may hold it to read. Refused
/// while anyone else has the file open for writing.
pub const OPEN_HOLD: usize = 32;

/// What a filesystem is to the VFS: the tree of what it holds, and the calls
/// that change it.
///
/// Every call arrives with its mount's lock held -- `mount` and `unmount`
/// excepted, made before the filesystem is in its slot and after it is out
/// of it, where nothing else can reach it -- so an implementation needs no
/// locking of its own: two calls never overlap on the same filesystem.
/// Calls on two filesystems do: what one keeps is its own, and anything two
/// mounts share has a lock of its own. A `NodeId` it is handed is one of
/// its own tree's, live when the call was made -- the VFS found it there
/// under the same lock.
pub trait FileSystem: Send {
    /// What `mounts` shows the filesystem as
    fn name(&self) -> &'static str;

    /// A line about the filesystem for `mounts`
    fn info(&self, _out: &mut dyn core::fmt::Write) {}

    /// The block device it is on, as a handle, or 0
    fn device(&self) -> usize {
        0
    }

    /// How big it is and how much of it is free, as `df` says it -- from
    /// what the filesystem counts in memory, nothing read: None for one
    /// that keeps no such count, or is not mounted
    fn space(&self) -> Option<Space> {
        None
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

    /// The biggest a file here can be while it costs only what is written
    /// of it, its holes nothing: how far past its end a test may write
    /// (`selftest`). 0 where a file's whole size is paid for -- in memory,
    /// or in blocks of its own.
    fn sparse_max(&self) -> usize {
        0
    }

    /// How big the blocks are that a file's data is kept in on the device
    /// (`device`) -- what a disk's image reads and writes off the VFS's
    /// lock, a block where `map_blocks` says it is -- or 0 for a filesystem
    /// that keeps no blocks of its own on a device, whose images go
    /// through it a call at a time. Never more than a page.
    fn data_block_size(&self) -> usize {
        0
    }

    /// The device sector each of the `out.len()` blocks of `file` from
    /// block `first` begins at -- 0 for a hole, which is nothing on the
    /// device and reads as zeros -- every one of them inside the file.
    /// Nothing is allocated or changed. false on an I/O error, or a pointer
    /// that leads outside the filesystem -- and for a filesystem with no
    /// blocks of its own (`data_block_size` 0).
    fn map_blocks(&mut self, _file: NodeId, _first: u64, _out: &mut [u64]) -> bool {
        false
    }

    /// Give back the blocks of `count` blocks of `file` from block `first`,
    /// of those inside it -- holes from here on, reading as zeros, the
    /// file's size as it was -- in the filesystem's own order, so that no
    /// crash leaves a block both in use and free. How many were given back,
    /// or None: an I/O error, or a filesystem with no blocks of its own
    /// (`data_block_size` 0), the default.
    fn punch(&mut self, _file: NodeId, _first: u64, _count: u64) -> Option<u64> {
        None
    }

    /// How many names the filesystem's own record says `file` has -- its
    /// inode's link count -- or None where it keeps none (a file of one
    /// name), the default. What a remove asks of a file held as a disk's
    /// image by another name: whether this name is the last.
    fn links(&mut self, _file: NodeId) -> Option<u32> {
        None
    }
}

/// What `FileSystem::space` answers, in bytes: the blocks a file's data may
/// take, and those of them no file has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    pub total_bytes: u64,
    pub free_bytes: u64,
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

/// Why a directory had no entry to give, other than having no more.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirError {
    /// The path names nothing: no mount holds it, or something on the way
    /// to it is missing or not a directory
    NotFound,
    /// It names a file
    NotDir,
    /// The filesystem could not read the directory's entries
    Unreadable,
}

/// What `mounts` shows of one mount.
pub struct MountInfo {
    path: [u8; MAX_PATH],
    path_len: usize,
    pub fs_name: &'static str,
    info: Line,
    pub read_only: bool,
    /// The block device it is on, as a handle, or 0
    pub device: usize,
    pub space: Option<Space>,
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

/* ---- the mounts ---- */

/// What the table holds of a mount: where it is, which it is and the device
/// it is on -- what a path is resolved through, and a second mount of the
/// device refused by, without the mount's own lock.
struct Entry {
    path: [u8; MAX_PATH],
    path_len: usize,
    /// Its mount's: tells it from the one that takes its place
    id: u64,
    /// The block device it is on, as a handle, or 0
    device: usize,
}

impl Entry {
    fn path(&self) -> &[u8] {
        &self.path[..self.path_len]
    }
}

/// The mount points: a mount's entry at its slot's index, there from when
/// its filesystem is in the slot until before it is taken out -- so that
/// whoever finds a path here and then takes the slot's lock finds the mount
/// in it, unless it was taken down in between.
struct Table {
    entries: [Option<Entry>; MAX_MOUNTS],
}

/// What only a mount, an unmount and `unmount_all` touch, and hold for the
/// whole of each.
struct Admin {
    next_id: u64,
}

struct Mount {
    /// Tells this mount from the one that takes its place: its entry's
    id: u64,
    fs: Box<dyn FileSystem>,
    read_only: bool,
    /// On the filesystem's device, for as long as it is mounted
    claim: usize,
    /// Handles open on it, which is what refuses an unmount
    open_files: usize,
    /// The writers and holds of every file open on it, by identity.
    opens: BTreeMap<u64, Opens>,
}

impl Mount {
    /// How file `key` is open.
    fn opens(&self, key: u64) -> Opens {
        self.opens.get(&key).copied().unwrap_or_default()
    }

    /// Whether the file `node` is held as a disk's image, by whatever name.
    fn held(&self, node: NodeId) -> bool {
        self.opens(identity_key(&*self.fs, node)).holds != 0
    }

    /// The files at or under `top` held as a disk's image by any of their
    /// names -- by identity, as `held` asks; a few, a guest's disk each.
    fn held_under(&self, top: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let tree = self.fs.tree();
        let mut at = Some(top);
        while let Some(id) = at {
            if tree.get(id).is_some_and(|n| n.is_file()) && self.held(id) {
                out.push(id);
            }
            at = tree.next_under(top, id);
        }
        out
    }

    /// The mount, if it may be written.
    fn writable(&mut self) -> Option<&mut Mount> {
        if self.read_only {
            trace!(0, "vfs: that mount is read-only");
            return None;
        }
        Some(self)
    }

    /// Off its device and gone. The filesystem is released with it.
    fn take_down(mut self) {
        self.fs.unmount();
        block::release(self.claim);
    }
}

/// A mount's place: what is mounted in it, if anything, and the files open
/// on that.
struct MountSlot {
    /// Which it is: what a handle into it says
    index: usize,
    /// Kept across the mounts the slot has, so that the generations go on
    /// rising and no handle into a mount gone names a file of the next.
    files: Files,
    mount: Option<Mount>,
}

/* ---- open files ---- */

/// An open file, as everyone outside the VFS knows it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Handle(NonZeroUsize);

/* The slot, plus one, below -- the file's slot in its mount's table above
 * the mount's slot -- and the generation above. */
const HANDLE_SLOT_BITS: u32 = 32;
const HANDLE_SLOT_MASK: usize = (1 << HANDLE_SLOT_BITS) - 1;
const _: () = assert!(usize::BITS == 2 * HANDLE_SLOT_BITS, "a handle is a slot and a generation");
/// The bits of a handle's slot that say which mount's it is.
const HANDLE_MOUNT_BITS: u32 = 4;
const HANDLE_MOUNT_MASK: usize = (1 << HANDLE_MOUNT_BITS) - 1;
const _: () = assert!(MAX_MOUNTS <= 1 << HANDLE_MOUNT_BITS, "a handle says which mount's slot it is in");
/// The files one mount may have open at once: as many as a handle's slot
/// has room for beside its mount's.
const MAX_FILES: usize = (HANDLE_SLOT_MASK >> HANDLE_MOUNT_BITS) - 1;

impl Handle {
    fn new(mount_slot: usize, file_slot: usize, generation: u32) -> Option<Handle> {
        if mount_slot >= MAX_MOUNTS || file_slot >= MAX_FILES {
            return None;
        }
        let slot = (file_slot << HANDLE_MOUNT_BITS) | mount_slot;
        NonZeroUsize::new(((generation as usize) << HANDLE_SLOT_BITS) | (slot + 1)).map(Handle)
    }

    fn slot(self) -> usize {
        (self.0.get() & HANDLE_SLOT_MASK) - 1
    }

    /// The slot of the mount it is on.
    fn mount_slot(self) -> usize {
        self.slot() & HANDLE_MOUNT_MASK
    }

    /// Its slot in its mount's table of files.
    fn file_slot(self) -> usize {
        self.slot() >> HANDLE_MOUNT_BITS
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

/// A position over a node of its mount's filesystem.
struct OpenFile {
    node: NodeId,
    pos: usize,
    flags: usize,
}

struct FileSlot {
    generation: u32,
    file: Option<OpenFile>,
}

/// The files open on a mount, each in a slot of its own.
struct Files {
    slots: Vec<FileSlot>,
}

impl Files {
    fn get(&mut self, handle: Handle) -> Option<&mut OpenFile> {
        let slot = self.slots.get_mut(handle.file_slot())?;
        if slot.generation != handle.generation() {
            return None;
        }
        slot.file.as_mut()
    }

    /// A slot for an open file on the mount in slot `mount_slot`, or None
    /// when there is no memory for another.
    fn add(&mut self, mount_slot: usize, file: OpenFile) -> Option<Handle> {
        let index = match self.slots.iter().position(|slot| slot.file.is_none()) {
            Some(index) => index,
            None => {
                if self.slots.try_reserve(1).is_err() {
                    return None;
                }
                self.slots.push(FileSlot { generation: 0, file: None });
                self.slots.len() - 1
            }
        };

        let slot = &mut self.slots[index];
        let handle = Handle::new(mount_slot, index, slot.generation)?;
        slot.file = Some(file);
        Some(handle)
    }

    fn take(&mut self, handle: Handle) -> Option<OpenFile> {
        let slot = self.slots.get_mut(handle.file_slot())?;
        if slot.generation != handle.generation() {
            return None;
        }
        let file = slot.file.take()?;
        /* The handle names nothing from here on, whatever takes the slot. */
        slot.generation = slot.generation.wrapping_add(1);
        Some(file)
    }

    /// Every file abandoned: its mount is going, and a handle kept past the
    /// unmount finds no file rather than a filesystem that is gone.
    fn abandon(&mut self) {
        for slot in self.slots.iter_mut() {
            if slot.file.take().is_some() {
                slot.generation = slot.generation.wrapping_add(1);
            }
        }
    }
}

/// How a file is open, across all its names: its writers and its holds.
/// What refuses a write to a disk's image is counted by the file -- its
/// mount and inode (`identity_key`) -- and not by the name it was opened
/// by, so that a second name, a hard link, does not get past the hold.
#[derive(Clone, Copy, Default)]
struct Opens {
    writers: usize,
    holds: usize,
}

/// Marks a node's key apart from an inode number (`identity_key`).
const NODE_KEY: u64 = 1 << 63;

/// Which file `node` of `fs` is: its on-disk inode, so that two names of
/// one file are one -- or, on a filesystem that keeps none, the node.
fn identity_key(fs: &dyn FileSystem, node: NodeId) -> u64 {
    match fs.tree().get(node).map(|n| n.ino) {
        Some(ino) if ino != 0 => ino as u64,
        _ => NODE_KEY | node.key(),
    }
}

pub struct Vfs {
    admin: Mutex<Admin>,
    table: Mutex<Table>,
    /// Each mount under its own lock, at its entry's index in the table
    mounts: Box<[Mutex<MountSlot>; MAX_MOUNTS]>,
}

/// What a mount's claim on its device says to whoever is refused it.
const MOUNT_HOLDER: &core::ffi::CStr = c"a mounted filesystem";

/// What a path came to, on its mount.
struct Resolved {
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
        let mut mounts = Vec::new();
        mounts.try_reserve_exact(MAX_MOUNTS).ok()?;
        for index in 0..MAX_MOUNTS {
            mounts.push(Mutex::new(MountSlot { index, files: Files { slots: Vec::new() }, mount: None })?);
        }
        Some(Box::new(Vfs {
            admin: Mutex::new(Admin { next_id: 1 })?,
            table: Mutex::new(Table { entries: [const { None }; MAX_MOUNTS] })?,
            mounts: mounts.into_boxed_slice().try_into().ok()?,
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

        /* Mounts and unmounts one at a time: what is found of the table here
         * stays so until this is done -- the filesystem's own mount, which
         * may replay a journal, among it -- while calls on every mount there
         * is go on. */
        let mut admin = self.admin.lock();

        let device = fs.device();

        let index = {
            let table = self.table.lock();
            for entry in table.entries.iter().flatten() {
                if entry.path() == path {
                    trace!(0, "vfs: something is mounted on that path already");
                    return None;
                }
                if device != 0 && entry.device == device {
                    trace!(0, "vfs: that device is mounted already");
                    return None;
                }
            }
            match table.entries.iter().position(Option::is_none) {
                Some(index) => index,
                None => {
                    trace!(0, "vfs: {} mounts is all there is room for", MAX_MOUNTS);
                    return None;
                }
            }
        };

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

        let id = admin.next_id;
        admin.next_id += 1;
        let mut entry = Entry { path: [0; MAX_PATH], path_len: path.len(), id, device };
        entry.path[..path.len()].copy_from_slice(path);

        /* The filesystem in its slot before its path is in the table: whoever
         * finds the path finds the mount. */
        let mut slot = self.mounts[index].lock();
        assert!(slot.mount.is_none(), "vfs: a slot the table has no entry for holds a mount");
        slot.mount = Some(Mount { id, fs, read_only, claim, open_files: 0, opens: BTreeMap::new() });
        self.table.lock().entries[index] = Some(entry);
        Some(read_only)
    }

    /// Take a filesystem off its mount point and release it. False if it is
    /// not mounted there, or is busy.
    pub fn unmount(&self, path: &[u8]) -> bool {
        let _admin = self.admin.lock();

        let found = {
            let table = self.table.lock();
            table.entries.iter().enumerate().find_map(|(index, entry)| {
                entry.as_ref().filter(|entry| entry.path() == path).map(|entry| (index, entry.id))
            })
        };
        let Some((index, id)) = found else {
            trace!(0, "vfs: nothing is mounted there");
            return false;
        };

        let mut slot = self.lock_mounted(index, id);
        let open_files = slot.mount.as_ref().map_or(0, |mount| mount.open_files);
        if open_files != 0 {
            trace!(0, "vfs: the mount is busy, {} files open", open_files);
            return false;
        }

        let mount = self.take_mount(&mut slot);
        drop(slot);
        if let Some(mount) = mount {
            mount.take_down();
        }
        true
    }

    /// Take everything down, deepest mount first, releasing each filesystem.
    /// This is shutdown: a handle left open is abandoned, not honoured.
    pub fn unmount_all(&self) {
        let _admin = self.admin.lock();

        loop {
            /* Deepest first, so a mount inside another goes before it. */
            let found = {
                let table = self.table.lock();
                let mut deepest: Option<(usize, &Entry)> = None;
                for (index, entry) in table.entries.iter().enumerate() {
                    let Some(entry) = entry else { continue };
                    if deepest.is_none_or(|(_, longest)| entry.path_len >= longest.path_len) {
                        deepest = Some((index, entry));
                    }
                }
                deepest.map(|(index, entry)| (index, entry.id, entry.path, entry.path_len))
            };
            let Some((index, id, path, path_len)) = found else { return };

            let mut slot = self.lock_mounted(index, id);
            let mount = self.take_mount(&mut slot);
            drop(slot);
            let Some(mount) = mount else { continue };

            trace!(0, "vfs: unmounting {}", core::str::from_utf8(&path[..path_len]).unwrap_or("?"));
            if mount.open_files != 0 {
                trace!(0, "vfs: unmounting with {} files still open", mount.open_files);
            }
            mount.take_down();
        }
    }

    /// The slot at `index`, locked, holding the mount `id` its entry names:
    /// under `admin`, which alone changes either, it holds what the table
    /// says.
    fn lock_mounted(&self, index: usize, id: u64) -> MutexGuard<'_, MountSlot> {
        let slot = self.mounts[index].lock();
        assert!(slot.mount.as_ref().is_some_and(|mount| mount.id == id),
                "vfs: the table names a mount its slot does not hold");
        slot
    }

    /// The mount in `slot` out of the table and out of the slot, the files
    /// open on it abandoned: no path finds it from here on, and no handle.
    /// The caller holds `admin`, and the slot's lock.
    fn take_mount(&self, slot: &mut MountSlot) -> Option<Mount> {
        self.table.lock().entries[slot.index] = None;
        slot.files.abandon();
        slot.mount.take()
    }

    /// Which mount a path is on -- the one the VFS resolves it through --
    /// as an identity no other mount has had or will have: for a walk that
    /// keeps to one filesystem (`find -xdev`).
    pub fn mount_id(&self, path: &[u8]) -> Option<u64> {
        let table = self.table.lock();
        let (index, _) = Self::find_mount(&table, path)?;
        Some(table.entries[index].as_ref()?.id)
    }

    /// The space of the filesystem a path is on -- the one the VFS resolves
    /// it through (`FileSystem::space`): what a file written there may take,
    /// whether the path is there yet or not. None for a path no mount holds,
    /// or a filesystem that does not say.
    pub fn space_of(&self, path: &[u8]) -> Option<Space> {
        let (slot, _) = self.locate_path(path)?;
        slot.mount.as_ref()?.fs.space()
    }

    pub fn mount_count(&self) -> usize {
        self.table.lock().entries.iter().flatten().count()
    }

    /// The mounts in the order they were taken -- each one's slot and id,
    /// the empty slots last -- as the table has them at one moment, to be
    /// walked without it.
    fn mounted(&self) -> [Option<(usize, u64)>; MAX_MOUNTS] {
        let mut order: [Option<(usize, u64)>; MAX_MOUNTS] = {
            let table = self.table.lock();
            core::array::from_fn(|index| table.entries[index].as_ref().map(|entry| (index, entry.id)))
        };
        /* Every mount is given a higher id than the one before */
        order.sort_unstable_by_key(|mounted| mounted.map_or(u64::MAX, |(_, id)| id));
        order
    }

    /// What the index'th mount is, for `mounts` to print.
    pub fn mount_info(&self, index: usize) -> Option<MountInfo> {
        let (slot, id) = (*self.mounted().get(index)?)?;
        self.info_of(slot, id)
    }

    /// Every mount, as `mount_info` has each: those the table had at one
    /// moment, in the order they were taken, each copied under its own lock
    /// -- one taken down meanwhile left out, none seen twice.
    pub fn mount_infos(&self) -> [Option<MountInfo>; MAX_MOUNTS] {
        let mut infos: [Option<MountInfo>; MAX_MOUNTS] = [const { None }; MAX_MOUNTS];
        let mut at = 0;
        for (slot, id) in self.mounted().into_iter().flatten() {
            if let Some(info) = self.info_of(slot, id) {
                infos[at] = Some(info);
                at += 1;
            }
        }
        infos
    }

    /// What the mount in slot `index` is, while it is still the mount `id`:
    /// None for one taken down since.
    fn info_of(&self, index: usize, id: u64) -> Option<MountInfo> {
        let slot = self.mounts.get(index)?.lock();
        let mount = slot.mount.as_ref().filter(|mount| mount.id == id)?;
        let mut info = MountInfo {
            path: [0; MAX_PATH],
            path_len: 0,
            fs_name: mount.fs.name(),
            info: Line { text: [0; INFO_MAX], len: 0 },
            read_only: mount.read_only,
            device: mount.fs.device(),
            space: mount.fs.space(),
        };
        {
            /* Its entry: in the table for as long as the slot holds it */
            let table = self.table.lock();
            let entry = table.entries[index].as_ref()?;
            info.path = entry.path;
            info.path_len = entry.path_len;
        }
        mount.fs.info(&mut info.info);
        Some(info)
    }

    /* ---- resolution ---- */

    /// The mount a path is on: the slot of the longest mount path it starts
    /// with, and what is left of it after that.
    fn find_mount<'p>(table: &Table, path: &'p [u8]) -> Option<(usize, &'p [u8])> {
        let mut best: Option<(usize, usize)> = None;

        for (index, entry) in table.entries.iter().enumerate() {
            let Some(entry) = entry else { continue };
            let mpath = entry.path();
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

    /// The slot `pick` finds in the table, locked, and what else it found
    /// there -- as long as the slot still holds the mount the table named.
    /// One taken down between the two has `pick` ask the table again, as it
    /// is then, which no longer has it: each time round follows an unmount
    /// that went through, so the walk ends. None when `pick` finds nothing.
    fn locate<R>(&self, mut pick: impl FnMut(&Table) -> Option<(usize, R)>)
        -> Option<(MutexGuard<'_, MountSlot>, R)>
    {
        loop {
            let (index, id, found) = {
                let table = self.table.lock();
                let (index, found) = pick(&table)?;
                (index, table.entries.get(index)?.as_ref()?.id, found)
            };
            let slot = self.mounts.get(index)?.lock();
            if slot.mount.as_ref().is_some_and(|mount| mount.id == id) {
                return Some((slot, found));
            }
        }
    }

    /// The mount `path` is on, locked, and what is left of the path past
    /// where it is mounted.
    fn locate_path<'p>(&self, path: &'p [u8]) -> Option<(MutexGuard<'_, MountSlot>, &'p [u8])> {
        self.locate(|table| Self::find_mount(table, path))
    }

    /// The mount `path` is on, locked, and the path walked there to what it
    /// names.
    fn lock_resolved(&self, path: &[u8]) -> Option<(MutexGuard<'_, MountSlot>, Resolved)> {
        let Some((mut slot, rest)) = self.locate_path(path) else {
            trace!(0, "vfs: no mount holds that path");
            return None;
        };
        let resolved = Self::resolve(slot.mount.as_mut()?, rest)?;
        Some((slot, resolved))
    }

    /// The slot of the mount the open file `handle` is on, locked: the one
    /// lock a call on an open file takes.
    fn lock_handle(&self, handle: Handle) -> Option<MutexGuard<'_, MountSlot>> {
        Some(self.mounts.get(handle.mount_slot())?.lock())
    }

    /// Walk what is left of a path past its mount point to what it names.
    fn resolve(mount: &mut Mount, rest: &[u8]) -> Option<Resolved> {
        let fs = &mut mount.fs;
        let mut at = fs.root()?;

        let mut resolved = Resolved {
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
            /* No filesystem here keeps one: ext2's format forbids it, and
             * nanofs's names end at the first. */
            if component.contains(&0) {
                trace!(0, "vfs: a name in that path has a NUL in it");
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
        let (slot, resolved) = self.lock_resolved(path)?;
        let node = slot.mount.as_ref()?.fs.tree().get(resolved.node?)?;

        Some(FileStat {
            kind: node.kind,
            size: if node.is_file() { node.size } else { 0 },
            ino: node.ino,
        })
    }

    /// The index'th entry of the directory at `path`; `None` past the last,
    /// and for anything that is not a directory it can read.
    pub fn read_dir(&self, path: &[u8], index: usize) -> Option<DirEntry> {
        self.dir_entry(path, index).ok().flatten()
    }

    /// The index'th entry of the directory at `path`: `Ok(None)` past the
    /// last, and an error that says why when there is no directory to read
    /// -- so that one the filesystem cannot read is told from an empty one.
    pub fn dir_entry(&self, path: &[u8], index: usize) -> Result<Option<DirEntry>, DirError> {
        let (mut slot, resolved) = self.lock_resolved(path).ok_or(DirError::NotFound)?;
        let dir = resolved.node.ok_or(DirError::NotFound)?;
        let fs = &mut slot.mount.as_mut().ok_or(DirError::NotFound)?.fs;

        if !fs.tree().get(dir).ok_or(DirError::NotFound)?.is_dir() {
            return Err(DirError::NotDir);
        }
        if !fs.load_dir(dir) {
            return Err(DirError::Unreadable);
        }

        let tree = fs.tree();
        let Some(child) = tree.children(dir).nth(index).and_then(|child| tree.get(child)) else {
            return Ok(None);
        };

        let mut entry = DirEntry {
            name: [0; NAME_MAX],
            name_len: child.name().len(),
            kind: child.kind,
            size: if child.is_file() { child.size } else { 0 },
        };
        entry.name[..entry.name_len].copy_from_slice(child.name());
        Ok(Some(entry))
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

        let writes = flags & (OPEN_WRITE | OPEN_CREATE | OPEN_TRUNCATE) != 0;

        let (mut guard, resolved) = self.lock_resolved(path)?;
        let slot = &mut *guard;
        let mount = slot.mount.as_mut()?;

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

        let key = identity_key(&*mount.fs, node);
        let opens = mount.opens(key);
        let size = match mount.fs.tree().get(node) {
            Some(found) if found.is_file() => {
                /* A disk's image is its holder's alone to write -- and only
                 * what nobody else writes is held: counted by the file, so
                 * that another name of it is the same file. */
                if opens.holds != 0 && writes {
                    trace!(0, "vfs: that file is a disk's image, held while it is open");
                    return None;
                }
                if flags & OPEN_HOLD != 0 && opens.writers != 0 {
                    trace!(0, "vfs: that file is open for writing, and cannot be held as a disk's image");
                    return None;
                }
                found.size
            }
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

        let handle = slot.files.add(slot.index, OpenFile { node, pos, flags })?;

        if let Some(found) = mount.fs.tree_mut().get_mut(node) {
            found.open_count += 1;
            if flags & OPEN_WRITE != 0 {
                found.writers += 1;
            }
            if flags & OPEN_HOLD != 0 {
                found.holds += 1;
            }
        }
        mount.open_files += 1;
        let counted = mount.opens.entry(key).or_default();
        if flags & OPEN_WRITE != 0 {
            counted.writers += 1;
        }
        if flags & OPEN_HOLD != 0 {
            counted.holds += 1;
        }
        Some(handle)
    }

    pub fn close(&self, handle: Handle) {
        self.close_as(handle, false);
    }

    /// Lets go of a file held as a disk's image: what its image does as it
    /// goes, and nothing else may (`close`).
    pub(crate) fn release(&self, handle: Handle) {
        self.close_as(handle, true);
    }

    fn close_as(&self, handle: Handle, image: bool) {
        let Some(mut guard) = self.lock_handle(handle) else { return };
        let slot = &mut *guard;

        /* A held file is let go of by its image alone: a word that names its
         * handle -- guessed, or one a caller of the file ABI got wrong --
         * must not take the hold away from under the reads and writes the
         * image goes on making off this lock. */
        match slot.files.get(handle) {
            Some(file) if file.flags & OPEN_HOLD != 0 && !image => {
                trace!(0, "vfs: a disk's image is let go of by its image, not closed");
                return;
            }
            Some(_) => {}
            None => return,
        }
        let Some(file) = slot.files.take(handle) else { return };
        let Some(mount) = slot.mount.as_mut() else { return };

        let key = identity_key(&*mount.fs, file.node);
        if let Some(node) = mount.fs.tree_mut().get_mut(file.node) {
            node.open_count = node.open_count.saturating_sub(1);
            if file.flags & OPEN_WRITE != 0 {
                node.writers = node.writers.saturating_sub(1);
            }
            if file.flags & OPEN_HOLD != 0 {
                node.holds = node.holds.saturating_sub(1);
            }
        }
        mount.open_files = mount.open_files.saturating_sub(1);
        if let Some(counted) = mount.opens.get_mut(&key) {
            if file.flags & OPEN_WRITE != 0 {
                counted.writers = counted.writers.saturating_sub(1);
            }
            if file.flags & OPEN_HOLD != 0 {
                counted.holds = counted.holds.saturating_sub(1);
            }
            if counted.writers == 0 && counted.holds == 0 {
                mount.opens.remove(&key);
            }
        }
    }

    /// What was read into `buf`: 0 at the end of the file, None on error.
    pub fn read(&self, handle: Handle, buf: &mut [u8]) -> Option<usize> {
        let mut guard = self.lock_handle(handle)?;
        let slot = &mut *guard;

        let (node, pos, flags) = {
            let file = slot.files.get(handle)?;
            (file.node, file.pos, file.flags)
        };
        if flags & OPEN_READ == 0 {
            trace!(0, "vfs: that handle is not open for reading");
            return None;
        }

        let fs = &mut slot.mount.as_mut()?.fs;
        let size = fs.tree().get(node)?.size;
        if pos >= size || buf.is_empty() {
            return Some(0);
        }

        let take = buf.len().min(size - pos);
        if !fs.read(node, &mut buf[..take], pos) {
            return None;
        }

        slot.files.get(handle)?.pos = pos + take;
        Some(take)
    }

    pub fn write(&self, handle: Handle, data: &[u8]) -> bool {
        self.write_at(handle, data).is_some()
    }

    fn write_at(&self, handle: Handle, data: &[u8]) -> Option<()> {
        let mut guard = self.lock_handle(handle)?;
        let slot = &mut *guard;

        let (node, pos, flags) = {
            let file = slot.files.get(handle)?;
            (file.node, file.pos, file.flags)
        };
        if flags & OPEN_WRITE == 0 {
            trace!(0, "vfs: that handle is not open for writing");
            return None;
        }
        /* A held file never grows: its holder writes within it. */
        if flags & OPEN_HOLD != 0 {
            trace!(0, "vfs: a disk's image is written within its size");
            return None;
        }
        if data.is_empty() {
            return Some(());
        }

        let fs = &mut slot.mount.as_mut()?.fs;
        let pos = if flags & OPEN_APPEND != 0 { fs.tree().get(node)?.size } else { pos };
        let end = pos.checked_add(data.len())?;

        if !fs.write(node, data, pos) {
            return None;
        }

        slot.files.get(handle)?.pos = end;
        Some(())
    }

    /// As much of `buf` as the file has from `offset`: the count read, 0 at
    /// or past its end, None on an error. The handle's position is not
    /// moved, so two callers of one handle do not race on it.
    pub fn read_at(&self, handle: Handle, offset: usize, buf: &mut [u8]) -> Option<usize> {
        let mut guard = self.lock_handle(handle)?;
        let slot = &mut *guard;

        let (node, flags) = {
            let file = slot.files.get(handle)?;
            (file.node, file.flags)
        };
        if flags & OPEN_READ == 0 {
            trace!(0, "vfs: that handle is not open for reading");
            return None;
        }

        let fs = &mut slot.mount.as_mut()?.fs;
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
        let Some(mut guard) = self.lock_handle(handle) else { return false };
        let slot = &mut *guard;

        let (node, flags) = match slot.files.get(handle) {
            Some(file) => (file.node, file.flags),
            None => return false,
        };
        if flags & OPEN_WRITE == 0 {
            trace!(0, "vfs: that handle is not open for writing");
            return false;
        }

        let Some(fs) = slot.mount.as_mut().map(|mount| &mut mount.fs) else {
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

    /// The file cut to `size` bytes, or grown to it -- with a hole, where
    /// the filesystem has them -- through a handle open for writing: a disk
    /// image made the size of its disk before anything is written into it.
    /// The handle's position is not moved.
    pub fn set_length(&self, handle: Handle, size: usize) -> bool {
        let Some(mut guard) = self.lock_handle(handle) else { return false };
        let slot = &mut *guard;

        let (node, flags) = match slot.files.get(handle) {
            Some(file) => (file.node, file.flags),
            None => return false,
        };
        if flags & OPEN_WRITE == 0 {
            trace!(0, "vfs: that handle is not open for writing");
            return false;
        }
        let Some(mount) = slot.mount.as_mut() else { return false };
        let held = mount.held(node);
        let Some(mount) = mount.writable() else { return false };
        match mount.fs.tree().get(node) {
            Some(found) if found.is_file() && !held => {}
            Some(found) if found.is_file() => {
                trace!(0, "vfs: that file is a disk's image, its size held while it is open");
                return false;
            }
            _ => return false,
        }
        mount.fs.truncate(node, size)
    }

    /// The device the blocks of the file `handle` holds (`OPEN_HOLD`) are on,
    /// and how big one is: what its image reads and writes off the lock --
    /// or None where its filesystem keeps no blocks of its own on a device,
    /// or the handle holds nothing.
    pub fn geometry(&self, handle: Handle) -> Option<(usize, usize)> {
        let mut guard = self.lock_handle(handle)?;
        let slot = &mut *guard;

        let flags = slot.files.get(handle)?.flags;
        if flags & OPEN_HOLD == 0 {
            return None;
        }
        let fs = &slot.mount.as_ref()?.fs;
        match (fs.device(), fs.data_block_size()) {
            (0, _) | (_, 0) => None,
            (device, size) => Some((device, size)),
        }
    }

    /// Where `out.len()` blocks of the file `handle` holds, from block
    /// `first`, are on its device: each one's first sector, 0 for a hole
    /// (`FileSystem::map_blocks`). Only a held file is mapped, and only
    /// blocks inside it: what the caller then reads and writes there, off
    /// this lock, stays the file's -- and nobody else's to write -- for as
    /// long as the hold does.
    pub fn map(&self, handle: Handle, first: u64, out: &mut [u64]) -> bool {
        let Some(mut guard) = self.lock_handle(handle) else { return false };
        let slot = &mut *guard;

        let (node, flags) = match slot.files.get(handle) {
            Some(file) => (file.node, file.flags),
            None => return false,
        };
        if flags & OPEN_HOLD == 0 {
            trace!(0, "vfs: only a file held as a disk's image is mapped");
            return false;
        }
        let Some(mount) = slot.mount.as_mut() else { return false };
        let size = match mount.fs.tree().get(node) {
            Some(found) => found.size as u64,
            None => return false,
        };
        let block = mount.fs.data_block_size() as u64;
        if block == 0 {
            return false;
        }
        match first.checked_add(out.len() as u64) {
            Some(end) if end <= size.div_ceil(block) => mount.fs.map_blocks(node, first, out),
            _ => false,
        }
    }

    /// The whole blocks inside `len` bytes from `offset` of the file `handle`
    /// holds for writing given back to its filesystem (`FileSystem::punch`):
    /// holes, reading as zeros, the file's size as it was -- a guest's
    /// discard of its image. What the range covers of a block only in part
    /// is left as it is. How many blocks were given back, or None. The caller
    /// -- the image -- has no read of its own under way off this lock on
    /// what it gives back: the next file may have those blocks.
    pub fn punch(&self, handle: Handle, offset: u64, len: u64) -> Option<u64> {
        let mut guard = self.lock_handle(handle)?;
        let slot = &mut *guard;

        let (node, flags) = {
            let file = slot.files.get(handle)?;
            (file.node, file.flags)
        };
        if flags & (OPEN_HOLD | OPEN_WRITE) != OPEN_HOLD | OPEN_WRITE {
            trace!(0, "vfs: only a file held for writing as a disk's image is punched");
            return None;
        }
        let mount = slot.mount.as_mut()?;
        if mount.read_only {
            trace!(0, "vfs: that mount is read-only");
            return None;
        }
        let block = mount.fs.data_block_size() as u64;
        if block == 0 {
            return None;
        }
        let first = offset.div_ceil(block);
        let end = offset.checked_add(len)? / block;
        if first >= end {
            return Some(0);
        }
        mount.fs.punch(node, first, end - first)
    }

    /// The filesystem the file is on, synced: everything written to it on
    /// its disk's medium.
    pub fn sync_file(&self, handle: Handle) -> bool {
        let Some(mut guard) = self.lock_handle(handle) else { return false };
        let slot = &mut *guard;

        if slot.files.get(handle).is_none() {
            return false;
        }
        slot.mount.as_mut().is_some_and(|mount| mount.fs.sync())
    }

    pub fn seek(&self, handle: Handle, pos: usize) -> bool {
        let Some(mut slot) = self.lock_handle(handle) else { return false };
        match slot.files.get(handle) {
            Some(file) => {
                file.pos = pos;
                true
            }
            None => false,
        }
    }

    pub fn tell(&self, handle: Handle) -> usize {
        let Some(mut slot) = self.lock_handle(handle) else { return 0 };
        slot.files.get(handle).map_or(0, |file| file.pos)
    }

    pub fn size(&self, handle: Handle) -> usize {
        let Some(mut guard) = self.lock_handle(handle) else { return 0 };
        let slot = &mut *guard;

        let Some(node) = slot.files.get(handle).map(|file| file.node) else { return 0 };
        slot.mount.as_ref()
            .and_then(|mount| mount.fs.tree().get(node))
            .map_or(0, |node| node.size)
    }

    /// Which file the open file is: its mount -- whose id no other mount is
    /// ever given, and never 0 -- and on it the file's on-disk inode, so
    /// that two names of one file are one; or, on a filesystem that keeps
    /// none, its node. Neither is ever 0.
    pub fn identity(&self, handle: Handle) -> Option<(u64, u64)> {
        let mut guard = self.lock_handle(handle)?;
        let slot = &mut *guard;

        let node = slot.files.get(handle)?.node;
        let mount = slot.mount.as_ref()?;
        mount.fs.tree().get(node)?;
        Some((mount.id, identity_key(&*mount.fs, node)))
    }

    /// The open file's size, or None for a handle that is no open file's.
    pub fn length(&self, handle: Handle) -> Option<usize> {
        let mut guard = self.lock_handle(handle)?;
        let slot = &mut *guard;

        let node = slot.files.get(handle)?.node;
        Some(slot.mount.as_ref()?.fs.tree().get(node)?.size)
    }

    pub fn create(&self, path: &[u8], directory: bool) -> bool {
        self.try_create(path, directory).is_some()
    }

    fn try_create(&self, path: &[u8], directory: bool) -> Option<NodeId> {
        let (mut slot, resolved) = self.lock_resolved(path)?;
        let mount = slot.mount.as_mut()?.writable()?;

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
        self.try_remove(path).is_some()
    }

    fn try_remove(&self, path: &[u8]) -> Option<()> {
        let (mut slot, resolved) = self.lock_resolved(path)?;
        let node = resolved.node?;
        let mount = slot.mount.as_mut()?.writable()?;

        if mount.fs.tree().get(node)?.is_dir() && !mount.fs.load_dir(node) {
            return None;
        }
        if mount.fs.tree().has_open_files(node) {
            trace!(0, "vfs: something under that path is open");
            return None;
        }
        /* A file held by another name, whose link count says this name is
         * its last: given back, its blocks would go to the next file while
         * its image went on writing them off this lock. A sound filesystem
         * counts every name; one that counts fewer is no reason to let a
         * guest's disk become another file. */
        let held = mount.held_under(node);
        if held.iter().any(|&file| mount.fs.links(file).is_some_and(|n| n <= 1)) {
            trace!(0, "vfs: under that path is the last name of a disk's image held by another, while it is open");
            return None;
        }
        mount.fs.remove(node).then_some(())
    }

    pub fn truncate(&self, path: &[u8], size: usize) -> bool {
        self.try_truncate(path, size).is_some()
    }

    /// `FileSystem::sparse_max` of the filesystem `path` is on: 0 when no
    /// mount holds it.
    pub fn sparse_max(&self, path: &[u8]) -> usize {
        match self.locate_path(path) {
            Some((slot, _)) => slot.mount.as_ref().map_or(0, |mount| mount.fs.sparse_max()),
            None => 0,
        }
    }

    fn try_truncate(&self, path: &[u8], size: usize) -> Option<()> {
        let (mut slot, resolved) = self.lock_resolved(path)?;
        let node = resolved.node?;
        let mount = slot.mount.as_mut()?;
        let held = mount.held(node);
        let mount = mount.writable()?;

        let found = mount.fs.tree().get(node)?;
        if !found.is_file() {
            return None;
        }
        /* Cut under a disk's image -- by any name of it -- the blocks its
         * map named would go back to the filesystem, and to the next file,
         * while the image still wrote them. */
        if held {
            trace!(0, "vfs: that file is a disk's image, held while it is open");
            return None;
        }
        mount.fs.truncate(node, size).then_some(())
    }

    pub fn rename(&self, from: &[u8], to: &[u8]) -> bool {
        self.try_rename(from, to).is_some()
    }

    fn try_rename(&self, from: &[u8], to: &[u8]) -> Option<()> {
        /* Both paths found in the table as it is at one moment: on one mount,
         * or no rename. */
        let mut across = false;
        let found = self.locate(|table| {
            let (from_mount, from_rest) = Self::find_mount(table, from)?;
            let (to_mount, to_rest) = Self::find_mount(table, to)?;
            across = from_mount != to_mount;
            (!across).then_some((from_mount, (from_rest, to_rest)))
        });
        if across {
            trace!(0, "vfs: a rename across two mounts is not a rename");
            return None;
        }
        let (mut slot, (from_rest, to_rest)) = found?;
        let mount = slot.mount.as_mut()?;

        let source = Self::resolve(mount, from_rest)?;
        let node = source.node?;
        {
            let mount = mount.writable()?;

            if mount.fs.tree().parent(node).is_none() {
                trace!(0, "vfs: the root of a mount cannot be renamed");
                return None;
            }
            if mount.fs.tree().get(node)?.is_dir() && !mount.fs.load_dir(node) {
                return None;
            }
            if mount.fs.tree().has_open_files(node) {
                trace!(0, "vfs: something under that path is open");
                return None;
            }
        }

        let target = Self::resolve(mount, to_rest)?;
        if target.node.is_some() {
            trace!(0, "vfs: the new path exists already");
            return None;
        }
        let (new_parent, new_name) = target.new_entry()?;

        /* A directory cannot be moved inside itself. */
        if mount.fs.tree().is_ancestor(node, new_parent) {
            trace!(0, "vfs: that would move a directory inside itself");
            return None;
        }

        mount.fs.rename(node, new_parent, new_name).then_some(())
    }

    /// Every filesystem synced, a mount at a time.
    pub fn sync(&self) -> bool {
        let mut ok = true;
        for slot in self.mounts.iter() {
            let mut slot = slot.lock();
            if let Some(mount) = slot.mount.as_mut() {
                if !mount.fs.sync() {
                    ok = false;
                }
            }
        }
        ok
    }

    /// Replace a file's contents, creating it if it is missing.
    pub fn write_file(&self, path: &[u8], data: &[u8]) -> bool {
        self.try_write_file(path, data).is_some()
    }

    fn try_write_file(&self, path: &[u8], data: &[u8]) -> Option<()> {
        let (mut slot, resolved) = self.lock_resolved(path)?;
        let mount = slot.mount.as_mut()?;
        let held = resolved.node.is_some_and(|node| mount.held(node));
        let fs = &mut mount.writable()?.fs;

        let node = match resolved.node {
            Some(node) => node,
            None => {
                let (parent, name) = resolved.new_entry()?;
                fs.create_file(parent, name)?
            }
        };

        let size = match fs.tree().get(node) {
            /* A disk's image is its holder's alone to write (`OPEN_HOLD`):
             * cut to nothing under it, its blocks would go to the next file
             * while the image still wrote them. */
            Some(found) if found.is_file() && held => {
                trace!(0, "vfs: that file is a disk's image, held while it is open");
                return None;
            }
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
