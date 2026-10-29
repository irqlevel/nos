//! The block map of ext2 and ext3: twelve direct pointers in the inode's
//! `i_block`, then a single, a double and a triple indirect block, each a
//! block of 32-bit pointers -- 0 a hole.

use alloc::string::String;
use alloc::vec;

use crate::{le32, Map, ReadBlock};

/// Pointers straight in the inode.
pub const DIRECT: usize = 12;
/// The levels of indirection: single, double, triple.
pub const LEVELS: usize = 3;

/// Where file block `lblock` of the file whose `i_block` is `i_block` is,
/// reading indirect blocks through `read`. A run is as long as consecutive
/// pointers of one array point at consecutive blocks; a hole as long as
/// what the missing pointer would have mapped from the block on -- and
/// never a block past it. `ino` names the file in an error.
pub fn map(i_block: &[u8], lblock: u64, block_size: usize, ino: u32, read: &mut ReadBlock) -> Result<Map, String> {
    let per = (block_size / 4) as u64;
    let direct = |i: usize| u64::from(le32(i_block, i * 4));
    if lblock < DIRECT as u64 {
        let i = lblock as usize;
        return Ok(run_of((i..DIRECT).map(direct)));
    }
    /* Which of the single, double and triple indirect trees the block is
     * in, and where in it: `cover` blocks the tree maps, `rest` the
     * block's place among them. */
    let mut rest = lblock - DIRECT as u64;
    let mut level = 1usize;
    let mut cover = per;
    while rest >= cover {
        rest -= cover;
        level += 1;
        if level > LEVELS {
            return Err(alloc::format!("inode {}: block {} past a block map's reach", ino, lblock));
        }
        cover = cover.saturating_mul(per);
    }
    let mut ptr = direct(DIRECT - 1 + level);
    let mut buf = vec![0u8; block_size];
    loop {
        if ptr == 0 {
            return Ok(Map::Hole { run: cover - rest });
        }
        read(ptr, &mut buf)?;
        let child = cover / per;
        let i = (rest / child) as usize;
        if child == 1 {
            return Ok(run_of((i..per as usize).map(|j| u64::from(le32(&buf, j * 4)))));
        }
        ptr = u64::from(le32(&buf, i * 4));
        rest %= child;
        cover = child;
    }
}

/// A run of blocks from the pointers of a block map, the first being the
/// block asked for: how many are consecutive, or how many holes.
pub fn run_of(mut ptrs: impl Iterator<Item = u64>) -> Map {
    let Some(first) = ptrs.next() else { return Map::Hole { run: 1 } };
    if first == 0 {
        let run = 1 + ptrs.take_while(|&p| p == 0).count() as u64;
        return Map::Hole { run };
    }
    let mut run = 1u64;
    for p in ptrs {
        if p != first + run {
            break;
        }
        run += 1;
    }
    Map::Blocks { block: first, run }
}
