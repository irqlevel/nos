//! Inodes and blocks handed out and given back, through a transaction:
//! each group's bitmap held to its checksum as it is read -- or, in a group
//! never initialised, made as the kernel makes it -- and the group's counts
//! kept with it. No file is given a block of the system zone, whatever a
//! bitmap says; no inode is handed out from below the first one files may
//! have.

use alloc::vec;
use alloc::vec::Vec;

use extfmt::group::{self, BLOCK_UNINIT, INODE_UNINIT};
use kcore::trace;

use super::source::Source;
use super::txn::{Kind, Txn};

fn bit(map: &[u8], i: u64) -> bool {
    map[(i / 8) as usize] & (1 << (i % 8)) != 0
}

fn set_bit(map: &mut [u8], i: u64, on: bool) {
    let (byte, mask) = ((i / 8) as usize, 1u8 << (i % 8));
    if on {
        map[byte] |= mask;
    } else {
        map[byte] &= !mask;
    }
}

impl Txn<'_> {
    /// Group `g`'s block bitmap as the kernel makes one for a group never
    /// initialised: the system zone's blocks in it taken, and the bits past
    /// the group's end. And how many it leaves free.
    fn made_block_bitmap(&self, g: u64) -> (Vec<u8>, u64) {
        let bs = self.m.bs;
        let mut map = vec![0u8; bs];
        let (start, len) = (self.m.group_start(g), self.m.group_len(g));
        let mut taken = 0u64;
        let first = self.m.system.partition_point(|&(_, e)| e <= start);
        for &(s, e) in &self.m.system[first..] {
            if s >= start + len {
                break;
            }
            for b in s.max(start)..e.min(start + len) {
                set_bit(&mut map, b - start, true);
                taken += 1;
            }
        }
        for i in len..(bs * 8) as u64 {
            set_bit(&mut map, i, true);
        }
        (map, len - taken)
    }

    /// Group `g`'s bitmap -- of inodes, or of blocks -- in this transaction
    /// to change: its block. Read and held to the checksum its descriptor
    /// carries; or, never initialised, made, and the group's flag for it
    /// cleared. The descriptor is in the transaction too, to carry the
    /// bitmap's new checksum.
    pub fn bitmap(&mut self, g: u64, inodes: bool) -> Option<u64> {
        let (flags, block, stored) = {
            let d = self.group(g)?;
            if inodes {
                (d.flags(), d.inode_bitmap(), d.inode_bitmap_csum())
            } else {
                (d.flags(), d.block_bitmap(), d.block_bitmap_csum())
            }
        };
        let kind = if inodes { Kind::InodeBitmap(g) } else { Kind::BlockBitmap(g) };
        if self.dirty.contains_key(&block) {
            return Some(block);
        }
        let flag = if inodes { INODE_UNINIT } else { BLOCK_UNINIT };
        if self.m.uninit && flags & flag != 0 {
            let (map, free) = if inodes {
                let mut map = vec![0u8; self.m.bs];
                for i in u64::from(self.m.ipg)..(self.m.bs * 8) as u64 {
                    set_bit(&mut map, i, true);
                }
                (map, 0)
            } else {
                self.made_block_bitmap(g)
            };
            self.fresh(block, kind)?.copy_from_slice(&map);
            let mut d = self.group_mut(g)?;
            d.set_flags(flags & !flag);
            if !inodes {
                d.set_free_blocks_count(free as u32);
            }
            return Some(block);
        }
        let csum = self.m.csum;
        let sb = self.m.sb.clone();
        let data = self.modify(block, kind)?;
        if csum {
            let computed = if inodes { group::inode_bitmap_csum(&sb, data) } else { group::block_bitmap_csum(&sb, data) };
            if computed.is_none_or(|c| !group::csum_matches(stored.0, stored.1, c)) {
                trace!(0, "ext4: group {}'s {} bitmap fails its checksum", g, if inodes { "inode" } else { "block" });
                return None;
            }
        }
        self.group_mut(g)?;
        Some(block)
    }

    /// A block, free and the filesystem's to give: the first at `goal` or
    /// after it in its group, then in the groups after. None, said, when
    /// there is none -- or a bitmap is damaged.
    pub fn alloc_block(&mut self, goal: u64) -> Option<u64> {
        let (groups, first, blocks) = (self.m.groups, self.m.first_data, self.m.blocks);
        let goal = goal.clamp(first, blocks.saturating_sub(1));
        let g0 = self.m.group_of(goal);
        for k in 0..groups {
            let g = (g0 + k) % groups;
            if self.group(g)?.free_blocks_count() == 0 {
                continue;
            }
            let map = self.bitmap(g, false)?;
            let (start, len) = (self.m.group_start(g), self.m.group_len(g));
            let from = if k == 0 { goal.saturating_sub(start).min(len) } else { 0 };
            let found = {
                let data = &self.dirty.get(&map)?.data;
                let freed = &self.freed;
                (from..len).chain(0..from).find(|&i| !bit(data, i) && !freed.contains(&(start + i)))
            };
            let Some(i) = found else {
                trace!(0, "ext4: group {} counts free blocks, and its bitmap has none", g);
                continue;
            };
            let b = start + i;
            if self.m.in_system(b) {
                trace!(0, "ext4: group {}'s bitmap has block {}, the filesystem's own, free: refused", g, b);
                return None;
            }
            set_bit(&mut self.dirty.get_mut(&map)?.data, i, true);
            let mut d = self.group_mut(g)?;
            let (free, flags) = (d.view().free_blocks_count(), d.view().flags());
            d.set_free_blocks_count(free.saturating_sub(1));
            d.set_flags(flags & !BLOCK_UNINIT);
            self.cache.forget(b);
            return Some(b);
        }
        trace!(0, "ext4: no free blocks");
        None
    }

    /// An inode, the lowest free in the first group from `goal` on that has
    /// one -- so that the part of a table never written shrinks by one -- a
    /// directory's counted as one. Its group's block bitmap is initialised
    /// with it, as the kernel's is.
    pub fn alloc_inode(&mut self, goal: u64, dir: bool) -> Option<u32> {
        let (groups, ipg, first_ino) = (self.m.groups, self.m.ipg, self.m.first_ino);
        for k in 0..groups {
            let g = (goal + k) % groups;
            if self.group(g)?.free_inodes_count() == 0 {
                continue;
            }
            let map = self.bitmap(g, true)?;
            let found = {
                let data = &self.dirty.get(&map)?.data;
                (0..u64::from(ipg)).find(|&i| !bit(data, i) && g * u64::from(ipg) + i + 1 >= u64::from(first_ino))
            };
            let Some(i) = found else {
                trace!(0, "ext4: group {} counts free inodes, and its bitmap has none", g);
                continue;
            };
            set_bit(&mut self.dirty.get_mut(&map)?.data, i, true);
            let uninit = self.m.uninit;
            let block_uninit = {
                let mut d = self.group_mut(g)?;
                let v = d.view();
                let (free, dirs, unused, flags) = (v.free_inodes_count(), v.used_dirs_count(), v.itable_unused(), v.flags());
                d.set_free_inodes_count(free.saturating_sub(1));
                if dir {
                    d.set_used_dirs_count(dirs + 1);
                }
                if uninit && i >= u64::from(ipg.saturating_sub(unused)) {
                    d.set_itable_unused(ipg - i as u32 - 1);
                }
                d.set_flags(flags & !INODE_UNINIT);
                uninit && flags & BLOCK_UNINIT != 0
            };
            if block_uninit {
                self.bitmap(g, false)?;
            }
            return Some((g * u64::from(ipg) + i + 1) as u32);
        }
        trace!(0, "ext4: no free inodes");
        None
    }

    /// Block `b` given back: free in its bitmap and its group's count, not
    /// handed out again before the commit, kept from any checkpoint, and --
    /// where the live log holds a copy of it -- revoked, so that no replay
    /// writes that copy over whatever the block becomes. False for a block
    /// outside the filesystem's data, or free already: damage.
    pub fn free_block(&mut self, b: u64) -> bool {
        if b < self.m.first_data || b >= self.m.blocks || self.m.in_system(b) {
            trace!(0, "ext4: a block to free, {}, outside the filesystem's data: refused", b);
            return false;
        }
        self.dirty.remove(&b);
        let g = self.m.group_of(b);
        let Some(map) = self.bitmap(g, false) else { return false };
        let i = b - self.m.group_start(g);
        let Some(d) = self.dirty.get_mut(&map) else { return false };
        if !bit(&d.data, i) {
            trace!(0, "ext4: block {} to free, and its bitmap has it free already", b);
            return false;
        }
        set_bit(&mut d.data, i, false);
        let Some(mut desc) = self.group_mut(g) else { return false };
        let free = desc.view().free_blocks_count();
        desc.set_free_blocks_count(free + 1);
        if self.jnl.in_log.contains_key(&b) {
            self.revoke.insert(b);
        }
        self.freed.insert(b);
        true
    }

    /// Inode `ino` given back in its bitmap and its group's counts -- a
    /// directory's among them.
    pub fn free_inode(&mut self, ino: u32, dir: bool) -> bool {
        let ipg = self.m.ipg;
        if ino < self.m.first_ino || ino > self.m.sb.inodes_count() {
            trace!(0, "ext4: an inode to free, {}, that files cannot have: refused", ino);
            return false;
        }
        let (g, i) = (u64::from((ino - 1) / ipg), u64::from((ino - 1) % ipg));
        let Some(map) = self.bitmap(g, true) else { return false };
        let Some(d) = self.dirty.get_mut(&map) else { return false };
        if !bit(&d.data, i) {
            trace!(0, "ext4: inode {} to free, and its bitmap has it free already", ino);
            return false;
        }
        set_bit(&mut d.data, i, false);
        let Some(mut desc) = self.group_mut(g) else { return false };
        let (free, dirs) = (desc.view().free_inodes_count(), desc.view().used_dirs_count());
        desc.set_free_inodes_count(free + 1);
        if dir {
            if dirs == 0 {
                trace!(0, "ext4: group {} counts no directories, and one is freed", g);
                return false;
            }
            desc.set_used_dirs_count(dirs - 1);
        }
        true
    }
}
