//! nanofs: images made from a tree by the fuzzer's own mkfs, mounted,
//! worked through the VFS and held to the model -- every answer, the tree
//! read back, the image a clean unmount leaves, and what the power going
//! leaves: "either the old file or the new one, never a mix", which the
//! driver's copy-on-write and its checksums promise. And images somebody
//! made to fool it, checksums and all: the checksums are integrity, not
//! authentication, so an inode that is a lie is as well-sealed as a true
//! one.

use std::collections::BTreeMap;

use kcore::crc32::crc32_with_hole;

use crate::image::nanofs::{self as img, ino, sb};
use crate::machine::disk::{self, Media};
use crate::model::{Data, Node};
use crate::targets::fsops::{self, Fs, Limits, Model};
use crate::{reached, Input};

fn name(r: &mut Input) -> Vec<u8> {
    loop {
        let n = fsops::name(r);
        if !n.is_empty() && n.len() < img::NAME_LEN && !n.contains(&0) && n != b"." && n != b".." {
            return n;
        }
    }
}

fn tree(r: &mut Input, depth: usize) -> Node {
    let mut c = BTreeMap::new();
    for _ in 0..r.below(6) {
        let node = if depth < 4 && r.chance(80) {
            tree(r, depth + 1)
        } else {
            let len = match r.u8() % 24 {
                0..=3 => 0,
                4..=7 => r.below(100),
                8..=11 => 4096 + r.below(3) - 1,
                12..=14 => r.below(64 << 10),
                15 => img::MAX_FILE as u64 - r.below(2),
                _ => r.below(8192),
            };
            Node::File(Data::from(&crate::input::noise(r.u32(), len as usize)))
        };
        c.insert(name(r), node);
    }
    Node::Dir(c)
}

fn sector_size(r: &mut Input) -> u64 {
    r.pick(&[512, 512, 1024, 2048, 4096, 4096])
}

pub fn nanofs(r: &mut Input) {
    let tree = tree(r, 0);
    let mut uuid = [0u8; 16];
    for b in uuid.iter_mut() {
        *b = r.u8();
    }
    let Ok(made) = img::mkfs(&tree, uuid, r.pick(&[0, 0, 4, 16])) else { return };
    let c = img::check(&made.media);
    invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the fuzzer's nanofs mkfs made an image its checker \
               faults: {:?}, {:?}", c.corrupt, c.unclean);
    invariant!(c.tree.as_ref() == Some(&tree), "the fuzzer's nanofs mkfs made an image of another tree: {:?}",
               c.tree.as_ref().and_then(|t| tree.diff(t)));

    let ss = sector_size(r);
    let sectors = img::DEVICE_BYTES / ss + r.below(8);
    let mut d = disk::Disk::new("vdb", ss, sectors, made.media);
    d.seed_chaos(r.u64());
    d.volatile = r.u8() % 4 != 0;
    d.latency = r.chance(64);
    if d.volatile {
        for _ in 0..r.below(4) {
            d.crash_at.insert(r.below(2000));
        }
    }
    let faults = r.chance(24);
    if faults {
        for _ in 0..r.range(1, 3) {
            d.fail.insert(r.below(800));
        }
    }
    disk::insert(1, d);
    let Some(dev) = disk::register(1) else { return };

    let ro = r.chance(32);
    if ro {
        disk::with(1, |d| d.no_writes = Some("its nanofs is mounted read-only".into()));
    }
    let mounted = fs::nanofs::mount_at("/", dev.handle(), ro);
    if faults && mounted < 0 {
        return;
    }
    invariant!(mounted == ro as i32, "nanofs mounted as {} ({} asked)", mounted, ro);
    reached(if ro { "mounted read-only" } else { "mounted for writing" });
    let vfs = fs::vfs_instance().expect("the VFS");
    let limits = Limits::fixed(img::MAX_FILE as u64, img::MAX_ENTRIES);
    let mut f = Fs { vfs, model: Model::new(tree, ro, limits), strict: !faults };

    let mut ops = 0;
    while r.more() && ops < 200 {
        ops += 1;
        if r.chance(8) {
            let busy = !f.model.open.is_empty();
            let done = vfs.unmount(b"/");
            invariant!(done != busy, "an unmount with {} files open {}", f.model.open.len(),
                       if done { "went through" } else { "was refused" });
            if done {
                let again = fs::nanofs::mount_at("/", dev.handle(), ro);
                if faults && again < 0 {
                    return;
                }
                invariant!(again == ro as i32, "nanofs mounted again as {}", again);
                reached("remounted midway");
            }
            continue;
        }
        fsops::step(&mut f, r, img::BLOCK as u64);
    }
    f.compare();
    f.close_all();
    invariant!(vfs.unmount(b"/"), "the unmount at the end was refused");
    disk::with(1, |d| d.no_writes = Some("its nanofs is unmounted".into()));

    let media = disk::with(1, |d| d.current.clone());
    let c = img::check(&media);
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

    if !ro {
        disk::with(1, |d| d.no_writes = None);
    }
    let again = fs::nanofs::mount_at("/", dev.handle(), ro);
    invariant!(again == ro as i32, "nanofs mounted at the end as {}", again);
    f.compare();
    invariant!(vfs.unmount(b"/"), "the last unmount was refused");

    let crashes = disk::with(1, |d| std::mem::take(&mut d.crashes));
    for (n, snap) in crashes {
        let c = img::check(&snap);
        reached("a power cut judged");
        invariant!(c.corrupt.is_empty(), "the power going before request {} leaves the image corrupt: {:?}", n,
                   c.corrupt);
    }
}

/// Seals an inode block, or the superblock, as the driver does.
fn reseal(m: &mut Media, at: u64, hole: usize) {
    let mut b = m.bytes(at, img::BLOCK);
    img::seal(&mut b, hole);
    m.write(at, &b);
}

/// A lie told well: a field of the superblock or of an inode, or a
/// directory's entry, made what it must not be -- and, most often, the
/// checksum over it put right.
fn damage(r: &mut Input, m: &mut Media, inodes: &[u32]) {
    let some_inode = |r: &mut Input| -> u32 {
        match r.u8() % 3 {
            0 => 0,
            _ if !inodes.is_empty() => inodes[r.below(inodes.len() as u64) as usize],
            _ => r.below(img::INODES as u64) as u32,
        }
    };
    let edge = |r: &mut Input| -> u32 {
        let any = r.u32();
        r.pick(&[0, 1, 2, 255, 256, 257, img::INODES - 1, img::INODES, img::DATA_BLOCKS - 1, img::DATA_BLOCKS,
                 img::MAX_FILE as u32, img::MAX_FILE as u32 + 1, u32::MAX, any])
    };
    let seal = !r.chance(32);
    match r.u8() % 6 {
        0 => {
            let (i, d) = (r.below(128) as usize, r.below(2048) as usize);
            let at = r.pick(&[sb::BLOCK_SIZE, sb::INODE_COUNT, sb::DATA_BLOCK_COUNT, sb::INODE_START, sb::DATA_START,
                              sb::VERSION, sb::INODE_BITMAP + i, sb::DATA_BITMAP + d]);
            let v = edge(r);
            m.write(at as u64, &v.to_le_bytes());
            if seal {
                reseal(m, 0, sb::CHECKSUM);
            }
        }
        1 | 2 => {
            let n = some_inode(r);
            let at = img::inode_at(n);
            let k = r.below(256) as usize;
            let field = r.pick(&[ino::TYPE, ino::SIZE, ino::PARENT, ino::DATA_CHECKSUM, ino::BLOCKS + 4 * k,
                                 ino::BLOCKS]);
            let v = if field == ino::TYPE { r.pick(&[0, 1, 2, 3, u32::MAX]) } else { edge(r) };
            m.write(at + field as u64, &v.to_le_bytes());
            if seal {
                reseal(m, at, ino::CHECKSUM);
            }
        }
        3 => {
            /* A name: none, all of the field, a slash in it. */
            let n = some_inode(r);
            let at = img::inode_at(n);
            let name: Vec<u8> = match r.u8() % 3 {
                0 => vec![0u8; 64],
                1 => vec![b'x'; 64],
                _ => b"a/b\0".to_vec(),
            };
            m.write(at + ino::NAME as u64, &name);
            if seal {
                reseal(m, at, ino::CHECKSUM);
            }
        }
        4 => {
            /* A directory's entry: nothing, itself, its parent, a file of
             * another directory's, past the table. */
            let n = some_inode(r);
            let i = m.bytes(img::inode_at(n), img::BLOCK);
            let b = u32::from_le_bytes(i[ino::BLOCKS..ino::BLOCKS + 4].try_into().expect("four bytes"));
            if b < img::DATA_BLOCKS {
                let k = r.below(8);
                let other = some_inode(r);
                let v = r.pick(&[0, n, 1, img::INODES, u32::MAX, other]);
                m.write(img::data_at(b) + 8 * k, &v.to_le_bytes());
            }
        }
        _ => {
            /* A data block of a file's, changed under its checksum. */
            let n = some_inode(r);
            let i = m.bytes(img::inode_at(n), img::BLOCK);
            let b = u32::from_le_bytes(i[ino::BLOCKS..ino::BLOCKS + 4].try_into().expect("four bytes"));
            if b < img::DATA_BLOCKS {
                m.write(img::data_at(b) + r.below(img::BLOCK as u64), &[r.u8()]);
            }
        }
    }
    let _ = crc32_with_hole;
}

pub fn nanofsbad(r: &mut Input) {
    let tree = tree(r, 0);
    let Ok(made) = img::mkfs(&tree, [7u8; 16], 0) else { return };
    let inodes: Vec<u32> = made.inodes.values().copied().collect();
    let mut media = made.media;
    for _ in 0..r.range(1, 4) {
        damage(r, &mut media, &inodes);
    }
    let ss = sector_size(r);
    let mut d = disk::Disk::new("vdb", ss, img::DEVICE_BYTES / ss, media);
    d.latency = r.chance(32);
    disk::insert(1, d);
    let Some(dev) = disk::register(1) else { return };
    let ro = r.chance(64);
    if ro {
        disk::with(1, |d| d.no_writes = Some("its nanofs is mounted read-only".into()));
    }
    let mounted = fs::nanofs::mount_at("/", dev.handle(), ro);
    if mounted < 0 {
        reached("a damaged image refused");
        return;
    }
    reached("a damaged image mounted");
    let vfs = fs::vfs_instance().expect("the VFS");
    let limits = Limits::fixed(img::MAX_FILE as u64, img::MAX_ENTRIES);
    let mut f = Fs { vfs, model: Model::new(Node::dir(), ro, limits), strict: false };
    crate::targets::ext2bad::walk(&mut f, b"/", 0);
    let settled = crate::machine::heap::live();
    crate::targets::ext2bad::walk(&mut f, b"/", 0);
    let after = crate::machine::heap::live();
    invariant!(after <= settled, "a walk of a damaged image that changed nothing left {} bytes more allocated than \
               the walk before it", after - settled);
    let mut ops = 0;
    while r.more() && ops < 100 {
        ops += 1;
        fsops::step(&mut f, r, img::BLOCK as u64);
    }
    crate::targets::ext2bad::walk(&mut f, b"/", 0);
    f.close_all();
    invariant!(vfs.unmount(b"/"), "the unmount of a damaged image was refused");
    let before = disk::with(1, |d| d.log.len());
    disk::with(1, |d| d.no_writes = Some("its nanofs is unmounted".into()));
    let _ = vfs.stat(b"/");
    invariant!(disk::with(1, |d| d.log.len()) == before, "the disk was read after its filesystem was unmounted");
}
