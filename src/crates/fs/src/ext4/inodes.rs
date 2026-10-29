//! An inode's fields as a call changes them, and a new inode as it is
//! written: whole, every field it has room for -- the extra ones past the
//! first 128 bytes among them -- so that nothing of what an uninitialised
//! table held before survives in it.

use alloc::vec;
use alloc::vec::Vec;

use extfmt::extent::{self, Header, ROOT_MAX};
use extfmt::inode::{self as xinode, EXTENTS_FL, HUGE_FILE_FL};
use extfmt::{le16, le32, set_le16, set_le32};

use kcore::trace;

use super::meta::Meta;

/// How long a directory's link count can be before dir_nlink holds it at 1
/// (`EXT4_LINK_MAX`).
pub const LINK_MAX: u16 = 65000;

/// The extra bytes past the first 128 a new inode gives itself: what the
/// fields this writes take, and what the superblock asks of new inodes,
/// within the inode.
fn extra_isize(m: &Meta) -> u16 {
    let room = (m.inode_size - xinode::GOOD_OLD_SIZE) as u16;
    m.sb.want_extra_isize().max(m.sb.min_extra_isize()).max(32).min(room) & !3
}

/// A new inode of `mode` and `links` links, made at `now`: extents for its
/// map, an empty root, the generation given.
pub fn new_inode(m: &Meta, mode: u16, links: u16, generation: u32, now: u32) -> Vec<u8> {
    let mut raw = vec![0u8; m.inode_size];
    set_le16(&mut raw, xinode::I_MODE, mode);
    set_le16(&mut raw, xinode::I_LINKS_COUNT, links);
    for at in [xinode::I_ATIME, xinode::I_CTIME, xinode::I_MTIME] {
        set_le32(&mut raw, at, now);
    }
    set_le32(&mut raw, xinode::I_FLAGS, EXTENTS_FL);
    extent::set_header(&mut raw[xinode::I_BLOCK..xinode::I_BLOCK + extent::ROOT_BYTES],
        Header { entries: 0, max: ROOT_MAX, depth: 0, generation: 0 });
    set_le32(&mut raw, xinode::I_GENERATION, generation);
    if m.inode_size > xinode::GOOD_OLD_SIZE {
        let extra = extra_isize(m);
        set_le16(&mut raw, xinode::I_EXTRA_ISIZE, extra);
        if xinode::GOOD_OLD_SIZE + usize::from(extra) >= xinode::I_CRTIME + 4 {
            set_le32(&mut raw, xinode::I_CRTIME, now);
        }
    }
    raw
}

pub fn links(raw: &[u8]) -> u16 {
    le16(raw, xinode::I_LINKS_COUNT)
}

pub fn set_links(raw: &mut [u8], n: u16) {
    set_le16(raw, xinode::I_LINKS_COUNT, n);
}

pub fn set_size(raw: &mut [u8], size: u64) {
    set_le32(raw, xinode::I_SIZE_LO, size as u32);
    set_le32(raw, xinode::I_SIZE_HIGH, (size >> 32) as u32);
}

/// Changed now: its status, and -- `content` -- what it holds.
pub fn touch(raw: &mut [u8], now: u32, content: bool) {
    set_le32(raw, xinode::I_CTIME, now);
    if content {
        set_le32(raw, xinode::I_MTIME, now);
    }
}

/// `blocks` filesystem blocks more -- or fewer -- counted in its i_blocks:
/// in 512-byte units, or in blocks for an inode huge_file flags so. False,
/// nothing changed, for a count its fields cannot hold: a file of more
/// than 2 TiB of blocks where the filesystem has no huge_file.
pub fn add_blocks(m: &Meta, raw: &mut [u8], blocks: i64) -> bool {
    let huge = m.huge_file() && le32(raw, xinode::I_FLAGS) & HUGE_FILE_FL != 0;
    let units = if huge { blocks } else { blocks * (m.bs as i64 / 512) };
    let have = u64::from(le32(raw, xinode::I_BLOCKS_LO))
        | if m.huge_file() { u64::from(le16(raw, xinode::I_BLOCKS_HIGH)) << 32 } else { 0 };
    let room = if m.huge_file() { (1u64 << 48) - 1 } else { u64::from(u32::MAX) };
    let Some(total) = have.checked_add_signed(units).filter(|&t| t <= room) else {
        trace!(0, "ext4: an inode's block count past what its fields hold");
        return false;
    };
    set_le32(raw, xinode::I_BLOCKS_LO, total as u32);
    if m.huge_file() {
        set_le16(raw, xinode::I_BLOCKS_HIGH, (total >> 32) as u16);
    }
    true
}

/// Deleted at `now`: no links, nothing mapped, nothing counted -- what the
/// kernel leaves of an inode it frees, whose blocks are given back.
pub fn delete(raw: &mut [u8], now: u32) {
    set_links(raw, 0);
    set_size(raw, 0);
    set_le32(raw, xinode::I_DTIME, now);
    set_le32(raw, xinode::I_BLOCKS_LO, 0);
    set_le16(raw, xinode::I_BLOCKS_HIGH, 0);
    set_le32(raw, xinode::I_FILE_ACL_LO, 0);
    set_le16(raw, xinode::I_FILE_ACL_HIGH, 0);
    set_le32(raw, xinode::I_CTIME, now);
    let flags = le32(raw, xinode::I_FLAGS);
    let root = &mut raw[xinode::I_BLOCK..xinode::I_BLOCK + extent::ROOT_BYTES];
    root.fill(0);
    if flags & EXTENTS_FL != 0 {
        extent::set_header(root, Header { entries: 0, max: ROOT_MAX, depth: 0, generation: 0 });
    }
}
