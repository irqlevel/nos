//! Directory blocks. A linear one is a chain of entries -- inode, record
//! length, name length, type, name -- that covers the block, ending, with
//! metadata_csum, in a 12-byte tail entry holding the block's checksum. An
//! htree directory's first block is its index's root (the `.` and `..`
//! entries, the second spanning the block, the index inside it) and its
//! other index blocks look like one empty entry spanning the block; read as
//! linear, they name nothing -- which is how a reader without htree reads
//! one -- and with metadata_csum carry their checksum in a tail of their
//! own after the index's last possible entry.

use alloc::string::String;

use crate::crc::crc32c;
use crate::{le16, le32};

/// An entry's fixed part: inode, record length, name length, type.
pub const ENTRY_HEADER: usize = 8;
/// The tail entry metadata_csum ends a leaf block with.
pub const TAIL: usize = 12;
/// The type its tail entry has, which no real entry has.
pub const TAIL_FT: u8 = 0xDE;
/// The longest name.
pub const NAME_MAX: usize = 255;

/* What an entry's type says it names. */
pub const FT_UNKNOWN: u8 = 0;
pub const FT_REG_FILE: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_CHRDEV: u8 = 3;
pub const FT_BLKDEV: u8 = 4;
pub const FT_FIFO: u8 = 5;
pub const FT_SOCK: u8 = 6;
pub const FT_SYMLINK: u8 = 7;

/// An index's root: the `.` entry, the `..` entry's header and name, then
/// `dx_root_info`, which is 8 bytes.
pub const DX_ROOT_INFO: usize = 24;
pub const DX_ROOT_INFO_LENGTH: u8 = 8;
/// Where the count and limit are in an index's root and in its other
/// blocks.
pub const DX_ROOT_COUNT: usize = 32;
pub const DX_NODE_COUNT: usize = 8;
/// An index entry: a hash and a block.
pub const DX_ENTRY: usize = 8;
/// The tail after an index block's entries: a reserved word, the checksum.
pub const DX_TAIL: usize = 8;

/// A record length as stored: 0, or 65535, is a whole block of 64 KiB,
/// whose length does not fit (`ext4_rec_len_from_disk`).
pub fn rec_len_from_disk(len: u16, block_size: usize) -> usize {
    if block_size >= 65536 && (len == 0 || len == u16::MAX) {
        block_size
    } else {
        usize::from(len)
    }
}

/// One entry, as read from a block.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    /// Where it is in the block.
    pub at: usize,
    /// 0 for an entry that names nothing (a deleted one, a gap).
    pub ino: u32,
    pub rec_len: usize,
    pub file_type: u8,
    pub name: &'a [u8],
}

/// The entries of a linear block, each held to the block: a record that is
/// shorter than its header or than its name, not a multiple of four, or
/// that runs past the block ends the walk with an error. `dir` names the
/// directory in it.
pub struct Entries<'a> {
    block: &'a [u8],
    at: usize,
    dir: u32,
    done: bool,
}

pub fn entries(block: &[u8], dir: u32) -> Entries<'_> {
    Entries { block, at: 0, dir, done: false }
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<Entry<'a>, String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.at + ENTRY_HEADER > self.block.len() {
            return None;
        }
        let (b, at) = (self.block, self.at);
        let ino = le32(b, at);
        let rec_len = rec_len_from_disk(le16(b, at + 4), b.len());
        let name_len = usize::from(b[at + 6]);
        if rec_len < ENTRY_HEADER || rec_len % 4 != 0 || at + rec_len > b.len() || ENTRY_HEADER + name_len > rec_len {
            self.done = true;
            return Some(Err(alloc::format!("directory {}: an entry at {} of {} bytes, its name {}", self.dir, at,
                                           rec_len, name_len)));
        }
        self.at += rec_len;
        Some(Ok(Entry { at, ino, rec_len, file_type: b[at + 7],
                        name: &b[at + ENTRY_HEADER..at + ENTRY_HEADER + name_len] }))
    }
}

/// The inode named `name` among the entries of one linear block.
pub fn find_entry(block: &[u8], name: &[u8], dir: u32) -> Result<Option<u32>, String> {
    for e in entries(block, dir) {
        let e = e?;
        if e.ino != 0 && e.name == name {
            return Ok(Some(e.ino));
        }
    }
    Ok(None)
}

/// The checksum a leaf block's tail carries, if the block ends in one
/// (`get_dirent_tail`): an entry naming nothing, 12 bytes long, with no
/// name and the tail's type.
pub fn tail(block: &[u8]) -> Option<u32> {
    if block.len() < TAIL {
        return None;
    }
    let at = block.len() - TAIL;
    let is_tail = le32(block, at) == 0 && rec_len_from_disk(le16(block, at + 4), block.len()) == TAIL
        && block[at + 6] == 0 && block[at + 7] == TAIL_FT;
    is_tail.then(|| le32(block, at + 8))
}

/// The checksum a leaf block should carry: from the directory inode's seed
/// over everything before the tail (`ext4_dirblock_csum`).
pub fn leaf_csum(inode_seed: u32, block: &[u8]) -> u32 {
    crc32c(inode_seed, &block[..block.len().saturating_sub(TAIL)])
}

/// Where an index block's count and limit are, if it is one
/// (`get_dx_countlimit`): after one entry spanning the block -- an
/// interior block -- or after `.` and a `..` spanning the rest and a
/// `dx_root_info` of the right length -- the root.
pub fn dx_count_offset(block: &[u8]) -> Option<usize> {
    let bs = block.len();
    if bs < DX_ROOT_COUNT + 4 {
        return None;
    }
    let rlen = rec_len_from_disk(le16(block, 4), bs);
    if rlen == bs {
        return Some(DX_NODE_COUNT);
    }
    if rlen != 12 || rec_len_from_disk(le16(block, 12 + 4), bs) != bs - 12 {
        return None;
    }
    /* dx_root_info: reserved_zero (4), hash_version, info_length,
     * indirect_levels, unused_flags. */
    if le32(block, DX_ROOT_INFO) != 0 || block[DX_ROOT_INFO + 5] != DX_ROOT_INFO_LENGTH {
        return None;
    }
    Some(DX_ROOT_COUNT)
}

/// An index block's limit and count, from where `dx_count_offset` found
/// them.
pub fn dx_count_limit(block: &[u8], count_offset: usize) -> (u16, u16) {
    (le16(block, count_offset), le16(block, count_offset + 2))
}

/// The checksum an index block carries and the one it should
/// (`ext4_dx_csum`: from the inode's seed over the block up to its last
/// entry, then the tail's reserved word and a zero word) -- or None when
/// the block is no index block or has no room for its tail.
pub fn dx_csum(inode_seed: u32, block: &[u8]) -> Option<(u32, u32)> {
    let count_offset = dx_count_offset(block)?;
    let (limit, count) = dx_count_limit(block, count_offset);
    let tail = count_offset + usize::from(limit) * DX_ENTRY;
    let used = count_offset + usize::from(count) * DX_ENTRY;
    if tail + DX_TAIL > block.len() || used > tail {
        return None;
    }
    let mut c = crc32c(inode_seed, &block[..used]);
    c = crc32c(c, &block[tail..tail + 4]);
    c = crc32c(c, &[0, 0, 0, 0]);
    Some((le32(block, tail + 4), c))
}
