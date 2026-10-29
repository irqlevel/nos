//! An extended attribute block: the attributes that do not fit in their
//! inode, behind a header that counts the inodes sharing the block -- a
//! block identical for several files is kept once -- and, with
//! metadata_csum, a checksum seeded by the block's own number, since no one
//! inode owns it.

use crate::crc::crc32c;
use crate::{le32, set_le32};

pub const MAGIC: u32 = 0xEA02_0000;
/* The header: magic, references, blocks (always 1), hash, checksum, then
 * reserved words to 32 bytes. */
pub const H_MAGIC: usize = 0x00;
pub const H_REFCOUNT: usize = 0x04;
pub const H_BLOCKS: usize = 0x08;
pub const H_HASH: usize = 0x0C;
pub const H_CHECKSUM: usize = 0x10;
pub const HEADER: usize = 32;

pub fn magic(block: &[u8]) -> u32 {
    le32(block, H_MAGIC)
}

/// How many inodes name the block.
pub fn refcount(block: &[u8]) -> u32 {
    le32(block, H_REFCOUNT)
}

pub fn stored_csum(block: &[u8]) -> u32 {
    le32(block, H_CHECKSUM)
}

/// The checksum block `number`, whose bytes are `block`, should carry
/// (`ext4_xattr_block_csum`): from the filesystem's seed over the block's
/// number, then the block, its checksum field taken as zero. None for a
/// buffer too short to be one.
pub fn block_csum(fs_seed: u32, number: u64, block: &[u8]) -> Option<u32> {
    if block.len() < HEADER {
        return None;
    }
    let mut c = crc32c(fs_seed, &number.to_le_bytes());
    c = crc32c(c, &block[..H_CHECKSUM]);
    c = crc32c(c, &[0, 0, 0, 0]);
    Some(crc32c(c, &block[H_CHECKSUM + 4..]))
}

pub fn set_refcount(block: &mut [u8], v: u32) {
    set_le32(block, H_REFCOUNT, v);
}

/// Put on attribute block `number` the checksum it should carry: false for
/// a buffer too short to be one.
pub fn set_block_csum(fs_seed: u32, number: u64, block: &mut [u8]) -> bool {
    let Some(c) = block_csum(fs_seed, number, block) else { return false };
    set_le32(block, H_CHECKSUM, c);
    true
}
