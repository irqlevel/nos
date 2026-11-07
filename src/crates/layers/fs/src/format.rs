//! `format`: a filesystem made on a disk or a partition.
//!
//! What the command does is the same whatever it makes. It finds the
//! device and takes it for itself (`block::claim_as`), so that nothing
//! mounted, no disk log and no other writer is on it or on the disk under
//! it while it writes; it refuses a whole disk the kernel knows partitions
//! of; it has the filesystem's maker plan the filesystem for the device's
//! size, which refuses an option or a device it cannot make one of; then --
//! unless told `force` -- it refuses a device that holds anything a look at
//! its two ends finds (`diskimage::contents`): a filesystem, a partition
//! table or a volume, named, or any byte at its start that is not a zero;
//! and it writes what the plan hands it, in batches of pages through the
//! device's queue, and says what it made.
//!
//! A filesystem is a [`Maker`] in [`MAKERS`]: its name, the options it
//! takes, and the [`Plan`] it makes for a device of a given size -- the
//! writes, in order, that leave the device holding it. That is all a new
//! filesystem adds, with its name in the command's help (`shell.rs`): ext2
//! and ext4 are one maker over `extfmt::mkfs`, which lays them out with no
//! kernel in it.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use block::Disk;
use extfmt::mkfs::{self, Kind};
use kcore::cmd::Output;
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;

/// What `format`'s claim on its device says to whoever is refused it.
const FORMAT_HOLDER: &core::ffi::CStr = c"format";

/// The pages a batch of writes goes to the device in: a piece of each, in
/// flight together where the device takes several.
const BATCH_PAGES: usize = 32;

/// One write of a filesystem's making.
#[derive(Clone, Copy, Debug)]
pub enum Piece<'a> {
    /// These bytes at byte `at` of the device: whole sectors.
    Bytes { at: u64, data: &'a [u8] },
    /// `len` bytes of zeros from byte `at`: whole sectors.
    Zeros { at: u64, len: u64 },
    /// Everything before it on the device, past its cache, before anything
    /// after it is written.
    Barrier,
}

/// A filesystem `format` makes.
pub trait Maker: Sync {
    /// The name `format` is told to make it by.
    fn name(&self) -> &'static str;
    /// The options it takes, as `format`'s usage lists them.
    fn options(&self) -> &'static str;
    /// The filesystem it makes on a device of `bytes` bytes in sectors of
    /// `sector_size`, given `options` -- the words after the device, `force`
    /// taken out -- or why there is none.
    fn plan(&self, bytes: u64, sector_size: u64, options: &[&str]) -> Result<Box<dyn Plan>, String>;
}

/// A filesystem planned for a device.
pub trait Plan {
    /// What it is, as `format` says it once made.
    fn describe(&self, out: &mut dyn Write) -> core::fmt::Result;
    /// Every write of its making, to `put` in order; the first `put`
    /// refuses stops them.
    fn write(&self, put: &mut dyn FnMut(Piece<'_>) -> bool) -> Result<(), String>;
}

/// The filesystems `format` makes: a new one is a [`Maker`], a line here,
/// and its name in the command's help.
static MAKERS: [&dyn Maker; 2] = [&Ext(Kind::Ext2), &Ext(Kind::Ext4)];

/* ---- ext2 and ext4 ---- */

/// The ext family, laid out by `extfmt::mkfs`.
struct Ext(Kind);

/// A label is the superblock's 16 bytes: printable ASCII, no more.
const LABEL_MAX: usize = 16;

impl Maker for Ext {
    fn name(&self) -> &'static str {
        match self.0 {
            Kind::Ext2 => "ext2",
            Kind::Ext4 => "ext4",
        }
    }

    fn options(&self) -> &'static str {
        "[label=<name>] [bs=1024|2048|4096]"
    }

    fn plan(&self, bytes: u64, sector_size: u64, options: &[&str]) -> Result<Box<dyn Plan>, String> {
        let mut o = mkfs::Options {
            kind: self.0,
            block_size: None,
            label: [0; LABEL_MAX],
            uuid: [0; 16],
            hash_seed: [0; 16],
            now: kcore::time::wall_clock_secs() as u32,
        };
        for word in options {
            match word.split_once('=') {
                Some(("label", v)) if v.len() <= LABEL_MAX && v.bytes().all(|c| c.is_ascii_graphic()) => {
                    /* The last one given: nothing of an earlier one left. */
                    o.label = [0; LABEL_MAX];
                    o.label[..v.len()].copy_from_slice(v.as_bytes());
                }
                Some(("label", _)) => return Err(format!("a label is at most {} printable characters", LABEL_MAX)),
                Some(("bs", v)) => {
                    o.block_size = Some(v.parse().map_err(|_| String::from("bs is 1024, 2048 or 4096"))?);
                }
                _ => return Err(format!("{} takes no '{}'", self.name(), word)),
            }
        }
        if !kcore::random::fill_random(&mut o.uuid) || !kcore::random::fill_random(&mut o.hash_seed) {
            return Err(String::from("no randomness for the filesystem's UUID"));
        }
        /* A random UUID as RFC 4122 marks one: version 4, its variant. */
        o.uuid[6] = (o.uuid[6] & 0x0F) | 0x40;
        o.uuid[8] = (o.uuid[8] & 0x3F) | 0x80;
        let layout = mkfs::Layout::new(bytes, sector_size, &o).map_err(String::from)?;
        Ok(Box::new(ExtPlan { layout, uuid: o.uuid }))
    }
}

struct ExtPlan {
    layout: mkfs::Layout,
    uuid: [u8; 16],
}

impl Plan for ExtPlan {
    fn describe(&self, out: &mut dyn Write) -> core::fmt::Result {
        let l = &self.layout;
        let name = if l.kind() == Kind::Ext4 { "ext4" } else { "ext2" };
        write!(out, "{}: {} blocks of {} bytes in {} groups, {} inodes", name, l.blocks(), l.block_size(), l.groups(),
               l.inodes())?;
        if l.journal_blocks() > 0 {
            write!(out, ", a journal of {} blocks", l.journal_blocks())?;
        }
        write!(out, ", {} free; uuid ", l.free_blocks())?;
        for (i, b) in self.uuid.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                out.write_char('-')?;
            }
            write!(out, "{:02x}", b)?;
        }
        Ok(())
    }

    fn write(&self, put: &mut dyn FnMut(Piece<'_>) -> bool) -> Result<(), String> {
        self.layout
            .write(&mut |p| put(match p {
                mkfs::Piece::Bytes { at, data } => Piece::Bytes { at, data },
                mkfs::Piece::Zeros { at, len } => Piece::Zeros { at, len },
                mkfs::Piece::Barrier => Piece::Barrier,
            }))
            .map_err(String::from)
    }
}

/* ---- the command ---- */

/// The claim on the device, given back however the command ends.
struct Held(usize);

impl Drop for Held {
    fn drop(&mut self) {
        block::release(self.0);
    }
}

/// Writes to a device, gathered a page a piece into one DMA buffer and
/// handed to the device a batch at a time.
struct Writer<'a> {
    dev: &'a Disk,
    buf: DmaBuffer,
    pieces: Vec<block::Piece>,
    sector: u64,
    /// Why a write could not be made, once one could not.
    failed: Option<&'static str>,
}

impl<'a> Writer<'a> {
    fn new(dev: &'a Disk) -> Option<Writer<'a>> {
        let buf = DmaBuffer::new(BATCH_PAGES)?;
        let mut pieces = Vec::new();
        pieces.try_reserve_exact(BATCH_PAGES).ok()?;
        Some(Writer { dev, buf, pieces, sector: dev.sector_size(), failed: None })
    }

    fn put(&mut self, p: Piece<'_>) -> bool {
        let ok = match p {
            Piece::Bytes { at, data } => {
                let mut ok = true;
                for (k, chunk) in data.chunks(PAGE_SIZE).enumerate() {
                    ok = ok && self.add(at + (k * PAGE_SIZE) as u64, chunk.len(), Some(chunk));
                }
                ok
            }
            Piece::Zeros { at, len } => {
                let (mut done, mut ok) = (0u64, true);
                while ok && done < len {
                    let n = (len - done).min(PAGE_SIZE as u64) as usize;
                    ok = self.add(at + done, n, None);
                    done += n as u64;
                }
                ok
            }
            Piece::Barrier => self.submit() && self.flush(),
        };
        ok && self.failed.is_none()
    }

    /// A piece of a page: `data`, or zeros, `len` bytes at byte `at`.
    fn add(&mut self, at: u64, len: usize, data: Option<&[u8]>) -> bool {
        if self.sector == 0 || at % self.sector != 0 || len as u64 % self.sector != 0 || len > PAGE_SIZE {
            self.failed = Some("a write of no whole sectors");
            return false;
        }
        if self.pieces.len() == BATCH_PAGES && !self.submit() {
            return false;
        }
        let page = self.pieces.len() * PAGE_SIZE;
        let room = &mut self.buf.as_mut_slice()[page..page + len];
        match data {
            Some(d) => room.copy_from_slice(d),
            None => room.fill(0),
        }
        self.pieces.push(block::Piece { sector: at / self.sector, at: page, len });
        true
    }

    fn submit(&mut self) -> bool {
        if self.pieces.is_empty() {
            return true;
        }
        let ok = self.dev.write_pieces(&self.buf, &self.pieces).is_ok();
        self.pieces.clear();
        if !ok {
            self.failed = Some("the device failed a write");
        }
        ok
    }

    fn flush(&mut self) -> bool {
        let ok = self.dev.flush().is_ok();
        if !ok {
            self.failed = Some("the device failed a flush");
        }
        ok
    }
}

/// What the device holds, from its first `contents::HEAD` bytes and its
/// last `contents::TAIL`: None for nothing, or a read that failed.
fn holds(dev: &Disk, size: u64) -> Result<Option<diskimage::contents::Found>, &'static str> {
    use diskimage::contents::{contents, HEAD, TAIL};
    let sector = dev.sector_size();
    if sector == 0 || sector > PAGE_SIZE as u64 || PAGE_SIZE as u64 % sector != 0 {
        return Err("a sector size it cannot read a page at a time");
    }
    let mut page = DmaBuffer::new(1).ok_or("no memory to read the device")?;
    let mut read = |from: u64, len: usize| -> Result<Vec<u8>, &'static str> {
        let mut out = Vec::new();
        out.try_reserve_exact(len).map_err(|_| "no memory to read the device")?;
        let mut at = from;
        while out.len() < len {
            let n = (len - out.len()).min(PAGE_SIZE);
            let buf = &mut page.as_mut_slice()[..n];
            dev.read(at / sector, buf).map_err(|_| "a read of the device failed")?;
            out.extend_from_slice(buf);
            at += n as u64;
        }
        Ok(out)
    };
    let head_len = (HEAD as u64).min(size) / sector * sector;
    let tail_len = (TAIL as u64).min(size) / sector * sector;
    let head = read(0, head_len as usize)?;
    let tail = read(size - tail_len, tail_len as usize)?;
    Ok(contents(&head, &tail, size))
}

fn usage(out: &mut Output) {
    let _ = writeln!(out, "usage: format <filesystem> <disk> [options] [force]");
    for m in MAKERS.iter() {
        let _ = writeln!(out, "       format {} <disk> {} [force]", m.name(), m.options());
    }
    let _ = writeln!(out, "force: over what the disk holds now");
}

pub fn format(args: &str, out: &mut Output) {
    let mut words = args.split_whitespace();
    let (Some(fs), Some(name)) = (words.next(), words.next()) else {
        usage(out);
        return;
    };
    let mut force = false;
    let mut options: [&str; 8] = [""; 8];
    let mut count = 0;
    for w in words {
        if w == "force" {
            force = true;
        } else if count < options.len() {
            options[count] = w;
            count += 1;
        } else {
            let _ = writeln!(out, "format: too many options");
            return;
        }
    }
    let Some(maker) = MAKERS.iter().find(|m| m.name() == fs) else {
        let _ = write!(out, "format: no filesystem '{}'; format makes", fs);
        for m in MAKERS.iter() {
            let _ = write!(out, " {}", m.name());
        }
        let _ = writeln!(out);
        return;
    };
    let Some(dev) = Disk::open(name) else {
        let _ = writeln!(out, "disk '{}' not found", name);
        return;
    };
    /* A disk whose partitions the kernel has registered: what is written
     * over the table, the partitions would go on naming -- the table's
     * entries are the kernel's for good. One of them is formatted instead. */
    if dev.parent().is_none() && dev.partitions() > 0 {
        let _ = writeln!(out, "format: {} holds partitions the kernel knows ({}): format one of them", name,
                         dev.partitions());
        return;
    }
    let _held = match block::claim_as(dev.handle(), FORMAT_HOLDER) {
        Ok(claim) => Held(claim),
        Err(held_by) => {
            let _ = writeln!(out, "disk '{}' is in use by {}", name, held_by);
            return;
        }
    };
    /* The plan first: an option it does not take, or a device too small or
     * of sectors it cannot write, is said before anything is read. */
    let (sector, size) = (dev.sector_size(), dev.sectors().saturating_mul(dev.sector_size()));
    let plan = match maker.plan(size, sector, &options[..count]) {
        Ok(plan) => plan,
        Err(why) => {
            let _ = writeln!(out, "format: {}: {}", name, why);
            return;
        }
    };
    if !force {
        match holds(&dev, size) {
            Ok(None) => {}
            Ok(Some(found)) => {
                let _ = writeln!(out, "format: {} holds {} -- add force to format over it", name, found);
                return;
            }
            Err(why) => {
                let _ = writeln!(out, "format: {}: {}", name, why);
                return;
            }
        }
    }
    let Some(mut writer) = Writer::new(&dev) else {
        let _ = writeln!(out, "format: no memory for the writes");
        return;
    };
    let written = plan.write(&mut |p| writer.put(p));
    let flushed = written.is_ok() && writer.submit() && writer.flush();
    if let Some(why) = writer.failed {
        let _ = writeln!(out, "format: {}: {} -- what is on it now is no filesystem", name, why);
        return;
    }
    if let Err(why) = written {
        let _ = writeln!(out, "format: {}: {} -- what is on it now is no filesystem", name, why);
        return;
    }
    if !flushed {
        let _ = writeln!(out, "format: {}: the last writes did not reach the device", name);
        return;
    }
    let _ = write!(out, "formatted {} as ", name);
    let _ = plan.describe(out);
    let _ = writeln!(out);
}
