//! The superblock: 1024 bytes at byte 1024 of the filesystem (copies of it
//! start some of the groups), saying how big the filesystem is and how it
//! is laid out, which features it has, and -- with metadata_csum -- the
//! seed every other checksum starts from.

use alloc::string::String;

use crate::crc::crc32c;
use crate::features::{incompat, ro_compat};
use crate::{le16, le32, set_le16, set_le32};

/// Where the primary superblock is, in bytes from the start.
pub const OFFSET: u64 = 1024;
pub const SIZE: usize = 1024;
pub const MAGIC: u16 = 0xEF53;

/// The largest block the format has: 64 KiB, log 6 past 1 KiB.
pub const MAX_LOG_BLOCK_SIZE: u32 = 6;
/// The inode of revision 0, and the least any revision has.
pub const GOOD_OLD_INODE_SIZE: u64 = 128;
/// The first inode a file may have in revision 0; later ones say.
pub const GOOD_OLD_FIRST_INO: u32 = 11;
/// Group descriptors without and with 64bit.
pub const DESC_SIZE_32: u64 = 32;
pub const DESC_SIZE_64: u64 = 64;
/// `s_checksum_type` for CRC32C, the only kind there is.
pub const CHECKSUM_CRC32C: u8 = 1;

/// `s_state`: cleanly unmounted; errors detected.
pub const STATE_VALID: u16 = 0x1;
pub const STATE_ERROR: u16 = 0x2;

/* Offsets, as struct ext4_super_block has them. */
pub const S_INODES_COUNT: usize = 0x00;
pub const S_BLOCKS_COUNT_LO: usize = 0x04;
pub const S_R_BLOCKS_COUNT_LO: usize = 0x08;
pub const S_FREE_BLOCKS_COUNT_LO: usize = 0x0C;
pub const S_FREE_INODES_COUNT: usize = 0x10;
pub const S_FIRST_DATA_BLOCK: usize = 0x14;
pub const S_LOG_BLOCK_SIZE: usize = 0x18;
pub const S_LOG_CLUSTER_SIZE: usize = 0x1C;
pub const S_BLOCKS_PER_GROUP: usize = 0x20;
pub const S_CLUSTERS_PER_GROUP: usize = 0x24;
pub const S_INODES_PER_GROUP: usize = 0x28;
pub const S_MTIME: usize = 0x2C;
pub const S_WTIME: usize = 0x30;
pub const S_MNT_COUNT: usize = 0x34;
pub const S_MAX_MNT_COUNT: usize = 0x36;
pub const S_MAGIC: usize = 0x38;
pub const S_STATE: usize = 0x3A;
pub const S_ERRORS: usize = 0x3C;
pub const S_MINOR_REV_LEVEL: usize = 0x3E;
pub const S_LASTCHECK: usize = 0x40;
pub const S_CHECKINTERVAL: usize = 0x44;
pub const S_CREATOR_OS: usize = 0x48;
pub const S_REV_LEVEL: usize = 0x4C;
pub const S_FIRST_INO: usize = 0x54;
pub const S_INODE_SIZE: usize = 0x58;
pub const S_BLOCK_GROUP_NR: usize = 0x5A;
pub const S_FEATURE_COMPAT: usize = 0x5C;
pub const S_FEATURE_INCOMPAT: usize = 0x60;
pub const S_FEATURE_RO_COMPAT: usize = 0x64;
pub const S_UUID: usize = 0x68;
pub const S_VOLUME_NAME: usize = 0x78;
pub const S_LAST_MOUNTED: usize = 0x88;
pub const S_RESERVED_GDT_BLOCKS: usize = 0xCE;
pub const S_JOURNAL_UUID: usize = 0xD0;
pub const S_JOURNAL_INUM: usize = 0xE0;
pub const S_JOURNAL_DEV: usize = 0xE4;
pub const S_LAST_ORPHAN: usize = 0xE8;
pub const S_HASH_SEED: usize = 0xEC;
pub const S_DEF_HASH_VERSION: usize = 0xFC;
pub const S_JNL_BACKUP_TYPE: usize = 0xFD;
pub const S_DESC_SIZE: usize = 0xFE;
pub const S_DEFAULT_MOUNT_OPTS: usize = 0x100;
pub const S_FIRST_META_BG: usize = 0x104;
pub const S_MKFS_TIME: usize = 0x108;
pub const S_JNL_BLOCKS: usize = 0x10C;
pub const S_BLOCKS_COUNT_HI: usize = 0x150;
pub const S_R_BLOCKS_COUNT_HI: usize = 0x154;
pub const S_FREE_BLOCKS_COUNT_HI: usize = 0x158;
pub const S_MIN_EXTRA_ISIZE: usize = 0x15C;
pub const S_WANT_EXTRA_ISIZE: usize = 0x15E;
pub const S_FLAGS: usize = 0x160;
pub const S_LOG_GROUPS_PER_FLEX: usize = 0x174;
pub const S_CHECKSUM_TYPE: usize = 0x175;
pub const S_KBYTES_WRITTEN: usize = 0x178;
pub const S_ERROR_COUNT: usize = 0x194;
pub const S_FIRST_ERROR_TIME: usize = 0x198;
pub const S_LAST_ERROR_TIME: usize = 0x1CC;
pub const S_CHECKSUM_SEED: usize = 0x270;
pub const S_ORPHAN_FILE_INUM: usize = 0x280;
pub const S_CHECKSUM: usize = 0x3FC;

/// The words of `s_jnl_blocks`: the journal inode's `i_block`, then its
/// size, high and low.
pub const JNL_BLOCKS_WORDS: usize = 17;

/// A superblock, as read: a copy of its bytes, whose geometry has been
/// checked enough that what is worked out from it cannot overflow or
/// divide by zero.
#[derive(Clone)]
pub struct Super {
    raw: [u8; SIZE],
}

impl Super {
    /// The superblock in `bytes`, its first 1024: one with the magic and a
    /// geometry that holds together, or why not. What a driver further
    /// requires of one -- a block no bigger than it reads, the inode count
    /// exactly what the groups hold, the groups' tables inside the
    /// filesystem -- is the driver's to check.
    pub fn parse(bytes: &[u8]) -> Result<Super, String> {
        if bytes.len() < SIZE {
            return Err(alloc::format!("{} bytes of a superblock of {}", bytes.len(), SIZE));
        }
        let mut raw = [0u8; SIZE];
        raw.copy_from_slice(&bytes[..SIZE]);
        let sb = Super { raw };
        sb.check()?;
        Ok(sb)
    }

    fn check(&self) -> Result<(), String> {
        if self.magic() != MAGIC {
            return Err(String::from("no ext2/3/4 superblock"));
        }
        if self.log_block_size() > MAX_LOG_BLOCK_SIZE {
            return Err(alloc::format!("a block size of log {}, past the format's", self.log_block_size()));
        }
        let (blocks, first) = (self.blocks_count(), u64::from(self.first_data_block()));
        if blocks <= first || self.blocks_per_group() == 0 || self.inodes_count() == 0
            || self.inodes_per_group() == 0
        {
            return Err(String::from("a superblock with no blocks or no inodes"));
        }
        if self.group_count().checked_mul(u64::from(self.inodes_per_group()))
            .is_none_or(|n| n < u64::from(self.inodes_count()))
        {
            return Err(String::from("a superblock whose groups hold fewer inodes than it has"));
        }
        let (inode_size, block_size) = (self.inode_size(), self.block_size());
        if inode_size < GOOD_OLD_INODE_SIZE || !inode_size.is_power_of_two() || inode_size > block_size {
            return Err(alloc::format!("inodes of {} bytes", inode_size));
        }
        let desc_size = self.desc_size();
        if desc_size < DESC_SIZE_32 || !desc_size.is_power_of_two() || desc_size > block_size {
            return Err(alloc::format!("group descriptors of {} bytes", desc_size));
        }
        Ok(())
    }

    /// The superblock's bytes.
    pub fn raw(&self) -> &[u8; SIZE] {
        &self.raw
    }

    /// The superblock's bytes, to change: its checksum is then the
    /// writer's to set (`set_checksum`), and its geometry not to be
    /// changed -- nothing checks it again.
    pub fn raw_mut(&mut self) -> &mut [u8; SIZE] {
        &mut self.raw
    }

    #[inline]
    fn u16(&self, at: usize) -> u16 {
        le16(&self.raw, at)
    }

    #[inline]
    fn u32(&self, at: usize) -> u32 {
        le32(&self.raw, at)
    }

    /// A count of the 64bit feature's: the low word, and the high one when
    /// the filesystem has it.
    fn wide(&self, lo: usize, hi: usize) -> u64 {
        let mut v = u64::from(self.u32(lo));
        if self.is_64bit() {
            v |= u64::from(self.u32(hi)) << 32;
        }
        v
    }

    fn set_wide(&mut self, lo: usize, hi: usize, v: u64) {
        set_le32(&mut self.raw, lo, v as u32);
        if self.is_64bit() {
            set_le32(&mut self.raw, hi, (v >> 32) as u32);
        }
    }

    pub fn magic(&self) -> u16 {
        self.u16(S_MAGIC)
    }
    pub fn inodes_count(&self) -> u32 {
        self.u32(S_INODES_COUNT)
    }
    pub fn blocks_count(&self) -> u64 {
        self.wide(S_BLOCKS_COUNT_LO, S_BLOCKS_COUNT_HI)
    }
    pub fn r_blocks_count(&self) -> u64 {
        self.wide(S_R_BLOCKS_COUNT_LO, S_R_BLOCKS_COUNT_HI)
    }
    pub fn free_blocks_count(&self) -> u64 {
        self.wide(S_FREE_BLOCKS_COUNT_LO, S_FREE_BLOCKS_COUNT_HI)
    }
    pub fn set_free_blocks_count(&mut self, v: u64) {
        self.set_wide(S_FREE_BLOCKS_COUNT_LO, S_FREE_BLOCKS_COUNT_HI, v);
    }
    pub fn free_inodes_count(&self) -> u32 {
        self.u32(S_FREE_INODES_COUNT)
    }
    pub fn set_free_inodes_count(&mut self, v: u32) {
        set_le32(&mut self.raw, S_FREE_INODES_COUNT, v);
    }
    pub fn first_data_block(&self) -> u32 {
        self.u32(S_FIRST_DATA_BLOCK)
    }
    pub fn log_block_size(&self) -> u32 {
        self.u32(S_LOG_BLOCK_SIZE)
    }
    /// The block size in bytes. `parse` has checked its log.
    pub fn block_size(&self) -> u64 {
        1024u64 << self.log_block_size().min(MAX_LOG_BLOCK_SIZE)
    }
    pub fn log_cluster_size(&self) -> u32 {
        self.u32(S_LOG_CLUSTER_SIZE)
    }
    pub fn blocks_per_group(&self) -> u32 {
        self.u32(S_BLOCKS_PER_GROUP)
    }
    /// Clusters a group's block bitmap counts: blocks, without bigalloc.
    pub fn clusters_per_group(&self) -> u32 {
        if self.ro_compat() & ro_compat::BIGALLOC != 0 {
            self.u32(S_CLUSTERS_PER_GROUP)
        } else {
            self.blocks_per_group()
        }
    }
    pub fn inodes_per_group(&self) -> u32 {
        self.u32(S_INODES_PER_GROUP)
    }
    pub fn mtime(&self) -> u32 {
        self.u32(S_MTIME)
    }
    pub fn set_mtime(&mut self, v: u32) {
        set_le32(&mut self.raw, S_MTIME, v);
    }
    pub fn wtime(&self) -> u32 {
        self.u32(S_WTIME)
    }
    pub fn set_wtime(&mut self, v: u32) {
        set_le32(&mut self.raw, S_WTIME, v);
    }
    pub fn mnt_count(&self) -> u16 {
        self.u16(S_MNT_COUNT)
    }
    pub fn set_mnt_count(&mut self, v: u16) {
        set_le16(&mut self.raw, S_MNT_COUNT, v);
    }
    pub fn state(&self) -> u16 {
        self.u16(S_STATE)
    }
    pub fn set_state(&mut self, v: u16) {
        set_le16(&mut self.raw, S_STATE, v);
    }
    pub fn errors(&self) -> u16 {
        self.u16(S_ERRORS)
    }
    pub fn creator_os(&self) -> u32 {
        self.u32(S_CREATOR_OS)
    }
    pub fn rev_level(&self) -> u32 {
        self.u32(S_REV_LEVEL)
    }
    /// The first inode a file may have: those before it are reserved.
    pub fn first_ino(&self) -> u32 {
        if self.rev_level() == 0 { GOOD_OLD_FIRST_INO } else { self.u32(S_FIRST_INO) }
    }
    /// The bytes an inode takes in its table.
    pub fn inode_size(&self) -> u64 {
        if self.rev_level() == 0 { GOOD_OLD_INODE_SIZE } else { u64::from(self.u16(S_INODE_SIZE)) }
    }
    pub fn compat(&self) -> u32 {
        self.u32(S_FEATURE_COMPAT)
    }
    pub fn set_compat(&mut self, v: u32) {
        set_le32(&mut self.raw, S_FEATURE_COMPAT, v);
    }
    pub fn incompat(&self) -> u32 {
        self.u32(S_FEATURE_INCOMPAT)
    }
    pub fn set_incompat(&mut self, v: u32) {
        set_le32(&mut self.raw, S_FEATURE_INCOMPAT, v);
    }
    pub fn ro_compat(&self) -> u32 {
        self.u32(S_FEATURE_RO_COMPAT)
    }
    pub fn set_ro_compat(&mut self, v: u32) {
        set_le32(&mut self.raw, S_FEATURE_RO_COMPAT, v);
    }
    pub fn uuid(&self) -> [u8; 16] {
        let mut u = [0u8; 16];
        u.copy_from_slice(&self.raw[S_UUID..S_UUID + 16]);
        u
    }
    /// The volume label's 16 bytes, NUL-padded.
    pub fn volume_name(&self) -> [u8; 16] {
        let mut n = [0u8; 16];
        n.copy_from_slice(&self.raw[S_VOLUME_NAME..S_VOLUME_NAME + 16]);
        n
    }
    pub fn reserved_gdt_blocks(&self) -> u16 {
        self.u16(S_RESERVED_GDT_BLOCKS)
    }
    pub fn journal_uuid(&self) -> [u8; 16] {
        let mut u = [0u8; 16];
        u.copy_from_slice(&self.raw[S_JOURNAL_UUID..S_JOURNAL_UUID + 16]);
        u
    }
    pub fn journal_inum(&self) -> u32 {
        self.u32(S_JOURNAL_INUM)
    }
    pub fn journal_dev(&self) -> u32 {
        self.u32(S_JOURNAL_DEV)
    }
    /// The first inode of the orphan list, 0 for none.
    pub fn last_orphan(&self) -> u32 {
        self.u32(S_LAST_ORPHAN)
    }
    pub fn set_last_orphan(&mut self, v: u32) {
        set_le32(&mut self.raw, S_LAST_ORPHAN, v);
    }
    /// The four words seeding directory index hashes.
    pub fn hash_seed(&self) -> [u32; 4] {
        [self.u32(S_HASH_SEED), self.u32(S_HASH_SEED + 4), self.u32(S_HASH_SEED + 8), self.u32(S_HASH_SEED + 12)]
    }
    pub fn def_hash_version(&self) -> u8 {
        self.raw[S_DEF_HASH_VERSION]
    }
    pub fn jnl_backup_type(&self) -> u8 {
        self.raw[S_JNL_BACKUP_TYPE]
    }
    /// The bytes a group descriptor takes: 32, or what the superblock
    /// says with 64bit.
    pub fn desc_size(&self) -> u64 {
        if self.is_64bit() { u64::from(self.u16(S_DESC_SIZE)) } else { DESC_SIZE_32 }
    }
    pub fn first_meta_bg(&self) -> u32 {
        self.u32(S_FIRST_META_BG)
    }
    /// The backup of the journal inode's block map or extent root
    /// (`s_jnl_blocks`): its `i_block` words, then the size, high and low.
    pub fn jnl_blocks(&self) -> [u32; JNL_BLOCKS_WORDS] {
        let mut w = [0u32; JNL_BLOCKS_WORDS];
        for (i, v) in w.iter_mut().enumerate() {
            *v = self.u32(S_JNL_BLOCKS + 4 * i);
        }
        w
    }
    pub fn min_extra_isize(&self) -> u16 {
        self.u16(S_MIN_EXTRA_ISIZE)
    }
    pub fn want_extra_isize(&self) -> u16 {
        self.u16(S_WANT_EXTRA_ISIZE)
    }
    pub fn flags(&self) -> u32 {
        self.u32(S_FLAGS)
    }
    pub fn log_groups_per_flex(&self) -> u8 {
        self.raw[S_LOG_GROUPS_PER_FLEX]
    }
    pub fn checksum_type(&self) -> u8 {
        self.raw[S_CHECKSUM_TYPE]
    }
    pub fn error_count(&self) -> u32 {
        self.u32(S_ERROR_COUNT)
    }
    pub fn checksum_seed(&self) -> u32 {
        self.u32(S_CHECKSUM_SEED)
    }
    pub fn orphan_file_inum(&self) -> u32 {
        self.u32(S_ORPHAN_FILE_INUM)
    }
    pub fn checksum(&self) -> u32 {
        self.u32(S_CHECKSUM)
    }

    pub fn is_64bit(&self) -> bool {
        self.incompat() & incompat::BIT64 != 0
    }
    pub fn metadata_csum(&self) -> bool {
        self.ro_compat() & ro_compat::METADATA_CSUM != 0
    }
    pub fn gdt_csum(&self) -> bool {
        self.ro_compat() & ro_compat::GDT_CSUM != 0
    }
    pub fn huge_file(&self) -> bool {
        self.ro_compat() & ro_compat::HUGE_FILE != 0
    }
    pub fn largedir(&self) -> bool {
        self.incompat() & incompat::LARGEDIR != 0
    }

    /// How many block groups there are.
    pub fn group_count(&self) -> u64 {
        let first = u64::from(self.first_data_block());
        self.blocks_count().saturating_sub(first).div_ceil(u64::from(self.blocks_per_group().max(1)))
    }

    /// The seed metadata_csum's checksums start from: the superblock's own
    /// with metadata_csum_seed -- so that the UUID may change without
    /// rewriting every checksum -- else the UUID's CRC32C.
    pub fn csum_seed(&self) -> u32 {
        if self.incompat() & incompat::CSUM_SEED != 0 {
            self.checksum_seed()
        } else {
            crc32c(!0, &self.raw[S_UUID..S_UUID + 16])
        }
    }

    /// The superblock's checksum, as it should be: over every byte before
    /// it, from !0 (not from the seed).
    pub fn compute_checksum(&self) -> u32 {
        crc32c(!0, &self.raw[..S_CHECKSUM])
    }

    /// Whether the checksum it carries is right -- true where it carries
    /// none (no metadata_csum).
    pub fn checksum_ok(&self) -> bool {
        !self.metadata_csum() || self.checksum() == self.compute_checksum()
    }

    pub fn set_checksum(&mut self) {
        let c = self.compute_checksum();
        set_le32(&mut self.raw, S_CHECKSUM, c);
    }
}
