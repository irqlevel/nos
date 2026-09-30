//! ext2, on images the format allows: every block size, one group or many,
//! small inodes and big, a superblock copy in every group or in the sparse
//! few, files with holes and without, directories with gaps between their
//! entries -- made by the fuzzer's own mkfs and populated with a tree --
//! mounted, worked through the VFS, and held to three things: every call's
//! answer is the model's; the image a clean unmount leaves is one e2fsck
//! passes, holding the model's tree, and mounts again to show it; and what
//! the power going at any point leaves is at worst unclean, never corrupt
//! -- which is what the driver's commit order promises.

use std::collections::BTreeMap;

use crate::image::ext2::{self as img, Geometry, Style};
use crate::machine::disk::{self, Media};
use crate::model::{Data, Node};
use crate::targets::fsops::{self, Fs, Limits, Model, NAME_MAX};
use crate::{reached, Input};

/// When the images are made: before the kernel's wall clock starts.
const MKFS_TIME: u32 = 1_749_000_000;

/// Where a target puts each image it judges, for a look with e2fsck:
/// `FS_FUZZ_DUMP=<dir>`.
pub fn dump(tag: &str, media: &Media, bytes: u64) {
    if let Some(dir) = std::env::var_os("FS_FUZZ_DUMP") {
        let name = format!("{}/{}-{}-{}.img", dir.to_string_lossy(), tag, std::process::id(),
                           crate::machine::sched::now());
        let _ = std::fs::write(name, media.to_vec(bytes));
    }
}

/// A geometry mke2fs could make: `valid()`, or the smallest sound one.
pub fn geometry(r: &mut Input) -> Geometry {
    let log = r.below(3) as u32;
    let bs = 1024u32 << log;
    let isz: u16 = if r.u8() % 3 == 0 { 256 } else { 128 };
    let bpg = match r.u8() % 4 {
        0 => 8 * bs,
        1 => 256,
        2 => 512 + 8 * r.below(64) as u32,
        _ => 1024,
    };
    let per_block = bs / isz as u32;
    let ipg = (per_block.max(8) * r.range(1, 16) as u32).min(8 * bs);
    let groups = r.range(1, 5) as u32;
    let first = (log == 0) as u32;
    let mut blocks = first + groups * bpg - if groups > 1 { r.below(bpg as u64 / 2) as u32 } else { 0 };
    /* Small enough to be quick: a few megabytes. */
    let cap = (16u32 << 20) / bs;
    if blocks > cap {
        blocks = cap;
    }
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
        sparse_super: r.u8() % 4 != 0,
        large_file: r.bool(),
        dir_index: r.chance(64),
        ro_compat_extra: 0,
        reserved_blocks: r.pick(&[0, 0, 5, 50]),
        label,
        uuid,
    };
    /* A group too short for its own metadata: drop it. */
    while !g.valid() && g.groups() > 1 {
        g.blocks = first + (g.groups() - 1) * bpg;
    }
    if !g.valid() {
        g.blocks_per_group = 8 * bs;
        g.inodes_per_group = per_block.max(8) * 4;
        g.blocks = first + 2048.min(8 * bs);
    }
    g
}

/// The biggest file the driver maps: past its double indirect block there
/// is nothing, and ext2 without the large-file feature ends at 4 GiB.
pub fn max_file(bs: u64) -> u64 {
    let ppb = bs / 4;
    ((12 + ppb + ppb * ppb) * bs).min(u32::MAX as u64)
}

/// A name for the image's own tree: none the format forbids.
fn tree_name(r: &mut Input) -> Vec<u8> {
    loop {
        let n = fsops::name(r);
        if !n.is_empty() && n.len() < NAME_MAX && !n.contains(&0) && n != b"." && n != b".." && n != b"lost+found" {
            return n;
        }
    }
}

/// What a file of the image's holds: nothing, a little, a block's worth
/// either side of an edge -- the direct blocks' end, the indirect block's
/// -- or a few pieces far apart, with holes between.
fn content(r: &mut Input, bs: u64, budget: &mut u64) -> Data {
    let ppb = bs / 4;
    let len = match r.u8() % 8 {
        0 => 0,
        1 => r.below(100),
        2 => bs + r.below(3) - 1,
        3 => 12 * bs + r.below(3) - 1,
        4 => (12 + ppb) * bs + r.below(3) - 1,
        _ => r.below(8 * bs),
    };
    let mut d = Data::new();
    if r.u8() % 4 == 0 && len > 3 * bs {
        /* Sparse: a few pieces, the rest holes. */
        for _ in 0..r.range(1, 4) {
            let at = r.below(len);
            let n = r.below(bs * 2).min(len - at);
            d.write(at, &crate::input::noise(r.u32(), n as usize));
        }
        d.truncate(len);
    } else {
        d = Data::from(&crate::input::noise(r.u32(), len as usize));
    }
    let blocks = d.nonzero_blocks(bs).len() as u64 + 2;
    if blocks > *budget {
        return Data::new();
    }
    *budget -= blocks;
    d
}

fn dir(r: &mut Input, depth: u32, bs: u64, budget: &mut u64, inodes: &mut u64) -> Node {
    let mut c = BTreeMap::new();
    for _ in 0..r.below(6) {
        if *inodes == 0 {
            break;
        }
        *inodes -= 1;
        let name = tree_name(r);
        let node = if depth < 4 && r.u8() % 3 == 0 && *budget > 4 {
            *budget -= 2;
            dir(r, depth + 1, bs, budget, inodes)
        } else {
            Node::File(content(r, bs, budget))
        };
        c.insert(name, node);
    }
    Node::Dir(c)
}

/// A tree for an image: directories four deep at most, files of every
/// shape, within `budget` blocks and `inodes` inodes.
pub fn tree(r: &mut Input, bs: u64, budget: &mut u64, inodes: &mut u64) -> Node {
    dir(r, 0, bs, budget, inodes)
}

/// The sector sizes a device under an ext2 of this block size may have.
fn sector_size(r: &mut Input, bs: u64) -> u64 {
    let sizes: Vec<u64> = [512, 1024, 2048, 4096].into_iter().filter(|s| *s <= bs).collect();
    r.pick(&sizes)
}

pub fn ext2(r: &mut Input) {
    let g = geometry(r);
    let bs = g.block_size() as u64;
    let data_blocks = (0..g.groups()).map(|k| {
        let l = g.layout(k);
        (l.start + l.len - l.data) as u64
    }).sum::<u64>();
    let mut budget = data_blocks / 2;
    let mut inodes = (g.inodes() as u64).saturating_sub(img::FIRST_INO as u64 + 2) / 2;
    let tree = dir(r, 0, bs, &mut budget, &mut inodes);
    let style = Style { holes: r.bool(), slack: r.bool(), scatter: r.pick(&[0, 0, 4, 16]) };
    let made = match img::mkfs(&g, &tree, &style, MKFS_TIME, r.u64()) {
        Ok(made) => made,
        Err(_) => return,
    };
    let ss = sector_size(r, bs);
    let sectors = g.blocks as u64 * bs / ss + r.below(8);
    let dev_bytes = sectors * ss;

    /* The fuzzer's mkfs and its checker, agreeing: or every verdict below is
     * worth nothing. */
    let mut want = tree.clone();
    want.children_mut().expect("a directory").insert(b"lost+found".to_vec(), Node::dir());
    let c = img::check(&made.media, dev_bytes);
    invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the fuzzer's mkfs made an image its checker \
               faults ({:?}): {:?}, {:?}", g, c.corrupt, c.unclean);
    invariant!(c.tree.as_ref() == Some(&want), "the fuzzer's mkfs made an image of another tree: {:?}",
               c.tree.as_ref().and_then(|t| want.diff(t)));
    dump("mkfs", &made.media, dev_bytes);

    let mut d = disk::Disk::new("vda", ss, sectors, made.media);
    d.seed_chaos(r.u64());
    d.volatile = r.u8() % 4 != 0;
    d.latency = r.chance(64);
    if d.volatile {
        for _ in 0..r.below(4) {
            d.crash_at.insert(r.below(3000));
        }
    }
    /* Now and then requests that fail: the answers are then nobody's to
     * predict, and the image is held to what a power cut is -- at worst
     * unclean. */
    let faults = r.chance(24);
    if faults {
        for _ in 0..r.range(1, 3) {
            d.fail.insert(r.below(800));
        }
    }
    disk::insert(0, d);
    let Some(dev) = disk::register(0) else { return };

    let ro_features = r.chance(16);
    let ro_asked = r.chance(32);
    if ro_features {
        /* A feature the driver does not keep up: read-only whatever was
         * asked. */
        disk::with(0, |d| {
            let mut s = d.current.bytes(img::SB_OFFSET, 1024);
            let f = img::rd32(&s, img::sb::FEATURE_RO_COMPAT) | 0x0010;
            img::wr32(&mut s, img::sb::FEATURE_RO_COMPAT, f);
            d.current.write(img::SB_OFFSET, &s);
            d.durable.write(img::SB_OFFSET, &s);
        });
    }
    let read_only = ro_asked || ro_features;
    if read_only {
        disk::with(0, |d| d.no_writes = Some("its ext2 is mounted read-only".into()));
    }
    let mounted = fs::ext2::mount_at("/", dev.handle(), ro_asked);
    if faults && mounted < 0 {
        return;
    }
    /* A superblock that would not write is read-only too: only when
     * requests fail. */
    let read_only = read_only || (faults && mounted == 1);
    invariant!(mounted == read_only as i32, "ext2 mounted as {} ({} asked, read-only features {})", mounted,
               ro_asked, ro_features);
    reached(if read_only { "mounted read-only" } else { "mounted for writing" });
    let vfs = fs::vfs_instance().expect("the VFS");
    let mut f = Fs { vfs, model: Model::new(want, read_only, Limits::fixed(max_file(bs), usize::MAX)),
                     strict: !faults };

    let mut ops = 0;
    while r.more() && ops < 200 {
        ops += 1;
        if r.chance(8) {
            /* Unmount and mount again: refused while a file is open, and
             * then everything as it was. */
            let busy = !f.model.open.is_empty();
            let done = vfs.unmount(b"/");
            invariant!(done != busy, "an unmount with {} files open {}", f.model.open.len(),
                       if done { "went through" } else { "was refused" });
            if done {
                let again = fs::ext2::mount_at("/", dev.handle(), ro_asked);
                if faults && again != read_only as i32 {
                    /* A superblock that would not write, or read: the
                     * run is over. */
                    return;
                }
                invariant!(again == read_only as i32, "ext2 mounted again as {}", again);
                reached("remounted midway");
            }
            continue;
        }
        fsops::step(&mut f, r, bs);
    }
    f.compare();
    f.close_all();
    invariant!(vfs.unmount(b"/"), "the unmount at the end was refused");
    disk::with(0, |d| d.no_writes = Some("its ext2 is unmounted".into()));

    /* The image a clean unmount leaves: whole, and the model's tree. */
    let media = disk::with(0, |d| d.current.clone());
    dump("final", &media, dev_bytes);
    let c = img::check(&media, dev_bytes);
    if faults {
        /* Requests failed: at worst what a power cut leaves. */
        invariant!(c.corrupt.is_empty(), "the image after requests failed is corrupt: {:?}", c.corrupt);
        reached("an image judged after requests failed");
        return;
    }
    invariant!(c.corrupt.is_empty(), "the image after a clean unmount is corrupt: {:?}", c.corrupt);
    invariant!(c.unclean.is_empty(), "the image after a clean unmount is not clean: {:?}", c.unclean);
    if let Some(t) = &c.tree {
        if let Some(diff) = f.model.tree.diff(t) {
            panic!("invariant: the image after a clean unmount holds another tree than the model's: {}", diff);
        }
    }

    /* Mounted again, it shows the same. */
    if !read_only {
        disk::with(0, |d| d.no_writes = None);
    }
    let again = fs::ext2::mount_at("/", dev.handle(), read_only);
    invariant!(again == read_only as i32, "ext2 mounted at the end as {}", again);
    f.compare();
    invariant!(vfs.unmount(b"/"), "the last unmount was refused");

    /* Every power cut: at worst unclean. */
    let crashes = disk::with(0, |d| std::mem::take(&mut d.crashes));
    for (n, snap) in crashes {
        let c = img::check(&snap, dev_bytes);
        dump(if c.unclean.is_empty() && c.corrupt.is_empty() { "crash-clean" } else { "crash-unclean" }, &snap, dev_bytes);
        reached("a power cut judged");
        invariant!(c.corrupt.is_empty(), "the power going before request {} leaves the image corrupt: {:?}", n,
                   c.corrupt);
    }
}
