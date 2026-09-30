//! ext2 images, as the format has them: made (`mkfs` -- what mke2fs lays
//! out, and a tree put on it) and judged (`check` -- what e2fsck -fn says
//! of one). The fuzzer's own, written from the format and not from the
//! kernel's driver, so that what the driver writes is held to the format
//! rather than to itself.
//!
//! `check` sorts what it finds two ways. Corruption is what no crash of a
//! correct driver may leave: a block in use and free in the bitmap, a
//! block two files hold, a name leading to an inode that is not in use, a
//! directory block that does not parse, a pointer out of the filesystem.
//! An unclean filesystem is one e2fsck still has to fix -- a block or an
//! inode leaked, a count wrong, a link count off -- which a power cut may
//! leave (the driver's commit order promises no more), and a clean unmount
//! must not.

use std::collections::{BTreeMap, BTreeSet};

use crate::machine::disk::Media;
use crate::model::{Data, Node};

pub const MAGIC: u16 = 0xEF53;
pub const SB_OFFSET: u64 = 1024;
pub const ROOT_INO: u32 = 2;
const LOST_FOUND_INO: u32 = 11;
pub const FIRST_INO: u32 = 11;
pub const GD_SIZE: usize = 32;

pub const INCOMPAT_FILETYPE: u32 = 0x0002;
pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
pub const RO_COMPAT_LARGE_FILE: u32 = 0x0002;
pub const COMPAT_DIR_INDEX: u32 = 0x0020;
/// State: cleanly unmounted
pub const STATE_VALID: u16 = 0x0001;

pub const S_IFMT: u16 = 0xF000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFREG: u16 = 0x8000;
pub const FT_REG: u8 = 1;
pub const FT_DIR: u8 = 2;

/* Where the fields are: the superblock's, an inode's, a descriptor's. */
pub mod sb {
    pub const INODES_COUNT: usize = 0;
    pub const BLOCKS_COUNT: usize = 4;
    pub const R_BLOCKS_COUNT: usize = 8;
    pub const FREE_BLOCKS: usize = 12;
    pub const FREE_INODES: usize = 16;
    pub const FIRST_DATA_BLOCK: usize = 20;
    pub const LOG_BLOCK_SIZE: usize = 24;
    pub const LOG_FRAG_SIZE: usize = 28;
    pub const BLOCKS_PER_GROUP: usize = 32;
    pub const FRAGS_PER_GROUP: usize = 36;
    pub const INODES_PER_GROUP: usize = 40;
    pub const MTIME: usize = 44;
    pub const WTIME: usize = 48;
    pub const MNT_COUNT: usize = 52;
    pub const MAX_MNT_COUNT: usize = 54;
    pub const MAGIC: usize = 56;
    pub const STATE: usize = 58;
    pub const ERRORS: usize = 60;
    pub const LASTCHECK: usize = 64;
    pub const REV_LEVEL: usize = 76;
    pub const FIRST_INO: usize = 84;
    pub const INODE_SIZE: usize = 88;
    pub const BLOCK_GROUP_NR: usize = 90;
    pub const FEATURE_COMPAT: usize = 92;
    pub const FEATURE_INCOMPAT: usize = 96;
    pub const FEATURE_RO_COMPAT: usize = 100;
    pub const UUID: usize = 104;
    pub const VOLUME_NAME: usize = 120;
    pub const HASH_SEED: usize = 236;
    pub const DEF_HASH_VERSION: usize = 252;
    pub const MKFS_TIME: usize = 264;
}

pub mod ino {
    pub const MODE: usize = 0;
    pub const SIZE: usize = 4;
    pub const ATIME: usize = 8;
    pub const CTIME: usize = 12;
    pub const MTIME: usize = 16;
    pub const DTIME: usize = 20;
    pub const LINKS: usize = 26;
    pub const BLOCKS: usize = 28;
    pub const FLAGS: usize = 32;
    pub const BLOCK: usize = 40;
    pub const DIR_ACL: usize = 108;
}

pub mod gd {
    pub const BLOCK_BITMAP: usize = 0;
    pub const INODE_BITMAP: usize = 4;
    pub const INODE_TABLE: usize = 8;
    pub const FREE_BLOCKS: usize = 12;
    pub const FREE_INODES: usize = 14;
    pub const USED_DIRS: usize = 16;
}

pub fn rd16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

pub fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub fn wr16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn wr32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn bit(map: &[u8], i: usize) -> bool {
    map[i / 8] & (1 << (i % 8)) != 0
}

fn set_bit(map: &mut [u8], i: usize) {
    map[i / 8] |= 1 << (i % 8);
}

/* ---- the geometry ---- */

#[derive(Clone, Debug)]
pub struct Geometry {
    /// A block is 1024 << this.
    pub log_block_size: u32,
    pub blocks: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u16,
    pub sparse_super: bool,
    pub large_file: bool,
    pub dir_index: bool,
    /// A read-only-compatible feature the driver does not keep up, which
    /// has it mount the image read-only.
    pub ro_compat_extra: u32,
    pub reserved_blocks: u32,
    pub label: [u8; 16],
    pub uuid: [u8; 16],
}

/// Where a group's own blocks are.
#[derive(Clone, Copy, Debug)]
pub struct GroupLayout {
    pub start: u32,
    pub len: u32,
    /// A copy of the superblock and the descriptors at its start
    pub has_super: bool,
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    /// The first block past the group's metadata
    pub data: u32,
}

fn power_of(mut n: u32, base: u32) -> bool {
    while n > 1 && n % base == 0 {
        n /= base;
    }
    n == 1
}

impl Geometry {
    pub fn block_size(&self) -> u32 {
        1024 << self.log_block_size
    }

    pub fn first_data_block(&self) -> u32 {
        (self.log_block_size == 0) as u32
    }

    pub fn groups(&self) -> u32 {
        (self.blocks - self.first_data_block()).div_ceil(self.blocks_per_group)
    }

    pub fn gdt_blocks(&self) -> u32 {
        (self.groups() * GD_SIZE as u32).div_ceil(self.block_size())
    }

    pub fn itable_blocks(&self) -> u32 {
        self.inodes_per_group * self.inode_size as u32 / self.block_size()
    }

    pub fn has_super(&self, g: u32) -> bool {
        !self.sparse_super || g <= 1 || power_of(g, 3) || power_of(g, 5) || power_of(g, 7)
    }

    pub fn layout(&self, g: u32) -> GroupLayout {
        let start = self.first_data_block() + g * self.blocks_per_group;
        let len = self.blocks_per_group.min(self.blocks - start);
        let has_super = self.has_super(g);
        let block_bitmap = start + if has_super { 1 + self.gdt_blocks() } else { 0 };
        GroupLayout {
            start,
            len,
            has_super,
            block_bitmap,
            inode_bitmap: block_bitmap + 1,
            inode_table: block_bitmap + 2,
            data: block_bitmap + 2 + self.itable_blocks(),
        }
    }

    pub fn inodes(&self) -> u32 {
        self.inodes_per_group * self.groups()
    }

    /// Whether this is a geometry mke2fs would make, and `mkfs` lays out:
    /// every group holding its metadata and a block to spare.
    pub fn valid(&self) -> bool {
        let bs = self.block_size();
        let isz = self.inode_size as u32;
        let per_block = bs / isz.max(1);
        if self.log_block_size > 2
            || !self.inode_size.is_power_of_two() || isz < 128 || isz > bs
            || self.blocks_per_group == 0 || self.blocks_per_group % 8 != 0 || self.blocks_per_group > 8 * bs
            || self.inodes_per_group == 0 || self.inodes_per_group % 8 != 0 || self.inodes_per_group > 8 * bs
            || self.inodes_per_group % per_block != 0
            || self.blocks <= self.first_data_block() + 1
        {
            return false;
        }
        if (self.inodes() as u64) < FIRST_INO as u64 + 1 {
            return false;
        }
        (0..self.groups()).all(|g| {
            let l = self.layout(g);
            l.data < l.start + l.len
        })
    }

    pub fn features(&self) -> (u32, u32, u32) {
        let compat = if self.dir_index { COMPAT_DIR_INDEX } else { 0 };
        let mut ro = self.ro_compat_extra;
        if self.sparse_super {
            ro |= RO_COMPAT_SPARSE_SUPER;
        }
        if self.large_file {
            ro |= RO_COMPAT_LARGE_FILE;
        }
        (compat, INCOMPAT_FILETYPE, ro)
    }
}

/* ---- mkfs ---- */

/// How the tree is laid down: all of it valid ext2, in the ways the format
/// allows a writer to differ.
#[derive(Clone, Debug, Default)]
pub struct Style {
    /// A block of a file that is all zeros is left a hole.
    pub holes: bool,
    /// Directories get deleted entries and slack between their live ones,
    /// and a spare empty block at their end, as a directory has that files
    /// were removed from.
    pub slack: bool,
    /// Blocks are taken from anywhere in the filesystem rather than from
    /// the start, so that a file is in more than one group.
    pub scatter: u64,
}

/// What `mkfs` made: the image, and which inode each path became.
pub struct Made {
    pub media: Media,
    pub inodes: BTreeMap<Vec<Vec<u8>>, u32>,
}

struct Builder<'a> {
    g: &'a Geometry,
    bs: usize,
    img: Media,
    /// Per block: in use.
    used: Vec<bool>,
    /// Per inode (index 0 is inode 1): in use.
    ino_used: Vec<bool>,
    /// The inodes' content, by number, written out at the end.
    table: BTreeMap<u32, Vec<u8>>,
    dirs: Vec<u32>,
    now: u32,
    style: Style,
    chaos: u64,
}

impl Builder<'_> {
    fn rand(&mut self) -> u64 {
        let mut x = self.chaos;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.chaos = x;
        x
    }

    fn alloc_block(&mut self) -> Result<u32, String> {
        let first = self.g.first_data_block() as usize;
        let n = self.used.len();
        let start = if self.style.scatter != 0 && self.rand() % self.style.scatter == 0 {
            first + (self.rand() as usize % (n - first))
        } else {
            first
        };
        for k in 0..n - first {
            let b = first + (start - first + k) % (n - first);
            if !self.used[b] {
                self.used[b] = true;
                return Ok(b as u32);
            }
        }
        Err("no free block".into())
    }

    fn alloc_inode(&mut self, dir: bool) -> Result<u32, String> {
        for i in FIRST_INO as usize - 1..self.ino_used.len() {
            if !self.ino_used[i] {
                self.ino_used[i] = true;
                let ino = i as u32 + 1;
                if dir {
                    self.dirs.push(ino);
                }
                return Ok(ino);
            }
        }
        Err("no free inode".into())
    }

    fn write_block(&mut self, b: u32, data: &[u8]) {
        self.img.write(b as u64 * self.bs as u64, data);
    }

    fn new_inode(&self, mode: u16, links: u16) -> Vec<u8> {
        let mut i = vec![0u8; self.g.inode_size as usize];
        wr16(&mut i, ino::MODE, mode);
        wr16(&mut i, ino::LINKS, links);
        wr32(&mut i, ino::ATIME, self.now);
        wr32(&mut i, ino::CTIME, self.now);
        wr32(&mut i, ino::MTIME, self.now);
        i
    }

    /// Lays `blocks` (logical index, content) under inode `i`: data blocks,
    /// and the indirect blocks that lead to them. The count of blocks it
    /// took, indirect ones included.
    fn map(&mut self, i: &mut [u8], blocks: &[(u32, Vec<u8>)]) -> Result<u32, String> {
        let ppb = (self.bs / 4) as u32;
        let mut taken = 0;
        let mut ind: BTreeMap<u32, Vec<u8>> = BTreeMap::new(); // single indirect, by which one
        let mut ind_no: BTreeMap<u32, u32> = BTreeMap::new();
        let mut dind: Option<(u32, Vec<u8>)> = None;
        for (logical, content) in blocks {
            let b = self.alloc_block()?;
            taken += 1;
            self.write_block(b, content);
            let logical = *logical;
            if logical < 12 {
                wr32(i, ino::BLOCK + 4 * logical as usize, b);
                continue;
            }
            let rel = logical - 12;
            let (which, slot) = if rel < ppb {
                (0, rel)
            } else {
                let rel = rel - ppb;
                if rel >= ppb * ppb {
                    return Err("a file past the double indirect block".into());
                }
                (1 + rel / ppb, rel % ppb)
            };
            if !ind.contains_key(&which) {
                let nb = self.alloc_block()?;
                taken += 1;
                ind.insert(which, vec![0u8; self.bs]);
                ind_no.insert(which, nb);
                if which == 0 {
                    wr32(i, ino::BLOCK + 4 * 12, nb);
                } else {
                    if dind.is_none() {
                        let db = self.alloc_block()?;
                        taken += 1;
                        dind = Some((db, vec![0u8; self.bs]));
                        wr32(i, ino::BLOCK + 4 * 13, db);
                    }
                    let d = dind.as_mut().expect("made above");
                    wr32(&mut d.1, 4 * (which - 1) as usize, nb);
                }
            }
            wr32(ind.get_mut(&which).expect("made above"), 4 * slot as usize, b);
        }
        for (which, content) in ind {
            let b = ind_no[&which];
            self.write_block(b, &content);
        }
        if let Some((b, content)) = dind {
            self.write_block(b, &content);
        }
        Ok(taken)
    }

    fn file(&mut self, content: &Data) -> Result<u32, String> {
        let ino_no = self.alloc_inode(false)?;
        let mut i = self.new_inode(S_IFREG | 0o644, 1);
        if content.len() > u32::MAX as u64 {
            return Err("a file past what ext2 without large files holds".into());
        }
        wr32(&mut i, ino::SIZE, content.len() as u32);
        let bs = self.bs as u64;
        let logical: Vec<u64> = if self.style.holes {
            content.nonzero_blocks(bs)
        } else {
            let n = content.len().div_ceil(bs);
            if n > self.used.len() as u64 {
                return Err("a file bigger than the filesystem".into());
            }
            (0..n).collect()
        };
        let mut blocks = Vec::new();
        for k in logical {
            let mut block = content.read(k * bs, bs);
            block.resize(self.bs, 0);
            blocks.push((u32::try_from(k).map_err(|_| "a block past what ext2 numbers".to_string())?, block));
        }
        let taken = self.map(&mut i, &blocks)?;
        wr32(&mut i, ino::BLOCKS, taken * (self.bs as u32 / 512));
        self.table.insert(ino_no, i);
        Ok(ino_no)
    }

    /// The blocks of a directory holding `entries` (inode, name, type): "."
    /// and ".." first, a record's length to the end of its block where the
    /// next does not fit.
    fn dir_blocks(&mut self, entries: &[(u32, Vec<u8>, u8)]) -> Vec<Vec<u8>> {
        let bs = self.bs;
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        let mut cur = vec![0u8; bs];
        let mut pos = 0usize;
        let mut last: Option<usize> = None;
        for (k, (ino_no, name, ft)) in entries.iter().enumerate() {
            /* A deleted entry before this one, now and then: a gap. */
            let gap = if self.style.slack && k >= 2 && self.rand() % 4 == 0 { 12 + 4 * (self.rand() % 8) as usize } else { 0 };
            let need = (8 + name.len()).div_ceil(4) * 4;
            if pos + gap + need > bs {
                if let Some(l) = last {
                    wr16(&mut cur, l + 4, (bs - l) as u16);
                }
                blocks.push(std::mem::replace(&mut cur, vec![0u8; bs]));
                pos = 0;
            }
            if gap != 0 && pos + gap + need <= bs {
                /* A record of a removed file: inode 0, its length the gap. */
                wr32(&mut cur, pos, 0);
                wr16(&mut cur, pos + 4, gap as u16);
                pos += gap;
            }
            wr32(&mut cur, pos, *ino_no);
            wr16(&mut cur, pos + 4, need as u16);
            cur[pos + 6] = name.len() as u8;
            cur[pos + 7] = *ft;
            cur[pos + 8..pos + 8 + name.len()].copy_from_slice(name);
            last = Some(pos);
            pos += need;
        }
        if let Some(l) = last {
            wr16(&mut cur, l + 4, (bs - l) as u16);
        }
        blocks.push(cur);
        if self.style.slack && self.rand() % 3 == 0 {
            /* An empty block: one record of nothing, the whole block long. */
            let mut empty = vec![0u8; bs];
            wr16(&mut empty, 4, bs as u16);
            blocks.push(empty);
        }
        blocks
    }

    fn dir(&mut self, ino_no: u32, parent: u32, children: &[(u32, Vec<u8>, u8)], subdirs: u16) -> Result<(), String> {
        let mut entries = vec![(ino_no, b".".to_vec(), FT_DIR), (parent, b"..".to_vec(), FT_DIR)];
        entries.extend_from_slice(children);
        let blocks = self.dir_blocks(&entries);
        let mut i = self.new_inode(S_IFDIR | 0o755, 2 + subdirs);
        wr32(&mut i, ino::SIZE, (blocks.len() * self.bs) as u32);
        let list: Vec<(u32, Vec<u8>)> = blocks.into_iter().enumerate().map(|(k, b)| (k as u32, b)).collect();
        let taken = self.map(&mut i, &list)?;
        wr32(&mut i, ino::BLOCKS, taken * (self.bs as u32 / 512));
        self.table.insert(ino_no, i);
        Ok(())
    }

    /// The tree under `node`, as directory `ino_no`'s children.
    fn populate(&mut self, node: &Node, ino_no: u32, parent: u32, at: &mut Vec<Vec<u8>>,
                inodes: &mut BTreeMap<Vec<Vec<u8>>, u32>, extra: &[(u32, Vec<u8>, u8)]) -> Result<(), String> {
        let mut children: Vec<(u32, Vec<u8>, u8)> = extra.to_vec();
        let mut subdirs = extra.iter().filter(|e| e.2 == FT_DIR).count() as u16;
        let mut sub: Vec<(u32, &Node, Vec<u8>)> = Vec::new();
        for (name, child) in node.children().expect("a directory") {
            at.push(name.clone());
            match child {
                Node::File(content) => {
                    let n = self.file(content)?;
                    inodes.insert(at.clone(), n);
                    children.push((n, name.clone(), FT_REG));
                }
                Node::Dir(_) => {
                    let n = self.alloc_inode(true)?;
                    inodes.insert(at.clone(), n);
                    children.push((n, name.clone(), FT_DIR));
                    sub.push((n, child, name.clone()));
                    subdirs += 1;
                }
            }
            at.pop();
        }
        self.dir(ino_no, parent, &children, subdirs)?;
        for (n, child, name) in sub {
            at.push(name);
            self.populate(child, n, ino_no, at, inodes, &[])?;
            at.pop();
        }
        Ok(())
    }
}

/// An ext2 image of geometry `g` holding `tree` -- its root the root
/// directory, lost+found beside what the tree has -- as mke2fs and a copy
/// in would leave it: a filesystem e2fsck -fn passes.
pub fn mkfs(g: &Geometry, tree: &Node, style: &Style, now: u32, chaos: u64) -> Result<Made, String> {
    if !g.valid() {
        return Err(format!("not a geometry mkfs makes: {:?}", g));
    }
    if tree.children().is_some_and(|c| c.contains_key(&b"lost+found".to_vec())) {
        return Err("the tree has a lost+found of its own".into());
    }
    let bs = g.block_size() as usize;
    let mut b = Builder {
        g,
        bs,
        img: Media::new(),
        used: vec![false; g.blocks as usize],
        ino_used: vec![false; g.inodes() as usize],
        table: BTreeMap::new(),
        dirs: Vec::new(),
        now,
        style: style.clone(),
        chaos: chaos | 1,
    };
    /* Block 0 before the first group, when there is one, is nobody's. */
    for i in 0..g.first_data_block() as usize {
        b.used[i] = true;
    }
    for grp in 0..g.groups() {
        let l = g.layout(grp);
        for blk in l.start..l.data {
            b.used[blk as usize] = true;
        }
    }
    /* The reserved inodes, 1 to 10, and the two every ext2 has. */
    for i in 0..FIRST_INO as usize - 1 {
        b.ino_used[i] = true;
    }
    b.dirs.push(ROOT_INO);
    let lf = b.alloc_inode(true)?;
    debug_assert_eq!(lf, LOST_FOUND_INO);
    let mut inodes = BTreeMap::new();
    b.dir(lf, ROOT_INO, &[], 0)?;
    b.populate(tree, ROOT_INO, ROOT_INO, &mut Vec::new(), &mut inodes, &[(lf, b"lost+found".to_vec(), FT_DIR)])?;

    /* The inode tables. */
    let isz = g.inode_size as usize;
    let table = std::mem::take(&mut b.table);
    for (n, content) in &table {
        let idx = n - 1;
        let l = g.layout(idx / g.inodes_per_group);
        let at = l.inode_table as u64 * bs as u64 + ((idx % g.inodes_per_group) as usize * isz) as u64;
        b.img.write(at, content);
    }

    /* The bitmaps, the descriptors, and the superblock with its copies. */
    let mut gdt = vec![0u8; g.gdt_blocks() as usize * bs];
    let (mut free_blocks, mut free_inodes) = (0u32, 0u32);
    for grp in 0..g.groups() {
        let l = g.layout(grp);
        let mut bmap = vec![0u8; bs];
        let mut free = 0;
        for k in 0..l.len {
            if b.used[(l.start + k) as usize] {
                set_bit(&mut bmap, k as usize);
            } else {
                free += 1;
            }
        }
        /* Past the group's end, set: the padding e2fsck looks for. */
        for k in l.len as usize..bs * 8 {
            set_bit(&mut bmap, k);
        }
        let mut imap = vec![0u8; bs];
        let mut ifree = 0;
        for k in 0..g.inodes_per_group {
            if b.ino_used[(grp * g.inodes_per_group + k) as usize] {
                set_bit(&mut imap, k as usize);
            } else {
                ifree += 1;
            }
        }
        for k in g.inodes_per_group as usize..bs * 8 {
            set_bit(&mut imap, k);
        }
        b.write_block(l.block_bitmap, &bmap);
        b.write_block(l.inode_bitmap, &imap);
        let used_dirs = b.dirs.iter().filter(|&&d| (d - 1) / g.inodes_per_group == grp).count();
        let e = grp as usize * GD_SIZE;
        wr32(&mut gdt, e + gd::BLOCK_BITMAP, l.block_bitmap);
        wr32(&mut gdt, e + gd::INODE_BITMAP, l.inode_bitmap);
        wr32(&mut gdt, e + gd::INODE_TABLE, l.inode_table);
        wr16(&mut gdt, e + gd::FREE_BLOCKS, free as u16);
        wr16(&mut gdt, e + gd::FREE_INODES, ifree as u16);
        wr16(&mut gdt, e + gd::USED_DIRS, used_dirs as u16);
        free_blocks += free;
        free_inodes += ifree;
    }

    let (compat, incompat, ro_compat) = g.features();
    let mut s = vec![0u8; 1024];
    wr32(&mut s, sb::INODES_COUNT, g.inodes());
    wr32(&mut s, sb::BLOCKS_COUNT, g.blocks);
    wr32(&mut s, sb::R_BLOCKS_COUNT, g.reserved_blocks);
    wr32(&mut s, sb::FREE_BLOCKS, free_blocks);
    wr32(&mut s, sb::FREE_INODES, free_inodes);
    wr32(&mut s, sb::FIRST_DATA_BLOCK, g.first_data_block());
    wr32(&mut s, sb::LOG_BLOCK_SIZE, g.log_block_size);
    wr32(&mut s, sb::LOG_FRAG_SIZE, g.log_block_size);
    wr32(&mut s, sb::BLOCKS_PER_GROUP, g.blocks_per_group);
    wr32(&mut s, sb::FRAGS_PER_GROUP, g.blocks_per_group);
    wr32(&mut s, sb::INODES_PER_GROUP, g.inodes_per_group);
    wr32(&mut s, sb::WTIME, now);
    wr16(&mut s, sb::MAX_MNT_COUNT, 0xFFFF);
    wr16(&mut s, sb::MAGIC, MAGIC);
    wr16(&mut s, sb::STATE, STATE_VALID);
    wr16(&mut s, sb::ERRORS, 1);
    wr32(&mut s, sb::LASTCHECK, now);
    wr32(&mut s, sb::REV_LEVEL, 1);
    wr32(&mut s, sb::FIRST_INO, FIRST_INO);
    wr16(&mut s, sb::INODE_SIZE, g.inode_size);
    wr32(&mut s, sb::FEATURE_COMPAT, compat);
    wr32(&mut s, sb::FEATURE_INCOMPAT, incompat);
    wr32(&mut s, sb::FEATURE_RO_COMPAT, ro_compat);
    s[sb::UUID..sb::UUID + 16].copy_from_slice(&g.uuid);
    s[sb::VOLUME_NAME..sb::VOLUME_NAME + 16].copy_from_slice(&g.label);
    if g.dir_index {
        for k in 0..4 {
            wr32(&mut s, sb::HASH_SEED + 4 * k, (b.rand() as u32) | 1);
        }
        s[sb::DEF_HASH_VERSION] = 1;
    }
    wr32(&mut s, sb::MKFS_TIME, now);
    b.img.write(SB_OFFSET, &s);
    let gdt_at = (g.first_data_block() + 1) as u64 * bs as u64;
    b.img.write(gdt_at, &gdt);
    for grp in 1..g.groups() {
        let l = g.layout(grp);
        if !l.has_super {
            continue;
        }
        let mut copy = s.clone();
        wr16(&mut copy, sb::BLOCK_GROUP_NR, grp as u16);
        b.img.write(l.start as u64 * bs as u64, &copy);
        b.img.write((l.start + 1) as u64 * bs as u64, &gdt);
    }
    Ok(Made { media: b.img, inodes })
}

/* ---- check ---- */

#[derive(Default, Debug)]
pub struct Check {
    /// What no crash of a correct driver may leave.
    pub corrupt: Vec<String>,
    /// What e2fsck would still fix: a leak, a count, a link count.
    pub unclean: Vec<String>,
    /// The tree, read back: None when the superblock or the root does not
    /// parse.
    pub tree: Option<Node>,
}

impl Check {
    fn corrupt(&mut self, s: String) {
        if self.corrupt.len() < 16 {
            self.corrupt.push(s);
        }
    }

    fn unclean(&mut self, s: String) {
        if self.unclean.len() < 16 {
            self.unclean.push(s);
        }
    }
}

/// What owns a block, as pass 1 found it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Free,
    Meta,
    Inode(u32),
}


struct Reader<'a> {
    img: &'a Media,
    bs: usize,
    blocks: u32,
    first_data_block: u32,
    ipg: u32,
    isz: usize,
    gdt: Vec<u8>,
    owner: Vec<Owner>,
}

impl Reader<'_> {
    fn block(&self, b: u32) -> Vec<u8> {
        self.img.bytes(b as u64 * self.bs as u64, self.bs)
    }

    fn inode(&self, n: u32) -> Vec<u8> {
        let idx = n - 1;
        let grp = idx / self.ipg;
        let it = rd32(&self.gdt, grp as usize * GD_SIZE + gd::INODE_TABLE);
        let at = it as u64 * self.bs as u64 + ((idx % self.ipg) as usize * self.isz) as u64;
        self.img.bytes(at, 128)
    }

    /// Block `b` for inode `n`: false, and what is wrong said, when it is
    /// outside the filesystem, metadata, or somebody's already.
    fn claim(&mut self, n: u32, b: u32, what: &str, c: &mut Check) -> bool {
        if b < self.first_data_block || b >= self.blocks {
            c.corrupt(format!("inode {}: {} block {} is outside the filesystem", n, what, b));
            return false;
        }
        match self.owner[b as usize] {
            Owner::Free => {
                self.owner[b as usize] = Owner::Inode(n);
                true
            }
            Owner::Meta => {
                c.corrupt(format!("inode {}: {} block {} is the filesystem's metadata", n, what, b));
                false
            }
            Owner::Inode(other) => {
                c.corrupt(format!("inode {}: {} block {} is inode {}'s too", n, what, b, other));
                false
            }
        }
    }

    /// The inode's data blocks, (logical, physical) in order, and how many
    /// indirect blocks lead to them -- each claimed for `n`.
    fn blocks_of(&mut self, n: u32, i: &[u8], c: &mut Check) -> (Vec<(u64, u32)>, u32) {
        let ppb = (self.bs / 4) as u64;
        let mut data = Vec::new();
        let mut indirect = 0u32;
        for k in 0..12 {
            let b = rd32(i, ino::BLOCK + 4 * k);
            if b != 0 && self.claim(n, b, "data", c) {
                data.push((k as u64, b));
            }
        }
        /* The single, double and triple indirect trees. */
        let mut base = 12u64;
        for (slot, depth) in [(12usize, 1u32), (13, 2), (14, 3)] {
            let top = rd32(i, ino::BLOCK + 4 * slot);
            if top != 0 && self.claim(n, top, "indirect", c) {
                indirect += 1;
                let mut stack = vec![(top, depth, base)];
                while let Some((blk, d, first)) = stack.pop() {
                    let content = self.block(blk);
                    for k in 0..ppb {
                        let p = rd32(&content, 4 * k as usize);
                        if p == 0 {
                            continue;
                        }
                        let logical = first + k * ppb.pow(d - 1);
                        if d == 1 {
                            if self.claim(n, p, "data", c) {
                                data.push((logical, p));
                            }
                        } else if self.claim(n, p, "indirect", c) {
                            indirect += 1;
                            stack.push((p, d - 1, logical));
                        }
                    }
                }
            }
            base += ppb.pow(depth);
        }
        data.sort();
        (data, indirect)
    }
}

/// What pass 1 learnt of an inode in use.
struct InUse {
    mode: u16,
    size: u64,
    data: Vec<(u64, u32)>,
    links: u32,
}

/// Reads the image as e2fsck -fn would, on a device of `dev_bytes` bytes:
/// what is corrupt, what is unclean, and the tree it holds.
pub fn check(img: &Media, dev_bytes: u64) -> Check {
    let mut c = Check::default();
    let s = img.bytes(SB_OFFSET, 1024);
    if rd16(&s, sb::MAGIC) != MAGIC {
        c.corrupt("no ext2 superblock".into());
        return c;
    }
    let log = rd32(&s, sb::LOG_BLOCK_SIZE);
    let rev = rd32(&s, sb::REV_LEVEL);
    if log > 2 || rev < 1 {
        c.corrupt(format!("block size log {} or revision {} is none of ext2's", log, rev));
        return c;
    }
    let bs = 1024usize << log;
    let blocks = rd32(&s, sb::BLOCKS_COUNT);
    let first_data_block = rd32(&s, sb::FIRST_DATA_BLOCK);
    let bpg = rd32(&s, sb::BLOCKS_PER_GROUP);
    let ipg = rd32(&s, sb::INODES_PER_GROUP);
    let inodes = rd32(&s, sb::INODES_COUNT);
    let isz = rd16(&s, sb::INODE_SIZE) as usize;
    if first_data_block != (log == 0) as u32 || bpg == 0 || bpg as usize > 8 * bs || ipg == 0
        || ipg as usize > 8 * bs || !(isz.is_power_of_two() && isz >= 128 && isz <= bs)
        || blocks <= first_data_block
    {
        c.corrupt(format!("the superblock's geometry is broken: first data block {}, {} blocks and {} inodes a \
                           group, inodes of {} bytes", first_data_block, bpg, ipg, isz));
        return c;
    }
    if blocks as u64 * bs as u64 > dev_bytes {
        c.corrupt(format!("{} blocks of {} bytes do not fit the device's {} bytes", blocks, bs, dev_bytes));
        return c;
    }
    let groups = (blocks - first_data_block).div_ceil(bpg);
    if inodes as u64 != ipg as u64 * groups as u64 {
        c.corrupt(format!("{} inodes, where {} groups of {} make {}", inodes, groups, ipg, ipg as u64 * groups as u64));
        return c;
    }
    let incompat = rd32(&s, sb::FEATURE_INCOMPAT);
    if incompat != INCOMPAT_FILETYPE {
        c.corrupt(format!("incompatible features {:#x}", incompat));
        return c;
    }
    let sparse = rd32(&s, sb::FEATURE_RO_COMPAT) & RO_COMPAT_SPARSE_SUPER != 0;
    let gdt_blocks = (groups as usize * GD_SIZE).div_ceil(bs);
    let gdt = img.bytes((first_data_block as u64 + 1) * bs as u64, gdt_blocks * bs);

    let mut r = Reader { img, bs, blocks, first_data_block, ipg, isz, gdt, owner: vec![Owner::Free; blocks as usize] };
    for b in 0..first_data_block {
        r.owner[b as usize] = Owner::Meta;
    }
    let itable_blocks = (ipg as usize * isz).div_ceil(bs) as u32;
    let mut bmaps = Vec::new();
    let mut imaps = Vec::new();
    for g in 0..groups {
        let start = first_data_block + g * bpg;
        let len = bpg.min(blocks - start);
        let has_super = !sparse || g <= 1 || power_of(g, 3) || power_of(g, 5) || power_of(g, 7);
        if has_super {
            for b in start..(start + 1 + gdt_blocks as u32).min(start + len) {
                r.owner[b as usize] = Owner::Meta;
            }
        }
        let e = g as usize * GD_SIZE;
        let (bb, ib, it) = (rd32(&r.gdt, e + gd::BLOCK_BITMAP), rd32(&r.gdt, e + gd::INODE_BITMAP),
                            rd32(&r.gdt, e + gd::INODE_TABLE));
        let inside = |b: u32, n: u32| b >= start && b.checked_add(n).is_some_and(|end| end <= start + len);
        if !inside(bb, 1) || !inside(ib, 1) || !inside(it, itable_blocks) {
            c.corrupt(format!("group {}'s bitmaps or inode table ({}, {}, {}) are not in the group", g, bb, ib, it));
            return c;
        }
        for b in [bb, ib].into_iter().chain(it..it + itable_blocks) {
            if r.owner[b as usize] == Owner::Meta {
                c.corrupt(format!("group {}'s metadata block {} is another's", g, b));
            }
            r.owner[b as usize] = Owner::Meta;
        }
        bmaps.push(r.block(bb));
        imaps.push(r.block(ib));
    }
    if !c.corrupt.is_empty() {
        return c;
    }
    let bitmap_says = |n: u32| -> bool {
        let idx = n - 1;
        bit(&imaps[(idx / ipg) as usize], (idx % ipg) as usize)
    };

    /* Pass 1: every inode in use -- one with links, as e2fsck has it --
     * and the blocks each holds. */
    let mut in_use: BTreeMap<u32, InUse> = BTreeMap::new();
    for n in (ROOT_INO..=ROOT_INO).chain(FIRST_INO..=inodes) {
        let i = r.inode(n);
        let links = rd16(&i, ino::LINKS) as u32;
        let mode = rd16(&i, ino::MODE);
        if links == 0 {
            if rd32(&i, ino::DTIME) == 0 && mode != 0 {
                c.unclean(format!("inode {} is deleted, and its dtime is not set", n));
            }
            continue;
        }
        if rd32(&i, ino::DTIME) != 0 {
            c.unclean(format!("inode {} is in use, and its dtime is set", n));
        }
        if !bitmap_says(n) {
            c.corrupt(format!("inode {} is in use (links {}), and free in the inode bitmap", n, links));
        }
        let (data, indirect) = r.blocks_of(n, &i, &mut c);
        let size = rd32(&i, ino::SIZE) as u64
            | if mode & S_IFMT == S_IFREG { (rd32(&i, ino::DIR_ACL) as u64) << 32 } else { 0 };
        let want = (data.len() as u32 + indirect) * (bs as u32 / 512);
        if rd32(&i, ino::BLOCKS) != want {
            c.unclean(format!("inode {}: i_blocks {}, and {} blocks make {}", n, rd32(&i, ino::BLOCKS),
                              data.len() as u32 + indirect, want));
        }
        match mode & S_IFMT {
            S_IFREG => {
                if let Some(&(last, _)) = data.last() {
                    if last * bs as u64 > size {
                        c.unclean(format!("inode {}: a block at {} past its size {}", n, last, size));
                    }
                }
            }
            S_IFDIR => {
                let nblocks = size / bs as u64;
                if size % bs as u64 != 0 || data.len() as u64 != nblocks
                    || data.iter().enumerate().any(|(k, &(l, _))| l != k as u64)
                {
                    c.unclean(format!("directory {}: size {}, and blocks {:?}", n, size,
                                      data.iter().map(|d| d.0).collect::<Vec<_>>()));
                }
            }
            _ => c.unclean(format!("inode {} is in use, and of mode {:#o}", n, mode)),
        }
        in_use.insert(n, InUse { mode, size, data, links });
    }

    /* Pass 2 and 3: every directory's entries, from the root down. */
    let mut refs: BTreeMap<u32, u32> = BTreeMap::new();
    let mut subdirs: BTreeMap<u32, u32> = BTreeMap::new();
    let mut parent_of: BTreeMap<u32, u32> = BTreeMap::new();
    let mut entries_of: BTreeMap<u32, Vec<(Vec<u8>, u32)>> = BTreeMap::new();
    let mut reached: BTreeSet<u32> = BTreeSet::new();
    let mut todo = vec![(ROOT_INO, ROOT_INO)];
    reached.insert(ROOT_INO);
    if !in_use.get(&ROOT_INO).is_some_and(|u| u.mode & S_IFMT == S_IFDIR) {
        c.corrupt("the root is no directory in use".into());
        return c;
    }
    while let Some((n, parent)) = todo.pop() {
        let data = in_use[&n].data.clone();
        parent_of.insert(n, parent);
        let mut entries = Vec::new();
        let mut names = BTreeSet::new();
        for &(logical, b) in &data {
            let blk = r.block(b);
            let mut pos = 0usize;
            let mut k = 0;
            while pos < bs {
                if pos + 8 > bs {
                    c.corrupt(format!("directory {}: block {} ends inside an entry at {}", n, logical, pos));
                    break;
                }
                let e_ino = rd32(&blk, pos);
                let rec = rd16(&blk, pos + 4) as usize;
                let nl = blk[pos + 6] as usize;
                let ft = blk[pos + 7];
                if rec < 8 || rec % 4 != 0 || rec > bs - pos || 8 + nl > rec {
                    c.corrupt(format!("directory {}: block {}: an entry at {} of length {} (name {})", n, logical,
                                      pos, rec, nl));
                    break;
                }
                let name = blk[pos + 8..pos + 8 + nl].to_vec();
                if logical == 0 && k < 2 {
                    let (want_name, want_ino): (&[u8], u32) = if k == 0 { (b".", n) } else { (b"..", parent) };
                    if e_ino == 0 || name != want_name {
                        c.corrupt(format!("directory {}: entry {} is '{}' (inode {}), not '{}'", n, k,
                                          crate::model::show(&name), e_ino, crate::model::show(want_name)));
                    } else if e_ino != want_ino {
                        let what = format!("directory {}: '{}' is inode {}, not {}", n, crate::model::show(&name),
                                           e_ino, want_ino);
                        if k == 0 { c.corrupt(what) } else { c.unclean(what) }
                    }
                } else if e_ino != 0 {
                    let show = crate::model::show(&name);
                    if nl == 0 || name.iter().any(|&ch| ch == b'/' || ch == 0) || name == b"." || name == b".." {
                        c.corrupt(format!("directory {}: a name '{}' no entry may have", n, show));
                    } else if e_ino > inodes || e_ino < FIRST_INO {
                        c.corrupt(format!("directory {}: '{}' names inode {}, which no name may", n, show, e_ino));
                    } else if let Some(u) = in_use.get(&e_ino) {
                        /* Two entries of one name: what an error of the
                         * device's between writing an entry and saying so
                         * can leave, and a later create of the name add
                         * to. The first is the one looked up; nothing is
                         * lost or shared, and e2fsck renames one. */
                        if !names.insert(name.clone()) {
                            c.unclean(format!("directory {}: '{}' twice", n, show));
                        }
                        let want_ft = match u.mode & S_IFMT {
                            S_IFDIR => FT_DIR,
                            S_IFREG => FT_REG,
                            _ => 0,
                        };
                        if ft != want_ft {
                            c.corrupt(format!("directory {}: '{}' is of type {}, its inode {} of mode {:#o}", n, show,
                                              ft, e_ino, u.mode));
                        }
                        *refs.entry(e_ino).or_insert(0) += 1;
                        entries.push((name, e_ino));
                        if u.mode & S_IFMT == S_IFDIR {
                            *subdirs.entry(n).or_insert(0) += 1;
                            if reached.insert(e_ino) {
                                todo.push((e_ino, n));
                            } else {
                                c.unclean(format!("directory {} is named again in {}", e_ino, n));
                            }
                        } else {
                            reached.insert(e_ino);
                        }
                    } else {
                        c.corrupt(format!("directory {}: '{}' names inode {}, which is not in use", n, show, e_ino));
                    }
                }
                k += 1;
                pos += rec;
            }
        }
        if data.is_empty() {
            c.corrupt(format!("directory {} has no blocks", n));
        }
        entries_of.insert(n, entries);
    }

    /* Pass 4: each inode's link count is what names it; nothing in use is
     * left unnamed. */
    for (&n, u) in &in_use {
        if !reached.contains(&n) {
            c.unclean(format!("inode {} is in use, and nothing names it", n));
            continue;
        }
        let want = if u.mode & S_IFMT == S_IFDIR {
            let named = refs.get(&n).copied().unwrap_or(0);
            2 + subdirs.get(&n).copied().unwrap_or(0) + named.saturating_sub(1)
        } else {
            refs.get(&n).copied().unwrap_or(0)
        };
        if u.links != want {
            c.unclean(format!("inode {}: links {}, and {} name it", n, u.links, want));
        }
    }

    /* Pass 5: the bitmaps and the counts against what is in use. */
    let mut free_blocks_total = 0u64;
    let mut free_inodes_total = 0u64;
    for g in 0..groups {
        let start = first_data_block + g * bpg;
        let len = bpg.min(blocks - start);
        let map = &bmaps[g as usize];
        let mut free = 0u32;
        for k in 0..len {
            let b = start + k;
            let set = bit(map, k as usize);
            match (set, r.owner[b as usize]) {
                (false, Owner::Meta) => c.corrupt(format!("block {} is the filesystem's metadata, and free", b)),
                (false, Owner::Inode(n)) => c.corrupt(format!("block {} is inode {}'s, and free", b, n)),
                (true, Owner::Free) => c.unclean(format!("block {} is marked in use, and nobody's", b)),
                _ => {}
            }
            free += !set as u32;
        }
        if (len..bpg).any(|k| !bit(map, k as usize)) {
            c.unclean(format!("group {}: the block bitmap's padding is not set", g));
        }
        let e = g as usize * GD_SIZE;
        if rd16(&r.gdt, e + gd::FREE_BLOCKS) as u32 != free {
            c.unclean(format!("group {}: {} free blocks counted, {} in its bitmap", g,
                              rd16(&r.gdt, e + gd::FREE_BLOCKS), free));
        }
        let imap = &imaps[g as usize];
        let mut ifree = 0u32;
        let mut dirs = 0u32;
        for k in 0..ipg {
            let n = g * ipg + k + 1;
            let set = bit(imap, k as usize);
            if n < FIRST_INO && !set {
                c.unclean(format!("reserved inode {} is free in the bitmap", n));
            } else if n >= FIRST_INO && set && !in_use.contains_key(&n) {
                c.unclean(format!("inode {} is marked in use, and is not", n));
            }
            if in_use.get(&n).is_some_and(|u| u.mode & S_IFMT == S_IFDIR) {
                dirs += 1;
            }
            ifree += !set as u32;
        }
        if rd16(&r.gdt, e + gd::FREE_INODES) as u32 != ifree {
            c.unclean(format!("group {}: {} free inodes counted, {} in its bitmap", g,
                              rd16(&r.gdt, e + gd::FREE_INODES), ifree));
        }
        if rd16(&r.gdt, e + gd::USED_DIRS) as u32 != dirs {
            c.unclean(format!("group {}: {} directories counted, {} there", g, rd16(&r.gdt, e + gd::USED_DIRS), dirs));
        }
        free_blocks_total += free as u64;
        free_inodes_total += ifree as u64;
    }
    if rd32(&s, sb::FREE_BLOCKS) as u64 != free_blocks_total {
        c.unclean(format!("the superblock counts {} free blocks, the groups {}", rd32(&s, sb::FREE_BLOCKS),
                          free_blocks_total));
    }
    if rd32(&s, sb::FREE_INODES) as u64 != free_inodes_total {
        c.unclean(format!("the superblock counts {} free inodes, the groups {}", rd32(&s, sb::FREE_INODES),
                          free_inodes_total));
    }
    if rd16(&s, sb::STATE) & STATE_VALID == 0 {
        c.unclean("the filesystem is not marked cleanly unmounted".into());
    }

    /* The tree, from the root down. */
    fn build(r: &Reader, n: u32, entries_of: &BTreeMap<u32, Vec<(Vec<u8>, u32)>>, in_use: &BTreeMap<u32, InUse>,
             depth: u32) -> Node {
        let mut children = BTreeMap::new();
        if depth < 64 {
            for (name, child) in entries_of.get(&n).map(|v| v.as_slice()).unwrap_or(&[]) {
                let node = if entries_of.contains_key(child) {
                    build(r, *child, entries_of, in_use, depth + 1)
                } else {
                    Node::File(content(r, &in_use[child]))
                };
                children.insert(name.clone(), node);
            }
        }
        Node::Dir(children)
    }
    fn content(r: &Reader, u: &InUse) -> Data {
        let mut d = Data::new();
        for &(logical, b) in &u.data {
            let at = logical * r.bs as u64;
            if at >= u.size {
                continue;
            }
            let end = (at + r.bs as u64).min(u.size);
            d.write(at, &r.block(b)[..(end - at) as usize]);
        }
        d.truncate(u.size);
        d
    }
    c.tree = Some(build(&r, ROOT_INO, &entries_of, &in_use, 0));
    c
}
