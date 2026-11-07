//! Inodes: `inode_size` bytes each in their group's table -- ext2's 128,
//! and past them ext4's extra fields (as many as `i_extra_isize` says) and
//! the extended attributes kept in the inode -- and, with metadata_csum, a
//! checksum of all of it, seeded by the inode's number and generation.

use crate::crc::crc32c;
use crate::{le16, le32};

/// ext2's inode, and the least any inode is.
pub const GOOD_OLD_SIZE: usize = 128;
/// `i_block`: a block map's fifteen pointers, an extent tree's root, a
/// fast symlink's target or inline data.
pub const I_BLOCK_BYTES: usize = 60;

/* Offsets, as struct ext4_inode has them. */
pub const I_MODE: usize = 0x00;
pub const I_UID: usize = 0x02;
pub const I_SIZE_LO: usize = 0x04;
pub const I_ATIME: usize = 0x08;
pub const I_CTIME: usize = 0x0C;
pub const I_MTIME: usize = 0x10;
pub const I_DTIME: usize = 0x14;
pub const I_GID: usize = 0x18;
pub const I_LINKS_COUNT: usize = 0x1A;
pub const I_BLOCKS_LO: usize = 0x1C;
pub const I_FLAGS: usize = 0x20;
pub const I_VERSION: usize = 0x24;
pub const I_BLOCK: usize = 0x28;
pub const I_GENERATION: usize = 0x64;
pub const I_FILE_ACL_LO: usize = 0x68;
pub const I_SIZE_HIGH: usize = 0x6C;
pub const I_BLOCKS_HIGH: usize = 0x74;
pub const I_FILE_ACL_HIGH: usize = 0x76;
pub const I_UID_HIGH: usize = 0x78;
pub const I_GID_HIGH: usize = 0x7A;
pub const I_CHECKSUM_LO: usize = 0x7C;
pub const I_EXTRA_ISIZE: usize = 0x80;
pub const I_CHECKSUM_HI: usize = 0x82;
pub const I_CTIME_EXTRA: usize = 0x84;
pub const I_MTIME_EXTRA: usize = 0x88;
pub const I_ATIME_EXTRA: usize = 0x8C;
pub const I_CRTIME: usize = 0x90;
pub const I_CRTIME_EXTRA: usize = 0x94;
pub const I_VERSION_HI: usize = 0x98;
pub const I_PROJID: usize = 0x9C;

/* `i_mode`'s type. */
pub const S_IFMT: u16 = 0xF000;
pub const S_IFSOCK: u16 = 0xC000;
pub const S_IFLNK: u16 = 0xA000;
pub const S_IFREG: u16 = 0x8000;
pub const S_IFBLK: u16 = 0x6000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFCHR: u16 = 0x2000;
pub const S_IFIFO: u16 = 0x1000;

/* `i_flags`. */
pub const INDEX_FL: u32 = 0x1000;
pub const HUGE_FILE_FL: u32 = 0x4_0000;
pub const EXTENTS_FL: u32 = 0x8_0000;
pub const EA_INODE_FL: u32 = 0x20_0000;
pub const INLINE_DATA_FL: u32 = 0x1000_0000;

/// An inode's bytes, all `inode_size` of them.
#[derive(Clone, Copy)]
pub struct Inode<'a> {
    raw: &'a [u8],
}

impl<'a> Inode<'a> {
    /// An inode over `raw`: None when it is shorter than an inode is.
    pub fn new(raw: &'a [u8]) -> Option<Inode<'a>> {
        (raw.len() >= GOOD_OLD_SIZE).then_some(Inode { raw })
    }
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }
    pub fn mode(&self) -> u16 {
        le16(self.raw, I_MODE)
    }
    pub fn file_type(&self) -> u16 {
        self.mode() & S_IFMT
    }
    pub fn is_dir(&self) -> bool {
        self.file_type() == S_IFDIR
    }
    pub fn is_file(&self) -> bool {
        self.file_type() == S_IFREG
    }
    pub fn is_link(&self) -> bool {
        self.file_type() == S_IFLNK
    }
    pub fn uid(&self) -> u32 {
        u32::from(le16(self.raw, I_UID)) | (u32::from(le16(self.raw, I_UID_HIGH)) << 16)
    }
    pub fn gid(&self) -> u32 {
        u32::from(le16(self.raw, I_GID)) | (u32::from(le16(self.raw, I_GID_HIGH)) << 16)
    }
    /// The size: its high word counts for a regular file, and for anything
    /// with largedir (`ext4_isize`) -- before largedir a directory's high
    /// word was ext2's `i_dir_acl`.
    pub fn size(&self, largedir: bool) -> u64 {
        let lo = u64::from(le32(self.raw, I_SIZE_LO));
        if largedir || self.is_file() {
            lo | (u64::from(le32(self.raw, I_SIZE_HIGH)) << 32)
        } else {
            lo
        }
    }
    pub fn atime(&self) -> u32 {
        le32(self.raw, I_ATIME)
    }
    pub fn ctime(&self) -> u32 {
        le32(self.raw, I_CTIME)
    }
    pub fn mtime(&self) -> u32 {
        le32(self.raw, I_MTIME)
    }
    /// When it was deleted -- or, on the orphan list, the next orphan.
    pub fn dtime(&self) -> u32 {
        le32(self.raw, I_DTIME)
    }
    pub fn links_count(&self) -> u16 {
        le16(self.raw, I_LINKS_COUNT)
    }
    /// What it takes on the device, in 512-byte sectors -- or, with
    /// HUGE_FILE_FL, in blocks: the 48 bits huge_file gives it, else 32.
    pub fn blocks(&self, huge_file: bool) -> u64 {
        let lo = u64::from(le32(self.raw, I_BLOCKS_LO));
        if huge_file { lo | (u64::from(le16(self.raw, I_BLOCKS_HIGH)) << 32) } else { lo }
    }
    pub fn flags(&self) -> u32 {
        le32(self.raw, I_FLAGS)
    }
    /// `i_block`'s 60 bytes.
    pub fn i_block(&self) -> &'a [u8] {
        &self.raw[I_BLOCK..I_BLOCK + I_BLOCK_BYTES]
    }
    pub fn generation(&self) -> u32 {
        le32(self.raw, I_GENERATION)
    }
    /// The block of its extended attributes, 0 for none.
    pub fn file_acl(&self) -> u64 {
        u64::from(le32(self.raw, I_FILE_ACL_LO)) | (u64::from(le16(self.raw, I_FILE_ACL_HIGH)) << 32)
    }
    /// How many bytes of extra fields follow the first 128 -- 0 for an
    /// inode that has no room for any.
    pub fn extra_isize(&self) -> u16 {
        if self.raw.len() >= I_EXTRA_ISIZE + 2 { le16(self.raw, I_EXTRA_ISIZE) } else { 0 }
    }
    /// Whether the extra fields reach the end of a field ending at byte
    /// `end` (`EXT4_FITS_IN_INODE`) -- and the inode does.
    pub fn fits(&self, end: usize) -> bool {
        self.raw.len() >= end && GOOD_OLD_SIZE + usize::from(self.extra_isize()) >= end
    }
    /// The checksum carried, and in how many bits: 32 where the extra
    /// fields reach `i_checksum_hi`, else the low 16.
    pub fn checksum(&self) -> (u32, u32) {
        let lo = u32::from(le16(self.raw, I_CHECKSUM_LO));
        if self.fits(I_CHECKSUM_HI + 2) {
            (lo | (u32::from(le16(self.raw, I_CHECKSUM_HI)) << 16), 32)
        } else {
            (lo, 16)
        }
    }
}

/// The seed of everything that belongs to inode `ino` -- its own checksum,
/// its extent blocks', its directory blocks' -- from the filesystem's
/// seed, the number and the generation.
pub fn seed(fs_seed: u32, ino: u32, generation: u32) -> u32 {
    crc32c(crc32c(fs_seed, &ino.to_le_bytes()), &generation.to_le_bytes())
}

/// The checksum inode `ino`, whose bytes are `raw`, should carry
/// (`ext4_inode_csum`): from its seed over every byte, its checksum fields
/// taken as zero -- the high one only where the extra fields reach it.
pub fn csum(fs_seed: u32, ino: u32, raw: &[u8]) -> u32 {
    let Some(inode) = Inode::new(raw) else { return 0 };
    let mut c = seed(fs_seed, ino, inode.generation());
    c = crc32c(c, &raw[..I_CHECKSUM_LO]);
    c = crc32c(c, &[0, 0]);
    c = crc32c(c, &raw[I_CHECKSUM_LO + 2..GOOD_OLD_SIZE]);
    if raw.len() > GOOD_OLD_SIZE {
        let hi = I_CHECKSUM_HI.min(raw.len());
        c = crc32c(c, &raw[GOOD_OLD_SIZE..hi]);
        let mut at = hi;
        if inode.fits(I_CHECKSUM_HI + 2) {
            c = crc32c(c, &[0, 0]);
            at += 2;
        }
        c = crc32c(c, &raw[at..]);
    }
    c
}

/// Whether inode `ino` carries the checksum it should.
pub fn csum_matches(fs_seed: u32, ino: u32, raw: &[u8]) -> bool {
    let Some(inode) = Inode::new(raw) else { return false };
    let (stored, bits) = inode.checksum();
    crate::group::csum_matches(stored, bits, csum(fs_seed, ino, raw))
}

/// Put on inode `ino` the checksum it should carry.
pub fn set_csum(fs_seed: u32, ino: u32, raw: &mut [u8]) {
    if raw.len() < GOOD_OLD_SIZE {
        return;
    }
    let c = csum(fs_seed, ino, raw);
    crate::set_le16(raw, I_CHECKSUM_LO, c as u16);
    if (Inode { raw: &*raw }).fits(I_CHECKSUM_HI + 2) {
        crate::set_le16(raw, I_CHECKSUM_HI, (c >> 16) as u16);
    }
}
