//! extfmt on the host: the format crate's views and checksums held to the
//! filesystems e2fsprogs makes, and walked over them damaged.
//!
//!     extfmt-test crc
//!
//! holds CRC32C and CRC16 to known answers (RFC 3720's, CRC-16/ARC's) and
//! to carrying on from one call to the next.
//!
//!     extfmt-test verify <image> [--regions]
//!
//! walks a filesystem's metadata the way a driver does -- superblock, every
//! group's descriptor and bitmaps, every inode in use, every extent block
//! of its tree, every block of every directory, every extended attribute
//! block -- and checks every checksum it carries against the one extfmt
//! computes: a line per kind counted, `MISMATCH` for a checksum that is
//! not what it should be, `ERROR` for metadata that does not parse. With
//! --regions, the byte ranges each checksum covers, for a test to damage.
//! Exit 0 when everything matched and parsed, 1 for a mismatch, 2 for an
//! error.
//!
//!     extfmt-test fuzz <image> [--seed N] [--iterations N]
//!
//! walks the same image with 1 to 8 of its metadata's bytes changed at a
//! time, each run a different image, and holds the code to never panicking
//! and always ending: what a disk says is the disk's to choose.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use extfmt::crc::{crc16, crc32c};
use extfmt::group::{self, Desc, BLOCK_UNINIT, INODE_UNINIT};
use extfmt::inode::{self, Inode, EXTENTS_FL, INDEX_FL, INLINE_DATA_FL};
use extfmt::sb::Super;
use extfmt::{blockmap, dir, extent, xattr, Map};

/// The most blocks of a directory walked, and extent nodes of a file: what
/// keeps a damaged size or tree from making a walk take hours.
const MAX_DIR_BLOCKS: u64 = 4096;
const MAX_EXTENT_NODES: u32 = 4096;

/// The iteration a fuzz run is at, for the panic hook to name.
static ITERATION: AtomicU64 = AtomicU64::new(0);

/// `len` bytes of the image at `offset`, if they are all inside it.
fn get(img: &[u8], offset: u64, len: usize) -> Option<&[u8]> {
    let start = usize::try_from(offset).ok()?;
    img.get(start..start.checked_add(len)?)
}

#[derive(Default)]
struct Report {
    counts: BTreeMap<&'static str, u64>,
    mismatches: Vec<String>,
    errors: Vec<String>,
    /// What each checksum covers: kind, offset, length.
    regions: BTreeSet<(u64, usize, &'static str)>,
    /// What the walk read of the metadata, checksummed or not: where a
    /// fuzz run aims its damage.
    walked: BTreeSet<(u64, usize)>,
    /// Extended attribute blocks seen, which inodes share.
    xattr_seen: BTreeSet<u64>,
}

impl Report {
    fn count(&mut self, kind: &'static str) {
        *self.counts.entry(kind).or_default() += 1;
    }
    fn mismatch(&mut self, what: String) {
        if self.mismatches.len() < 64 {
            self.mismatches.push(what);
        }
    }
    fn error(&mut self, what: String) {
        if self.errors.len() < 64 {
            self.errors.push(what);
        }
    }
    fn region(&mut self, kind: &'static str, offset: u64, len: usize) {
        self.regions.insert((offset, len, kind));
    }
    fn walk(&mut self, offset: u64, len: usize) {
        self.walked.insert((offset, len));
    }
}

/// What one walk needs of the filesystem.
struct Fs<'a> {
    img: &'a [u8],
    sb: Super,
    bs: usize,
    seed: u32,
    csum: bool,
}

impl Fs<'_> {
    fn block(&self, b: u64) -> Option<&[u8]> {
        if b >= self.sb.blocks_count() {
            return None;
        }
        get(self.img, b.checked_mul(self.bs as u64)?, self.bs)
    }
}

fn walk(img: &[u8]) -> Report {
    let mut r = Report::default();
    let Some(raw) = get(img, extfmt::sb::OFFSET, extfmt::sb::SIZE) else {
        r.error(String::from("an image too short for a superblock"));
        return r;
    };
    let sb = match Super::parse(raw) {
        Ok(sb) => sb,
        Err(e) => {
            r.error(e);
            return r;
        }
    };
    let fs = Fs { img, bs: sb.block_size() as usize, seed: sb.csum_seed(), csum: sb.metadata_csum(), sb };
    if fs.csum {
        r.count("superblock");
        if !fs.sb.checksum_ok() {
            r.mismatch(String::from("the superblock"));
        }
        r.region("superblock", extfmt::sb::OFFSET, extfmt::sb::SIZE);
    }
    let ds = fs.sb.desc_size() as usize;
    let gdt = (u64::from(fs.sb.first_data_block()) + 1) * fs.bs as u64;
    let ipg = u64::from(fs.sb.inodes_per_group());
    let isz = fs.sb.inode_size() as usize;
    let uninit = fs.csum || fs.sb.gdt_csum();
    for g in 0..fs.sb.group_count() {
        let at = gdt + g * ds as u64;
        let Some(d) = get(img, at, ds) else {
            r.error(format!("group {}: its descriptor past the image", g));
            break;
        };
        let desc = Desc::new(d).expect("a descriptor is at least 32 bytes");
        r.walk(at, ds);
        if let Some(c) = group::desc_csum(&fs.sb, g as u32, d) {
            r.count("descriptor");
            if c != desc.checksum() {
                r.mismatch(format!("group {}'s descriptor", g));
            }
            r.region("descriptor", at, ds);
        }
        let flags = if uninit { desc.flags() } else { 0 };
        if fs.csum && flags & BLOCK_UNINIT == 0 {
            check_bitmap(&fs, &mut r, g, "block bitmap", desc.block_bitmap(), desc.block_bitmap_csum(),
                         group::block_bitmap_csum, fs.sb.clusters_per_group());
        }
        if flags & INODE_UNINIT != 0 {
            continue;
        }
        if fs.csum {
            check_bitmap(&fs, &mut r, g, "inode bitmap", desc.inode_bitmap(), desc.inode_bitmap_csum(),
                         group::inode_bitmap_csum, fs.sb.inodes_per_group());
        }
        let Some(ibm) = fs.block(desc.inode_bitmap()) else {
            r.error(format!("group {}: its inode bitmap outside the filesystem", g));
            continue;
        };
        let used = if uninit { ipg.saturating_sub(u64::from(desc.itable_unused())) } else { ipg };
        for i in 0..used.min(ipg) {
            let Some(&byte) = ibm.get((i / 8) as usize) else { break };
            if byte >> (i % 8) & 1 == 0 {
                continue;
            }
            let ino = g * ipg + i + 1;
            if ino > u64::from(fs.sb.inodes_count()) {
                break;
            }
            let at = desc.inode_table().checked_mul(fs.bs as u64).and_then(|t| t.checked_add(i * isz as u64));
            let Some(raw) = at.and_then(|at| get(img, at, isz)) else {
                r.error(format!("inode {} outside the image", ino));
                continue;
            };
            check_inode(&fs, &mut r, ino as u32, raw, at.unwrap_or(0));
        }
    }
    r
}

fn check_bitmap(fs: &Fs, r: &mut Report, g: u64, kind: &'static str, block: u64, stored: (u32, u32),
                compute: fn(&Super, &[u8]) -> Option<u32>, bits: u32) {
    let Some(bitmap) = fs.block(block) else {
        r.error(format!("group {}'s {} outside the filesystem", g, kind));
        return;
    };
    r.count(kind);
    match compute(&fs.sb, bitmap) {
        Some(c) if group::csum_matches(stored.0, stored.1, c) => {}
        Some(_) => r.mismatch(format!("group {}'s {}", g, kind)),
        None => r.error(format!("group {}'s {} of more bits than its block", g, kind)),
    }
    r.region(kind, block * fs.bs as u64, (bits / 8) as usize);
}

fn check_inode(fs: &Fs, r: &mut Report, ino: u32, raw: &[u8], at: u64) {
    /* An inode all zeros is taken as sound, as e2fsck takes it. */
    if raw[..inode::GOOD_OLD_SIZE].iter().all(|&b| b == 0) {
        r.count("zeroed inode");
        return;
    }
    r.walk(at, raw.len());
    if fs.csum {
        r.count("inode");
        if !inode::csum_matches(fs.seed, ino, raw) {
            r.mismatch(format!("inode {}", ino));
        }
        r.region("inode", at, raw.len());
    }
    let node = Inode::new(raw).expect("an inode is at least 128 bytes");
    if node.links_count() == 0 {
        return;
    }
    let iseed = inode::seed(fs.seed, ino, node.generation());
    let flags = node.flags();
    if flags & EXTENTS_FL != 0 && flags & INLINE_DATA_FL == 0 {
        let mut budget = MAX_EXTENT_NODES;
        check_extents(fs, r, ino, iseed, node.i_block(), None, &mut budget);
    }
    if node.is_dir() && flags & INLINE_DATA_FL == 0 {
        check_dir(fs, r, ino, iseed, &node);
    }
    let acl = node.file_acl();
    if acl != 0 && r.xattr_seen.insert(acl) {
        match fs.block(acl) {
            None => r.error(format!("inode {}: an attribute block {} outside the filesystem", ino, acl)),
            Some(blk) if xattr::magic(blk) != xattr::MAGIC => {
                r.error(format!("inode {}: an attribute block {} without the magic", ino, acl))
            }
            Some(blk) => {
                r.walk(acl * fs.bs as u64, fs.bs);
                if fs.csum {
                    r.count("xattr block");
                    if xattr::block_csum(fs.seed, acl, blk) != Some(xattr::stored_csum(blk)) {
                        r.mismatch(format!("attribute block {}", acl));
                    }
                    r.region("xattr block", acl * fs.bs as u64, fs.bs);
                }
            }
        }
    }
}

fn check_extents(fs: &Fs, r: &mut Report, ino: u32, iseed: u32, node: &[u8], depth_wanted: Option<u16>,
                 budget: &mut u32) {
    let h = match extent::header(node) {
        Ok(h) => h,
        Err(e) => return r.error(format!("inode {}: {}", ino, e)),
    };
    if depth_wanted.is_some_and(|d| d != h.depth) {
        return r.error(format!("inode {}: an extent node of depth {} under one of {}", ino, h.depth,
                               depth_wanted.unwrap_or(0) + 1));
    }
    if h.depth == 0 {
        return;
    }
    for i in 0..usize::from(h.entries) {
        if *budget == 0 {
            return r.error(format!("inode {}: more than {} extent nodes", ino, MAX_EXTENT_NODES));
        }
        *budget -= 1;
        let e = extent::index(node, i);
        let Some(child) = fs.block(e.child) else {
            r.error(format!("inode {}: an extent node at block {} outside the filesystem", ino, e.child));
            continue;
        };
        r.walk(e.child * fs.bs as u64, fs.bs);
        if fs.csum {
            r.count("extent block");
            match (extent::stored_csum(child), extent::node_csum(iseed, child)) {
                (Some(s), Some(c)) if s == c => {}
                (Some(_), Some(_)) => r.mismatch(format!("inode {}'s extent block {}", ino, e.child)),
                _ => r.error(format!("inode {}: an extent block {} with no room for its checksum", ino, e.child)),
            }
            if let Ok(ch) = extent::header(child) {
                r.region("extent block", e.child * fs.bs as u64, extent::tail_offset(ch.max) + extent::TAIL);
            }
        }
        check_extents(fs, r, ino, iseed, child, Some(h.depth - 1), budget);
    }
}

fn check_dir(fs: &Fs, r: &mut Report, ino: u32, iseed: u32, node: &Inode) {
    let bs = fs.bs as u64;
    let blocks = node.size(fs.sb.largedir()).div_ceil(bs).min(MAX_DIR_BLOCKS);
    let indexed = node.flags() & INDEX_FL != 0;
    let extents = node.flags() & EXTENTS_FL != 0;
    let fs_blocks = fs.sb.blocks_count();
    let mut read = |b: u64, buf: &mut [u8]| match fs.block(b) {
        Some(src) => {
            buf.copy_from_slice(src);
            Ok(())
        }
        None => Err(format!("block {} outside the filesystem", b)),
    };
    let mut lblock = 0;
    while lblock < blocks {
        let map = if extents {
            extent::map(node.i_block(), lblock, fs_blocks, fs.bs, ino, &mut read)
        } else {
            blockmap::map(node.i_block(), lblock, fs.bs, ino, &mut read)
        };
        match map {
            Err(e) => return r.error(e),
            Ok(Map::Hole { run }) => lblock = lblock.saturating_add(run.max(1)),
            Ok(Map::Blocks { block, run }) => {
                for k in 0..run.min(blocks - lblock) {
                    check_dir_block(fs, r, ino, iseed, indexed, lblock + k, block + k);
                }
                lblock = lblock.saturating_add(run.max(1));
            }
        }
    }
}

fn check_dir_block(fs: &Fs, r: &mut Report, ino: u32, iseed: u32, indexed: bool, lblock: u64, block: u64) {
    let Some(blk) = fs.block(block) else {
        return r.error(format!("directory {}: block {} outside the filesystem", ino, block));
    };
    r.walk(block * fs.bs as u64, fs.bs);
    let spans = extfmt::le32(blk, 0) == 0 && dir::rec_len_from_disk(extfmt::le16(blk, 4), fs.bs) == fs.bs;
    if indexed && (lblock == 0 || spans) {
        if fs.csum {
            r.count("dx block");
            match dir::dx_csum(iseed, blk) {
                Some((s, c)) if s == c => {}
                Some(_) => r.mismatch(format!("directory {}'s index block {}", ino, block)),
                None => r.error(format!("directory {}: an index block {} without room for its tail", ino, block)),
            }
            if let Some(at) = dir::dx_count_offset(blk) {
                let (_, count) = dir::dx_count_limit(blk, at);
                r.region("dx block", block * fs.bs as u64, (at + usize::from(count) * dir::DX_ENTRY).min(fs.bs));
            }
        }
    } else if fs.csum {
        r.count("dir block");
        match dir::tail(blk) {
            Some(s) if s == dir::leaf_csum(iseed, blk) => {}
            Some(_) => r.mismatch(format!("directory {}'s block {}", ino, block)),
            None => r.mismatch(format!("directory {}'s block {} has no checksum tail", ino, block)),
        }
        r.region("dir block", block * fs.bs as u64, fs.bs);
    }
    for e in dir::entries(blk, ino) {
        if let Err(e) = e {
            r.error(e);
            break;
        }
    }
}

fn crc() -> i32 {
    let mut failed = 0;
    let mut check = |what: &str, ok: bool| {
        println!("{}: {}", if ok { "PASS" } else { "FAIL" }, what);
        failed += i32::from(!ok);
    };
    let c32 = |data: &[u8]| !crc32c(!0, data);
    check("CRC32C of \"123456789\" is 0xE3069283", c32(b"123456789") == 0xE306_9283);
    /* RFC 3720, appendix B.4. */
    check("CRC32C of 32 zero bytes is 0x8A9136AA", c32(&[0u8; 32]) == 0x8A91_36AA);
    check("CRC32C of 32 0xFF bytes is 0x62A8AB43", c32(&[0xFFu8; 32]) == 0x62A8_AB43);
    let up: Vec<u8> = (0u8..32).collect();
    let down: Vec<u8> = (0u8..32).rev().collect();
    check("CRC32C of 0..31 is 0x46DD794E", c32(&up) == 0x46DD_794E);
    check("CRC32C of 31..0 is 0x113FDB5C", c32(&down) == 0x113F_DB5C);
    check("CRC32C carries on from one call to the next",
          crc32c(crc32c(!0, b"1234"), b"56789") == crc32c(!0, b"123456789"));
    check("CRC16 of \"123456789\" is 0xBB3D (CRC-16/ARC)", crc16(0, b"123456789") == 0xBB3D);
    check("CRC16 carries on from one call to the next",
          crc16(crc16(!0, b"1234"), b"56789") == crc16(!0, b"123456789"));
    failed
}

fn verify(path: &str, regions: bool) -> i32 {
    let img = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}: {}", path, e);
            return 2;
        }
    };
    let r = walk(&img);
    for (kind, n) in &r.counts {
        println!("count {} {}", kind.replace(' ', "_"), n);
    }
    for m in &r.mismatches {
        println!("MISMATCH {}", m);
    }
    for e in &r.errors {
        println!("ERROR {}", e);
    }
    if regions {
        for (offset, len, kind) in &r.regions {
            println!("region {} {} {}", kind.replace(' ', "_"), offset, len);
        }
    }
    if !r.mismatches.is_empty() {
        1
    } else if !r.errors.is_empty() {
        2
    } else {
        0
    }
}

/// xorshift64*: a sequence the seed decides.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

fn fuzz(path: &str, seed: u64, iterations: u64) -> i32 {
    let mut img = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}: {}", path, e);
            return 2;
        }
    };
    let clean = walk(&img);
    if !clean.mismatches.is_empty() || !clean.errors.is_empty() {
        eprintln!("{}: not sound before any damage", path);
        return 2;
    }
    let regions: Vec<(u64, usize)> = clean.regions.iter().map(|&(o, l, _)| (o, l)).chain(clean.walked.iter().copied())
        .filter(|&(_, l)| l > 0).collect();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("FINDING: seed {} iteration {}: {}", seed, ITERATION.load(Ordering::Relaxed), info);
    }));
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let head = img.len().min(1 << 20) as u64;
    let mut flagged = 0u64;
    for it in 0..iterations {
        ITERATION.store(it, Ordering::Relaxed);
        let mut undo = Vec::new();
        for _ in 0..1 + rng.below(8) {
            let at = if !regions.is_empty() && rng.below(10) < 8 {
                let (o, l) = regions[rng.below(regions.len() as u64) as usize];
                o + rng.below(l as u64)
            } else {
                rng.below(head)
            } as usize;
            let Some(&old) = img.get(at) else { continue };
            undo.push((at, old));
            img[at] = match rng.below(3) {
                0 => 0,
                1 => 0xFF,
                _ => rng.next() as u8,
            };
        }
        let r = walk(&img);
        flagged += u64::from(!r.mismatches.is_empty() || !r.errors.is_empty());
        for (at, old) in undo.into_iter().rev() {
            img[at] = old;
        }
    }
    println!("fuzz: {} damaged images walked, {} of them flagged, no panic", iterations, flagged);
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let code = match args.get(1).map(String::as_str) {
        Some("crc") => crc(),
        Some("verify") if args.len() >= 3 => verify(&args[2], args.iter().any(|a| a == "--regions")),
        Some("fuzz") if args.len() >= 3 => {
            let seed = flag("--seed").and_then(|s| s.parse().ok()).unwrap_or(1);
            let iterations = flag("--iterations").and_then(|s| s.parse().ok()).unwrap_or(2000);
            fuzz(&args[2], seed, iterations)
        }
        _ => {
            eprintln!("usage: extfmt-test crc | verify <image> [--regions] | fuzz <image> [--seed N] [--iterations N]");
            2
        }
    };
    std::process::exit(code);
}
