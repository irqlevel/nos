//! Directories, as a transaction changes them. They are kept linear: an
//! index met on the way in is dropped first -- its root made the `.` and
//! `..` it also is, its other blocks the empty leaves -- in the same
//! transaction, so that no reader ever finds an index that leaves an entry
//! out. Every block is held to its checksum as it is read, and a name is
//! looked for on the disk, never only in the tree, which leaves some out.

use alloc::vec::Vec;

use extfmt::dir;
use extfmt::inode::{self as xinode, Inode, EXTENTS_FL, INDEX_FL};
use extfmt::{le16, le32, set_le16, set_le32, Map};
use kcore::trace;

use super::inodes::{self as inode, LINK_MAX};
use super::source;
use super::txn::{Kind, Txn};

/// Whether block `l` of a directory -- indexed or not -- is one of its
/// index's: its root, or a block one entry naming nothing spans whole.
fn is_dx(indexed: bool, l: usize, blk: &[u8]) -> bool {
    indexed && (l == 0 || (le32(blk, 0) == 0 && dir::rec_len_from_disk(le16(blk, 4), blk.len()) == blk.len()))
}

impl Txn<'_> {
    /// Where a leaf's entries end: at its tail, with checksums.
    fn leaf_end(&self) -> usize {
        if self.m.csum { self.m.bs - dir::TAIL } else { self.m.bs }
    }

    /// The blocks of directory `ino` (`raw`), in order: a hole in one, or
    /// an unwritten run, is damage.
    pub fn dir_blocks(&mut self, ino: u32, raw: &[u8]) -> Option<Vec<u64>> {
        let size = Inode::new(raw)?.size(self.m.largedir());
        let bs = self.m.bs as u64;
        if size % bs != 0 {
            trace!(0, "ext4: directory {} is {} bytes, not whole blocks", ino, size);
            return None;
        }
        let n = size / bs;
        let mut out = Vec::new();
        if out.try_reserve(n as usize).is_err() {
            return None;
        }
        let mut l = 0u64;
        while l < n {
            match source::map(self, ino, raw, l)? {
                Map::Blocks { block, run } => {
                    for k in 0..run.min(n - l) {
                        out.push(block + k);
                    }
                    l += run.max(1);
                }
                Map::Hole { .. } => {
                    trace!(0, "ext4: directory {} has a hole at block {}", ino, l);
                    return None;
                }
            }
        }
        Some(out)
    }

    /// Block `b`, the `l`th of directory `ino`, held to its checksum -- an
    /// index block's, or a leaf's -- unless this transaction made it.
    fn dir_block(&mut self, ino: u32, iseed: u32, indexed: bool, l: usize, b: u64) -> Option<Vec<u8>> {
        let made = self.dirty.contains_key(&b);
        let blk = self.read(b)?;
        if self.m.csum && !made {
            let ok = if is_dx(indexed, l, &blk) {
                dir::dx_csum(iseed, &blk).is_some_and(|(s, c)| s == c)
            } else {
                dir::tail(&blk).is_some_and(|s| s == dir::leaf_csum(iseed, &blk))
            };
            if !ok {
                trace!(0, "ext4: block {} of directory {} fails its checksum", l, ino);
                return None;
            }
        }
        Some(blk)
    }

    /// Directory `dir` and what a change to it needs: its inode, which must
    /// be a directory mapped by extents, and its checksum seed.
    fn dir_inode(&mut self, dir: u32) -> Option<(Vec<u8>, u32)> {
        let raw = self.inode(dir)?;
        let node = Inode::new(&raw)?;
        if !node.is_dir() {
            trace!(0, "ext4: inode {} is not a directory", dir);
            return None;
        }
        if node.flags() & EXTENTS_FL == 0 {
            trace!(0, "ext4: directory {} is mapped by blocks, which this does not change", dir);
            return None;
        }
        let iseed = xinode::seed(self.m.seed, dir, node.generation());
        Some((raw, iseed))
    }

    /// An entry naming `child` as `name`, of type `ft`, in directory `dir`:
    /// into the first entry with room after it, else into a block of its
    /// own at the directory's end. Refused where the directory holds the
    /// name already, in an entry the tree may not show. The directory's
    /// inode stored with its times, and its size when it grew.
    pub fn add_entry(&mut self, dir: u32, name: &[u8], child: u32, ft: u8) -> bool {
        let Some((mut raw, iseed)) = self.dir_inode(dir) else { return false };
        if le32(&raw, xinode::I_FLAGS) & INDEX_FL != 0 && !self.deindex(dir, iseed, &mut raw) {
            return false;
        }
        let Some(blocks) = self.dir_blocks(dir, &raw) else { return false };
        let (bs, end, need) = (self.m.bs, self.leaf_end(), dir::entry_len(name.len()));
        /* Where it goes: the block, the entry, what that entry keeps. */
        let mut slot: Option<(u64, usize, usize, usize)> = None;
        for (l, &b) in blocks.iter().enumerate() {
            let Some(blk) = self.dir_block(dir, iseed, false, l, b) else { return false };
            for e in dir::entries(&blk[..end], dir) {
                let e = match e {
                    Ok(e) => e,
                    Err(why) => {
                        trace!(0, "ext4: {}", why);
                        return false;
                    }
                };
                if e.ino != 0 && e.name == name {
                    trace!(0, "ext4: directory {} names that already", dir);
                    return false;
                }
                let keep = if e.ino == 0 { 0 } else { dir::entry_len(e.name.len()) };
                if slot.is_none() && !(l == 0 && e.at == 0) && e.rec_len >= keep + need {
                    slot = Some((b, e.at, keep, e.rec_len));
                }
            }
        }
        let now = self.now;
        match slot {
            Some((b, at, keep, rec_len)) => {
                let Some(blk) = self.modify(b, Kind::Dir { seed: iseed, dx: false }) else { return false };
                if keep != 0 {
                    set_le16(blk, at + 4, keep as u16);
                }
                dir::put_entry(blk, at + keep, child, rec_len - keep, name, ft);
            }
            None => {
                let l = blocks.len() as u64;
                let home = self.m.group_start(u64::from((dir - 1) / self.m.ipg));
                let goal = blocks.last().map_or(home, |&b| b + 1);
                let Some(b) = self.alloc_block(goal) else { return false };
                let csum = self.m.csum;
                {
                    let Some(blk) = self.fresh(b, Kind::Dir { seed: iseed, dx: false }) else { return false };
                    dir::put_entry(blk, 0, child, end, name, ft);
                    if csum {
                        dir::put_tail(blk);
                    }
                }
                if !self.extent_append(dir, &mut raw, l, b) {
                    return false;
                }
                let size = Inode::new(&raw).map_or(0, |i| i.size(self.m.largedir()));
                inode::set_size(&mut raw, size + bs as u64);
                if !inode::add_blocks(self.m, &mut raw, 1) {
                    return false;
                }
            }
        }
        inode::touch(&mut raw, now, true);
        self.put_inode(dir, &mut raw)
    }

    /// The entry naming `child` as `name` taken out of directory `dir`:
    /// merged into the entry before it, or -- the first of its block --
    /// left naming nothing. An index stays as it is: an entry gone from a
    /// leaf leaves every hash range it has true.
    pub fn remove_entry(&mut self, dir: u32, name: &[u8], child: u32) -> bool {
        let Some((mut raw, iseed)) = self.dir_inode(dir) else { return false };
        let indexed = le32(&raw, xinode::I_FLAGS) & INDEX_FL != 0;
        let Some(blocks) = self.dir_blocks(dir, &raw) else { return false };
        let end = self.leaf_end();
        for (l, &b) in blocks.iter().enumerate() {
            let Some(blk) = self.dir_block(dir, iseed, indexed, l, b) else { return false };
            if is_dx(indexed, l, &blk) {
                continue;
            }
            let mut prev: Option<(usize, usize)> = None;
            let mut found = None;
            for e in dir::entries(&blk[..end], dir) {
                let e = match e {
                    Ok(e) => e,
                    Err(why) => {
                        trace!(0, "ext4: {}", why);
                        return false;
                    }
                };
                if e.ino == child && e.name == name && !(l == 0 && e.at == 0) {
                    found = Some((e.at, e.rec_len, prev));
                    break;
                }
                prev = Some((e.at, e.rec_len));
            }
            let Some((at, rec_len, prev)) = found else { continue };
            let Some(blk) = self.modify(b, Kind::Dir { seed: iseed, dx: false }) else { return false };
            match prev {
                Some((p, p_len)) => set_le16(blk, p + 4, (p_len + rec_len) as u16),
                None => set_le32(blk, at, 0),
            }
            inode::touch(&mut raw, self.now, true);
            return self.put_inode(dir, &mut raw);
        }
        trace!(0, "ext4: directory {} has no entry of inode {} by that name", dir, child);
        false
    }

    /// Directory `dir`'s index dropped, in `raw` and its blocks: the root
    /// made `.` and a `..` running to the tail, every other index block a
    /// leaf naming nothing, the flag cleared. Its leaves keep every entry,
    /// which is all a linear directory is.
    fn deindex(&mut self, dir: u32, iseed: u32, raw: &mut [u8]) -> bool {
        let Some(blocks) = self.dir_blocks(dir, raw) else { return false };
        let end = self.leaf_end();
        for (l, &b) in blocks.iter().enumerate() {
            let Some(blk) = self.dir_block(dir, iseed, true, l, b) else { return false };
            if !is_dx(true, l, &blk) {
                continue;
            }
            let root = if l == 0 {
                let sound = dir::dx_count_offset(&blk) == Some(dir::DX_ROOT_COUNT) && blk[6] == 1 && blk[8] == b'.'
                    && blk[12 + 6] == 2 && &blk[12 + 8..12 + 10] == b"..";
                if !sound {
                    trace!(0, "ext4: directory {}'s index has no root", dir);
                    return false;
                }
                Some((le32(&blk, 0), blk[7], le32(&blk, 12), blk[12 + 7]))
            } else {
                None
            };
            let csum = self.m.csum;
            let Some(new) = self.modify(b, Kind::Dir { seed: iseed, dx: false }) else { return false };
            new.fill(0);
            match root {
                Some((dot, dot_ft, dotdot, dotdot_ft)) => {
                    dir::put_entry(new, 0, dot, 12, b".", dot_ft);
                    dir::put_entry(new, 12, dotdot, end - 12, b"..", dotdot_ft);
                }
                None => dir::put_entry(new, 0, 0, end, b"", 0),
            }
            if csum {
                dir::put_tail(new);
            }
        }
        let flags = le32(raw, xinode::I_FLAGS);
        set_le32(raw, xinode::I_FLAGS, flags & !INDEX_FL);
        trace!(1, "ext4: directory {}'s index dropped: it is linear now", dir);
        true
    }

    /// Whether directory `dir` (`raw`) names nothing but itself and its
    /// parent, on the disk -- whatever the tree shows of it.
    pub fn dir_empty(&mut self, dir: u32, raw: &[u8]) -> Option<bool> {
        let node = Inode::new(raw)?;
        let iseed = xinode::seed(self.m.seed, dir, node.generation());
        let indexed = node.flags() & INDEX_FL != 0;
        let blocks = self.dir_blocks(dir, raw)?;
        let end = self.leaf_end();
        for (l, &b) in blocks.iter().enumerate() {
            let blk = self.dir_block(dir, iseed, indexed, l, b)?;
            if is_dx(indexed, l, &blk) {
                continue;
            }
            for e in dir::entries(&blk[..end], dir) {
                let e = e.ok()?;
                if e.ino != 0 && e.name != b"." && e.name != b".." {
                    return Some(false);
                }
            }
        }
        Some(true)
    }

    /// Directory `dir`'s `..` made to name `parent`: in its first block's
    /// second entry, where an index's root has it too.
    pub fn set_dotdot(&mut self, dir: u32, parent: u32) -> bool {
        let Some((raw, iseed)) = self.dir_inode(dir) else { return false };
        let indexed = le32(&raw, xinode::I_FLAGS) & INDEX_FL != 0;
        let Some(blocks) = self.dir_blocks(dir, &raw) else { return false };
        let Some(&b0) = blocks.first() else { return false };
        let Some(blk) = self.dir_block(dir, iseed, indexed, 0, b0) else { return false };
        let at = dir::rec_len_from_disk(le16(&blk, 4), blk.len());
        if at < 12 || at + 10 > blk.len() || blk[at + 6] != 2 || &blk[at + 8..at + 10] != b".." {
            trace!(0, "ext4: directory {} has no '..' second", dir);
            return false;
        }
        let dx = is_dx(indexed, 0, &blk);
        let Some(new) = self.modify(b0, Kind::Dir { seed: iseed, dx }) else { return false };
        set_le32(new, at, parent);
        true
    }

    /// Directory `dir` given a subdirectory more, or one fewer: its link
    /// count as dir_nlink keeps it -- 1, once past what the count holds,
    /// and 1 kept -- and its times.
    pub fn link_dir(&mut self, dir: u32, more: bool) -> bool {
        let Some(mut raw) = self.inode(dir) else { return false };
        let n = inode::links(&raw);
        let next = match (more, n) {
            (_, 1) => 1,
            (true, n) if n >= LINK_MAX => {
                if !self.m.dir_nlink() {
                    trace!(0, "ext4: directory {} has all the subdirectories its link count holds", dir);
                    return false;
                }
                1
            }
            (true, n) => n + 1,
            (false, n) if n > 2 => n - 1,
            (false, n) => {
                trace!(0, "ext4: directory {} counts {} links, and loses a subdirectory", dir, n);
                n
            }
        };
        inode::set_links(&mut raw, next);
        inode::touch(&mut raw, self.now, true);
        self.put_inode(dir, &mut raw)
    }
}
