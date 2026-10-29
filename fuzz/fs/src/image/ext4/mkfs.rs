//! mkfs: an ext4 image of a geometry holding a tree, as mke2fs and a copy
//! in would leave it -- and, asked, with the newest copies of some of its
//! metadata, and of its files' data as Linux's data=journal has a journal
//! hold them, only in a journal nobody replayed, their homes stale.

use std::collections::{BTreeMap, BTreeSet};

use crate::machine::disk::Media;
use crate::model::{Data, Node};

use super::jbd2::Journal;
use super::*;

/// What a writer may do differently within the format.
#[derive(Clone, Debug, Default)]
pub struct Style {
    /// Blocks are taken from anywhere now and then rather than from the
    /// start, so that files are in pieces and their trees deep.
    pub scatter: u64,
    /// Extent blocks hold at most this many entries, 0 for as many as fit:
    /// trees deeper than their files need.
    pub fill: u16,
    /// A block of zeros of a file is now and then allocated as an unwritten
    /// extent over garbage, which must read as zeros.
    pub unwritten: bool,
    /// Groups that hold nothing are left uninitialised (BLOCK_UNINIT,
    /// INODE_UNINIT, itable_unused), as mke2fs leaves them.
    pub uninit: bool,
    /// What nobody wrote -- uninitialised bitmaps, unused inode slots past
    /// a group's last, free blocks -- holds noise rather than zeros.
    pub garbage: bool,
    /// Entries the kernel's tree does not show: a name of 64 bytes or more,
    /// fast and slow symlinks, a device node.
    pub hidden: bool,
    /// Deleted entries and gaps between a directory's live ones.
    pub slack: bool,
    /// Directories now and then indexed, as Linux indexes one past a block:
    /// an htree of one level, its names placed by their half-MD4 hashes.
    pub htree: bool,
    /// The journal carries v3's checksums, as Linux leaves it; else none,
    /// as mke2fs makes it.
    pub journal_csum: bool,
    /// The journal's tags and revoke records 64-bit on a filesystem that is
    /// not -- as one shrunk by `resize2fs -s` keeps its journal.
    pub journal_64: bool,
    /// Second names, each of a file in the same directory: (the link's
    /// path, the path of what it names) -- one inode, two links.
    pub links: Vec<(Vec<Vec<u8>>, Vec<Vec<u8>>)>,
    /// How many blocks' newest copies -- metadata's, and files' data -- are
    /// only in the journal, in transactions nobody replayed, their homes
    /// holding garbage: 0 for a clean journal.
    pub journaled: u8,
    /// Orphans, as a crash between the transactions of one remove or one
    /// cut leaves them, each put at the head of the superblock's list in
    /// turn.
    pub orphans: Vec<Orphan>,
    /// A file of unwritten extents in the root -- what fallocate leaves a
    /// disk's image -- made last, over every free block but about this
    /// many: a disk all but full.
    pub leave_free: Option<u64>,
}

/// The name of the file `Style::leave_free` makes.
pub const FILLER: &[u8] = b"preallocated";

/// An orphan mkfs leaves.
#[derive(Clone, Debug)]
pub enum Orphan {
    /// A file of this content, of no links, named nowhere.
    Deleted(Data),
    /// The file at this path cut to this size: its extents running on past
    /// it, and what its last block held past it still there.
    Cut(Vec<Vec<u8>>, u64),
}

/// What `mkfs` made: the image, which inode each path became, how many
/// entries it made that the tree does not show, and which blocks only the
/// journal holds the newest copy of.
pub struct Made {
    pub media: Media,
    pub inodes: BTreeMap<Vec<Vec<u8>>, u32>,
    pub hidden: u32,
    /// The directories holding those entries, by path, and their names.
    pub hidden_at: BTreeMap<Vec<Vec<u8>>, Vec<Vec<u8>>>,
    pub journaled: Vec<u64>,
    /// The file blocks of each file allocated unwritten, by path.
    pub unwritten: BTreeMap<Vec<Vec<u8>>, Vec<u64>>,
    /// The size of the file `Style::leave_free` made, if it made one.
    pub filled: Option<u64>,
}

/// A file's runs: logical block, device block, length, unwritten.
type Run = (u64, u64, u32, bool);

/// The deepest extent tree made: 4, where the kernel allows 5. No file
/// needs a fifth level at 4 KiB blocks -- e2fsck calls one "could be more
/// shallow" -- and e2fsck 1.46.5, the one the gate judges by, writes past
/// a five-entry array of levels on one (CVE-2022-1304's fix came after).
const MAX_DEPTH: u16 = 4;

struct Builder<'a> {
    g: &'a Geometry,
    bs: usize,
    img: Media,
    used: Vec<bool>,
    ino_used: Vec<bool>,
    table: BTreeMap<u32, Vec<u8>>,
    now: u32,
    style: Style,
    chaos: u64,
    seed: u32,
    /// Blocks of metadata written outside the groups' own -- directory and
    /// extent blocks -- which a journal may hold the newest copies of.
    meta: BTreeSet<u64>,
    /// Blocks of files' data, written: which a journal may hold the newest
    /// copies of too, as Linux's data=journal leaves them -- and a read of
    /// the file mounted read-only must then take from the journal.
    data: BTreeSet<u64>,
    hidden: u32,
    hidden_at: BTreeMap<Vec<Vec<u8>>, Vec<Vec<u8>>>,
    /// The superblock's seed of the directories' name hash.
    hash_seed: [u32; 4],
    /// The file blocks of each inode allocated unwritten.
    unwritten: BTreeMap<u32, Vec<u64>>,
    /// The head of the orphan list.
    last_orphan: u32,
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

    fn noise(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.rand() as u8).collect()
    }

    fn alloc_block(&mut self) -> Result<u64, String> {
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
                return Ok(b as u64);
            }
        }
        Err("no free block".into())
    }

    /// `n` blocks, consecutive where the first free run allows it.
    fn alloc_run(&mut self, n: usize) -> Result<Vec<u64>, String> {
        let first = self.g.first_data_block() as usize;
        let mut at = first;
        while at + n <= self.used.len() {
            match (at..at + n).find(|&b| self.used[b]) {
                Some(b) => at = b + 1,
                None => {
                    for b in at..at + n {
                        self.used[b] = true;
                    }
                    return Ok((at as u64..(at + n) as u64).collect());
                }
            }
        }
        (0..n).map(|_| self.alloc_block()).collect()
    }

    fn alloc_inode(&mut self) -> Result<u32, String> {
        /* The lowest free, as mke2fs takes them -- or, scattering, the
         * lowest free of a group picked at random. */
        let ipg = self.g.inodes_per_group as usize;
        let from = if self.style.scatter != 0 && self.rand() % 4 == 0 {
            (self.rand() as usize % self.g.groups() as usize) * ipg
        } else {
            0
        };
        let n = self.ino_used.len();
        for k in 0..n {
            let i = (from + k) % n;
            if i >= FIRST_INO as usize - 1 && !self.ino_used[i] {
                self.ino_used[i] = true;
                return Ok(i as u32 + 1);
            }
        }
        Err("no free inode".into())
    }

    fn write_block(&mut self, b: u64, data: &[u8]) {
        self.img.write(b * self.bs as u64, data);
    }

    fn new_inode(&mut self, mode: u16, links: u16) -> Vec<u8> {
        let mut i = vec![0u8; self.g.inode_size as usize];
        wr16(&mut i, ino::MODE, mode);
        wr16(&mut i, ino::LINKS, links);
        wr32(&mut i, ino::ATIME, self.now);
        wr32(&mut i, ino::CTIME, self.now);
        wr32(&mut i, ino::MTIME, self.now);
        let generation = self.rand() as u32;
        wr32(&mut i, ino::GENERATION, generation);
        if i.len() > 128 {
            wr16(&mut i, ino::EXTRA_ISIZE, 32);
            wr32(&mut i, ino::CRTIME, self.now);
        }
        i
    }

    /// The seed of inode `n`'s checksums, from its generation.
    fn inode_seed(&self, n: u32, i: &[u8]) -> u32 {
        crc32c(crc32c(self.seed, &n.to_le_bytes()), &rd32(i, ino::GENERATION).to_le_bytes())
    }

    /// Runs of `blocks` (logical, device, unwritten), merged where they
    /// follow on, each no longer than its kind may be.
    fn runs(blocks: &[(u64, u64, bool)]) -> Vec<Run> {
        let mut out: Vec<Run> = Vec::new();
        for &(l, p, uw) in blocks {
            let max = if uw { EXT_INIT_MAX_LEN - 1 } else { EXT_INIT_MAX_LEN };
            if let Some(last) = out.last_mut() {
                if last.3 == uw && last.0 + last.2 as u64 == l && last.1 + last.2 as u64 == p && last.2 < max {
                    last.2 += 1;
                    continue;
                }
            }
            out.push((l, p, 1, uw));
        }
        out
    }

    /// An extent node of `entries` (each 12 bytes), at depth `depth`, with
    /// room for `max`.
    fn node(entries: &[[u8; 12]], depth: u16, max: u16, len: usize) -> Vec<u8> {
        let mut n = vec![0u8; len];
        wr16(&mut n, 0, EXT_MAGIC);
        wr16(&mut n, 2, entries.len() as u16);
        wr16(&mut n, 4, max);
        wr16(&mut n, 6, depth);
        for (k, e) in entries.iter().enumerate() {
            n[12 + 12 * k..24 + 12 * k].copy_from_slice(e);
        }
        n
    }

    /// The extent tree of `runs` under inode `i`: in the inode while four
    /// entries do, else leaves and index blocks up to a root of four,
    /// each block ending in its checksum. The blocks it took.
    fn extents(&mut self, n: u32, i: &mut [u8], runs: &[Run]) -> Result<u64, String> {
        let mut level: Vec<(u64, [u8; 12])> = runs.iter().map(|&(l, p, len, uw)| {
            let mut e = [0u8; 12];
            wr32(&mut e, 0, l as u32);
            wr16(&mut e, 4, if uw { len + EXT_INIT_MAX_LEN } else { len } as u16);
            wr16(&mut e, 6, (p >> 32) as u16);
            wr32(&mut e, 8, p as u32);
            (l, e)
        }).collect();
        let cap = ((self.bs - 12) / 12) as u16;
        let fill = if self.style.fill == 0 { cap } else { self.style.fill.clamp(1, cap) };
        let seed = self.inode_seed(n, i);
        let mut depth = 0u16;
        let mut taken = 0u64;
        while level.len() > 4 {
            if depth == MAX_DEPTH {
                return Err("an extent tree deeper than this makes".into());
            }
            let mut up = Vec::new();
            for chunk in level.chunks(fill as usize) {
                let b = self.alloc_block()?;
                taken += 1;
                let entries: Vec<[u8; 12]> = chunk.iter().map(|c| c.1).collect();
                let mut blk = Self::node(&entries, depth, cap, self.bs);
                let at = 12 + 12 * cap as usize;
                let c = crc32c(seed, &blk[..at]);
                wr32(&mut blk, at, c);
                self.write_block(b, &blk);
                self.meta.insert(b);
                let mut e = [0u8; 12];
                wr32(&mut e, 0, chunk[0].0 as u32);
                wr32(&mut e, 4, b as u32);
                wr16(&mut e, 8, (b >> 32) as u16);
                up.push((chunk[0].0, e));
            }
            level = up;
            depth += 1;
        }
        let entries: Vec<[u8; 12]> = level.iter().map(|c| c.1).collect();
        let root = Self::node(&entries, depth, 4, 60);
        i[ino::BLOCK..ino::BLOCK + 60].copy_from_slice(&root);
        wr32(i, ino::FLAGS, rd32(i, ino::FLAGS) | ino::EXTENTS_FL);
        Ok(taken)
    }

    fn set_blocks(&self, i: &mut [u8], blocks: u64) {
        let units = blocks * (self.bs as u64 / 512);
        wr32(i, ino::BLOCKS, units as u32);
        wr16(i, ino::BLOCKS_HIGH, (units >> 32) as u16);
    }

    fn file(&mut self, content: &Data) -> Result<u32, String> {
        let n = self.alloc_inode()?;
        let mut i = self.new_inode(S_IFREG | 0o644, 1);
        let bs = self.bs as u64;
        let len = content.len();
        if len.div_ceil(bs) > u32::MAX as u64 {
            return Err("a file past what extents number".into());
        }
        wr32(&mut i, ino::SIZE, len as u32);
        wr32(&mut i, ino::SIZE_HIGH, (len >> 32) as u32);
        let nonzero: BTreeSet<u64> = content.nonzero_blocks(bs).into_iter().collect();
        let mut blocks = Vec::new();
        for l in 0..len.div_ceil(bs) {
            if nonzero.contains(&l) {
                let b = self.alloc_block()?;
                let mut data = content.read(l * bs, bs);
                data.resize(self.bs, 0);
                self.write_block(b, &data);
                self.data.insert(b);
                blocks.push((l, b, false));
            } else if self.style.unwritten && self.rand() % 3 == 0 {
                /* Allocated, never written: what is there is whatever the
                 * block had, and must read as zeros. */
                let b = self.alloc_block()?;
                let junk = self.noise(self.bs);
                self.write_block(b, &junk);
                blocks.push((l, b, true));
                self.unwritten.entry(n).or_default().push(l);
            }
        }
        let runs = Self::runs(&blocks);
        let taken = self.extents(n, &mut i, &runs)?;
        self.set_blocks(&mut i, blocks.len() as u64 + taken);
        self.table.insert(n, i);
        Ok(n)
    }

    /// Inode `n`, a file of unwritten extents over the free blocks in
    /// order, as many as leave about `left` free once its tree has its own
    /// -- and no more runs than a tree this deep holds: its size.
    fn filler(&mut self, n: u32, left: u64) -> Result<u64, String> {
        let first = self.g.first_data_block() as usize;
        let cap = ((self.bs - 12) / 12) as u64;
        let fill = if self.style.fill == 0 { cap } else { u64::from(self.style.fill).clamp(2, cap) };
        let max_runs = 4 * fill.pow(u32::from(MAX_DEPTH));
        let tree_blocks = |runs: u64| {
            let (mut level, mut t) = (runs, 0u64);
            while level > 4 {
                level = level.div_ceil(fill);
                t += level;
            }
            t
        };
        let free: Vec<u64> = (first..self.used.len()).filter(|&b| !self.used[b]).map(|b| b as u64).collect();
        let mut blocks: Vec<(u64, u64, bool)> = Vec::new();
        let (mut runs, mut run_len) = (0u64, 0u32);
        for &b in &free {
            let follows = blocks.last().is_some_and(|&(_, p, _)| p + 1 == b) && run_len < EXT_INIT_MAX_LEN - 1;
            let r = if follows { runs } else { runs + 1 };
            if r > max_runs || blocks.len() as u64 + 1 + tree_blocks(r) + left > free.len() as u64 {
                break;
            }
            run_len = if follows { run_len + 1 } else { 1 };
            runs = r;
            blocks.push((blocks.len() as u64, b, true));
        }
        for &(_, b, _) in &blocks {
            self.used[b as usize] = true;
        }
        let mut i = self.new_inode(S_IFREG | 0o644, 1);
        let size = blocks.len() as u64 * self.bs as u64;
        wr32(&mut i, ino::SIZE, size as u32);
        wr32(&mut i, ino::SIZE_HIGH, (size >> 32) as u32);
        let runs = Self::runs(&blocks);
        let taken = self.extents(n, &mut i, &runs)?;
        self.set_blocks(&mut i, blocks.len() as u64 + taken);
        self.unwritten.insert(n, (0..blocks.len() as u64).collect());
        self.table.insert(n, i);
        Ok(size)
    }

    /// A file of a name the tree does not show, or a symlink or device node
    /// it leaves out: (inode, name, type).
    fn hidden_entries(&mut self) -> Result<Vec<(u32, Vec<u8>, u8)>, String> {
        let mut out = Vec::new();
        let long_len = 64 + self.rand() as usize % 192;
        let long: Vec<u8> = (0..long_len).map(|k| b'a' + (k % 26) as u8).collect();
        let n = self.file(&Data::from(b"named past what the tree holds"))?;
        out.push((n, long, FT_REG));
        /* A fast symlink: its target in i_block. */
        let n = self.alloc_inode()?;
        let mut i = self.new_inode(S_IFLNK | 0o777, 1);
        let target = b"somewhere/else";
        wr32(&mut i, ino::SIZE, target.len() as u32);
        i[ino::BLOCK..ino::BLOCK + target.len()].copy_from_slice(target);
        self.table.insert(n, i);
        out.push((n, b"fast-link".to_vec(), FT_SYMLINK));
        /* A slow one: its target in a block of its own. */
        let n = self.alloc_inode()?;
        let mut i = self.new_inode(S_IFLNK | 0o777, 1);
        let target: Vec<u8> = (0..100).map(|k| b"abc/"[k % 4]).collect();
        wr32(&mut i, ino::SIZE, target.len() as u32);
        let b = self.alloc_block()?;
        let mut data = target.clone();
        data.resize(self.bs, 0);
        self.write_block(b, &data);
        let taken = self.extents(n, &mut i, &[(0, b, 1, false)])?;
        self.set_blocks(&mut i, 1 + taken);
        self.table.insert(n, i);
        out.push((n, b"slow-link".to_vec(), FT_SYMLINK));
        /* A character device, its numbers in i_block. */
        let n = self.alloc_inode()?;
        let mut i = self.new_inode(S_IFCHR | 0o600, 1);
        wr32(&mut i, ino::BLOCK + 4, (1 << 8) | 3);
        self.table.insert(n, i);
        out.push((n, b"null".to_vec(), FT_CHRDEV));
        self.hidden += out.len() as u32;
        Ok(out)
    }

    /// A directory's blocks for `entries`: packed linearly, gaps now and
    /// then, the last entry of each block running to its tail.
    fn dir_blocks(&mut self, entries: &[(u32, Vec<u8>, u8)]) -> Vec<Vec<u8>> {
        let bs = self.bs;
        let usable = bs - 12;
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        let mut cur = vec![0u8; bs];
        let (mut pos, mut last) = (0usize, None::<usize>);
        for (k, (n, name, ft)) in entries.iter().enumerate() {
            let gap = if self.style.slack && k >= 2 && self.rand() % 4 == 0 { 12 + 4 * (self.rand() % 8) as usize } else { 0 };
            let need = (8 + name.len()).div_ceil(4) * 4;
            if pos + gap + need > usable {
                if let Some(l) = last {
                    wr16(&mut cur, l + 4, (usable - l) as u16);
                }
                blocks.push(std::mem::replace(&mut cur, vec![0u8; bs]));
                pos = 0;
            }
            if gap != 0 && pos + gap + need <= usable {
                wr16(&mut cur, pos + 4, gap as u16);
                pos += gap;
            }
            wr32(&mut cur, pos, *n);
            wr16(&mut cur, pos + 4, need as u16);
            cur[pos + 6] = name.len() as u8;
            cur[pos + 7] = *ft;
            cur[pos + 8..pos + 8 + name.len()].copy_from_slice(name);
            last = Some(pos);
            pos += need;
        }
        match last {
            Some(l) => wr16(&mut cur, l + 4, (usable - l) as u16),
            None => wr16(&mut cur, 4, usable as u16),
        }
        blocks.push(cur);
        blocks
    }

    /// An htree directory's blocks for `children`: the index's root -- `.`,
    /// a `..` spanning it, one level of entries and the index's tail --
    /// then the leaves, each the names of its range of hashes, none split
    /// across two. None where they would not take one level.
    fn dx_blocks(&mut self, n: u32, parent: u32, children: &[(u32, Vec<u8>, u8)], seed: u32)
        -> Option<Vec<Vec<u8>>> {
        let bs = self.bs;
        let usable = bs - 12;
        let mut named: Vec<(u32, (u32, Vec<u8>, u8))> =
            children.iter().map(|c| (hash::name_hash(&c.1, &self.hash_seed), c.clone())).collect();
        named.sort_by_key(|e| e.0);
        let cap = usable * (2 + (self.rand() % 3) as usize) / 4;
        let mut leaves: Vec<(u32, Vec<(u32, Vec<u8>, u8)>)> = Vec::new();
        let (mut cur, mut used, mut bound) = (Vec::new(), 0usize, 0u32);
        for k in 0..named.len() {
            let need = (8 + named[k].1 .1.len()).div_ceil(4) * 4;
            if used + need > cap && !cur.is_empty() && named[k - 1].0 != named[k].0 {
                leaves.push((bound, std::mem::take(&mut cur)));
                used = 0;
                bound = named[k].0;
            }
            if used + need > usable {
                return None;
            }
            cur.push(named[k].1.clone());
            used += need;
        }
        leaves.push((bound, cur));
        let limit = (bs - 40) / 8;
        if leaves.len() > limit {
            return None;
        }
        let mut root = vec![0u8; bs];
        wr32(&mut root, 0, n);
        wr16(&mut root, 4, 12);
        root[6] = 1;
        root[7] = FT_DIR;
        root[8] = b'.';
        wr32(&mut root, 12, parent);
        wr16(&mut root, 16, (bs - 12) as u16);
        root[18] = 2;
        root[19] = FT_DIR;
        root[20..22].copy_from_slice(b"..");
        /* dx_root_info: half-MD4, 8 bytes of it, one level. */
        root[28] = 1;
        root[29] = 8;
        wr16(&mut root, 32, limit as u16);
        wr16(&mut root, 34, leaves.len() as u16);
        wr32(&mut root, 36, 1);
        for (k, (low, _)) in leaves.iter().enumerate().skip(1) {
            wr32(&mut root, 32 + 8 * k, *low);
            wr32(&mut root, 32 + 8 * k + 4, k as u32 + 1);
        }
        let tail = 32 + 8 * limit;
        let mut x = crc32c(seed, &root[..32 + 8 * leaves.len()]);
        x = crc32c(x, &root[tail..tail + 4]);
        x = crc32c(x, &[0, 0, 0, 0]);
        wr32(&mut root, tail + 4, x);
        let mut out = vec![root];
        let slack = self.style.slack;
        self.style.slack = false;
        for (_, entries) in &leaves {
            let mut blocks = self.dir_blocks(entries);
            if blocks.len() != 1 {
                self.style.slack = slack;
                return None;
            }
            out.push(blocks.remove(0));
        }
        self.style.slack = slack;
        Some(out)
    }

    fn dir(&mut self, n: u32, parent: u32, children: &[(u32, Vec<u8>, u8)], subdirs: u32) -> Result<(), String> {
        let mut entries = vec![(n, b".".to_vec(), FT_DIR), (parent, b"..".to_vec(), FT_DIR)];
        entries.extend_from_slice(children);
        let links = if 2 + subdirs > 65000 { 1 } else { 2 + subdirs } as u16;
        let mut i = self.new_inode(S_IFDIR | 0o755, links);
        let seed = self.inode_seed(n, &i);
        let indexed = self.style.htree && children.len() >= 4 && self.rand() % 2 == 0;
        let dx = if indexed { self.dx_blocks(n, parent, children, seed) } else { None };
        let indexed = dx.is_some();
        let blocks = match dx {
            Some(blocks) => {
                let flags = rd32(&i, ino::FLAGS) | ino::INDEX_FL;
                wr32(&mut i, ino::FLAGS, flags);
                blocks
            }
            None => self.dir_blocks(&entries),
        };
        let mut placed = Vec::new();
        for (l, mut blk) in blocks.into_iter().enumerate() {
            /* The tail: an entry of nothing, 12 bytes, of the checksum's
             * type, holding the checksum of what is before it -- but on an
             * index's root, which carries its own. */
            if !(indexed && l == 0) {
                let at = self.bs - 12;
                wr16(&mut blk, at + 4, 12);
                blk[at + 7] = FT_DIR_CSUM;
                let c = crc32c(seed, &blk[..at]);
                wr32(&mut blk, at + 8, c);
            }
            let b = self.alloc_block()?;
            self.write_block(b, &blk);
            self.meta.insert(b);
            placed.push((l as u64, b, false));
        }
        wr32(&mut i, ino::SIZE, (placed.len() * self.bs) as u32);
        let runs = Self::runs(&placed);
        let taken = self.extents(n, &mut i, &runs)?;
        self.set_blocks(&mut i, placed.len() as u64 + taken);
        self.table.insert(n, i);
        Ok(())
    }

    /// The tree under `node`, as directory `n`'s children.
    fn populate(&mut self, node: &Node, n: u32, parent: u32, at: &mut Vec<Vec<u8>>,
                inodes: &mut BTreeMap<Vec<Vec<u8>>, u32>, extra: &[(u32, Vec<u8>, u8)]) -> Result<(), String> {
        let mut children: Vec<(u32, Vec<u8>, u8)> = extra.to_vec();
        let mut subdirs = extra.iter().filter(|e| e.2 == FT_DIR).count() as u32;
        let mut sub: Vec<(u32, &Node, Vec<u8>)> = Vec::new();
        for (name, child) in node.children().expect("a directory") {
            at.push(name.clone());
            if self.style.links.iter().any(|(l, _)| l == at) {
                /* A second name: made below, once what it names is. */
                at.pop();
                continue;
            }
            match child {
                Node::File(content) => {
                    let c = self.file(content)?;
                    inodes.insert(at.clone(), c);
                    children.push((c, name.clone(), FT_REG));
                }
                Node::Dir(_) => {
                    let c = self.alloc_inode()?;
                    inodes.insert(at.clone(), c);
                    children.push((c, name.clone(), FT_DIR));
                    sub.push((c, child, name.clone()));
                    subdirs += 1;
                }
            }
            at.pop();
        }
        for (link, target) in self.style.links.clone() {
            if link.len() != at.len() + 1 || !link.starts_with(at) {
                continue;
            }
            let Some(&c) = inodes.get(&target) else { return Err("a link to nothing".into()) };
            inodes.insert(link.clone(), c);
            children.push((c, link.last().cloned().unwrap_or_default(), FT_REG));
            if let Some(i) = self.table.get_mut(&c) {
                let n = rd16(i, ino::LINKS);
                wr16(i, ino::LINKS, n + 1);
            }
        }
        if self.style.hidden && self.rand() % 3 == 0 {
            let h = self.hidden_entries()?;
            self.hidden_at.insert(at.clone(), h.iter().map(|e| e.1.clone()).collect());
            children.extend(h);
        }
        self.dir(n, parent, &children, subdirs)?;
        for (c, child, name) in sub {
            at.push(name);
            self.populate(child, c, n, at, inodes, &[])?;
            at.pop();
        }
        Ok(())
    }

    /// The journal's blocks and its inode -- its blocks run together where
    /// they can be -- with s_jnl_blocks's backup of where they are.
    fn journal(&mut self) -> Result<Vec<u64>, String> {
        let n = self.g.journal_blocks as usize;
        let blocks = self.alloc_run(n)?;
        self.ino_used[JOURNAL_INO as usize - 1] = true;
        let mut i = self.new_inode(S_IFREG | 0o600, 1);
        let len = (n * self.bs) as u64;
        wr32(&mut i, ino::SIZE, len as u32);
        wr32(&mut i, ino::SIZE_HIGH, (len >> 32) as u32);
        let list: Vec<(u64, u64, bool)> = blocks.iter().enumerate().map(|(l, &b)| (l as u64, b, false)).collect();
        let runs = Self::runs(&list);
        let taken = self.extents(JOURNAL_INO, &mut i, &runs)?;
        self.set_blocks(&mut i, n as u64 + taken);
        self.table.insert(JOURNAL_INO, i);
        /* Its own extent blocks are read to find it, before any replay:
         * not a block the journal may hold the newest copy of. */
        self.meta.clear();
        Ok(blocks)
    }
}

/// An ext4 image of geometry `g` holding `tree` -- its root the root
/// directory, lost+found beside what the tree has -- as mke2fs and a copy
/// in would leave it: a filesystem e2fsck -fn passes, or, with
/// `style.journaled`, one that passes it once its journal is replayed.
pub fn mkfs(g: &Geometry, tree: &Node, style: &Style, now: u32, chaos: u64) -> Result<Made, String> {
    if !g.valid() {
        return Err(format!("not a geometry mkfs makes: {:?}", g));
    }
    if tree.children().is_some_and(|c| c.contains_key(&b"lost+found".to_vec())) {
        return Err("the tree has a lost+found of its own".into());
    }
    if style.journaled > 0 && g.journal_blocks == 0 {
        return Err("journaled blocks with no journal".into());
    }
    let bs = g.block_size() as usize;
    /* The checksums' seed: the UUID's -- with metadata_csum_seed, the seed
     * is kept and the UUID changed after, as tune2fs -U leaves it. */
    let seed = crc32c(!0, &g.uuid);
    let mut uuid = g.uuid;
    if g.csum_seed {
        uuid[0] ^= 0x5A;
        uuid[15] ^= 0xA5;
    }
    let mut b = Builder {
        g,
        bs,
        img: Media::new(),
        used: vec![false; g.blocks as usize],
        ino_used: vec![false; g.inodes() as usize],
        table: BTreeMap::new(),
        now,
        style: style.clone(),
        chaos: chaos | 1,
        seed,
        meta: BTreeSet::new(),
        data: BTreeSet::new(),
        hidden: 0,
        hidden_at: BTreeMap::new(),
        hash_seed: [0; 4],
        unwritten: BTreeMap::new(),
        last_orphan: 0,
    };
    for k in 0..4 {
        b.hash_seed[k] = b.rand() as u32;
    }
    for i in 0..g.first_data_block() as usize {
        b.used[i] = true;
    }
    for grp in 0..g.groups() {
        for blk in g.meta_in(grp) {
            b.used[blk as usize] = true;
        }
    }
    for i in 0..FIRST_INO as usize - 1 {
        b.ino_used[i] = true;
    }
    let journal = if g.journal_blocks > 0 { Some(b.journal()?) } else { None };
    let lf = b.alloc_inode()?;
    if lf != LOST_FOUND_INO {
        return Err("lost+found is not inode 11".into());
    }
    let mut inodes = BTreeMap::new();
    b.dir(lf, ROOT_INO, &[], 0)?;
    /* The filler named in the root as it is made, and given its blocks
     * once everything else has its own. */
    if style.leave_free.is_some() && tree.children().is_some_and(|c| c.contains_key(FILLER)) {
        return Err("the tree has a file of the filler's name".into());
    }
    let filler = match style.leave_free {
        Some(_) => Some(b.alloc_inode()?),
        None => None,
    };
    let mut top = vec![(lf, b"lost+found".to_vec(), FT_DIR)];
    if let Some(n) = filler {
        top.push((n, FILLER.to_vec(), FT_REG));
    }
    b.populate(tree, ROOT_INO, ROOT_INO, &mut Vec::new(), &mut inodes, &top)?;
    for o in &style.orphans {
        let n = match o {
            Orphan::Deleted(content) => {
                let n = b.file(content)?;
                if let Some(i) = b.table.get_mut(&n) {
                    wr16(i, ino::LINKS, 0);
                }
                n
            }
            Orphan::Cut(path, size) => {
                let Some(&n) = inodes.get(path) else { return Err("a cut of nothing".into()) };
                let Some(i) = b.table.get_mut(&n) else { return Err("a cut of no inode".into()) };
                let len = rd32(i, ino::SIZE) as u64 | (rd32(i, ino::SIZE_HIGH) as u64) << 32;
                if *size >= len || rd16(i, ino::LINKS) != 1 {
                    return Err("a cut of a file not bigger, or of two names".into());
                }
                wr32(i, ino::SIZE, *size as u32);
                wr32(i, ino::SIZE_HIGH, (*size >> 32) as u32);
                n
            }
        };
        let head = b.last_orphan;
        if let Some(i) = b.table.get_mut(&n) {
            wr32(i, ino::DTIME, head);
        }
        b.last_orphan = n;
    }
    let filled = match (filler, style.leave_free) {
        (Some(n), Some(left)) => {
            let size = b.filler(n, left)?;
            inodes.insert(vec![FILLER.to_vec()], n);
            Some(size)
        }
        _ => None,
    };

    if style.garbage {
        /* Noise in some free blocks: a hole must read as zeros, never as
         * what a free block holds. */
        for blk in 0..g.blocks as usize {
            if !b.used[blk] && b.rand() % 8 == 0 {
                let junk = b.noise(bs);
                b.write_block(blk as u64, &junk);
            }
        }
    }
    write_groups(&mut b);
    let journaled = match &journal {
        Some(blocks) => write_journal(&mut b, blocks, &uuid)?,
        None => Vec::new(),
    };
    write_super(&mut b, &uuid, journal.as_deref(), !journaled.is_empty());
    let hidden = b.hidden;
    let unwritten = inodes.iter().filter_map(|(p, n)| b.unwritten.get(n).map(|v| (p.clone(), v.clone()))).collect();
    Ok(Made { media: b.img, inodes, hidden, hidden_at: b.hidden_at, journaled, unwritten, filled })
}

/// The groups: inode tables, bitmaps and descriptors, each with its
/// checksum, the groups that hold nothing left uninitialised if the style
/// asks.
fn write_groups(b: &mut Builder) {
    let g = b.g;
    let bs = b.bs;
    let ipg = g.inodes_per_group as usize;
    let isz = g.inode_size as usize;
    let ds = g.desc_size() as usize;
    let groups = g.groups();
    let mut gdt = vec![0u8; g.gdt_blocks() as usize * bs];
    for grp in 0..groups {
        let l = g.layout(grp);
        let first_ino = grp as usize * ipg;
        let in_use: Vec<usize> = (0..ipg).filter(|&k| b.ino_used[first_ino + k]).collect();
        let last_used = in_use.last().map(|&k| k + 1).unwrap_or(0);
        let data_in_group =
            (l.start..l.start + l.len).filter(|&blk| b.used[blk as usize]).count() > g.meta_in(grp).len();
        let others_meta = g.meta_in(grp).len() as u64 > (if l.has_super { 1 + g.gdt_blocks() } else { 0 })
            + own_meta_in(g, grp);
        let last = grp + 1 == groups;
        let block_uninit = b.style.uninit && grp != 0 && !last && !data_in_group && !others_meta;
        let inode_uninit = b.style.uninit && grp != 0 && in_use.is_empty();
        let unused = if b.style.uninit { (ipg - last_used) as u32 } else { 0 };
        let zeroed = !b.style.garbage;
        let mut flags = 0u16;
        if block_uninit {
            flags |= gd::BLOCK_UNINIT;
        }
        if inode_uninit {
            flags |= gd::INODE_UNINIT;
        }
        if zeroed || !b.style.uninit {
            flags |= gd::INODE_ZEROED;
        }
        /* The inode table: every inode in use, zeros below the last of
         * them, and past it -- where nothing is read -- noise when the
         * style has garbage. */
        let mut table = vec![0u8; g.itable_blocks() as usize * bs];
        for k in 0..ipg {
            let n = (first_ino + k + 1) as u32;
            let slot = &mut table[k * isz..(k + 1) * isz];
            if let Some(i) = b.table.get(&n) {
                let mut i = i.clone();
                set_inode_csum(b.seed, n, &mut i);
                slot.copy_from_slice(&i);
            } else if b.style.uninit && !zeroed && (k >= last_used || inode_uninit) {
                let junk = b.noise(isz);
                slot.copy_from_slice(&junk);
            }
        }
        b.img.write(l.inode_table * bs as u64, &table);
        /* The bitmaps: bits past the group's blocks and inodes set. */
        let mut bb = vec![0u8; bs];
        for k in 0..8 * bs {
            let blk = l.start + k as u64;
            if k as u64 >= l.len || b.used[blk as usize] {
                bb[k / 8] |= 1 << (k % 8);
            }
        }
        let mut ib = vec![0u8; bs];
        for k in 0..8 * bs {
            if k >= ipg || b.ino_used[first_ino + k] {
                ib[k / 8] |= 1 << (k % 8);
            }
        }
        let (bb_csum, ib_csum) = (crc32c(b.seed, &bb[..g.blocks_per_group as usize / 8]),
                                  crc32c(b.seed, &ib[..ipg / 8]));
        let bb = if block_uninit && b.style.garbage { b.noise(bs) } else { bb };
        let ib = if inode_uninit && b.style.garbage { b.noise(bs) } else { ib };
        b.img.write(l.block_bitmap * bs as u64, &bb);
        b.img.write(l.inode_bitmap * bs as u64, &ib);
        let free_blocks = if block_uninit {
            (l.len - g.meta_in(grp).len() as u64) as u32
        } else {
            (0..l.len).filter(|&k| !b.used[(l.start + k) as usize]).count() as u32
        };
        let free_inodes = (ipg - in_use.len()) as u32;
        let dirs = in_use.iter().filter(|&&k| {
            b.table.get(&((first_ino + k + 1) as u32)).is_some_and(|i| rd16(i, ino::MODE) & S_IFMT == S_IFDIR)
        }).count() as u32;
        let d = &mut gdt[grp as usize * ds..(grp as usize + 1) * ds];
        let split = |d: &mut [u8], lo: usize, hi: usize, v: u64, wide: bool| {
            wr32(d, lo, v as u32);
            if wide {
                wr32(d, hi, (v >> 32) as u32);
            }
        };
        let split16 = |d: &mut [u8], lo: usize, hi: usize, v: u32, wide: bool| {
            wr16(d, lo, v as u16);
            if wide {
                wr16(d, hi, (v >> 16) as u16);
            }
        };
        let wide = ds >= 64;
        split(d, gd::BLOCK_BITMAP, gd::BLOCK_BITMAP_HI, l.block_bitmap, wide);
        split(d, gd::INODE_BITMAP, gd::INODE_BITMAP_HI, l.inode_bitmap, wide);
        split(d, gd::INODE_TABLE, gd::INODE_TABLE_HI, l.inode_table, wide);
        split16(d, gd::FREE_BLOCKS, gd::FREE_BLOCKS_HI, free_blocks, wide);
        split16(d, gd::FREE_INODES, gd::FREE_INODES_HI, free_inodes, wide);
        split16(d, gd::USED_DIRS, gd::USED_DIRS_HI, dirs, wide);
        split16(d, gd::ITABLE_UNUSED, gd::ITABLE_UNUSED_HI, unused, wide);
        wr16(d, gd::FLAGS, flags);
        if !block_uninit {
            split16(d, gd::BLOCK_BITMAP_CSUM, gd::BLOCK_BITMAP_CSUM_HI, bb_csum, wide);
        }
        if !inode_uninit {
            split16(d, gd::INODE_BITMAP_CSUM, gd::INODE_BITMAP_CSUM_HI, ib_csum, wide);
        }
        let mut c = crc32c(b.seed, &(grp as u32).to_le_bytes());
        c = crc32c(c, &d[..gd::CHECKSUM]);
        c = crc32c(c, &[0, 0]);
        c = crc32c(c, &d[gd::CHECKSUM + 2..]);
        wr16(d, gd::CHECKSUM, c as u16);
    }
    /* The descriptors after every copy of the superblock. */
    for grp in 0..groups {
        if g.has_super(grp) {
            let at = g.layout(grp).start + 1;
            b.img.write(at * bs as u64, &gdt);
        }
    }
}

/// The group's own bitmaps and inode table that lie inside it.
fn own_meta_in(g: &Geometry, grp: u64) -> u64 {
    let l = g.layout(grp);
    let inside = |blk: u64| blk >= l.start && blk < l.start + l.len;
    [l.block_bitmap, l.inode_bitmap].iter().filter(|&&blk| inside(blk)).count() as u64
        + (l.inode_table..l.inode_table + g.itable_blocks()).filter(|&blk| inside(blk)).count() as u64
}

/// Inode `n`'s checksum (`ext4_inode_csum`): from its seed over its bytes,
/// the checksum fields as zeros -- the high one where the extra fields
/// reach it.
pub fn inode_csum(seed: u32, n: u32, i: &[u8]) -> u32 {
    let mut c = crc32c(crc32c(seed, &n.to_le_bytes()), &rd32(i, ino::GENERATION).to_le_bytes());
    c = crc32c(c, &i[..ino::CHECKSUM_LO]);
    c = crc32c(c, &[0, 0]);
    c = crc32c(c, &i[ino::CHECKSUM_LO + 2..128]);
    if i.len() > 128 {
        c = crc32c(c, &i[128..ino::CHECKSUM_HI]);
        let fits = 128 + rd16(i, ino::EXTRA_ISIZE) as usize >= ino::CHECKSUM_HI + 2;
        let at = if fits {
            c = crc32c(c, &[0, 0]);
            ino::CHECKSUM_HI + 2
        } else {
            ino::CHECKSUM_HI
        };
        c = crc32c(c, &i[at..]);
    }
    c
}

fn set_inode_csum(seed: u32, n: u32, i: &mut [u8]) {
    let c = inode_csum(seed, n, i);
    wr16(i, ino::CHECKSUM_LO, c as u16);
    if i.len() > 128 && 128 + rd16(i, ino::EXTRA_ISIZE) as usize >= ino::CHECKSUM_HI + 2 {
        wr16(i, ino::CHECKSUM_HI, (c >> 16) as u16);
    }
}

/// The journal: its superblock and, with `style.journaled`, transactions
/// holding the newest copies of that many blocks, metadata's and files'
/// data's about evenly -- their homes then given garbage -- among others a
/// replay must leave alone: a copy of a block a later transaction revokes,
/// and a last transaction never committed. The blocks whose homes are
/// stale.
fn write_journal(b: &mut Builder, blocks: &[u64], uuid: &[u8; 16]) -> Result<Vec<u64>, String> {
    let bs = b.bs;
    let seq = 1 + (b.rand() % 1000) as u32;
    let mut j = Journal::new(bs, blocks.to_vec(), b.style.journal_csum, b.g.bit64 || b.style.journal_64, *uuid, seq);
    let mut stale = Vec::new();
    if b.style.journaled > 0 {
        let meta: Vec<u64> = b.meta.iter().copied().collect();
        let data: Vec<u64> = b.data.iter().copied().collect();
        let mut picked: Vec<u64> = Vec::new();
        for _ in 0..b.style.journaled {
            let from = if data.is_empty() || (!meta.is_empty() && b.rand() % 2 == 0) { &meta } else { &data };
            if from.is_empty() {
                break;
            }
            let c = from[b.rand() as usize % from.len()];
            if !picked.contains(&c) {
                picked.push(c);
            }
        }
        let others: Vec<u64> = meta.iter().chain(&data).copied().filter(|c| !picked.contains(c)).collect();
        let read = |b: &Builder, blk: u64| b.img.bytes(blk * bs as u64, bs);
        let mut time = b.now as u64;
        /* A garbage copy of a block, revoked by a later transaction. */
        let revoked = others.first().copied();
        if let Some(r) = revoked {
            let junk = b.noise(bs);
            j.transaction(&mut b.img, &[(r, junk)], &[], true, time);
            time += 1;
        }
        for chunk in picked.chunks(3) {
            let copies: Vec<(u64, Vec<u8>)> = chunk.iter().map(|&blk| (blk, read(b, blk))).collect();
            let revokes: Vec<u64> = if chunk[0] == picked[0] { revoked.into_iter().collect() } else { Vec::new() };
            j.transaction(&mut b.img, &copies, &revokes, true, time);
            time += 1;
        }
        /* The last transaction, never committed: what it holds is not to
         * be replayed. */
        if let Some(&u) = others.get(1) {
            let junk = b.noise(bs);
            j.transaction(&mut b.img, &[(u, junk)], &[], false, time);
        }
        for &blk in &picked {
            let junk = b.noise(bs);
            b.write_block(blk, &junk);
            stale.push(blk);
        }
    }
    let start = if stale.is_empty() { 0 } else { 1 };
    let sb = j.superblock(start, seq);
    b.write_block(blocks[0], &sb);
    Ok(stale)
}

/// The superblock, and its copy at the start of each group that has one.
fn write_super(b: &mut Builder, uuid: &[u8; 16], journal: Option<&[u64]>, recover: bool) {
    let g = b.g;
    let bs = g.block_size();
    let (compat, mut incompat, ro) = g.features();
    if recover {
        incompat |= INCOMPAT_RECOVER;
    }
    let free_blocks = (g.first_data_block()..g.blocks).filter(|&blk| !b.used[blk as usize]).count() as u64;
    let free_inodes = b.ino_used.iter().filter(|&&u| !u).count() as u32;
    let mut s = vec![0u8; 1024];
    wr32(&mut s, sb::INODES_COUNT, g.inodes() as u32);
    wr32(&mut s, sb::BLOCKS_COUNT_LO, g.blocks as u32);
    wr32(&mut s, sb::FREE_BLOCKS_LO, free_blocks as u32);
    if g.bit64 {
        wr32(&mut s, sb::BLOCKS_COUNT_HI, (g.blocks >> 32) as u32);
        wr32(&mut s, sb::FREE_BLOCKS_HI, (free_blocks >> 32) as u32);
    }
    wr32(&mut s, sb::FREE_INODES, free_inodes);
    wr32(&mut s, sb::FIRST_DATA_BLOCK, g.first_data_block() as u32);
    wr32(&mut s, sb::LOG_BLOCK_SIZE, g.log_block_size);
    wr32(&mut s, sb::LOG_CLUSTER_SIZE, g.log_block_size);
    wr32(&mut s, sb::BLOCKS_PER_GROUP, g.blocks_per_group);
    wr32(&mut s, sb::CLUSTERS_PER_GROUP, g.blocks_per_group);
    wr32(&mut s, sb::INODES_PER_GROUP, g.inodes_per_group);
    wr32(&mut s, sb::WTIME, b.now);
    wr16(&mut s, sb::MAX_MNT_COUNT, 0xFFFF);
    wr16(&mut s, sb::MAGIC, MAGIC);
    wr16(&mut s, sb::STATE, 1);
    wr16(&mut s, sb::ERRORS, 1);
    wr32(&mut s, sb::LASTCHECK, b.now);
    wr32(&mut s, sb::REV_LEVEL, 1);
    wr32(&mut s, sb::FIRST_INO, FIRST_INO);
    wr16(&mut s, sb::INODE_SIZE, g.inode_size);
    wr32(&mut s, sb::FEATURE_COMPAT, compat);
    wr32(&mut s, sb::FEATURE_INCOMPAT, incompat);
    wr32(&mut s, sb::FEATURE_RO_COMPAT, ro);
    s[sb::UUID..sb::UUID + 16].copy_from_slice(uuid);
    s[sb::VOLUME_NAME..sb::VOLUME_NAME + 16].copy_from_slice(&g.label);
    for k in 0..4 {
        wr32(&mut s, sb::HASH_SEED + 4 * k, b.hash_seed[k]);
    }
    s[sb::DEF_HASH_VERSION] = 1;
    if g.bit64 {
        wr16(&mut s, sb::DESC_SIZE, 64);
    }
    wr32(&mut s, sb::MKFS_TIME, b.now);
    wr32(&mut s, sb::LAST_ORPHAN, b.last_orphan);
    if let Some(blocks) = journal {
        wr32(&mut s, sb::JOURNAL_INUM, JOURNAL_INO);
        /* s_jnl_blocks: the journal inode's i_block, then its size. */
        let i = &b.table[&JOURNAL_INO];
        s[sb::JNL_BLOCKS..sb::JNL_BLOCKS + 60].copy_from_slice(&i[ino::BLOCK..ino::BLOCK + 60]);
        let len = (blocks.len() as u64) * bs;
        wr32(&mut s, sb::JNL_BLOCKS + 60, (len >> 32) as u32);
        wr32(&mut s, sb::JNL_BLOCKS + 64, len as u32);
        s[sb::JNL_BACKUP_TYPE] = 1;
    }
    if g.inode_size > 128 {
        wr16(&mut s, sb::MIN_EXTRA_ISIZE, 32);
        wr16(&mut s, sb::WANT_EXTRA_ISIZE, 32);
    }
    /* s_flags: hashes of unsigned chars. */
    wr32(&mut s, sb::FLAGS, 0x2);
    s[sb::LOG_GROUPS_PER_FLEX] = g.log_groups_per_flex;
    s[sb::CHECKSUM_TYPE] = 1;
    if g.csum_seed {
        wr32(&mut s, sb::CHECKSUM_SEED, b.seed);
    }
    for grp in 0..g.groups() {
        if !g.has_super(grp) {
            continue;
        }
        let mut copy = s.clone();
        wr16(&mut copy, 0x5A, grp as u16);
        let c = crc32c(!0, &copy[..sb::CHECKSUM]);
        wr32(&mut copy, sb::CHECKSUM, c);
        let at = if grp == 0 { sb::OFFSET } else { g.layout(grp).start * bs };
        b.img.write(at, &copy);
    }
}
