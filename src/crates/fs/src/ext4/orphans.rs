//! The orphan list: inodes whose giving back is not finished, on the
//! superblock's list -- linked through each one's dtime -- for whoever
//! mounts the filesystem next to finish. A remove of a file, or a cut,
//! whose blocks one transaction cannot give back puts the inode on it in
//! its first transaction and takes it off in its last; each one between
//! gives back a bounded share, from the file's end. A crash between them
//! leaves an inode of no links named nowhere -- the next mount deletes it
//! -- or a file whose extents run past its size -- the next mount cuts
//! them -- as Linux and e2fsck both do.
//!
//! Calls run one at a time, so an inode always leaves the list from its
//! head: the one put on last.
//!
//! A mount for writing releases what the list holds -- its own crash's, or
//! another kernel's -- once the journal is replayed, the last block of a
//! file cut made zeros past its end, as Linux makes it. A list that is not
//! sound mounts the filesystem read-only, with the reason.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec;

use extfmt::group::{self, INODE_UNINIT};
use extfmt::inode::{Inode, EXTENTS_FL};
use extfmt::{le32, set_le32, Map};
use kcore::trace;

use super::txn::Txn;
use super::Ext4;

/// What an inode's dtime holds while it is an orphan: the next one's number.
fn next_of(raw: &[u8]) -> u32 {
    le32(raw, extfmt::inode::I_DTIME)
}

fn set_next(raw: &mut [u8], ino: u32) {
    set_le32(raw, extfmt::inode::I_DTIME, ino);
}

impl Txn<'_> {
    /// Inode `ino` (`raw`, for the caller to store) at the head of the
    /// orphan list.
    pub fn orphan_add(&mut self, ino: u32, raw: &mut [u8]) -> bool {
        let Some(head) = self.super_now().map(|s| s.last_orphan()) else { return false };
        if head == ino {
            trace!(0, "ext4: inode {} is on the orphan list already", ino);
            return false;
        }
        set_next(raw, head);
        trace!(1, "ext4: inode {} on the orphan list, the rest of its blocks for transactions of their own", ino);
        self.change_super(|s| s.set_last_orphan(ino))
    }

    /// Inode `ino` (`raw`, for the caller to store) off the orphan list,
    /// whose head it must be.
    pub fn orphan_del(&mut self, ino: u32, raw: &mut [u8]) -> bool {
        let Some(head) = self.super_now().map(|s| s.last_orphan()) else { return false };
        if head != ino {
            trace!(0, "ext4: inode {} is to leave the orphan list, and its head is {}", ino, head);
            return false;
        }
        let next = next_of(raw);
        set_next(raw, 0);
        self.change_super(|s| s.set_last_orphan(next))
    }

    /// One transaction's share of what orphan `ino` -- the list's head --
    /// has to give back: its blocks past its size, from the end back, or
    /// with no links all of them and then itself. Whether that finished it,
    /// and it is off the list.
    pub fn orphan_step(&mut self, ino: u32) -> Option<bool> {
        let mut raw = self.inode(ino)?;
        let node = Inode::new(&raw)?;
        let (deleted, is_dir) = (node.links_count() == 0, node.is_dir());
        let keep = if deleted { 0 } else { node.size(true).div_ceil(self.m.bs as u64) };
        let mapped = node.flags() & EXTENTS_FL != 0;
        if mapped {
            let pieces = self.free_pieces();
            let (_, end) = self.ext_unmap_tail(ino, &mut raw, keep, 1 << 32, pieces)?;
            if end > keep {
                return self.put_inode(ino, &mut raw).then_some(false);
            }
        } else if node.blocks(self.m.huge_file()) != 0 {
            trace!(0, "ext4: orphan inode {} is mapped by blocks, which this does not give back", ino);
            return None;
        }
        if !self.orphan_del(ino, &mut raw) {
            return None;
        }
        if deleted {
            return self.release(ino, &mut raw, is_dir).then_some(true);
        }
        self.put_inode(ino, &mut raw).then_some(true)
    }
}

impl Ext4 {
    /// Orphan `ino` -- just put on the list by its call's first transaction
    /// -- given back by transactions of its own until it is off it. False,
    /// said, when one fails: it is left on the list, for the next mount.
    pub(super) fn finish_orphan(&mut self, ino: u32) -> bool {
        let limit = self.m.as_ref().map_or(0, |m| m.blocks);
        let mut steps = 0u64;
        loop {
            let step = match self.begin() {
                Some(mut t) => match t.orphan_step(ino) {
                    Some(done) => t.commit().then_some(done),
                    None => None,
                },
                None => None,
            };
            steps += 1;
            match step {
                Some(true) => return true,
                Some(false) if steps <= limit => {}
                _ => {
                    trace!(0, "ext4: inode {} is left on the orphan list, for the next mount to give back", ino);
                    return false;
                }
            }
        }
    }

    /// Whether inode `ino` is in use by its group's bitmap: read and held
    /// to its checksum.
    fn inode_in_use(&mut self, ino: u32) -> Result<bool, String> {
        let Some(m) = self.m.as_ref() else { return Err(String::from("not mounted")) };
        let (g, i) = (u64::from((ino - 1) / m.ipg), u64::from((ino - 1) % m.ipg));
        let Some(d) = m.desc(g) else { return Err(format!("no descriptor for inode {}'s group", ino)) };
        if m.uninit && d.flags() & INODE_UNINIT != 0 {
            return Ok(false);
        }
        let (block, stored, csum) = (d.inode_bitmap(), d.inode_bitmap_csum(), m.csum);
        let sb = m.sb.clone();
        let mut map = vec![0u8; m.bs];
        if !self.meta_block(block, &mut map) {
            return Err(format!("inode {}'s bitmap does not read", ino));
        }
        if csum && group::inode_bitmap_csum(&sb, &map).is_none_or(|c| !group::csum_matches(stored.0, stored.1, c)) {
            return Err(format!("inode {}'s bitmap fails its checksum", ino));
        }
        Ok(map[(i / 8) as usize] & (1 << (i % 8)) != 0)
    }

    /// The orphan list walked, nothing written: every inode on it one files
    /// may have, in use, read and held to its checksum, mapped by extents or
    /// holding nothing, and named once. How many, or why the list cannot be
    /// released.
    pub(super) fn orphans_sound(&mut self) -> Result<u32, String> {
        let Some((mut next, first, count)) =
            self.m.as_ref().map(|m| (m.sb.last_orphan(), m.first_ino, m.sb.inodes_count())) else {
            return Err(String::from("not mounted"));
        };
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        while next != 0 {
            if next < first || next > count || !seen.insert(next) {
                return Err(format!("its orphan list names inode {}, which can be no orphan", next));
            }
            if !self.inode_in_use(next)? {
                return Err(format!("its orphan list names inode {}, which is not in use", next));
            }
            let Some(raw) = self.inode(next) else {
                return Err(format!("orphan inode {} does not read", next));
            };
            let Some(node) = Inode::new(&raw) else {
                return Err(format!("orphan inode {} does not parse", next));
            };
            if node.flags() & EXTENTS_FL == 0 && node.blocks(self.m.as_ref().is_some_and(|m| m.huge_file())) != 0 {
                return Err(format!("orphan inode {} is mapped by blocks, which this does not give back", next));
            }
            next = next_of(&raw);
        }
        Ok(seen.len() as u32)
    }

    /// What of orphan `ino`'s last block lies past its size made zeros, as
    /// Linux makes it when it cuts one: a file cut as a crash came may hold
    /// there what it held before, and would show it when it grew.
    fn zero_past_size(&mut self, ino: u32) -> bool {
        let bs = self.io.bs as u64;
        let Some(raw) = self.inode(ino) else { return false };
        let Some(node) = Inode::new(&raw) else { return false };
        let size = node.size(true);
        if node.links_count() == 0 || size % bs == 0 {
            return true;
        }
        let block = match self.map(ino, &raw, size / bs) {
            Some(Map::Blocks { block, .. }) => block,
            Some(_) => return true,
            None => return false,
        };
        let mut blk = vec![0u8; self.io.bs];
        if !self.io.read(block, &mut blk) {
            return false;
        }
        if blk[(size % bs) as usize..].iter().all(|&b| b == 0) {
            return true;
        }
        blk[(size % bs) as usize..].fill(0);
        self.io.write(block, &blk, false)
    }

    /// Every orphan the list holds released, its head first, transactions
    /// of a bounded share each: how many, or why not -- the journal then
    /// aborted, the filesystem left for whoever mounts it next.
    pub(super) fn release_orphans(&mut self) -> Result<u32, String> {
        let mut released = 0u32;
        loop {
            let head = self.m.as_ref().map_or(0, |m| m.sb.last_orphan());
            if head == 0 {
                return Ok(released);
            }
            if !self.zero_past_size(head) {
                return Err(format!("what orphan inode {} holds past its size cannot be made zeros", head));
            }
            if !self.finish_orphan(head) {
                return Err(format!("orphan inode {} could not be released", head));
            }
            released += 1;
        }
    }
}
