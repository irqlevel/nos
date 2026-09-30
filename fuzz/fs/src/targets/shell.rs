//! The shell's storage commands -- what the console, the UDP shell and an
//! SSH session run: `mount`, `umount`, `format`, `ls`, `cat`, `write`,
//! `append`, `mkdir`, `touch`, `cp -r`, `rm`, `mv`, `stat`, `sync`,
//! `fstest`, `disks`, `partitions`, `diskread`, `diskwrite`, `disklog` --
//! with arguments of every kind, over a ramfs root and an ext2 and a nanofs
//! on disks. First the kernel's own self-test on each filesystem, which
//! must pass; then whatever the input types. And at the end, every disk no
//! command wrote to raw is a filesystem e2fsck -- or nanofs's checker --
//! passes: what the commands did through the VFS left it whole.

use crate::image::{ext2 as ext2img, nanofs as nanoimg};
use crate::machine::{cmd, disk};
use crate::model::{Data, Node};
use crate::{reached, Input};

const DISKS: &[&str] = &["vda", "vdb", "vdc", "vdc1", "vdz", ""];

const PATHS: &[&str] = &["/", "/e", "/n", "/e/a", "/n/a", "/e/d/f", "/n/d", "/tmp", "/e/lost+found", "..", "/e/..",
                         "/n/../e", "x", "/e/fstest.tmp", "/proc", "/mnt"];

fn path(r: &mut Input) -> String {
    match r.u8() % 10 {
        0 => format!("/e/{}", "d/".repeat(r.range(1, 40) as usize)),
        1 => "/".to_string() + &"p".repeat(r.range(60, 300) as usize),
        2 => format!("{}/{}", r.pick(PATHS), r.pick(&["a", "b", "c.txt", "d"])),
        _ => r.pick(PATHS).to_string(),
    }
}

fn size(r: &mut Input) -> String {
    r.pick(&["0", "1", "4K", "64K", "300K", "1M", "1025K", "abc", "99999999999999999999M", "-1", "4k"]).to_string()
}

fn text(r: &mut Input) -> String {
    let n = r.below(80) as usize;
    (0..n).map(|_| (b'a' + r.u8() % 26) as char).collect()
}

/// A command line of the shell's, arguments and all.
fn line(r: &mut Input) -> String {
    let p = path(r);
    match r.u8() % 22 {
        0 => format!("ls {}", p),
        1 => format!("cat {}", p),
        2 => format!("write {} {}", p, text(r)),
        3 => format!("append {} {}", p, text(r)),
        4 => format!("mkdir {}", p),
        5 => format!("touch {}", p),
        6 => format!("cp {} {}", p, path(r)),
        7 => format!("cp -r {} {}", p, path(r)),
        8 => format!("rm {}", p),
        9 => format!("mv {} {}", p, path(r)),
        10 => format!("stat {}", p),
        11 => "sync".to_string(),
        12 => "mounts".to_string(),
        13 => format!("umount {}", p),
        14 => match r.u8() % 4 {
            0 => format!("mount ramfs {}", p),
            1 => format!("mount nanofs {} {}", r.pick(DISKS), p),
            2 => format!("mount ext2 {} {}{}", r.pick(DISKS), p, r.pick(&["", " ro", " rw"])),
            _ => format!("mount {}", r.pick(&["", "fat", "ext2", "nanofs vdb"])),
        },
        15 => format!("format {} {}", r.pick(&["nanofs", "ext2", ""]), r.pick(DISKS)),
        16 => format!("fstest {} {}", p, size(r)),
        17 => "disks".to_string(),
        18 => format!("partitions {}", r.pick(DISKS)),
        19 => format!("diskread {} {}", r.pick(DISKS), r.pick(&["0", "1", "2", "18446744073709551615", "x"])),
        20 => format!("diskwrite {} {} {}", r.pick(DISKS), r.pick(&["0", "2", "100"]), r.pick(&["00", "ff00ff", "zz", ""])),
        _ => r.pick(&["disklog", "del", "rm", "cp", "mv a", "mount ext2 vda", "fstest", "ls /e /n /x"]).to_string(),
    }
}

/// A small tree for a disk's image.
fn tree(r: &mut Input) -> Node {
    let mut c = std::collections::BTreeMap::new();
    for k in 0..r.below(4) {
        let n = r.below(9000) as usize;
        c.insert(format!("f{}", k).into_bytes(), Node::File(Data::from(&crate::input::noise(r.u32(), n))));
    }
    if r.bool() {
        c.insert(b"d".to_vec(), Node::dir());
    }
    Node::Dir(c)
}

pub fn shell(r: &mut Input) {
    /* An ext2 on vda, a nanofs on vdb, and vdc carved in two by an MBR, the
     * partition holding nothing yet. */
    /* Roomy enough for the self-test's files, in every block size. */
    let log = r.below(3) as u32;
    let bs = 1024u64 << log;
    let mut label = [0u8; 16];
    label[..3].copy_from_slice(b"nos");
    let g = ext2img::Geometry {
        log_block_size: log,
        blocks: ((4u64 << 20) / bs) as u32,
        blocks_per_group: 8 * bs as u32,
        inodes_per_group: 256,
        inode_size: 128,
        sparse_super: true,
        large_file: true,
        dir_index: false,
        ro_compat_extra: 0,
        reserved_blocks: 0,
        label,
        uuid: [9; 16],
    };
    let Ok(e) = ext2img::mkfs(&g, &tree(r), &Default::default(), 1_749_000_000, r.u64()) else { return };
    let ext2_bytes = g.blocks as u64 * bs;
    disk::insert(0, disk::Disk::new("vda", 512, ext2_bytes / 512, e.media));
    let Ok(n) = nanoimg::mkfs(&tree(r), [5u8; 16], 0) else { return };
    disk::insert(1, disk::Disk::new("vdb", 512, nanoimg::DEVICE_BYTES / 512, n.media));
    let entry = crate::image::part::MbrEntry { status: 0, kind: 0x83, start: 2048, size: 16384 };
    let none = crate::image::part::MbrEntry::default();
    let mbr = crate::image::part::mbr(&[entry, none, none, none], crate::image::part::MBR_SIGNATURE);
    disk::insert(2, disk::Disk::new("vdc", 512, 32768, disk::Media::from_bytes(&mbr)));
    for slot in 0..3 {
        if disk::register(slot).is_none() {
            return;
        }
    }
    block::rust_partitions_probe();
    invariant!(fs::ramfs::mount_at("/", false), "a ramfs would not mount at the root");

    let out = cmd::run("mount ext2 vda /e");
    invariant!(out.contains("mounted ext2 on /e (rw)"), "mount ext2 vda /e said {:?}", out);
    let out = cmd::run("mount nanofs vdb /n");
    invariant!(out.contains("mounted nanofs on /n"), "mount nanofs vdb /n said {:?}", out);

    /* The kernel's own test of each filesystem, on each as it came. */
    for (dir, big) in [("/", "300K"), ("/e", "40K"), ("/n", "64K")] {
        let out = cmd::run(&format!("fstest {} {}", dir, big));
        invariant!(out.contains("fstest: passed"), "fstest {} {} said {:?}", dir, big, out.lines().last());
    }
    reached("fstest passed on ramfs, ext2 and nanofs");

    /* Then whatever is typed. A disk a command writes to raw is no longer
     * a filesystem anybody vouches for. */
    let mut raw = [false; 3];
    let mut lines = 0;
    while r.more() && lines < 100 {
        lines += 1;
        let l = line(r);
        let out = cmd::run(&l);
        crate::targets::fsops::say(format_args!("{} -> {:?}", l, out.lines().last()));
        if out.contains("formatted") || out.contains("wrote ") {
            let target = l.split_whitespace().nth(if l.starts_with("format") { 2 } else { 1 }).unwrap_or("");
            for (slot, name) in ["vda", "vdb", "vdc"].iter().enumerate() {
                if target.starts_with(name) {
                    raw[slot] = true;
                }
            }
            reached("a disk written raw from the shell");
        }
    }

    let vfs = fs::vfs_instance().expect("the VFS");
    vfs.unmount_all();
    for (slot, bytes) in [(0usize, ext2_bytes), (1, nanoimg::DEVICE_BYTES)] {
        if raw[slot] {
            continue;
        }
        let m = disk::with(slot, |d| d.current.clone());
        if slot == 0 {
            let c = ext2img::check(&m, bytes);
            invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the shell's commands left vda's ext2 {:?} {:?}",
                       c.corrupt, c.unclean);
        } else {
            let c = nanoimg::check(&m);
            invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the shell's commands left vdb's nanofs {:?} \
                       {:?}", c.corrupt, c.unclean);
        }
    }
}
