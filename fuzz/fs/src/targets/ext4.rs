//! ext4, on images the fuzzer's own mkfs makes from the format: every block
//! size, 32- and 64-byte descriptors, flex_bg or not, metadata_csum with
//! its own seed or the UUID's, groups left uninitialised with garbage where
//! nothing was written, files in extent trees of any depth with unwritten
//! extents over garbage, entries the kernel's tree does not show, and
//! journals holding transactions nobody replayed.
//!
//! `ext4mkfs` makes images and nothing else: with FS_FUZZ_DUMP set it
//! writes each out beside a manifest of its tree, for `fs-fuzz.py
//! --e2fsck` to hold to e2fsck and debugfs -- the judge held to the
//! format's own tools before any driver is held to the judge.

use std::collections::BTreeMap;

use crate::image::ext4::{self as img, Geometry, Orphan, Style};
use crate::machine::disk;
use crate::model::{Data, Node};
use crate::targets::fsops::{self, Fs, Limits, Model};
use crate::{reached, Input};

/// When the images are made: before the kernel's wall clock starts.
pub(super) const MKFS_TIME: u32 = 1_749_000_000;

/// True about one time in `n`, and false past the input's end, whose zeros
/// are the ordinary choice (`Input::chance` is in 256ths).
pub(super) fn one_in(r: &mut Input, n: u8) -> bool {
    r.u8() % n == 1
}

/// A geometry mke2fs could make, small enough to be quick.
pub fn geometry(r: &mut Input) -> Geometry {
    let log = r.below(3) as u32;
    let bs = 1024u64 << log;
    let isz: u16 = if r.u8() % 4 == 0 { 128 } else { 256 };
    let bpg: u32 = match r.u8() % 3 {
        0 => 8 * bs as u32,
        1 => 2048,
        _ => 1024 + 8 * r.below(64) as u32,
    }
    .min(8 * bs as u32);
    let per_block = (bs / isz as u64) as u32;
    let ipg = (per_block.max(8) * r.range(1, 8) as u32).min(8 * bs as u32);
    let groups = r.range(1, 6);
    let first = (log == 0) as u64;
    let blocks = (first + groups * bpg as u64 - if groups > 1 { r.below(bpg as u64 / 2) } else { 0 })
        .min((24 << 20) / bs);
    let mut label = [0u8; 16];
    label[..3].copy_from_slice(b"nos");
    let mut uuid = [0u8; 16];
    for b in uuid.iter_mut() {
        *b = r.u8();
    }
    let mut g = Geometry {
        log_block_size: log,
        blocks,
        blocks_per_group: bpg,
        inodes_per_group: ipg,
        inode_size: isz,
        bit64: r.bool(),
        log_groups_per_flex: r.pick(&[0, 1, 2, 4]),
        csum_seed: one_in(r, 4),
        journal_blocks: if one_in(r, 8) { 0 } else { 1024 },
        label,
        uuid,
    };
    while !g.valid() && g.groups() > 1 {
        g.blocks = first + (g.groups() - 1) * bpg as u64;
    }
    if !g.valid() {
        g.log_groups_per_flex = 0;
        g.blocks_per_group = 8 * bs as u32;
        g.inodes_per_group = per_block.max(8) * 4;
        g.blocks = (first + 4096).min(first + 8 * bs);
    }
    g
}

pub fn style(r: &mut Input) -> Style {
    Style {
        scatter: r.pick(&[0, 0, 3, 16]),
        fill: r.pick(&[0, 0, 0, 2, 3, 7]),
        unwritten: one_in(r, 3),
        uninit: r.bool(),
        garbage: one_in(r, 3),
        hidden: one_in(r, 3),
        slack: one_in(r, 3),
        htree: r.bool(),
        journal_csum: r.bool(),
        journal_64: one_in(r, 4),
        links: Vec::new(),
        journaled: 0,
        orphans: Vec::new(),
        leave_free: None,
    }
}

/// Every file under `n`, by path, and its size.
fn files(n: &Node, at: &mut Vec<Vec<u8>>, out: &mut Vec<(Vec<Vec<u8>>, u64)>) {
    for (name, child) in n.children().into_iter().flatten() {
        at.push(name.clone());
        match child {
            Node::File(d) => out.push((at.clone(), d.len())),
            Node::Dir(_) => files(child, at, out),
        }
        at.pop();
    }
}

/// Now and then orphans for `tree`'s image -- a file of no links named
/// nowhere, a file of one name cut short, none of the names `links` gives
/// a second -- and the tree a mount that releases them leaves, or a read
/// sees: `tree` with each cut made.
pub(super) fn orphans(r: &mut Input, tree: &Node, bs: u64, budget: &mut u64, links: &[(Vec<Vec<u8>>, Vec<Vec<u8>>)])
                      -> (Vec<Orphan>, Node) {
    let mut out = Vec::new();
    let mut want = tree.clone();
    if !one_in(r, 6) {
        return (out, want);
    }
    let mut all = Vec::new();
    files(tree, &mut Vec::new(), &mut all);
    all.retain(|(p, len)| *len > 0 && !links.iter().any(|(l, t)| l == p || t == p));
    for _ in 0..r.range(1, 4) {
        if all.is_empty() || r.bool() {
            out.push(Orphan::Deleted(content(r, bs, budget)));
            continue;
        }
        let (path, len) = all.swap_remove(r.below(all.len() as u64) as usize);
        let size = match r.u8() % 3 {
            0 => 0,
            1 => len - 1 - r.below(len.min(bs)),
            _ => r.below(len),
        };
        if let Some(Node::File(d)) = want.get_mut(&path) {
            d.truncate(size);
        }
        out.push(Orphan::Cut(path, size));
    }
    reached("ext4: an image with orphans");
    (out, want)
}

/// What a file holds: nothing, a little, a block either side of a few
/// edges, many small pieces apart -- each an extent of its own -- or a few
/// pieces far apart, past 4 GiB, with holes between.
fn content(r: &mut Input, bs: u64, budget: &mut u64) -> Data {
    let mut d = Data::new();
    match r.u8() % 8 {
        0 => {}
        1 => d = Data::from(&crate::input::noise(r.u32(), r.below(100) as usize)),
        2 => d = Data::from(&crate::input::noise(r.u32(), (bs * r.range(1, 6) + r.below(3) - 1) as usize)),
        3 | 4 => {
            /* Pieces a block apart: an extent each, enough to fill leaves. */
            let pieces = r.range(5, 600);
            for k in 0..pieces {
                d.write(k * 2 * bs, &crate::input::noise(r.u32(), bs as usize));
            }
        }
        5 => {
            let len = r.range(4, 6) << 30;
            for _ in 0..r.range(1, 4) {
                let at = r.below(len);
                let n = r.below(2 * bs).min(len - at);
                d.write(at, &crate::input::noise(r.u32(), n as usize));
            }
            d.truncate(len);
        }
        _ => d = Data::from(&crate::input::noise(r.u32(), r.below(8 * bs) as usize)),
    }
    let blocks = d.nonzero_blocks(bs).len() as u64 * 2 + 8;
    if blocks > *budget {
        return Data::new();
    }
    *budget -= blocks;
    d
}

fn name(r: &mut Input) -> Vec<u8> {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789-_";
    let len = r.range(1, 40) as usize;
    (0..len).map(|_| CHARS[r.below(CHARS.len() as u64) as usize]).collect()
}

/// A tree of directories and files within a budget of blocks and inodes.
pub fn tree(r: &mut Input, bs: u64, budget: &mut u64, inodes: &mut u64) -> Node {
    fn level(r: &mut Input, bs: u64, budget: &mut u64, inodes: &mut u64, depth: u32) -> Node {
        let mut children = BTreeMap::new();
        for _ in 0..r.below(if depth == 0 { 24 } else { 8 }) {
            if *inodes < 6 || *budget < 16 {
                break;
            }
            let n = name(r);
            if n == b"lost+found" || children.contains_key(&n) {
                continue;
            }
            *inodes -= 1;
            if depth < 4 && one_in(r, 4) {
                *budget -= 2;
                children.insert(n, level(r, bs, budget, inodes, depth + 1));
            } else {
                children.insert(n, Node::File(content(r, bs, budget)));
            }
        }
        Node::Dir(children)
    }
    level(r, bs, budget, inodes, 0)
}

/// Where a target's images go for a look with e2fsck: `FS_FUZZ_DUMP=<dir>`,
/// each with a manifest of what its tree holds -- a line a directory or a
/// file, a file's size and the FNV-1a of its bytes, holes as zeros.
pub fn dump(tag: &str, made: &img::Made, tree: &Node, bytes: u64) {
    let Some(dir) = std::env::var_os("FS_FUZZ_DUMP") else { return };
    let base = format!("{}/{}-{}-{}", dir.to_string_lossy(), tag, std::process::id(),
                       crate::machine::sched::now());
    write_sparse(&format!("{}.img", base), &made.media, bytes);
    if !made.journaled.is_empty() {
        /* And as the judge's replay leaves it, for e2fsck's to be compared
         * with. */
        let mut replayed = made.media.clone();
        if img::replay::replay(&mut replayed).is_ok() {
            write_sparse(&format!("{}.replayed", base), &replayed, bytes);
        }
    }
    let mut lines = Vec::new();
    fn walk(n: &Node, at: &mut Vec<u8>, lines: &mut Vec<String>) {
        for (name, child) in n.children().expect("a directory") {
            let len = at.len();
            at.push(b'/');
            at.extend_from_slice(name);
            let hex: String = at.iter().map(|b| format!("{:02x}", b)).collect();
            match child {
                Node::Dir(_) => {
                    lines.push(format!("d {}", hex));
                    walk(child, at, lines);
                }
                Node::File(d) => {
                    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                    for &b in d.to_vec().iter() {
                        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
                    }
                    lines.push(format!("f {} {} {:016x}", hex, d.len(), h));
                }
            }
            at.truncate(len);
        }
    }
    walk(tree, &mut Vec::new(), &mut lines);
    lines.push(format!("hidden {}", made.hidden));
    lines.push(format!("journaled {}", made.journaled.len()));
    let _ = std::fs::write(format!("{}.manifest", base), lines.join("\n") + "\n");
}

/// The judge's check of an image from outside -- one mke2fs made, damaged
/// or not -- for `fs-fuzz.py --e2fsck` to hold the judge to e2fsck by:
/// FS_FUZZ_IMAGE names it, and what the check found is printed. Without it
/// there is nothing to do; the input is not used.
pub fn ext4judge(_r: &mut Input) {
    let Some(path) = std::env::var_os("FS_FUZZ_IMAGE") else { return };
    let Ok(bytes) = std::fs::read(&path) else {
        println!("verdict unreadable");
        return;
    };
    let c = img::check(&crate::machine::disk::Media::from_bytes(&bytes), bytes.len() as u64);
    let verdict = if !c.corrupt.is_empty() { "corrupt" } else if !c.unclean.is_empty() { "unclean" } else { "clean" };
    fn count(n: &Node) -> (u64, u64) {
        match n {
            Node::File(_) => (1, 0),
            Node::Dir(c) => c.values().map(count).fold((0, 1), |a, b| (a.0 + b.0, a.1 + b.1)),
        }
    }
    let (files, dirs) = c.tree.as_ref().map(count).unwrap_or((0, 0));
    println!("verdict {} replayed {} orphans {} files {} dirs {}", verdict, c.replayed, c.orphans, files, dirs);
    for e in c.corrupt.iter().map(|e| ("corrupt", e)).chain(c.unclean.iter().map(|e| ("unclean", e))) {
        println!("{}: {}", e.0, e.1);
    }
}

/// A medium written out sparse: what was never written is zeros, and
/// costs nothing.
pub(super) fn write_sparse(path: &str, media: &crate::machine::disk::Media, bytes: u64) {
    use std::io::{Seek, SeekFrom, Write};
    let Ok(mut f) = std::fs::File::create(path) else { return };
    let all = media.to_vec(bytes);
    let _ = f.set_len(bytes);
    for (k, piece) in all.chunks(64 << 10).enumerate() {
        if piece.iter().any(|&b| b != 0) {
            let _ = f.seek(SeekFrom::Start(k as u64 * (64 << 10))).and_then(|_| f.write_all(piece));
        }
    }
}

/// Images made, from geometries, trees and styles the input chooses --
/// a journal with transactions nobody replayed now and then.
pub fn ext4mkfs(r: &mut Input) {
    let g = geometry(r);
    let mut st = style(r);
    if g.journal_blocks > 0 && one_in(r, 3) {
        st.journaled = r.range(1, 12) as u8;
    }
    let bs = g.block_size();
    let mut budget = g.blocks / 2;
    let mut inodes = g.inodes() / 2;
    /* Files a dump's reader can read whole: no far ones when dumping. */
    let tree = tree(r, bs, &mut budget, &mut inodes);
    let dumping = std::env::var_os("FS_FUZZ_DUMP").is_some();
    if dumping && too_big(&tree) {
        return;
    }
    match img::mkfs(&g, &tree, &st, MKFS_TIME, r.u64()) {
        Ok(made) => {
            reached("ext4mkfs: an image made");
            if !made.journaled.is_empty() {
                reached("ext4mkfs: its journal holding what its homes do not");
            }
            agree(&g, &st, &made, &tree);
            dump("ext4mkfs", &made, &tree, g.blocks * bs);
        }
        Err(_) => reached("ext4mkfs: a tree the geometry could not hold"),
    }
}

/// The judge's mkfs and its check, agreeing -- or every verdict it gives is
/// worth nothing: the image clean, its journal replayed if it holds
/// anything, and the tree mkfs was given, lost+found beside it.
pub(super) fn agree(g: &Geometry, st: &Style, made: &img::Made, tree: &Node) {
    let c = img::check(&made.media, g.blocks * g.block_size());
    invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the judge's mkfs made an image its check faults ({:?}, \
               {:?}): {:?}, {:?}", g, st, c.corrupt, c.unclean);
    invariant!((c.replayed > 0) == !made.journaled.is_empty(), "the check replayed {} transactions of a journal mkfs \
               planted {} blocks in", c.replayed, made.journaled.len());
    invariant!(c.orphans as usize == st.orphans.len(), "the check released {} orphans of the {} mkfs planted",
               c.orphans, st.orphans.len());
    let mut want = tree.clone();
    want.children_mut().expect("a directory").insert(b"lost+found".to_vec(), Node::dir());
    invariant!(c.tree.as_ref() == Some(&want), "the judge's check reads another tree than its mkfs made: {:?}",
               c.tree.as_ref().and_then(|t| want.diff(t)));
}

/// ext4 mounted read-only on images the fuzzer's mkfs made: a journal
/// nobody replayed among them now and then, its homes stale, which the
/// driver must read through. Every call's answer
/// the model's, whose tree is the one mkfs was given -- lost+found beside
/// it, and none of the entries the kernel's tree leaves out -- and nothing
/// written to the disk at all, as a read-only mount must not.
pub fn ext4(r: &mut Input) {
    let g = geometry(r);
    let mut st = style(r);
    if g.journal_blocks > 0 && one_in(r, 3) {
        st.journaled = r.range(1, 12) as u8;
    }
    let bs = g.block_size();
    let mut budget = g.blocks / 2;
    let mut inodes = g.inodes() / 2;
    let tree = tree(r, bs, &mut budget, &mut inodes);
    /* Orphans a read-only mount leaves as they are: a deleted file named
     * nowhere, a cut one read to its size. */
    let (planted, tree_seen) = orphans(r, &tree, bs, &mut budget, &[]);
    st.orphans = planted;
    let Ok(made) = img::mkfs(&g, &tree, &st, MKFS_TIME, r.u64()) else { return };
    let tree = tree_seen;
    agree(&g, &st, &made, &tree);
    let sizes: Vec<u64> = [512, 1024, 2048, 4096].into_iter().filter(|s| *s <= bs).collect();
    let ss = r.pick(&sizes);
    let sectors = g.blocks * bs / ss + r.below(8);
    let journaled = !made.journaled.is_empty();
    let mut d = disk::Disk::new("vda", ss, sectors, made.media);
    d.latency = r.chance(64);
    d.no_writes = Some("ext4 mounts read-only, and writes nothing".into());
    disk::insert(0, d);
    let Some(dev) = disk::register(0) else { return };
    let mounted = fs::ext4::mount_at("/", dev.handle(), true);
    invariant!(mounted == 1, "ext4 did not mount, read-only, an image of the fuzzer's mkfs ({:?}, {:?}): {}", g, st,
               mounted);
    reached(if journaled { "mounted, a journal not replayed read through" } else { "mounted" });
    let mut want = tree;
    want.children_mut().expect("a directory").insert(b"lost+found".to_vec(), Node::dir());
    let vfs = fs::vfs_instance().expect("the VFS");
    let mut f = Fs::new(vfs, Model::new(want, true, Limits::fixed(1 << 44, usize::MAX)), true);
    let mut ops = 0;
    while r.more() && ops < 120 {
        ops += 1;
        fsops::step(&mut f, r, bs);
    }
    f.compare();
    f.close_all();
    invariant!(vfs.unmount(b"/"), "the unmount at the end was refused");
}

/// ext4 on the fuzzer's images damaged: bytes of the superblock, the
/// descriptors, bitmaps and inode tables, and of anything else changed --
/// a journal nobody replayed among them now and then. Refused, or mounted
/// and walked -- every directory listed, every name looked up, every file
/// read -- three times, the third leaving nothing allocated the second did
/// not; and nothing written, nothing panicking, nothing read once it is
/// unmounted.
pub fn ext4bad(r: &mut Input) {
    let g = geometry(r);
    let mut st = style(r);
    if g.journal_blocks > 0 && one_in(r, 3) {
        st.journaled = r.range(1, 8) as u8;
    }
    let bs = g.block_size();
    let mut budget = 96;
    let mut inodes = 32;
    let tree = tree(r, bs, &mut budget, &mut inodes);
    let Ok(made) = img::mkfs(&g, &tree, &st, MKFS_TIME, r.u64()) else { return };
    let mut media = made.media;
    let len = g.blocks * bs;
    for _ in 0..r.range(1, 4) {
        /* The metadata at the start of the first groups, mostly; else
         * anywhere -- a directory's block, an extent block, the journal. */
        let at = if r.u8() % 5 < 3 { r.below(len.min(512 * bs)) } else { r.below(len) };
        let byte = match r.u8() % 3 {
            0 => 0,
            1 => 0xFF,
            _ => r.u8(),
        };
        media.write(at, &[byte]);
    }
    let sizes: Vec<u64> = [512, 1024, 4096].into_iter().filter(|s| *s <= bs).collect();
    let ss = r.pick(&sizes);
    let mut d = disk::Disk::new("vda", ss, len.div_ceil(ss) + r.below(4), media);
    d.latency = r.chance(32);
    d.no_writes = Some("ext4 mounts read-only, and writes nothing".into());
    disk::insert(0, d);
    let Some(dev) = disk::register(0) else { return };
    let _ = fs::extfs::driver_for(&dev);
    if fs::ext4::mount_at("/", dev.handle(), true) < 0 {
        reached("a damaged image refused");
        return;
    }
    reached("a damaged image mounted");
    let vfs = fs::vfs_instance().expect("the VFS");
    let mut f = Fs::new(vfs, Model::new(tree, true, Limits::fixed(1 << 44, usize::MAX)), false);
    let walk = crate::targets::ext2bad::walk;
    walk(&mut f, b"/", 0);
    walk(&mut f, b"/", 0);
    let settled = crate::machine::heap::live();
    walk(&mut f, b"/", 0);
    let after = crate::machine::heap::live();
    invariant!(after <= settled, "a walk of a damaged image that changed nothing left {} bytes more allocated than \
               the walk before it", after - settled);
    let mut ops = 0;
    while r.more() && ops < 100 {
        ops += 1;
        fsops::step(&mut f, r, bs);
    }
    walk(&mut f, b"/", 0);
    f.close_all();
    invariant!(vfs.unmount(b"/"), "the unmount of a damaged image was refused");
    let before = disk::with(0, |d| d.log.len());
    let _ = vfs.stat(b"/");
    invariant!(disk::with(0, |d| d.log.len()) == before, "the disk was read after its filesystem was unmounted");
}

pub(super) fn too_big(n: &Node) -> bool {
    match n {
        Node::File(d) => d.len() > 64 << 20,
        Node::Dir(c) => c.values().any(too_big),
    }
}
