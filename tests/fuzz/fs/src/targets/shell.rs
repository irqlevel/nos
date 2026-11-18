//! The shell's storage commands -- what the console, the UDP shell and an
//! SSH session run: `mount`, `umount`, `ls`, `find`, `cat`, `write`,
//! `append`, `mkdir`, `touch`, `cp -r`, `rm`, `mv`, `stat`, `truncate`,
//! `sync`, `fstest`, `df`, `disks`, `partitions`, `diskread`, `diskwrite`, `disklog` --
//! with arguments of every kind, over a ramfs root and two ext2s on disks,
//! of two block sizes. First the kernel's own self-test on each filesystem,
//! which must pass; then whatever the input types. What `df` says is held
//! to the judge's own reading of it: every mount's line in the mounts'
//! order, or the line of the mount each path given is under -- the longest
//! mount path it starts with -- its size, used and free from what the VFS
//! counts, to a tenth of their unit, and the share used rounded up; `-`
//! where a filesystem counts nothing. What `ls`, `mounts` and `disks` print
//! is held to its columns: a listing's names in one column and its sizes
//! right-aligned, the mounts' `on`, mount points and `ro`/`rw` each in one,
//! the disks' sectors and sector sizes each in one. And at the end, every
//! disk no command wrote to raw is a filesystem e2fsck passes: what the
//! commands did through the VFS left it whole.

use crate::image::ext2 as ext2img;
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

/// What `truncate` is given: sizes a file may be cut or grown to, and ones
/// that are no size -- past a word, not a number. None so big that a `cat`
/// or a `cp` of the file after it would read gigabytes of holes (the far
/// file of `fstest` is for those), nor that a ramfs would ask the host for:
/// it gets terabytes there, untouched, until they are zeroed.
fn truncate_size(r: &mut Input) -> String {
    r.pick(&["0", "1", "100", "4095", "4K", "5000", "64K", "300K", "1M", "3M", "0G", "0T", "16777216T",
             "99999999999999999999T", "x", "-1", "1Q", ""]).to_string()
}

fn text(r: &mut Input) -> String {
    let n = r.below(80) as usize;
    (0..n).map(|_| (b'a' + r.u8() % 26) as char).collect()
}

/// A command line of the shell's, arguments and all.
fn line(r: &mut Input) -> String {
    let p = path(r);
    match r.u8() % 24 {
        21 => format!("truncate {} {}", p, truncate_size(r)),
        22 => format!("find {} {}", p, r.pick(&["", "-xdev", "-type d", "-name f*", "-mindepth 1 -maxdepth 2", "-empty",
                                                "-size +1K -o -type d", "-xdev -path /e*", "! ( -name a -o -iname B )",
                                                "-name", "( -type f", "/n -name x"])),
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
        14 => match r.u8() % 3 {
            0 => format!("mount ramfs {}", p),
            1 => format!("mount ext2 {} {}{}", r.pick(DISKS), p, r.pick(&["", " ro", " rw"])),
            _ => format!("mount {}", r.pick(&["", "fat", "ext2", "xfs vdb"])),
        },
        15 => format!("umount {}", r.pick(&["/e", "/n", "/", "/proc"])),
        16 => format!("fstest {} {}", p, size(r)),
        17 => "disks".to_string(),
        18 => format!("partitions {}", r.pick(DISKS)),
        19 => format!("diskread {} {}", r.pick(DISKS), r.pick(&["0", "1", "2", "18446744073709551615", "x"])),
        20 => format!("diskwrite {} {} {}", r.pick(DISKS), r.pick(&["0", "2", "100"]), r.pick(&["00", "ff00ff", "zz", ""])),
        23 => match r.u8() % 4 {
            0 => "df".to_string(),
            1 => format!("df {}", p),
            2 => format!("df {} {}", p, path(r)),
            _ => format!("df {} {} {}", r.pick(&["/", "x", "", "/e/", "/e/../n"]), p, r.pick(&["/n/d", "/proc", "/mnt/a"])),
        },
        _ => r.pick(&["disklog", "del", "rm", "cp", "mv a", "mount ext2 vda", "fstest", "ls /e /n /x"]).to_string(),
    }
}

/// A size as `df` says it, worked out here: bytes below 1024, and above
/// that a tenth of the largest of P, T, G, M and K -- each 1024 of the next
/// below -- that it holds one of, the rest cut off.
fn df_size(bytes: u64) -> String {
    for (unit, shift) in [("P", 50u32), ("T", 40), ("G", 30), ("M", 20), ("K", 10)] {
        let scale = 1u128 << shift;
        if u128::from(bytes) >= scale {
            let tenths = u128::from(bytes) * 10 / scale;
            return format!("{}.{}{}", tenths / 10, tenths % 10, unit);
        }
    }
    format!("{}B", bytes)
}

/// What `df` should say of a mount, a word a column.
fn df_words(m: &fs::vfs::MountInfo) -> Vec<String> {
    let device = block::Disk::from_handle(m.device).map_or("-".to_string(), |d| d.name().to_string());
    let mut words = vec![String::from_utf8_lossy(m.path()).into_owned(), m.fs_name.to_string(), device];
    match m.space {
        Some(s) => {
            let used = s.total_bytes.saturating_sub(s.free_bytes);
            words.push(df_size(s.total_bytes));
            words.push(df_size(used));
            words.push(df_size(s.free_bytes));
            words.push(if s.total_bytes == 0 {
                "-".to_string()
            } else {
                format!("{}%", (u128::from(used) * 100 + u128::from(s.total_bytes) - 1) / u128::from(s.total_bytes))
            });
        }
        None => words.extend(["-", "-", "-", "-"].map(String::from)),
    }
    words
}

/// The mount a path is under, as the judge reads it: the longest mount path
/// it starts with whole -- the path itself, or followed by a `/` -- every
/// absolute path being under `/`.
fn df_mount_of<'m>(mounts: &'m [fs::vfs::MountInfo], p: &str) -> Option<&'m fs::vfs::MountInfo> {
    let p = p.as_bytes();
    mounts
        .iter()
        .filter(|m| {
            let mp = m.path();
            p == mp || (mp == b"/" && p.starts_with(b"/")) || (p.starts_with(mp) && p.get(mp.len()) == Some(&b'/'))
        })
        .max_by_key(|m| m.path().len())
}

/// Each word of a line, with the column it starts at.
fn words_at(line: &str) -> Vec<(usize, &str)> {
    let mut words = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        match (c == ' ', start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                words.push((s, &line[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        words.push((s, &line[s..]));
    }
    words
}

/// Whether every one of `columns` is the same: what a printer that lines
/// its columns up gives.
fn one_column(what: &str, l: &str, columns: &[usize], out: &str) {
    invariant!(columns.windows(2).all(|w| w[0] == w[1]), "{}'s {} are not in one column: {:?}\n{}", l, what, columns, out);
}

/// What `ls`, `mounts` and `disks` printed, held to their columns.
fn judge_columns(l: &str, out: &str) {
    if l == "ls" || l.starts_with("ls ") {
        /* An entry is `f  <size>  <name>` or `d  <name>`, the names in one
           column and the sizes right-aligned */
        let entries: Vec<Vec<(usize, &str)>> = out.lines().filter(|x| x.starts_with("f ") || x.starts_with("d "))
            .map(words_at).collect();
        let names: Vec<usize> = entries.iter().filter_map(|w| w.last().map(|(at, _)| *at)).collect();
        one_column("names", l, &names, out);
        let ends: Vec<usize> = entries.iter().filter(|w| w[0].1 == "f" && w.len() == 3).map(|w| w[1].0 + w[1].1.len())
            .collect();
        one_column("sizes' ends", l, &ends, out);
        if names.len() > 1 && !ends.is_empty() {
            reached("ls: a listing in columns, files and directories");
        }
    } else if l == "mounts" {
        let lines: Vec<Vec<(usize, &str)>> = out.lines().map(words_at).filter(|w| w.len() >= 4 && w[1].1 == "on")
            .collect();
        one_column("ons", l, &lines.iter().map(|w| w[1].0).collect::<Vec<_>>(), out);
        one_column("mount points", l, &lines.iter().map(|w| w[2].0).collect::<Vec<_>>(), out);
        one_column("ro/rw", l, &lines.iter().filter_map(|w| w.last().map(|(at, _)| *at)).collect::<Vec<_>>(), out);
        if lines.len() > 2 {
            reached("mounts: three mounts and more in columns");
        }
    } else if l == "disks" {
        let lines: Vec<Vec<(usize, &str)>> = out.lines().map(words_at).filter(|w| w.len() >= 6).collect();
        let at = |word: &str| -> Vec<usize> {
            lines.iter().filter_map(|w| w.iter().find(|(_, x)| *x == word).map(|(at, _)| *at)).collect()
        };
        one_column("\"sectors\"", l, &at("sectors"), out);
        one_column("\"bytes/sector\"", l, &at("bytes/sector"), out);
        invariant!(lines.iter().all(|w| w.iter().any(|(_, x)| *x == "sectors")), "{} said a line with no sectors:\n{}", l,
                   out);
    }
}

/// What `l`, a `df`, printed, held to what it should have.
fn judge_df(l: &str, out: &str) {
    let vfs = fs::vfs_instance().expect("the VFS");
    let mounts: Vec<fs::vfs::MountInfo> = vfs.mount_infos().into_iter().flatten().collect();
    let args: Vec<&str> = l.split_whitespace().skip(1).collect();
    let mut want: Vec<Vec<String>> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    if args.is_empty() {
        want.extend(mounts.iter().map(df_words));
    }
    for p in &args {
        match df_mount_of(&mounts, p) {
            Some(m) => want.push(df_words(m)),
            None => refused.push(format!("df: {}: no filesystem holds it", p)),
        }
    }
    let mut lines = out.lines();
    for r in &refused {
        let got = lines.next();
        invariant!(got == Some(r.as_str()), "{} said {:?} where it should have said {:?}", l, got, r);
    }
    if !want.is_empty() {
        let header: Vec<&str> = lines.next().unwrap_or("").split_whitespace().collect();
        invariant!(header == ["mount", "type", "device", "size", "used", "free", "use%"], "{} began {:?}", l, header);
    }
    for w in &want {
        let got: Vec<String> = lines.next().unwrap_or("").split_whitespace().map(String::from).collect();
        invariant!(&got == w, "{} said {:?} of a mount, the judge {:?}", l, got, w);
        if w[3] != "-" {
            reached("df: a filesystem's size, used and free");
        } else {
            reached("df: a filesystem that counts nothing");
        }
    }
    let rest: Vec<&str> = lines.collect();
    invariant!(rest.is_empty(), "{} said more than it should: {:?}", l, rest);
    if !refused.is_empty() {
        reached("df: a path no filesystem holds");
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

/// An ext2 of 4 MiB in blocks of 1 KiB << log -- roomy enough for the
/// self-test's files at every block size -- labelled and numbered as given.
fn geometry(log: u32, name: &[u8], uuid: [u8; 16]) -> ext2img::Geometry {
    let bs = 1024u64 << log;
    let mut label = [0u8; 16];
    label[..name.len()].copy_from_slice(name);
    ext2img::Geometry {
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
        uuid,
    }
}

pub fn shell(r: &mut Input) {
    /* An ext2 on vda and another on vdb, each of its own block size, and
     * vdc carved in two by an MBR, the partition holding nothing yet. */
    let g = geometry(r.below(3) as u32, b"nos", [9; 16]);
    let Ok(e) = ext2img::mkfs(&g, &tree(r), &Default::default(), 1_749_000_000, r.u64()) else { return };
    let ext2_bytes = g.blocks as u64 * g.block_size() as u64;
    disk::insert(0, disk::Disk::new("vda", 512, ext2_bytes / 512, e.media));
    let g2 = geometry(r.below(3) as u32, b"data", [5; 16]);
    let Ok(e2) = ext2img::mkfs(&g2, &tree(r), &Default::default(), 1_749_000_000, r.u64()) else { return };
    let ext2b_bytes = g2.blocks as u64 * g2.block_size() as u64;
    disk::insert(1, disk::Disk::new("vdb", 512, ext2b_bytes / 512, e2.media));
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
    let out = cmd::run("mount ext2 vdb /n");
    invariant!(out.contains("mounted ext2 on /n (rw)"), "mount ext2 vdb /n said {:?}", out);

    /* The kernel's own test of each filesystem, on each as it came. */
    for (dir, big) in [("/", "300K"), ("/e", "40K"), ("/n", "64K")] {
        let out = cmd::run(&format!("fstest {} {}", dir, big));
        invariant!(out.contains("fstest: passed"), "fstest {} {} said {:?}", dir, big, out.lines().last());
    }
    reached("fstest passed on ramfs and two ext2s");

    /* Then whatever is typed. A disk a command writes to raw is no longer
     * a filesystem anybody vouches for. */
    let mut raw = [false; 3];
    let mut lines = 0;
    while r.more() && lines < 100 {
        lines += 1;
        let l = line(r);
        let out = cmd::run(&l);
        crate::targets::fsops::say(format_args!("{} -> {:?}", l, out.lines().last()));
        if l == "df" || l.starts_with("df ") {
            judge_df(&l, &out);
        }
        judge_columns(&l, &out);
        if out.contains("wrote ") {
            let target = l.split_whitespace().nth(1).unwrap_or("");
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
    for (slot, name, bytes) in [(0usize, "vda", ext2_bytes), (1, "vdb", ext2b_bytes)] {
        if raw[slot] {
            continue;
        }
        let m = disk::with(slot, |d| d.current.clone());
        let c = ext2img::check(&m, bytes);
        invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the shell's commands left {}'s ext2 {:?} {:?}", name,
                   c.corrupt, c.unclean);
    }
}
