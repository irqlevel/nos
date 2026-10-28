//! The calls that change the tree -- create, rename, remove -- each one
//! transaction, and the tree in memory changed only once the journal holds
//! it: a call that fails leaves both as they were. A remove of a directory
//! with things in it is one transaction a node, deepest first, so that a
//! crash part way leaves what is left whole; and it starts only once every
//! directory in it is known to hold on the disk nothing the tree leaves
//! out, so that a refusal leaves everything.

use alloc::vec::Vec;

use extfmt::dir::{self, FT_DIR, FT_REG_FILE};
use extfmt::inode::{self as xinode, Inode, S_IFDIR, S_IFREG};
use extfmt::xattr;
use kcore::time::wall_clock_secs;
use kcore::trace;

use super::inodes;
use super::txn::{Kind, Txn};
use super::Ext4;
use crate::vnode::{Kind as VKind, NodeId, NAME_MAX};

/// A name a directory entry may have here: one the tree can hold, and not
/// one of the two every directory has.
fn usable_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() < NAME_MAX && name != b"." && name != b".." && !name.contains(&b'/')
        && !name.contains(&0)
}

impl Txn<'_> {
    /// A new file -- or directory, its first block holding `.` and `..` --
    /// named `name` in directory `dir`: its inode, written whole, and its
    /// entry. The new inode's number.
    fn make(&mut self, dir: u32, name: &[u8], is_dir: bool, generation: u32) -> Option<u32> {
        let ipg = self.m.ipg;
        let ino = self.alloc_inode(u64::from((dir - 1) / ipg), is_dir)?;
        let mode = if is_dir { S_IFDIR | 0o755 } else { S_IFREG | 0o644 };
        let mut raw = inodes::new_inode(self.m, mode, if is_dir { 2 } else { 1 }, generation, self.now);
        if is_dir {
            let iseed = xinode::seed(self.m.seed, ino, generation);
            let b = self.alloc_block(self.m.group_start(u64::from((ino - 1) / ipg)))?;
            let (bs, csum) = (self.m.bs, self.m.csum);
            let end = if csum { bs - dir::TAIL } else { bs };
            {
                let blk = self.fresh(b, Kind::Dir { seed: iseed, dx: false })?;
                dir::put_entry(blk, 0, ino, 12, b".", FT_DIR);
                dir::put_entry(blk, 12, dir, end - 12, b"..", FT_DIR);
                if csum {
                    dir::put_tail(blk);
                }
            }
            if !self.extent_append(ino, &mut raw, 0, b) {
                return None;
            }
            inodes::set_size(&mut raw, bs as u64);
            if !inodes::add_blocks(self.m, &mut raw, 1) {
                return None;
            }
        }
        if !self.put_inode(ino, &mut raw) || !self.add_entry(dir, name, ino, if is_dir { FT_DIR } else { FT_REG_FILE }) {
            return None;
        }
        if is_dir && !self.link_dir(dir, true) {
            return None;
        }
        Some(ino)
    }

    /// Inode `ino`'s status changed now.
    fn touch_inode(&mut self, ino: u32) -> bool {
        let Some(mut raw) = self.inode(ino) else { return false };
        inodes::touch(&mut raw, self.now, false);
        self.put_inode(ino, &mut raw)
    }

    /// The name `name` of `ino` taken out of directory `parent`: a
    /// directory -- which must be empty on the disk -- freed with it, a
    /// file freed with its last link.
    fn unlink(&mut self, parent: u32, name: &[u8], ino: u32, is_dir: bool) -> bool {
        if !self.remove_entry(parent, name, ino) {
            return false;
        }
        let Some(mut raw) = self.inode(ino) else { return false };
        let Some(node) = Inode::new(&raw) else { return false };
        if is_dir {
            if !node.is_dir() {
                return false;
            }
            match self.dir_empty(ino, &raw) {
                Some(true) => {}
                Some(false) => {
                    trace!(0, "ext4: directory {} holds entries on the disk the tree does not show: not removed", ino);
                    return false;
                }
                None => return false,
            }
            return self.link_dir(parent, false) && self.release(ino, &mut raw, true);
        }
        let links = inodes::links(&raw);
        if links > 1 {
            inodes::set_links(&mut raw, links - 1);
            inodes::touch(&mut raw, self.now, false);
            return self.put_inode(ino, &mut raw);
        }
        self.release(ino, &mut raw, false)
    }

    /// Inode `ino` (`raw`), named nowhere now, given back: every block it
    /// holds, its share of an attribute block, and the inode itself, left
    /// as the kernel leaves one it deletes.
    fn release(&mut self, ino: u32, raw: &mut [u8], is_dir: bool) -> bool {
        let Some((runs, tree)) = self.file_blocks(ino, raw) else { return false };
        for (start, len) in runs {
            for b in start..start + len {
                if !self.free_block(b) {
                    return false;
                }
            }
        }
        for b in tree {
            if !self.free_block(b) {
                return false;
            }
        }
        let acl = Inode::new(raw).map_or(0, |i| i.file_acl());
        if acl != 0 && !self.release_xattr(ino, acl) {
            return false;
        }
        inodes::delete(raw, self.now);
        self.put_inode(ino, raw) && self.free_inode(ino, is_dir)
    }

    /// Attribute block `b`, one inode fewer naming it: freed with the last.
    fn release_xattr(&mut self, ino: u32, b: u64) -> bool {
        if b < self.m.first_data || b >= self.m.blocks || self.m.in_system(b) {
            trace!(0, "ext4: inode {} names attribute block {}, which no file may hold", ino, b);
            return false;
        }
        let made = self.dirty.contains_key(&b);
        let Some(data) = self.read(b) else { return false };
        if xattr::magic(&data) != xattr::MAGIC
            || (self.m.csum && !made && xattr::block_csum(self.m.seed, b, &data) != Some(xattr::stored_csum(&data))) {
            trace!(0, "ext4: inode {}'s attribute block {} is damaged", ino, b);
            return false;
        }
        match xattr::refcount(&data) {
            0 => {
                trace!(0, "ext4: attribute block {} counts no references", b);
                false
            }
            1 => self.free_block(b),
            n => match self.modify(b, Kind::Xattr) {
                Some(blk) => {
                    xattr::set_refcount(blk, n - 1);
                    true
                }
                None => false,
            },
        }
    }
}

impl Ext4 {
    /// Whether a call may change the filesystem: mounted for writing, its
    /// journal not aborted.
    pub(super) fn writable(&self) -> bool {
        if !self.mounted || self.read_only {
            trace!(0, "ext4: read-only");
            return false;
        }
        if self.jnl.as_ref().is_none_or(|j| j.aborted) {
            trace!(0, "ext4: the journal is aborted: nothing more is written");
            return false;
        }
        true
    }

    /// A transaction, for one call.
    pub(super) fn begin(&mut self) -> Option<Txn<'_>> {
        let now = wall_clock_secs() as u32;
        Some(Txn::new(&mut self.io, self.m.as_mut()?, &mut self.cache, self.jnl.as_mut()?, now))
    }

    fn dir_ino(&self, dir: NodeId) -> Option<u32> {
        match self.tree.get(dir) {
            Some(node) if node.is_dir() => Some(node.ino as u32),
            _ => None,
        }
    }

    pub fn create(&mut self, dir: NodeId, name: &[u8], is_dir: bool) -> Option<NodeId> {
        if !self.writable() {
            return None;
        }
        let Some(dir_ino) = self.dir_ino(dir) else {
            trace!(0, "ext4: a create in something that is not a directory");
            return None;
        };
        if !usable_name(name) {
            trace!(0, "ext4: a name of {} bytes cannot be made", name.len());
            return None;
        }
        if self.lookup(dir, name).is_some() {
            trace!(0, "ext4: there is something by that name already");
            return None;
        }
        let Some(node) = self.tree.alloc(name, if is_dir { VKind::Dir } else { VKind::File }) else {
            trace!(0, "ext4: no memory for a vnode");
            return None;
        };
        let generation = kcore::random::random_u64().map_or(wall_clock_secs() as u32 ^ 0x9E37_79B9, |r| r as u32);
        let made = match self.begin() {
            Some(mut t) => match t.make(dir_ino, name, is_dir, generation) {
                Some(ino) => t.commit().then_some(ino),
                None => None,
            },
            None => None,
        };
        let Some(ino) = made else {
            self.tree.free_tree(node);
            return None;
        };
        self.tree[node].ino = ino as usize;
        self.tree.insert_child(dir, node);
        if is_dir {
            /* Made here, with nothing in it: there is nothing to load */
            self.tree[node].dir_loaded = true;
            self.hidden.insert(ino, 0);
        }
        Some(node)
    }

    pub fn rename(&mut self, node: NodeId, new_dir: NodeId, new_name: &[u8]) -> bool {
        if !self.writable() {
            return false;
        }
        let Some((ino, is_dir)) = self.tree.get(node).map(|n| (n.ino as u32, n.is_dir())) else { return false };
        let Some(old_dir) = self.tree.parent(node) else {
            trace!(0, "ext4: the root cannot be renamed");
            return false;
        };
        let (Some(old_ino), Some(new_ino)) = (self.dir_ino(old_dir), self.dir_ino(new_dir)) else {
            trace!(0, "ext4: the target of a rename is not a directory");
            return false;
        };
        if !usable_name(new_name) {
            trace!(0, "ext4: a name of {} bytes cannot be made", new_name.len());
            return false;
        }
        if self.lookup(new_dir, new_name).is_some() {
            trace!(0, "ext4: there is something by that name already");
            return false;
        }
        let old_name: Vec<u8> = self.tree[node].name().to_vec();
        let moved = old_dir != new_dir;
        let ft = if is_dir { FT_DIR } else { FT_REG_FILE };
        let done = match self.begin() {
            Some(mut t) => {
                let ok = t.add_entry(new_ino, new_name, ino, ft)
                    && t.remove_entry(old_ino, &old_name, ino)
                    && (!is_dir || !moved
                        || (t.set_dotdot(ino, new_ino) && t.link_dir(old_ino, false) && t.link_dir(new_ino, true)))
                    && t.touch_inode(ino);
                ok && t.commit()
            }
            None => false,
        };
        if done {
            self.tree.rename(node, new_dir, new_name);
        }
        done
    }

    /// Whether everything at and under `top` may go: every directory in it
    /// readable, and holding on the disk nothing the tree leaves out.
    fn removable(&mut self, top: NodeId) -> bool {
        let mut stack: Vec<NodeId> = Vec::new();
        stack.push(top);
        while let Some(at) = stack.pop() {
            let Some((ino, is_dir)) = self.tree.get(at).map(|n| (n.ino as u32, n.is_dir())) else { return false };
            if !is_dir {
                continue;
            }
            if !self.load_dir(at) {
                return false;
            }
            if self.hidden.get(&ino).copied().unwrap_or(0) != 0 {
                trace!(0, "ext4: directory {} holds entries the tree does not show: nothing removed", ino);
                return false;
            }
            stack.extend(self.tree.children(at));
        }
        true
    }

    pub fn remove(&mut self, node: NodeId) -> bool {
        if !self.writable() {
            return false;
        }
        if self.tree.parent(node).is_none() {
            trace!(0, "ext4: the root cannot be removed");
            return false;
        }
        if !self.removable(node) {
            return false;
        }
        /* Deepest first: each time down to something with nothing under
         * it, and that removed -- a walk, not a recursion. */
        loop {
            let mut at = node;
            while let Some(child) = self.tree.first_child(at) {
                at = child;
            }
            if !self.remove_one(at) {
                return false;
            }
            if at == node {
                return true;
            }
        }
    }

    /// `node` -- a file, or a directory with nothing left under it -- out
    /// of its parent and given back, one transaction.
    fn remove_one(&mut self, node: NodeId) -> bool {
        let Some((ino, is_dir)) = self.tree.get(node).map(|n| (n.ino as u32, n.is_dir())) else { return false };
        let Some(parent) = self.tree.parent(node).and_then(|p| self.dir_ino(p)) else { return false };
        let name: Vec<u8> = self.tree[node].name().to_vec();
        let done = match self.begin() {
            Some(mut t) => t.unlink(parent, &name, ino, is_dir) && t.commit(),
            None => false,
        };
        if done {
            self.tree.free_tree(node);
            self.hidden.remove(&ino);
        }
        done
    }
}
