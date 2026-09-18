//! ext2, read and write: the filesystem the kernel boots its root from.
//!
//! Rev 1 images with the filetype dirent feature, block sizes up to a page,
//! direct, single- and double-indirect blocks. What it will not parse it
//! refuses at mount rather than misreading -- ext3 and ext4 share ext2's
//! magic, so the magic alone says little -- and what it could read but not
//! safely write it mounts read-only.
//!
//! Every call arrives with the VFS lock held (see `FsOps`), so there is no
//! locking here: two calls never overlap on the same filesystem.
//!
//! The commit order is what e2fsck judges, and it is the same everywhere: the
//! allocation bitmap, then the data and the indirect blocks, then the device
//! flush, then the inode that leads to them, and last the counts in the group
//! descriptors and the superblock. A machine that stops midway leaves at
//! worst something unreferenced for e2fsck to reclaim, never a block that is
//! both in use and free.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_int;

use kcore::block::Disk;
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::time::wall_clock_secs;
use kcore::trace;

use crate::vfs::FsOps;
use crate::vnode::{self, VNode, FLAG_DIR_LOADED, NAME_MAX, TYPE_DIR, TYPE_FILE};

pub const MAGIC: u16 = 0xEF53;

const MODE_TYPE_MASK: u16 = 0xF000;
const MODE_DIR: u16 = 0x4000;
const MODE_FILE: u16 = 0x8000;
/// What a file and a directory made here get: 0644 and 0755
const MODE_FILE_DEFAULT: u16 = MODE_FILE | 0o644;
const MODE_DIR_DEFAULT: u16 = MODE_DIR | 0o755;

const DIR_TYPE_UNKNOWN: u8 = 0;
const DIR_TYPE_FILE: u8 = 1;
const DIR_TYPE_DIR: u8 = 2;

const ROOT_INODE: u32 = 2;

/* FeatureIncompat bits. FileType is required: load_dir keys directory
 * detection off the dirent FileType byte, which without this feature is the
 * high half of a 16-bit name length. Any other incompat bit (ext3 journal
 * recovery, ext4 extents/64bit, meta_bg, ...) changes the on-disk format in
 * ways this driver cannot parse and must refuse. */
const INCOMPAT_FILETYPE: u32 = 0x0002;
const INCOMPAT_SUPPORTED: u32 = INCOMPAT_FILETYPE;

/* FeatureRoCompat bits this driver maintains when writing. SparseSuper only
 * changes where the backup superblocks are (this driver updates the primary
 * alone, as Linux does); LargeFile allows a 64-bit size, which is refused
 * rather than produced. Anything else -- gdt_csum with its group checksums
 * and uninitialised-group flags above all -- would be silently broken by a
 * write, so an image carrying it is mounted read-only. */
const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
const RO_COMPAT_LARGE_FILE: u32 = 0x0002;
const RO_COMPAT_WRITABLE: u32 = RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE;

/// Superblock State: off while the filesystem is mounted for writing
const STATE_VALID: u16 = 0x0001;

/// An htree-indexed directory (compat dir_index). This driver modifies
/// directories linearly and clears the flag when it does, which is what
/// Linux expects of a writer that does not maintain the index.
const INODE_FLAG_INDEX: u32 = 0x0000_1000;

/// BlockSize = 1024 << LogBlockSize must not exceed a page
const MAX_LOG_BLOCK_SIZE: u32 = 2;

const DIRECT_BLOCKS: u32 = 12;
const INDIRECT_SLOT: usize = 12;
const DINDIRECT_SLOT: usize = 13;

const SUPER_BLOCK_OFFSET: usize = 1024;
const SUPER_BLOCK_SIZE: usize = 1024;
const GROUP_DESC_SIZE: usize = 32;
const INODE_SIZE: usize = 128;

/// inode.blocks counts units of this many bytes, whatever the block size
const BLOCKS_UNIT: u32 = 512;

const DIR_ENTRY_HEADER: usize = 8;
const DIR_ENTRY_ALIGN: usize = 4;
const MAX_NAME_LEN: usize = 255;

/// Blocks a truncate releases between two inode commits (see `truncate_inode`)
const FREE_BATCH: usize = 512;

/// Recursion cap for a recursive remove
const MAX_DIR_DEPTH: u32 = 32;

/* ---- on-disk structures ---- */

#[repr(C)]
#[derive(Clone, Copy)]
struct SuperBlock {
    inode_count: u32,
    block_count: u32,
    reserved_block_count: u32,
    free_block_count: u32,
    free_inode_count: u32,
    first_data_block: u32,
    log_block_size: u32,
    log_frag_size: u32,
    blocks_per_group: u32,
    frags_per_group: u32,
    inodes_per_group: u32,
    mount_time: u32,
    write_time: u32,
    mount_count: u16,
    max_mount_count: u16,
    magic: u16,
    state: u16,
    errors: u16,
    minor_rev_level: u16,
    last_check: u32,
    check_interval: u32,
    creator_os: u32,
    rev_level: u32,
    def_res_uid: u16,
    def_res_gid: u16,
    /* Rev 1+ */
    first_inode: u32,
    inode_size: u16,
    block_group_nr: u16,
    feature_compat: u32,
    feature_incompat: u32,
    feature_ro_compat: u32,
    uuid: [u8; 16],
    volume_name: [u8; 16],
    padding: [u8; 888],
}

const _: () = assert!(core::mem::size_of::<SuperBlock>() == SUPER_BLOCK_SIZE);

#[repr(C)]
#[derive(Clone, Copy)]
struct Inode {
    mode: u16,
    uid: u16,
    size: u32,
    access_time: u32,
    create_time: u32,
    modify_time: u32,
    delete_time: u32,
    gid: u16,
    links_count: u16,
    blocks: u32,
    flags: u32,
    osd1: u32,
    block: [u32; 15],
    generation: u32,
    file_acl: u32,
    dir_acl: u32,
    frag_addr: u32,
    osd2: [u8; 12],
}

const _: () = assert!(core::mem::size_of::<Inode>() == INODE_SIZE);

/// What `probe` reads off an unmounted superblock: enough to pick a root
/// filesystem by label or UUID. The C++ side declares the same struct.
#[repr(C)]
pub struct Identity {
    pub uuid: [u8; 16],
    pub label: [u8; 17],
}

/* ---- little-endian scalars in a buffer ---- */

fn rd_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn wr_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn wr_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// A POD out of a buffer at an offset. The kernel is little-endian on both
/// architectures, which is the byte order ext2 is written in.
///
/// # Safety
/// `off + size_of::<T>()` is within `buf`, and `T` is a plain structure of
/// scalars with no padding that means anything.
unsafe fn read_pod<T: Copy>(buf: &[u8], off: usize) -> T {
    unsafe { core::ptr::read_unaligned(buf.as_ptr().add(off) as *const T) }
}

/// # Safety
/// As for `read_pod`.
unsafe fn write_pod<T: Copy>(buf: &mut [u8], off: usize, v: &T) {
    unsafe { core::ptr::write_unaligned(buf.as_mut_ptr().add(off) as *mut T, *v) }
}

/* ---- bitmaps ---- */

/* Little-endian bit order: bit i of the map is bit (i % 8) of byte (i / 8),
 * the same numbering the x86 bit instructions use. */

fn bit_test(map: &[u8], bit: usize) -> bool {
    map[bit / 8] & (1u8 << (bit % 8)) != 0
}

fn bit_set(map: &mut [u8], bit: usize) {
    map[bit / 8] |= 1u8 << (bit % 8);
}

fn bit_clear(map: &mut [u8], bit: usize) {
    map[bit / 8] &= !(1u8 << (bit % 8));
}

/* ---- directory entries ---- */

fn dir_entry_size(name_len: usize) -> usize {
    (DIR_ENTRY_HEADER + name_len + DIR_ENTRY_ALIGN - 1) & !(DIR_ENTRY_ALIGN - 1)
}

fn de_inode(b: &[u8], p: usize) -> u32 {
    rd_u32(b, p)
}

fn de_rec_len(b: &[u8], p: usize) -> usize {
    rd_u16(b, p + 4) as usize
}

fn de_name_len(b: &[u8], p: usize) -> usize {
    b[p + 6] as usize
}

fn de_file_type(b: &[u8], p: usize) -> u8 {
    b[p + 7]
}

fn de_name(b: &[u8], p: usize) -> &[u8] {
    &b[p + DIR_ENTRY_HEADER..p + DIR_ENTRY_HEADER + de_name_len(b, p)]
}

fn de_set_inode(b: &mut [u8], p: usize, v: u32) {
    wr_u32(b, p, v);
}

fn de_set_rec_len(b: &mut [u8], p: usize, v: usize) {
    wr_u16(b, p + 4, v as u16);
}

/// Fill a record's header and name, leaving its length alone.
fn de_fill(b: &mut [u8], p: usize, ino: u32, name: &[u8], file_type: u8) {
    de_set_inode(b, p, ino);
    b[p + 6] = name.len() as u8;
    b[p + 7] = file_type;
    b[p + DIR_ENTRY_HEADER..p + DIR_ENTRY_HEADER + name.len()].copy_from_slice(name);
}

/* ---- block I/O ---- */

/// What it takes to read and write a block: everything here is fixed once
/// the filesystem is mounted, which is what lets a caller hold a buffer of
/// its own across a call.
struct Io {
    dev: Disk,
    block_size: usize,
    sector_size: usize,
    first_data_block: u32,
    block_count: u32,
}

impl Io {
    fn read_block(&self, block: u32, buf: &mut [u8]) -> bool {
        if block >= self.block_count {
            trace!(0, "ext2: read of block {} beyond {}", block, self.block_count);
            return false;
        }
        let sectors = self.block_size / self.sector_size;
        let start = block as u64 * sectors as u64;
        self.dev.read(start, &mut buf[..self.block_size]).is_ok()
    }

    fn write_block(&self, block: u32, buf: &[u8], fua: bool) -> bool {
        if block >= self.block_count {
            trace!(0, "ext2: write of block {} beyond {}", block, self.block_count);
            return false;
        }
        let sectors = self.block_size / self.sector_size;
        let start = block as u64 * sectors as u64;
        self.dev.write(start, &buf[..self.block_size], fua).is_ok()
    }

    /// A block number read off the disk (an inode or indirect block pointer)
    /// must land inside the filesystem and past the boot block; anything else
    /// is corruption, and following it would read or overwrite metadata.
    fn is_data_block(&self, block: u32) -> bool {
        block >= self.first_data_block && block < self.block_count
    }

    fn flush(&self) -> bool {
        self.dev.flush().is_ok()
    }
}

/* ---- the filesystem ---- */

pub struct Ext2 {
    io: Io,
    read_only: bool,
    mounted: bool,

    sb: SuperBlock,
    /// The group descriptor table, as it is on disk: `group_count` records of
    /// 32 bytes, in a buffer rounded up to whole blocks.
    gdt: Vec<u8>,
    gdt_blocks: usize,
    gdt_block: u32,
    group_count: u32,
    inode_size: usize,
    ptrs_per_block: u32,
    root: *mut VNode,
    /// Free counts in the descriptors and the superblock changed in memory
    meta_dirty: bool,

    /* Page-aligned scratch, one block used of each. `tmp` serves the inode
     * table, the superblock and the group descriptors; `data` the data block
     * read-modify-write and directory blocks; `ind` and `dind` cache the
     * indirect and doubly-indirect block last used, so a sequential pass over
     * a file does not re-read them per data block; `bitmap` holds the bitmap
     * block being allocated from. */
    tmp: DmaBuffer,
    data: DmaBuffer,
    ind: DmaBuffer,
    ind_block: u32,
    dind: DmaBuffer,
    dind_block: u32,
    bitmap: DmaBuffer,
    bitmap_block: u32,
    bitmap_dirty: bool,
}

/// The superblock, off a device that is not mounted. None when the device
/// carries no ext2 this driver would recognise.
fn read_super_block(dev: &Disk, scratch: &mut [u8]) -> Option<SuperBlock> {
    let sector_size = dev.sector_size() as usize;
    if sector_size == 0 || sector_size > PAGE_SIZE || !sector_size.is_power_of_two() {
        trace!(0, "ext2: unsupported sector size {}", sector_size);
        return None;
    }

    let start = SUPER_BLOCK_OFFSET / sector_size;
    let off = SUPER_BLOCK_OFFSET % sector_size;
    let count = (off + SUPER_BLOCK_SIZE + sector_size - 1) / sector_size;

    scratch.fill(0);
    if dev.read(start as u64, &mut scratch[..count * sector_size]).is_err() {
        trace!(0, "ext2: failed to read the superblock");
        return None;
    }

    /* off + 1024 is within a page for every sector size up to one */
    Some(unsafe { read_pod::<SuperBlock>(scratch, off) })
}

/// Does the device carry an ext2 superblock this driver would mount?
pub fn probe(dev: &Disk, id: &mut Identity) -> bool {
    let mut scratch = match DmaBuffer::new(1) {
        Some(buf) => buf,
        None => return false,
    };

    let sb = match read_super_block(dev, scratch.as_mut_slice()) {
        Some(sb) => sb,
        None => return false,
    };
    if sb.magic != MAGIC || sb.rev_level < 1 {
        return false;
    }

    id.uuid = sb.uuid;
    id.label[..16].copy_from_slice(&sb.volume_name);
    id.label[16] = 0;
    true
}

impl Ext2 {
    /// A filesystem over a device, not yet mounted.
    pub fn new(dev: Disk) -> Option<Box<Ext2>> {
        let sector_size = dev.sector_size() as usize;
        if sector_size == 0 || sector_size > PAGE_SIZE {
            return None;
        }

        Some(Box::new(Ext2 {
            io: Io {
                dev,
                block_size: 0,
                sector_size,
                first_data_block: 0,
                block_count: 0,
            },
            read_only: false,
            mounted: false,
            sb: unsafe { core::mem::zeroed() },
            gdt: Vec::new(),
            gdt_blocks: 0,
            gdt_block: 0,
            group_count: 0,
            inode_size: INODE_SIZE,
            ptrs_per_block: 0,
            root: core::ptr::null_mut(),
            meta_dirty: false,
            tmp: DmaBuffer::new(1)?,
            data: DmaBuffer::new(1)?,
            ind: DmaBuffer::new(1)?,
            ind_block: 0,
            dind: DmaBuffer::new(1)?,
            dind_block: 0,
            bitmap: DmaBuffer::new(1)?,
            bitmap_block: 0,
            bitmap_dirty: false,
        }))
    }

    fn block_size(&self) -> usize {
        self.io.block_size
    }

    /* ---- group descriptors ---- */

    fn gd_off(&self, group: u32) -> usize {
        group as usize * GROUP_DESC_SIZE
    }

    fn gd_block_bitmap(&self, group: u32) -> u32 {
        rd_u32(&self.gdt, self.gd_off(group))
    }

    fn gd_inode_bitmap(&self, group: u32) -> u32 {
        rd_u32(&self.gdt, self.gd_off(group) + 4)
    }

    fn gd_inode_table(&self, group: u32) -> u32 {
        rd_u32(&self.gdt, self.gd_off(group) + 8)
    }

    fn gd_free_blocks(&self, group: u32) -> u16 {
        rd_u16(&self.gdt, self.gd_off(group) + 12)
    }

    fn gd_set_free_blocks(&mut self, group: u32, v: u16) {
        let off = self.gd_off(group) + 12;
        wr_u16(&mut self.gdt, off, v);
    }

    fn gd_free_inodes(&self, group: u32) -> u16 {
        rd_u16(&self.gdt, self.gd_off(group) + 14)
    }

    fn gd_set_free_inodes(&mut self, group: u32, v: u16) {
        let off = self.gd_off(group) + 14;
        wr_u16(&mut self.gdt, off, v);
    }

    fn gd_used_dirs(&self, group: u32) -> u16 {
        rd_u16(&self.gdt, self.gd_off(group) + 16)
    }

    fn gd_set_used_dirs(&mut self, group: u32, v: u16) {
        let off = self.gd_off(group) + 16;
        wr_u16(&mut self.gdt, off, v);
    }

    /// The primary superblock only. Linux does the same on an ordinary write;
    /// the backups are refreshed by e2fsck and resize2fs.
    fn write_super(&mut self) -> bool {
        let block = (SUPER_BLOCK_OFFSET / self.block_size()) as u32;
        let off = SUPER_BLOCK_OFFSET % self.block_size();

        if !self.io.read_block(block, self.tmp.as_mut_slice()) {
            trace!(0, "ext2: read of the superblock's block failed");
            return false;
        }

        let sb = self.sb;
        unsafe { write_pod(self.tmp.as_mut_slice(), off, &sb) };
        if !self.io.write_block(block, self.tmp.as_slice(), true) {
            trace!(0, "ext2: write of the superblock failed");
            return false;
        }
        true
    }

    fn write_group_descs(&mut self) -> bool {
        let bs = self.block_size();
        for i in 0..self.gdt_blocks {
            /* Through tmp: a block-sized slice of the table is not
             * page-aligned for block sizes under a page, and DMA needs it so */
            self.tmp.as_mut_slice()[..bs].copy_from_slice(&self.gdt[i * bs..(i + 1) * bs]);
            if !self.io.write_block(self.gdt_block + i as u32, self.tmp.as_slice(), true) {
                trace!(0, "ext2: write of group descriptor block {} failed", i);
                return false;
            }
        }
        true
    }

    /// Put the in-memory free counts on disk: group descriptors first, then
    /// the superblock that summarises them.
    fn commit_meta(&mut self) -> bool {
        if !self.meta_dirty {
            return true;
        }

        self.sb.write_time = wall_clock_secs() as u32;
        if !self.write_group_descs() || !self.write_super() {
            return false;
        }

        self.meta_dirty = false;
        true
    }

    fn blocks_in_group(&self, group: u32) -> u32 {
        let first = self.sb.first_data_block + group * self.sb.blocks_per_group;
        if first >= self.sb.block_count {
            return 0;
        }
        let left = self.sb.block_count - first;
        left.min(self.sb.blocks_per_group)
    }

    /* ---- mount ---- */

    pub fn mount(&mut self, read_only: bool) -> bool {
        if self.mounted {
            trace!(0, "ext2: already mounted");
            return false;
        }
        self.read_only = read_only;

        let sb = match read_super_block(&self.io.dev, self.tmp.as_mut_slice()) {
            Some(sb) => sb,
            None => return false,
        };
        self.sb = sb;

        if self.sb.magic != MAGIC {
            trace!(0, "ext2: bad magic {:#x}", self.sb.magic);
            return false;
        }

        /* Refuse formats this driver cannot parse: mounting e.g. an ext4
         * image (same magic) would return unrelated disk blocks as file data
         * with success status. Rev 0 lacks the required filetype feature. */
        if self.sb.rev_level < 1
            || self.sb.feature_incompat & !INCOMPAT_SUPPORTED != 0
            || self.sb.feature_incompat & INCOMPAT_FILETYPE == 0
        {
            trace!(0, "ext2: unsupported rev {} / incompat features {:#x}",
                self.sb.rev_level, self.sb.feature_incompat);
            return false;
        }

        /* log_block_size is raw disk data feeding a shift: bound it before
         * shifting, a count past the word size being undefined. */
        if self.sb.log_block_size > MAX_LOG_BLOCK_SIZE {
            trace!(0, "ext2: unsupported log block size {}", self.sb.log_block_size);
            return false;
        }

        let block_size = (1024usize) << self.sb.log_block_size;
        if block_size > PAGE_SIZE
            || block_size < self.io.sector_size
            || block_size % self.io.sector_size != 0
        {
            trace!(0, "ext2: unsupported block size {} (sector size {})",
                block_size, self.io.sector_size);
            return false;
        }
        self.io.block_size = block_size;
        self.ptrs_per_block = (block_size / 4) as u32;

        self.inode_size = if self.sb.inode_size > 0 {
            self.sb.inode_size as usize
        } else {
            INODE_SIZE
        };

        /* inode_size is on-disk data. read_inode copies a fixed 128 bytes
         * from an offset within a single block-sized buffer, so inode_size
         * must be at least that and divide the block size evenly -- otherwise
         * a crafted image could make the copy straddle the end of tmp. */
        if self.inode_size < INODE_SIZE || block_size % self.inode_size != 0 {
            trace!(0, "ext2: unsupported inode size {} (block size {})",
                self.inode_size, block_size);
            return false;
        }

        /* Both are divisors (here and in read_inode); a corrupt image with
         * either at 0 would divide by zero. */
        let bits = (block_size * 8) as u32;
        if self.sb.blocks_per_group == 0
            || self.sb.inodes_per_group == 0
            || self.sb.inodes_per_group > bits
            || self.sb.blocks_per_group > bits
        {
            trace!(0, "ext2: bad blocks/inodes per group {}/{}",
                self.sb.blocks_per_group, self.sb.inodes_per_group);
            return false;
        }

        if self.sb.first_data_block >= self.sb.block_count {
            trace!(0, "ext2: first data block {} beyond {}",
                self.sb.first_data_block, self.sb.block_count);
            return false;
        }
        self.io.first_data_block = self.sb.first_data_block;
        self.io.block_count = self.sb.block_count;

        self.group_count = (self.sb.block_count - self.sb.first_data_block
            + self.sb.blocks_per_group - 1) / self.sb.blocks_per_group;
        if self.group_count == 0 {
            trace!(0, "ext2: zero groups");
            return false;
        }

        /* The group descriptor table starts at block first_data_block + 1:
         * for 1 KiB blocks that is block 2 (0 boot, 1 superblock), for 4 KiB
         * blocks block 1 (the superblock is inside block 0 at offset 1024). */
        self.gdt_block = self.sb.first_data_block + 1;
        let gdt_size = self.group_count as usize * GROUP_DESC_SIZE;
        self.gdt_blocks = (gdt_size + block_size - 1) / block_size;

        let bytes = self.gdt_blocks * block_size;
        if self.gdt.try_reserve_exact(bytes).is_err() {
            trace!(0, "ext2: no memory for {} bytes of group descriptors", bytes);
            return false;
        }
        self.gdt.resize(bytes, 0);

        for i in 0..self.gdt_blocks {
            if !self.io.read_block(self.gdt_block + i as u32, self.tmp.as_mut_slice()) {
                trace!(0, "ext2: failed to read group descriptor block {}",
                    self.gdt_block as usize + i);
                return self.mount_failed();
            }
            self.gdt[i * block_size..(i + 1) * block_size]
                .copy_from_slice(&self.tmp.as_slice()[..block_size]);
        }

        for g in 0..self.group_count {
            if !self.io.is_data_block(self.gd_block_bitmap(g))
                || !self.io.is_data_block(self.gd_inode_bitmap(g))
                || !self.io.is_data_block(self.gd_inode_table(g))
            {
                trace!(0, "ext2: group {} descriptor points outside the filesystem", g);
                return self.mount_failed();
            }
        }

        self.root = new_vnode(core::ptr::null_mut(), b"/", TYPE_DIR, ROOT_INODE, 0);
        if self.root.is_null() {
            trace!(0, "ext2: no memory for the root vnode");
            return self.mount_failed();
        }

        if !self.read_only && self.sb.feature_ro_compat & !RO_COMPAT_WRITABLE != 0 {
            trace!(0, "ext2: ro_compat features {:#x} are not maintained by this driver, mounting read-only",
                self.sb.feature_ro_compat);
            self.read_only = true;
        }

        if !self.read_only {
            /* Like Linux: the valid bit is off while the filesystem is
             * mounted for writing, so a crash shows as "not cleanly
             * unmounted" to the next mount and to e2fsck. */
            if self.sb.state & STATE_VALID == 0 {
                trace!(0, "ext2: the filesystem was not cleanly unmounted");
            }

            self.sb.state &= !STATE_VALID;
            self.sb.mount_count = self.sb.mount_count.wrapping_add(1);
            self.sb.mount_time = wall_clock_secs() as u32;
            if !self.write_super() {
                trace!(0, "ext2: cannot write the superblock, mounting read-only");
                self.read_only = true;
            }
        }

        self.mounted = true;
        trace!(0, "ext2: mounted {} blocks, {} inodes, blocksize {}, {} groups, {}",
            self.sb.block_count, self.sb.inode_count, block_size, self.group_count,
            if self.read_only { "ro" } else { "rw" });
        true
    }

    /// Give back what a failed mount took, and say so.
    fn mount_failed(&mut self) -> bool {
        if !self.root.is_null() {
            free_vnode(self.root);
            self.root = core::ptr::null_mut();
        }
        self.gdt = Vec::new();
        false
    }

    pub fn unmount(&mut self) {
        if !self.mounted {
            return;
        }

        if !self.read_only {
            self.flush_bitmap();
            self.commit_meta();
            self.sb.state |= STATE_VALID;
            self.sb.write_time = wall_clock_secs() as u32;
            self.write_super();
            self.io.flush();
        }

        /* Every vnode sits in exactly one child list, so the tree walk frees
         * them all; depth is bounded by the paths that loaded them. */
        if !self.root.is_null() {
            unsafe { free_tree(self.root) };
            self.root = core::ptr::null_mut();
        }

        self.gdt = Vec::new();
        self.ind_block = 0;
        self.dind_block = 0;
        self.bitmap_block = 0;
        self.bitmap_dirty = false;
        self.meta_dirty = false;
        self.mounted = false;
    }

    pub fn sync(&mut self) -> bool {
        if !self.mounted || self.read_only {
            return true;
        }

        self.flush_bitmap() && self.commit_meta() && self.io.flush()
    }

    /* ---- inodes ---- */

    fn inode_group(&self, ino: u32) -> u32 {
        (ino - 1) / self.sb.inodes_per_group
    }

    /// Where an inode sits: the block of the inode table holding it, and the
    /// offset within that block.
    fn inode_place(&self, ino: u32) -> Option<(u32, usize)> {
        if ino == 0 || ino > self.sb.inode_count {
            trace!(0, "ext2: inode {} out of range", ino);
            return None;
        }

        let group = self.inode_group(ino);
        if group >= self.group_count {
            trace!(0, "ext2: group {} out of range for inode {}", group, ino);
            return None;
        }

        let index = (ino - 1) % self.sb.inodes_per_group;
        let byte_offset = index as usize * self.inode_size;
        let block = self.gd_inode_table(group) + (byte_offset / self.block_size()) as u32;
        Some((block, byte_offset % self.block_size()))
    }

    fn read_inode(&mut self, ino: u32) -> Option<Inode> {
        let (block, off) = self.inode_place(ino)?;
        if !self.io.read_block(block, self.tmp.as_mut_slice()) {
            trace!(0, "ext2: read of the block holding inode {} failed", ino);
            return None;
        }
        Some(unsafe { read_pod::<Inode>(self.tmp.as_slice(), off) })
    }

    /// Read-modify-write of the inode table block, with FUA: an inode commit
    /// is the point a change becomes real.
    fn write_inode(&mut self, ino: u32, inode: &Inode) -> bool {
        let (block, off) = match self.inode_place(ino) {
            Some(place) => place,
            None => return false,
        };

        if !self.io.read_block(block, self.tmp.as_mut_slice()) {
            trace!(0, "ext2: read of the block holding inode {} failed", ino);
            return false;
        }

        unsafe { write_pod(self.tmp.as_mut_slice(), off, inode) };
        if !self.io.write_block(block, self.tmp.as_slice(), true) {
            trace!(0, "ext2: write of the block holding inode {} failed", ino);
            return false;
        }
        true
    }

    fn new_inode(mode: u16) -> Inode {
        let now = wall_clock_secs() as u32;
        let mut inode: Inode = unsafe { core::mem::zeroed() };
        inode.mode = mode;
        inode.links_count = 1;
        inode.access_time = now;
        inode.create_time = now;
        inode.modify_time = now;
        inode
    }

    /* ---- allocation ---- */

    fn load_bitmap(&mut self, block: u32) -> bool {
        if self.bitmap_block == block {
            return true;
        }

        if !self.flush_bitmap() {
            return false;
        }

        if !self.io.read_block(block, self.bitmap.as_mut_slice()) {
            trace!(0, "ext2: read of bitmap block {} failed", block);
            self.bitmap_block = 0;
            return false;
        }

        self.bitmap_block = block;
        true
    }

    fn flush_bitmap(&mut self) -> bool {
        if !self.bitmap_dirty {
            return true;
        }

        if !self.io.write_block(self.bitmap_block, self.bitmap.as_slice(), true) {
            trace!(0, "ext2: write of bitmap block {} failed", self.bitmap_block);
            return false;
        }

        self.bitmap_dirty = false;
        true
    }

    /// The first clear bit of the loaded bitmap at or after `from` and below
    /// `count`, or None. A full byte is stepped over whole, which is most of
    /// them on a filesystem with anything on it.
    fn free_bit_from(&self, from: u32, count: u32) -> Option<u32> {
        let map = &self.bitmap.as_slice()[..self.block_size()];
        for byte in from as usize / 8..(count as usize + 7) / 8 {
            if map[byte] == 0xFF {
                continue;
            }
            for bit in 0..8 {
                let idx = byte * 8 + bit;
                if idx >= count as usize {
                    break;
                }
                if idx >= from as usize && !bit_test(map, idx) {
                    return Some(idx as u32);
                }
            }
        }
        None
    }

    /// A free block, from `goal_group` if it has one (keeping a file near its
    /// inode), else from the first group that does. None when the disk is full.
    fn alloc_block(&mut self, goal_group: u32) -> Option<u32> {
        for k in 0..self.group_count {
            let g = (goal_group + k) % self.group_count;
            if self.gd_free_blocks(g) == 0 {
                continue;
            }

            if !self.load_bitmap(self.gd_block_bitmap(g)) {
                return None;
            }

            let count = self.blocks_in_group(g);
            if let Some(idx) = self.free_bit_from(0, count) {
                let bs = self.block_size();
                bit_set(&mut self.bitmap.as_mut_slice()[..bs], idx as usize);
                self.bitmap_dirty = true;
                let free = self.gd_free_blocks(g);
                self.gd_set_free_blocks(g, free.saturating_sub(1));
                self.sb.free_block_count = self.sb.free_block_count.saturating_sub(1);
                self.meta_dirty = true;
                return Some(self.sb.first_data_block + g * self.sb.blocks_per_group + idx);
            }

            /* The descriptor's count and the bitmap disagree: trust the
             * bitmap and stop the descriptor from sending us here again */
            trace!(0, "ext2: group {} claims {} free blocks but its bitmap is full",
                g, self.gd_free_blocks(g));
            self.gd_set_free_blocks(g, 0);
            self.meta_dirty = true;
        }

        trace!(0, "ext2: no free blocks");
        None
    }

    fn free_block(&mut self, block: u32) -> bool {
        if !self.io.is_data_block(block) {
            trace!(0, "ext2: free of block {} outside the filesystem", block);
            return false;
        }

        let rel = block - self.sb.first_data_block;
        let g = rel / self.sb.blocks_per_group;
        let idx = (rel % self.sb.blocks_per_group) as usize;

        if !self.load_bitmap(self.gd_block_bitmap(g)) {
            return false;
        }

        let bs = self.block_size();
        if !bit_test(&self.bitmap.as_slice()[..bs], idx) {
            trace!(0, "ext2: block {} is free already", block);
            return false;
        }

        bit_clear(&mut self.bitmap.as_mut_slice()[..bs], idx);
        self.bitmap_dirty = true;
        let free = self.gd_free_blocks(g);
        self.gd_set_free_blocks(g, free.saturating_add(1));
        self.sb.free_block_count = self.sb.free_block_count.saturating_add(1);
        self.meta_dirty = true;

        /* A cached indirect block that is no longer one */
        if self.ind_block == block {
            self.ind_block = 0;
        }
        if self.dind_block == block {
            self.dind_block = 0;
        }
        true
    }

    fn alloc_inode(&mut self, goal_group: u32, is_dir: bool) -> Option<u32> {
        for k in 0..self.group_count {
            let g = (goal_group + k) % self.group_count;
            if self.gd_free_inodes(g) == 0 {
                continue;
            }

            if !self.load_bitmap(self.gd_inode_bitmap(g)) {
                return None;
            }

            let count = self.sb.inodes_per_group;
            let mut taken = None;
            let mut from = 0;
            while let Some(idx) = self.free_bit_from(from, count) {
                let ino = g * self.sb.inodes_per_group + idx + 1;
                if ino < self.sb.first_inode || ino > self.sb.inode_count {
                    from = idx + 1;
                    continue;
                }
                taken = Some((idx, ino));
                break;
            }

            if let Some((idx, ino)) = taken {
                let bs = self.block_size();
                bit_set(&mut self.bitmap.as_mut_slice()[..bs], idx as usize);
                self.bitmap_dirty = true;
                let free = self.gd_free_inodes(g);
                self.gd_set_free_inodes(g, free.saturating_sub(1));
                self.sb.free_inode_count = self.sb.free_inode_count.saturating_sub(1);
                if is_dir {
                    let dirs = self.gd_used_dirs(g);
                    self.gd_set_used_dirs(g, dirs.saturating_add(1));
                }
                self.meta_dirty = true;
                return Some(ino);
            }

            trace!(0, "ext2: group {} claims {} free inodes but its bitmap is full",
                g, self.gd_free_inodes(g));
            self.gd_set_free_inodes(g, 0);
            self.meta_dirty = true;
        }

        trace!(0, "ext2: no free inodes");
        None
    }

    fn free_inode(&mut self, ino: u32, is_dir: bool) -> bool {
        if ino < self.sb.first_inode || ino > self.sb.inode_count {
            trace!(0, "ext2: free of inode {} out of range", ino);
            return false;
        }

        let g = self.inode_group(ino);
        let idx = ((ino - 1) % self.sb.inodes_per_group) as usize;

        if !self.load_bitmap(self.gd_inode_bitmap(g)) {
            return false;
        }

        let bs = self.block_size();
        if !bit_test(&self.bitmap.as_slice()[..bs], idx) {
            trace!(0, "ext2: inode {} is free already", ino);
            return false;
        }

        bit_clear(&mut self.bitmap.as_mut_slice()[..bs], idx);
        self.bitmap_dirty = true;
        let free = self.gd_free_inodes(g);
        self.gd_set_free_inodes(g, free.saturating_add(1));
        self.sb.free_inode_count = self.sb.free_inode_count.saturating_add(1);
        if is_dir {
            let dirs = self.gd_used_dirs(g);
            self.gd_set_used_dirs(g, dirs.saturating_sub(1));
        }
        self.meta_dirty = true;
        true
    }
}

/* ---- block mapping ---- */

impl Ext2 {
    fn load_ind(&mut self, block: u32) -> bool {
        if self.ind_block == block {
            return true;
        }
        if !self.io.read_block(block, self.ind.as_mut_slice()) {
            trace!(0, "ext2: read of indirect block {} failed", block);
            self.ind_block = 0;
            return false;
        }
        self.ind_block = block;
        true
    }

    fn load_dind(&mut self, block: u32) -> bool {
        if self.dind_block == block {
            return true;
        }
        if !self.io.read_block(block, self.dind.as_mut_slice()) {
            trace!(0, "ext2: read of double indirect block {} failed", block);
            self.dind_block = 0;
            return false;
        }
        self.dind_block = block;
        true
    }

    /* Indirect blocks go down with a plain write; every path that commits an
     * inode flushes the device first, so they are on disk before the inode
     * that leads to them. */

    fn write_ind(&self, block: u32) -> bool {
        if self.io.write_block(block, self.ind.as_slice(), false) {
            return true;
        }
        trace!(0, "ext2: write of indirect block {} failed", block);
        false
    }

    fn write_dind(&self, block: u32) -> bool {
        if self.io.write_block(block, self.dind.as_slice(), false) {
            return true;
        }
        trace!(0, "ext2: write of double indirect block {} failed", block);
        false
    }

    fn ind_ptr(&self, index: u32) -> u32 {
        rd_u32(self.ind.as_slice(), index as usize * 4)
    }

    fn set_ind_ptr(&mut self, index: u32, v: u32) {
        wr_u32(self.ind.as_mut_slice(), index as usize * 4, v);
    }

    fn dind_ptr(&self, index: u32) -> u32 {
        rd_u32(self.dind.as_slice(), index as usize * 4)
    }

    fn set_dind_ptr(&mut self, index: u32, v: u32) {
        wr_u32(self.dind.as_mut_slice(), index as usize * 4, v);
    }

    fn block_units(&self) -> u32 {
        (self.block_size() / BLOCKS_UNIT as usize) as u32
    }

    /// A fresh block, zeroed in the `ind` buffer and on disk, for an indirect
    /// level that was missing.
    fn fresh_ind(&mut self, goal_group: u32) -> Option<u32> {
        let block = self.alloc_block(goal_group)?;
        let bs = self.block_size();
        self.ind.as_mut_slice()[..bs].fill(0);
        self.ind_block = block;
        if !self.write_ind(block) {
            return None;
        }
        Some(block)
    }

    fn fresh_dind(&mut self, goal_group: u32) -> Option<u32> {
        let block = self.alloc_block(goal_group)?;
        let bs = self.block_size();
        self.dind.as_mut_slice()[..bs].fill(0);
        self.dind_block = block;
        if !self.write_dind(block) {
            return None;
        }
        Some(block)
    }

    /// The physical block behind logical block `logical` of `inode`, and
    /// whether it is fresh -- holding nothing worth reading. With `allocate`,
    /// a missing data block is allocated, and so is a missing indirect block
    /// on the way, zeroed on disk; without it a hole comes back as block 0.
    /// None means an I/O error or corruption: an on-disk pointer that points
    /// outside the filesystem, or a file that needs the triple-indirect
    /// block, which this driver does not do -- failing beats silently
    /// answering with zeros for the block's data.
    fn map_block(
        &mut self, inode: &mut Inode, goal_group: u32, logical: u32, allocate: bool,
    ) -> Option<(u32, bool)> {
        let units = self.block_units();

        /* Direct blocks (0..11) */
        if logical < DIRECT_BLOCKS {
            let slot = inode.block[logical as usize];
            if slot != 0 {
                if !self.io.is_data_block(slot) {
                    trace!(0, "ext2: direct block {} points outside the filesystem", slot);
                    return None;
                }
                return Some((slot, false));
            }
            if !allocate {
                return Some((0, false));
            }

            let b = self.alloc_block(goal_group)?;
            inode.block[logical as usize] = b;
            inode.blocks = inode.blocks.saturating_add(units);
            return Some((b, true));
        }

        let ppb = self.ptrs_per_block;
        let mut rel = logical - DIRECT_BLOCKS;
        let ind_block;
        let ind_index;

        if rel < ppb {
            /* Single indirect (12) */
            ind_index = rel;
            let existing = inode.block[INDIRECT_SLOT];
            if existing == 0 {
                if !allocate {
                    return Some((0, false));
                }
                let b = self.fresh_ind(goal_group)?;
                inode.block[INDIRECT_SLOT] = b;
                inode.blocks = inode.blocks.saturating_add(units);
                ind_block = b;
            } else if !self.io.is_data_block(existing) {
                trace!(0, "ext2: indirect block {} points outside the filesystem", existing);
                return None;
            } else {
                ind_block = existing;
            }
        } else if ((rel - ppb) as u64) < ppb as u64 * ppb as u64 {
            /* Double indirect (13) */
            rel -= ppb;
            let l1_index = rel / ppb;
            ind_index = rel % ppb;

            let existing = inode.block[DINDIRECT_SLOT];
            let dind_block = if existing == 0 {
                if !allocate {
                    return Some((0, false));
                }
                let b = self.fresh_dind(goal_group)?;
                inode.block[DINDIRECT_SLOT] = b;
                inode.blocks = inode.blocks.saturating_add(units);
                b
            } else if !self.io.is_data_block(existing) {
                trace!(0, "ext2: double indirect block {} points outside the filesystem", existing);
                return None;
            } else {
                existing
            };

            if !self.load_dind(dind_block) {
                return None;
            }

            let existing = self.dind_ptr(l1_index);
            if existing == 0 {
                if !allocate {
                    return Some((0, false));
                }
                let b = self.fresh_ind(goal_group)?;
                self.set_dind_ptr(l1_index, b);
                if !self.write_dind(dind_block) {
                    return None;
                }
                inode.blocks = inode.blocks.saturating_add(units);
                ind_block = b;
            } else if !self.io.is_data_block(existing) {
                trace!(0, "ext2: indirect block {} points outside the filesystem", existing);
                return None;
            } else {
                ind_block = existing;
            }
        } else {
            trace!(0, "ext2: triple indirect is not supported (logical block {})", logical);
            return None;
        }

        if !self.load_ind(ind_block) {
            return None;
        }

        let slot = self.ind_ptr(ind_index);
        if slot != 0 {
            if !self.io.is_data_block(slot) {
                trace!(0, "ext2: block {} points outside the filesystem", slot);
                return None;
            }
            return Some((slot, false));
        }
        if !allocate {
            return Some((0, false));
        }

        let b = self.alloc_block(goal_group)?;
        self.set_ind_ptr(ind_index, b);
        if !self.write_ind(ind_block) {
            return None;
        }
        inode.blocks = inode.blocks.saturating_add(units);
        Some((b, true))
    }

    /// Walk the file's blocks downward from `last_kept - 1` to `from_block`,
    /// clearing each pointer and collecting the block for release, an
    /// indirect block too once its last entry is gone. Stops when the batch
    /// is full; `last_kept` says where to resume. Modified indirect blocks
    /// are written before returning, so the inode the caller commits next
    /// leads to a consistent tree.
    fn release_tail(
        &mut self, inode: &mut Inode, from_block: u32, batch: &mut Vec<u32>, last_kept: &mut u32,
    ) -> bool {
        let units = self.block_units();
        let ppb = self.ptrs_per_block;
        let mut ind_dirty = false;
        let mut dind_dirty = false;

        while *last_kept > from_block && batch.len() < FREE_BATCH {
            let logical = *last_kept - 1;

            if logical < DIRECT_BLOCKS {
                let p = inode.block[logical as usize];
                if p != 0 {
                    batch.push(p);
                    inode.block[logical as usize] = 0;
                    inode.blocks = inode.blocks.saturating_sub(units);
                }
            } else if logical - DIRECT_BLOCKS < ppb {
                let i = logical - DIRECT_BLOCKS;
                let ind_block = inode.block[INDIRECT_SLOT];
                if ind_block != 0 && self.io.is_data_block(ind_block) {
                    if !self.load_ind(ind_block) {
                        return false;
                    }
                    if self.ind_ptr(i) != 0 {
                        batch.push(self.ind_ptr(i));
                        self.set_ind_ptr(i, 0);
                        ind_dirty = true;
                        inode.blocks = inode.blocks.saturating_sub(units);
                    }
                    if i == 0 {
                        /* Nothing is left behind this indirect block */
                        if batch.len() >= FREE_BATCH {
                            /* Batch full: keep the empty indirect block for
                             * the next round rather than leak it */
                            break;
                        }
                        batch.push(ind_block);
                        inode.block[INDIRECT_SLOT] = 0;
                        inode.blocks = inode.blocks.saturating_sub(units);
                        self.ind_block = 0;
                        ind_dirty = false;
                    }
                } else if ind_block != 0 {
                    trace!(0, "ext2: indirect block {} points outside the filesystem, dropped",
                        ind_block);
                    inode.block[INDIRECT_SLOT] = 0;
                }
            } else if ((logical - DIRECT_BLOCKS - ppb) as u64) < ppb as u64 * ppb as u64 {
                let rel = logical - DIRECT_BLOCKS - ppb;
                let i1 = rel / ppb;
                let i2 = rel % ppb;
                let dind_block = inode.block[DINDIRECT_SLOT];
                if dind_block != 0 && self.io.is_data_block(dind_block) {
                    if !self.load_dind(dind_block) {
                        return false;
                    }
                    let ind_block = self.dind_ptr(i1);
                    if ind_block != 0 && self.io.is_data_block(ind_block) {
                        if !self.load_ind(ind_block) {
                            return false;
                        }
                        if self.ind_ptr(i2) != 0 {
                            batch.push(self.ind_ptr(i2));
                            self.set_ind_ptr(i2, 0);
                            ind_dirty = true;
                            inode.blocks = inode.blocks.saturating_sub(units);
                        }
                        if i2 == 0 {
                            if batch.len() >= FREE_BATCH {
                                break;
                            }
                            batch.push(ind_block);
                            self.set_dind_ptr(i1, 0);
                            dind_dirty = true;
                            inode.blocks = inode.blocks.saturating_sub(units);
                            self.ind_block = 0;
                            ind_dirty = false;
                        }
                    } else if ind_block != 0 {
                        trace!(0, "ext2: indirect block {} points outside the filesystem, dropped",
                            ind_block);
                        self.set_dind_ptr(i1, 0);
                        dind_dirty = true;
                    }

                    if rel == 0 {
                        if batch.len() >= FREE_BATCH {
                            break;
                        }
                        batch.push(dind_block);
                        inode.block[DINDIRECT_SLOT] = 0;
                        inode.blocks = inode.blocks.saturating_sub(units);
                        self.dind_block = 0;
                        dind_dirty = false;
                    }
                } else if dind_block != 0 {
                    trace!(0, "ext2: double indirect block {} points outside the filesystem, dropped",
                        dind_block);
                    inode.block[DINDIRECT_SLOT] = 0;
                }
            }
            /* Past what this driver maps there is nothing to release */

            *last_kept = logical;
        }

        if ind_dirty && self.ind_block != 0 && !self.write_ind(self.ind_block) {
            return false;
        }
        if dind_dirty && self.dind_block != 0 && !self.write_dind(self.dind_block) {
            return false;
        }
        true
    }

    /* ---- data ---- */

    fn read_inode_data(&mut self, inode: &Inode, buf: &mut [u8], offset: usize) -> bool {
        let file_size = inode.size as usize;
        if offset >= file_size {
            return false;
        }

        let bs = self.block_size();
        let len = buf.len().min(file_size - offset);
        let mut done = 0;
        let mut block_idx = (offset / bs) as u32;
        let mut byte_off = offset % bs;

        /* A read maps but never allocates, so the inode is not modified */
        let mut scratch = *inode;

        while done < len {
            let (phys, _) = match self.map_block(&mut scratch, 0, block_idx, false) {
                Some(mapped) => mapped,
                None => {
                    trace!(0, "ext2: logical block {} is unmappable", block_idx);
                    return false;
                }
            };

            let chunk = (bs - byte_off).min(len - done);
            if phys == 0 {
                /* A hole reads as zeros */
                buf[done..done + chunk].fill(0);
            } else {
                if !self.io.read_block(phys, self.data.as_mut_slice()) {
                    trace!(0, "ext2: read of block {} failed", phys);
                    return false;
                }
                buf[done..done + chunk]
                    .copy_from_slice(&self.data.as_slice()[byte_off..byte_off + chunk]);
            }

            done += chunk;
            byte_off = 0;
            block_idx += 1;
        }

        true
    }

    /// Write `data` at `offset`, allocating what the range needs. Data blocks
    /// go down with plain writes; the caller flushes the device and then
    /// commits the inode, so the content is on disk before anything points at
    /// it. On failure the inode still describes every block allocated so far,
    /// and the caller commits it as it is, which keeps the bitmap honest.
    fn write_inode_data(
        &mut self, inode: &mut Inode, goal_group: u32, data: &[u8], offset: usize,
    ) -> bool {
        let bs = self.block_size();
        let mut done = 0;
        let mut block_idx = (offset / bs) as u32;
        let mut byte_off = offset % bs;

        while done < data.len() {
            let (phys, fresh) = match self.map_block(inode, goal_group, block_idx, true) {
                Some(mapped) => mapped,
                None => {
                    trace!(0, "ext2: logical block {} is unmappable", block_idx);
                    return false;
                }
            };

            let chunk = (bs - byte_off).min(data.len() - done);
            if chunk < bs {
                if fresh {
                    self.data.as_mut_slice()[..bs].fill(0);
                } else if !self.io.read_block(phys, self.data.as_mut_slice()) {
                    trace!(0, "ext2: read of block {} failed", phys);
                    return false;
                }
            }

            self.data.as_mut_slice()[byte_off..byte_off + chunk]
                .copy_from_slice(&data[done..done + chunk]);
            if !self.io.write_block(phys, self.data.as_slice(), false) {
                trace!(0, "ext2: write of block {} failed", phys);
                return false;
            }

            done += chunk;
            byte_off = 0;
            block_idx += 1;
        }

        let end = offset + data.len();
        if end > inode.size as usize {
            inode.size = end as u32;
        }
        inode.modify_time = wall_clock_secs() as u32;
        true
    }

    /// Shrink (or grow, sparsely) an inode to `new_size`, releasing the
    /// blocks past it in batches: each batch is cut off the tree, the inode
    /// committed without it, and only then are its blocks freed in the
    /// bitmap, so a crash leaks at most one batch to e2fsck and never leaves
    /// a block both in use and free.
    fn truncate_inode(&mut self, ino: u32, inode: &mut Inode, new_size: usize) -> bool {
        if new_size > u32::MAX as usize {
            trace!(0, "ext2: size {} is too large", new_size);
            return false;
        }

        inode.modify_time = wall_clock_secs() as u32;
        if new_size >= inode.size as usize {
            /* Growth is a hole: reads see zeros, no block is spent */
            inode.size = new_size as u32;
            return true;
        }

        let bs = self.block_size();
        let keep_blocks = ((new_size + bs - 1) / bs) as u32;
        let mut last_kept = ((inode.size as usize + bs - 1) / bs) as u32;
        inode.size = new_size as u32;

        let mut batch: Vec<u32> = Vec::new();
        if batch.try_reserve_exact(FREE_BATCH).is_err() {
            trace!(0, "ext2: no memory for the batch of blocks to free");
            return false;
        }

        while last_kept > keep_blocks {
            batch.clear();
            if !self.release_tail(inode, keep_blocks, &mut batch, &mut last_kept) {
                return false;
            }

            if !self.io.flush() || !self.write_inode(ino, inode) {
                return false;
            }

            for i in 0..batch.len() {
                self.free_block(batch[i]);
            }
        }

        /* The kept tail block: zero what lies past the new end, or a later
         * write inside that block would leave old bytes in the gap */
        let tail = new_size % bs;
        if tail != 0 {
            let (phys, _) = match self.map_block(inode, 0, keep_blocks - 1, false) {
                Some(mapped) => mapped,
                None => return false,
            };
            if phys != 0 {
                if !self.io.read_block(phys, self.data.as_mut_slice()) {
                    return false;
                }
                self.data.as_mut_slice()[tail..bs].fill(0);
                if !self.io.write_block(phys, self.data.as_slice(), false) {
                    return false;
                }
            }
        }

        true
    }
}

/* ---- vnodes ---- */

/// A vnode of this filesystem, linked into its parent's children. Null when
/// there is no memory for it: a directory with more entries than the kernel
/// has room for is a failure to report, not one to panic on.
fn new_vnode(
    parent: *mut VNode, name: &[u8], node_type: c_int, ino: u32, size: usize,
) -> *mut VNode {
    let node = vnode::alloc();
    if node.is_null() {
        trace!(0, "ext2: no memory for a vnode");
        return node;
    }

    unsafe {
        let n = &mut *node;
        let len = name.len().min(NAME_MAX - 1);
        n.name[..len].copy_from_slice(&name[..len]);
        n.node_type = node_type;
        n.parent = parent;
        n.size = if node_type == TYPE_FILE { size } else { 0 };
        n.ino = ino as usize;

        if !parent.is_null() {
            vnode::insert_child(parent, node);
        }
    }
    node
}

fn free_vnode(node: *mut VNode) {
    unsafe { vnode::free(node) };
}

/// # Safety
/// `node` is a vnode of this filesystem, off its parent's list already or
/// being freed with the tree it heads.
unsafe fn free_tree(node: *mut VNode) {
    unsafe { vnode::free_tree(node) };
}

/* ---- directories ---- */

impl Ext2 {
    /// Read a directory's entries into its vnode the first time it is needed.
    /// Only what a path walk touches is ever loaded, so a big tree costs
    /// memory in proportion to what is used, not to what is on disk.
    pub fn load_dir(&mut self, dir: *mut VNode) -> bool {
        if dir.is_null() || !unsafe { (*dir).is_dir() } {
            return false;
        }
        if unsafe { (*dir).flags } & FLAG_DIR_LOADED != 0 {
            return true;
        }

        let dir_ino = unsafe { (*dir).ino } as u32;
        let inode = match self.read_inode(dir_ino) {
            Some(inode) => inode,
            None => {
                trace!(0, "ext2: read of directory inode {} failed", dir_ino);
                return false;
            }
        };

        if inode.mode & MODE_TYPE_MASK != MODE_DIR {
            trace!(0, "ext2: inode {} is not a directory", dir_ino);
            return false;
        }

        let bs = self.block_size();
        let dir_blocks = ((inode.size as usize + bs - 1) / bs) as u32;
        let mut scratch = inode;

        for blk in 0..dir_blocks {
            let (phys, _) = match self.map_block(&mut scratch, 0, blk, false) {
                Some(mapped) => mapped,
                None => return false,
            };
            if phys == 0 {
                continue;
            }
            if !self.io.read_block(phys, self.data.as_mut_slice()) {
                trace!(0, "ext2: read of block {} of inode {} failed", blk, dir_ino);
                return false;
            }

            let mut pos = 0;
            while pos + DIR_ENTRY_HEADER <= bs {
                let (ino, rec_len, name_len, file_type) = {
                    let b = self.data.as_slice();
                    (de_inode(b, pos), de_rec_len(b, pos), de_name_len(b, pos), de_file_type(b, pos))
                };

                if rec_len < DIR_ENTRY_HEADER + name_len
                    || rec_len > bs - pos
                    || rec_len % DIR_ENTRY_ALIGN != 0
                {
                    trace!(0, "ext2: bad directory entry at offset {} of inode {}",
                        blk as usize * bs + pos, dir_ino);
                    break;
                }

                if ino != 0 && name_len > 0 && ino <= self.sb.inode_count {
                    let mut name = [0u8; NAME_MAX];
                    let skip = {
                        let b = self.data.as_slice();
                        let raw = de_name(b, pos);
                        if raw == b"." || raw == b".." {
                            true
                        } else if name_len >= NAME_MAX {
                            /* A truncated name would collide with other long
                             * names in a lookup; skip the entry instead of
                             * silently shortening it */
                            trace!(0, "ext2: a name of {} bytes in inode {} is too long, skipped",
                                name_len, dir_ino);
                            true
                        } else {
                            name[..name_len].copy_from_slice(raw);
                            false
                        }
                    };

                    if !skip && !self.adopt(dir, &name[..name_len], ino, file_type) {
                        return false;
                    }
                }

                pos += rec_len;
            }
        }

        unsafe { (*dir).flags |= FLAG_DIR_LOADED };
        true
    }

    /// Make a vnode for one directory entry, if it names something this
    /// driver puts in the tree. False only when there is no memory for it.
    fn adopt(&mut self, dir: *mut VNode, name: &[u8], ino: u32, file_type: u8) -> bool {
        /* Everything but files and directories -- symlinks, devices -- is
         * left out of the tree */
        let mut is_dir = file_type == DIR_TYPE_DIR;
        let mut is_file = file_type == DIR_TYPE_FILE;
        let mut size = 0;

        if is_file || file_type == DIR_TYPE_UNKNOWN {
            match self.read_inode(ino) {
                Some(child) => {
                    let mode = child.mode & MODE_TYPE_MASK;
                    is_dir = mode == MODE_DIR;
                    is_file = mode == MODE_FILE;
                    size = child.size as usize;
                    if is_file && child.dir_acl != 0 {
                        trace!(0, "ext2: inode {} is over 4 GiB, skipped", ino);
                        is_file = false;
                    }
                }
                None => {
                    is_dir = false;
                    is_file = false;
                }
            }
        }

        /* A directory entry that leads back up the tree is a cycle in the
         * image; following it would never end */
        if is_dir {
            let mut up = dir;
            while !up.is_null() {
                if unsafe { (*up).ino } == ino as usize {
                    trace!(0, "ext2: an entry of inode {} is an ancestor, skipped",
                        unsafe { (*dir).ino });
                    is_dir = false;
                    break;
                }
                up = unsafe { (*up).parent };
            }
        }

        if !is_dir && !is_file {
            return true;
        }

        let node = new_vnode(
            dir, name, if is_dir { TYPE_DIR } else { TYPE_FILE }, ino, size);
        !node.is_null()
    }

    /// Put (ino, name) into `dir`: in the first slack big enough in a block
    /// it has, else in a new block appended to it. `dir_inode` is updated in
    /// memory (size, mtime, the htree flag dropped); the caller commits it.
    fn add_dir_entry(
        &mut self, dir: *mut VNode, dir_inode: &mut Inode, ino: u32, name: &[u8], file_type: u8,
    ) -> bool {
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return false;
        }
        let need = dir_entry_size(name.len());
        let bs = self.block_size();

        dir_inode.flags &= !INODE_FLAG_INDEX;
        dir_inode.modify_time = wall_clock_secs() as u32;

        let dir_ino = unsafe { (*dir).ino } as u32;
        let goal_group = self.inode_group(dir_ino);
        let dir_blocks = ((dir_inode.size as usize + bs - 1) / bs) as u32;

        for blk in 0..dir_blocks {
            let (phys, _) = match self.map_block(dir_inode, goal_group, blk, false) {
                Some(mapped) => mapped,
                None => return false,
            };
            if phys == 0 {
                continue;
            }
            if !self.io.read_block(phys, self.data.as_mut_slice()) {
                return false;
            }

            let mut pos = 0;
            while pos + DIR_ENTRY_HEADER <= bs {
                let (at_ino, rec_len, name_len) = {
                    let b = self.data.as_slice();
                    (de_inode(b, pos), de_rec_len(b, pos), de_name_len(b, pos))
                };

                /* An occupied record must hold its own name, or the slack
                 * computed below would wrap and place the new entry past the
                 * end of the block */
                if rec_len < DIR_ENTRY_HEADER
                    || rec_len > bs - pos
                    || rec_len % DIR_ENTRY_ALIGN != 0
                    || (at_ino != 0 && rec_len < DIR_ENTRY_HEADER + name_len)
                {
                    trace!(0, "ext2: bad directory entry at offset {} of inode {}",
                        blk as usize * bs + pos, dir_ino);
                    return false;
                }

                let used = if at_ino == 0 { 0 } else { dir_entry_size(name_len) };
                if rec_len - used >= need {
                    let b = self.data.as_mut_slice();
                    let at = if used == 0 {
                        /* A free slot: take it whole */
                        pos
                    } else {
                        de_set_rec_len(b, pos, used);
                        de_set_rec_len(b, pos + used, rec_len - used);
                        pos + used
                    };
                    de_fill(b, at, ino, name, file_type);
                    return self.io.write_block(phys, self.data.as_slice(), true);
                }

                pos += rec_len;
            }
        }

        /* No room: a new block holding this one entry */
        let (phys, _) = match self.map_block(dir_inode, goal_group, dir_blocks, true) {
            Some(mapped) => mapped,
            None => return false,
        };

        let b = self.data.as_mut_slice();
        b[..bs].fill(0);
        de_set_rec_len(b, 0, bs);
        de_fill(b, 0, ino, name, file_type);
        if !self.io.write_block(phys, self.data.as_slice(), true) {
            return false;
        }

        dir_inode.size += bs as u32;
        true
    }

    /// Take (ino, name) out of `dir`: the entry is folded into its
    /// predecessor's record, or emptied if it leads its block.
    fn remove_dir_entry(
        &mut self, dir: *mut VNode, dir_inode: &mut Inode, ino: u32, name: &[u8],
    ) -> bool {
        let bs = self.block_size();
        let dir_ino = unsafe { (*dir).ino } as u32;
        let dir_blocks = ((dir_inode.size as usize + bs - 1) / bs) as u32;

        dir_inode.flags &= !INODE_FLAG_INDEX;
        dir_inode.modify_time = wall_clock_secs() as u32;

        for blk in 0..dir_blocks {
            let (phys, _) = match self.map_block(dir_inode, 0, blk, false) {
                Some(mapped) => mapped,
                None => return false,
            };
            if phys == 0 {
                continue;
            }
            if !self.io.read_block(phys, self.data.as_mut_slice()) {
                return false;
            }

            let mut pos = 0;
            let mut prev: Option<usize> = None;
            while pos + DIR_ENTRY_HEADER <= bs {
                let (rec_len, name_len) = {
                    let b = self.data.as_slice();
                    (de_rec_len(b, pos), de_name_len(b, pos))
                };

                if rec_len < DIR_ENTRY_HEADER + name_len
                    || rec_len > bs - pos
                    || rec_len % DIR_ENTRY_ALIGN != 0
                {
                    trace!(0, "ext2: bad directory entry at offset {} of inode {}",
                        blk as usize * bs + pos, dir_ino);
                    return false;
                }

                let matches = {
                    let b = self.data.as_slice();
                    de_inode(b, pos) == ino && de_name(b, pos) == name
                };

                if matches {
                    let b = self.data.as_mut_slice();
                    match prev {
                        Some(before) => {
                            let grown = de_rec_len(b, before) + rec_len;
                            de_set_rec_len(b, before, grown);
                        }
                        None => de_set_inode(b, pos, 0),
                    }
                    return self.io.write_block(phys, self.data.as_slice(), true);
                }

                prev = Some(pos);
                pos += rec_len;
            }
        }

        trace!(0, "ext2: an entry to remove is not in inode {}", dir_ino);
        false
    }

    /// Point a moved directory's ".." at its new parent
    fn set_dot_dot(&mut self, dir_inode: &mut Inode, parent_ino: u32) -> bool {
        let bs = self.block_size();
        let (phys, _) = match self.map_block(dir_inode, 0, 0, false) {
            Some(mapped) => mapped,
            None => return false,
        };
        if phys == 0 || !self.io.read_block(phys, self.data.as_mut_slice()) {
            return false;
        }

        let dot_len = de_rec_len(self.data.as_slice(), 0);
        if dot_len < DIR_ENTRY_HEADER || dot_len + DIR_ENTRY_HEADER + 2 > bs {
            return false;
        }

        let b = self.data.as_slice();
        if de_name_len(b, dot_len) != 2 || &b[dot_len + DIR_ENTRY_HEADER..dot_len + 10] != b".." {
            trace!(0, "ext2: the second entry of a directory is not ..");
            return false;
        }

        de_set_inode(self.data.as_mut_slice(), dot_len, parent_ino);
        self.io.write_block(phys, self.data.as_slice(), true)
    }

    fn link_count_adjust(&mut self, ino: u32, delta: i32) -> bool {
        let mut inode = match self.read_inode(ino) {
            Some(inode) => inode,
            None => return false,
        };

        if delta < 0 {
            inode.links_count = inode.links_count.saturating_sub((-delta) as u16);
        } else {
            inode.links_count = inode.links_count.saturating_add(delta as u16);
        }
        inode.modify_time = wall_clock_secs() as u32;
        self.write_inode(ino, &inode)
    }
}

/* ---- what the VFS calls ---- */

impl Ext2 {
    pub fn root(&self) -> *mut VNode {
        self.root
    }

    pub fn lookup(&mut self, dir: *mut VNode, name: &[u8]) -> *mut VNode {
        if dir.is_null() || !unsafe { (*dir).is_dir() } || !self.load_dir(dir) {
            return core::ptr::null_mut();
        }

        for child in unsafe { vnode::children(dir) } {
            if unsafe { (*child).name_is(name) } {
                return child;
            }
        }
        core::ptr::null_mut()
    }

    pub fn read(&mut self, file: *mut VNode, buf: &mut [u8], offset: usize) -> bool {
        if file.is_null() || !unsafe { (*file).is_file() } {
            trace!(0, "ext2: a read of something that is not a file");
            return false;
        }
        if buf.is_empty() {
            return true;
        }

        let ino = unsafe { (*file).ino } as u32;
        let inode = match self.read_inode(ino) {
            Some(inode) => inode,
            None => {
                trace!(0, "ext2: read of inode {} failed", ino);
                return false;
            }
        };

        self.read_inode_data(&inode, buf, offset)
    }

    pub fn write(&mut self, file: *mut VNode, data: &[u8], offset: usize) -> bool {
        if file.is_null() || !unsafe { (*file).is_file() } {
            trace!(0, "ext2: a write to something that is not a file");
            return false;
        }
        if self.read_only {
            trace!(0, "ext2: read-only");
            return false;
        }
        if data.is_empty() {
            return true;
        }

        let end = offset as u64 + data.len() as u64;
        if end > u32::MAX as u64 {
            trace!(0, "ext2: a write of {} bytes at {} is past what ext2 holds",
                data.len(), offset);
            return false;
        }

        let ino = unsafe { (*file).ino } as u32;
        let mut inode = match self.read_inode(ino) {
            Some(inode) => inode,
            None => {
                trace!(0, "ext2: read of inode {} failed", ino);
                return false;
            }
        };

        if inode.mode & MODE_TYPE_MASK != MODE_FILE {
            trace!(0, "ext2: inode {} is not a regular file", ino);
            return false;
        }

        let group = self.inode_group(ino);
        let ok = self.write_inode_data(&mut inode, group, data, offset);

        /* Commit order: the allocation bits, the data and indirect blocks,
         * then the inode that leads to them, then the counts. Also on
         * failure: the blocks the inode picked up are then owned rather than
         * leaked. */
        if !self.flush_bitmap() || !self.io.flush() || !self.write_inode(ino, &inode)
            || !self.commit_meta()
        {
            trace!(0, "ext2: commit of inode {} failed", ino);
            return false;
        }

        unsafe { (*file).size = inode.size as usize };
        ok
    }

    pub fn truncate(&mut self, file: *mut VNode, size: usize) -> bool {
        if file.is_null() || !unsafe { (*file).is_file() } {
            trace!(0, "ext2: a truncate of something that is not a file");
            return false;
        }
        if self.read_only {
            trace!(0, "ext2: read-only");
            return false;
        }

        let ino = unsafe { (*file).ino } as u32;
        let mut inode = match self.read_inode(ino) {
            Some(inode) => inode,
            None => {
                trace!(0, "ext2: read of inode {} failed", ino);
                return false;
            }
        };

        if inode.mode & MODE_TYPE_MASK != MODE_FILE {
            trace!(0, "ext2: inode {} is not a regular file", ino);
            return false;
        }

        let ok = self.truncate_inode(ino, &mut inode, size);

        if !self.io.flush() || !self.write_inode(ino, &inode) || !self.flush_bitmap()
            || !self.commit_meta()
        {
            trace!(0, "ext2: commit of inode {} failed", ino);
            return false;
        }

        unsafe { (*file).size = inode.size as usize };
        ok
    }

    /// The name a create is given, checked: empty and over-long ones are
    /// refused rather than truncated into a collision.
    fn usable_name(&self, name: &[u8]) -> bool {
        !name.is_empty() && name.len() < NAME_MAX
    }

    pub fn create_file(&mut self, dir: *mut VNode, name: &[u8]) -> *mut VNode {
        let null = core::ptr::null_mut();
        if dir.is_null() || !unsafe { (*dir).is_dir() } {
            trace!(0, "ext2: a file made somewhere that is not a directory");
            return null;
        }
        if self.read_only {
            trace!(0, "ext2: read-only");
            return null;
        }
        if !self.usable_name(name) {
            trace!(0, "ext2: a name of {} bytes cannot be made", name.len());
            return null;
        }
        if !self.lookup(dir, name).is_null() {
            trace!(0, "ext2: there is something by that name already");
            return null;
        }

        let dir_ino = unsafe { (*dir).ino } as u32;
        let mut dir_inode = match self.read_inode(dir_ino) {
            Some(inode) => inode,
            None => return null,
        };

        let group = self.inode_group(dir_ino);
        let ino = match self.alloc_inode(group, false) {
            Some(ino) => ino,
            None => return null,
        };

        /* The inode and its allocation bit are on disk before anything names
         * it: a crash in between leaves an unreferenced inode for e2fsck, not
         * a name leading nowhere */
        let inode = Ext2::new_inode(MODE_FILE_DEFAULT);
        if !self.write_inode(ino, &inode) || !self.flush_bitmap() || !self.commit_meta() {
            trace!(0, "ext2: commit of inode {} failed", ino);
            self.give_back_inode(ino, false);
            return null;
        }

        if !self.add_dir_entry(dir, &mut dir_inode, ino, name, DIR_TYPE_FILE) {
            trace!(0, "ext2: the directory entry could not be added");
            self.give_back_inode(ino, false);
            return null;
        }

        if !self.flush_bitmap() || !self.io.flush()
            || !self.write_inode(dir_ino, &dir_inode) || !self.commit_meta()
        {
            trace!(0, "ext2: commit of directory inode {} failed", dir_ino);
            return null;
        }

        new_vnode(dir, name, TYPE_FILE, ino, 0)
    }

    /// Undo an allocation a create could not finish, and put what that
    /// changed on disk; there is nothing to do about a failure here.
    fn give_back_inode(&mut self, ino: u32, is_dir: bool) {
        self.free_inode(ino, is_dir);
        self.flush_bitmap();
        self.commit_meta();
    }

    fn give_back(&mut self, ino: u32, block: u32, is_dir: bool) {
        self.free_block(block);
        self.free_inode(ino, is_dir);
        self.flush_bitmap();
        self.commit_meta();
    }

    pub fn create_dir(&mut self, dir: *mut VNode, name: &[u8]) -> *mut VNode {
        let null = core::ptr::null_mut();
        if dir.is_null() || !unsafe { (*dir).is_dir() } {
            trace!(0, "ext2: a directory made somewhere that is not a directory");
            return null;
        }
        if self.read_only {
            trace!(0, "ext2: read-only");
            return null;
        }
        if !self.usable_name(name) {
            trace!(0, "ext2: a name of {} bytes cannot be made", name.len());
            return null;
        }
        if !self.lookup(dir, name).is_null() {
            trace!(0, "ext2: there is something by that name already");
            return null;
        }

        let dir_ino = unsafe { (*dir).ino } as u32;
        let mut dir_inode = match self.read_inode(dir_ino) {
            Some(inode) => inode,
            None => return null,
        };

        let group = self.inode_group(dir_ino);
        let ino = match self.alloc_inode(group, true) {
            Some(ino) => ino,
            None => return null,
        };

        let goal = self.inode_group(ino);
        let block = match self.alloc_block(goal) {
            Some(block) => block,
            None => {
                self.free_inode(ino, true);
                return null;
            }
        };

        /* "." and "..", the latter spanning the rest of the block */
        let bs = self.block_size();
        let dot_len = dir_entry_size(1);
        {
            let b = self.data.as_mut_slice();
            b[..bs].fill(0);
            de_set_rec_len(b, 0, dot_len);
            de_fill(b, 0, ino, b".", DIR_TYPE_DIR);
            de_set_rec_len(b, dot_len, bs - dot_len);
            de_fill(b, dot_len, dir_ino, b"..", DIR_TYPE_DIR);
        }

        let mut inode = Ext2::new_inode(MODE_DIR_DEFAULT);
        inode.links_count = 2;
        inode.size = bs as u32;
        inode.blocks = self.block_units();
        inode.block[0] = block;

        if !self.io.write_block(block, self.data.as_slice(), true)
            || !self.write_inode(ino, &inode)
            || !self.flush_bitmap()
            || !self.commit_meta()
        {
            trace!(0, "ext2: commit of inode {} failed", ino);
            self.give_back(ino, block, true);
            return null;
        }

        if !self.add_dir_entry(dir, &mut dir_inode, ino, name, DIR_TYPE_DIR) {
            trace!(0, "ext2: the directory entry could not be added");
            self.give_back(ino, block, true);
            return null;
        }

        /* The new directory's ".." is a link to the parent */
        dir_inode.links_count = dir_inode.links_count.saturating_add(1);
        if !self.flush_bitmap() || !self.io.flush()
            || !self.write_inode(dir_ino, &dir_inode) || !self.commit_meta()
        {
            trace!(0, "ext2: commit of directory inode {} failed", dir_ino);
            return null;
        }

        let node = new_vnode(dir, name, TYPE_DIR, ino, 0);
        if !node.is_null() {
            unsafe { (*node).flags |= FLAG_DIR_LOADED };
        }
        node
    }

    pub fn rename(&mut self, node: *mut VNode, new_dir: *mut VNode, new_name: &[u8]) -> bool {
        if node.is_null() || new_dir.is_null() {
            trace!(0, "ext2: a rename of nothing");
            return false;
        }
        if self.read_only {
            trace!(0, "ext2: read-only");
            return false;
        }
        if unsafe { (*node).parent }.is_null() {
            trace!(0, "ext2: the root cannot be renamed");
            return false;
        }
        if !unsafe { (*new_dir).is_dir() } {
            trace!(0, "ext2: the target of a rename is not a directory");
            return false;
        }
        if !self.usable_name(new_name) {
            trace!(0, "ext2: a name of {} bytes cannot be made", new_name.len());
            return false;
        }
        if !self.lookup(new_dir, new_name).is_null() {
            trace!(0, "ext2: there is something by that name already");
            return false;
        }

        let old_dir = unsafe { (*node).parent };
        let is_dir = unsafe { (*node).is_dir() };
        let moved = old_dir != new_dir;
        let ino = unsafe { (*node).ino } as u32;
        let new_dir_ino = unsafe { (*new_dir).ino } as u32;
        let old_dir_ino = unsafe { (*old_dir).ino } as u32;

        let mut old_name = [0u8; NAME_MAX];
        let old_len = {
            let name = unsafe { (*node).name() };
            old_name[..name.len()].copy_from_slice(name);
            name.len()
        };

        let mut new_dir_inode = match self.read_inode(new_dir_ino) {
            Some(inode) => inode,
            None => return false,
        };

        /* The new name first, the old one second: a crash between the two
         * leaves the file reachable under both, which e2fsck reduces to one,
         * rather than under neither */
        let file_type = if is_dir { DIR_TYPE_DIR } else { DIR_TYPE_FILE };
        if !self.add_dir_entry(new_dir, &mut new_dir_inode, ino, new_name, file_type) {
            trace!(0, "ext2: the directory entry could not be added");
            return false;
        }

        if moved {
            if !self.flush_bitmap() || !self.io.flush()
                || !self.write_inode(new_dir_ino, &new_dir_inode)
            {
                return false;
            }

            let mut old_dir_inode = match self.read_inode(old_dir_ino) {
                Some(inode) => inode,
                None => return false,
            };
            if !self.remove_dir_entry(old_dir, &mut old_dir_inode, ino, &old_name[..old_len]) {
                return false;
            }

            if is_dir {
                /* The moved directory's ".." now links the new parent */
                let mut inode = match self.read_inode(ino) {
                    Some(inode) => inode,
                    None => return false,
                };
                if !self.set_dot_dot(&mut inode, new_dir_ino) {
                    return false;
                }
                old_dir_inode.links_count = old_dir_inode.links_count.saturating_sub(1);
                if !self.link_count_adjust(new_dir_ino, 1) {
                    return false;
                }
            }

            if !self.write_inode(old_dir_ino, &old_dir_inode) || !self.commit_meta() {
                return false;
            }
        } else {
            if !self.remove_dir_entry(old_dir, &mut new_dir_inode, ino, &old_name[..old_len]) {
                return false;
            }
            if !self.flush_bitmap() || !self.io.flush()
                || !self.write_inode(new_dir_ino, &new_dir_inode) || !self.commit_meta()
            {
                return false;
            }
        }

        unsafe { vnode::rename(node, new_dir, new_name) };
        true
    }

    /// Take `node` out of its parent, release its blocks and inode, and free
    /// the vnode; a directory goes with everything under it. The name goes
    /// first, so a crash leaves at worst an orphan for e2fsck.
    fn remove_node(&mut self, node: *mut VNode, depth: u32) -> bool {
        if depth >= MAX_DIR_DEPTH {
            trace!(0, "ext2: the directory depth limit of {} was reached", MAX_DIR_DEPTH);
            return false;
        }

        let is_dir = unsafe { (*node).is_dir() };
        if is_dir {
            if !self.load_dir(node) {
                return false;
            }
            loop {
                let child = unsafe { vnode::first_child(node) };
                if child.is_null() {
                    break;
                }
                if !self.remove_node(child, depth + 1) {
                    return false;
                }
            }
        }

        let parent = unsafe { (*node).parent };
        let parent_ino = unsafe { (*parent).ino } as u32;
        let ino = unsafe { (*node).ino } as u32;

        let mut name = [0u8; NAME_MAX];
        let len = {
            let from = unsafe { (*node).name() };
            name[..from.len()].copy_from_slice(from);
            from.len()
        };

        let mut parent_inode = match self.read_inode(parent_ino) {
            Some(inode) => inode,
            None => return false,
        };
        if !self.remove_dir_entry(parent, &mut parent_inode, ino, &name[..len]) {
            return false;
        }
        if is_dir {
            parent_inode.links_count = parent_inode.links_count.saturating_sub(1);
        }
        if !self.write_inode(parent_ino, &parent_inode) {
            return false;
        }

        let mut inode = match self.read_inode(ino) {
            Some(inode) => inode,
            None => return false,
        };
        if !self.truncate_inode(ino, &mut inode, 0) {
            return false;
        }
        inode.links_count = 0;
        inode.delete_time = wall_clock_secs() as u32;
        if !self.io.flush() || !self.write_inode(ino, &inode) {
            return false;
        }
        self.free_inode(ino, is_dir);

        unsafe { vnode::unlink(node) };
        free_vnode(node);
        true
    }

    pub fn remove(&mut self, node: *mut VNode) -> bool {
        if node.is_null() {
            trace!(0, "ext2: a remove of nothing");
            return false;
        }
        if self.read_only {
            trace!(0, "ext2: read-only");
            return false;
        }
        if unsafe { (*node).parent }.is_null() {
            trace!(0, "ext2: the root cannot be removed");
            return false;
        }

        let ok = self.remove_node(node, 0);
        if !self.flush_bitmap() || !self.commit_meta() {
            return false;
        }
        ok
    }

    /// The device and the label, for `mounts`.
    fn info(&self, buf: &mut [u8]) {
        if buf.is_empty() {
            return;
        }
        buf[0] = 0;

        let mut name = [0u8; 32];
        let len = match self.io.dev.name(&mut name) {
            Some(name) => name.len(),
            None => 0,
        };
        let mut at = put(buf, 0, &name[..len]);

        let label = &self.sb.volume_name;
        let end = label.iter().position(|b| *b == 0).unwrap_or(label.len());
        if end > 0 {
            at = put(buf, at, b" label=");
            put(buf, at, &label[..end]);
        }
    }
}

/// Bytes into a C string buffer at `at`, NUL-terminated, as far as they fit.
fn put(buf: &mut [u8], at: usize, bytes: &[u8]) -> usize {
    let room = buf.len().saturating_sub(at + 1);
    let take = bytes.len().min(room);
    buf[at..at + take].copy_from_slice(&bytes[..take]);
    buf[at + take] = 0;
    at + take
}

/* ---- the ops table the VFS drives it by ---- */

/// # Safety
/// `ctx` is the pointer a mount was made with, and the filesystem is alive.
unsafe fn fs<'a>(ctx: *mut u8) -> &'a mut Ext2 {
    unsafe { &mut *(ctx as *mut Ext2) }
}

/// A NUL-terminated name the caller passed, as bytes.
///
/// # Safety
/// `name` points at a NUL-terminated string of at most NAME_MAX bytes.
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

extern "C" fn op_load_dir(ctx: *mut u8, dir: *mut VNode) -> i32 {
    if unsafe { fs(ctx) }.load_dir(dir) { 0 } else { -1 }
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

extern "C" fn op_rename(
    ctx: *mut u8, node: *mut VNode, dir: *mut VNode, name: *const u8,
) -> i32 {
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
    if !fs.mount(read_only != 0) {
        return -1;
    }
    /* The image may be one this driver can read but must not write */
    if fs.read_only { 1 } else { 0 }
}

extern "C" fn op_unmount(ctx: *mut u8) {
    unsafe { fs(ctx) }.unmount();
}

extern "C" fn op_destroy(ctx: *mut u8) {
    drop(unsafe { Box::from_raw(ctx as *mut Ext2) });
}

fn ops_for(fs: *mut Ext2) -> FsOps {
    FsOps {
        name: b"ext2\0".as_ptr(),
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

/// Does the device carry an ext2 this driver would mount? Fills `out` when
/// it does, so the caller can pick a root filesystem by label or UUID.
///
/// # Safety
/// `out` points at an Identity.
#[no_mangle]
pub unsafe extern "C" fn rust_ext2_probe(device: usize, out: *mut Identity) -> i32 {
    let dev = match Disk::from_handle(device) {
        Some(dev) => dev,
        None => return -1,
    };
    if out.is_null() {
        return -1;
    }

    if probe(&dev, unsafe { &mut *out }) { 0 } else { -1 }
}

/// Mount the device's ext2 at `path`: 0 mounted for writing, 1 mounted
/// read-only -- asked for, or all the image allows -- and -1 not mounted.
///
/// # Safety
/// `path` points at `path_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_ext2_mount(
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

    let fs = match Ext2::new(dev) {
        Some(fs) => Box::into_raw(fs),
        None => {
            trace!(0, "ext2: no memory for the filesystem");
            return -1;
        }
    };

    let ops = ops_for(fs);
    if !vfs.mount(at, &ops, read_only != 0) {
        /* Not mounted: nothing took it, and it is ours to release */
        drop(unsafe { Box::from_raw(fs) });
        return -1;
    }

    /* The VFS holds it now; this is the last look at it from here. */
    if unsafe { (*fs).read_only } { 1 } else { 0 }
}
