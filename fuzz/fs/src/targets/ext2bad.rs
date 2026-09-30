//! ext2 on images somebody else made badly -- or on purpose: a sound image
//! from the fuzzer's mkfs, and then a few of its fields made what they must
//! not be -- the superblock's geometry, a group's descriptor, an inode's
//! mode, size, links and block pointers (out of the filesystem, into its
//! metadata, into another file, at itself), a directory's entries (lengths
//! that run off the block, names with a slash, an entry naming its own
//! ancestor), an indirect block, a bitmap. Mounted if the driver takes it,
//! and worked through the VFS; nothing is held to a model -- the image has
//! none -- but everything to what holds for any image: no panic, no
//! overflow, no request outside the device, no write to a filesystem
//! mounted read-only or not mounted at all, a directory that lists the
//! same twice running, a name listed that can be looked up, a file that
//! reads no more than its size.

use crate::image::ext2::{self as img, gd, ino, sb, Geometry, Style};
use crate::machine::disk::{self, Media};
use crate::model::show;
use crate::targets::ext2::{geometry, max_file};
use crate::targets::fsops::{self, Fs, Limits, Model};
use crate::{reached, Input};

/// A value for a field of `bits` bits: an edge of the field's, a number
/// that means something in this filesystem, or anything.
fn value(r: &mut Input, bits: u32, g: &Geometry) -> u32 {
    let max = if bits >= 32 { u32::MAX } else { (1u32 << bits) - 1 };
    let meaningful = [0, 1, 2, 3, 11, 12, 13, 14, g.blocks - 1, g.blocks, g.blocks + 1, g.inodes(), g.inodes() + 1,
                      g.blocks_per_group, g.inodes_per_group, g.first_data_block() + 1, g.layout(0).block_bitmap,
                      g.layout(0).inode_table, max, max - 1, max / 2 + 1];
    (match r.u8() % 3 {
        0 => r.pick(&meaningful),
        1 => r.value32(),
        _ => r.u32(),
    }) & max
}

fn set(m: &mut Media, at: u64, bits: u32, v: u32) {
    match bits {
        8 => m.write(at, &[v as u8]),
        16 => m.write(at, &(v as u16).to_le_bytes()),
        _ => m.write(at, &v.to_le_bytes()),
    }
}

/// Where inode `n` is on the image.
fn inode_at(g: &Geometry, n: u32) -> u64 {
    let idx = n.max(1) - 1;
    let l = g.layout((idx / g.inodes_per_group).min(g.groups() - 1));
    l.inode_table as u64 * g.block_size() as u64 + ((idx % g.inodes_per_group) * g.inode_size as u32) as u64
}

/// A block of inode `n`'s: a data block or an indirect one, the first
/// found.
fn block_of(m: &Media, g: &Geometry, n: u32, which: u32) -> Option<u32> {
    let i = m.bytes(inode_at(g, n), 128);
    let b = img::rd32(&i, ino::BLOCK + 4 * (which as usize % 15));
    (b != 0 && b < g.blocks).then_some(b)
}

/// One field of the image made what it must not be.
fn damage(r: &mut Input, m: &mut Media, g: &Geometry, inodes: &[u32]) {
    let bs = g.block_size() as u64;
    let some_inode = |r: &mut Input| -> u32 {
        match r.u8() % 4 {
            0 => img::ROOT_INO,
            1 => 11,
            _ if !inodes.is_empty() => inodes[r.below(inodes.len() as u64) as usize],
            _ => r.range(1, g.inodes() as u64) as u32,
        }
    };
    match r.u8() % 11 {
        0 | 10 => {
            /* A superblock field: most often one the geometry is worked
             * out from. */
            let (at, bits) = r.pick(&[
                (sb::BLOCKS_COUNT, 32), (sb::BLOCKS_COUNT, 32), (sb::INODES_COUNT, 32), (sb::INODES_COUNT, 32),
                (sb::BLOCKS_PER_GROUP, 32), (sb::INODES_PER_GROUP, 32), (sb::FIRST_DATA_BLOCK, 32),
                (sb::INODES_COUNT, 32), (sb::BLOCKS_COUNT, 32), (sb::FREE_BLOCKS, 32), (sb::FREE_INODES, 32),
                (sb::FIRST_DATA_BLOCK, 32), (sb::LOG_BLOCK_SIZE, 32), (sb::BLOCKS_PER_GROUP, 32),
                (sb::INODES_PER_GROUP, 32), (sb::MAGIC, 16), (sb::STATE, 16), (sb::REV_LEVEL, 32),
                (sb::FIRST_INO, 32), (sb::INODE_SIZE, 16), (sb::FEATURE_INCOMPAT, 32), (sb::FEATURE_RO_COMPAT, 32),
                (sb::MNT_COUNT, 16),
            ]);
            let v = value(r, bits, g);
            set(m, img::SB_OFFSET + at as u64, bits, v);
        }
        1 => {
            /* A group descriptor's field. */
            let grp = r.below(g.groups() as u64);
            let (at, bits) = r.pick(&[(gd::BLOCK_BITMAP, 32), (gd::INODE_BITMAP, 32), (gd::INODE_TABLE, 32),
                                      (gd::FREE_BLOCKS, 16), (gd::FREE_INODES, 16), (gd::USED_DIRS, 16)]);
            let v = value(r, bits, g);
            set(m, (g.first_data_block() as u64 + 1) * bs + grp * img::GD_SIZE as u64 + at as u64, bits, v);
        }
        2 | 3 => {
            /* An inode's field. */
            let n = some_inode(r);
            let base = inode_at(g, n);
            let (at, bits) = r.pick(&[(ino::MODE, 16), (ino::SIZE, 32), (ino::LINKS, 16), (ino::BLOCKS, 32),
                                      (ino::FLAGS, 32), (ino::DIR_ACL, 32), (ino::DTIME, 32)]);
            let v = if at == ino::MODE && r.bool() {
                r.pick(&[img::S_IFDIR | 0o755, img::S_IFREG | 0o644, 0o120777, 0]) as u32
            } else if at == ino::FLAGS && r.bool() {
                0x1000
            } else {
                value(r, bits, g)
            };
            set(m, base + at as u64, bits, v);
        }
        4 => {
            /* A block pointer of an inode's: anywhere, another inode's
             * block, or its own indirect block. */
            let n = some_inode(r);
            let slot = r.below(15) as u32;
            let v = match r.u8() % 3 {
                0 => {
                    let other = some_inode(r);
                    block_of(m, g, other, r.below(15) as u32).unwrap_or(1)
                }
                1 => block_of(m, g, n, 12 + r.below(2) as u32).unwrap_or(0),
                _ => value(r, 32, g),
            };
            set(m, inode_at(g, n) + ino::BLOCK as u64 + 4 * slot as u64, 32, v);
        }
        5 | 6 => {
            /* A directory entry, in a directory's first block. */
            let n = some_inode(r);
            let Some(b) = block_of(m, g, n, 0) else { return };
            let base = b as u64 * bs;
            let at = base + 4 * r.below(bs / 4);
            match r.u8() % 5 {
                0 => {
                    let any = r.u16() as u32;
                    set(m, at + 4, 16, r.pick(&[0, 4, 7, 8, 12, bs as u32, bs as u32 + 4, 0xFFFF, any]))
                }
                1 => {
                    let any = r.u8() as u32;
                    set(m, at + 6, 8, r.pick(&[0, 1, 2, 255, any]))
                }
                2 => set(m, at + 7, 8, r.pick(&[0, 1, 2, 7, 0xFF])),
                3 => set(m, at, 32, r.pick(&[0, 1, img::ROOT_INO, n, g.inodes(), g.inodes() + 1, u32::MAX])),
                _ => m.write(at + 8, &[r.pick(&[b'/', 0, b'.'])]),
            }
        }
        7 => {
            /* A pointer in an indirect block: anywhere, or back at the
             * block itself. */
            let n = some_inode(r);
            let Some(b) = block_of(m, g, n, 12 + r.below(2) as u32) else { return };
            let v = if r.bool() { b } else { value(r, 32, g) };
            set(m, b as u64 * bs + 4 * r.below(bs / 4), 32, v);
        }
        8 => {
            /* A bitmap's byte: blocks in use marked free, or the reverse. */
            let l = g.layout(r.below(g.groups() as u64) as u32);
            let map = if r.bool() { l.block_bitmap } else { l.inode_bitmap };
            let at = map as u64 * bs + r.below(bs);
            let any = r.u8();
            m.write(at, &[r.pick(&[0, 0xFF, any])]);
        }
        _ => {
            /* Anything in the metadata. */
            let end = g.layout(0).data as u64 * bs;
            let at = r.below(end.max(1));
            m.write(at, &[r.u8()]);
        }
    }
}

pub fn ext2bad(r: &mut Input) {
    let g = geometry(r);
    let bs = g.block_size() as u64;
    let mut budget = 64;
    let mut inodes_left = 24;
    let tree = crate::targets::ext2::tree(r, bs, &mut budget, &mut inodes_left);
    let style = Style { holes: r.bool(), slack: r.bool(), scatter: 0 };
    let Ok(made) = img::mkfs(&g, &tree, &style, 1_749_000_000, r.u64()) else { return };
    let inodes: Vec<u32> = made.inodes.values().copied().collect();
    let mut media = made.media;
    for _ in 0..r.range(1, 4) {
        damage(r, &mut media, &g, &inodes);
    }
    let ss = r.pick(&[512u64, 512, 4096, 1024]).min(bs);
    let sectors = (g.blocks as u64 * bs).div_ceil(ss) + r.below(4);
    let mut d = disk::Disk::new("vda", ss, sectors, media);
    d.latency = r.chance(32);
    disk::insert(0, d);
    let Some(dev) = disk::register(0) else { return };

    let ro = r.chance(64);
    if ro {
        disk::with(0, |d| d.no_writes = Some("its ext2 is mounted read-only".into()));
    }
    let mut probe = fs::ext2::Identity { uuid: [0; 16], label: [0; 17] };
    let _ = fs::ext2::probe(&dev, &mut probe);
    let mounted = fs::ext2::mount_at("/", dev.handle(), ro);
    if mounted < 0 {
        reached("a damaged image refused");
        return;
    }
    if mounted == 1 && !ro {
        disk::with(0, |d| d.no_writes = Some("its ext2 mounted read-only".into()));
    }
    reached("a damaged image mounted");
    let vfs = fs::vfs_instance().expect("the VFS");
    let mut f = Fs { vfs, model: Model::new(tree, mounted == 1, Limits::fixed(max_file(bs), usize::MAX)),
                     strict: false };

    /* Everything that can be listed, listed twice, and looked up; and
     * again, and again: work that changes nothing leaves nothing behind. */
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

    /* Unmounted, it is nobody's to read or write. */
    let before = disk::with(0, |d| d.log.len());
    disk::with(0, |d| d.no_writes = Some("its ext2 is unmounted".into()));
    let _ = vfs.stat(b"/");
    invariant!(disk::with(0, |d| d.log.len()) == before, "the disk was read after its filesystem was unmounted");
}

/// Every directory under `p`: listed twice, the same both times; every name
/// in it looked up; every file read to its end, and no further.
pub fn walk(f: &mut Fs, p: &[u8], depth: usize) {
    if depth > 40 {
        return;
    }
    let first = f.list(p);
    let second = f.list(p);
    invariant!(first == second, "{} listed {} entries, and then {} with nothing done between", show(p), first.len(),
               second.len());
    for (name, dir, size) in first {
        if name.contains(&b'/') || name.contains(&0) || name.is_empty() {
            continue;
        }
        let mut child = p.to_vec();
        if !child.ends_with(b"/") {
            child.push(b'/');
        }
        child.extend_from_slice(&name);
        /* The name is looked up to the first node of it -- a damaged
         * directory may hold two -- and it reads no more than that one's
         * size. */
        let st = f.vfs.stat(&child);
        invariant!(st.is_some(), "{} is listed, and does not stat", show(&child));
        let _ = size;
        if dir {
            walk(f, &child, depth + 1);
        } else if let (Some(data), Some(st)) = (f.read_file_upto(&child, 1 << 20), st) {
            invariant!(data.len() <= st.size as u64, "{} read {} bytes of the {} it has", show(&child), data.len(),
                       st.size);
            reached("a file of a damaged image read");
        }
    }
}
