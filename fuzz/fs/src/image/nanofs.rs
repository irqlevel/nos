//! nanofs images, as the format has them: made (`mkfs`, what `format` and a
//! tree put on it would leave) and judged (`check`). The format is the
//! kernel's own -- there is no other writer of it than scripts/mkfs_nanofs.py
//! -- so this is written from its layout: a superblock with both bitmaps,
//! an inode a block, every block checksummed and every file's data too.
//!
//! What `check` calls corrupt is what nanofs promises a crash never leaves:
//! a reachable inode or a file's data that fails its checksum ("either the
//! old file or the new one, never a mix"), two files on one block, a
//! reachable inode the bitmap says is free. Unclean is what the mount
//! tolerates or repairs, and a clean unmount should not leave: a leaked
//! inode or block, an entry naming nothing, a name in two directories.

use std::collections::{BTreeMap, BTreeSet};

use kcore::crc32::{crc32_update, crc32_with_hole};

use crate::machine::disk::Media;
use crate::model::{Data, Node};

pub const MAGIC: u32 = 0x4E41_4E4F;
pub const VERSION: u32 = 1;
pub const BLOCK: usize = 4096;
pub const INODES: u32 = 1024;
pub const DATA_BLOCKS: u32 = 16384;
pub const INODE_START: u32 = 1;
pub const DATA_START: u32 = 1 + INODES;
/// The blocks a file may have, and so the bytes.
pub const MAX_BLOCKS: usize = 256;
pub const MAX_FILE: usize = MAX_BLOCKS * BLOCK;
pub const MAX_ENTRIES: usize = 256;
/// A name is shorter than this.
pub const NAME_LEN: usize = 64;
/// The whole device: the superblock, the inodes and the data.
pub const DEVICE_BYTES: u64 = (DATA_START as u64 + DATA_BLOCKS as u64) * BLOCK as u64;

pub const TYPE_FREE: u32 = 0;
pub const TYPE_FILE: u32 = 1;
pub const TYPE_DIR: u32 = 2;

pub mod sb {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const UUID: usize = 8;
    pub const CHECKSUM: usize = 24;
    pub const BLOCK_SIZE: usize = 28;
    pub const INODE_COUNT: usize = 32;
    pub const DATA_BLOCK_COUNT: usize = 36;
    pub const INODE_START: usize = 40;
    pub const DATA_START: usize = 44;
    pub const INODE_BITMAP: usize = 48;
    pub const DATA_BITMAP: usize = 48 + 128;
}

pub mod ino {
    pub const TYPE: usize = 0;
    pub const SIZE: usize = 4;
    pub const NAME: usize = 8;
    pub const PARENT: usize = 72;
    pub const CHECKSUM: usize = 76;
    pub const DATA_CHECKSUM: usize = 80;
    pub const BLOCKS: usize = 84;
}

fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn wr32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn bit(map: &[u8], i: usize) -> bool {
    map[i / 8] & (1 << (i % 8)) != 0
}

fn set_bit(map: &mut [u8], i: usize) {
    map[i / 8] |= 1 << (i % 8);
}

pub fn inode_at(n: u32) -> u64 {
    (INODE_START + n) as u64 * BLOCK as u64
}

pub fn data_at(b: u32) -> u64 {
    (DATA_START + b) as u64 * BLOCK as u64
}

/// A file's data checksum, as the driver sums it: each block's CRC over the
/// bytes of the file it holds, exclusive-ored; 0 for an empty file.
pub fn data_checksum(content: &[u8]) -> u32 {
    content.chunks(BLOCK).fold(0, |sum, chunk| sum ^ crc32_update(0, chunk))
}

/// Seals a block whose own checksum lives at `hole`.
pub fn seal(block: &mut [u8], hole: usize) {
    let sum = crc32_with_hole(&block[..BLOCK], hole);
    wr32(block, hole, sum);
}

/* ---- mkfs ---- */

pub struct Made {
    pub media: Media,
    /// Which inode each path became.
    pub inodes: BTreeMap<Vec<Vec<u8>>, u32>,
}

/// A nanofs holding `tree`, the root its root: as `format` and the driver
/// would leave it. Err when the tree does not fit the format -- a name too
/// long, a directory too full, a file too big, a tree too deep.
pub fn mkfs(tree: &Node, uuid: [u8; 16], scatter: u64) -> Result<Made, String> {
    let mut s = vec![0u8; BLOCK];
    wr32(&mut s, sb::MAGIC, MAGIC);
    wr32(&mut s, sb::VERSION, VERSION);
    s[sb::UUID..sb::UUID + 16].copy_from_slice(&uuid);
    wr32(&mut s, sb::BLOCK_SIZE, BLOCK as u32);
    wr32(&mut s, sb::INODE_COUNT, INODES);
    wr32(&mut s, sb::DATA_BLOCK_COUNT, DATA_BLOCKS);
    wr32(&mut s, sb::INODE_START, INODE_START);
    wr32(&mut s, sb::DATA_START, DATA_START);

    struct B {
        img: Media,
        imap: Vec<bool>,
        dmap: Vec<bool>,
        chaos: u64,
        scatter: u64,
        inodes: BTreeMap<Vec<Vec<u8>>, u32>,
    }
    impl B {
        fn rand(&mut self) -> u64 {
            let mut x = self.chaos;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.chaos = x;
            x
        }
        fn take(map: &mut [bool], from: usize) -> Option<u32> {
            let n = map.len();
            (0..n).map(|k| (from + k) % n).find(|&i| !map[i]).map(|i| {
                map[i] = true;
                i as u32
            })
        }
        fn block(&mut self) -> Result<u32, String> {
            let from = if self.scatter != 0 && self.rand() % self.scatter == 0 { self.rand() as usize % DATA_BLOCKS as usize } else { 0 };
            B::take(&mut self.dmap, from).ok_or_else(|| "no free data block".to_string())
        }
        fn inode(&mut self) -> Result<u32, String> {
            let from = if self.scatter != 0 && self.rand() % self.scatter == 0 { 1 + self.rand() as usize % (INODES as usize - 1) } else { 1 };
            B::take(&mut self.imap, from).ok_or_else(|| "no free inode".to_string())
        }
        fn put(&mut self, n: u32, kind: u32, size: u32, name: &[u8], parent: u32, blocks: &[u32], dsum: u32) {
            let mut i = vec![0u8; BLOCK];
            wr32(&mut i, ino::TYPE, kind);
            wr32(&mut i, ino::SIZE, size);
            i[ino::NAME..ino::NAME + name.len()].copy_from_slice(name);
            wr32(&mut i, ino::PARENT, parent);
            wr32(&mut i, ino::DATA_CHECKSUM, dsum);
            for (k, b) in blocks.iter().enumerate() {
                wr32(&mut i, ino::BLOCKS + 4 * k, *b);
            }
            seal(&mut i, ino::CHECKSUM);
            self.img.write(inode_at(n), &i);
        }
        fn dir(&mut self, n: u32, name: &[u8], parent: u32, node: &Node, at: &mut Vec<Vec<u8>>) -> Result<(), String> {
            let children = node.children().expect("a directory");
            if children.len() > MAX_ENTRIES {
                return Err("a directory of more entries than one holds".into());
            }
            let block = self.block()?;
            let mut entries = vec![0u8; BLOCK];
            let mut k = 0;
            for (cname, child) in children {
                if cname.is_empty() || cname.len() >= NAME_LEN || cname.contains(&0) {
                    return Err("a name nanofs does not keep".into());
                }
                let c = self.inode()?;
                wr32(&mut entries, 8 * k, c);
                k += 1;
                at.push(cname.clone());
                self.inodes.insert(at.clone(), c);
                match child {
                    Node::Dir(_) => self.dir(c, cname, n, child, at)?,
                    Node::File(content) => {
                        if content.len() > MAX_FILE as u64 {
                            return Err("a file bigger than nanofs holds".into());
                        }
                        let content = &content.to_vec();
                        if content.len() > MAX_FILE {
                            return Err("a file bigger than nanofs holds".into());
                        }
                        let mut blocks = Vec::new();
                        for chunk in content.chunks(BLOCK) {
                            let b = self.block()?;
                            let mut data = chunk.to_vec();
                            data.resize(BLOCK, 0);
                            self.img.write(data_at(b), &data);
                            blocks.push(b);
                        }
                        self.put(c, TYPE_FILE, content.len() as u32, cname, n, &blocks, data_checksum(content));
                    }
                }
                at.pop();
            }
            self.img.write(data_at(block), &entries);
            self.put(n, TYPE_DIR, k as u32, name, parent, &[block], 0);
            Ok(())
        }
    }

    let mut b = B {
        img: Media::new(),
        imap: vec![false; INODES as usize],
        dmap: vec![false; DATA_BLOCKS as usize],
        chaos: 0x9E37_79B9_7F4A_7C15 ^ scatter,
        scatter,
        inodes: BTreeMap::new(),
    };
    b.imap[0] = true;
    b.dir(0, b"/", 0, tree, &mut Vec::new())?;
    for (i, used) in b.imap.iter().enumerate() {
        if *used {
            set_bit(&mut s[sb::INODE_BITMAP..sb::DATA_BITMAP], i);
        }
    }
    for (i, used) in b.dmap.iter().enumerate() {
        if *used {
            set_bit(&mut s[sb::DATA_BITMAP..sb::DATA_BITMAP + 2048], i);
        }
    }
    seal(&mut s, sb::CHECKSUM);
    b.img.write(0, &s);
    Ok(Made { media: b.img, inodes: b.inodes })
}

/* ---- check ---- */

#[derive(Default, Debug)]
pub struct Check {
    pub corrupt: Vec<String>,
    pub unclean: Vec<String>,
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

/// Reads the image as the mount does, and as it should be: what is
/// corrupt, what is unclean, and the tree it holds.
pub fn check(img: &Media) -> Check {
    let mut c = Check::default();
    let s = img.bytes(0, BLOCK);
    if rd32(&s, sb::MAGIC) != MAGIC || rd32(&s, sb::VERSION) != VERSION {
        c.corrupt("no nanofs superblock".into());
        return c;
    }
    if crc32_with_hole(&s, sb::CHECKSUM) != rd32(&s, sb::CHECKSUM) {
        c.corrupt("the superblock's checksum does not match".into());
        return c;
    }
    if rd32(&s, sb::BLOCK_SIZE) != BLOCK as u32 || rd32(&s, sb::INODE_COUNT) != INODES
        || rd32(&s, sb::DATA_BLOCK_COUNT) != DATA_BLOCKS || rd32(&s, sb::INODE_START) != INODE_START
        || rd32(&s, sb::DATA_START) != DATA_START
    {
        c.corrupt("the superblock's layout is not nanofs's".into());
        return c;
    }
    let imap = s[sb::INODE_BITMAP..sb::DATA_BITMAP].to_vec();
    let dmap = s[sb::DATA_BITMAP..sb::DATA_BITMAP + 2048].to_vec();

    let mut owner: BTreeMap<u32, u32> = BTreeMap::new();
    let mut reached: BTreeSet<u32> = BTreeSet::new();
    fn walk(img: &Media, n: u32, parent: u32, depth: usize, c: &mut Check, imap: &[u8], owner: &mut BTreeMap<u32, u32>,
            reached: &mut BTreeSet<u32>, ancestors: &mut Vec<u32>) -> Option<(Vec<u8>, Node)> {
        let i = img.bytes(inode_at(n), BLOCK);
        let kind = rd32(&i, ino::TYPE);
        if kind == TYPE_FREE {
            c.unclean(format!("an entry names inode {}, which is free", n));
            return None;
        }
        if crc32_with_hole(&i, ino::CHECKSUM) != rd32(&i, ino::CHECKSUM) {
            c.corrupt(format!("inode {}'s checksum does not match", n));
            return None;
        }
        if !bit(imap, n as usize) {
            c.unclean(format!("inode {} is reachable, and free in the bitmap", n));
        }
        let name_raw = &i[ino::NAME..ino::NAME + NAME_LEN];
        let name = name_raw[..name_raw.iter().position(|b| *b == 0).unwrap_or(NAME_LEN)].to_vec();
        if n != 0 && rd32(&i, ino::PARENT) != parent {
            c.unclean(format!("inode {} says its directory is {}, and {} holds it", n, rd32(&i, ino::PARENT), parent));
        }
        let size = rd32(&i, ino::SIZE) as usize;
        let block = |k: usize| rd32(&i, ino::BLOCKS + 4 * k);
        let mut claim = |b: u32, c: &mut Check| -> bool {
            if b >= DATA_BLOCKS {
                c.corrupt(format!("inode {} names data block {}, which is not one", n, b));
                return false;
            }
            if let Some(other) = owner.insert(b, n) {
                c.corrupt(format!("data block {} is inode {}'s and inode {}'s", b, other, n));
                return false;
            }
            true
        };
        match kind {
            TYPE_FILE => {
                if size > MAX_FILE {
                    c.corrupt(format!("inode {} claims {} bytes", n, size));
                    return None;
                }
                let mut content = vec![0u8; size];
                for k in 0..size.div_ceil(BLOCK) {
                    let b = block(k);
                    if !claim(b, c) {
                        return None;
                    }
                    let at = k * BLOCK;
                    let end = (at + BLOCK).min(size);
                    content[at..end].copy_from_slice(&img.bytes(data_at(b), end - at));
                }
                let stored = rd32(&i, ino::DATA_CHECKSUM);
                if stored != 0 && data_checksum(&content) != stored {
                    c.corrupt(format!("inode {}'s data does not match its checksum", n));
                }
                Some((name, Node::File(Data::from(&content))))
            }
            TYPE_DIR => {
                if size > MAX_ENTRIES {
                    c.corrupt(format!("directory {} claims {} entries", n, size));
                    return None;
                }
                let b = block(0);
                if !claim(b, c) {
                    return None;
                }
                let entries = img.bytes(data_at(b), BLOCK);
                let mut children = BTreeMap::new();
                ancestors.push(n);
                for k in 0..size {
                    let e = rd32(&entries, 8 * k);
                    if e >= INODES {
                        c.unclean(format!("directory {} names inode {}, past the table", n, e));
                        continue;
                    }
                    if ancestors.contains(&e) {
                        c.unclean(format!("directory {} names inode {}, its own ancestor", n, e));
                        continue;
                    }
                    if !reached.insert(e) {
                        c.unclean(format!("inode {} is named twice (again in {})", e, n));
                        continue;
                    }
                    if let Some((cname, node)) = walk(img, e, n, depth + 1, c, imap, owner, reached, ancestors) {
                        if cname.is_empty() {
                            c.corrupt(format!("inode {} has no name", e));
                        }
                        /* Tolerated by the mount -- the first is the one
                         * looked up -- and left by a move whose second
                         * step failed: nothing is lost or shared. */
                        if children.insert(cname.clone(), node).is_some() {
                            c.unclean(format!("directory {} holds '{}' twice", n, crate::model::show(&cname)));
                        }
                    }
                }
                ancestors.pop();
                Some((name, Node::Dir(children)))
            }
            _ => {
                c.corrupt(format!("inode {} is of type {}", n, kind));
                None
            }
        }
    }
    reached.insert(0);
    let root = walk(img, 0, 0, 0, &mut c, &imap, &mut owner, &mut reached, &mut Vec::new());
    match root {
        Some((_, tree @ Node::Dir(_))) => c.tree = Some(tree),
        Some(_) => {
            c.corrupt("the root is a file".into());
            return c;
        }
        None => {
            c.corrupt("the root cannot be read".into());
            return c;
        }
    }
    for i in 0..INODES {
        if bit(&imap, i as usize) && !reached.contains(&i) {
            c.unclean(format!("inode {} is marked in use, and unreachable", i));
        }
    }
    for b in 0..DATA_BLOCKS {
        let set = bit(&dmap, b as usize);
        match (set, owner.get(&b)) {
            (false, Some(n)) => c.unclean(format!("data block {} is inode {}'s, and free in the bitmap", b, n)),
            (true, None) => c.unclean(format!("data block {} is marked in use, and nobody's", b)),
            _ => {}
        }
    }
    c
}
