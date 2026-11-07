//! A filesystem of the ext family made from nothing, for a device of a
//! given size: the geometry it gets, and every block a fresh one holds --
//! the superblock and its copies, the group descriptors, the bitmaps, the
//! inode tables, the root directory and lost+found, and for ext4 the
//! journal -- handed to the caller as writes ([`Piece`]), in the order a
//! device should take them. What was there before stops being a filesystem
//! first, and the primary superblock is written last, so that a format cut
//! short leaves no superblock naming metadata that was never written.
//!
//! ext4 is made with what nos's driver writes (`features::classify` calls
//! it `Writable`): a journal, extents, metadata_csum, filetype,
//! sparse_super, large_file, dir_nlink and extra_isize. No flex_bg, no
//! 64bit, no resize_inode: every group carries its own bitmaps and inode
//! table, a descriptor is 32 bytes, and the filesystem cannot grow in
//! place. ext2 is revision 1 with filetype, sparse_super and large_file.
//! Every inode table and the journal are written whole, zeros where
//! nothing is yet, as scripts/mkrootfs.sh has mke2fs write them
//! (`lazy_itable_init=0`, `lazy_journal_init=0`): no group is left for a
//! driver to initialise later, and nothing a former use of the blocks left
//! is ever read as either.
//!
//! The device's identity and the time are the caller's to give: this crate
//! has neither a clock nor randomness.

use alloc::vec;
use alloc::vec::Vec;

use crate::dir::{self, FT_DIR};
use crate::extent::{self, Header, Leaf};
use crate::features::{compat, incompat, ro_compat};
use crate::group::{self, DescMut, INODE_ZEROED};
use crate::inode::{self, EXTENTS_FL, S_IFDIR, S_IFREG};
use crate::jbd2;
use crate::sb::{self, Super};
use crate::{set_le16, set_le32};

/// The kind of filesystem made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ext2,
    Ext4,
}

/// What is asked of the filesystem made.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub kind: Kind,
    /// 1024, 2048 or 4096 bytes; None for what the device's size picks --
    /// 1 KiB below 512 MiB, as mke2fs makes a small filesystem, 4 KiB from
    /// there -- and never less than the device's sector.
    pub block_size: Option<u32>,
    /// The volume label, NUL-padded.
    pub label: [u8; 16],
    pub uuid: [u8; 16],
    /// The seed of a hashed directory's hashes.
    pub hash_seed: [u8; 16],
    /// Seconds since the epoch: the superblock's times and every inode's.
    pub now: u32,
}

/// One write of the filesystem's making.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    /// These bytes at byte `at` of the device.
    Bytes { at: u64, data: &'a [u8] },
    /// `len` bytes of zeros from byte `at`.
    Zeros { at: u64, len: u64 },
    /// Everything before it on the device, past its cache, before anything
    /// after it is written.
    Barrier,
}

/// The smallest block made, and the largest: a page, the most the
/// kernel's drivers read a block into.
pub const MIN_BLOCK_SIZE: u64 = 1024;
pub const MAX_BLOCK_SIZE: u64 = 4096;

/// Below this a device gets 1 KiB blocks and an inode every 4 KiB, as
/// mke2fs makes a small filesystem; from it, 4 KiB blocks and an inode
/// every 16 KiB.
const SMALL_DEVICE: u64 = 512 * 1024 * 1024;
const INODE_RATIO_SMALL: u64 = 4096;
const INODE_RATIO: u64 = 16384;

const EXT4_INODE_SIZE: u64 = 256;
const EXT2_INODE_SIZE: u64 = 128;
/// The extra fields an ext4 inode made carries: as far as the creation
/// time, past the checksum's high half.
const EXTRA_ISIZE: u16 = 32;

const ROOT_INO: u32 = 2;
const JOURNAL_INO: u32 = 8;
/// The first inode a file may have, which is lost+found's.
const FIRST_INO: u32 = 11;
const LOST_FOUND_INO: u32 = FIRST_INO;
/// The least a group holds: the reserved inodes and lost+found in the
/// first, a whole byte of bitmap in every one.
const MIN_INODES_PER_GROUP: u64 = 16;

/// Descriptors of 32 bytes: no 64bit, and so a block number of 32 bits.
const DESC_SIZE: u64 = sb::DESC_SIZE_32;
const MAX_BLOCKS: u64 = u32::MAX as u64;

/// The journal: JBD2's least, and at most what four extents in the
/// journal inode's own `i_block` map, with no extent tree past it.
const MIN_JOURNAL_BLOCKS: u64 = 1024;
const MAX_JOURNAL_BLOCKS: u64 = 32768;
/// A group holds at least this much past its metadata, or the
/// filesystem ends before it (mke2fs's 50).
const MIN_GROUP_DATA: u64 = 50;

/// `s_rev_level`: inodes of the size the superblock says, features.
const REV_DYNAMIC: u32 = 1;
/// `s_errors`: carry on.
const ERRORS_CONTINUE: u16 = 1;
/// `s_max_mnt_count`: no check forced after so many mounts.
const MAX_MNT_COUNT_NONE: u16 = 0xFFFF;
/// `s_def_hash_version` and `s_flags`: half-MD4 over signed chars, what
/// mke2fs on x86 writes.
const HASH_HALF_MD4: u8 = 1;
const FLAGS_SIGNED_HASH: u32 = 0x1;
/// `s_jnl_backup_type`: `s_jnl_blocks` holds the journal inode's `i_block`.
const JNL_BACKUP_BLOCKS: u8 = 1;
/// `s_jnl_blocks`'s words after the `i_block` copy: the size's high and low.
const JNL_SIZE_HIGH_WORD: usize = 15;
const JNL_SIZE_WORD: usize = 16;

const ROOT_MODE: u16 = S_IFDIR | 0o755;
const LOST_FOUND_MODE: u16 = S_IFDIR | 0o700;
const JOURNAL_MODE: u16 = S_IFREG | 0o600;
const SECTOR: u64 = 512;

/// Whether group `g` carries a copy of the superblock and the descriptors
/// (sparse_super): the first two, and every power of 3, 5 and 7.
fn has_backup(g: u64) -> bool {
    g <= 1 || [3u64, 5, 7].iter().any(|&base| is_power_of(g, base))
}

/// Whether `n`, at least 2, is a power of `base`.
fn is_power_of(mut n: u64, base: u64) -> bool {
    while n % base == 0 {
        n /= base;
    }
    n == 1
}

/// mke2fs's journal for a filesystem of `blocks` blocks
/// (`ext2fs_default_journal_size`), held to `MAX_JOURNAL_BLOCKS`; 0 for
/// one too small to have one.
fn default_journal_blocks(blocks: u64) -> u64 {
    let wanted = match blocks {
        b if b < 2048 => 0,
        b if b < 32768 => 1024,
        b if b < 256 * 1024 => 4096,
        b if b < 512 * 1024 => 8192,
        b if b < 4096 * 1024 => 16384,
        _ => 32768,
    };
    wanted.min(MAX_JOURNAL_BLOCKS)
}

fn put_be32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

/// The filesystem a device gets: its geometry, worked out once, and where
/// what it starts with goes.
#[derive(Clone, Debug)]
pub struct Layout {
    kind: Kind,
    bs: u64,
    blocks: u64,
    /// The first data block: 1 for 1 KiB blocks, whose block 0 is the boot
    /// block and belongs to no group, else 0.
    first: u64,
    bpg: u64,
    groups: u64,
    ipg: u64,
    inode_size: u64,
    itable_blocks: u64,
    gdt_blocks: u64,
    /// The journal's runs on the device, in the file's order: first block,
    /// and how many.
    journal: Vec<(u64, u64)>,
    /// The blocks the filesystem made has free, counted once its journal
    /// is placed.
    free: u64,
    label: [u8; 16],
    uuid: [u8; 16],
    hash_seed: [u8; 16],
    now: u32,
}

impl Layout {
    /// The filesystem `o` asks for on a device of `device_bytes` bytes in
    /// sectors of `sector_size`, or why there is none.
    pub fn new(device_bytes: u64, sector_size: u64, o: &Options) -> Result<Layout, &'static str> {
        if sector_size < SECTOR || !sector_size.is_power_of_two() || sector_size > MAX_BLOCK_SIZE {
            return Err("a sector size no block of the format is a whole number of");
        }
        let bs = match o.block_size {
            Some(b) => u64::from(b),
            None if device_bytes < SMALL_DEVICE => MIN_BLOCK_SIZE.max(sector_size),
            None => MAX_BLOCK_SIZE,
        };
        if !bs.is_power_of_two() || !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&bs) {
            return Err("a block size other than 1024, 2048 or 4096");
        }
        if bs < sector_size {
            return Err("a block smaller than the device's sector");
        }
        let inode_size = match o.kind {
            Kind::Ext4 => EXT4_INODE_SIZE,
            Kind::Ext2 => EXT2_INODE_SIZE,
        };
        let ratio = if device_bytes < SMALL_DEVICE { INODE_RATIO_SMALL } else { INODE_RATIO };
        let first = if bs == MIN_BLOCK_SIZE { 1 } else { 0 };
        let bpg = 8 * bs;
        let mut blocks = device_bytes / bs;
        if blocks > MAX_BLOCKS {
            return Err("a device past what a filesystem without 64bit holds");
        }

        /* A last group too small for its metadata and a little data is left
         * off the end, and the groups worked out again without it. */
        let mut l = loop {
            if blocks <= first {
                return Err("a device too small for a filesystem");
            }
            let groups = (blocks - first).div_ceil(bpg);
            /* An inode every `ratio` bytes, spread over the groups: a whole
             * number of the inode table's blocks and of the bitmap's bytes in
             * each -- a step that is a power of two, so a multiple of both --
             * and no more than a bitmap block has bits for. */
            let step = (bs / inode_size).max(8);
            let wanted = (blocks * bs / ratio).max(u64::from(FIRST_INO) + 1);
            let ipg = (wanted.div_ceil(groups).div_ceil(step) * step).clamp(MIN_INODES_PER_GROUP.max(step), 8 * bs);
            if groups.checked_mul(ipg).is_none_or(|n| n > u64::from(u32::MAX)) {
                return Err("more inodes than a filesystem counts");
            }
            let l = Layout {
                kind: o.kind,
                bs,
                blocks,
                first,
                bpg,
                groups,
                ipg,
                inode_size,
                itable_blocks: ipg * inode_size / bs,
                gdt_blocks: (groups * DESC_SIZE).div_ceil(bs),
                journal: Vec::new(),
                free: 0,
                label: o.label,
                uuid: o.uuid,
                hash_seed: o.hash_seed,
                now: o.now,
            };
            let last = groups - 1;
            let size = l.group_size(last);
            if size >= l.meta_blocks(last) + MIN_GROUP_DATA {
                break l;
            }
            if groups == 1 {
                return Err("a device too small for a filesystem");
            }
            blocks -= size;
        };

        if l.kind == Kind::Ext4 {
            l.place_journal()?;
        }
        l.free = (0..l.groups).map(|g| l.free_in(g)).sum();
        Ok(l)
    }

    /// The journal: as big as mke2fs makes it for the filesystem's size,
    /// in the first data blocks after lost+found's, a run a group -- the
    /// metadata of the next between -- split where an extent ends, and no
    /// more runs than the journal inode's `i_block` maps.
    fn place_journal(&mut self) -> Result<(), &'static str> {
        let mut left = default_journal_blocks(self.blocks);
        if left < MIN_JOURNAL_BLOCKS {
            return Err("a device too small for ext4's journal");
        }
        let mut runs = Vec::new();
        let (mut g, mut at) = (0u64, self.data_start(0) + 2);
        while left > 0 && runs.len() < usize::from(extent::ROOT_MAX) && g < self.groups {
            let end = self.group_end(g);
            if at >= end {
                g += 1;
                if g < self.groups {
                    at = self.data_start(g);
                }
                continue;
            }
            let take = (end - at).min(left).min(u64::from(extent::INIT_MAX_LEN));
            runs.push((at, take));
            at += take;
            left -= take;
        }
        let total: u64 = runs.iter().map(|&(_, n)| n).sum();
        if total < MIN_JOURNAL_BLOCKS {
            return Err("a device too small for ext4's journal");
        }
        self.journal = runs;
        Ok(())
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn block_size(&self) -> u64 {
        self.bs
    }
    pub fn blocks(&self) -> u64 {
        self.blocks
    }
    pub fn groups(&self) -> u64 {
        self.groups
    }
    pub fn inodes(&self) -> u64 {
        self.groups * self.ipg
    }
    pub fn journal_blocks(&self) -> u64 {
        self.journal.iter().map(|&(_, n)| n).sum()
    }
    /// The blocks free in the filesystem made.
    pub fn free_blocks(&self) -> u64 {
        self.free
    }

    fn group_start(&self, g: u64) -> u64 {
        self.first + g * self.bpg
    }
    fn group_size(&self, g: u64) -> u64 {
        if g + 1 == self.groups { self.blocks - self.group_start(g) } else { self.bpg }
    }
    fn group_end(&self, g: u64) -> u64 {
        self.group_start(g) + self.group_size(g)
    }
    /// What starts group `g`, all of it in a row: the superblock's copy and
    /// the descriptors' where it has them, the two bitmaps, the inode table.
    fn meta_blocks(&self, g: u64) -> u64 {
        let copy = if has_backup(g) { 1 + self.gdt_blocks } else { 0 };
        copy + 2 + self.itable_blocks
    }
    fn block_bitmap(&self, g: u64) -> u64 {
        self.group_start(g) + if has_backup(g) { 1 + self.gdt_blocks } else { 0 }
    }
    fn inode_bitmap(&self, g: u64) -> u64 {
        self.block_bitmap(g) + 1
    }
    fn inode_table(&self, g: u64) -> u64 {
        self.block_bitmap(g) + 2
    }
    fn data_start(&self, g: u64) -> u64 {
        self.group_start(g) + self.meta_blocks(g)
    }
    fn root_block(&self) -> u64 {
        self.data_start(0)
    }
    fn lost_found_block(&self) -> u64 {
        self.data_start(0) + 1
    }

    /// The data blocks group `g` starts with taken, as runs inside it:
    /// the root's and lost+found's in the first, and the journal's.
    fn taken_in(&self, g: u64) -> Vec<(u64, u64)> {
        let (start, end) = (self.group_start(g), self.group_end(g));
        let mut runs = Vec::new();
        if g == 0 {
            runs.push((self.root_block(), 2));
        }
        for &(at, n) in &self.journal {
            let (from, to) = (at.max(start), (at + n).min(end));
            if from < to {
                runs.push((from, to - from));
            }
        }
        runs
    }
    fn free_in(&self, g: u64) -> u64 {
        let taken: u64 = self.taken_in(g).iter().map(|&(_, n)| n).sum();
        self.group_size(g) - self.meta_blocks(g) - taken
    }
    fn inodes_used_in(&self, g: u64) -> u64 {
        if g == 0 { u64::from(FIRST_INO) } else { 0 }
    }

    fn features(&self) -> (u32, u32, u32) {
        match self.kind {
            Kind::Ext4 => (
                compat::HAS_JOURNAL,
                incompat::FILETYPE | incompat::EXTENTS,
                ro_compat::SPARSE_SUPER | ro_compat::LARGE_FILE | ro_compat::DIR_NLINK | ro_compat::EXTRA_ISIZE
                    | ro_compat::METADATA_CSUM,
            ),
            Kind::Ext2 => (0, incompat::FILETYPE, ro_compat::SPARSE_SUPER | ro_compat::LARGE_FILE),
        }
    }

    /// The journal inode's `i_block`: the root of an extent tree of depth 0,
    /// a leaf for each of its runs.
    fn journal_root(&self) -> [u8; inode::I_BLOCK_BYTES] {
        let mut root = [0u8; inode::I_BLOCK_BYTES];
        extent::set_header(&mut root, Header {
            entries: self.journal.len() as u16,
            max: extent::ROOT_MAX,
            depth: 0,
            generation: 0,
        });
        let mut file_block = 0u64;
        for (i, &(start, n)) in self.journal.iter().enumerate() {
            extent::set_leaf(&mut root, i, Leaf { first: file_block as u32, len: n as u32, start, unwritten: false });
            file_block += n;
        }
        root
    }

    /// The superblock group `group`'s copy is -- the primary is group 0's
    /// -- its checksum on it.
    fn superblock(&self, group: u64) -> Result<[u8; sb::SIZE], &'static str> {
        let mut s = [0u8; sb::SIZE];
        let log = self.bs.trailing_zeros() - MIN_BLOCK_SIZE.trailing_zeros();
        let free_blocks = self.free;
        set_le32(&mut s, sb::S_INODES_COUNT, self.inodes() as u32);
        set_le32(&mut s, sb::S_BLOCKS_COUNT_LO, self.blocks as u32);
        set_le32(&mut s, sb::S_FREE_BLOCKS_COUNT_LO, free_blocks as u32);
        set_le32(&mut s, sb::S_FREE_INODES_COUNT, (self.inodes() - u64::from(FIRST_INO)) as u32);
        set_le32(&mut s, sb::S_FIRST_DATA_BLOCK, self.first as u32);
        set_le32(&mut s, sb::S_LOG_BLOCK_SIZE, log);
        set_le32(&mut s, sb::S_LOG_CLUSTER_SIZE, log);
        set_le32(&mut s, sb::S_BLOCKS_PER_GROUP, self.bpg as u32);
        set_le32(&mut s, sb::S_CLUSTERS_PER_GROUP, self.bpg as u32);
        set_le32(&mut s, sb::S_INODES_PER_GROUP, self.ipg as u32);
        set_le32(&mut s, sb::S_WTIME, self.now);
        set_le16(&mut s, sb::S_MAX_MNT_COUNT, MAX_MNT_COUNT_NONE);
        set_le16(&mut s, sb::S_MAGIC, sb::MAGIC);
        set_le16(&mut s, sb::S_STATE, sb::STATE_VALID);
        set_le16(&mut s, sb::S_ERRORS, ERRORS_CONTINUE);
        set_le32(&mut s, sb::S_LASTCHECK, self.now);
        set_le32(&mut s, sb::S_REV_LEVEL, REV_DYNAMIC);
        set_le32(&mut s, sb::S_FIRST_INO, FIRST_INO);
        set_le16(&mut s, sb::S_INODE_SIZE, self.inode_size as u16);
        /* The copies of a filesystem past 65535 groups number theirs as
         * mke2fs does: cut to the field. */
        set_le16(&mut s, sb::S_BLOCK_GROUP_NR, group as u16);
        let (c, i, r) = self.features();
        set_le32(&mut s, sb::S_FEATURE_COMPAT, c);
        set_le32(&mut s, sb::S_FEATURE_INCOMPAT, i);
        set_le32(&mut s, sb::S_FEATURE_RO_COMPAT, r);
        s[sb::S_UUID..sb::S_UUID + 16].copy_from_slice(&self.uuid);
        s[sb::S_VOLUME_NAME..sb::S_VOLUME_NAME + 16].copy_from_slice(&self.label);
        s[sb::S_HASH_SEED..sb::S_HASH_SEED + 16].copy_from_slice(&self.hash_seed);
        s[sb::S_DEF_HASH_VERSION] = HASH_HALF_MD4;
        set_le32(&mut s, sb::S_FLAGS, FLAGS_SIGNED_HASH);
        set_le32(&mut s, sb::S_MKFS_TIME, self.now);
        if self.kind == Kind::Ext4 {
            set_le32(&mut s, sb::S_JOURNAL_INUM, JOURNAL_INO);
            s[sb::S_JNL_BACKUP_TYPE] = JNL_BACKUP_BLOCKS;
            let root = self.journal_root();
            s[sb::S_JNL_BLOCKS..sb::S_JNL_BLOCKS + inode::I_BLOCK_BYTES].copy_from_slice(&root);
            let size = self.journal_blocks() * self.bs;
            set_le32(&mut s, sb::S_JNL_BLOCKS + 4 * JNL_SIZE_HIGH_WORD, (size >> 32) as u32);
            set_le32(&mut s, sb::S_JNL_BLOCKS + 4 * JNL_SIZE_WORD, size as u32);
            set_le16(&mut s, sb::S_MIN_EXTRA_ISIZE, EXTRA_ISIZE);
            set_le16(&mut s, sb::S_WANT_EXTRA_ISIZE, EXTRA_ISIZE);
            s[sb::S_CHECKSUM_TYPE] = sb::CHECKSUM_CRC32C;
        }
        let mut parsed = Super::parse(&s).map_err(|_| "a superblock of its own that does not parse")?;
        parsed.set_checksum();
        Ok(*parsed.raw())
    }

    /// An inode made: `mode`, `links`, `size` bytes over `blocks` of the
    /// filesystem's blocks, mapped by `i_block` -- an extent root on ext4 --
    /// its times now, its checksum on it.
    fn inode(&self, fs_seed: u32, ino: u32, mode: u16, links: u16, size: u64, blocks: u64,
             i_block: &[u8; inode::I_BLOCK_BYTES]) -> Vec<u8> {
        let mut i = vec![0u8; self.inode_size as usize];
        set_le16(&mut i, inode::I_MODE, mode);
        set_le32(&mut i, inode::I_SIZE_LO, size as u32);
        set_le32(&mut i, inode::I_SIZE_HIGH, (size >> 32) as u32);
        for at in [inode::I_ATIME, inode::I_CTIME, inode::I_MTIME] {
            set_le32(&mut i, at, self.now);
        }
        set_le16(&mut i, inode::I_LINKS_COUNT, links);
        set_le32(&mut i, inode::I_BLOCKS_LO, (blocks * (self.bs / SECTOR)) as u32);
        i[inode::I_BLOCK..inode::I_BLOCK + inode::I_BLOCK_BYTES].copy_from_slice(i_block);
        if self.kind == Kind::Ext4 {
            set_le32(&mut i, inode::I_FLAGS, EXTENTS_FL);
            set_le16(&mut i, inode::I_EXTRA_ISIZE, EXTRA_ISIZE);
            set_le32(&mut i, inode::I_CRTIME, self.now);
            inode::set_csum(fs_seed, ino, &mut i);
        }
        i
    }

    /// A directory's `i_block` for its one block: an extent of it on ext4,
    /// the first of its direct blocks on ext2.
    fn dir_i_block(&self, block: u64) -> [u8; inode::I_BLOCK_BYTES] {
        let mut root = [0u8; inode::I_BLOCK_BYTES];
        match self.kind {
            Kind::Ext4 => {
                extent::set_header(&mut root, Header { entries: 1, max: extent::ROOT_MAX, depth: 0, generation: 0 });
                extent::set_leaf(&mut root, 0, Leaf { first: 0, len: 1, start: block, unwritten: false });
            }
            Kind::Ext2 => set_le32(&mut root, 0, block as u32),
        }
        root
    }

    /// The first blocks of group 0's inode table, which hold the reserved
    /// inodes and lost+found: the root, the journal on ext4, lost+found --
    /// the others zeros, as the reserved ones not in use are.
    fn inode_table_head(&self, fs_seed: u32) -> Vec<u8> {
        let isz = self.inode_size as usize;
        let len = (FIRST_INO as usize * isz).div_ceil(self.bs as usize) * self.bs as usize;
        let mut head = vec![0u8; len];
        let mut place = |ino: u32, raw: &[u8]| {
            let at = (ino as usize - 1) * isz;
            head[at..at + isz].copy_from_slice(raw);
        };
        let one = 1u64;
        let root = self.inode(fs_seed, ROOT_INO, ROOT_MODE, 3, self.bs, one, &self.dir_i_block(self.root_block()));
        place(ROOT_INO, &root);
        let lost_found = self.inode(fs_seed, LOST_FOUND_INO, LOST_FOUND_MODE, 2, self.bs, one,
                                    &self.dir_i_block(self.lost_found_block()));
        place(LOST_FOUND_INO, &lost_found);
        if self.kind == Kind::Ext4 {
            let blocks = self.journal_blocks();
            let journal = self.inode(fs_seed, JOURNAL_INO, JOURNAL_MODE, 1, blocks * self.bs, blocks,
                                     &self.journal_root());
            place(JOURNAL_INO, &journal);
        }
        head
    }

    /// A directory's one block: its entries, the last running to the end
    /// -- to the tail on ext4, which carries the block's checksum.
    fn dir_block(&self, fs_seed: u32, dir_ino: u32, entries: &[(u32, &[u8])]) -> Vec<u8> {
        let bs = self.bs as usize;
        let mut b = vec![0u8; bs];
        let end = if self.kind == Kind::Ext4 { bs - dir::TAIL } else { bs };
        let mut at = 0;
        for (k, &(ino, name)) in entries.iter().enumerate() {
            let len = if k + 1 == entries.len() { end - at } else { dir::entry_len(name.len()) };
            dir::put_entry(&mut b, at, ino, len, name, FT_DIR);
            at += len;
        }
        if self.kind == Kind::Ext4 {
            dir::put_tail(&mut b);
            dir::set_leaf_csum(inode::seed(fs_seed, dir_ino, 0), &mut b);
        }
        b
    }

    /// Group `g`'s block bitmap: its metadata, the data it starts with,
    /// and past the group's end -- in the last, shorter one -- every bit
    /// set, as there is nothing there to allocate.
    fn block_bitmap_bytes(&self, g: u64) -> Vec<u8> {
        let mut b = vec![0u8; self.bs as usize];
        let start = self.group_start(g);
        let mut set = |from: u64, n: u64| {
            for bit in from..from + n {
                b[(bit / 8) as usize] |= 1 << (bit % 8);
            }
        };
        set(0, self.meta_blocks(g));
        for (at, n) in self.taken_in(g) {
            set(at - start, n);
        }
        let size = self.group_size(g);
        set(size, 8 * self.bs - size);
        b
    }

    /// Group `g`'s inode bitmap: the reserved inodes and lost+found in the
    /// first, and every bit past the group's inodes set.
    fn inode_bitmap_bytes(&self, g: u64) -> Vec<u8> {
        let mut b = vec![0u8; self.bs as usize];
        let mut set = |from: u64, n: u64| {
            for bit in from..from + n {
                b[(bit / 8) as usize] |= 1 << (bit % 8);
            }
        };
        set(0, self.inodes_used_in(g));
        set(self.ipg, 8 * self.bs - self.ipg);
        b
    }

    /// The journal's superblock, journal block 0: an empty ring of the rest
    /// of its blocks, its users the filesystem, and with metadata_csum the
    /// v3 checksums nos's driver and Linux's write.
    fn journal_super(&self) -> Result<Vec<u8>, &'static str> {
        let mut b = vec![0u8; self.bs as usize];
        put_be32(&mut b, 0, jbd2::MAGIC);
        put_be32(&mut b, 4, jbd2::SUPERBLOCK_V2);
        put_be32(&mut b, jbd2::S_BLOCKSIZE, self.bs as u32);
        put_be32(&mut b, jbd2::S_MAXLEN, self.journal_blocks() as u32);
        put_be32(&mut b, jbd2::S_FIRST, 1);
        put_be32(&mut b, jbd2::S_SEQUENCE, 1);
        put_be32(&mut b, jbd2::S_FEATURE_INCOMPAT, jbd2::INCOMPAT_CSUM_V3);
        b[jbd2::S_UUID..jbd2::S_UUID + 16].copy_from_slice(&self.uuid);
        put_be32(&mut b, jbd2::S_NR_USERS, 1);
        b[jbd2::S_USERS..jbd2::S_USERS + 16].copy_from_slice(&self.uuid);
        b[jbd2::S_CHECKSUM_TYPE] = jbd2::CHECKSUM_CRC32C;
        let mut js = jbd2::JournalSuper::parse(&b).map_err(|_| "a journal superblock of its own that does not parse")?;
        js.set_checksum();
        b[..jbd2::SB_SIZE].copy_from_slice(js.raw());
        Ok(b)
    }

    /// Every write of the filesystem's making, to `put` in order; the first
    /// it refuses stops them.
    pub fn write(&self, put: &mut dyn FnMut(Piece<'_>) -> bool) -> Result<(), &'static str> {
        const REFUSED: &str = "a write refused";
        let mut emit = |p: Piece<'_>| if put(p) { Ok(()) } else { Err(REFUSED) };
        let bs = self.bs;
        let primary = self.superblock(0)?;
        let sb = Super::parse(&primary).map_err(|_| "a superblock of its own that does not parse")?;
        let seed = sb.csum_seed();

        /* What was there stops being a filesystem: the boot block and the
         * primary superblock's place, on the device before anything new. */
        emit(Piece::Zeros { at: 0, len: if bs == MIN_BLOCK_SIZE { 2 * MIN_BLOCK_SIZE } else { bs } })?;
        emit(Piece::Barrier)?;

        for g in 0..self.groups {
            emit(Piece::Zeros { at: self.inode_table(g) * bs, len: self.itable_blocks * bs })?;
        }
        emit(Piece::Bytes { at: self.inode_table(0) * bs, data: &self.inode_table_head(seed) })?;

        let root = self.dir_block(seed, ROOT_INO, &[(ROOT_INO, b"."), (ROOT_INO, b".."),
                                                    (LOST_FOUND_INO, b"lost+found")]);
        emit(Piece::Bytes { at: self.root_block() * bs, data: &root })?;
        let lost_found = self.dir_block(seed, LOST_FOUND_INO, &[(LOST_FOUND_INO, b"."), (ROOT_INO, b"..")]);
        emit(Piece::Bytes { at: self.lost_found_block() * bs, data: &lost_found })?;

        if let Some(&(journal_start, _)) = self.journal.first() {
            for &(at, n) in &self.journal {
                emit(Piece::Zeros { at: at * bs, len: n * bs })?;
            }
            emit(Piece::Bytes { at: journal_start * bs, data: &self.journal_super()? })?;
        }

        /* The descriptors a block at a time -- a filesystem of many groups
         * has more of them than one allocation holds -- each block's groups'
         * bitmaps written as their checksums go into it, and the block then
         * into every group that keeps a copy. */
        let per_block = bs / DESC_SIZE;
        let copies: Vec<u64> = (0..self.groups).filter(|&g| has_backup(g)).collect();
        for k in 0..self.gdt_blocks {
            let mut gdt = vec![0u8; bs as usize];
            for g in k * per_block..((k + 1) * per_block).min(self.groups) {
                let bb = self.block_bitmap_bytes(g);
                let ib = self.inode_bitmap_bytes(g);
                emit(Piece::Bytes { at: self.block_bitmap(g) * bs, data: &bb })?;
                emit(Piece::Bytes { at: self.inode_bitmap(g) * bs, data: &ib })?;
                let at = ((g - k * per_block) * DESC_SIZE) as usize;
                let mut d = DescMut::new(&mut gdt[at..at + DESC_SIZE as usize]).ok_or("a descriptor of no size")?;
                d.set_block_bitmap(self.block_bitmap(g));
                d.set_inode_bitmap(self.inode_bitmap(g));
                d.set_inode_table(self.inode_table(g));
                d.set_free_blocks_count(self.free_in(g) as u32);
                d.set_free_inodes_count((self.ipg - self.inodes_used_in(g)) as u32);
                d.set_used_dirs_count(if g == 0 { 2 } else { 0 });
                if self.kind == Kind::Ext4 {
                    d.set_flags(INODE_ZEROED);
                    d.set_itable_unused((self.ipg - self.inodes_used_in(g)) as u32);
                    d.set_block_bitmap_csum(group::block_bitmap_csum(&sb, &bb).ok_or("a block bitmap too short")?);
                    d.set_inode_bitmap_csum(group::inode_bitmap_csum(&sb, &ib).ok_or("an inode bitmap too short")?);
                    d.set_checksum(&sb, g as u32);
                }
            }
            for &g in &copies {
                emit(Piece::Bytes { at: (self.group_start(g) + 1 + k) * bs, data: &gdt })?;
            }
        }

        /* The copies of the superblock, then -- once everything else is on
         * the device -- the primary. A copy starts its group's first block;
         * the primary is 1 KiB into the device, in block 0 of a filesystem
         * of bigger blocks. */
        emit(Piece::Barrier)?;
        for &g in copies.iter().filter(|&&g| g > 0) {
            let mut block = vec![0u8; bs as usize];
            block[..sb::SIZE].copy_from_slice(&self.superblock(g)?);
            emit(Piece::Bytes { at: self.group_start(g) * bs, data: &block })?;
        }
        emit(Piece::Barrier)?;
        if bs == MIN_BLOCK_SIZE {
            emit(Piece::Bytes { at: sb::OFFSET, data: &primary })?;
        } else {
            let mut block = vec![0u8; bs as usize];
            block[sb::OFFSET as usize..sb::OFFSET as usize + sb::SIZE].copy_from_slice(&primary);
            emit(Piece::Bytes { at: 0, data: &block })?;
        }
        emit(Piece::Barrier)
    }
}
