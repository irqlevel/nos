//! The extent tree ext4 maps a file by: a node is a header and entries --
//! an index node's naming a child block by the first file block under it,
//! a leaf's a run of up to 32768 file blocks on consecutive device blocks
//! (or up to 32767 that are allocated but unwritten, which read as zeroes).
//! The root is the inode's `i_block`, room for four entries; every node
//! below it is a block, ending -- with metadata_csum -- in a checksum.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::crc::crc32c;
use crate::{le16, le32, Map, ReadBlock};

pub const MAGIC: u16 = 0xF30A;
pub const HEADER: usize = 12;
pub const ENTRY: usize = 12;
/// The deepest a tree may be: the kernel's `EXT4_MAX_EXTENT_DEPTH`.
pub const MAX_DEPTH: u16 = 5;
/// A leaf's length up to this is a written run of that many blocks; past
/// it, an unwritten run of the length less this.
pub const INIT_MAX_LEN: u32 = 32768;
/// The longest unwritten run.
pub const UNINIT_MAX_LEN: u32 = INIT_MAX_LEN - 1;
/// The root, in the inode's `i_block`.
pub const ROOT_BYTES: usize = 60;
/// The checksum after a block node's last possible entry.
pub const TAIL: usize = 4;

/// A node's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub entries: u16,
    pub max: u16,
    pub depth: u16,
    pub generation: u32,
}

/// Node `node`'s header, if it is one: the magic, no more entries than it
/// has room for, that room inside the node, and no deeper than the format
/// allows.
pub fn header(node: &[u8]) -> Result<Header, String> {
    if node.len() < HEADER || le16(node, 0) != MAGIC {
        return Err(String::from("an extent node with no header"));
    }
    let h = Header { entries: le16(node, 2), max: le16(node, 4), depth: le16(node, 6), generation: le32(node, 8) };
    if h.entries > h.max || HEADER + usize::from(h.max) * ENTRY > node.len() || h.depth > MAX_DEPTH {
        return Err(alloc::format!("an extent node of {} entries in {}, depth {}", h.entries, h.max, h.depth));
    }
    Ok(h)
}

/// A leaf entry: file blocks `first..first + len` on device blocks from
/// `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub first: u32,
    pub len: u32,
    pub start: u64,
    pub unwritten: bool,
}

/// An index entry: the child node at `child` maps the file from `first`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Index {
    pub first: u32,
    pub child: u64,
}

fn entry(node: &[u8], i: usize) -> &[u8] {
    &node[HEADER + i * ENTRY..HEADER + (i + 1) * ENTRY]
}

/// Leaf entry `i` of a node `header` has checked, `i` under its entries.
pub fn leaf(node: &[u8], i: usize) -> Leaf {
    let e = entry(node, i);
    let raw_len = u32::from(le16(e, 4));
    let unwritten = raw_len > INIT_MAX_LEN;
    Leaf {
        first: le32(e, 0),
        len: if unwritten { raw_len - INIT_MAX_LEN } else { raw_len },
        start: (u64::from(le16(e, 6)) << 32) | u64::from(le32(e, 8)),
        unwritten,
    }
}

/// Index entry `i` of a node `header` has checked.
pub fn index(node: &[u8], i: usize) -> Index {
    let e = entry(node, i);
    Index { first: le32(e, 0), child: (u64::from(le16(e, 8)) << 32) | u64::from(le32(e, 4)) }
}

/// Where a block node's checksum is: after the last entry it has room for.
pub fn tail_offset(max: u16) -> usize {
    HEADER + usize::from(max) * ENTRY
}

/// The checksum block node `node` should carry -- from the inode's seed
/// over everything before the tail (`ext4_extent_block_csum`) -- or None
/// when the node leaves no room for one.
pub fn node_csum(inode_seed: u32, node: &[u8]) -> Option<u32> {
    let h = header(node).ok()?;
    let at = tail_offset(h.max);
    (at + TAIL <= node.len()).then(|| crc32c(inode_seed, &node[..at]))
}

/// The checksum block node `node` carries, if it has room for one.
pub fn stored_csum(node: &[u8]) -> Option<u32> {
    let h = header(node).ok()?;
    let at = tail_offset(h.max);
    (at + TAIL <= node.len()).then(|| le32(node, at))
}

/// Where file block `lblock` of the file whose tree's root is `root` is,
/// following the tree down through `read`. A run never reaches past where
/// the next entry of any node on the way begins; a hole reaches to there,
/// or is a single block past the last entry. Every node on the way is held
/// to its header, each a level below its parent; a run is held inside a
/// filesystem of `fs_blocks` blocks. `ino` names the file in an error.
pub fn map(root: &[u8], lblock: u64, fs_blocks: u64, block_size: usize, ino: u32,
           read: &mut ReadBlock) -> Result<Map, String> {
    let mut node: Vec<u8> = root.to_vec();
    /* The blocks this subtree covers end where the next index entry up
     * begins: no run reaches past it. */
    let mut limit = u64::MAX;
    let mut depth_wanted: Option<u16> = None;
    loop {
        let h = header(&node).map_err(|e| alloc::format!("inode {}: {}", ino, e))?;
        if depth_wanted.is_some_and(|d| d != h.depth) {
            return Err(alloc::format!("inode {}: an extent node of depth {} where {} belongs", ino, h.depth,
                                      depth_wanted.unwrap_or(0)));
        }
        let entries = usize::from(h.entries);
        if h.depth == 0 {
            for i in 0..entries {
                let e = leaf(&node, i);
                let (first, len) = (u64::from(e.first), u64::from(e.len));
                if lblock < first {
                    return Ok(Map::Hole { run: (first - lblock).min(limit.saturating_sub(lblock)) });
                }
                if lblock - first < len {
                    let run = (len - (lblock - first)).min(limit.saturating_sub(lblock));
                    if e.unwritten {
                        return Ok(Map::Hole { run });
                    }
                    let block = e.start + (lblock - first);
                    if block.checked_add(run).is_none_or(|end| end > fs_blocks) {
                        return Err(alloc::format!("inode {}: an extent at block {} past the filesystem", ino,
                                                  e.start));
                    }
                    return Ok(Map::Blocks { block, run });
                }
            }
            return Ok(Map::Hole { run: limit.saturating_sub(lblock).max(1) });
        }
        /* The last index entry starting at or before the block. */
        let mut child = None;
        for i in 0..entries {
            let e = index(&node, i);
            if u64::from(e.first) > lblock {
                limit = limit.min(u64::from(e.first));
                break;
            }
            child = Some(e.child);
        }
        let Some(child) = child else {
            return Ok(Map::Hole { run: limit.saturating_sub(lblock).max(1) });
        };
        let mut next = vec![0u8; block_size];
        read(child, &mut next)?;
        node = next;
        depth_wanted = Some(h.depth - 1);
    }
}
