//! ext2, ext3 and ext4, read-only: what GRUB reads a Linux disk's kernel
//! from. Enough of the format to find a file by its path and read it --
//! extents and block maps, 64-bit descriptors, flex_bg, htree directories
//! (read as the linear ones they also are), inline data, fast and slow
//! symlinks, holes -- and a refusal, saying which, of any incompatible
//! feature it does not read. The journal is not replayed, as GRUB does not
//! replay it: a filesystem left needing recovery is read as it is on disk.
//!
//! Everything on the disk is the guest's and is checked before it is used:
//! a block outside the filesystem, a descriptor or an inode outside its
//! table, an extent tree deeper than the format allows or whose node lies
//! about its entries, a directory entry running past its block -- each an
//! error, never an index out of range.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// A filesystem's bytes: `buf.len()` of them at `offset`, both whole
/// sectors.
pub trait Source {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String>;
}

const SUPERBLOCK_AT: u64 = 1024;
const SUPERBLOCK_BYTES: usize = 1024;
const MAGIC: u16 = 0xEF53;

/* Superblock offsets (the kernel's struct ext4_super_block). */
const S_INODES_COUNT: usize = 0;
const S_BLOCKS_COUNT_LO: usize = 4;
const S_FIRST_DATA_BLOCK: usize = 20;
const S_LOG_BLOCK_SIZE: usize = 24;
const S_BLOCKS_PER_GROUP: usize = 32;
const S_INODES_PER_GROUP: usize = 40;
const S_MAGIC: usize = 56;
const S_REV_LEVEL: usize = 76;
const S_INODE_SIZE: usize = 88;
const S_FEATURE_INCOMPAT: usize = 96;
const S_UUID: usize = 104;
const S_VOLUME_NAME: usize = 120;
const S_DESC_SIZE: usize = 254;
const S_BLOCKS_COUNT_HI: usize = 336;

/* Incompatible features: those read, and the rest refused. */
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_MMP: u32 = 0x100;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_EA_INODE: u32 = 0x400;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_LARGEDIR: u32 = 0x4000;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const INCOMPAT_READ: u32 = INCOMPAT_FILETYPE | INCOMPAT_RECOVER | INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_MMP
    | INCOMPAT_FLEX_BG | INCOMPAT_EA_INODE | INCOMPAT_CSUM_SEED | INCOMPAT_LARGEDIR | INCOMPAT_INLINE_DATA;

/* A group descriptor's inode table, low and (64-bit) high halves. */
const BG_INODE_TABLE_LO: usize = 8;
const BG_INODE_TABLE_HI: usize = 0x28;
const DESC_SIZE_32: u64 = 32;
const DESC_SIZE_64: u64 = 64;

/* Inode offsets. */
const I_MODE: usize = 0;
const I_SIZE_LO: usize = 4;
const I_FLAGS: usize = 32;
const I_BLOCK: usize = 40;
const I_BLOCK_BYTES: usize = 60;
const I_SIZE_HIGH: usize = 108;
const INODE_MIN: u64 = 128;
const S_IFMT: u16 = 0xF000;
const S_IFREG: u16 = 0x8000;
const S_IFDIR: u16 = 0x4000;
const S_IFLNK: u16 = 0xA000;
const EXTENTS_FL: u32 = 0x8_0000;
const INLINE_DATA_FL: u32 = 0x1000_0000;
const ROOT_INO: u32 = 2;

/* The extent tree: a node is a header, then entries -- an index's naming a
 * child block, a leaf's a run of blocks. */
const EXT_MAGIC: u16 = 0xF30A;
const EXT_HEADER: usize = 12;
const EXT_ENTRY: usize = 12;
const EXT_MAX_DEPTH: u16 = 5;
/// A leaf's length above this is an uninitialized extent, which reads as
/// zeroes, of length less this.
const EXT_INIT_MAX_LEN: u64 = 32768;

/// Block sizes read: 1, 2 and 4 KiB -- what mkfs makes on x86.
const MAX_LOG_BLOCK_SIZE: u32 = 2;
/// Directory entry: inode, record length, name length, type, name.
const DIRENT_HEADER: usize = 8;
const MAX_SYMLINKS: u32 = 8;
const MAX_LINK_BYTES: u64 = 4096;
/// The most blocks of a directory searched.
const MAX_DIR_BLOCKS: u64 = 1 << 16;

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// An inode, as read.
#[derive(Clone, Debug)]
pub struct Inode {
    pub ino: u32,
    mode: u16,
    flags: u32,
    pub size: u64,
    block: [u8; I_BLOCK_BYTES],
}

impl Inode {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
    pub fn is_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }
    fn is_link(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }
}

/// Where a file block is: a run of `run` blocks from `block`, or a hole of
/// `run` blocks that reads as zeroes.
enum Map {
    Blocks { block: u64, run: u64 },
    Hole { run: u64 },
}

pub struct Fs<S> {
    src: S,
    block_size: u64,
    blocks: u64,
    inodes: u32,
    inodes_per_group: u32,
    groups: u64,
    inode_size: u64,
    desc_size: u64,
    /// Where the group descriptors begin, in bytes.
    descriptors: u64,
    incompat: u32,
    uuid: [u8; 16],
    label: [u8; 16],
}

impl<S: Source> Fs<S> {
    /// The filesystem on `src`, if there is one this reads.
    pub fn mount(mut src: S) -> Result<Fs<S>, String> {
        let mut sb = [0u8; SUPERBLOCK_BYTES];
        src.read(SUPERBLOCK_AT, &mut sb)?;
        if u16_at(&sb, S_MAGIC) != MAGIC {
            return Err(String::from("no ext2/3/4 superblock"));
        }
        let log = u32_at(&sb, S_LOG_BLOCK_SIZE);
        if log > MAX_LOG_BLOCK_SIZE {
            return Err(alloc::format!("blocks of {} KiB, more than this reads", 1u64 << log.min(20)));
        }
        let block_size = 1024u64 << log;
        let incompat = u32_at(&sb, S_FEATURE_INCOMPAT);
        if incompat & !INCOMPAT_READ != 0 {
            return Err(alloc::format!("incompatible features {:#x} this does not read", incompat & !INCOMPAT_READ));
        }
        let wide = incompat & INCOMPAT_64BIT != 0;
        let mut blocks = u64::from(u32_at(&sb, S_BLOCKS_COUNT_LO));
        if wide {
            blocks |= u64::from(u32_at(&sb, S_BLOCKS_COUNT_HI)) << 32;
        }
        let first_data_block = u64::from(u32_at(&sb, S_FIRST_DATA_BLOCK));
        let blocks_per_group = u64::from(u32_at(&sb, S_BLOCKS_PER_GROUP));
        let inodes = u32_at(&sb, S_INODES_COUNT);
        let inodes_per_group = u32_at(&sb, S_INODES_PER_GROUP);
        if blocks <= first_data_block || blocks_per_group == 0 || inodes == 0 || inodes_per_group == 0 {
            return Err(String::from("a superblock with no blocks or no inodes"));
        }
        let groups = (blocks - first_data_block).div_ceil(blocks_per_group);
        if groups.checked_mul(u64::from(inodes_per_group)).is_none_or(|n| n < u64::from(inodes)) {
            return Err(String::from("a superblock whose groups hold fewer inodes than it has"));
        }
        let inode_size = if u32_at(&sb, S_REV_LEVEL) == 0 { INODE_MIN } else { u64::from(u16_at(&sb, S_INODE_SIZE)) };
        if inode_size < INODE_MIN || !inode_size.is_power_of_two() || inode_size > block_size {
            return Err(alloc::format!("inodes of {} bytes", inode_size));
        }
        let desc_size = if wide { u64::from(u16_at(&sb, S_DESC_SIZE)) } else { DESC_SIZE_32 };
        if desc_size < DESC_SIZE_32 || !desc_size.is_power_of_two() || desc_size > block_size {
            return Err(alloc::format!("group descriptors of {} bytes", desc_size));
        }
        let descriptors = (first_data_block + 1) * block_size;
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&sb[S_UUID..S_UUID + 16]);
        let mut label = [0u8; 16];
        label.copy_from_slice(&sb[S_VOLUME_NAME..S_VOLUME_NAME + 16]);
        Ok(Fs { src, block_size, blocks, inodes, inodes_per_group, groups, inode_size, desc_size, descriptors,
                incompat, uuid, label })
    }

    /// Its UUID as GRUB's `search --fs-uuid` writes one: 8-4-4-4-12 lowercase
    /// hex.
    pub fn uuid(&self) -> String {
        let mut s = String::new();
        for (i, b) in self.uuid.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                s.push('-');
            }
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{:02x}", b));
        }
        s
    }

    /// Its label, up to the first NUL.
    pub fn label(&self) -> String {
        let end = self.label.iter().position(|&b| b == 0).unwrap_or(self.label.len());
        String::from_utf8_lossy(&self.label[..end]).into_owned()
    }

    /// Left needing its journal replayed: read as it is on disk regardless.
    pub fn needs_recovery(&self) -> bool {
        self.incompat & INCOMPAT_RECOVER != 0
    }

    /// Read whole block `block` into `buf`, a block long.
    fn read_block(&mut self, block: u64, buf: &mut [u8]) -> Result<(), String> {
        if block >= self.blocks {
            return Err(alloc::format!("block {} of a filesystem of {}", block, self.blocks));
        }
        self.src.read(block * self.block_size, buf)
    }

    /// Inode `ino`.
    pub fn inode(&mut self, ino: u32) -> Result<Inode, String> {
        if ino == 0 || ino > self.inodes {
            return Err(alloc::format!("inode {} of {}", ino, self.inodes));
        }
        let group = u64::from((ino - 1) / self.inodes_per_group);
        let index = u64::from((ino - 1) % self.inodes_per_group);
        if group >= self.groups {
            return Err(alloc::format!("inode {} in group {} of {}", ino, group, self.groups));
        }
        let bs = self.block_size;
        let mut buf = vec![0u8; bs as usize];

        let at = self.descriptors + group * self.desc_size;
        self.read_block(at / bs, &mut buf)?;
        let d = (at % bs) as usize;
        let mut table = u64::from(u32_at(&buf, d + BG_INODE_TABLE_LO));
        if self.desc_size >= DESC_SIZE_64 {
            table |= u64::from(u32_at(&buf, d + BG_INODE_TABLE_HI)) << 32;
        }
        let at = table
            .checked_mul(bs)
            .and_then(|t| t.checked_add(index * self.inode_size))
            .ok_or_else(|| alloc::format!("group {}'s inode table at block {}", group, table))?;
        self.read_block(at / bs, &mut buf)?;
        let i = (at % bs) as usize;
        let raw = &buf[i..i + self.inode_size as usize];
        let mut block = [0u8; I_BLOCK_BYTES];
        block.copy_from_slice(&raw[I_BLOCK..I_BLOCK + I_BLOCK_BYTES]);
        let mut size = u64::from(u32_at(raw, I_SIZE_LO));
        let mode = u16_at(raw, I_MODE);
        if mode & S_IFMT == S_IFREG {
            size |= u64::from(u32_at(raw, I_SIZE_HIGH)) << 32;
        }
        Ok(Inode { ino, mode, flags: u32_at(raw, I_FLAGS), size, block })
    }

    /// Where the file's block `lblock` is.
    fn map(&mut self, inode: &Inode, lblock: u64) -> Result<Map, String> {
        if inode.flags & EXTENTS_FL != 0 {
            self.map_extent(inode, lblock)
        } else {
            self.map_indirect(inode, lblock)
        }
    }

    fn map_extent(&mut self, inode: &Inode, lblock: u64) -> Result<Map, String> {
        let mut node: Vec<u8> = inode.block.to_vec();
        /* The blocks this subtree covers end where the next index entry
         * up begins: no run reaches past it. */
        let mut limit = u64::MAX;
        let mut depth_wanted: Option<u16> = None;
        loop {
            if node.len() < EXT_HEADER || u16_at(&node, 0) != EXT_MAGIC {
                return Err(alloc::format!("inode {}: an extent node with no header", inode.ino));
            }
            let entries = usize::from(u16_at(&node, 2));
            let max = usize::from(u16_at(&node, 4));
            let depth = u16_at(&node, 6);
            if entries > max || EXT_HEADER + max * EXT_ENTRY > node.len() || depth > EXT_MAX_DEPTH
                || depth_wanted.is_some_and(|d| d != depth)
            {
                return Err(alloc::format!("inode {}: an extent node of {} entries in {}, depth {}", inode.ino,
                                          entries, max, depth));
            }
            let entry = |i: usize| &node[EXT_HEADER + i * EXT_ENTRY..EXT_HEADER + (i + 1) * EXT_ENTRY];
            if depth == 0 {
                for i in 0..entries {
                    let e = entry(i);
                    let first = u64::from(u32_at(e, 0));
                    let mut len = u64::from(u16_at(e, 4));
                    let start = (u64::from(u16_at(e, 6)) << 32) | u64::from(u32_at(e, 8));
                    let unwritten = len > EXT_INIT_MAX_LEN;
                    if unwritten {
                        len -= EXT_INIT_MAX_LEN;
                    }
                    if lblock < first {
                        return Ok(Map::Hole { run: (first - lblock).min(limit.saturating_sub(lblock)) });
                    }
                    if lblock - first < len {
                        let run = (len - (lblock - first)).min(limit.saturating_sub(lblock));
                        if unwritten {
                            return Ok(Map::Hole { run });
                        }
                        let block = start + (lblock - first);
                        if block.checked_add(run).is_none_or(|end| end > self.blocks) {
                            return Err(alloc::format!("inode {}: an extent at block {} past the filesystem",
                                                      inode.ino, start));
                        }
                        return Ok(Map::Blocks { block, run });
                    }
                }
                return Ok(Map::Hole { run: limit.saturating_sub(lblock).max(1) });
            }
            /* The last index entry starting at or before the block. */
            let mut child = None;
            for i in 0..entries {
                let e = entry(i);
                let first = u64::from(u32_at(e, 0));
                if first > lblock {
                    limit = limit.min(first);
                    break;
                }
                child = Some((u64::from(u16_at(e, 8)) << 32) | u64::from(u32_at(e, 4)));
            }
            let Some(child) = child else {
                return Ok(Map::Hole { run: limit.saturating_sub(lblock).max(1) });
            };
            let mut next = vec![0u8; self.block_size as usize];
            self.read_block(child, &mut next)?;
            node = next;
            depth_wanted = Some(depth - 1);
        }
    }

    /// The block map of ext2 and ext3: twelve direct pointers, then a
    /// single, a double and a triple indirect block. A run is as long as
    /// consecutive pointers in one array point at consecutive blocks.
    fn map_indirect(&mut self, inode: &Inode, lblock: u64) -> Result<Map, String> {
        let per = self.block_size / 4;
        let direct = |i: usize| u64::from(u32_at(&inode.block, i * 4));
        if lblock < 12 {
            let i = lblock as usize;
            return Ok(run_of((i..12).map(direct)));
        }
        /* Which of the single, double and triple indirect trees the block is
         * in, and where in it: `cover` blocks the tree maps, `rest` the
         * block's place among them. */
        let mut rest = lblock - 12;
        let mut level = 1usize;
        let mut cover = per;
        while rest >= cover {
            rest -= cover;
            level += 1;
            if level > 3 {
                return Err(alloc::format!("inode {}: block {} past a block map's reach", inode.ino, lblock));
            }
            cover = cover.saturating_mul(per);
        }
        let mut ptr = direct(11 + level);
        let mut buf = vec![0u8; self.block_size as usize];
        loop {
            if ptr == 0 {
                /* A hole as long as what this pointer would have mapped,
                 * from the block on -- and not a block past it. */
                return Ok(Map::Hole { run: cover - rest });
            }
            self.read_block(ptr, &mut buf)?;
            let child = cover / per;
            let i = (rest / child) as usize;
            if child == 1 {
                return Ok(run_of((i..per as usize).map(|j| u64::from(u32_at(&buf, j * 4)))));
            }
            ptr = u64::from(u32_at(&buf, i * 4));
            rest %= child;
            cover = child;
        }
    }

    /// Read the file's bytes `[offset, offset + dest.len())` into `dest`.
    pub fn read(&mut self, inode: &Inode, offset: u64, dest: &mut [u8]) -> Result<(), String> {
        if offset.checked_add(dest.len() as u64).is_none_or(|end| end > inode.size) {
            return Err(alloc::format!("inode {}: {} bytes at {} of {}", inode.ino, dest.len(), offset, inode.size));
        }
        if inode.flags & INLINE_DATA_FL != 0 {
            if inode.size > I_BLOCK_BYTES as u64 {
                return Err(alloc::format!("inode {}: inline data past its inode, which this does not read",
                                          inode.ino));
            }
            let at = offset as usize;
            dest.copy_from_slice(&inode.block[at..at + dest.len()]);
            return Ok(());
        }
        let bs = self.block_size;
        let mut bounce: Vec<u8> = Vec::new();
        let mut done = 0usize;
        while done < dest.len() {
            let pos = offset + done as u64;
            let (lblock, within) = (pos / bs, (pos % bs) as usize);
            let left = dest.len() - done;
            match self.map(inode, lblock)? {
                Map::Hole { run } => {
                    let n = (run.saturating_mul(bs) - within as u64).min(left as u64) as usize;
                    dest[done..done + n].fill(0);
                    done += n;
                }
                Map::Blocks { block, run } => {
                    if block.checked_add(run).is_none_or(|end| end > self.blocks) {
                        return Err(alloc::format!("inode {}: blocks {}..+{} past the filesystem's {}", inode.ino,
                                                  block, run, self.blocks));
                    }
                    if within == 0 && left as u64 >= bs {
                        /* Whole blocks straight into place, as many as the
                         * run and the file have. */
                        let n = (run.min(left as u64 / bs) * bs) as usize;
                        self.src.read(block * bs, &mut dest[done..done + n])?;
                        done += n;
                    } else {
                        if bounce.is_empty() {
                            bounce = vec![0u8; bs as usize];
                        }
                        self.read_block(block, &mut bounce)?;
                        let n = (bs as usize - within).min(left);
                        dest[done..done + n].copy_from_slice(&bounce[within..within + n]);
                        done += n;
                    }
                }
            }
        }
        Ok(())
    }

    /// The whole of a file of at most `max` bytes.
    pub fn read_all(&mut self, inode: &Inode, max: u64) -> Result<Vec<u8>, String> {
        if inode.size > max {
            return Err(alloc::format!("inode {}: {} bytes, more than {}", inode.ino, inode.size, max));
        }
        let mut v = vec![0u8; inode.size as usize];
        self.read(inode, 0, &mut v)?;
        Ok(v)
    }

    /// The inode named `name` in directory `dir`.
    fn find(&mut self, dir: &Inode, name: &[u8]) -> Result<Option<u32>, String> {
        if dir.flags & INLINE_DATA_FL != 0 {
            /* An inline directory: its parent's inode first, then entries. */
            if name == b".." {
                return Ok(Some(u32_at(&dir.block, 0)));
            }
            let end = (dir.size as usize).min(I_BLOCK_BYTES);
            return find_entry(&dir.block[4.min(end)..end], name, dir.ino);
        }
        let bs = self.block_size;
        let blocks = dir.size.div_ceil(bs).min(MAX_DIR_BLOCKS);
        let mut buf = vec![0u8; bs as usize];
        let mut lblock = 0;
        while lblock < blocks {
            match self.map(dir, lblock)? {
                Map::Hole { run } => lblock = lblock.saturating_add(run),
                Map::Blocks { block, run } => {
                    for k in 0..run.min(blocks - lblock) {
                        self.read_block(block + k, &mut buf)?;
                        if let Some(ino) = find_entry(&buf, name, dir.ino)? {
                            return Ok(Some(ino));
                        }
                    }
                    lblock += run;
                }
            }
        }
        Ok(None)
    }

    /// A symlink's target.
    fn link_target(&mut self, link: &Inode) -> Result<Vec<u8>, String> {
        if link.size > MAX_LINK_BYTES {
            return Err(alloc::format!("inode {}: a symlink of {} bytes", link.ino, link.size));
        }
        if link.flags & (EXTENTS_FL | INLINE_DATA_FL) == 0 && link.size < I_BLOCK_BYTES as u64 {
            return Ok(link.block[..link.size as usize].to_vec());
        }
        self.read_all(link, MAX_LINK_BYTES)
    }

    /// The inode at `path`, from the root, symlinks followed.
    pub fn lookup(&mut self, path: &str) -> Result<Inode, String> {
        let root = self.inode(ROOT_INO)?;
        let mut dir = root.clone();
        /* The components still to walk, the next last. */
        let mut todo: Vec<Vec<u8>> = path.split('/').rev().map(|c| c.as_bytes().to_vec()).collect();
        let mut links = 0u32;
        let mut current = dir.clone();
        while let Some(name) = todo.pop() {
            if name.is_empty() || name == b"." {
                continue;
            }
            if !current.is_dir() {
                return Err(alloc::format!("{}: a component is not a directory", path));
            }
            dir = current.clone();
            let Some(ino) = self.find(&dir, &name)? else {
                return Err(alloc::format!("{}: no such file", path));
            };
            let node = self.inode(ino)?;
            if node.is_link() {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(alloc::format!("{}: more than {} symlinks", path, MAX_SYMLINKS));
                }
                let target = self.link_target(&node)?;
                for c in target.split(|&b| b == b'/').rev() {
                    todo.push(c.to_vec());
                }
                current = if target.first() == Some(&b'/') { root.clone() } else { dir.clone() };
                continue;
            }
            current = node;
        }
        Ok(current)
    }
}

/// A run of blocks from the pointers of a block map, the first being the
/// block asked for: how many are consecutive, or how many holes.
fn run_of(mut ptrs: impl Iterator<Item = u64>) -> Map {
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

/// The inode named `name` among the entries of one directory block.
fn find_entry(block: &[u8], name: &[u8], dir: u32) -> Result<Option<u32>, String> {
    let mut at = 0usize;
    while at + DIRENT_HEADER <= block.len() {
        let ino = u32_at(block, at);
        let rec_len = usize::from(u16_at(block, at + 4));
        let name_len = usize::from(block[at + 6]);
        if rec_len < DIRENT_HEADER || rec_len % 4 != 0 || at + rec_len > block.len()
            || DIRENT_HEADER + name_len > rec_len
        {
            return Err(alloc::format!("directory {}: an entry at {} of {} bytes, its name {}", dir, at, rec_len,
                                      name_len));
        }
        if ino != 0 && &block[at + DIRENT_HEADER..at + DIRENT_HEADER + name_len] == name {
            return Ok(Some(ino));
        }
        at += rec_len;
    }
    Ok(None)
}
