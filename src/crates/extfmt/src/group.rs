//! Group descriptors: where each block group's bitmaps and inode table are,
//! how much of it is free and whether its bitmaps and table have been
//! written at all (the uninit flags) -- and, with metadata_csum, a checksum
//! of the descriptor (gdt_csum's CRC16 before it) and the checksums of both
//! its bitmaps.

use crate::crc::{crc16, crc32c};
use crate::sb::{Super, DESC_SIZE_32, DESC_SIZE_64};
use crate::{le16, le32, set_le16, set_le32};

/// `bg_flags`: the inode bitmap and table were never written -- the bitmap
/// reads as all free; the block bitmap was never written -- it is what the
/// group's own metadata takes; the inode table is known zeroed.
pub const INODE_UNINIT: u16 = 0x1;
pub const BLOCK_UNINIT: u16 = 0x2;
pub const INODE_ZEROED: u16 = 0x4;

/* Offsets, as struct ext4_group_desc has them: the 32 bytes of every
 * descriptor, then the high halves 64bit adds. */
pub const BG_BLOCK_BITMAP_LO: usize = 0x00;
pub const BG_INODE_BITMAP_LO: usize = 0x04;
pub const BG_INODE_TABLE_LO: usize = 0x08;
pub const BG_FREE_BLOCKS_COUNT_LO: usize = 0x0C;
pub const BG_FREE_INODES_COUNT_LO: usize = 0x0E;
pub const BG_USED_DIRS_COUNT_LO: usize = 0x10;
pub const BG_FLAGS: usize = 0x12;
pub const BG_EXCLUDE_BITMAP_LO: usize = 0x14;
pub const BG_BLOCK_BITMAP_CSUM_LO: usize = 0x18;
pub const BG_INODE_BITMAP_CSUM_LO: usize = 0x1A;
pub const BG_ITABLE_UNUSED_LO: usize = 0x1C;
pub const BG_CHECKSUM: usize = 0x1E;
pub const BG_BLOCK_BITMAP_HI: usize = 0x20;
pub const BG_INODE_BITMAP_HI: usize = 0x24;
pub const BG_INODE_TABLE_HI: usize = 0x28;
pub const BG_FREE_BLOCKS_COUNT_HI: usize = 0x2C;
pub const BG_FREE_INODES_COUNT_HI: usize = 0x2E;
pub const BG_USED_DIRS_COUNT_HI: usize = 0x30;
pub const BG_ITABLE_UNUSED_HI: usize = 0x32;
pub const BG_EXCLUDE_BITMAP_HI: usize = 0x34;
pub const BG_BLOCK_BITMAP_CSUM_HI: usize = 0x38;
pub const BG_INODE_BITMAP_CSUM_HI: usize = 0x3A;

/// A descriptor holds a bitmap checksum's high half only when it reaches
/// past it (the kernel's `EXT4_BG_*_BITMAP_CSUM_HI_END`).
pub const BLOCK_BITMAP_CSUM_HI_END: usize = BG_BLOCK_BITMAP_CSUM_HI + 2;
pub const INODE_BITMAP_CSUM_HI_END: usize = BG_INODE_BITMAP_CSUM_HI + 2;

/// One descriptor: `desc_size` bytes of the descriptor table.
#[derive(Clone, Copy)]
pub struct Desc<'a> {
    raw: &'a [u8],
}

/// Its fields, from a descriptor's bytes of any size the format has.
fn wide32(raw: &[u8], lo: usize, hi: usize) -> u64 {
    let mut v = u64::from(le32(raw, lo));
    if raw.len() as u64 >= DESC_SIZE_64 {
        v |= u64::from(le32(raw, hi)) << 32;
    }
    v
}

fn wide16(raw: &[u8], lo: usize, hi: usize) -> u32 {
    let mut v = u32::from(le16(raw, lo));
    if raw.len() as u64 >= DESC_SIZE_64 {
        v |= u32::from(le16(raw, hi)) << 16;
    }
    v
}

impl<'a> Desc<'a> {
    /// A descriptor over `raw`: None when it is shorter than the least a
    /// descriptor is.
    pub fn new(raw: &'a [u8]) -> Option<Desc<'a>> {
        (raw.len() as u64 >= DESC_SIZE_32).then_some(Desc { raw })
    }
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }
    pub fn block_bitmap(&self) -> u64 {
        wide32(self.raw, BG_BLOCK_BITMAP_LO, BG_BLOCK_BITMAP_HI)
    }
    pub fn inode_bitmap(&self) -> u64 {
        wide32(self.raw, BG_INODE_BITMAP_LO, BG_INODE_BITMAP_HI)
    }
    pub fn inode_table(&self) -> u64 {
        wide32(self.raw, BG_INODE_TABLE_LO, BG_INODE_TABLE_HI)
    }
    pub fn free_blocks_count(&self) -> u32 {
        wide16(self.raw, BG_FREE_BLOCKS_COUNT_LO, BG_FREE_BLOCKS_COUNT_HI)
    }
    pub fn free_inodes_count(&self) -> u32 {
        wide16(self.raw, BG_FREE_INODES_COUNT_LO, BG_FREE_INODES_COUNT_HI)
    }
    pub fn used_dirs_count(&self) -> u32 {
        wide16(self.raw, BG_USED_DIRS_COUNT_LO, BG_USED_DIRS_COUNT_HI)
    }
    pub fn itable_unused(&self) -> u32 {
        wide16(self.raw, BG_ITABLE_UNUSED_LO, BG_ITABLE_UNUSED_HI)
    }
    pub fn flags(&self) -> u16 {
        le16(self.raw, BG_FLAGS)
    }
    /// The checksum the descriptor carries.
    pub fn checksum(&self) -> u16 {
        le16(self.raw, BG_CHECKSUM)
    }
    /// The block bitmap's checksum as carried, and how many of its bits
    /// the descriptor has room for (16 or 32).
    pub fn block_bitmap_csum(&self) -> (u32, u32) {
        csum_halves(self.raw, BG_BLOCK_BITMAP_CSUM_LO, BG_BLOCK_BITMAP_CSUM_HI, BLOCK_BITMAP_CSUM_HI_END)
    }
    pub fn inode_bitmap_csum(&self) -> (u32, u32) {
        csum_halves(self.raw, BG_INODE_BITMAP_CSUM_LO, BG_INODE_BITMAP_CSUM_HI, INODE_BITMAP_CSUM_HI_END)
    }
}

fn csum_halves(raw: &[u8], lo: usize, hi: usize, hi_end: usize) -> (u32, u32) {
    let mut v = u32::from(le16(raw, lo));
    if raw.len() >= hi_end {
        v |= u32::from(le16(raw, hi)) << 16;
        (v, 32)
    } else {
        (v, 16)
    }
}

/// A descriptor to change.
pub struct DescMut<'a> {
    raw: &'a mut [u8],
}

fn set_wide32(raw: &mut [u8], lo: usize, hi: usize, v: u64) {
    set_le32(raw, lo, v as u32);
    if raw.len() as u64 >= DESC_SIZE_64 {
        set_le32(raw, hi, (v >> 32) as u32);
    }
}

fn set_wide16(raw: &mut [u8], lo: usize, hi: usize, v: u32) {
    set_le16(raw, lo, v as u16);
    if raw.len() as u64 >= DESC_SIZE_64 {
        set_le16(raw, hi, (v >> 16) as u16);
    }
}

impl<'a> DescMut<'a> {
    pub fn new(raw: &'a mut [u8]) -> Option<DescMut<'a>> {
        (raw.len() as u64 >= DESC_SIZE_32).then_some(DescMut { raw })
    }
    pub fn view(&self) -> Desc<'_> {
        Desc { raw: self.raw }
    }
    pub fn set_free_blocks_count(&mut self, v: u32) {
        set_wide16(self.raw, BG_FREE_BLOCKS_COUNT_LO, BG_FREE_BLOCKS_COUNT_HI, v);
    }
    pub fn set_free_inodes_count(&mut self, v: u32) {
        set_wide16(self.raw, BG_FREE_INODES_COUNT_LO, BG_FREE_INODES_COUNT_HI, v);
    }
    pub fn set_used_dirs_count(&mut self, v: u32) {
        set_wide16(self.raw, BG_USED_DIRS_COUNT_LO, BG_USED_DIRS_COUNT_HI, v);
    }
    pub fn set_itable_unused(&mut self, v: u32) {
        set_wide16(self.raw, BG_ITABLE_UNUSED_LO, BG_ITABLE_UNUSED_HI, v);
    }
    pub fn set_flags(&mut self, v: u16) {
        set_le16(self.raw, BG_FLAGS, v);
    }
    pub fn set_block_bitmap(&mut self, v: u64) {
        set_wide32(self.raw, BG_BLOCK_BITMAP_LO, BG_BLOCK_BITMAP_HI, v);
    }
    pub fn set_inode_bitmap(&mut self, v: u64) {
        set_wide32(self.raw, BG_INODE_BITMAP_LO, BG_INODE_BITMAP_HI, v);
    }
    pub fn set_inode_table(&mut self, v: u64) {
        set_wide32(self.raw, BG_INODE_TABLE_LO, BG_INODE_TABLE_HI, v);
    }
    /// Store a bitmap's checksum, as much of it as the descriptor holds.
    pub fn set_block_bitmap_csum(&mut self, v: u32) {
        set_le16(self.raw, BG_BLOCK_BITMAP_CSUM_LO, v as u16);
        if self.raw.len() >= BLOCK_BITMAP_CSUM_HI_END {
            set_le16(self.raw, BG_BLOCK_BITMAP_CSUM_HI, (v >> 16) as u16);
        }
    }
    pub fn set_inode_bitmap_csum(&mut self, v: u32) {
        set_le16(self.raw, BG_INODE_BITMAP_CSUM_LO, v as u16);
        if self.raw.len() >= INODE_BITMAP_CSUM_HI_END {
            set_le16(self.raw, BG_INODE_BITMAP_CSUM_HI, (v >> 16) as u16);
        }
    }
    /// Put the descriptor's own checksum on it, if the filesystem has one.
    pub fn set_checksum(&mut self, sb: &Super, group: u32) {
        if let Some(c) = desc_csum(sb, group, self.raw) {
            set_le16(self.raw, BG_CHECKSUM, c);
        }
    }
}

/// The checksum descriptor `raw` of group `group` should carry -- None
/// where the filesystem has neither metadata_csum nor gdt_csum. With
/// metadata_csum: CRC32C from the seed over the group's number and the
/// descriptor, its checksum field taken as zero, cut to 16 bits; with
/// gdt_csum, CRC16 over the UUID, the number and the descriptor without
/// its checksum field (`ext4_group_desc_csum`).
pub fn desc_csum(sb: &Super, group: u32, raw: &[u8]) -> Option<u16> {
    let size = (sb.desc_size() as usize).min(raw.len());
    let le_group = group.to_le_bytes();
    let after = BG_CHECKSUM + 2;
    if sb.metadata_csum() {
        let mut c = crc32c(sb.csum_seed(), &le_group);
        c = crc32c(c, &raw[..BG_CHECKSUM]);
        c = crc32c(c, &[0, 0]);
        if after < size {
            c = crc32c(c, &raw[after..size]);
        }
        return Some((c & 0xFFFF) as u16);
    }
    if sb.gdt_csum() {
        let mut c = crc16(!0, &sb.uuid());
        c = crc16(c, &le_group);
        c = crc16(c, &raw[..BG_CHECKSUM]);
        if sb.is_64bit() && after < size {
            c = crc16(c, &raw[after..size]);
        }
        return Some(c);
    }
    None
}

/// The checksum a group's block bitmap should carry: CRC32C from the seed
/// over the bitmap's bits for the group's clusters. None when the bitmap
/// given is shorter than that -- a geometry the block cannot hold.
pub fn block_bitmap_csum(sb: &Super, bitmap: &[u8]) -> Option<u32> {
    let bytes = (sb.clusters_per_group() / 8) as usize;
    (bytes <= bitmap.len()).then(|| crc32c(sb.csum_seed(), &bitmap[..bytes]))
}

/// The checksum a group's inode bitmap should carry, over its bits for the
/// group's inodes.
pub fn inode_bitmap_csum(sb: &Super, bitmap: &[u8]) -> Option<u32> {
    let bytes = (sb.inodes_per_group() / 8) as usize;
    (bytes <= bitmap.len()).then(|| crc32c(sb.csum_seed(), &bitmap[..bytes]))
}

/// Whether a checksum carried in `bits` bits matches the one computed.
pub fn csum_matches(stored: u32, bits: u32, computed: u32) -> bool {
    if bits >= 32 { stored == computed } else { stored == computed & ((1u32 << bits) - 1) }
}
