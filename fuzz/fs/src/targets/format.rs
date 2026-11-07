//! format: the shell's `format`, on disks of every size and sector it may
//! meet, holding whatever the input put there -- nothing, noise, an ext2 or
//! an ext4 of the fuzzer's own, a partition table with a filesystem in its
//! partition -- told `force` or not, given options right and wrong, the
//! device mounted or not, its requests failing and the power going at any
//! one of them. Held to what the command says it does:
//!
//!   - what it refuses -- a filesystem it does not make, an option, a size
//!     or a sector it cannot make one on, a device in use, a disk the kernel
//!     knows partitions of, a device holding anything without `force`, a
//!     read that failed -- it refuses before a single write;
//!   - what it says it made is there: the fuzzer's own judge finds the
//!     filesystem clean and empty but for lost+found, of the blocks, block
//!     size, groups, inodes, free blocks, label and UUID the command named
//!     and the options asked for, all of it on the medium; the kernel's
//!     driver mounts it, its `fstest` passes, and the unmount leaves it
//!     clean;
//!   - a partition's making writes nothing outside the partition;
//!   - a making a failed request stopped says so, and leaves no superblock
//!     of a new filesystem;
//!   - the power going at any of its requests leaves either no superblock of
//!     the new filesystem, or the whole of it: the superblock is written
//!     last, once everything else is on the medium.
//!
//! The judges are the fuzzer's (image/), not extfmt, which made it.

use crate::image::ext2 as ext2img;
use crate::image::ext4 as ext4img;
use crate::image::part::{mbr, MbrEntry, MBR_SIGNATURE};
use crate::machine::cmd;
use crate::machine::disk::{self, Media, Op};
use crate::model::Node;
use crate::{reached, Input};
use std::borrow::Cow;

/// When the fuzzer's own images are made.
const MKFS_TIME: u32 = 1_749_000_000;

/// The superblock, at 1 KiB whatever the block size, and what is read of it.
const SB: u64 = 1024;
const SB_INODES: usize = 0x00;
const SB_BLOCKS: usize = 0x04;
const SB_FIRST_DATA_BLOCK: usize = 0x14;
const SB_LOG_BLOCK_SIZE: usize = 0x18;
const SB_BLOCKS_PER_GROUP: usize = 0x20;
const SB_MAGIC: usize = 0x38;
const SB_COMPAT: usize = 0x5C;
const SB_INCOMPAT: usize = 0x60;
const SB_UUID: usize = 0x68;
const SB_LABEL: usize = 0x78;
const EXT_MAGIC: u16 = 0xEF53;
const COMPAT_HAS_JOURNAL: u32 = 0x4;
const INCOMPAT_EXTENTS: u32 = 0x40;

/// Where a partition starts, in the 512-byte sectors of the disk under it.
const PART_START: u64 = 2048;

/// A device the power is cut at every request of: at most this big, and
/// that many requests from the command's first.
const DENSE_CUTS_MAX: u64 = 3 << 20;
const DENSE_CUTS: u64 = 512;

/// The biggest filesystem written out for e2fsck (FS_FUZZ_DUMP): an image
/// is written whole, holes and all.
const DUMP_MAX: u64 = 64 << 20;

/// What a filesystem the kernel's `fstest` runs on must have, at least: the
/// self-test's files, a far one's tree, a disk's image of 96 KiB.
const FSTEST_FREE: u64 = 2 << 20;
const FSTEST_INODES: u64 = 64;

/// What is on the disk before the command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Before {
    Zeros,
    Noise,
    Ext2,
    Ext4,
    /// An MBR, its one partition zeros or an ext2 of the fuzzer's own.
    Table,
}

/// What the command said it made.
#[derive(Debug, Default)]
struct Said {
    kind: String,
    blocks: u64,
    block_size: u64,
    groups: u64,
    inodes: u64,
    journal: u64,
    free: u64,
    uuid: [u8; 16],
}

/// The number before `word` in `line`.
fn before(line: &str, word: &str) -> Option<u64> {
    let i = line.find(word)?;
    line[..i].split_whitespace().last()?.parse().ok()
}

/// "formatted vda as ext4: N blocks of B bytes in G groups, I inodes[, a
/// journal of J blocks], F free; uuid U", read back.
fn said(out: &str, name: &str) -> Option<Said> {
    let head = format!("formatted {} as ", name);
    let line = out.lines().find(|l| l.starts_with(&head))?;
    let rest = &line[head.len()..];
    let (kind, rest) = rest.split_once(':')?;
    let journal = match rest.find("a journal of ") {
        Some(i) => rest[i + "a journal of ".len()..].split_whitespace().next()?.parse().ok()?,
        None => 0,
    };
    let hex: String = rest.split_once("uuid ")?.1.trim().chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut uuid = [0u8; 16];
    for (k, b) in uuid.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * k..2 * k + 2], 16).ok()?;
    }
    Some(Said {
        kind: kind.to_string(),
        blocks: before(rest, " blocks of")?,
        block_size: before(rest, " bytes in")?,
        groups: before(rest, " groups")?,
        inodes: before(rest, " inodes")?,
        journal,
        free: before(rest, " free;")?,
        uuid,
    })
}

/// The superblock's magic and UUID in `m`, if it has the magic.
fn sb_uuid(m: &Media) -> Option<[u8; 16]> {
    let s = m.bytes(SB, 1024);
    (ext2img::rd16(&s, SB_MAGIC) == EXT_MAGIC).then(|| s[SB_UUID..SB_UUID + 16].try_into().expect("16 bytes"))
}

/// A label for `label=`: printable ASCII mostly, now and then too long,
/// empty, or not ASCII at all.
fn label(r: &mut Input) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.~!";
    match r.u8() % 12 {
        0 => String::new(),
        1 => "\u{e9}t\u{e9}".to_string(),
        _ => {
            let n = if r.chance(6) { r.range(17, 24) } else { r.range(1, 16) };
            (0..n).map(|_| CHARS[r.below(CHARS.len() as u64) as usize] as char).collect()
        }
    }
}

/// The options of a command line: (the words, the label the last label=
/// gives -- None for none -- and the block size the last bs= gives).
fn options(r: &mut Input) -> (Vec<String>, Option<String>, Option<u64>) {
    let (mut words, mut lab, mut bs) = (Vec::new(), None, None);
    /* Half the time only what the command takes -- a label of its own and
     * a block size of ext's -- for a filesystem made more often than not. */
    if r.bool() {
        if r.bool() {
            let l = label(r);
            if l.len() <= 16 && l.bytes().all(|c| c.is_ascii_graphic()) {
                words.push(format!("label={}", l));
                lab = Some(l);
            }
        }
        if r.bool() {
            let b = r.pick(&[1024u64, 2048, 4096]);
            words.push(format!("bs={}", b));
            bs = Some(b);
        }
        return (words, lab, bs);
    }
    for _ in 0..r.below(4) {
        match r.u8() % 8 {
            0 | 1 => {
                let l = label(r);
                words.push(format!("label={}", l));
                lab = Some(l);
            }
            2 | 3 | 4 => {
                let b = r.pick(&["1024", "2048", "4096", "512", "8192", "x", ""]);
                words.push(format!("bs={}", b));
                bs = b.parse().ok();
            }
            5 => words.push(r.pick(&["colour=red", "label", "=", "bs", "ro"]).to_string()),
            _ => words.push("force".to_string()),
        }
    }
    (words, lab, bs)
}

/// The device's size in bytes, before it is cut to whole sectors.
fn size(r: &mut Input) -> u64 {
    match r.u8() % 8 {
        0 => r.below(256 << 10),
        1 => r.range(1 << 20, 3 << 20),
        2..=5 => r.range(2 << 20, 48 << 20),
        6 => r.range(48 << 20, 160 << 20),
        /* Past the size where 4 KiB blocks start, now and then. */
        _ => if r.chance(4) { r.range(500 << 20, 560 << 20) } else { 8 << 20 },
    }
}

/// An ext2 of the fuzzer's own: its medium and bytes.
fn ext2_of(r: &mut Input) -> Option<(Media, u64)> {
    let g = super::ext2::geometry(r);
    let made = ext2img::mkfs(&g, &Node::dir(), &Default::default(), MKFS_TIME, r.u64()).ok()?;
    Some((made.media, g.blocks as u64 * g.block_size() as u64))
}

/// An ext4 of the fuzzer's own: its medium and bytes.
fn ext4_of(r: &mut Input) -> Option<(Media, u64)> {
    let g = super::ext4::geometry(r);
    let made = ext4img::mkfs(&g, &Node::dir(), &Default::default(), MKFS_TIME, r.u64()).ok()?;
    Some((made.media, g.blocks * g.block_size()))
}

/// The bytes of `m` from `at`, `len` of them, as a medium of their own.
fn region(m: &Media, at: u64, len: u64) -> Cow<'_, Media> {
    if at == 0 {
        return Cow::Borrowed(m);
    }
    Cow::Owned(Media::from_bytes(&m.bytes(at, len as usize)))
}

/// The judge's verdict on a filesystem of `kind` made on `m`, `len` bytes:
/// what is wrong with it, nothing when it is clean, its tree lost+found
/// alone where `empty`. And its free blocks, in bytes, as the judge counts
/// them -- None where it did not get that far.
fn judge(kind: &str, m: &Media, len: u64, empty: bool) -> (Vec<String>, Option<u64>) {
    /* The fuzzer's own code, and its memory: a judge of a filesystem of
     * hundreds of megabytes takes more at once than the kernel's heap gives
     * one allocation. */
    crate::machine::sched::harness(|| judge_of(kind, m, len, empty))
}

fn judge_of(kind: &str, m: &Media, len: u64, empty: bool) -> (Vec<String>, Option<u64>) {
    let mut lpf = std::collections::BTreeMap::new();
    lpf.insert(b"lost+found".to_vec(), Node::dir());
    let want = Node::Dir(lpf);
    let (mut wrong, tree, data, free) = if kind == "ext4" {
        let c = ext4img::check(m, len);
        let mut w: Vec<String> = c.corrupt.into_iter().chain(c.unclean).collect();
        if c.replayed != 0 || c.orphans != 0 || c.mounted {
            w.push(format!("replayed {}, orphans {}, left mounted {}", c.replayed, c.orphans, c.mounted));
        }
        (w, c.tree, c.data_bytes, c.free_bytes)
    } else {
        let c = ext2img::check(m, len);
        (c.corrupt.into_iter().chain(c.unclean).collect::<Vec<_>>(), c.tree, c.data_bytes, c.free_bytes)
    };
    if empty && wrong.is_empty() && tree.as_ref() != Some(&want) {
        wrong.push(format!("its tree is {:?}, not lost+found alone", tree));
    }
    (wrong, (data != 0).then_some(free))
}

pub fn format(r: &mut Input) {
    /* The disk, and what is on it. */
    let before = match r.u8() % 8 {
        0 | 1 | 2 => Before::Zeros,
        3 => Before::Noise,
        4 => Before::Ext2,
        5 => Before::Ext4,
        _ => Before::Table,
    };
    let sector: u64 = if before != Before::Table && r.chance(4) { 4096 } else { 512 };
    let mut media = Media::new();
    let mut bytes = size(r);
    /* The partition, where there is one: its first byte and its length. */
    let mut part: Option<(u64, u64)> = None;
    let mut part_fs = false;
    match before {
        Before::Zeros => {}
        Before::Noise => {
            let n = bytes.min(128 << 10) as usize;
            let mut head = crate::input::noise(r.u32(), n);
            /* No superblock of ext's by chance: the target tells a new one
             * from an old one by it. */
            if head.len() > SB as usize + SB_MAGIC {
                head[SB as usize + SB_MAGIC] = 0;
            }
            media = Media::from_bytes(&head);
        }
        Before::Ext2 | Before::Ext4 => {
            let Some((m, len)) = (if before == Before::Ext2 { ext2_of(r) } else { ext4_of(r) }) else { return };
            media = m;
            bytes = len + r.below(1 << 20);
        }
        Before::Table => {
            let mut len = r.range(64 << 10, 32 << 20) / 512 * 512;
            let mut inside = Media::new();
            if r.bool() {
                let Some((m, n)) = ext2_of(r) else { return };
                inside = m;
                len = n.div_ceil(512) * 512 + r.below(64) * 512;
                part_fs = true;
            }
            let entry = MbrEntry { status: 0, kind: 0x83, start: PART_START as u32, size: (len / 512) as u32 };
            let none = MbrEntry::default();
            media = Media::from_bytes(&mbr(&[entry, none, none, none], MBR_SIGNATURE));
            inside.copy_into(&mut media, PART_START * 512);
            part = Some((PART_START * 512, len));
            bytes = PART_START * 512 + len + r.below(256) * 512;
        }
    }
    let sectors = bytes / sector;
    let dev_bytes = sectors * sector;
    let mut d = disk::Disk::new("vda", sector, sectors, media.clone());
    d.seed_chaos(r.u64());
    d.volatile = r.u8() % 3 != 0;
    d.latency = r.chance(32);
    if r.chance(4) {
        d.take_async(r.pick(&[4096, 65536, 1 << 20]), r.range(1, 32) as usize);
    }
    disk::insert(0, d);
    let Some(_dev) = disk::register(0) else { return };
    if before == Before::Table {
        block::rust_partitions_probe();
    }
    invariant!(fs::ramfs::mount_at("/", false), "a ramfs would not mount at the root");

    /* What the command is pointed at: the disk, or its partition. */
    let on_part = part.is_some() && r.u8() % 4 != 0;
    let name = if on_part { "vda1" } else { "vda" };
    let (at, len) = if on_part { part.expect("a partition") } else { (0, dev_bytes) };
    /* Now and then what is on it mounted first: the command must not touch
     * a device in use. */
    let mounted = match before {
        Before::Ext2 | Before::Ext4 if r.chance(4) => {
            let kind = if before == Before::Ext2 { "ext2" } else { "ext4" };
            cmd::run(&format!("mount {} vda /m", kind)).contains("mounted")
        }
        Before::Table if part_fs && r.chance(3) => cmd::run("mount ext2 vda1 /m").contains("mounted"),
        _ => false,
    };

    /* The command line. */
    let fs_name = match r.u8() % 16 {
        0 => r.pick(&["xfs", "ext3", "", "EXT4"]),
        k if k % 2 == 0 => "ext4",
        _ => "ext2",
    };
    let (words, lab, bs) = options(r);
    let force = words.iter().any(|w| w == "force") || (before != Before::Zeros && r.u8() % 4 != 0);
    let mut line = format!("format {} {}", fs_name, if r.chance(64) { "vdz" } else { name });
    for w in &words {
        line += " ";
        line += w;
    }
    if force && !words.iter().any(|w| w == "force") {
        line += " force";
    }

    /* Requests that fail, and power cuts, from the command's first. */
    let mark = disk::with(0, |d| {
        let mark = d.log.len() as u64;
        let span = r.pick(&[4u64, 32, 512, 4096]);
        if r.chance(6) {
            for _ in 0..r.range(1, 3) {
                d.fail.insert(mark + r.below(span));
            }
        }
        if r.chance(24) {
            d.dead_from = Some(mark + r.below(span));
        }
        if d.volatile {
            for _ in 0..r.below(4) {
                d.crash_at.insert(mark + r.below(span));
            }
            /* On a small device now and then the power going at every one
             * of the command's requests: the one between its superblock and
             * its last flush among them. */
            if dev_bytes <= DENSE_CUTS_MAX && r.chance(4) {
                d.crash_at.extend(mark..mark + DENSE_CUTS);
            }
        }
        mark as usize
    });

    let out = cmd::run(&line);
    crate::targets::fsops::say(format_args!("{} -> {:?}", line, out.lines().last()));
    let (log, current, crashes, cached) = disk::with(0, |d| {
        d.fail.clear();
        d.dead_from = None;
        d.crash_at.clear();
        (d.log[mark..].to_vec(), d.current.clone(), std::mem::take(&mut d.crashes), d.cache.len())
    });
    let wrote = log.iter().any(|io| matches!(io.op, Op::Write | Op::WriteFua));
    let failed = log.iter().any(|io| !io.ok);
    let made = out.contains("formatted ");
    let old_uuid = sb_uuid(&region(&media, at, len));

    /* A refusal writes nothing; a write that failed is said. */
    invariant!(!(made && failed), "{}: said {:?} with a request failed", line, out);
    if !made && wrote {
        invariant!(failed && (out.contains("what is on it now is no filesystem")
                              || out.contains("did not reach the device")),
                   "{}: wrote, and neither made a filesystem nor said why not: {:?}", line, out);
        reached("a format stopped by a failed request");
    }
    if mounted || (part.is_some() && !on_part) {
        invariant!(!wrote, "{}: wrote to a device in use, or to a disk of partitions: {:?}", line, out);
    }
    if !force && before != Before::Zeros && !(before == Before::Table && on_part && !part_fs) {
        invariant!(!wrote, "{}: wrote over {:?} without force: {:?}", line, before, out);
        if out.contains("holds ") {
            reached("format refused a device holding something");
        }
    }
    if !wrote {
        reached("format refused before writing");
    }

    /* A partition's making: nothing outside it. */
    if wrote && on_part {
        let (pat, plen) = part.expect("a partition");
        invariant!(current.bytes(0, pat as usize) == media.bytes(0, pat as usize),
                   "{}: wrote before its partition", line);
        let end = pat + plen;
        if end < dev_bytes {
            invariant!(current.bytes(end, (dev_bytes - end) as usize) == media.bytes(end, (dev_bytes - end) as usize),
                       "{}: wrote past its partition", line);
        }
        reached("a partition formatted, nothing outside it touched");
    }

    /* Whatever happened, the device -- as it is now, and as the power
     * going at any request left it -- holds the superblock it held, none,
     * or a new filesystem whole: never the start of one. */
    for (n, pic) in crashes.iter().map(|(n, m)| (Some(*n), m)).chain(std::iter::once((None, &current))) {
        let pic = region(pic, at, len);
        let now = sb_uuid(&pic);
        if now.is_none() || now == old_uuid {
            if n.is_some() {
                reached("a power cut before format's superblock");
            }
            continue;
        }
        let (wrong, _) = judge(fs_name, &pic, len, true);
        invariant!(wrong.is_empty(), "{}: {} a superblock over {:?}", line,
                   n.map_or("ended with".to_string(), |n| format!("the power going before request {} leaves", n)),
                   wrong);
        if n.is_some() {
            reached("a power cut after format's superblock: the whole filesystem there");
        }
    }

    if !made {
        disk_done();
        return;
    }

    /* What it said it made is what is there. */
    let Some(s) = said(&out, name) else { panic!("invariant: {}: said {:?}, which does not read", line, out) };
    invariant!(s.kind == fs_name, "{}: made {} where {} was asked", line, s.kind, fs_name);
    invariant!(cached == 0, "{}: done with {} writes still in the device's cache", line, cached);
    let m = region(&current, at, len);
    let sb = m.bytes(SB, 1024);
    let rd32 = |o: usize| ext2img::rd32(&sb, o) as u64;
    let first = rd32(SB_FIRST_DATA_BLOCK);
    let bpg = rd32(SB_BLOCKS_PER_GROUP).max(1);
    let block_size = 1024u64 << rd32(SB_LOG_BLOCK_SIZE).min(16);
    invariant!(rd32(SB_BLOCKS) == s.blocks && block_size == s.block_size && rd32(SB_INODES) == s.inodes
               && (s.blocks.saturating_sub(first)).div_ceil(bpg) == s.groups,
               "{}: said {:?}; the superblock has {} blocks of {}, {} inodes, {} a group from {}", line, s,
               rd32(SB_BLOCKS), block_size, rd32(SB_INODES), bpg, first);
    invariant!(s.blocks * s.block_size <= len, "{}: {} blocks of {} on {} bytes", line, s.blocks, s.block_size, len);
    invariant!(sb[SB_UUID..SB_UUID + 16] == s.uuid, "{}: the superblock's UUID is not the one said", line);
    invariant!(s.uuid[6] >> 4 == 4 && s.uuid[8] >> 6 == 2, "{}: a UUID that is not a random one's: {:?}", line, s.uuid);
    let mut want_label = [0u8; 16];
    if let Some(l) = &lab {
        invariant!(l.len() <= 16 && l.bytes().all(|c| c.is_ascii_graphic()), "{}: made, given the label {:?}", line, l);
        want_label[..l.len()].copy_from_slice(l.as_bytes());
    }
    invariant!(sb[SB_LABEL..SB_LABEL + 16] == want_label, "{}: the label is {:?}", line, &sb[SB_LABEL..SB_LABEL + 16]);
    if let Some(b) = bs {
        invariant!(s.block_size == b, "{}: blocks of {} where bs={} was asked", line, s.block_size, b);
    }
    let ext4 = s.kind == "ext4";
    let (compat, incompat) = (rd32(SB_COMPAT) as u32, rd32(SB_INCOMPAT) as u32);
    invariant!((compat & COMPAT_HAS_JOURNAL != 0) == ext4 && (incompat & INCOMPAT_EXTENTS != 0) == ext4
               && (s.journal != 0) == ext4, "{}: an {} of features {:#x}/{:#x}, journal {}", line, s.kind, compat,
               incompat, s.journal);
    let (wrong, free) = judge(&s.kind, &m, len, true);
    invariant!(wrong.is_empty(), "{}: the judge finds {:?}", line, wrong);
    if let Some(free) = free {
        invariant!(free == s.free * s.block_size, "{}: said {} blocks free, the judge counts {} bytes", line, s.free,
                   free);
    }
    reached(if ext4 { "format made an ext4 the judge passes" } else { "format made an ext2 the judge passes" });
    if len <= DUMP_MAX {
        super::ext2::dump("format", &m, len);
    }

    /* The kernel's own driver over it. */
    let out = cmd::run(&format!("mount {} {} /f", s.kind, name));
    invariant!(out.contains(&format!("mounted {} on /f (rw)", s.kind)), "{}: then its mount said {:?}", line, out);
    if s.free * s.block_size >= FSTEST_FREE && s.inodes >= FSTEST_INODES {
        let out = cmd::run("fstest /f 64K");
        invariant!(out.contains("fstest: passed"), "{}: fstest on it said {:?}", line, out.lines().last());
        reached("fstest passed on a filesystem format made");
    }
    let out = cmd::run("umount /f");
    invariant!(out.contains("unmounted /f"), "{}: its unmount said {:?}", line, out);
    let after = disk::with(0, |d| d.current.clone());
    let after = region(&after, at, len);
    let (wrong, _) = judge(&s.kind, &after, len, false);
    invariant!(wrong.is_empty(), "{}: what the driver left on it: {:?}", line, wrong);
    if len <= DUMP_MAX {
        super::ext2::dump("format-used", &after, len);
    }
    disk_done();
}

/// Everything mounted let go of: the input's tasks done.
fn disk_done() {
    if let Some(vfs) = fs::vfs_instance() {
        vfs.unmount_all();
    }
}
