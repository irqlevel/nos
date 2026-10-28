//! Where the walks read metadata from: the filesystem as it stands
//! (`Reader` -- a journal's newer copies a read-only mount reads through,
//! the blocks committed and not yet home, the clean ones, the device), or
//! as a transaction has it so far (`Txn`). The same walks over either, so
//! that what a call changes is read back the way any read reads it: every
//! inode and every extent node held to its checksum on the way.

use alloc::vec;
use alloc::vec::Vec;

use extfmt::group::{Desc, INODE_UNINIT};
use extfmt::inode::{self as xinode, Inode, EXTENTS_FL};
use extfmt::{blockmap, extent, Map};
use kcore::trace;

use super::cache::Cache;
use super::io::Io;
use super::journal::Overlay;
use super::meta::Meta;

pub(super) trait Source {
    fn meta(&self) -> &Meta;
    /// Group `g`'s descriptor as this source has it.
    fn group(&self, g: u64) -> Option<Desc<'_>>;
    /// Metadata block `b` into `out`, a block long.
    fn block(&mut self, b: u64, out: &mut [u8]) -> bool;
}

pub(super) struct Reader<'a> {
    pub io: &'a mut Io,
    pub cache: &'a mut Cache,
    pub overlay: &'a Overlay,
    pub m: &'a Meta,
}

/// Block `b` as a read sees it: a journal's newer copy where a read-only
/// mount has one, else the newest held, else its home -- kept clean when
/// `keep`.
pub(super) fn read_block(io: &mut Io, cache: &mut Cache, overlay: &Overlay, b: u64, out: &mut [u8], keep: bool)
    -> bool {
    if let Some(&(copy, escaped)) = overlay.get(&b) {
        if !io.read(copy, out) {
            return false;
        }
        if escaped {
            out[..4].copy_from_slice(&extfmt::jbd2::MAGIC.to_be_bytes());
        }
        return true;
    }
    if let Some(held) = cache.get(b) {
        out[..held.len()].copy_from_slice(held);
        return true;
    }
    if !io.read(b, out) {
        return false;
    }
    if keep {
        cache.insert_clean(b, &out[..io.bs]);
    }
    true
}

impl Source for Reader<'_> {
    fn meta(&self) -> &Meta {
        self.m
    }

    fn group(&self, g: u64) -> Option<Desc<'_>> {
        self.m.desc(g)
    }

    fn block(&mut self, b: u64, out: &mut [u8]) -> bool {
        read_block(self.io, self.cache, self.overlay, b, out, true)
    }
}

/// Inode `ino`'s bytes: None for one outside the filesystem, in a part of
/// an inode table never written, or failing its checksum -- each said.
pub(super) fn inode(src: &mut dyn Source, ino: u32) -> Option<Vec<u8>> {
    let (count, ipg, isz, bs, uninit, csum, seed) = {
        let m = src.meta();
        (m.sb.inodes_count(), m.ipg, m.inode_size, m.bs, m.uninit, m.csum, m.seed)
    };
    if ino == 0 || ino > count {
        trace!(0, "ext4: inode {} of {}", ino, count);
        return None;
    }
    let (group, index) = (u64::from((ino - 1) / ipg), (ino - 1) % ipg);
    let (flags, unused, table) = src.group(group).map(|d| (d.flags(), d.itable_unused(), d.inode_table()))?;
    if uninit && (flags & INODE_UNINIT != 0 || index >= ipg.saturating_sub(unused)) {
        trace!(0, "ext4: inode {} is in a part of its group's table never written", ino);
        return None;
    }
    let at = u64::from(index) * isz as u64;
    let block = table + at / bs as u64;
    let within = (at % bs as u64) as usize;
    let mut blk = vec![0u8; bs];
    if !src.block(block, &mut blk) {
        return None;
    }
    let raw = blk.get(within..within + isz)?.to_vec();
    let zero = raw[..xinode::GOOD_OLD_SIZE].iter().all(|&b| b == 0);
    if csum && !zero && !xinode::csum_matches(seed, ino, &raw) {
        trace!(0, "ext4: inode {} fails its checksum", ino);
        return None;
    }
    Some(raw)
}

/// Where block `lblock` of the file inode `ino` (`raw`) is: through its
/// extent tree, every node of it held to its checksum, or its block map.
/// An unwritten run is a hole: it reads as zeros.
pub(super) fn map(src: &mut dyn Source, ino: u32, raw: &[u8], lblock: u64) -> Option<Map> {
    let node = Inode::new(raw)?;
    let (fs_blocks, bs, csum, seed) = {
        let m = src.meta();
        (m.blocks, m.bs, m.csum, m.seed)
    };
    let iseed = xinode::seed(seed, ino, node.generation());
    let extents = node.flags() & EXTENTS_FL != 0;
    let mut read = |b: u64, buf: &mut [u8]| {
        if !src.block(b, buf) {
            return Err(alloc::format!("block {} does not read", b));
        }
        if csum && extents && extent::stored_csum(buf).zip(extent::node_csum(iseed, buf)).is_none_or(|(s, c)| s != c) {
            return Err(alloc::format!("inode {}'s extent block {} fails its checksum", ino, b));
        }
        Ok(())
    };
    let mapped = if extents {
        extent::map(node.i_block(), lblock, fs_blocks, bs, ino, &mut read)
    } else {
        blockmap::map(node.i_block(), lblock, bs, ino, &mut read)
    };
    match mapped {
        Ok(Map::Blocks { block, run }) if block.checked_add(run).is_none_or(|e| e > fs_blocks) => {
            trace!(0, "ext4: inode {}: blocks {}..+{} past the filesystem", ino, block, run);
            None
        }
        Ok(m) => Some(m),
        Err(e) => {
            trace!(0, "ext4: {}", e);
            None
        }
    }
}
