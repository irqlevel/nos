//! ext4 images, as the format has them: made (`mkfs` -- what mke2fs and a
//! copy in would leave, its journal holding transactions nobody replayed
//! if asked) and, next, judged. The fuzzer's own, written from the format
//! and not from the kernel's driver or from extfmt, which the driver reads
//! through: what the driver reads is held to the format rather than to
//! itself. Its images are held to e2fsck in turn (`fs-fuzz.py --e2fsck`).
//!
//! The features are the ones nos's ext4 driver writes -- has_journal,
//! extent, 64bit or not, flex_bg or not, metadata_csum with or without its
//! own seed, huge_file, dir_nlink, extra_isize, sparse_super, large_file,
//! filetype, ext_attr, dir_index -- and what a writer may do differently
//! within them is the input's: groups left uninitialised with garbage where
//! nothing was written, files in extent trees of any depth, some of their
//! zeros allocated but unwritten over garbage, entries the kernel's tree
//! does not show (long names, symlinks, device nodes).

pub mod check;
pub mod hash;
pub mod jbd2;
pub mod mkfs;
pub mod replay;

pub use check::check;
pub use mkfs::{mkfs, Made, Orphan, Style, FILLER};

pub use super::ext2::{rd16, rd32, wr16, wr32};

pub const MAGIC: u16 = 0xEF53;
pub const ROOT_INO: u32 = 2;
pub const JOURNAL_INO: u32 = 8;
pub const LOST_FOUND_INO: u32 = 11;
pub const FIRST_INO: u32 = 11;

/* Features, as the kernel numbers them. */
pub const COMPAT_HAS_JOURNAL: u32 = 0x4;
pub const COMPAT_EXT_ATTR: u32 = 0x8;
pub const COMPAT_DIR_INDEX: u32 = 0x20;
pub const INCOMPAT_FILETYPE: u32 = 0x2;
pub const INCOMPAT_RECOVER: u32 = 0x4;
pub const INCOMPAT_EXTENTS: u32 = 0x40;
pub const INCOMPAT_64BIT: u32 = 0x80;
pub const INCOMPAT_FLEX_BG: u32 = 0x200;
pub const INCOMPAT_CSUM_SEED: u32 = 0x2000;
pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
pub const RO_COMPAT_LARGE_FILE: u32 = 0x2;
pub const RO_COMPAT_HUGE_FILE: u32 = 0x8;
pub const RO_COMPAT_DIR_NLINK: u32 = 0x20;
pub const RO_COMPAT_EXTRA_ISIZE: u32 = 0x40;
pub const RO_COMPAT_METADATA_CSUM: u32 = 0x400;

/// The superblock's fields, by offset (struct ext4_super_block).
pub mod sb {
    pub const OFFSET: u64 = 1024;
    pub const INODES_COUNT: usize = 0x00;
    pub const BLOCKS_COUNT_LO: usize = 0x04;
    pub const R_BLOCKS_COUNT_LO: usize = 0x08;
    pub const FREE_BLOCKS_LO: usize = 0x0C;
    pub const FREE_INODES: usize = 0x10;
    pub const FIRST_DATA_BLOCK: usize = 0x14;
    pub const LOG_BLOCK_SIZE: usize = 0x18;
    pub const LOG_CLUSTER_SIZE: usize = 0x1C;
    pub const BLOCKS_PER_GROUP: usize = 0x20;
    pub const CLUSTERS_PER_GROUP: usize = 0x24;
    pub const INODES_PER_GROUP: usize = 0x28;
    pub const MTIME: usize = 0x2C;
    pub const WTIME: usize = 0x30;
    pub const MAX_MNT_COUNT: usize = 0x36;
    pub const MAGIC: usize = 0x38;
    pub const STATE: usize = 0x3A;
    pub const ERRORS: usize = 0x3C;
    pub const LASTCHECK: usize = 0x40;
    pub const CREATOR_OS: usize = 0x48;
    pub const REV_LEVEL: usize = 0x4C;
    pub const FIRST_INO: usize = 0x54;
    pub const INODE_SIZE: usize = 0x58;
    pub const FEATURE_COMPAT: usize = 0x5C;
    pub const FEATURE_INCOMPAT: usize = 0x60;
    pub const FEATURE_RO_COMPAT: usize = 0x64;
    pub const UUID: usize = 0x68;
    pub const VOLUME_NAME: usize = 0x78;
    pub const JOURNAL_INUM: usize = 0xE0;
    pub const LAST_ORPHAN: usize = 0xE8;
    pub const HASH_SEED: usize = 0xEC;
    pub const DEF_HASH_VERSION: usize = 0xFC;
    pub const JNL_BACKUP_TYPE: usize = 0xFD;
    pub const DESC_SIZE: usize = 0xFE;
    pub const MKFS_TIME: usize = 0x108;
    pub const JNL_BLOCKS: usize = 0x10C;
    pub const BLOCKS_COUNT_HI: usize = 0x150;
    pub const R_BLOCKS_COUNT_HI: usize = 0x154;
    pub const FREE_BLOCKS_HI: usize = 0x158;
    pub const MIN_EXTRA_ISIZE: usize = 0x15C;
    pub const WANT_EXTRA_ISIZE: usize = 0x15E;
    pub const FLAGS: usize = 0x160;
    pub const LOG_GROUPS_PER_FLEX: usize = 0x174;
    pub const CHECKSUM_TYPE: usize = 0x175;
    pub const CHECKSUM_SEED: usize = 0x270;
    pub const CHECKSUM: usize = 0x3FC;
}

/// A group descriptor's fields (struct ext4_group_desc).
pub mod gd {
    pub const BLOCK_BITMAP: usize = 0x00;
    pub const INODE_BITMAP: usize = 0x04;
    pub const INODE_TABLE: usize = 0x08;
    pub const FREE_BLOCKS: usize = 0x0C;
    pub const FREE_INODES: usize = 0x0E;
    pub const USED_DIRS: usize = 0x10;
    pub const FLAGS: usize = 0x12;
    pub const BLOCK_BITMAP_CSUM: usize = 0x18;
    pub const INODE_BITMAP_CSUM: usize = 0x1A;
    pub const ITABLE_UNUSED: usize = 0x1C;
    pub const CHECKSUM: usize = 0x1E;
    pub const BLOCK_BITMAP_HI: usize = 0x20;
    pub const INODE_BITMAP_HI: usize = 0x24;
    pub const INODE_TABLE_HI: usize = 0x28;
    pub const FREE_BLOCKS_HI: usize = 0x2C;
    pub const FREE_INODES_HI: usize = 0x2E;
    pub const USED_DIRS_HI: usize = 0x30;
    pub const ITABLE_UNUSED_HI: usize = 0x32;
    pub const BLOCK_BITMAP_CSUM_HI: usize = 0x38;
    pub const INODE_BITMAP_CSUM_HI: usize = 0x3A;
    pub const INODE_UNINIT: u16 = 0x1;
    pub const BLOCK_UNINIT: u16 = 0x2;
    pub const INODE_ZEROED: u16 = 0x4;
}

/// An inode's fields (struct ext4_inode).
pub mod ino {
    pub const MODE: usize = 0x00;
    pub const SIZE: usize = 0x04;
    pub const ATIME: usize = 0x08;
    pub const CTIME: usize = 0x0C;
    pub const MTIME: usize = 0x10;
    pub const DTIME: usize = 0x14;
    pub const LINKS: usize = 0x1A;
    pub const BLOCKS: usize = 0x1C;
    pub const FLAGS: usize = 0x20;
    pub const BLOCK: usize = 0x28;
    pub const GENERATION: usize = 0x64;
    pub const FILE_ACL: usize = 0x68;
    pub const SIZE_HIGH: usize = 0x6C;
    pub const BLOCKS_HIGH: usize = 0x74;
    pub const CHECKSUM_LO: usize = 0x7C;
    pub const EXTRA_ISIZE: usize = 0x80;
    pub const CHECKSUM_HI: usize = 0x82;
    pub const CRTIME: usize = 0x90;
    pub const EXTENTS_FL: u32 = 0x8_0000;
    pub const INDEX_FL: u32 = 0x1000;
}

pub const S_IFMT: u16 = 0xF000;
pub const S_IFREG: u16 = 0x8000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFLNK: u16 = 0xA000;
pub const S_IFCHR: u16 = 0x2000;
pub const FT_REG: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_CHRDEV: u8 = 3;
pub const FT_SYMLINK: u8 = 7;
/// The directory block tail's type, which no real entry has.
pub const FT_DIR_CSUM: u8 = 0xDE;

/// The extent tree's magic, and the most a written extent may be.
pub const EXT_MAGIC: u16 = 0xF30A;
pub const EXT_INIT_MAX_LEN: u32 = 32768;

/* CRC32C, the format's checksum: the running value in and out, as the
 * kernel chains it -- the fuzzer's own, as the rest is. */
const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC: [u32; 256] = crc_table();

pub fn crc32c(mut c: u32, data: &[u8]) -> u32 {
    for &b in data {
        c = CRC[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

pub fn rd64(b: &[u8], at: usize) -> u64 {
    rd32(b, at) as u64 | ((rd32(b, at + 4) as u64) << 32)
}

/* ---- the geometry ---- */

/// What an image is made of. Everything else -- where each group's
/// metadata is, how many descriptors there are -- follows from it.
#[derive(Clone, Debug)]
pub struct Geometry {
    /// A block is 1024 << this.
    pub log_block_size: u32,
    pub blocks: u64,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u16,
    /// 64-byte descriptors and 64-bit block numbers.
    pub bit64: bool,
    /// flex_bg's groups to a flex group, as a log; 0 for no flex_bg.
    pub log_groups_per_flex: u8,
    /// metadata_csum_seed: the checksums' seed kept in the superblock, the
    /// UUID changed after it was made.
    pub csum_seed: bool,
    /// The blocks of the journal: 0 for none.
    pub journal_blocks: u32,
    pub label: [u8; 16],
    pub uuid: [u8; 16],
}

/// Where a group's blocks are: its own start and length; the copy of the
/// superblock and descriptors it starts with, if it has one; and where its
/// bitmaps and inode table are -- in it, or with flex_bg in the first group
/// of its flex group.
#[derive(Clone, Copy, Debug)]
pub struct GroupLayout {
    pub start: u64,
    pub len: u64,
    pub has_super: bool,
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
}

fn power_of(mut n: u64, base: u64) -> bool {
    while n > 1 && n % base == 0 {
        n /= base;
    }
    n == 1
}

impl Geometry {
    pub fn block_size(&self) -> u64 {
        1024 << self.log_block_size
    }
    pub fn first_data_block(&self) -> u64 {
        (self.log_block_size == 0) as u64
    }
    pub fn groups(&self) -> u64 {
        (self.blocks - self.first_data_block()).div_ceil(self.blocks_per_group as u64)
    }
    pub fn desc_size(&self) -> u64 {
        if self.bit64 { 64 } else { 32 }
    }
    pub fn gdt_blocks(&self) -> u64 {
        (self.groups() * self.desc_size()).div_ceil(self.block_size())
    }
    pub fn itable_blocks(&self) -> u64 {
        self.inodes_per_group as u64 * self.inode_size as u64 / self.block_size()
    }
    pub fn inodes(&self) -> u64 {
        self.inodes_per_group as u64 * self.groups()
    }
    pub fn flex(&self) -> u64 {
        1 << self.log_groups_per_flex
    }
    /// sparse_super's: groups 0 and 1, and the powers of 3, 5 and 7.
    pub fn has_super(&self, g: u64) -> bool {
        g <= 1 || power_of(g, 3) || power_of(g, 5) || power_of(g, 7)
    }
    fn group_start(&self, g: u64) -> u64 {
        self.first_data_block() + g * self.blocks_per_group as u64
    }
    /// The first block a group's own copy of the superblock leaves free.
    fn after_super(&self, g: u64) -> u64 {
        self.group_start(g) + if self.has_super(g) { 1 + self.gdt_blocks() } else { 0 }
    }
    pub fn layout(&self, g: u64) -> GroupLayout {
        let start = self.group_start(g);
        let len = (self.blocks_per_group as u64).min(self.blocks - start);
        let (bb, ib, it) = if self.log_groups_per_flex == 0 {
            let bb = self.after_super(g);
            (bb, bb + 1, bb + 2)
        } else {
            /* Every group's of the flex group in its first group, after
             * its superblock: the block bitmaps, the inode bitmaps, the
             * inode tables, each in group order. */
            let leader = g - g % self.flex();
            let k = g - leader;
            let base = self.after_super(leader);
            let n = self.flex().min(self.groups() - leader);
            (base + k, base + n + k, base + 2 * n + k * self.itable_blocks())
        };
        GroupLayout { start, len, has_super: self.has_super(g), block_bitmap: bb, inode_bitmap: ib, inode_table: it }
    }

    /// The blocks of group `g`'s own that no file may have: its superblock
    /// copy and descriptors, and any group's bitmaps and inode table that
    /// lie in it.
    pub fn meta_in(&self, g: u64) -> Vec<u64> {
        let l = self.layout(g);
        let mut v: Vec<u64> = (l.start..self.after_super(g)).collect();
        for h in 0..self.groups() {
            let m = self.layout(h);
            for b in [m.block_bitmap, m.inode_bitmap].into_iter().chain(m.inode_table..m.inode_table + self.itable_blocks()) {
                if b >= l.start && b < l.start + l.len {
                    v.push(b);
                }
            }
        }
        v.sort_unstable();
        v
    }

    /// A geometry mke2fs could make and `mkfs` lays out: every group's
    /// metadata inside the filesystem and in the group it belongs in,
    /// room left over, and inodes for the reserved ones and a few more.
    pub fn valid(&self) -> bool {
        let bs = self.block_size();
        let isz = self.inode_size as u64;
        if self.log_block_size > 2 || !(isz == 128 || isz == 256) || isz > bs
            || self.blocks_per_group == 0 || self.blocks_per_group % 8 != 0 || self.blocks_per_group as u64 > 8 * bs
            || self.inodes_per_group == 0 || self.inodes_per_group % 8 != 0 || self.inodes_per_group as u64 > 8 * bs
            || (self.inodes_per_group as u64 * isz) % bs != 0
            || self.blocks <= self.first_data_block() + 1 || self.log_groups_per_flex > 4
            || (!self.bit64 && self.blocks > u32::MAX as u64)
        {
            return false;
        }
        if self.inodes() < FIRST_INO as u64 + 8 || self.inodes() > u32::MAX as u64 {
            return false;
        }
        /* Each group's metadata in the group it belongs in -- the flex
         * group's first -- with blocks to spare there, and the last group
         * not too short to hold its copy of the superblock. */
        (0..self.groups()).all(|g| {
            let l = self.layout(g);
            let home = if self.log_groups_per_flex == 0 { g } else { g - g % self.flex() };
            let h = self.layout(home);
            let end = h.start + h.len;
            l.inode_table + self.itable_blocks() < end && l.block_bitmap < end && l.inode_bitmap < end
                && self.after_super(g) < l.start + l.len
        })
    }

    pub fn features(&self) -> (u32, u32, u32) {
        let mut compat = COMPAT_EXT_ATTR | COMPAT_DIR_INDEX;
        if self.journal_blocks > 0 {
            compat |= COMPAT_HAS_JOURNAL;
        }
        let mut incompat = INCOMPAT_FILETYPE | INCOMPAT_EXTENTS;
        if self.bit64 {
            incompat |= INCOMPAT_64BIT;
        }
        if self.log_groups_per_flex > 0 {
            incompat |= INCOMPAT_FLEX_BG;
        }
        if self.csum_seed {
            incompat |= INCOMPAT_CSUM_SEED;
        }
        let mut ro = RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE | RO_COMPAT_HUGE_FILE | RO_COMPAT_DIR_NLINK
            | RO_COMPAT_METADATA_CSUM;
        if self.inode_size > 128 {
            ro |= RO_COMPAT_EXTRA_ISIZE;
        }
        (compat, incompat, ro)
    }
}
