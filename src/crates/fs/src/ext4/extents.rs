//! Extent trees as a transaction changes them. A change is made to one
//! leaf's entries as a list -- an extent put in where nothing maps, a range
//! taken out, part of an unwritten extent made written -- and then settled
//! up the path that leads to the leaf: a node that overflows split, the
//! root a level deeper when it does; a node left empty freed and taken out
//! of its parent; every index entry's key its child's first block; and an
//! emptied tree a leaf again. Every node read on the way is held to its
//! header, its depth and its checksum, and no node but the root is ever
//! empty. And every block a tree holds, for a file's blocks to be given
//! back.

use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;

use extfmt::extent::{self, Header, Index, Leaf, INIT_MAX_LEN, MAX_DEPTH, ROOT_BYTES, ROOT_MAX, UNINIT_MAX_LEN};
use extfmt::inode::{self as xinode, Inode, EXTENTS_FL};
use kcore::trace;

use super::inodes as inode;
use super::txn::{Kind, Txn};

/// A node of a tree, parsed.
struct Node {
    /// Its block, or None for the root in the inode.
    at: Option<u64>,
    /// Allocated by this change: its block is written, never read.
    new: bool,
    depth: u16,
    max: u16,
    generation: u32,
    leaves: Vec<Leaf>,
    index: Vec<Index>,
}

impl Node {
    fn parse(at: Option<u64>, bytes: &[u8]) -> Option<Node> {
        let h = extent::header(bytes).ok()?;
        let n = usize::from(h.entries);
        let (leaves, index) = if h.depth == 0 {
            ((0..n).map(|i| extent::leaf(bytes, i)).collect(), Vec::new())
        } else {
            (Vec::new(), (0..n).map(|i| extent::index(bytes, i)).collect())
        };
        Some(Node { at, new: false, depth: h.depth, max: h.max, generation: h.generation, leaves, index })
    }

    fn len(&self) -> usize {
        if self.depth == 0 { self.leaves.len() } else { self.index.len() }
    }

    fn first(&self) -> Option<u32> {
        if self.depth == 0 { self.leaves.first().map(|e| e.first) } else { self.index.first().map(|x| x.first) }
    }

    /// Its bytes into `out`: the header and the entries, zeros after them
    /// -- a block's checksum is the seal's to put on.
    fn store(&self, out: &mut [u8]) {
        out.fill(0);
        let h = Header { entries: self.len() as u16, max: self.max, depth: self.depth, generation: self.generation };
        extent::set_header(out, h);
        if self.depth == 0 {
            for (i, e) in self.leaves.iter().enumerate() {
                extent::set_leaf(out, i, *e);
            }
        } else {
            for (i, x) in self.index.iter().enumerate() {
                extent::set_index(out, i, *x);
            }
        }
    }

    /// The entries from `at` on, moved into a node of their own at block
    /// `b`, of room `max`.
    fn split_off(&mut self, at: usize, b: u64, max: u16) -> Node {
        let (leaves, index) = if self.depth == 0 {
            (self.leaves.split_off(at), Vec::new())
        } else {
            (Vec::new(), self.index.split_off(at))
        };
        Node { at: Some(b), new: true, depth: self.depth, max, generation: 0, leaves, index }
    }
}

/// The way from the root down to a leaf: the nodes, at each index node the
/// entry taken, and where the next subtree to the right begins -- the
/// first block the leaf's entries may not reach.
struct Path {
    nodes: Vec<Node>,
    taken: Vec<usize>,
    limit: u64,
}

/// Entries of a leaf that run on into each other -- written or not alike,
/// file blocks and device blocks following on, within what an entry holds
/// -- made one.
fn merge(leaves: &mut Vec<Leaf>) {
    let mut out: Vec<Leaf> = Vec::with_capacity(leaves.len());
    for e in leaves.drain(..) {
        if let Some(last) = out.last_mut() {
            let max = if e.unwritten { UNINIT_MAX_LEN } else { INIT_MAX_LEN };
            if last.unwritten == e.unwritten && u64::from(last.first) + u64::from(last.len) == u64::from(e.first)
                && last.start + u64::from(last.len) == e.start && last.len + e.len <= max {
                last.len += e.len;
                continue;
            }
        }
        out.push(e);
    }
    *leaves = out;
}

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

    /// The path to the leaf that maps -- or would map -- file block `l` of
    /// inode `ino` (`raw`), which must be mapped by extents.
    fn path_to(&mut self, ino: u32, iseed: u32, raw: &[u8], l: u32) -> Option<Path> {
        let Some(root) = Node::parse(None, &raw[xinode::I_BLOCK..xinode::I_BLOCK + ROOT_BYTES]) else {
            trace!(0, "ext4: inode {}'s extent root is damaged", ino);
            return None;
        };
        let mut path = Path { nodes: vec![root], taken: Vec::new(), limit: u64::MAX };
        loop {
            let n = path.nodes.last()?;
            if n.depth == 0 {
                return Some(path);
            }
            if n.index.is_empty() {
                trace!(0, "ext4: inode {} has an empty index node", ino);
                return None;
            }
            let k = n.index.iter().rposition(|x| x.first <= l).unwrap_or(0);
            if let Some(next) = n.index.get(k + 1) {
                path.limit = path.limit.min(u64::from(next.first));
            }
            let (child, depth) = (n.index[k].child, n.depth - 1);
            let bytes = self.node(ino, iseed, child, depth)?;
            let node = Node::parse(Some(child), &bytes)?;
            path.taken.push(k);
            path.nodes.push(node);
        }
    }

    /// A node into the transaction: the root into `raw`, a block as an
    /// extent block -- one allocated by this change filled, never read.
    fn store_node(&mut self, iseed: u32, raw: &mut [u8], node: &Node) -> bool {
        match node.at {
            None => {
                node.store(&mut raw[xinode::I_BLOCK..xinode::I_BLOCK + ROOT_BYTES]);
                true
            }
            Some(b) => {
                let kind = Kind::Extent { seed: iseed };
                let out = if node.new && !self.dirty.contains_key(&b) { self.fresh(b, kind) } else { self.modify(b, kind) };
                match out {
                    Some(out) => {
                        node.store(out);
                        true
                    }
                    None => false,
                }
            }
        }
    }

    /// The path, its leaf's entries changed -- `hint` where -- settled and
    /// stored: overflows split (the root a level deeper), empty nodes freed,
    /// keys put right, an emptied tree a leaf. The blocks a new node takes,
    /// or a freed one gave, counted in `raw`.
    fn settle(&mut self, ino: u32, iseed: u32, raw: &mut [u8], mut path: Path, mut hint: usize) -> bool {
        let block_max = extent::block_max(self.m.bs);
        let goal = path.nodes.iter().rev().find_map(|n| n.at)
            .unwrap_or_else(|| self.m.group_start(u64::from((ino - 1) / self.m.ipg)));
        let mut gone = vec![false; path.nodes.len()];
        let mut lv = path.nodes.len() - 1;
        loop {
            let count = path.nodes[lv].len();
            if count > usize::from(path.nodes[lv].max) {
                if lv == 0 {
                    /* The root is full: what it holds into a block of its
                     * own, the root a level deeper above it. */
                    if path.nodes[0].depth >= MAX_DEPTH {
                        trace!(0, "ext4: inode {}'s tree is as deep as the format allows", ino);
                        return false;
                    }
                    let Some(b) = self.alloc_block(goal) else { return false };
                    if !inode::add_blocks(self.m, raw, 1) {
                        return false;
                    }
                    let root = &mut path.nodes[0];
                    let child = Node {
                        at: Some(b), new: true, depth: root.depth, max: block_max, generation: 0,
                        leaves: core::mem::take(&mut root.leaves), index: core::mem::take(&mut root.index),
                    };
                    root.depth += 1;
                    root.max = ROOT_MAX;
                    root.index = vec![Index { first: child.first().unwrap_or(0), child: b }];
                    path.nodes.insert(1, child);
                    path.taken.insert(0, 0);
                    gone.insert(1, false);
                    lv = 1;
                    continue;
                }
                /* Split: past the change when it is at the end -- as files
                 * grow -- else in half. */
                let at = if hint + 1 >= count { count - 1 } else { count / 2 };
                let Some(b) = self.alloc_block(goal) else { return false };
                if !inode::add_blocks(self.m, raw, 1) {
                    return false;
                }
                let right = path.nodes[lv].split_off(at, b, block_max);
                if !self.store_node(iseed, raw, &right) {
                    return false;
                }
                let parent = lv - 1;
                let place = path.taken[parent] + 1;
                path.nodes[parent].index.insert(place, Index { first: right.first().unwrap_or(0), child: b });
                hint = place;
                lv = parent;
                continue;
            }
            if count == 0 && lv > 0 {
                /* Empty below the root: freed, and out of its parent. */
                let Some(b) = path.nodes[lv].at else { return false };
                if !self.free_block(b) || !inode::add_blocks(self.m, raw, -1) {
                    return false;
                }
                gone[lv] = true;
                let parent = lv - 1;
                path.nodes[parent].index.remove(path.taken[parent]);
                hint = path.taken[parent];
                lv = parent;
                continue;
            }
            break;
        }
        if path.nodes[0].len() == 0 {
            path.nodes[0].depth = 0;
            path.nodes[0].max = ROOT_MAX;
        }
        for lv in (0..path.nodes.len()).rev() {
            if gone[lv] {
                continue;
            }
            if lv > 0 {
                let first = path.nodes[lv].first();
                let k = path.taken[lv - 1];
                if let (Some(first), Some(x)) = (first, path.nodes[lv - 1].index.get_mut(k)) {
                    x.first = first;
                }
            }
            let node = &path.nodes[lv];
            if !self.store_node(iseed, raw, node) {
                return false;
            }
        }
        true
    }

    fn iseed(&self, ino: u32, raw: &[u8]) -> Option<u32> {
        let node = Inode::new(raw)?;
        if node.flags() & EXTENTS_FL == 0 {
            trace!(0, "ext4: inode {} is mapped by blocks, which this does not change", ino);
            return None;
        }
        Some(xinode::seed(self.m.seed, ino, node.generation()))
    }

    /// The extent that maps file block `l` of inode `ino` (`raw`), if one
    /// does: Some(None) for a hole, None for a tree that does not read.
    pub fn ext_lookup(&mut self, ino: u32, raw: &[u8], l: u32) -> Option<Option<Leaf>> {
        let iseed = self.iseed(ino, raw)?;
        let path = self.path_to(ino, iseed, raw, l)?;
        let leaf = path.nodes.last()?;
        Some(leaf.leaves.iter().copied()
            .find(|e| e.first <= l && u64::from(l) < u64::from(e.first) + u64::from(e.len)))
    }

    /// File blocks `e.first..e.first + e.len` mapped to the device's from
    /// `e.start`, where nothing maps them yet: into the leaf they belong in,
    /// run on into the extent before them where they follow it.
    pub fn ext_insert(&mut self, ino: u32, raw: &mut [u8], e: Leaf) -> bool {
        let Some(iseed) = self.iseed(ino, raw) else { return false };
        let Some(mut path) = self.path_to(ino, iseed, raw, e.first) else { return false };
        let end = u64::from(e.first) + u64::from(e.len);
        let limit = path.limit;
        let Some(leaf) = path.nodes.last_mut() else { return false };
        let p = leaf.leaves.partition_point(|x| x.first < e.first);
        let overlaps = (p > 0 && u64::from(leaf.leaves[p - 1].first) + u64::from(leaf.leaves[p - 1].len) > u64::from(e.first))
            || leaf.leaves.get(p).is_some_and(|x| end > u64::from(x.first))
            || end > limit;
        if overlaps {
            trace!(0, "ext4: blocks {}..{} of inode {} are mapped already", e.first, end, ino);
            return false;
        }
        leaf.leaves.insert(p, e);
        merge(&mut leaf.leaves);
        self.settle(ino, iseed, raw, path, p)
    }

    /// Block `lblock` of file `ino` mapped to device block `pblock`: what a
    /// directory grows by.
    pub fn extent_append(&mut self, ino: u32, raw: &mut [u8], lblock: u64, pblock: u64) -> bool {
        let Ok(first) = u32::try_from(lblock) else { return false };
        self.ext_insert(ino, raw, Leaf { first, len: 1, start: pblock, unwritten: false })
    }

    /// What maps file blocks `from..to` of inode `ino` (`raw`) taken out --
    /// an extent trimmed at either end, split around the middle, or taken
    /// whole -- and the blocks given back, counted out of `raw`: how many.
    pub fn ext_unmap(&mut self, ino: u32, raw: &mut [u8], from: u64, to: u64) -> Option<u64> {
        let iseed = self.iseed(ino, raw)?;
        let mut freed = 0u64;
        let mut pos = from;
        while pos < to {
            let mut path = self.path_to(ino, iseed, raw, u32::try_from(pos).ok()?)?;
            let next = path.limit;
            let leaf = path.nodes.last_mut()?;
            let mut out: Vec<Leaf> = Vec::new();
            out.try_reserve(leaf.leaves.len() + 1).ok()?;
            let mut hint = 0;
            let mut cut = Vec::new();
            for e in leaf.leaves.iter() {
                let (f, end) = (u64::from(e.first), u64::from(e.first) + u64::from(e.len));
                let (a, z) = (f.max(pos), end.min(to));
                if a >= z {
                    out.push(*e);
                    continue;
                }
                cut.push((e.start + (a - f), z - a));
                if f < a {
                    out.push(Leaf { len: (a - f) as u32, ..*e });
                }
                if z < end {
                    out.push(Leaf { first: z as u32, len: (end - z) as u32, start: e.start + (z - f), unwritten: e.unwritten });
                }
                hint = out.len().saturating_sub(1);
            }
            leaf.leaves = out;
            for (start, n) in cut {
                for b in start..start + n {
                    if !self.free_block(b) {
                        return None;
                    }
                }
                if !inode::add_blocks(self.m, raw, -(n as i64)) {
                    return None;
                }
                freed += n;
            }
            if !self.settle(ino, iseed, raw, path, hint) {
                return None;
            }
            pos = next;
        }
        Some(freed)
    }

    /// File blocks `l..l + n` of inode `ino`, inside one unwritten extent,
    /// made written -- the extent split around them, and what then runs on
    /// made one: how many were, from `l`.
    pub fn ext_mark_written(&mut self, ino: u32, raw: &mut [u8], l: u32, n: u32) -> Option<u32> {
        let iseed = self.iseed(ino, raw)?;
        let mut path = self.path_to(ino, iseed, raw, l)?;
        let leaf = path.nodes.last_mut()?;
        let Some(k) = leaf.leaves.iter()
            .position(|e| e.first <= l && u64::from(l) < u64::from(e.first) + u64::from(e.len)) else {
            trace!(0, "ext4: block {} of inode {} is in no extent to make written", l, ino);
            return None;
        };
        let e = leaf.leaves[k];
        if !e.unwritten {
            return Some(n);
        }
        let end = u64::from(e.first) + u64::from(e.len);
        let m = u64::from(n).min(end - u64::from(l)) as u32;
        let mut pieces = Vec::new();
        if e.first < l {
            pieces.push(Leaf { len: l - e.first, ..e });
        }
        pieces.push(Leaf { first: l, len: m, start: e.start + u64::from(l - e.first), unwritten: false });
        if u64::from(l) + u64::from(m) < end {
            pieces.push(Leaf {
                first: l + m,
                len: (end - u64::from(l) - u64::from(m)) as u32,
                start: e.start + u64::from(l - e.first) + u64::from(m),
                unwritten: true,
            });
        }
        leaf.leaves.splice(k..k + 1, pieces);
        merge(&mut leaf.leaves);
        self.settle(ino, iseed, raw, path, k).then_some(m)
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
