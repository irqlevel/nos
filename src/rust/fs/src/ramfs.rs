//! ramfs: a filesystem that is only ever in memory.
//!
//! What the ISO boots on when there is no root on disk, and what `/tmp` and
//! the boot self-test use. A file is one growing buffer, a directory is a
//! list of children, and nothing survives a reboot.
//!
//! Every call arrives with the VFS lock held (see `FileSystem`), so there is
//! no locking here.

use alloc::boxed::Box;

use kcore::trace;

use crate::vfs::FileSystem;
use crate::vnode::{Kind, NodeId, Tree, NAME_MAX};

pub struct RamFs {
    tree: Tree,
    root: Option<NodeId>,
}

impl RamFs {
    pub fn new() -> Option<RamFs> {
        let mut tree = Tree::new();
        let root = match tree.alloc(b"/", Kind::Dir) {
            Some(root) => root,
            None => {
                trace!(0, "ramfs: no memory for the root");
                return None;
            }
        };
        /* In-memory directories are born complete */
        tree[root].dir_loaded = true;
        Some(RamFs { tree, root: Some(root) })
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn root(&self) -> Option<NodeId> {
        self.root
    }

    pub fn unmount(&mut self) {
        self.tree.clear();
        self.root = None;
    }

    pub fn lookup(&self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        if !self.tree.get(dir)?.is_dir() {
            return None;
        }
        self.tree.find_child(dir, name)
    }

    fn create(&mut self, dir: NodeId, name: &[u8], kind: Kind) -> Option<NodeId> {
        if name.is_empty() {
            trace!(0, "ramfs: something made with no name");
            return None;
        }
        if !self.tree.get(dir)?.is_dir() {
            trace!(0, "ramfs: something made somewhere that is not a directory");
            return None;
        }
        if self.lookup(dir, name).is_some() {
            trace!(0, "ramfs: there is something by that name already");
            return None;
        }

        let node = match self.tree.alloc(name, kind) {
            Some(node) => node,
            None => {
                trace!(0, "ramfs: no memory for a node");
                return None;
            }
        };
        if kind == Kind::Dir {
            self.tree[node].dir_loaded = true;
        }
        self.tree.insert_child(dir, node);
        Some(node)
    }

    pub fn create_file(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        self.create(dir, name, Kind::File)
    }

    pub fn create_dir(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        self.create(dir, name, Kind::Dir)
    }

    /// The file's buffer made `size` bytes long, keeping what it holds and
    /// reading as zeros where it grew. The growth is amortized, so a file
    /// written a line at a time costs a logarithmic number of copies rather
    /// than one per write.
    fn resize(&mut self, file: NodeId, size: usize) -> bool {
        let node = &mut self.tree[file];
        let len = node.data.len();
        if size > len && node.data.try_reserve(size - len).is_err() {
            trace!(0, "ramfs: no memory for {} bytes", size);
            return false;
        }
        node.data.resize(size, 0);
        node.size = size;
        true
    }

    pub fn read(&self, file: NodeId, buf: &mut [u8], offset: usize) -> bool {
        let node = match self.tree.get(file) {
            Some(node) if node.is_file() => node,
            _ => {
                trace!(0, "ramfs: a read of something that is not a file");
                return false;
            }
        };

        if offset >= node.data.len() {
            trace!(0, "ramfs: a read at {} is past the {} bytes there are",
                offset, node.data.len());
            return false;
        }

        let take = buf.len().min(node.data.len() - offset);
        buf[..take].copy_from_slice(&node.data[offset..offset + take]);
        true
    }

    pub fn write(&mut self, file: NodeId, data: &[u8], offset: usize) -> bool {
        let size = match self.tree.get(file) {
            Some(node) if node.is_file() => node.data.len(),
            _ => {
                trace!(0, "ramfs: a write to something that is not a file");
                return false;
            }
        };
        if data.is_empty() {
            return true;
        }

        let end = match offset.checked_add(data.len()) {
            Some(end) => end,
            None => {
                trace!(0, "ramfs: a write of {} bytes at {} is past memory",
                    data.len(), offset);
                return false;
            }
        };

        /* Writing past the end leaves a hole that reads as zeros */
        if end > size && !self.resize(file, end) {
            return false;
        }

        self.tree[file].data[offset..end].copy_from_slice(data);
        true
    }

    pub fn truncate(&mut self, file: NodeId, size: usize) -> bool {
        match self.tree.get(file) {
            Some(node) if node.is_file() => {}
            _ => {
                trace!(0, "ramfs: a truncate of something that is not a file");
                return false;
            }
        }
        self.resize(file, size)
    }

    pub fn rename(&mut self, node: NodeId, new_dir: NodeId, new_name: &[u8]) -> bool {
        if new_name.is_empty() || self.tree.get(node).is_none() {
            trace!(0, "ramfs: a rename of nothing");
            return false;
        }
        if self.tree.parent(node).is_none() {
            trace!(0, "ramfs: the root cannot be renamed");
            return false;
        }
        if !self.tree.get(new_dir).is_some_and(|dir| dir.is_dir()) {
            trace!(0, "ramfs: the target of a rename is not a directory");
            return false;
        }
        if new_name.len() >= NAME_MAX {
            trace!(0, "ramfs: a name of {} bytes is too long", new_name.len());
            return false;
        }
        if self.lookup(new_dir, new_name).is_some() {
            trace!(0, "ramfs: there is something by that name already");
            return false;
        }

        self.tree.rename(node, new_dir, new_name);
        true
    }

    pub fn remove(&mut self, node: NodeId) -> bool {
        if self.tree.get(node).is_none() {
            trace!(0, "ramfs: a remove of nothing");
            return false;
        }
        if self.tree.parent(node).is_none() {
            trace!(0, "ramfs: the root cannot be removed");
            return false;
        }

        /* Its buffer, and those of everything under it, go with the nodes */
        self.tree.free_tree(node);
        true
    }
}

impl FileSystem for RamFs {
    fn name(&self) -> &'static str {
        "ramfs"
    }

    fn mount(&mut self, read_only: bool) -> Option<bool> {
        Some(read_only)
    }

    fn unmount(&mut self) {
        RamFs::unmount(self);
    }

    fn tree(&self) -> &Tree {
        &self.tree
    }

    fn tree_mut(&mut self) -> &mut Tree {
        &mut self.tree
    }

    fn root(&self) -> Option<NodeId> {
        self.root
    }

    /* `load_dir` is the default: born complete, there is nowhere else the
     * entries could be. And `sync`: nowhere to push it to. */

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        RamFs::lookup(self, dir, name)
    }

    fn create_file(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        RamFs::create_file(self, dir, name)
    }

    fn create_dir(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        RamFs::create_dir(self, dir, name)
    }

    fn read(&mut self, file: NodeId, buf: &mut [u8], offset: usize) -> bool {
        RamFs::read(self, file, buf, offset)
    }

    fn write(&mut self, file: NodeId, data: &[u8], offset: usize) -> bool {
        RamFs::write(self, file, data, offset)
    }

    fn truncate(&mut self, file: NodeId, size: usize) -> bool {
        RamFs::truncate(self, file, size)
    }

    fn rename(&mut self, node: NodeId, dir: NodeId, name: &[u8]) -> bool {
        RamFs::rename(self, node, dir, name)
    }

    fn remove(&mut self, node: NodeId) -> bool {
        RamFs::remove(self, node)
    }
}

/* ---- what the kernel calls ---- */

/// Mount a fresh ramfs at `path`: 0 mounted, -1 not.
///
/// # Safety
/// `path` points at `path_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_ramfs_mount(
    path: *const u8, path_len: usize, read_only: i32,
) -> i32 {
    match unsafe { crate::path(path, path_len) } {
        Some(at) if mount_bytes(at, read_only != 0) => 0,
        _ => -1,
    }
}

/// A fresh ramfs at `path`.
pub fn mount_at(path: &str, read_only: bool) -> bool {
    mount_bytes(path.as_bytes(), read_only)
}

fn mount_bytes(at: &[u8], read_only: bool) -> bool {
    let (vfs, fs) = match (crate::vfs_instance(), RamFs::new()) {
        (Some(vfs), Some(fs)) => (vfs, fs),
        _ => return false,
    };
    vfs.mount(at, Box::new(fs), read_only).is_some()
}
