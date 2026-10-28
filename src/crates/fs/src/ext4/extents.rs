//! Extent trees as a transaction changes them: a block added at a file's
//! end -- what a directory grows by -- the tree grown to take it, a leaf or
//! a level at a time; and every block a tree holds, for a file's blocks to
//! be given back. Every node read on the way is held to its header, its
//! depth and its checksum; an index entry names the first block of its
//! child, and no node but the root is ever empty.

use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;

use extfmt::extent::{self, Header, Index, Leaf, INIT_MAX_LEN, MAX_DEPTH, ROOT_BYTES, ROOT_MAX};
use extfmt::inode::{self as xinode, Inode, EXTENTS_FL};
use kcore::trace;

use super::inodes as inode;
use super::txn::{Kind, Txn};

/// One node of the path down the tree's right edge: where it is -- None
/// for the root, in the inode -- and its header.
type Step = (Option<u64>, Header);

impl Txn<'_> {
    /// Block node `b` of inode `ino`'s tree, read and held to its checksum
    /// and to the depth it should have.
    fn node(&mut self, ino: u32, iseed: u32, b: u64, depth: u16) -> Option<Vec<u8>> {
        if b < self.m.first_data || b >= self.m.blocks || self.m.in_system(b) {
            trace!(0, "ext4: inode {}'s tree names block {}, which no tree may hold", ino, b);
            return None;
        }
        let made = self.dirty.contains_key(&b);
        let data = self.read(b)?;
        let h = match extent::header(&data) {
            Ok(h) => h,
            Err(why) => {
                trace!(0, "ext4: inode {}'s extent block {}: {}", ino, b, why);
                return None;
            }
        };
        if h.depth != depth || (h.entries == 0 && !made) {
            trace!(0, "ext4: inode {}'s extent block {} is at depth {} with {} entries, below a node at {}", ino, b,
                h.depth, h.entries, depth + 1);
            return None;
        }
        if self.m.csum && !made && extent::stored_csum(&data) != extent::node_csum(iseed, &data) {
            trace!(0, "ext4: inode {}'s extent block {} fails its checksum", ino, b);
            return None;
        }
        Some(data)
    }

    /// Node `at` of the path, to change: the root in `raw`, or its block.
    fn node_mut<'b>(&'b mut self, raw: &'b mut [u8], at: Option<u64>, iseed: u32) -> Option<&'b mut [u8]> {
        match at {
            None => Some(&mut raw[xinode::I_BLOCK..xinode::I_BLOCK + ROOT_BYTES]),
            Some(b) => self.modify(b, Kind::Extent { seed: iseed }),
        }
    }

    /// Block `lblock` of file `ino` -- past every block it maps -- mapped to
    /// device block `pblock`, the tree in `raw` grown to take it: the last
    /// extent stretched where the two run on, else an extent of its own in
    /// the rightmost leaf, a new leaf (and new levels down to it) under the
    /// lowest node on the way with room where that is full, and a level
    /// more at the root where none has. Every block a new node takes is
    /// counted in `raw`.
    pub fn extent_append(&mut self, ino: u32, raw: &mut [u8], lblock: u64, pblock: u64) -> bool {
        let Ok(first) = u32::try_from(lblock) else { return false };
        let Some(node) = Inode::new(raw) else { return false };
        if node.flags() & EXTENTS_FL == 0 {
            return false;
        }
        let iseed = xinode::seed(self.m.seed, ino, node.generation());
        let block_max = extent::block_max(self.m.bs);
        for _ in 0..2 {
            /* Down the right edge, to the leaf. */
            let mut node = raw[xinode::I_BLOCK..xinode::I_BLOCK + ROOT_BYTES].to_vec();
            let Ok(h) = extent::header(&node) else {
                trace!(0, "ext4: inode {}'s extent root is damaged", ino);
                return false;
            };
            let mut path: Vec<Step> = vec![(None, h)];
            let mut depth = h.depth;
            while depth > 0 {
                let Ok(hh) = extent::header(&node) else { return false };
                if hh.entries == 0 {
                    trace!(0, "ext4: inode {} has an empty index node", ino);
                    return false;
                }
                let last = extent::index(&node, usize::from(hh.entries - 1));
                if u64::from(last.first) > lblock {
                    trace!(0, "ext4: inode {} maps past block {} already", ino, lblock);
                    return false;
                }
                let Some(child) = self.node(ino, iseed, last.child, depth - 1) else { return false };
                let Ok(ch) = extent::header(&child) else { return false };
                path.push((Some(last.child), ch));
                node = child;
                depth -= 1;
            }
            let (leaf_at, lh) = *path.last().expect("the root at least");
            if lh.entries > 0 {
                let e = extent::leaf(&node, usize::from(lh.entries - 1));
                let end = u64::from(e.first) + u64::from(e.len);
                if end > lblock {
                    trace!(0, "ext4: block {} of inode {} is mapped already", lblock, ino);
                    return false;
                }
                if !e.unwritten && end == lblock && e.start + u64::from(e.len) == pblock && e.len < INIT_MAX_LEN {
                    let Some(n) = self.node_mut(raw, leaf_at, iseed) else { return false };
                    extent::set_leaf(n, usize::from(lh.entries - 1), Leaf { len: e.len + 1, ..e });
                    return true;
                }
            }
            if lh.entries < lh.max {
                let Some(n) = self.node_mut(raw, leaf_at, iseed) else { return false };
                extent::set_leaf(n, usize::from(lh.entries), Leaf { first, len: 1, start: pblock, unwritten: false });
                extent::set_header(n, Header { entries: lh.entries + 1, ..lh });
                return true;
            }
            /* The leaf is full: a new one under the lowest node above it
             * with room -- and if none has, the root a level deeper first. */
            let Some(level) = (0..path.len() - 1).rev().find(|&k| path[k].1.entries < path[k].1.max) else {
                if !self.grow_root(ino, raw, iseed, pblock) {
                    return false;
                }
                continue;
            };
            let below = path.len() - 1 - level;
            let mut child = 0u64;
            let mut taken = 0u64;
            for k in 0..below {
                let Some(b) = self.alloc_block(pblock) else { return false };
                taken += 1;
                let Some(n) = self.fresh(b, Kind::Extent { seed: iseed }) else { return false };
                extent::set_header(n, Header { entries: 1, max: block_max, depth: k as u16, generation: 0 });
                if k == 0 {
                    extent::set_leaf(n, 0, Leaf { first, len: 1, start: pblock, unwritten: false });
                } else {
                    extent::set_index(n, 0, Index { first, child });
                }
                child = b;
            }
            let (at, ah) = path[level];
            let Some(n) = self.node_mut(raw, at, iseed) else { return false };
            extent::set_index(n, usize::from(ah.entries), Index { first, child });
            extent::set_header(n, Header { entries: ah.entries + 1, ..ah });
            inode::add_blocks(self.m, raw, taken);
            return true;
        }
        false
    }

    /// The root of `raw`'s tree a level deeper: what it holds moved into a
    /// block of its own, which the root then names.
    fn grow_root(&mut self, ino: u32, raw: &mut [u8], iseed: u32, goal: u64) -> bool {
        let root = raw[xinode::I_BLOCK..xinode::I_BLOCK + ROOT_BYTES].to_vec();
        let Ok(h) = extent::header(&root) else { return false };
        if h.depth >= MAX_DEPTH {
            trace!(0, "ext4: inode {}'s tree is as deep as the format allows", ino);
            return false;
        }
        let key = match (h.entries, h.depth) {
            (0, _) => 0,
            (_, 0) => extent::leaf(&root, 0).first,
            _ => extent::index(&root, 0).first,
        };
        let Some(b) = self.alloc_block(goal) else { return false };
        let max = extent::block_max(self.m.bs);
        {
            let Some(n) = self.fresh(b, Kind::Extent { seed: iseed }) else { return false };
            n[..ROOT_BYTES].copy_from_slice(&root);
            extent::set_header(n, Header { max, ..h });
        }
        let r = &mut raw[xinode::I_BLOCK..xinode::I_BLOCK + ROOT_BYTES];
        r.fill(0);
        extent::set_header(r, Header { entries: 1, max: ROOT_MAX, depth: h.depth + 1, generation: h.generation });
        extent::set_index(r, 0, Index { first: key, child: b });
        inode::add_blocks(self.m, raw, 1);
        true
    }

    /// Every block file `ino` (`raw`) holds: its runs -- unwritten ones
    /// among them -- and the blocks of its tree below the root, each node
    /// held to its header, depth and checksum, and no block named twice.
    pub fn file_blocks(&mut self, ino: u32, raw: &[u8]) -> Option<(Vec<(u64, u64)>, Vec<u64>)> {
        let node = Inode::new(raw)?;
        if node.flags() & EXTENTS_FL == 0 {
            trace!(0, "ext4: inode {} is mapped by blocks, which this does not free", ino);
            return None;
        }
        let iseed = xinode::seed(self.m.seed, ino, node.generation());
        let root = node.i_block()[..ROOT_BYTES].to_vec();
        let Ok(h) = extent::header(&root) else {
            trace!(0, "ext4: inode {}'s extent root is damaged", ino);
            return None;
        };
        let (mut runs, mut tree) = (Vec::new(), Vec::new());
        let mut seen = BTreeSet::new();
        let mut stack: Vec<(Vec<u8>, u16)> = vec![(root, h.depth)];
        while let Some((n, depth)) = stack.pop() {
            let h = extent::header(&n).ok()?;
            for i in 0..usize::from(h.entries) {
                if depth == 0 {
                    let e = extent::leaf(&n, i);
                    if e.len == 0 || e.start < self.m.first_data
                        || e.start.checked_add(u64::from(e.len)).is_none_or(|end| end > self.m.blocks) {
                        trace!(0, "ext4: inode {} maps blocks {}..+{}, outside the filesystem", ino, e.start, e.len);
                        return None;
                    }
                    runs.push((e.start, u64::from(e.len)));
                } else {
                    let x = extent::index(&n, i);
                    if !seen.insert(x.child) || seen.len() as u64 > self.m.blocks {
                        trace!(0, "ext4: inode {}'s tree names block {} twice", ino, x.child);
                        return None;
                    }
                    let child = self.node(ino, iseed, x.child, depth - 1)?;
                    tree.push(x.child);
                    stack.push((child, depth - 1));
                }
            }
        }
        Some((runs, tree))
    }
}
