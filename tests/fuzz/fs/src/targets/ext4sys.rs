//! ext4 mounted for writing over a file whose extent names the
//! filesystem's own blocks -- an inode table's, the descriptors', the
//! journal's log -- every checksum on it sound: a damaged image, or one
//! made to be. Every way a file's data reaches the device must refuse it
//! -- a write in place, a read, a cut's tail made zeros, a remove, a disk's
//! image mapped to be read and written off the VFS's lock -- and the blocks
//! it names stay as they were, while the filesystem's other files go on
//! being written and read as the model has them. The driver once held only
//! what it allocated to the system zone: a file's own extent was trusted,
//! and a guest's disk mapped onto an inode table would have written over
//! it.

use crate::image::ext4::{self as img, mkfs::inode_csum};
use crate::machine::disk;
use crate::model::{Data, Node};
use crate::targets::ext4::{geometry, style, MKFS_TIME};
use crate::{reached, Input};
use fs::image::Image;
use fs::vfs::{OPEN_READ, OPEN_WRITE};

/// The filesystem's own blocks a file's extent is made to name.
#[derive(Clone, Copy, Debug)]
enum Target {
    InodeTable,
    Descriptors,
    JournalLog,
}

pub fn ext4sys(r: &mut Input) {
    let mut g = geometry(r);
    g.journal_blocks = 1024;
    g.csum_seed = false;
    if !g.valid() {
        return;
    }
    let mut st = style(r);
    st.unwritten = false;
    st.hidden = false;
    let bs = g.block_size();
    let content = |seed: u8, n: u64| -> Vec<u8> { (0..n).map(|k| seed ^ (k as u8).wrapping_mul(31)).collect() };
    let mut tree = Node::dir();
    let children = tree.children_mut().expect("a directory");
    children.insert(b"f".to_vec(), Node::File(Data::from(&content(0x5A, 2 * bs))));
    children.insert(b"g".to_vec(), Node::File(Data::from(&content(0xC3, bs + 17))));
    let Ok(made) = img::mkfs(&g, &tree, &st, MKFS_TIME, r.u64()) else { return };
    let mut media = made.media;
    let Some(&ino) = made.inodes.get(&vec![b"f".to_vec()]) else { return };

    /* The file's first extent made to start on the filesystem's own
     * blocks, its inode's checksum put right. */
    let target = match r.u8() % 3 {
        0 => Target::InodeTable,
        1 => Target::Descriptors,
        _ => Target::JournalLog,
    };
    let isz = u64::from(g.inode_size);
    let ipg = u64::from(g.inodes_per_group);
    let at = g.layout((u64::from(ino) - 1) / ipg).inode_table * bs + (u64::from(ino) - 1) % ipg * isz;
    let mut inode = media.bytes(at, isz as usize);
    let root = img::ino::BLOCK;
    if img::rd16(&inode, root) != 0xF30A || img::rd16(&inode, root + 2) == 0 || img::rd16(&inode, root + 6) != 0 {
        return;
    }
    let len = u64::from(img::rd16(&inode, root + 12 + 4));
    /* The blocks of the target the extent covers -- past them may be what
     * a mount writes as it should, the journal's superblock after a table
     * of one block, say. */
    let (start, region) = match target {
        Target::InodeTable => (g.layout(0).inode_table, g.itable_blocks()),
        Target::Descriptors => (g.first_data_block() + 1, g.gdt_blocks()),
        Target::JournalLog => {
            let j = media.bytes(g.layout(0).inode_table * bs + (u64::from(img::JOURNAL_INO) - 1) * isz, isz as usize);
            /* The journal's own extent root, depth 0: its first block is its
             * superblock, the log after it. */
            if img::rd16(&j, root + 6) != 0 {
                return;
            }
            let first = u64::from(img::rd16(&j, root + 12 + 6)) << 32 | u64::from(img::rd32(&j, root + 12 + 8));
            (first + 1, u64::from(img::rd16(&j, root + 12 + 4)).saturating_sub(1))
        }
    };
    let held = len.min(region);
    img::wr16(&mut inode, root + 12 + 6, (start >> 32) as u16);
    img::wr32(&mut inode, root + 12 + 8, start as u32);
    let c = inode_csum(img::crc32c(!0, &g.uuid), ino, &inode);
    img::wr16(&mut inode, img::ino::CHECKSUM_LO, c as u16);
    if isz > 128 {
        img::wr16(&mut inode, img::ino::CHECKSUM_HI, (c >> 16) as u16);
    }
    media.write(at, &inode);
    let before = media.bytes(start * bs, (held * bs) as usize);

    let sizes: Vec<u64> = [512, 1024, 4096].into_iter().filter(|s| *s <= bs).collect();
    let ss = r.pick(&sizes);
    disk::insert(0, disk::Disk::new("vda", ss, g.blocks * bs / ss, media));
    let Some(dev) = disk::register(0) else { return };
    let mounted = fs::ext4::mount_at("/", dev.handle(), false);
    invariant!(mounted == 0, "ext4 did not mount for writing an image whose one file names {:?}: {}", target, mounted);
    let vfs = fs::vfs_instance().expect("the VFS");

    /* Every way to the file's data refused. */
    let buf_len = bs as usize;
    if let Some(h) = vfs.open(b"/f", OPEN_WRITE) {
        invariant!(!vfs.write(h, &vec![0xAA; buf_len]), "a write to a file mapped onto the {:?} succeeded", target);
        vfs.close(h);
    }
    if let Some(h) = vfs.open(b"/f", OPEN_READ) {
        let mut buf = vec![0u8; buf_len];
        invariant!(vfs.read(h, &mut buf).is_none(), "a read of a file mapped onto the {:?} succeeded", target);
        vfs.close(h);
    }
    invariant!(!vfs.truncate(b"/f", (bs / 2) as usize), "a cut of a file mapped onto the {:?} succeeded", target);
    if let Some(mut image) = Image::open(vfs, b"/f", true) {
        let mut buf = vec![0u8; buf_len];
        invariant!(!image.read_at(0, &mut buf), "an image of a file mapped onto the {:?} read", target);
        invariant!(!image.write_at(0, &vec![0x55; buf_len]), "an image of a file mapped onto the {:?} wrote", target);
        reached("ext4: a disk's image of a file mapped onto the filesystem's own blocks, refused");
    }
    invariant!(!vfs.remove(b"/f"), "a remove of a file mapped onto the {:?} succeeded", target);

    /* The file beside it as it was; nothing else written by any of the
     * refusals -- the blocks the damaged file names as they were. */
    let want = content(0xC3, bs + 17);
    let read_g = |vfs: &fs::vfs::Vfs| {
        let mut got = vec![0u8; want.len()];
        let n = vfs.open(b"/g", OPEN_READ).map(|h| {
            let n = vfs.read(h, &mut got);
            vfs.close(h);
            n
        });
        n == Some(Some(want.len())) && got == want
    };
    invariant!(read_g(vfs), "the file beside it did not read as it was");
    invariant!(vfs.unmount(b"/"), "the unmount was refused");
    let after = disk::with(0, |d| d.current.bytes(start * bs, (held * bs) as usize));
    invariant!(after == before, "blocks {}..+{} of the {:?}, which the damaged file names, were written", start, held,
               target);

    /* And the filesystem still written, the journal not aborted by any of
     * it: mounted again, a file made beside it and read back. */
    let again = fs::ext4::mount_at("/", dev.handle(), false);
    invariant!(again == 0, "ext4 did not mount for writing again: {}", again);
    /* Where the filesystem has the room: one made full holds its last
     * blocks for a punch. */
    if vfs.space_of(b"/").is_some_and(|s| s.free_bytes >= 4 * bs) {
        invariant!(vfs.write_file(b"/h", b"written"), "a new file was not written beside the damaged one");
        let mut h = [0u8; 7];
        let read_h = vfs.open(b"/h", OPEN_READ).and_then(|f| {
            let n = vfs.read(f, &mut h);
            vfs.close(f);
            n
        });
        invariant!(read_h == Some(7) && &h == b"written", "what was written beside it did not read back");
    }
    invariant!(read_g(vfs), "the file beside it did not read as it was, mounted again");
    invariant!(vfs.unmount(b"/"), "the second unmount was refused");
    reached(match target {
        Target::InodeTable => "ext4: a file mapped onto an inode table, refused",
        Target::Descriptors => "ext4: a file mapped onto the descriptors, refused",
        Target::JournalLog => "ext4: a file mapped onto the journal's log, refused",
    });
}
