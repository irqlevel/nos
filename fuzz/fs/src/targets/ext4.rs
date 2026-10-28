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

use crate::image::ext4::{self as img, Geometry, Style};
use crate::machine::disk;
use crate::model::{join, show, Data, Node};
use crate::targets::fsops::{self, Fs, Limits, Model};
use crate::{reached, Input};

/// When the images are made: before the kernel's wall clock starts.
const MKFS_TIME: u32 = 1_749_000_000;

/// True about one time in `n`, and false past the input's end, whose zeros
/// are the ordinary choice (`Input::chance` is in 256ths).
fn one_in(r: &mut Input, n: u8) -> bool {
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
        journaled: 0,
    }
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
fn write_sparse(path: &str, media: &crate::machine::disk::Media, bytes: u64) {
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
fn agree(g: &Geometry, st: &Style, made: &img::Made, tree: &Node) {
    let c = img::check(&made.media, g.blocks * g.block_size());
    invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the judge's mkfs made an image its check faults ({:?}, \
               {:?}): {:?}, {:?}", g, st, c.corrupt, c.unclean);
    invariant!((c.replayed > 0) == !made.journaled.is_empty(), "the check replayed {} transactions of a journal mkfs \
               planted {} blocks in", c.replayed, made.journaled.len());
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
    let Ok(made) = img::mkfs(&g, &tree, &st, MKFS_TIME, r.u64()) else { return };
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

fn too_big(n: &Node) -> bool {
    match n {
        Node::File(d) => d.len() > 64 << 20,
        Node::Dir(c) => c.values().any(too_big),
    }
}

/* ---- writing: names ---- */

/// A tree as a mount shows it, for holding one view to another: every path
/// -- a directory, or a file's size and the FNV-1a of its first 64 KiB.
type Shape = BTreeMap<Vec<Vec<u8>>, (bool, u64, u64)>;

/// How much of a file a shape hashes.
const SHAPE_BYTES: u64 = 64 << 10;

/// The requests a call is cut before, every one: more than a commit and a
/// checkpoint of the target's journals take.
const CUT_EVERY: u64 = 64;

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// A model's tree as a shape.
fn shape_of(n: &Node) -> Shape {
    fn walk(n: &Node, at: &mut Vec<Vec<u8>>, out: &mut Shape) {
        for (name, child) in n.children().expect("a directory") {
            at.push(name.clone());
            match child {
                Node::Dir(_) => {
                    out.insert(at.clone(), (true, 0, 0));
                    walk(child, at, out);
                }
                Node::File(d) => {
                    out.insert(at.clone(), (false, d.len(), fnv(&d.read(0, SHAPE_BYTES))));
                }
            }
            at.pop();
        }
    }
    let mut out = Shape::new();
    walk(n, &mut Vec::new(), &mut out);
    out
}

/// What the VFS shows of the tree mounted at "/", as a shape -- or why it
/// cannot be read.
fn shape_read(vfs: &fs::vfs::Vfs) -> Result<Shape, String> {
    let mut out = Shape::new();
    let mut stack: Vec<Vec<Vec<u8>>> = vec![Vec::new()];
    while let Some(at) = stack.pop() {
        let p = if at.is_empty() { b"/".to_vec() } else { join(&at) };
        let mut index = 0;
        loop {
            let e = match vfs.dir_entry(&p, index) {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(_) => return Err(format!("{} does not list", show(&p))),
            };
            index += 1;
            invariant!(index < 100_000, "the directory {} lists past {} entries", show(&p), index);
            let mut child = at.clone();
            child.push(e.name().to_vec());
            if e.kind == fs::vnode::Kind::Dir {
                out.insert(child.clone(), (true, 0, 0));
                stack.push(child);
                continue;
            }
            let path = join(&child);
            let Some(file) = fs::vfs::Open::new(vfs, &path, fs::vfs::OPEN_READ) else {
                return Err(format!("{} does not open", show(&path)));
            };
            let size = file.size() as u64;
            let mut buf = vec![0u8; size.min(SHAPE_BYTES) as usize];
            let mut k = 0;
            while k < buf.len() {
                match file.read(&mut buf[k..]) {
                    Some(0) | None => break,
                    Some(n) => k += n,
                }
            }
            if k != buf.len() {
                return Err(format!("{} reads {} bytes of {}", show(&path), k, buf.len()));
            }
            out.insert(child, (false, size, fnv(&buf)));
        }
    }
    Ok(out)
}

fn first_difference(a: &Shape, b: &Shape) -> Option<String> {
    for (k, v) in a {
        match b.get(k) {
            None => return Some(format!("{} only in the first", show(&join(k)))),
            Some(w) if w != v => return Some(format!("{}: {:?} and {:?}", show(&join(k)), v, w)),
            _ => {}
        }
    }
    b.keys().find(|k| !a.contains_key(*k)).map(|k| format!("{} only in the second", show(&join(k))))
}

/// Every path of `map` at or under `at`, moved to under `to`.
fn move_under<V>(map: &mut BTreeMap<Vec<Vec<u8>>, V>, at: &[Vec<u8>], to: &[Vec<u8>]) {
    let moved: Vec<Vec<Vec<u8>>> = map.keys().filter(|k| k.starts_with(at)).cloned().collect();
    for k in moved {
        let v = map.remove(&k).expect("listed");
        let mut n = to.to_vec();
        n.extend_from_slice(&k[at.len()..]);
        map.insert(n, v);
    }
}

/// One call of a run, as the disk saw it: the requests it made, the shape
/// before and after, and -- for a remove, which goes a node at a time --
/// what it was taking away.
struct Call {
    start: u64,
    end: u64,
    before: Shape,
    after: Shape,
    removing: Option<Vec<Vec<u8>>>,
}

/// Whether `got` is what a crash in the middle of `call` may leave: the
/// shape before it or after it -- or, for a remove, the shape before with
/// some of what is at and under its path gone.
fn may_leave(call: &Call, got: &Shape) -> bool {
    if *got == call.before || *got == call.after {
        return true;
    }
    let Some(p) = &call.removing else { return false };
    got.iter().all(|(k, v)| call.before.get(k) == Some(v))
        && call.before.keys().filter(|k| !got.contains_key(*k)).all(|k| k.starts_with(p))
}

/// What a run keeps beside the model: the shape of the model's tree --
/// kept with it, a file's content never changing here, so that its hash
/// moves with its name -- and the entries the tree leaves out, by
/// directory.
struct Names {
    shape: Shape,
    hidden: BTreeMap<Vec<Vec<u8>>, Vec<Vec<u8>>>,
    /// A remove that failed with requests failing, which may have taken
    /// any part of what it was removing: what the image is then held to.
    uncertain: Option<Call>,
}

impl Names {
    /// Whether `name` in `dir` is an entry the disk has and the tree does
    /// not show.
    fn blocked(&self, dir: &[Vec<u8>], name: &[u8]) -> bool {
        self.hidden.get(dir).is_some_and(|h| h.iter().any(|n| n == name))
    }

    /// Whether anything at or under `p` holds entries the tree leaves out.
    fn holds_hidden(&self, p: &[Vec<u8>]) -> bool {
        self.hidden.keys().any(|k| k.starts_with(p))
    }
}

/// `f` on the model, kept only if `got`: what the call did, the model's
/// answer.
fn model_if(f: &mut Fs, got: bool, call: impl FnOnce(&mut Model) -> bool) -> bool {
    let saved = f.model.tree.clone();
    let want = call(&mut f.model);
    if !got {
        f.model.tree = saved;
    }
    want
}

/// One call that changes names, or looks at them, the input's choice: each
/// answer held to the model's where `strict`, and never a success the model
/// does not have. Its requests and shapes go into `calls`.
fn names_step(f: &mut Fs, names: &mut Names, r: &mut Input, strict: bool, calls: &mut Vec<Call>) {
    use fsops::Res;
    let vfs = f.vfs;
    let start = disk::with(0, |d| d.log.len() as u64);
    let before = names.shape.clone();
    let mut removing = None;
    /* A call the kernel refused for want of room -- an inode, a block --
     * is one the model, which counts neither, has succeed. */
    let check = |got: bool, want: bool, mark: u64, what: &str| {
        invariant!(!got || want, "{} succeeded, and the model does not have it", what);
        if strict && !got && want && fsops::out_of_room(mark) {
            reached("ext4rw: a call refused for want of room");
            return;
        }
        if strict {
            invariant!(got == want, "{} failed, and the model has it", what);
        }
    };
    match r.u8() % 16 {
        0..=3 => {
            let p = match names.hidden.iter().nth(r.below(names.hidden.len().max(1) as u64) as usize) {
                /* Now and then a name only the disk has, in its directory. */
                Some((d, h)) if r.chance(32) && !h.is_empty() => {
                    let mut k = d.clone();
                    k.push(h[r.below(h.len() as u64) as usize].clone());
                    join(&k)
                }
                _ => fsops::path(r, &f.model),
            };
            let dir = r.bool();
            let res = f.model.resolve(&p);
            let blocked = matches!(&res, Res::Absent(d, n) if names.blocked(d, n));
            fsops::say(format_args!("create {} dir {}", show(&p), dir));
            let mark = crate::machine::trace_mark();
            let got = vfs.create(&p, dir);
            let want = !blocked && model_if(f, got, |m| m.create(&p, dir));
            check(got, want, mark, &format!("create {}", show(&p)));
            if got && want {
                if let Res::Absent(d, n) = res {
                    let mut k = d;
                    k.push(n);
                    names.shape.insert(k, if dir { (true, 0, 0) } else { (false, 0, fnv(&[])) });
                }
                reached(if dir { "ext4rw: a directory made" } else { "ext4rw: a file made" });
                if crate::machine::traced_since(mark).iter().any(|l| l.windows(13).any(|w| w == b"index dropped")) {
                    reached("ext4rw: an htree directory's index dropped");
                }
            }
            if blocked {
                reached("ext4rw: a name the disk has and the tree does not, refused");
            }
        }
        4 | 5 => {
            let p = fsops::path(r, &f.model);
            let res = f.model.resolve(&p);
            let blocked = matches!(&res, Res::Node(n) if names.holds_hidden(n));
            if let Res::Node(n) = &res {
                if !n.is_empty() {
                    removing = Some(n.clone());
                }
            }
            fsops::say(format_args!("remove {}", show(&p)));
            let mark = crate::machine::trace_mark();
            let got = vfs.remove(&p);
            let want = !blocked && model_if(f, got, |m| m.remove(&p));
            if let (false, true, false, Res::Node(_)) = (strict, want, got, &res) {
                /* Part of it may be gone, a node at a time, and with
                 * requests failing nothing says which: the run ends here,
                 * and the image is held to what such a remove may leave. */
                names.uncertain = Some(Call { start, end: start, before: before.clone(), after: before.clone(),
                                              removing: removing.clone() });
                reached("ext4rw: a remove that failed, perhaps part way");
            } else {
                check(got, want, mark, &format!("remove {}", show(&p)));
            }
            if got && want {
                if let Res::Node(n) = &res {
                    names.shape.retain(|k, _| !k.starts_with(n));
                }
                reached("ext4rw: something removed");
            }
            if blocked {
                reached("ext4rw: a directory holding what the tree does not show, kept");
            }
        }
        6..=8 => {
            let a = fsops::path(r, &f.model);
            let b = fsops::path(r, &f.model);
            let (src, dst) = (f.model.resolve(&a), f.model.resolve(&b));
            let blocked = matches!(&dst, Res::Absent(d, n) if names.blocked(d, n));
            fsops::say(format_args!("rename {} to {}", show(&a), show(&b)));
            let mark = crate::machine::trace_mark();
            let got = vfs.rename(&a, &b);
            let want = !blocked && model_if(f, got, |m| m.rename(&a, &b));
            check(got, want, mark, &format!("rename {} to {}", show(&a), show(&b)));
            if got && want {
                if let (Res::Node(s), Res::Absent(d, n)) = (src, dst) {
                    let mut to = d;
                    to.push(n);
                    move_under(&mut names.shape, &s, &to);
                    move_under(&mut names.hidden, &s, &to);
                    reached("ext4rw: something renamed");
                }
            }
        }
        9 => {
            /* A directory of many entries: it grows a block at a time, its
             * extents among the other directories'. */
            let p = fsops::path(r, &f.model);
            if let Res::Node(d) = f.model.resolve(&p) {
                if matches!(f.model.node(&d), Some(Node::Dir(_))) {
                    let n = r.range(8, 120);
                    let tag = r.u16();
                    for k in 0..n {
                        /* Each its own call: a crash between two leaves
                         * the ones before it made. */
                        let start = disk::with(0, |d| d.log.len() as u64);
                        let before = names.shape.clone();
                        let mut c = d.clone();
                        c.push(format!("entry-{:04x}-{:03}-with-a-longer-name", tag, k).into_bytes());
                        let path = join(&c);
                        let dir = k % 7 == 0;
                        let blocked = names.blocked(&d, c.last().expect("named"));
                        let mark = crate::machine::trace_mark();
                        let got = vfs.create(&path, dir);
                        let want = !blocked && model_if(f, got, |m| m.create(&path, dir));
                        check(got, want, mark, &format!("create {}", show(&path)));
                        if got && want {
                            names.shape.insert(c, if dir { (true, 0, 0) } else { (false, 0, fnv(&[])) });
                        }
                        let end = disk::with(0, |d| d.log.len() as u64);
                        calls.push(Call { start, end, before, after: names.shape.clone(), removing: None });
                    }
                    reached("ext4rw: a directory grown by many entries");
                    return;
                }
            }
        }
        10 => {
            let p = fsops::path(r, &f.model);
            f.stat(&p);
        }
        11 if strict => {
            let p = fsops::path(r, &f.model);
            f.list(&p);
        }
        12 if f.model.open.len() < 8 => {
            /* Something open: what keeps it from being renamed or removed. */
            let p = fsops::path(r, &f.model);
            if matches!(f.model.resolve(&p), Res::Node(_)) {
                f.open(&p, fs::vfs::OPEN_READ);
            }
        }
        13 => {
            let i = r.below(f.model.open.len().max(1) as u64) as usize;
            f.close(i);
        }
        14 => {
            let ok = vfs.sync();
            invariant!(ok || !strict, "sync failed");
        }
        _ if strict => f.compare(),
        _ => {}
    }
    let end = disk::with(0, |d| d.log.len() as u64);
    calls.push(Call { start, end, before, after: names.shape.clone(), removing });
}

/// What the power going at a request leaves, held to what it may: the
/// picture mounted read-only -- the journal read through, nothing written --
/// and then for writing -- the journal replayed -- showing the same, and
/// the image its clean unmount leaves clean to the judge, its tree what the
/// kernel showed. The shape the kernel showed.
fn judge_picture(vfs: &'static fs::vfs::Vfs, handle: usize, snap: disk::Media, dev_bytes: u64) -> Shape {
    if let Some(dir) = std::env::var_os("FS_FUZZ_DUMP") {
        /* For e2fsck, which replays it with its own replay. */
        let base = format!("{}/ext4rw-crash-{}-{}", dir.to_string_lossy(), std::process::id(), crate::machine::sched::now());
        write_sparse(&format!("{}.img", base), &snap, dev_bytes);
    }
    /* The judge's own replay of the picture first: a reader of the journal
     * that shares nothing with the kernel's. */
    let judged = img::check(&snap, dev_bytes);
    invariant!(judged.corrupt.is_empty() && judged.unclean.is_empty(), "a power cut's picture, replayed by the judge, \
               is not clean: {:?} {:?}", judged.corrupt, judged.unclean);
    disk::with(0, |d| {
        d.current = snap.clone();
        d.durable = snap;
        d.cache.clear();
        d.volatile = false;
        d.crash_at.clear();
        d.fail.clear();
        d.dead_from = None;
        d.no_writes = Some("its ext4 is mounted read-only".into());
    });
    let ro = fs::ext4::mount_at("/", handle, true);
    invariant!(ro == 1, "a power cut's picture does not mount read-only: {}", ro);
    let seen_ro = shape_read(vfs).unwrap_or_else(|e| panic!("invariant: a power cut's picture, read-only: {}", e));
    invariant!(vfs.unmount(b"/"), "the read-only picture's unmount was refused");
    disk::with(0, |d| d.no_writes = None);
    let rw = fs::ext4::mount_at("/", handle, false);
    invariant!(rw == 0, "a power cut's picture does not mount for writing: {}", rw);
    let seen = shape_read(vfs).unwrap_or_else(|e| panic!("invariant: a power cut's picture, replayed: {}", e));
    invariant!(vfs.unmount(b"/"), "the replayed picture's unmount was refused");
    invariant!(seen == seen_ro, "a power cut's picture read through its journal is not what its replay shows: {:?}",
               first_difference(&seen_ro, &seen));
    disk::with(0, |d| d.no_writes = Some("its ext4 is unmounted".into()));
    let media = disk::with(0, |d| d.current.clone());
    let c = img::check(&media, dev_bytes);
    invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "a power cut's picture, replayed and unmounted, is not \
               clean: {:?} {:?}", c.corrupt, c.unclean);
    invariant!(!c.mounted && c.replayed == 0, "a power cut's picture, replayed and unmounted, is left mounted ({}) or \
               with {} transactions to replay", c.mounted, c.replayed);
    if let Some(t) = &c.tree {
        invariant!(shape_of(t) == seen, "the judge reads another tree than the kernel showed: {:?}",
                   first_difference(&seen, &shape_of(t)));
    }
    if let Some(t) = &judged.tree {
        invariant!(shape_of(t) == seen, "the judge's replay of a picture leaves another tree than the kernel's: {:?}",
                   first_difference(&seen, &shape_of(t)));
    }
    seen
}

/// ext4 mounted for writing on images the fuzzer's mkfs made -- a journal
/// with transactions nobody replayed among them now and then, which the
/// mount replays -- and worked with what the driver writes so far: names.
/// Files and directories made, renamed and removed, every answer the
/// model's; unmounted and mounted again midway; the image a clean unmount
/// leaves clean to the judge, and the model's tree. Now and then the power
/// cut at chosen requests, each picture held to what the call in flight may
/// leave; now and then requests that fail, and then no success the model
/// does not have, nothing written in a mount after its first write failed,
/// and the image, replayed, the model's.
pub fn ext4rw(r: &mut Input) {
    let mut g = geometry(r);
    if g.journal_blocks == 0 {
        g.journal_blocks = 1024;
        if !g.valid() {
            return;
        }
    }
    let mut st = style(r);
    if one_in(r, 3) {
        st.journaled = r.range(1, 12) as u8;
    }
    let bs = g.block_size();
    let mut budget = g.blocks / 3;
    let mut inodes = g.inodes() / 3;
    let tree = tree(r, bs, &mut budget, &mut inodes);
    let Ok(made) = img::mkfs(&g, &tree, &st, MKFS_TIME, r.u64()) else { return };
    agree(&g, &st, &made, &tree);
    let sizes: Vec<u64> = [512, 1024, 2048, 4096].into_iter().filter(|s| *s <= bs).collect();
    let ss = r.pick(&sizes);
    let sectors = g.blocks * bs / ss + r.below(8);
    let dev_bytes = sectors * ss;
    let mut d = disk::Disk::new("vda", ss, sectors, made.media.clone());
    d.seed_chaos(r.u64());
    d.volatile = r.u8() % 4 != 0;
    d.latency = r.chance(64);
    let faults = r.chance(24);
    if faults {
        for _ in 0..r.range(1, 3) {
            d.fail.insert(r.below(3000));
        }
        if r.chance(64) {
            d.dead_from = Some(r.below(3000));
        }
    } else if d.volatile {
        for _ in 0..r.range(1, 4) {
            d.crash_at.insert(r.below(3000));
        }
    }
    disk::insert(0, d);
    let Some(dev) = disk::register(0) else { return };
    let handle = dev.handle();
    let mounted = fs::ext4::mount_at("/", handle, false);
    if faults && mounted != 0 {
        reached("ext4rw: a mount that met a failing request, refused");
        return;
    }
    invariant!(mounted == 0, "ext4 did not mount for writing an image of the fuzzer's mkfs ({:?}, {:?}): {}", g, st,
               mounted);
    reached(if made.journaled.is_empty() { "ext4rw: mounted for writing" } else { "ext4rw: mounted, its journal replayed" });
    let mut want = tree;
    want.children_mut().expect("a directory").insert(b"lost+found".to_vec(), Node::dir());
    let vfs = fs::vfs_instance().expect("the VFS");
    let mut names = Names { shape: shape_of(&want), hidden: made.hidden_at.clone(), uncertain: None };
    let initial = names.shape.clone();
    let mut f = Fs::new(vfs, Model::new(want, false, Limits::fixed(1 << 44, usize::MAX)), !faults);
    let mut calls: Vec<Call> = Vec::new();
    /* Where each mount begins among the disk's requests: a failed write
     * stops what the rest of its mount writes. */
    let mut sessions: Vec<u64> = vec![0];
    let mut ops = 0;
    let mut live = true;
    let mut cut_every = false;
    while r.more() && ops < 200 && live && names.uncertain.is_none() {
        ops += 1;
        if r.chance(10) {
            let busy = f.busy();
            let start = disk::with(0, |d| d.log.len() as u64);
            let before = names.shape.clone();
            let done = vfs.unmount(b"/");
            invariant!(done != busy, "an unmount with {} files open {}", f.model.open.len(),
                       if done { "went through" } else { "was refused" });
            if done {
                sessions.push(disk::with(0, |d| d.log.len() as u64));
                let again = fs::ext4::mount_at("/", handle, false);
                if faults && again != 0 {
                    live = false;
                } else {
                    invariant!(again == 0, "ext4 mounted again as {}", again);
                    reached("ext4rw: unmounted and mounted again");
                }
            }
            let end = disk::with(0, |d| d.log.len() as u64);
            calls.push(Call { start, end, before: before.clone(), after: before, removing: None });
            continue;
        }
        if !faults && !cut_every && disk::with(0, |d| d.volatile) && r.chance(12) {
            /* The power cut before every request of the next call: every
             * point of its commit -- the copies, the flush, the commit
             * block, a checkpoint -- judged. */
            disk::with(0, |d| {
                let n = d.log.len() as u64;
                d.crash_at.extend(n..n + CUT_EVERY);
            });
            cut_every = true;
            reached("ext4rw: the power cut before every request of a call");
        }
        names_step(&mut f, &mut names, r, !faults, &mut calls);
    }
    if live {
        if !faults {
            f.compare();
            let seen = shape_read(vfs).unwrap_or_else(|e| panic!("invariant: the tree at the end: {}", e));
            invariant!(seen == names.shape, "the tree at the end is not the model's: {:?}",
                       first_difference(&names.shape, &seen));
        }
        f.close_all();
        invariant!(vfs.unmount(b"/"), "the unmount at the end was refused");
    }
    if faults {
        /* In each mount, nothing written after its first write failed. */
        let log: Vec<disk::Io> = disk::with(0, |d| d.log.clone());
        sessions.push(log.len() as u64);
        let writes = |io: &disk::Io| io.op != disk::Op::Read;
        for w in sessions.windows(2) {
            let (from, to) = (w[0] as usize, (w[1] as usize).min(log.len()));
            if let Some(k) = (from..to).find(|&k| writes(&log[k]) && !log[k].ok) {
                let after = (k + 1..to).find(|&j| writes(&log[j]) && log[j].ok);
                invariant!(after.is_none(), "request {} was written after request {}, a write, failed",
                           after.unwrap_or(0), k);
                reached("ext4rw: a write failed, and nothing written after it");
            }
        }
        /* What the failures left, replayed: the model's. */
        let media = disk::with(0, |d| d.durable.clone());
        let seen = judge_picture(vfs, handle, media, dev_bytes);
        match &names.uncertain {
            Some(c) => invariant!(may_leave(c, &seen), "the image after a remove failed, replayed, is not what it may \
                                  leave: {:?}", first_difference(&c.before, &seen)),
            None => invariant!(seen == names.shape, "the image after requests failed, replayed, is not the model's: \
                               {:?}", first_difference(&names.shape, &seen)),
        }
        reached("ext4rw: an image judged after requests failed");
        return;
    }
    /* The image a clean unmount leaves: clean, nothing to replay, the
     * model's tree. */
    disk::with(0, |d| d.no_writes = Some("its ext4 is unmounted".into()));
    let media = disk::with(0, |d| d.current.clone());
    let c = img::check(&media, dev_bytes);
    invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the image after a clean unmount is not clean: {:?} {:?}",
               c.corrupt, c.unclean);
    invariant!(!c.mounted && c.replayed == 0, "the image after a clean unmount is left mounted ({}) or with {} \
               transactions to replay", c.mounted, c.replayed);
    if let Some(t) = &c.tree {
        if let Some(diff) = f.model.tree.diff(t) {
            panic!("invariant: the image after a clean unmount holds another tree than the model's: {}", diff);
        }
    }
    if !too_big(&f.model.tree) {
        /* For e2fsck, and for debugfs to read every file out of whole: not
         * one of gigabytes. */
        let written = img::Made { media, inodes: BTreeMap::new(), hidden: made.hidden, hidden_at: BTreeMap::new(),
                                  journaled: Vec::new() };
        dump("ext4rw", &written, &f.model.tree, g.blocks * bs);
    }
    /* Every power cut: what the call in flight may leave. */
    let crashes = disk::with(0, |d| std::mem::take(&mut d.crashes));
    for (n, snap) in crashes {
        let seen = judge_picture(vfs, handle, snap, dev_bytes);
        let during = calls.iter().find(|c| c.start <= n && n < c.end);
        let ok = match during {
            Some(c) => may_leave(c, &seen),
            None => seen == *calls.iter().filter(|c| c.end <= n).last().map_or(&initial, |c| &c.after),
        };
        invariant!(ok, "the power going before request {} leaves a tree no call could: {:?}", n,
                   during.map(|c| first_difference(&c.before, &seen)));
        reached("ext4rw: a power cut judged");
    }
}
