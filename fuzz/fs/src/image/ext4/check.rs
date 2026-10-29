//! check: an ext4 image read as e2fsck -fn reads one -- after its journal
//! is replayed and its orphans released, as e2fsck does first -- and what it
//! finds sorted two ways. Corruption is what no crash of a correct driver
//! may leave: a checksum wrong, a block in use and free, a block two inodes
//! hold, a name leading to an inode not in use, an extent tree that does not
//! parse. An unclean filesystem is one e2fsck would still fix: a leak, a
//! count, a link count, an extent past its file's end. The superblock's
//! free counts are not judged: Linux keeps them lazily, and e2fsck puts them
//! right without a word. The tree read back is what the kernel's would show
//! -- directories and regular files, names under 64 bytes.

use std::collections::{BTreeMap, BTreeSet};

use crate::machine::disk::Media;
use crate::model::{Data, Node};

use super::mkfs::inode_csum;
use super::replay;
use super::*;

#[derive(Default, Debug)]
pub struct Check {
    pub corrupt: Vec<String>,
    pub unclean: Vec<String>,
    /// The tree, read back: None when the superblock or the root does not
    /// parse.
    pub tree: Option<Node>,
    /// Transactions the journal held, replayed first.
    pub replayed: u32,
    /// Orphans the superblock's list held, released first.
    pub orphans: u32,
    /// The superblock says the filesystem is mounted: not cleanly
    /// unmounted, as a crash leaves it.
    pub mounted: bool,
    /// By block, whether it holds none of the filesystem's metadata: it is
    /// nobody's, or a regular file's data. Empty when the image is too
    /// damaged to say.
    pub no_metadata: Vec<bool>,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Owner {
    Free,
    Meta,
    Inode(u32),
}

/// What pass 1 learnt of an inode in use.
struct InUse {
    mode: u16,
    size: u64,
    links: u16,
    flags: u32,
    /// Its data, by logical block: (device block, unwritten).
    data: BTreeMap<u64, (u64, bool)>,
}

struct Fs<'a> {
    img: &'a Media,
    bs: u64,
    blocks: u64,
    first: u64,
    groups: u64,
    ipg: u32,
    bpg: u32,
    isz: usize,
    ds: usize,
    csum: bool,
    seed: u32,
    gdt: Vec<u8>,
    owner: Vec<Owner>,
}

fn has_super(g: u64) -> bool {
    fn power(mut n: u64, b: u64) -> bool {
        while n > 1 && n % b == 0 {
            n /= b;
        }
        n == 1
    }
    g <= 1 || power(g, 3) || power(g, 5) || power(g, 7)
}

impl Fs<'_> {
    fn block(&self, b: u64) -> Vec<u8> {
        self.img.bytes(b * self.bs, self.bs as usize)
    }
    fn desc(&self, g: u64) -> &[u8] {
        &self.gdt[g as usize * self.ds..(g as usize + 1) * self.ds]
    }
    fn wide(&self, d: &[u8], lo: usize, hi: usize) -> u64 {
        rd32(d, lo) as u64 | if self.ds >= 64 { (rd32(d, hi) as u64) << 32 } else { 0 }
    }
    fn wide16(&self, d: &[u8], lo: usize, hi: usize) -> u32 {
        rd16(d, lo) as u32 | if self.ds >= 64 { (rd16(d, hi) as u32) << 16 } else { 0 }
    }
    fn table(&self, g: u64) -> u64 {
        self.wide(self.desc(g), gd::INODE_TABLE, gd::INODE_TABLE_HI)
    }
    fn itable_blocks(&self) -> u64 {
        self.ipg as u64 * self.isz as u64 / self.bs
    }
    fn inode(&self, n: u32) -> Vec<u8> {
        let idx = (n - 1) % self.ipg;
        let at = self.table(((n - 1) / self.ipg) as u64) * self.bs + idx as u64 * self.isz as u64;
        self.img.bytes(at, self.isz)
    }
    fn inode_seed(&self, n: u32, i: &[u8]) -> u32 {
        crc32c(crc32c(self.seed, &n.to_le_bytes()), &rd32(i, ino::GENERATION).to_le_bytes())
    }

    /// Block `b` for `who`: false, and what is wrong said, when it is
    /// outside the filesystem, metadata, or somebody's already.
    fn claim(&mut self, who: u32, b: u64, what: &str, c: &mut Check) -> bool {
        if b < self.first || b >= self.blocks {
            c.corrupt(format!("inode {}: {} block {} outside the filesystem", who, what, b));
            return false;
        }
        match self.owner[b as usize] {
            Owner::Free => {
                self.owner[b as usize] = Owner::Inode(who);
                true
            }
            Owner::Meta => {
                c.corrupt(format!("inode {}: {} block {} is the filesystem's metadata", who, what, b));
                false
            }
            Owner::Inode(o) => {
                c.corrupt(format!("inode {}: {} block {} is inode {}'s too", who, what, b, o));
                false
            }
        }
    }

    /// The extent tree under `node` -- its entries in order, none
    /// overlapping, each node's checksum right -- into `data`; the tree's
    /// own blocks claimed and counted. `keep` is how many logical blocks of
    /// the file a pending orphan truncation leaves: blocks past it go to
    /// `freed`, not claimed.
    #[allow(clippy::too_many_arguments)]
    fn extents(&mut self, n: u32, seed: u32, node: &[u8], want: Option<u16>, range: (u64, u64), keep: u64,
               data: &mut BTreeMap<u64, (u64, bool)>, tree_blocks: &mut u64, freed: &mut BTreeSet<u64>,
               c: &mut Check) {
        if node.len() < 12 || rd16(node, 0) != EXT_MAGIC {
            return c.corrupt(format!("inode {}: an extent node without its magic", n));
        }
        let (entries, max, depth) = (rd16(node, 2) as usize, rd16(node, 4) as usize, rd16(node, 6));
        if entries > max || 12 + 12 * max > node.len() || depth > 5 || want.is_some_and(|w| w != depth) {
            return c.corrupt(format!("inode {}: an extent node of {} entries in {}, depth {}", n, entries, max, depth));
        }
        if want.is_some() && entries == 0 {
            c.unclean(format!("inode {}: an empty extent node below the root", n));
        }
        let mut prev_end = range.0;
        for k in 0..entries {
            let e = &node[12 + 12 * k..24 + 12 * k];
            let first = rd32(e, 0) as u64;
            if first < prev_end || first >= range.1 {
                return c.corrupt(format!("inode {}: extents out of order or outside their index ({} after {})", n,
                                         first, prev_end));
            }
            if depth == 0 {
                let raw = rd16(e, 4) as u64;
                let (len, uw) = if raw > EXT_INIT_MAX_LEN as u64 { (raw - EXT_INIT_MAX_LEN as u64, true) } else { (raw, false) };
                let start = ((rd16(e, 6) as u64) << 32) | rd32(e, 8) as u64;
                if len == 0 || first + len > range.1 {
                    return c.corrupt(format!("inode {}: an extent of {} blocks at {}", n, len, first));
                }
                for j in 0..len {
                    let (l, b) = (first + j, start + j);
                    if l >= keep {
                        if b < self.blocks {
                            freed.insert(b);
                        }
                        continue;
                    }
                    if self.claim(n, b, "data", c) {
                        data.insert(l, (b, uw));
                    }
                }
                prev_end = first + len;
            } else {
                let child = ((rd16(e, 8) as u64) << 32) | rd32(e, 4) as u64;
                let end = if k + 1 < entries { rd32(&node[24 + 12 * k..], 0) as u64 } else { range.1 };
                if !self.claim(n, child, "extent", c) {
                    continue;
                }
                *tree_blocks += 1;
                let blk = self.block(child);
                if self.csum {
                    let at = 12 + 12 * rd16(&blk, 4) as usize;
                    if at + 4 > blk.len() || crc32c(seed, &blk[..at]) != rd32(&blk, at) {
                        c.corrupt(format!("inode {}: extent block {} fails its checksum", n, child));
                        continue;
                    }
                }
                if rd16(&blk, 2) > 0 && rd32(&blk, 12) as u64 != first {
                    c.unclean(format!("inode {}: an index entry at {} whose child starts at {}", n, first, rd32(&blk, 12)));
                }
                self.extents(n, seed, &blk, Some(depth - 1), (first, end), keep, data, tree_blocks, freed, c);
                prev_end = first;
            }
        }
    }

    /// A block map's data and indirect blocks, ext2's and ext3's way.
    fn block_map(&mut self, n: u32, i: &[u8], data: &mut BTreeMap<u64, (u64, bool)>, tree_blocks: &mut u64,
                 c: &mut Check) {
        let ppb = self.bs / 4;
        for k in 0..12 {
            let b = rd32(i, ino::BLOCK + 4 * k) as u64;
            if b != 0 && self.claim(n, b, "data", c) {
                data.insert(k as u64, (b, false));
            }
        }
        let mut base = 12u64;
        for (slot, depth) in [(12usize, 1u32), (13, 2), (14, 3)] {
            let top = rd32(i, ino::BLOCK + 4 * slot) as u64;
            if top != 0 && self.claim(n, top, "indirect", c) {
                *tree_blocks += 1;
                let mut stack = vec![(top, depth, base)];
                while let Some((blk, d, first)) = stack.pop() {
                    let content = self.block(blk);
                    for k in 0..ppb {
                        let p = rd32(&content, 4 * k as usize) as u64;
                        if p == 0 {
                            continue;
                        }
                        let l = first + k * ppb.pow(d - 1);
                        if d == 1 {
                            if self.claim(n, p, "data", c) {
                                data.insert(l, (p, false));
                            }
                        } else if self.claim(n, p, "indirect", c) {
                            *tree_blocks += 1;
                            stack.push((p, d - 1, l));
                        }
                    }
                }
            }
            base += ppb.pow(depth);
        }
    }
}

/// Reads the image as e2fsck -fn would, on a device of `dev_bytes` bytes,
/// after replaying its journal and releasing its orphans: what is corrupt,
/// what is unclean, and the tree it holds. The image itself is not changed.
pub fn check(media: &Media, dev_bytes: u64) -> Check {
    let mut c = Check::default();
    let mut img = media.clone();
    match replay::replay(&mut img) {
        Ok(r) => c.replayed = r.transactions,
        Err(e) => {
            c.corrupt(format!("the journal: {}", e));
            return c;
        }
    }
    let s = img.bytes(sb::OFFSET, 1024);
    if rd16(&s, sb::MAGIC) != MAGIC || rd32(&s, sb::REV_LEVEL) < 1 || rd32(&s, sb::LOG_BLOCK_SIZE) > 2 {
        c.corrupt("no ext4 superblock this reads".into());
        return c;
    }
    let incompat = rd32(&s, sb::FEATURE_INCOMPAT);
    let known = INCOMPAT_FILETYPE | INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_CSUM_SEED;
    if incompat & !known != 0 {
        c.corrupt(format!("incompatible features {:#x}", incompat & !known));
        return c;
    }
    let csum = rd32(&s, sb::FEATURE_RO_COMPAT) & RO_COMPAT_METADATA_CSUM != 0;
    if csum && crc32c(!0, &s[..sb::CHECKSUM]) != rd32(&s, sb::CHECKSUM) {
        c.corrupt("the superblock fails its checksum".into());
        return c;
    }
    let bs = 1024u64 << rd32(&s, sb::LOG_BLOCK_SIZE);
    let bit64 = incompat & INCOMPAT_64BIT != 0;
    let blocks = rd32(&s, sb::BLOCKS_COUNT_LO) as u64 | if bit64 { (rd32(&s, sb::BLOCKS_COUNT_HI) as u64) << 32 } else { 0 };
    let first = rd32(&s, sb::FIRST_DATA_BLOCK) as u64;
    let (bpg, ipg) = (rd32(&s, sb::BLOCKS_PER_GROUP), rd32(&s, sb::INODES_PER_GROUP));
    let isz = rd16(&s, sb::INODE_SIZE) as usize;
    let ds = if bit64 { rd16(&s, sb::DESC_SIZE) as usize } else { 32 };
    if blocks <= first || blocks * bs > dev_bytes || bpg == 0 || bpg as u64 > 8 * bs || ipg == 0 || ipg as u64 > 8 * bs
        || !(isz == 128 || isz == 256) || (ds != 32 && ds != 64)
    {
        c.corrupt("a geometry that does not hold together".into());
        return c;
    }
    let groups = (blocks - first).div_ceil(bpg as u64);
    let inodes = rd32(&s, sb::INODES_COUNT);
    if groups * ipg as u64 != inodes as u64 {
        c.corrupt("an inode count that is not the groups'".into());
        return c;
    }
    let seed = if incompat & INCOMPAT_CSUM_SEED != 0 { rd32(&s, sb::CHECKSUM_SEED) } else { crc32c(!0, &s[sb::UUID..sb::UUID + 16]) };
    let gdt_blocks = (groups * ds as u64).div_ceil(bs);
    let gdt = img.bytes((first + 1) * bs, (gdt_blocks * bs) as usize);
    c.mounted = rd16(&s, sb::STATE) & 1 == 0;
    let mut fs = Fs { img: &img, bs, blocks, first, groups, ipg, bpg, isz, ds, csum, seed, gdt,
                      owner: vec![Owner::Free; blocks as usize] };

    /* Descriptors, and the metadata they and the superblock's copies take. */
    for b in 0..first {
        fs.owner[b as usize] = Owner::Meta;
    }
    /* resize_inode's reserved descriptor blocks, after each copy of the
     * descriptors: the filesystem's, as the kernel counts them. */
    let resize = rd32(&s, sb::FEATURE_COMPAT) & 0x10 != 0;
    let reserved = if resize { rd16(&s, 0xCE) as u64 } else { 0 };
    for g in 0..groups {
        let start = first + g * bpg as u64;
        if has_super(g) {
            for b in start..(start + 1 + gdt_blocks + reserved).min(blocks) {
                fs.owner[b as usize] = Owner::Meta;
            }
        }
        let d = fs.desc(g).to_vec();
        if csum {
            let mut x = crc32c(seed, &(g as u32).to_le_bytes());
            x = crc32c(x, &d[..gd::CHECKSUM]);
            x = crc32c(x, &[0, 0]);
            x = crc32c(x, &d[gd::CHECKSUM + 2..]);
            if x as u16 != rd16(&d, gd::CHECKSUM) {
                c.corrupt(format!("group {}'s descriptor fails its checksum", g));
                return c;
            }
        }
        let (bb, ib, it) = (fs.wide(&d, gd::BLOCK_BITMAP, gd::BLOCK_BITMAP_HI), fs.wide(&d, gd::INODE_BITMAP, gd::INODE_BITMAP_HI),
                            fs.table(g));
        for (b, n) in [(bb, 1), (ib, 1), (it, fs.itable_blocks())] {
            if b < first || b + n > blocks || (b..b + n).any(|x| fs.owner[x as usize] != Owner::Free) {
                c.corrupt(format!("group {}'s bitmaps or table outside the filesystem or over other metadata", g));
                return c;
            }
            for x in b..b + n {
                fs.owner[x as usize] = Owner::Meta;
            }
        }
    }
    let uninit = csum;
    let flags = |fs: &Fs, g: u64| if uninit { rd16(fs.desc(g), gd::FLAGS) } else { 0 };

    /* The bitmaps, each as its descriptor's checksum has it -- or, left
     * uninitialised, as the group's own metadata makes it. */
    let mut block_bit = vec![false; blocks as usize];
    let mut inode_bit = vec![false; inodes as usize];
    for g in 0..groups {
        let d = fs.desc(g).to_vec();
        let f = flags(&fs, g);
        let start = first + g * bpg as u64;
        let len = (bpg as u64).min(blocks - start);
        if f & gd::BLOCK_UNINIT == 0 {
            let bb = fs.block(fs.wide(&d, gd::BLOCK_BITMAP, gd::BLOCK_BITMAP_HI));
            if csum {
                let want = crc32c(seed, &bb[..bpg as usize / 8]);
                let have = fs.wide16(&d, gd::BLOCK_BITMAP_CSUM, gd::BLOCK_BITMAP_CSUM_HI);
                if if ds >= 64 { want != have } else { want & 0xFFFF != have } {
                    c.corrupt(format!("group {}'s block bitmap fails its checksum", g));
                }
            }
            for k in 0..len {
                block_bit[(start + k) as usize] = bb[(k / 8) as usize] >> (k % 8) & 1 != 0;
            }
            if (len..8 * bs).any(|k| bb[(k / 8) as usize] >> (k % 8) & 1 == 0) {
                c.unclean(format!("group {}'s block bitmap padding is not set", g));
            }
        } else {
            for k in 0..len {
                block_bit[(start + k) as usize] = fs.owner[(start + k) as usize] == Owner::Meta;
            }
        }
        if f & gd::INODE_UNINIT == 0 {
            let ib = fs.block(fs.wide(&d, gd::INODE_BITMAP, gd::INODE_BITMAP_HI));
            if csum {
                let want = crc32c(seed, &ib[..ipg as usize / 8]);
                let have = fs.wide16(&d, gd::INODE_BITMAP_CSUM, gd::INODE_BITMAP_CSUM_HI);
                if if ds >= 64 { want != have } else { want & 0xFFFF != have } {
                    c.corrupt(format!("group {}'s inode bitmap fails its checksum", g));
                }
            }
            for k in 0..ipg as u64 {
                inode_bit[(g * ipg as u64 + k) as usize] = ib[(k / 8) as usize] >> (k % 8) & 1 != 0;
            }
        }
    }
    /* An inode is looked at below its group's unused tail, as e2fsck looks. */
    let readable = |fs: &Fs, n: u32| {
        let g = ((n - 1) / ipg) as u64;
        let f = flags(fs, g);
        let unused = if uninit { fs.wide16(fs.desc(g), gd::ITABLE_UNUSED, gd::ITABLE_UNUSED_HI) } else { 0 };
        f & gd::INODE_UNINIT == 0 && (n - 1) % ipg < ipg.saturating_sub(unused)
    };

    /* The orphans, released first: each in use; with no links deleted --
     * its blocks and its inode then free -- and with links truncated to its
     * size. */
    let mut deleted: BTreeSet<u32> = BTreeSet::new();
    let mut truncated: BTreeMap<u32, u64> = BTreeMap::new();
    let mut next = rd32(&s, sb::LAST_ORPHAN);
    while next != 0 {
        if next > inodes || !inode_bit[next as usize - 1] || c.orphans > inodes || deleted.contains(&next)
            || truncated.contains_key(&next)
        {
            c.corrupt(format!("the orphan list names inode {}, which is no orphan", next));
            break;
        }
        c.orphans += 1;
        let i = fs.inode(next);
        let size = rd32(&i, ino::SIZE) as u64 | (rd32(&i, ino::SIZE_HIGH) as u64) << 32;
        if rd16(&i, ino::LINKS) == 0 {
            deleted.insert(next);
        } else {
            truncated.insert(next, size.div_ceil(bs));
        }
        next = rd32(&i, ino::DTIME);
    }

    /* Pass 1: every inode in use. */
    let mut in_use: BTreeMap<u32, InUse> = BTreeMap::new();
    let mut freed: BTreeSet<u64> = BTreeSet::new();
    let mut xattr_refs: BTreeMap<u64, u32> = BTreeMap::new();
    for n in 1..=inodes {
        if !readable(&fs, n) {
            continue;
        }
        let i = fs.inode(n);
        let used = inode_bit[n as usize - 1];
        let zero = i[..128].iter().all(|&b| b == 0);
        if !used {
            if !zero && rd16(&i, ino::MODE) != 0 && rd32(&i, ino::DTIME) == 0 && n >= FIRST_INO {
                c.unclean(format!("inode {} is free and holds a mode with no deletion time", n));
            }
            continue;
        }
        if zero {
            if n >= FIRST_INO || n == ROOT_INO {
                c.corrupt(format!("inode {} is in use and empty", n));
            }
            continue;
        }
        if csum {
            let x = inode_csum(seed, n, &i);
            let lo = rd16(&i, ino::CHECKSUM_LO) as u32;
            let fits = isz > 128 && 128 + rd16(&i, ino::EXTRA_ISIZE) as usize >= ino::CHECKSUM_HI + 2;
            let have = if fits { lo | (rd16(&i, ino::CHECKSUM_HI) as u32) << 16 } else { lo };
            if if fits { x != have } else { x & 0xFFFF != have } {
                c.corrupt(format!("inode {} fails its checksum", n));
                continue;
            }
        }
        if deleted.contains(&n) {
            /* Its blocks go back with it: whatever they are, freed. */
            let mut data = BTreeMap::new();
            let mut tb = 0;
            let seed_i = fs.inode_seed(n, &i);
            let mut scratch = Check::default();
            if rd32(&i, ino::FLAGS) & ino::EXTENTS_FL != 0 {
                fs.extents(n, seed_i, &i[ino::BLOCK..ino::BLOCK + 60], None, (0, 1 << 32), 0, &mut data, &mut tb,
                           &mut freed, &mut scratch);
            }
            continue;
        }
        let mode = rd16(&i, ino::MODE);
        let size = rd32(&i, ino::SIZE) as u64 | (rd32(&i, ino::SIZE_HIGH) as u64) << 32;
        let flags_i = rd32(&i, ino::FLAGS);
        let mut data = BTreeMap::new();
        let mut tree_blocks = 0u64;
        let kind = mode & S_IFMT;
        /* The reserved inodes mke2fs gives times and no type, and the
         * resize inode, whose tree of reserved descriptor blocks e2fsck
         * judges on its own: only the block at its top is its own. */
        if n < FIRST_INO && n != ROOT_INO && n != JOURNAL_INO {
            if n == 7 && resize {
                let dind = rd32(&i, ino::BLOCK + 4 * 13) as u64;
                if dind != 0 {
                    fs.claim(n, dind, "resize", &mut c);
                }
            }
            continue;
        }
        let fast_link = kind == S_IFLNK && flags_i & ino::EXTENTS_FL == 0 && size < 60;
        if matches!(kind, S_IFREG | S_IFDIR | S_IFLNK) && !fast_link {
            let keep = truncated.get(&n).copied().unwrap_or(u64::MAX);
            if flags_i & ino::EXTENTS_FL != 0 {
                let seed_i = fs.inode_seed(n, &i);
                fs.extents(n, seed_i, &i[ino::BLOCK..ino::BLOCK + 60], None, (0, 1 << 32), keep, &mut data,
                           &mut tree_blocks, &mut freed, &mut c);
            } else {
                fs.block_map(n, &i, &mut data, &mut tree_blocks, &mut c);
            }
        } else if !matches!(kind, S_IFREG | S_IFDIR | S_IFLNK | S_IFCHR | 0x6000 | 0x1000 | 0xC000) {
            c.unclean(format!("inode {} has an unknown mode {:#o}", n, mode));
        }
        let acl = rd32(&i, ino::FILE_ACL) as u64 | (rd16(&i, 0x76) as u64) << 32;
        let mut extra = 0;
        if acl != 0 {
            if !xattr_refs.contains_key(&acl) {
                if !fs.claim(n, acl, "attribute", &mut c) {
                    continue;
                }
                let blk = fs.block(acl);
                if rd32(&blk, 0) != 0xEA02_0000 {
                    c.corrupt(format!("inode {}: attribute block {} without its magic", n, acl));
                } else if csum {
                    let mut x = crc32c(seed, &acl.to_le_bytes());
                    x = crc32c(x, &blk[..0x10]);
                    x = crc32c(x, &[0, 0, 0, 0]);
                    x = crc32c(x, &blk[0x14..]);
                    if x != rd32(&blk, 0x10) {
                        c.corrupt(format!("attribute block {} fails its checksum", acl));
                    }
                }
            }
            *xattr_refs.entry(acl).or_default() += 1;
            extra = 1;
        }
        let units = (data.len() as u64 + tree_blocks + extra) * (bs / 512);
        let have = rd32(&i, ino::BLOCKS) as u64 | (rd16(&i, ino::BLOCKS_HIGH) as u64) << 32;
        /* An orphan's truncation, finished as e2fsck finishes it, counts its
         * blocks again as it goes. */
        if have != units && !truncated.contains_key(&n) {
            c.unclean(format!("inode {} counts {} sectors, has {}", n, have, units));
        }
        let in_eof = size.div_ceil(bs);
        if kind == S_IFREG {
            if let Some((&l, _)) = data.iter().next_back() {
                if l >= in_eof {
                    c.unclean(format!("inode {}: a block at {} past its end", n, l));
                }
            }
        }
        if kind == S_IFDIR && size % bs != 0 {
            c.unclean(format!("directory {} is {} bytes, not whole blocks", n, size));
        }
        let links = rd16(&i, ino::LINKS);
        in_use.insert(n, InUse { mode, size, links, flags: flags_i, data });
    }
    for (&b, &refs) in &xattr_refs {
        let blk = fs.block(b);
        if rd32(&blk, 4) != refs {
            c.unclean(format!("attribute block {} counts {} references, has {}", b, rd32(&blk, 4), refs));
        }
    }

    /* Pass 2: directories. */
    let mut refs: BTreeMap<u32, u32> = BTreeMap::new();
    let mut children: BTreeMap<u32, Vec<(Vec<u8>, u32, u8)>> = BTreeMap::new();
    let mut parent_of: BTreeMap<u32, u32> = BTreeMap::new();
    let mut dotdot: BTreeMap<u32, u32> = BTreeMap::new();
    let dirs: Vec<u32> = in_use.iter().filter(|(_, u)| u.mode & S_IFMT == S_IFDIR).map(|(&n, _)| n).collect();
    for &n in &dirs {
        let (size, flags_i, data) = {
            let u = &in_use[&n];
            (u.size, u.flags, u.data.clone())
        };
        let i = fs.inode(n);
        let seed_i = fs.inode_seed(n, &i);
        let indexed = flags_i & 0x1000 != 0;
        let mut names = BTreeSet::new();
        let mut list = Vec::new();
        for l in 0..size / bs {
            let Some(&(b, _)) = data.get(&l) else {
                c.unclean(format!("directory {} has a hole at block {}", n, l));
                continue;
            };
            let blk = fs.block(b);
            let spans = rd32(&blk, 0) == 0 && rd16(&blk, 4) as u64 == bs;
            if indexed && (l == 0 || spans) {
                if csum && !dx_ok(seed_i, &blk) {
                    c.corrupt(format!("directory {}'s index block {} fails its checksum", n, l));
                }
                if l != 0 {
                    continue;
                }
            } else if csum {
                let at = bs as usize - 12;
                let tail = rd32(&blk, at) == 0 && rd16(&blk, at + 4) == 12 && blk[at + 6] == 0 && blk[at + 7] == FT_DIR_CSUM;
                if !tail || crc32c(seed_i, &blk[..at]) != rd32(&blk, at + 8) {
                    c.corrupt(format!("directory {}'s block {} fails its checksum", n, l));
                    continue;
                }
            }
            let mut at = 0usize;
            let mut k = 0;
            while at + 8 <= bs as usize {
                let (child, rec, nl, ft) = (rd32(&blk, at), rd16(&blk, at + 4) as usize, blk[at + 6] as usize, blk[at + 7]);
                if rec < 8 || rec % 4 != 0 || at + rec > bs as usize || 8 + nl > rec {
                    c.corrupt(format!("directory {} block {}: an entry at {} does not parse", n, l, at));
                    break;
                }
                let name = blk[at + 8..at + 8 + nl].to_vec();
                if l == 0 && k == 0 && (name != b"." || child != n) {
                    c.corrupt(format!("directory {} does not begin with its '.'", n));
                }
                if l == 0 && k == 1 {
                    if name != b".." {
                        c.corrupt(format!("directory {} has no '..' second", n));
                    } else {
                        dotdot.insert(n, child);
                    }
                }
                if indexed && l == 0 && k == 1 {
                    break;
                }
                if child != 0 && !(l == 0 && k < 2) && ft != FT_DIR_CSUM {
                    if nl == 0 || name.contains(&b'/') || name.contains(&0) {
                        c.corrupt(format!("directory {} names something with an impossible name", n));
                    } else if child > inodes || !in_use.contains_key(&child) {
                        c.corrupt(format!("directory {}'s '{}' leads to inode {}, which is not in use", n,
                                          crate::model::show(&name), child));
                    } else {
                        let cm = in_use[&child].mode & S_IFMT;
                        let want_ft = match cm {
                            S_IFREG => FT_REG,
                            S_IFDIR => FT_DIR,
                            S_IFLNK => FT_SYMLINK,
                            S_IFCHR => FT_CHRDEV,
                            0x6000 => 4,
                            0x1000 => 5,
                            0xC000 => 6,
                            _ => 0,
                        };
                        if ft != want_ft {
                            c.unclean(format!("directory {}'s '{}' of type {} names a {:#o}", n, crate::model::show(&name), ft,
                                              cm));
                        }
                        if !names.insert(name.clone()) {
                            c.unclean(format!("directory {} names '{}' twice", n, crate::model::show(&name)));
                        }
                        *refs.entry(child).or_default() += 1;
                        if cm == S_IFDIR {
                            if parent_of.insert(child, n).is_some() {
                                c.unclean(format!("directory {} is named in two directories", child));
                            }
                        }
                        list.push((name, child, ft));
                    }
                }
                at += rec;
                k += 1;
            }
        }
        children.insert(n, list);
    }

    /* Pass 3: every directory reachable from the root, its '..' its
     * parent. */
    let mut reach = BTreeSet::from([ROOT_INO]);
    let mut stack = vec![ROOT_INO];
    while let Some(d) = stack.pop() {
        for (_, ch, _) in children.get(&d).into_iter().flatten() {
            if in_use.get(ch).is_some_and(|u| u.mode & S_IFMT == S_IFDIR) && reach.insert(*ch) {
                stack.push(*ch);
            }
        }
    }
    for &d in &dirs {
        if !reach.contains(&d) {
            c.unclean(format!("directory {} is unreachable", d));
        }
        let want = if d == ROOT_INO { ROOT_INO } else { parent_of.get(&d).copied().unwrap_or(0) };
        if dotdot.get(&d).copied() != Some(want) && want != 0 {
            c.unclean(format!("directory {}'s '..' is not its parent {}", d, want));
        }
    }

    /* Pass 4: link counts. */
    for (&n, u) in &in_use {
        if n < FIRST_INO && n != ROOT_INO {
            continue;
        }
        let names = refs.get(&n).copied().unwrap_or(0);
        let want = if u.mode & S_IFMT == S_IFDIR {
            let subdirs = children.get(&n).map(|l| l.iter().filter(|e| in_use.get(&e.1).is_some_and(|x| x.mode & S_IFMT == S_IFDIR)).count()).unwrap_or(0) as u32;
            let from_parent = if n == ROOT_INO { 1 } else { names };
            from_parent + 1 + subdirs
        } else {
            names
        };
        if names == 0 && n != ROOT_INO {
            c.unclean(format!("inode {} is in use and named nowhere", n));
        } else if u.links as u32 != want && !(u.links == 1 && want > 65000) {
            c.unclean(format!("inode {} counts {} links, has {}", n, u.links, want));
        }
    }

    /* Pass 5: the bitmaps against what owns each block and inode, and the
     * groups' counts. */
    for b in first..blocks {
        let owned = fs.owner[b as usize] != Owner::Free;
        let bit = block_bit[b as usize];
        if owned && !bit {
            c.corrupt(format!("block {} is in use and free in its bitmap", b));
        } else if !owned && bit && !freed.contains(&b) {
            c.unclean(format!("block {} is marked in use and is nobody's", b));
        }
    }
    for g in 0..groups {
        let d = fs.desc(g).to_vec();
        let start = first + g * bpg as u64;
        let len = (bpg as u64).min(blocks - start);
        /* As the counts stood before the orphans were released: what
         * e2fsck puts right as it releases them, it says nothing of. */
        let free_b = (start..start + len)
            .filter(|&b| fs.owner[b as usize] == Owner::Free && !freed.contains(&b)).count() as u32;
        let free_i = (0..ipg).filter(|&k| {
            let n = (g * ipg as u64 + k as u64 + 1) as u32;
            !in_use.contains_key(&n) && !deleted.contains(&n) && !(n < FIRST_INO && inode_bit[n as usize - 1])
        }).count() as u32;
        let dirs_g = (0..ipg).filter(|&k| {
            in_use.get(&((g * ipg as u64 + k as u64 + 1) as u32)).is_some_and(|u| u.mode & S_IFMT == S_IFDIR)
        }).count() as u32;
        let f = flags(&fs, g);
        if f & gd::BLOCK_UNINIT != 0 && (start..start + len).any(|b| matches!(fs.owner[b as usize], Owner::Inode(_))) {
            c.corrupt(format!("group {} is left uninitialised and holds a file's blocks", g));
        }
        if fs.wide16(&d, gd::FREE_BLOCKS, gd::FREE_BLOCKS_HI) != free_b {
            c.unclean(format!("group {} counts {} free blocks, has {}", g, fs.wide16(&d, gd::FREE_BLOCKS, gd::FREE_BLOCKS_HI), free_b));
        }
        if fs.wide16(&d, gd::FREE_INODES, gd::FREE_INODES_HI) != free_i {
            c.unclean(format!("group {} counts {} free inodes, has {}", g, fs.wide16(&d, gd::FREE_INODES, gd::FREE_INODES_HI), free_i));
        }
        if fs.wide16(&d, gd::USED_DIRS, gd::USED_DIRS_HI) != dirs_g {
            c.unclean(format!("group {} counts {} directories, has {}", g, fs.wide16(&d, gd::USED_DIRS, gd::USED_DIRS_HI), dirs_g));
        }
    }
    c.no_metadata = (0..blocks).map(|b| b >= first && fs.owner[b as usize] == Owner::Free).collect();
    for (&n, u) in &in_use {
        if n >= FIRST_INO && u.mode & S_IFMT == S_IFREG {
            for &(b, _) in u.data.values() {
                if let Some(x) = c.no_metadata.get_mut(b as usize) {
                    *x = true;
                }
            }
        }
    }
    if c.corrupt.iter().any(|e| e.contains("root")) || !in_use.contains_key(&ROOT_INO) {
        c.corrupt("the root directory is not in use".into());
        return c;
    }
    c.tree = Some(read_tree(&fs, &in_use, &children, ROOT_INO, &mut BTreeSet::new()));
    c
}

/// An htree index block's checksum: from the directory's seed over the
/// block up to its last entry, then the tail's reserved word and a zero.
fn dx_ok(seed: u32, blk: &[u8]) -> bool {
    let bs = blk.len();
    let off = if rd16(blk, 4) as usize == bs { 8 } else if rd16(blk, 4) == 12 && rd16(blk, 16) as usize == bs - 12 { 32 } else { return false };
    let (limit, count) = (rd16(blk, off) as usize, rd16(blk, off + 2) as usize);
    let tail = off + 8 * limit;
    if tail + 8 > bs || count > limit {
        return false;
    }
    let mut x = crc32c(seed, &blk[..off + 8 * count]);
    x = crc32c(x, &blk[tail..tail + 4]);
    x = crc32c(x, &[0, 0, 0, 0]);
    x == rd32(blk, tail + 4)
}

/// The tree under directory `n`, as the kernel's would show it.
fn read_tree(fs: &Fs, in_use: &BTreeMap<u32, InUse>, children: &BTreeMap<u32, Vec<(Vec<u8>, u32, u8)>>, n: u32,
             seen: &mut BTreeSet<u32>) -> Node {
    let mut out = BTreeMap::new();
    if !seen.insert(n) {
        return Node::Dir(out);
    }
    for (name, ch, _) in children.get(&n).into_iter().flatten() {
        if name.len() >= 64 {
            continue;
        }
        let Some(u) = in_use.get(ch) else { continue };
        match u.mode & S_IFMT {
            S_IFDIR => {
                out.insert(name.clone(), read_tree(fs, in_use, children, *ch, seen));
            }
            S_IFREG => {
                let mut d = Data::new();
                for (&l, &(b, uw)) in &u.data {
                    if uw || l * fs.bs >= u.size {
                        continue;
                    }
                    let n = (u.size - l * fs.bs).min(fs.bs);
                    d.write(l * fs.bs, &fs.img.bytes(b * fs.bs, n as usize));
                }
                d.truncate(u.size);
                out.insert(name.clone(), Node::File(d));
            }
            _ => {}
        }
    }
    Node::Dir(out)
}
