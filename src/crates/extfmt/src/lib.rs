//! The ext2, ext3 and ext4 on-disk format, as bytes: the superblock, the
//! group descriptors, inodes, extent trees and block maps, directory blocks
//! and extended attribute blocks, each read through accessors over the
//! bytes it is stored in -- and every checksum metadata_csum, and gdt_csum
//! before it, puts on them, computed as the kernel computes it.
//!
//! There is no kernel in it and nothing unsafe: hvboot reads a guest's
//! /boot through it, the kernel's ext4 driver is to read and write through
//! it, and fuzz/extfmt holds it to the filesystems e2fsprogs makes and to
//! what e2fsck says of them damaged (scripts/extfmt-test.py).
//!
//! Everything on a disk is untrusted. A view is made only over a buffer
//! long enough for what it reads, and what a structure says of its own
//! size -- a descriptor's, an inode's, an extent node's entries, a
//! directory entry's length -- is checked before it is used, so that a
//! damaged disk is an error and never an index out of range. What a
//! filesystem may be used for -- read, written, or not at all -- is the
//! caller's to decide from what [`features::classify`] says.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod blockmap;
pub mod crc;
pub mod dir;
pub mod extent;
pub mod features;
pub mod group;
pub mod inode;
pub mod sb;
pub mod xattr;

use alloc::string::String;

/// Where a file's block is: a run of `run` blocks from `block` on the
/// device, or a hole of `run` blocks that reads as zeroes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Map {
    Blocks { block: u64, run: u64 },
    Hole { run: u64 },
}

/// How the walks that follow a tree's pointers read a block: whole block
/// `block` into the buffer, which is a block long. Whether the block is
/// inside the filesystem is the reader's to check -- it knows how big the
/// filesystem is.
pub type ReadBlock<'a> = dyn FnMut(u64, &mut [u8]) -> Result<(), String> + 'a;

/// The little-endian `u16` at `at`. Callers read only inside a buffer
/// whose length they have checked for the structure.
#[inline]
pub fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

#[inline]
pub fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[inline]
pub fn le64(b: &[u8], at: usize) -> u64 {
    u64::from(le32(b, at)) | (u64::from(le32(b, at + 4)) << 32)
}

#[inline]
pub fn set_le16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub fn set_le32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub fn set_le64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
