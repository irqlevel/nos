//! nanofs: the kernel's own small checksummed filesystem.
//!
//! 1024 inodes, 16384 data blocks of 4 KiB, files up to 1 MiB, directories
//! up to 256 entries. It predates ext2 write support and remains for what it
//! is good at: a small store that says when it has been damaged. Every block
//! carries a CRC32 over itself, a file carries one over its data, and every
//! write is copy-on-write with the inode committed last -- so a crash leaves
//! either the old file or the new one, never a mix.
//!
//! The whole tree is read into vnodes at mount, which is what 1024 inodes
//! buys: there is no directory to load later, and a lookup is a walk of a
//! list that is already in memory.
//!
//! Every call arrives with the VFS lock held (see `FsOps`), so there is no
//! locking here.

use alloc::boxed::Box;
use alloc::vec::Vec;

use kcore::block::Disk;
use kcore::crc32::{crc32_update, crc32_with_hole};
use kcore::dma::DmaBuffer;
use kcore::trace;

use crate::vfs::FsOps;
use crate::vnode::{self, VNode, FLAG_DIR_LOADED, NAME_MAX, TYPE_DIR, TYPE_FILE};

pub const MAGIC: u32 = 0x4E41_4E4F; // "NANO"
pub const VERSION: u32 = 1;
pub const BLOCK_SIZE: usize = 4096;
pub const INODE_COUNT: u32 = 1024;
pub const DATA_BLOCK_COUNT: u32 = 16384;
pub const INODE_START: u32 = 1;
pub const DATA_START: u32 = 1 + INODE_COUNT;
pub const MAX_BLOCKS: usize = 256;
pub const MAX_DIR_ENTRIES: usize = 256;
pub const MAX_FILE_SIZE: usize = MAX_BLOCKS * BLOCK_SIZE;
/// Recursion cap for the mount-time tree walk (32 KiB kernel stack)
pub const MAX_DIR_DEPTH: u32 = 32;

const TYPE_FREE: u32 = 0;
const INODE_TYPE_FILE: u32 = 1;
const INODE_TYPE_DIR: u32 = 2;

/* Where things are in the superblock block (NanoSuperBlock) */
const SB_MAGIC: usize = 0;
const SB_VERSION: usize = 4;
const SB_UUID: usize = 8;
const SB_CHECKSUM: usize = 24;
const SB_BLOCK_SIZE: usize = 28;
const SB_INODE_COUNT: usize = 32;
const SB_DATA_BLOCK_COUNT: usize = 36;
const SB_INODE_START: usize = 40;
const SB_DATA_START: usize = 44;
const SB_INODE_BITMAP: usize = 48;
const SB_INODE_BITMAP_LEN: usize = 128;
const SB_DATA_BITMAP: usize = SB_INODE_BITMAP + SB_INODE_BITMAP_LEN;
const SB_DATA_BITMAP_LEN: usize = 2048;

/* Where things are in an inode block (NanoInode) */
const IN_TYPE: usize = 0;
const IN_SIZE: usize = 4;
const IN_NAME: usize = 8;
const IN_NAME_LEN: usize = 64;
const IN_PARENT: usize = 72;
const IN_CHECKSUM: usize = 76;
const IN_DATA_CHECKSUM: usize = 80;
const IN_BLOCKS: usize = 84;

/* A directory entry is an inode index and four reserved bytes */
const DIR_ENTRY_SIZE: usize = 8;

/* ---- little-endian scalars in a block ---- */

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn wr_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/* ---- the allocation bitmaps ----
 *
 * Bit i is bit (i % 8) of byte (i / 8), which is what Stdlib::Bitmap's
 * 64-bit words come to on a little-endian machine -- and what
 * scripts/mkfs_nanofs.py writes. */

fn bit_test(map: &[u8], bit: usize) -> bool {
    map[bit / 8] & (1u8 << (bit % 8)) != 0
}

fn bit_set(map: &mut [u8], bit: usize) {
    map[bit / 8] |= 1u8 << (bit % 8);
}

fn bit_clear(map: &mut [u8], bit: usize) {
    map[bit / 8] &= !(1u8 << (bit % 8));
}

/// The first clear bit below `count`, set, or None when there is none.
fn take_free_bit(map: &mut [u8], count: u32) -> Option<u32> {
    for byte in 0..(count as usize + 7) / 8 {
        if map[byte] == 0xFF {
            continue;
        }
        for bit in 0..8 {
            let idx = byte * 8 + bit;
            if idx >= count as usize {
                break;
            }
            if !bit_test(map, idx) {
                bit_set(map, idx);
                return Some(idx as u32);
            }
        }
    }
    None
}

/* ---- block I/O ---- */

struct Io {
    dev: Disk,
    sectors_per_block: u32,
}

impl Io {
    fn read_block(&self, block: u32, buf: &mut [u8]) -> bool {
        let start = block as u64 * self.sectors_per_block as u64;
        if self.dev.read(start, &mut buf[..BLOCK_SIZE]).is_ok() {
            return true;
        }
        trace!(0, "nanofs: read of block {} failed", block);
        false
    }

    fn write_block(&self, block: u32, buf: &[u8], fua: bool) -> bool {
        let start = block as u64 * self.sectors_per_block as u64;
        if self.dev.write(start, &buf[..BLOCK_SIZE], fua).is_ok() {
            return true;
        }
        trace!(0, "nanofs: write of block {} failed", block);
        false
    }

    fn flush(&self) -> bool {
        self.dev.flush().is_ok()
    }
}

/* ---- the filesystem ---- */

pub struct NanoFs {
    io: Io,
    /// The superblock, as it is on disk
    sb: DmaBuffer,
    /// One inode block
    inode: DmaBuffer,
    /// One data block
    data: DmaBuffer,
    /// The inode of a directory being added to or taken from, which is never
    /// the inode a caller is holding in `inode`
    dir_inode: DmaBuffer,
    /// The vnode of each inode, by index; the whole tree, made at mount
    vnodes: Vec<*mut VNode>,
    /// Inodes whose walk is still on the recursion stack. A directory entry
    /// naming one of those is a cycle in the image; linking it would put a
    /// cycle in the VFS tree.
    walking: Vec<bool>,
    mounted: bool,
}

impl NanoFs {
    pub fn new(dev: Disk) -> Option<Box<NanoFs>> {
        let sector_size = dev.sector_size();
        if sector_size == 0 || BLOCK_SIZE as u64 % sector_size != 0 {
            trace!(0, "nanofs: a sector size of {} does not divide a block", sector_size);
            return None;
        }

        let mut vnodes = Vec::new();
        let mut walking = Vec::new();
        if vnodes.try_reserve_exact(INODE_COUNT as usize).is_err()
            || walking.try_reserve_exact(INODE_COUNT as usize).is_err()
        {
            trace!(0, "nanofs: no memory for the vnode table");
            return None;
        }
        vnodes.resize(INODE_COUNT as usize, core::ptr::null_mut());
        walking.resize(INODE_COUNT as usize, false);

        Some(Box::new(NanoFs {
            io: Io { dev, sectors_per_block: (BLOCK_SIZE as u64 / sector_size) as u32 },
            sb: DmaBuffer::new(1)?,
            inode: DmaBuffer::new(1)?,
            data: DmaBuffer::new(1)?,
            dir_inode: DmaBuffer::new(1)?,
            vnodes,
            walking,
            mounted: false,
        }))
    }

    /* ---- the superblock ---- */

    fn sb_u32(&self, off: usize) -> u32 {
        rd_u32(self.sb.as_slice(), off)
    }

    fn inode_bitmap(&mut self) -> &mut [u8] {
        &mut self.sb.as_mut_slice()[SB_INODE_BITMAP..SB_INODE_BITMAP + SB_INODE_BITMAP_LEN]
    }

    fn data_bitmap(&mut self) -> &mut [u8] {
        &mut self.sb.as_mut_slice()[SB_DATA_BITMAP..SB_DATA_BITMAP + SB_DATA_BITMAP_LEN]
    }

    /// Put the superblock on disk, checksum first. Everything that changes
    /// an allocation bitmap commits it this way.
    fn flush_super(&mut self) -> bool {
        let sum = crc32_with_hole(&self.sb.as_slice()[..BLOCK_SIZE], SB_CHECKSUM);
        wr_u32(self.sb.as_mut_slice(), SB_CHECKSUM, sum);
        self.io.write_block(0, self.sb.as_slice(), true)
    }

    /* ---- inodes ---- */

    fn read_inode(&mut self, idx: u32) -> bool {
        if idx >= INODE_COUNT {
            trace!(0, "nanofs: inode {} is out of range", idx);
            return false;
        }
        self.io.read_block(INODE_START + idx, self.inode.as_mut_slice())
    }

    fn write_inode(&mut self, idx: u32, fua: bool) -> bool {
        if idx >= INODE_COUNT {
            trace!(0, "nanofs: inode {} is out of range", idx);
            return false;
        }
        let sum = crc32_with_hole(&self.inode.as_slice()[..BLOCK_SIZE], IN_CHECKSUM);
        wr_u32(self.inode.as_mut_slice(), IN_CHECKSUM, sum);
        self.io.write_block(INODE_START + idx, self.inode.as_slice(), fua)
    }

    /// Whether the inode block in hand carries the CRC it should.
    fn inode_ok(&self) -> bool {
        let block = &self.inode.as_slice()[..BLOCK_SIZE];
        crc32_with_hole(block, IN_CHECKSUM) == rd_u32(block, IN_CHECKSUM)
    }

    fn in_u32(&self, off: usize) -> u32 {
        rd_u32(self.inode.as_slice(), off)
    }

    fn in_set_u32(&mut self, off: usize, v: u32) {
        wr_u32(self.inode.as_mut_slice(), off, v);
    }

    fn in_block(&self, index: usize) -> u32 {
        rd_u32(self.inode.as_slice(), IN_BLOCKS + index * 4)
    }

    fn in_set_block(&mut self, index: usize, v: u32) {
        wr_u32(self.inode.as_mut_slice(), IN_BLOCKS + index * 4, v);
    }

    /* ---- allocation ----
     *
     * Taking a slot only sets the bit in memory; the caller commits the
     * bitmap with flush_super *after* the inode or block contents are on
     * disk, so a crash in between leaves the slot free rather than leaked.
     * Giving one back commits at once. */

    fn take_inode(&mut self) -> Option<u32> {
        match take_free_bit(self.inode_bitmap(), INODE_COUNT) {
            Some(idx) => Some(idx),
            None => {
                trace!(0, "nanofs: no free inodes");
                None
            }
        }
    }

    fn give_back_inode(&mut self, idx: u32) {
        if idx >= INODE_COUNT {
            return;
        }
        bit_clear(self.inode_bitmap(), idx as usize);
        self.flush_super();
    }

    fn take_data_block(&mut self) -> Option<u32> {
        match take_free_bit(self.data_bitmap(), DATA_BLOCK_COUNT) {
            Some(idx) => Some(idx),
            None => {
                trace!(0, "nanofs: no free data blocks");
                None
            }
        }
    }

    fn give_back_data_block(&mut self, idx: u32) {
        if idx >= DATA_BLOCK_COUNT {
            return;
        }
        bit_clear(self.data_bitmap(), idx as usize);
        self.flush_super();
    }

    /* ---- mount ---- */

    pub fn mount(&mut self) -> bool {
        if self.mounted {
            trace!(0, "nanofs: already mounted");
            return false;
        }

        if !self.io.read_block(0, self.sb.as_mut_slice()) {
            trace!(0, "nanofs: the superblock could not be read");
            return false;
        }

        if self.sb_u32(SB_MAGIC) != MAGIC {
            trace!(0, "nanofs: bad magic {:#x}", self.sb_u32(SB_MAGIC));
            return false;
        }
        if self.sb_u32(SB_VERSION) != VERSION {
            trace!(0, "nanofs: unsupported version {}", self.sb_u32(SB_VERSION));
            return false;
        }

        let sum = crc32_with_hole(&self.sb.as_slice()[..BLOCK_SIZE], SB_CHECKSUM);
        if sum != self.sb_u32(SB_CHECKSUM) {
            trace!(0, "nanofs: the superblock's checksum does not match");
            return false;
        }

        /* Every read and write below uses the fixed layout, so an image
         * claiming another one (an inode table starting at block 0, say,
         * which would alias the superblock) is refused rather than
         * followed. */
        if self.sb_u32(SB_BLOCK_SIZE) != BLOCK_SIZE as u32
            || self.sb_u32(SB_INODE_COUNT) != INODE_COUNT
            || self.sb_u32(SB_DATA_BLOCK_COUNT) != DATA_BLOCK_COUNT
            || self.sb_u32(SB_INODE_START) != INODE_START
            || self.sb_u32(SB_DATA_START) != DATA_START
        {
            trace!(0, "nanofs: unsupported layout (block size {}, {} inodes, {} data blocks, inodes at {}, data at {})",
                self.sb_u32(SB_BLOCK_SIZE), self.sb_u32(SB_INODE_COUNT),
                self.sb_u32(SB_DATA_BLOCK_COUNT), self.sb_u32(SB_INODE_START),
                self.sb_u32(SB_DATA_START));
            return false;
        }

        if self.walk(0, 0).is_null() {
            trace!(0, "nanofs: the root inode could not be read");
            self.free_all();
            return false;
        }

        self.mark_reachable_allocated();

        self.mounted = true;
        trace!(0, "nanofs: mounted, {} inodes, {} data blocks", INODE_COUNT, DATA_BLOCK_COUNT);
        true
    }

    pub fn unmount(&mut self) {
        if !self.mounted {
            return;
        }

        self.flush_super();
        self.io.flush();
        self.mounted = false;
        self.free_all();
    }

    fn free_all(&mut self) {
        for i in 0..self.vnodes.len() {
            let node = self.vnodes[i];
            if !node.is_null() {
                /* Each vnode is freed on its own: they are all in the table,
                 * so a tree walk would free them twice. */
                unsafe { vnode::free(node) };
                self.vnodes[i] = core::ptr::null_mut();
            }
        }
    }

    pub fn sync(&mut self) -> bool {
        if !self.mounted {
            return true;
        }
        self.io.flush()
    }

    /// Checksums are integrity, not authentication: a crafted image can carry
    /// a valid, checksummed, reachable inode whose allocation bit is clear,
    /// or live data blocks unmarked in the data bitmap. Taking a slot would
    /// then hand out live metadata and the next create or write would clobber
    /// the tree -- the root directory's own data block, say. So everything
    /// reachable from the root is marked allocated here, at mount, once the
    /// walk has filled the vnode table; the repair persists with the next
    /// superblock flush.
    fn mark_reachable_allocated(&mut self) {
        let mut repaired = false;

        for i in 0..INODE_COUNT {
            if self.vnodes[i as usize].is_null() {
                continue;
            }

            if !bit_test(self.inode_bitmap(), i as usize) {
                trace!(0, "nanofs: reachable inode {} was not marked allocated, repairing", i);
                bit_set(self.inode_bitmap(), i as usize);
                repaired = true;
            }

            if !self.read_inode(i) {
                continue;
            }

            let size = self.in_u32(IN_SIZE) as usize;
            let used = if self.in_u32(IN_TYPE) == INODE_TYPE_DIR {
                if size > 0 { 1 } else { 0 }
            } else {
                (size + BLOCK_SIZE - 1) / BLOCK_SIZE
            }
            .min(MAX_BLOCKS);

            for j in 0..used {
                let block = self.in_block(j);
                if block >= DATA_BLOCK_COUNT || bit_test(self.data_bitmap(), block as usize) {
                    continue;
                }
                trace!(0, "nanofs: live data block {} (inode {}) was not marked allocated, repairing",
                    block, i);
                bit_set(self.data_bitmap(), block as usize);
                repaired = true;
            }
        }

        if repaired {
            self.flush_super();
        }
    }

    /// Read the inode's vnode and, for a directory, everything under it.
    /// Null when the inode is free, damaged or out of range.
    fn walk(&mut self, idx: u32, depth: u32) -> *mut VNode {
        let null = core::ptr::null_mut();
        if idx >= INODE_COUNT {
            trace!(0, "nanofs: inode {} is out of range", idx);
            return null;
        }
        if depth >= MAX_DIR_DEPTH {
            trace!(0, "nanofs: the directory depth limit of {} was reached at inode {}",
                MAX_DIR_DEPTH, idx);
            return null;
        }
        if !self.vnodes[idx as usize].is_null() {
            return self.vnodes[idx as usize];
        }

        if !self.read_inode(idx) {
            return null;
        }
        let node_type = self.in_u32(IN_TYPE);
        if node_type == TYPE_FREE {
            trace!(0, "nanofs: inode {} is free", idx);
            return null;
        }
        if !self.inode_ok() {
            trace!(0, "nanofs: inode {}'s checksum does not match", idx);
            return null;
        }

        let is_dir = node_type == INODE_TYPE_DIR;
        let size = self.in_u32(IN_SIZE);
        let first_block = self.in_block(0);

        let node = vnode::alloc();
        if node.is_null() {
            trace!(0, "nanofs: no memory for the vnode of inode {}", idx);
            return null;
        }
        unsafe {
            let name = &self.inode.as_slice()[IN_NAME..IN_NAME + IN_NAME_LEN];
            let len = name.iter().position(|b| *b == 0).unwrap_or(IN_NAME_LEN).min(NAME_MAX - 1);
            (&mut (*node).name)[..len].copy_from_slice(&name[..len]);
            (*node).node_type = if is_dir { TYPE_DIR } else { TYPE_FILE };
            (*node).size = if is_dir { 0 } else { size as usize };
            (*node).ino = idx as usize;
            if is_dir {
                /* The whole tree is read here, so a directory is complete */
                (*node).flags = FLAG_DIR_LOADED;
            }
        }

        self.vnodes[idx as usize] = node;
        self.walking[idx as usize] = true;

        if is_dir && size > 0 {
            for child in self.dir_entries(idx, first_block, size) {
                let loaded = self.walk(child, depth + 1);
                /* A corrupted image may name this directory itself, an
                 * ancestor still being walked -- a cycle, the root
                 * included -- or an inode already linked somewhere else.
                 * Any of the three would make a tree that is not one. */
                if loaded.is_null()
                    || loaded == node
                    || self.walking[child as usize]
                    || !unsafe { vnode::is_unlinked(loaded) }
                {
                    continue;
                }
                unsafe {
                    (*loaded).parent = node;
                    vnode::insert_child(node, loaded);
                }
            }
        }

        self.walking[idx as usize] = false;
        node
    }

    /// The inode indices a directory's entries name. They are copied out of
    /// the block, because reading a child's inode is what happens next and
    /// there is one block buffer.
    fn dir_entries(&mut self, idx: u32, block: u32, count: u32) -> Vec<u32> {
        let mut entries = Vec::new();
        if block >= DATA_BLOCK_COUNT {
            trace!(0, "nanofs: inode {} names data block {}, which is not one", idx, block);
            return entries;
        }
        if !self.io.read_block(DATA_START + block, self.data.as_mut_slice()) {
            return entries;
        }

        let count = (count as usize).min(MAX_DIR_ENTRIES);
        if entries.try_reserve_exact(count).is_err() {
            trace!(0, "nanofs: no memory for {} directory entries", count);
            return entries;
        }
        for i in 0..count {
            entries.push(rd_u32(self.data.as_slice(), i * DIR_ENTRY_SIZE));
        }
        entries
    }
}

/* ---- directories ---- */

impl NanoFs {
    fn dir_u32(&self, off: usize) -> u32 {
        rd_u32(self.dir_inode.as_slice(), off)
    }

    fn dir_set_u32(&mut self, off: usize, v: u32) {
        wr_u32(self.dir_inode.as_mut_slice(), off, v);
    }

    /// Read a directory's inode into its own buffer, checked.
    fn read_dir_inode(&mut self, idx: u32) -> bool {
        if idx >= INODE_COUNT {
            trace!(0, "nanofs: inode {} is out of range", idx);
            return false;
        }
        if !self.io.read_block(INODE_START + idx, self.dir_inode.as_mut_slice()) {
            return false;
        }
        if self.dir_u32(IN_TYPE) != INODE_TYPE_DIR {
            trace!(0, "nanofs: inode {} is not a directory", idx);
            return false;
        }

        /* The entry count drives the loops below and an entries[count - 1]
         * write; an unchecked on-disk value would walk past the block. */
        let block = &self.dir_inode.as_slice()[..BLOCK_SIZE];
        if crc32_with_hole(block, IN_CHECKSUM) != rd_u32(block, IN_CHECKSUM) {
            trace!(0, "nanofs: directory inode {}'s checksum does not match", idx);
            return false;
        }
        if self.dir_u32(IN_SIZE) as usize > MAX_DIR_ENTRIES {
            trace!(0, "nanofs: directory {} claims {} entries, more than the {} there is room for",
                idx, self.dir_u32(IN_SIZE), MAX_DIR_ENTRIES);
            return false;
        }
        if self.dir_u32(IN_BLOCKS) >= DATA_BLOCK_COUNT {
            trace!(0, "nanofs: directory {} names data block {}, which is not one",
                idx, self.dir_u32(IN_BLOCKS));
            return false;
        }
        true
    }

    fn write_dir_inode(&mut self, idx: u32) -> bool {
        let sum = crc32_with_hole(&self.dir_inode.as_slice()[..BLOCK_SIZE], IN_CHECKSUM);
        wr_u32(self.dir_inode.as_mut_slice(), IN_CHECKSUM, sum);
        self.io.write_block(INODE_START + idx, self.dir_inode.as_slice(), false)
    }

    fn add_dir_entry(&mut self, dir_idx: u32, child_idx: u32) -> bool {
        if !self.read_dir_inode(dir_idx) {
            return false;
        }

        let count = self.dir_u32(IN_SIZE) as usize;
        if count >= MAX_DIR_ENTRIES {
            trace!(0, "nanofs: directory {} is full at {} entries", dir_idx, count);
            return false;
        }

        let block = self.dir_u32(IN_BLOCKS);
        if !self.io.read_block(DATA_START + block, self.data.as_mut_slice()) {
            return false;
        }

        let at = count * DIR_ENTRY_SIZE;
        wr_u32(self.data.as_mut_slice(), at, child_idx);
        wr_u32(self.data.as_mut_slice(), at + 4, 0);

        if !self.io.write_block(DATA_START + block, self.data.as_slice(), false) {
            return false;
        }

        self.dir_set_u32(IN_SIZE, count as u32 + 1);
        self.write_dir_inode(dir_idx)
    }

    fn remove_dir_entry(&mut self, dir_idx: u32, child_idx: u32) -> bool {
        if !self.read_dir_inode(dir_idx) {
            return false;
        }

        let count = self.dir_u32(IN_SIZE) as usize;
        let block = self.dir_u32(IN_BLOCKS);
        if !self.io.read_block(DATA_START + block, self.data.as_mut_slice()) {
            return false;
        }

        let mut at = None;
        for i in 0..count {
            if rd_u32(self.data.as_slice(), i * DIR_ENTRY_SIZE) == child_idx {
                at = Some(i);
                break;
            }
        }

        let at = match at {
            Some(at) => at,
            None => {
                trace!(0, "nanofs: inode {} is not an entry of directory {}", child_idx, dir_idx);
                return false;
            }
        };

        /* Close the gap: the entries after it move down one, and the last
         * one is cleared. */
        let entries = self.data.as_mut_slice();
        entries.copy_within(
            (at + 1) * DIR_ENTRY_SIZE..count * DIR_ENTRY_SIZE, at * DIR_ENTRY_SIZE);
        entries[(count - 1) * DIR_ENTRY_SIZE..count * DIR_ENTRY_SIZE].fill(0);

        if !self.io.write_block(DATA_START + block, self.data.as_slice(), false) {
            return false;
        }

        self.dir_set_u32(IN_SIZE, count as u32 - 1);
        self.write_dir_inode(dir_idx)
    }

    /* ---- what the VFS calls ---- */

    pub fn root(&self) -> *mut VNode {
        self.vnodes[0]
    }

    pub fn lookup(&self, dir: *mut VNode, name: &[u8]) -> *mut VNode {
        if dir.is_null() || !unsafe { (*dir).is_dir() } {
            return core::ptr::null_mut();
        }

        for child in unsafe { vnode::children(dir) } {
            if unsafe { (*child).name_is(name) } {
                return child;
            }
        }
        core::ptr::null_mut()
    }

    /// The checks every create shares. The inode index of the parent, or
    /// None when nothing may be made there.
    fn create_into(&mut self, dir: *mut VNode, name: &[u8]) -> Option<u32> {
        if dir.is_null() || name.is_empty() {
            trace!(0, "nanofs: something made with no name");
            return None;
        }
        if !unsafe { (*dir).is_dir() } {
            trace!(0, "nanofs: something made somewhere that is not a directory");
            return None;
        }
        if name.len() >= IN_NAME_LEN {
            trace!(0, "nanofs: a name of {} bytes is too long", name.len());
            return None;
        }
        if !self.lookup(dir, name).is_null() {
            trace!(0, "nanofs: there is something by that name already");
            return None;
        }
        Some(unsafe { (*dir).ino } as u32)
    }

    /// Fill the inode buffer for something newly made.
    fn init_inode(&mut self, node_type: u32, name: &[u8], parent: u32) {
        self.inode.as_mut_slice()[..BLOCK_SIZE].fill(0);
        self.in_set_u32(IN_TYPE, node_type);
        self.in_set_u32(IN_SIZE, 0);
        let len = name.len().min(IN_NAME_LEN - 1);
        self.inode.as_mut_slice()[IN_NAME..IN_NAME + len].copy_from_slice(&name[..len]);
        self.in_set_u32(IN_PARENT, parent);
        self.in_set_u32(IN_DATA_CHECKSUM, 0);
    }

    /// The vnode for something just made on disk, linked under its parent.
    fn link_new(&mut self, dir: *mut VNode, name: &[u8], idx: u32, is_dir: bool) -> *mut VNode {
        let node = vnode::alloc();
        if node.is_null() {
            trace!(0, "nanofs: no memory for a vnode");
            return node;
        }

        unsafe {
            let len = name.len().min(NAME_MAX - 1);
            (&mut (*node).name)[..len].copy_from_slice(&name[..len]);
            (*node).node_type = if is_dir { TYPE_DIR } else { TYPE_FILE };
            (*node).parent = dir;
            (*node).ino = idx as usize;
            if is_dir {
                (*node).flags = FLAG_DIR_LOADED;
            }
            vnode::insert_child(dir, node);
        }

        self.vnodes[idx as usize] = node;
        node
    }

    pub fn create_file(&mut self, dir: *mut VNode, name: &[u8]) -> *mut VNode {
        let null = core::ptr::null_mut();
        let dir_idx = match self.create_into(dir, name) {
            Some(idx) => idx,
            None => return null,
        };

        let idx = match self.take_inode() {
            Some(idx) => idx,
            None => return null,
        };

        self.init_inode(INODE_TYPE_FILE, name, dir_idx);
        if !self.write_inode(idx, false) {
            self.give_back_inode(idx);
            return null;
        }

        /* Commit the inode bitmap now the inode's contents are on disk */
        if !self.flush_super() || !self.add_dir_entry(dir_idx, idx) {
            trace!(0, "nanofs: a file could not be added to directory {}", dir_idx);
            self.give_back_inode(idx);
            return null;
        }

        let node = self.link_new(dir, name, idx, false);
        if node.is_null() {
            self.remove_dir_entry(dir_idx, idx);
            self.give_back_inode(idx);
        }
        node
    }

    pub fn create_dir(&mut self, dir: *mut VNode, name: &[u8]) -> *mut VNode {
        let null = core::ptr::null_mut();
        let dir_idx = match self.create_into(dir, name) {
            Some(idx) => idx,
            None => return null,
        };

        let idx = match self.take_inode() {
            Some(idx) => idx,
            None => return null,
        };
        let block = match self.take_data_block() {
            Some(block) => block,
            None => {
                self.give_back_inode(idx);
                return null;
            }
        };

        /* A new directory's entries start as nothing */
        self.data.as_mut_slice()[..BLOCK_SIZE].fill(0);
        if !self.io.write_block(DATA_START + block, self.data.as_slice(), false) {
            self.give_back_data_block(block);
            self.give_back_inode(idx);
            return null;
        }

        self.init_inode(INODE_TYPE_DIR, name, dir_idx);
        self.in_set_block(0, block);
        if !self.write_inode(idx, false) {
            self.give_back_data_block(block);
            self.give_back_inode(idx);
            return null;
        }

        /* Commit both bitmaps now the inode and its block are on disk */
        if !self.flush_super() || !self.add_dir_entry(dir_idx, idx) {
            trace!(0, "nanofs: a directory could not be added to directory {}", dir_idx);
            self.give_back_data_block(block);
            self.give_back_inode(idx);
            return null;
        }

        let node = self.link_new(dir, name, idx, true);
        if node.is_null() {
            self.remove_dir_entry(dir_idx, idx);
            self.give_back_data_block(block);
            self.give_back_inode(idx);
        }
        node
    }

    pub fn write(&mut self, file: *mut VNode, data: &[u8], offset: usize) -> bool {
        if file.is_null() || !unsafe { (*file).is_file() } {
            trace!(0, "nanofs: a write to something that is not a file");
            return false;
        }
        if data.is_empty() {
            return true;
        }

        let end = match offset.checked_add(data.len()) {
            Some(end) => end,
            None => {
                trace!(0, "nanofs: a write of {} bytes at {} is past what a file holds",
                    data.len(), offset);
                return false;
            }
        };

        let size = unsafe { (*file).size }.max(end);
        self.rewrite(file, size, data, offset)
    }

    pub fn truncate(&mut self, file: *mut VNode, size: usize) -> bool {
        if file.is_null() || !unsafe { (*file).is_file() } {
            trace!(0, "nanofs: a truncate of something that is not a file");
            return false;
        }
        if size == unsafe { (*file).size } {
            return true;
        }

        self.rewrite(file, size, &[], 0)
    }

    /// Replace the file's content with the old content resized to `new_size`
    /// -- cut short, or padded with zeros -- and then `data` at `offset`,
    /// which the callers keep within it. Every block of the new content goes
    /// to a freshly taken block, the inode is committed with FUA, and only
    /// then are the old blocks given back: a crash at any point leaves
    /// either the old file or the new one, never a mix.
    fn rewrite(&mut self, file: *mut VNode, new_size: usize, data: &[u8], offset: usize) -> bool {
        if new_size > MAX_FILE_SIZE {
            trace!(0, "nanofs: a size of {} is more than the {} a file holds",
                new_size, MAX_FILE_SIZE);
            return false;
        }

        let idx = unsafe { (*file).ino } as u32;
        if !self.read_inode(idx) {
            return false;
        }

        /* A damaged inode's size and blocks cannot be trusted for freeing:
         * an in-range garbage index would give back a block another file
         * owns. */
        if !self.inode_ok() {
            trace!(0, "nanofs: inode {}'s checksum does not match", idx);
            return false;
        }

        let old_size = (self.in_u32(IN_SIZE) as usize).min(MAX_FILE_SIZE);
        let old_count = (old_size + BLOCK_SIZE - 1) / BLOCK_SIZE;

        /* The old blocks go back only after the new inode is committed: the
         * bitmap is flushed with FUA when a block is given back, so freeing
         * first leaves a window where the on-disk inode names free blocks. */
        let mut old_blocks = [0u32; MAX_BLOCKS];
        for i in 0..MAX_BLOCKS {
            old_blocks[i] = self.in_block(i);
        }

        if new_size == 0 {
            for i in 0..MAX_BLOCKS {
                self.in_set_block(i, 0);
            }
            self.in_set_u32(IN_SIZE, 0);
            self.in_set_u32(IN_DATA_CHECKSUM, 0);
            if !self.write_inode(idx, true) {
                trace!(0, "nanofs: the truncate of inode {} could not be committed", idx);
                return false;
            }
            for i in 0..old_count {
                self.give_back_data_block(old_blocks[i]);
            }
            unsafe { (*file).size = 0 };
            return true;
        }

        let new_count = (new_size + BLOCK_SIZE - 1) / BLOCK_SIZE;
        let mut new_blocks = [0u32; MAX_BLOCKS];
        for i in 0..new_count {
            match self.take_data_block() {
                Some(block) => new_blocks[i] = block,
                None => {
                    trace!(0, "nanofs: only {} of the {} blocks inode {} needs were free",
                        i, new_count, idx);
                    for j in 0..i {
                        self.give_back_data_block(new_blocks[j]);
                    }
                    return false;
                }
            }
        }

        /* Bytes of the old content that survive */
        let keep = old_size.min(new_size);
        for i in 0..new_count {
            let start = i * BLOCK_SIZE;

            if start < keep && i < old_count {
                if old_blocks[i] >= DATA_BLOCK_COUNT
                    || !self.io.read_block(DATA_START + old_blocks[i], self.data.as_mut_slice())
                {
                    trace!(0, "nanofs: block {} of inode {} could not be read", i, idx);
                    for j in 0..new_count {
                        self.give_back_data_block(new_blocks[j]);
                    }
                    return false;
                }
                let valid = keep - start;
                if valid < BLOCK_SIZE {
                    self.data.as_mut_slice()[valid..BLOCK_SIZE].fill(0);
                }
            } else {
                self.data.as_mut_slice()[..BLOCK_SIZE].fill(0);
            }

            /* The part of [offset, offset + len) that falls in this block */
            if !data.is_empty() && offset < start + BLOCK_SIZE && offset + data.len() > start {
                let from = offset.max(start);
                let to = (offset + data.len()).min(start + BLOCK_SIZE);
                self.data.as_mut_slice()[from - start..to - start]
                    .copy_from_slice(&data[from - offset..to - offset]);
            }

            if !self.io.write_block(DATA_START + new_blocks[i], self.data.as_slice(), false) {
                for j in 0..new_count {
                    self.give_back_data_block(new_blocks[j]);
                }
                return false;
            }
        }

        /* The data must be durable before the FUA inode commit names it, and
         * the commit durable before the old blocks are given back -- the
         * bitmap is flushed with FUA there -- or a crash loses data the
         * bitmap already accounts for. */
        if !self.io.flush() || !self.flush_super() {
            trace!(0, "nanofs: the new blocks of inode {} could not be committed", idx);
            for j in 0..new_count {
                self.give_back_data_block(new_blocks[j]);
            }
            return false;
        }

        for i in 0..MAX_BLOCKS {
            self.in_set_block(i, if i < new_count { new_blocks[i] } else { 0 });
        }
        self.in_set_u32(IN_SIZE, new_size as u32);
        let sum = self.data_checksum(&new_blocks[..new_count], new_size);
        self.in_set_u32(IN_DATA_CHECKSUM, sum);

        if !self.write_inode(idx, true) {
            trace!(0, "nanofs: inode {} could not be committed", idx);
            /* The inode on disk still names the old blocks; the new ones go
             * back instead. */
            for j in 0..new_count {
                self.give_back_data_block(new_blocks[j]);
            }
            return false;
        }

        for i in 0..old_count {
            self.give_back_data_block(old_blocks[i]);
        }

        unsafe { (*file).size = new_size };
        true
    }

    /// The CRC of a file's data: the per-block CRCs, exclusive-ored together.
    /// 0 when the file is empty or a block cannot be read, which is what the
    /// C++ driver wrote and what images on disk carry.
    fn data_checksum(&mut self, blocks: &[u32], size: usize) -> u32 {
        if size == 0 {
            return 0;
        }

        let mut sum = 0;
        let mut left = size;
        for i in 0..MAX_BLOCKS {
            if left == 0 {
                break;
            }
            if i >= blocks.len()
                || blocks[i] >= DATA_BLOCK_COUNT
                || !self.io.read_block(DATA_START + blocks[i], self.data.as_mut_slice())
            {
                return 0;
            }

            let chunk = left.min(BLOCK_SIZE);
            sum ^= crc32_update(0, &self.data.as_slice()[..chunk]);
            left -= chunk;
        }
        sum
    }

    pub fn read(&mut self, file: *mut VNode, buf: &mut [u8], offset: usize) -> bool {
        if file.is_null() || !unsafe { (*file).is_file() } {
            trace!(0, "nanofs: a read of something that is not a file");
            return false;
        }

        let idx = unsafe { (*file).ino } as u32;
        if !self.read_inode(idx) {
            return false;
        }
        if !self.inode_ok() {
            trace!(0, "nanofs: inode {}'s checksum does not match", idx);
            return false;
        }

        /* A forged size past the maximum would make the loop below stop
         * early and answer with bytes of the caller's buffer it never wrote */
        let size = self.in_u32(IN_SIZE) as usize;
        if size > MAX_FILE_SIZE {
            trace!(0, "nanofs: inode {} claims {} bytes, more than the {} a file holds",
                idx, size, MAX_FILE_SIZE);
            return false;
        }
        if offset >= size {
            trace!(0, "nanofs: a read at {} is past the {} bytes inode {} has", offset, size, idx);
            return false;
        }

        let mut blocks = [0u32; MAX_BLOCKS];
        for i in 0..MAX_BLOCKS {
            blocks[i] = self.in_block(i);
        }
        let stored = self.in_u32(IN_DATA_CHECKSUM);

        let take = buf.len().min(size - offset);
        let mut done = 0;
        let mut block = offset / BLOCK_SIZE;
        let mut byte = offset % BLOCK_SIZE;

        while done < take && block < MAX_BLOCKS {
            if blocks[block] >= DATA_BLOCK_COUNT {
                trace!(0, "nanofs: inode {} names data block {}, which is not one",
                    idx, blocks[block]);
                return false;
            }
            if !self.io.read_block(DATA_START + blocks[block], self.data.as_mut_slice()) {
                return false;
            }

            let chunk = (BLOCK_SIZE - byte).min(take - done);
            buf[done..done + chunk].copy_from_slice(&self.data.as_slice()[byte..byte + chunk]);
            done += chunk;
            byte = 0;
            block += 1;
        }

        /* A short read would leave the caller consuming bytes nothing wrote */
        if done < take {
            trace!(0, "nanofs: {} of the {} bytes asked of inode {} were there", done, take, idx);
            return false;
        }

        if stored != 0 && self.data_checksum(&blocks[..], size) != stored {
            trace!(0, "nanofs: the data of inode {} does not match its checksum", idx);
            return false;
        }

        true
    }

    /// Take a node out of its directory, give back its blocks and inode, and
    /// free the vnode; a directory goes with everything under it.
    fn remove_tree(&mut self, node: *mut VNode) -> bool {
        let idx = unsafe { (*node).ino } as u32;

        if unsafe { (*node).is_dir() } {
            loop {
                let child = unsafe { vnode::first_child(node) };
                if child.is_null() {
                    break;
                }
                if !self.remove_tree(child) {
                    return false;
                }
            }
        }

        /* Blocks go back only if the inode can be trusted to name its own:
         * a damaged one could name blocks another file owns. */
        if self.read_inode(idx) && self.inode_ok() {
            let node_type = self.in_u32(IN_TYPE);
            let size = self.in_u32(IN_SIZE) as usize;
            if node_type == INODE_TYPE_DIR {
                let block = self.in_block(0);
                self.give_back_data_block(block);
            } else if node_type == INODE_TYPE_FILE && size > 0 {
                let count = ((size + BLOCK_SIZE - 1) / BLOCK_SIZE).min(MAX_BLOCKS);
                for i in 0..count {
                    let block = self.in_block(i);
                    self.give_back_data_block(block);
                }
            }
        }

        self.inode.as_mut_slice()[..BLOCK_SIZE].fill(0);
        self.write_inode(idx, false);
        self.give_back_inode(idx);

        self.vnodes[idx as usize] = core::ptr::null_mut();
        unsafe {
            vnode::unlink(node);
            vnode::free(node);
        }
        true
    }

    pub fn remove(&mut self, node: *mut VNode) -> bool {
        if node.is_null() {
            trace!(0, "nanofs: a remove of nothing");
            return false;
        }
        let parent = unsafe { (*node).parent };
        if parent.is_null() {
            trace!(0, "nanofs: the root cannot be removed");
            return false;
        }

        let idx = unsafe { (*node).ino } as u32;
        let parent_idx = unsafe { (*parent).ino } as u32;

        if !self.remove_dir_entry(parent_idx, idx) {
            trace!(0, "nanofs: inode {} could not be taken out of directory {}", idx, parent_idx);
            return false;
        }

        if !self.remove_tree(node) {
            return false;
        }
        self.io.flush()
    }

    pub fn rename(&mut self, node: *mut VNode, new_dir: *mut VNode, new_name: &[u8]) -> bool {
        if node.is_null() || new_dir.is_null() || new_name.is_empty() {
            trace!(0, "nanofs: a rename of nothing");
            return false;
        }
        let old_dir = unsafe { (*node).parent };
        if old_dir.is_null() {
            trace!(0, "nanofs: the root cannot be renamed");
            return false;
        }
        if !unsafe { (*new_dir).is_dir() } {
            trace!(0, "nanofs: the target of a rename is not a directory");
            return false;
        }
        if new_name.len() >= NAME_MAX || new_name.len() >= IN_NAME_LEN {
            trace!(0, "nanofs: a name of {} bytes is too long", new_name.len());
            return false;
        }
        if !self.lookup(new_dir, new_name).is_null() {
            trace!(0, "nanofs: there is something by that name already");
            return false;
        }

        let idx = unsafe { (*node).ino } as u32;
        let old_idx = unsafe { (*old_dir).ino } as u32;
        let new_idx = unsafe { (*new_dir).ino } as u32;

        if !self.read_inode(idx) || !self.inode_ok() {
            trace!(0, "nanofs: inode {} could not be read", idx);
            return false;
        }

        /* The name lives in the inode and a directory holds only inode
         * indices: a rename within one directory is an inode rewrite, a move
         * adds the index to the new directory first, so that a crash leaves
         * the file reachable -- twice, which the mount walk tolerates --
         * never lost. */
        let moved = new_idx != old_idx;
        if moved && !self.add_dir_entry(new_idx, idx) {
            trace!(0, "nanofs: inode {} could not be added to directory {}", idx, new_idx);
            return false;
        }

        /* Read again: adding the entry above used the inode buffer */
        if !self.read_inode(idx) || !self.inode_ok() {
            if moved {
                self.remove_dir_entry(new_idx, idx);
            }
            return false;
        }

        self.inode.as_mut_slice()[IN_NAME..IN_NAME + IN_NAME_LEN].fill(0);
        self.inode.as_mut_slice()[IN_NAME..IN_NAME + new_name.len()].copy_from_slice(new_name);
        self.in_set_u32(IN_PARENT, new_idx);

        if !self.write_inode(idx, true) {
            trace!(0, "nanofs: inode {} could not be committed", idx);
            if moved {
                self.remove_dir_entry(new_idx, idx);
            }
            return false;
        }

        if moved && !self.remove_dir_entry(old_idx, idx) {
            trace!(0, "nanofs: inode {} is in directory {} as well", idx, old_idx);
        }

        unsafe { vnode::rename(node, new_dir, new_name) };
        self.io.flush()
    }

    /// The UUID as hex, for `mounts`.
    fn info(&self, buf: &mut [u8]) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        const NEED: usize = 5 + 32 + 1;

        if buf.is_empty() {
            return;
        }
        buf[0] = 0;
        if buf.len() < NEED {
            return;
        }

        buf[..5].copy_from_slice(b"uuid=");
        for i in 0..16 {
            let byte = self.sb.as_slice()[SB_UUID + i];
            buf[5 + i * 2] = HEX[(byte >> 4) as usize];
            buf[5 + i * 2 + 1] = HEX[(byte & 0xF) as usize];
        }
        buf[NEED - 1] = 0;
    }
}

/// Write a fresh nanofs onto the device: a superblock, a root directory and
/// nothing in it.
pub fn format(dev: &Disk) -> bool {
    let sector_size = dev.sector_size();
    if sector_size == 0 || BLOCK_SIZE as u64 % sector_size != 0 {
        trace!(0, "nanofs: a sector size of {} does not divide a block", sector_size);
        return false;
    }

    let io = Io { dev: *dev, sectors_per_block: (BLOCK_SIZE as u64 / sector_size) as u32 };
    let mut block = match DmaBuffer::new(1) {
        Some(buf) => buf,
        None => return false,
    };

    /* The superblock: the layout, a UUID out of the kernel's entropy pool,
     * and the root's inode and first data block taken. */
    block.as_mut_slice()[..BLOCK_SIZE].fill(0);
    {
        let sb = block.as_mut_slice();
        wr_u32(sb, SB_MAGIC, MAGIC);
        wr_u32(sb, SB_VERSION, VERSION);
        wr_u32(sb, SB_BLOCK_SIZE, BLOCK_SIZE as u32);
        wr_u32(sb, SB_INODE_COUNT, INODE_COUNT);
        wr_u32(sb, SB_DATA_BLOCK_COUNT, DATA_BLOCK_COUNT);
        wr_u32(sb, SB_INODE_START, INODE_START);
        wr_u32(sb, SB_DATA_START, DATA_START);
        if !kcore::random::fill_random(&mut sb[SB_UUID..SB_UUID + 16]) {
            trace!(0, "nanofs: the filesystem will carry no uuid");
        }
        bit_set(&mut sb[SB_INODE_BITMAP..SB_INODE_BITMAP + SB_INODE_BITMAP_LEN], 0);
        bit_set(&mut sb[SB_DATA_BITMAP..SB_DATA_BITMAP + SB_DATA_BITMAP_LEN], 0);
    }
    let sum = crc32_with_hole(&block.as_slice()[..BLOCK_SIZE], SB_CHECKSUM);
    wr_u32(block.as_mut_slice(), SB_CHECKSUM, sum);
    if !io.write_block(0, block.as_slice(), false) {
        return false;
    }

    /* The root directory: inode 0, no entries, its block at data block 0 */
    block.as_mut_slice()[..BLOCK_SIZE].fill(0);
    {
        let inode = block.as_mut_slice();
        wr_u32(inode, IN_TYPE, INODE_TYPE_DIR);
        wr_u32(inode, IN_SIZE, 0);
        inode[IN_NAME] = b'/';
        wr_u32(inode, IN_PARENT, 0);
        wr_u32(inode, IN_BLOCKS, 0);
        wr_u32(inode, IN_DATA_CHECKSUM, 0);
    }
    let sum = crc32_with_hole(&block.as_slice()[..BLOCK_SIZE], IN_CHECKSUM);
    wr_u32(block.as_mut_slice(), IN_CHECKSUM, sum);
    if !io.write_block(INODE_START, block.as_slice(), false) {
        return false;
    }

    block.as_mut_slice()[..BLOCK_SIZE].fill(0);
    if !io.write_block(DATA_START, block.as_slice(), false) || !io.flush() {
        return false;
    }

    trace!(0, "nanofs: formatted");
    true
}

/* ---- the ops table the VFS drives it by ---- */

/// # Safety
/// `ctx` is the pointer a mount was made with, and the filesystem is alive.
unsafe fn fs<'a>(ctx: *mut u8) -> &'a mut NanoFs {
    unsafe { &mut *(ctx as *mut NanoFs) }
}

/// # Safety
/// `name` points at a NUL-terminated string.
unsafe fn cstr<'a>(name: *const u8) -> &'a [u8] {
    if name.is_null() {
        return &[];
    }
    let mut len = 0;
    while len < NAME_MAX && unsafe { *name.add(len) } != 0 {
        len += 1;
    }
    unsafe { core::slice::from_raw_parts(name, len) }
}

extern "C" fn op_info(ctx: *mut u8, buf: *mut u8, len: usize) {
    if buf.is_null() || len == 0 {
        return;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    unsafe { fs(ctx) }.info(buf);
}

extern "C" fn op_root(ctx: *mut u8) -> *mut VNode {
    unsafe { fs(ctx) }.root()
}

extern "C" fn op_load_dir(_ctx: *mut u8, _dir: *mut VNode) -> i32 {
    /* The whole tree is read at mount: there is nothing left to load */
    0
}

extern "C" fn op_lookup(ctx: *mut u8, dir: *mut VNode, name: *const u8) -> *mut VNode {
    unsafe { fs(ctx) }.lookup(dir, unsafe { cstr(name) })
}

extern "C" fn op_create_file(ctx: *mut u8, dir: *mut VNode, name: *const u8) -> *mut VNode {
    unsafe { fs(ctx) }.create_file(dir, unsafe { cstr(name) })
}

extern "C" fn op_create_dir(ctx: *mut u8, dir: *mut VNode, name: *const u8) -> *mut VNode {
    unsafe { fs(ctx) }.create_dir(dir, unsafe { cstr(name) })
}

extern "C" fn op_read(
    ctx: *mut u8, file: *mut VNode, buf: *mut u8, len: usize, off: usize,
) -> i32 {
    if len == 0 {
        return 0;
    }
    if buf.is_null() {
        return -1;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    if unsafe { fs(ctx) }.read(file, buf, off) { 0 } else { -1 }
}

extern "C" fn op_write(
    ctx: *mut u8, file: *mut VNode, data: *const u8, len: usize, off: usize,
) -> i32 {
    if len == 0 {
        return 0;
    }
    if data.is_null() {
        return -1;
    }
    let data = unsafe { core::slice::from_raw_parts(data, len) };
    if unsafe { fs(ctx) }.write(file, data, off) { 0 } else { -1 }
}

extern "C" fn op_truncate(ctx: *mut u8, file: *mut VNode, size: usize) -> i32 {
    if unsafe { fs(ctx) }.truncate(file, size) { 0 } else { -1 }
}

extern "C" fn op_rename(ctx: *mut u8, node: *mut VNode, dir: *mut VNode, name: *const u8) -> i32 {
    if unsafe { fs(ctx) }.rename(node, dir, unsafe { cstr(name) }) { 0 } else { -1 }
}

extern "C" fn op_remove(ctx: *mut u8, node: *mut VNode) -> i32 {
    if unsafe { fs(ctx) }.remove(node) { 0 } else { -1 }
}

extern "C" fn op_sync(ctx: *mut u8) -> i32 {
    if unsafe { fs(ctx) }.sync() { 0 } else { -1 }
}

extern "C" fn op_device(ctx: *mut u8) -> usize {
    unsafe { fs(ctx) }.io.dev.handle()
}

extern "C" fn op_mount(ctx: *mut u8, read_only: i32) -> i32 {
    let fs = unsafe { fs(ctx) };
    if !fs.mount() {
        return -1;
    }
    if read_only != 0 { 1 } else { 0 }
}

extern "C" fn op_unmount(ctx: *mut u8) {
    unsafe { fs(ctx) }.unmount();
}

extern "C" fn op_destroy(ctx: *mut u8) {
    drop(unsafe { Box::from_raw(ctx as *mut NanoFs) });
}

fn ops_for(fs: *mut NanoFs) -> FsOps {
    FsOps {
        name: b"nanofs\0".as_ptr(),
        info: Some(op_info),
        root: op_root,
        load_dir: op_load_dir,
        lookup: op_lookup,
        create_file: op_create_file,
        create_dir: op_create_dir,
        read: op_read,
        write: op_write,
        truncate: op_truncate,
        rename: op_rename,
        remove: op_remove,
        sync: op_sync,
        device: op_device,
        mount: op_mount,
        unmount: op_unmount,
        destroy: Some(op_destroy),
        ctx: fs as *mut u8,
    }
}

/* ---- what the kernel calls ---- */

/// Mount the device's nanofs at `path`: 0 mounted for writing, 1 mounted
/// read-only, -1 not mounted.
///
/// # Safety
/// `path` points at `path_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_nanofs_mount(
    path: *const u8, path_len: usize, device: usize, read_only: i32,
) -> i32 {
    let dev = match Disk::from_handle(device) {
        Some(dev) => dev,
        None => return -1,
    };
    let (vfs, at) = match (crate::vfs_instance(), unsafe { crate::path(path, path_len) }) {
        (Some(vfs), Some(at)) => (vfs, at),
        _ => return -1,
    };

    let fs = match NanoFs::new(dev) {
        Some(fs) => Box::into_raw(fs),
        None => {
            trace!(0, "nanofs: no memory for the filesystem");
            return -1;
        }
    };

    let ops = ops_for(fs);
    if !vfs.mount(at, &ops, read_only != 0) {
        drop(unsafe { Box::from_raw(fs) });
        return -1;
    }
    if read_only != 0 { 1 } else { 0 }
}

/// Write a fresh nanofs onto the device: 0 done, -1 not.
#[no_mangle]
pub extern "C" fn rust_nanofs_format(device: usize) -> i32 {
    match Disk::from_handle(device) {
        Some(dev) if format(&dev) => 0,
        _ => -1,
    }
}
