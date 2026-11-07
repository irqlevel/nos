//! `hv convert <image> <disk>`: a cloud's disk image -- QEMU's qcow2, which
//! Ubuntu, Debian and Fedora ship theirs in -- made the raw disk a guest is
//! given (`disk=`), by the `diskimage` crate. The disk is a new file, grown
//! to the image's size with a hole, and only what is not zeros is written
//! into it: Ubuntu's 3.5 GiB disk costs the 1.7 GiB it holds.
//!
//! Or the disk is a device of nos's (`crate::devices`), a partition or a
//! whole disk, which the image is poured onto (`pour`): every byte of the
//! image's disk written -- what the converter leaves out as zeros, since a
//! device reads as whatever it held -- through the device's queue, many
//! writes at once, and the rest of the device given back to it where it
//! takes that. A device is claimed while it is written: nothing else has it
//! meanwhile.
//!
//! Nothing is written over -- a disk that is there already is refused --
//! and a conversion that fails leaves nothing behind: the disk is removed,
//! and what was wrong is said, with where. A file that is not qcow2 is
//! refused by what it is -- a raw disk is given to a guest as it is, a
//! compressed stream is unpacked first -- not as a damaged image.

use alloc::string::String;
use alloc::vec::Vec;

use core::cell::Cell;

use diskimage::qcow2::{Error, Qcow2, Stats};
use diskimage::{detect, Format};
#[cfg(target_arch = "x86_64")]
use kcore::block::{DiskQueue, Started};
use kcore::consts::NS_PER_SEC;
use kcore::fs::File;
#[cfg(target_arch = "x86_64")]
use kcore::time::Duration;

use crate::work::{self, Work};

const USAGE: &str = "hv convert <image> <disk|device> [force] -- a qcow2 image made a raw disk, its holes kept, or \
                     poured onto a device of nos's (part:<id>, disk:<id>, dev:<name>): force over what is on it";
const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
/// How far apart the progress lines are, as wget's: a tenth of the time
/// so far, from two seconds to thirty.
const REPORT_MIN_NS: u64 = 2 * NS_PER_SEC;
const REPORT_MAX_NS: u64 = 30 * NS_PER_SEC;
const REPORT_SHARE: u64 = 10;
/// Tenths of a second, for the time a conversion took.
const NS_PER_TENTH: u64 = NS_PER_SEC / 10;

/// The image, read where the converter asks.
struct Image {
    file: File,
    len: u64,
}

impl diskimage::Source for Image {
    type Error = kcore::error::Error;

    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> kcore::error::Result<()> {
        match self.file.read_at(offset, buf) {
            Ok(got) if got == buf.len() => Ok(()),
            Ok(_) => Err(kcore::error::Error::IoError),
            Err(e) => Err(e),
        }
    }
}

/// The disk, written where the converter says: always inside its size,
/// which `set_len` gave it first -- and nothing more once `stopped` is set,
/// which ends the conversion.
struct Disk<'a> {
    file: &'a File,
    stopped: &'a Cell<bool>,
}

impl diskimage::Sink for Disk<'_> {
    type Error = kcore::error::Error;

    fn write_at(&mut self, offset: u64, data: &[u8]) -> kcore::error::Result<()> {
        if self.stopped.get() {
            return Err(kcore::error::Error::Busy);
        }
        self.file.write_at(offset, data)
    }
}

/// `hv convert <image> <disk|device> [force]`.
pub fn command(root: &str, args: &str, work: &mut dyn Work) {
    let mut words = args.split_ascii_whitespace();
    let (Some(from), Some(to)) = (words.next(), words.next()) else {
        let _ = writeln!(work, "{}", USAGE);
        return;
    };
    let force = match (words.next(), words.next()) {
        (None, _) => false,
        (Some("force"), None) => true,
        _ => {
            let _ = writeln!(work, "{}", USAGE);
            return;
        }
    };
    let result = if names_device(to) {
        onto_device(root, from, to, force, work)
    } else if force {
        Err(String::from("force is for a device: a file is never written over"))
    } else {
        convert(from, to, work)
    };
    if let Err(e) = result {
        let _ = writeln!(work, "hv: {}", e);
    }
}

/// Whether `word` names a device of nos's rather than a file
/// (`devices::source`).
#[cfg(target_arch = "x86_64")]
fn names_device(word: &str) -> bool {
    crate::devices::is_device(word)
}

#[cfg(not(target_arch = "x86_64"))]
fn names_device(word: &str) -> bool {
    word.starts_with("part:") || word.starts_with("disk:") || word.starts_with("dev:")
}

/// `hv convert <image> <device> [force]`: the image poured onto the device,
/// where it may be given (`devices::check`).
#[cfg(target_arch = "x86_64")]
fn onto_device(root: &str, from: &str, to: &str, force: bool, work: &mut dyn Work) -> Result<(), String> {
    let (disk, _) = crate::devices::given(to)?;
    crate::devices::check(root, "", &disk, force)?;
    pour(from, disk, "hv convert", work).map(|_| ())
}

#[cfg(not(target_arch = "x86_64"))]
fn onto_device(_root: &str, _from: &str, to: &str, _force: bool, _work: &mut dyn Work) -> Result<(), String> {
    Err(alloc::format!("{}: a device is given to a guest on x86-64 alone, so far", to))
}

/* ---- onto a device ---- */

#[cfg(target_arch = "x86_64")]
/// How many writes the pour has with the device at once, and how big each
/// is: 8 MiB of the kernel's buffers.
const POUR_BUFFERS: usize = 32;
#[cfg(target_arch = "x86_64")]
const POUR_BYTES: usize = 256 * 1024;
#[cfg(target_arch = "x86_64")]
/// How long a pour waits for a write before it looks again.
const POUR_WAIT_MS: u64 = 100;

/// A device the image's disk is poured onto: every byte of `[0, size)`
/// written, in order -- what the converter leaves out, zeros -- through
/// the device's queue, a buffer a write, `POUR_BUFFERS` of them out at
/// once; and nothing more once `stopped` is set.
#[cfg(target_arch = "x86_64")]
struct Onto<'a> {
    q: &'a DiskQueue,
    /// The disk's size, and how far it is written -- zeros filled -- so far.
    size: u64,
    at: u64,
    /// The buffer being filled -- its run starts at `run` -- and how much
    /// of it is.
    buf: Option<usize>,
    run: u64,
    fill: usize,
    /// Buffers with the device, and those that are free.
    out: usize,
    free: Vec<usize>,
    failed: bool,
    stopped: &'a Cell<bool>,
}

#[cfg(target_arch = "x86_64")]
impl<'a> Onto<'a> {
    fn new(q: &'a DiskQueue, size: u64, stopped: &'a Cell<bool>) -> Option<Onto<'a>> {
        let mut free = Vec::new();
        free.try_reserve_exact(q.buffers()).ok()?;
        free.extend((0..q.buffers()).rev());
        Some(Onto { q, size, at: 0, buf: None, run: 0, fill: 0, out: 0, free, failed: false, stopped })
    }

    /// What the device is done with, back on the free list: waiting for one
    /// when `wait` and none is free yet.
    fn reap(&mut self, wait: bool) {
        loop {
            let mut back = 0;
            let mut bad = false;
            self.q.reap(|k, ok| {
                back += 1;
                bad |= !ok;
                /* Into the room taken at `new`: every buffer of the queue. */
                if self.free.len() < self.free.capacity() {
                    self.free.push(k);
                }
            });
            self.out -= back.min(self.out);
            self.failed |= bad;
            if !wait || !self.free.is_empty() || self.out == 0 {
                return;
            }
            self.q.wait(Duration::from_millis(POUR_WAIT_MS));
        }
    }

    /// The buffer being filled started: its run written from its start.
    fn send(&mut self) -> kcore::error::Result<()> {
        let Some(k) = self.buf.take() else { return Ok(()) };
        if self.fill == 0 {
            self.free.push(k);
            return Ok(());
        }
        match self.q.start(k, true, self.run, self.fill) {
            Ok(Started::InFlight) => {
                self.out += 1;
                self.q.kick();
            }
            Ok(Started::Done) => self.free.push(k),
            Err(e) => {
                self.free.push(k);
                return Err(e);
            }
        }
        self.fill = 0;
        Ok(())
    }

    /// `data` after what is written so far -- `zeros` of it when None -- into
    /// buffers, each sent once it is full.
    fn put(&mut self, mut len: u64, data: Option<&[u8]>) -> kcore::error::Result<()> {
        let mut from = 0usize;
        while len > 0 {
            if self.stopped.get() || self.failed {
                return Err(kcore::error::Error::IoError);
            }
            let k = match self.buf {
                Some(k) => k,
                None => {
                    self.reap(true);
                    let k = self.free.pop().ok_or(kcore::error::Error::IoError)?;
                    (self.buf, self.run, self.fill) = (Some(k), self.at, 0);
                    k
                }
            };
            let n = (len as usize).min(POUR_BYTES - self.fill);
            {
                let mut lent = self.q.lend(k).ok_or(kcore::error::Error::Busy)?;
                let dst = &mut lent[self.fill..self.fill + n];
                match data {
                    Some(d) => dst.copy_from_slice(&d[from..from + n]),
                    None => dst.fill(0),
                }
            }
            self.fill += n;
            self.at += n as u64;
            from += n;
            len -= n as u64;
            if self.fill == POUR_BYTES {
                self.send()?;
            }
        }
        Ok(())
    }

    /// The rest of the disk zeros, everything sent and done, and the device
    /// flushed: Ok once all of it is on the device.
    fn finish(&mut self) -> kcore::error::Result<()> {
        self.put(self.size - self.at, None)?;
        self.send()?;
        while self.out > 0 {
            self.reap(true);
            if self.out > 0 {
                self.q.wait(Duration::from_millis(POUR_WAIT_MS));
            }
        }
        if self.failed {
            return Err(kcore::error::Error::IoError);
        }
        self.q.flush()
    }
}

#[cfg(target_arch = "x86_64")]
impl diskimage::Sink for Onto<'_> {
    type Error = kcore::error::Error;

    fn write_at(&mut self, offset: u64, data: &[u8]) -> kcore::error::Result<()> {
        /* In order, as the converter writes: what it left out between, zeros. */
        if offset < self.at || offset.checked_add(data.len() as u64).is_none_or(|end| end > self.size) {
            return Err(kcore::error::Error::InvalidValue);
        }
        self.put(offset - self.at, None)?;
        self.put(data.len() as u64, Some(data))
    }
}

#[cfg(target_arch = "x86_64")]
/// What a pour did: the disk's size, and what of the device was left after
/// it -- given back to the device where it takes that, as the pour said.
pub struct Poured {
    pub size: u64,
    pub rest: u64,
}

#[cfg(target_arch = "x86_64")]
/// `from` -- a qcow2 image, or a raw disk -- poured onto `disk`, a device
/// of nos's checked already, claimed for `holder` while it is written: Ok
/// once every byte of the image's disk is on the device -- the rest of it
/// given back where the device takes that -- or why not, the device then
/// holding part of it, which is the caller's to say.
pub fn pour(from: &str, disk: kcore::block::Disk, holder: &str, work: &mut dyn Work) -> Result<Poured, String> {
    let file = File::open(from, false).map_err(|e| alloc::format!("{}: {}", from, e))?;
    let len = file.size().map_err(|e| alloc::format!("{}: {}", from, e))?;
    let mut image = Image { file, len };
    let mut head = Vec::new();
    let take = len.min(diskimage::detect::HEAD as u64) as usize;
    head.try_reserve_exact(take).map_err(|_| String::from("out of memory"))?;
    head.resize(take, 0);
    diskimage::Source::read_at(&mut image, 0, &mut head).map_err(|e| alloc::format!("{}: {}", from, e))?;
    let format = detect(&head);
    drop(head);
    let img = match format {
        Format::Qcow2 => Some(Qcow2::open(&mut image).map_err(|e| said(e, from, disk.name().as_str()))?),
        Format::Raw | Format::Unknown => None,
        format => return Err(refused(from, format)),
    };
    let size = img.as_ref().map_or(len, |i| i.size());
    let name = disk.name();
    let ss = disk.sector_size();
    let device = disk.sectors().saturating_mul(ss);
    if ss == 0 || size % ss != 0 {
        return Err(alloc::format!("{}'s disk is {} bytes, not whole sectors of {}", from, size, name));
    }
    if size > device {
        return Err(alloc::format!("{}'s disk is {} MiB, and {} is {} MiB", from, size.div_ceil(MIB), name,
                                  device / MIB));
    }
    let gives_back = disk.can_discard();
    let q = disk.into_queue(true, holder, POUR_BUFFERS, POUR_BYTES)
        .map_err(|e| alloc::format!("{}: {}", name, e))?;
    let _ = writeln!(work, "hv: {}: {} a disk of {} MiB -- onto {}, {} MiB", from,
                     if img.is_some() { "qcow2," } else { "raw," }, size.div_ceil(MIB), name, device / MIB);

    let started = kcore::time::boot_time_ns();
    let stopped = Cell::new(false);
    let mut onto = Onto::new(&q, size, &stopped).ok_or_else(|| String::from("out of memory"))?;
    let mut reported = started;
    let mut progress = |at: u64| {
        if work.cancelled() {
            stopped.set(true);
            return;
        }
        work.progress(at, size);
        let now = kcore::time::boot_time_ns();
        let every = (now.saturating_sub(started) / REPORT_SHARE).clamp(REPORT_MIN_NS, REPORT_MAX_NS);
        if now.saturating_sub(reported) >= every && at < size {
            reported = now;
            let _ = writeln!(work, "hv: {} of {} MiB of the disk, {} s", at / MIB, size.div_ceil(MIB),
                             now.saturating_sub(started) / NS_PER_SEC);
        }
    };
    let stopped_or = |e: String| if stopped.get() { String::from(work::STOPPED) } else { e };
    match &img {
        Some(img) => {
            img.convert(&mut image, &mut onto, &mut progress).map_err(|e| stopped_or(said(e, from, name.as_str())))?;
        }
        None => {
            /* Raw: read as it is, a chunk at a time. */
            let mut chunk = Vec::new();
            chunk.try_reserve_exact(POUR_BYTES).map_err(|_| String::from("out of memory"))?;
            chunk.resize(POUR_BYTES, 0);
            let mut at = 0u64;
            while at < size {
                let n = (size - at).min(POUR_BYTES as u64) as usize;
                diskimage::Source::read_at(&mut image, at, &mut chunk[..n])
                    .map_err(|e| alloc::format!("{}: reading it failed -- {}", from, e))?;
                diskimage::Sink::write_at(&mut onto, at, &chunk[..n])
                    .map_err(|e| stopped_or(alloc::format!("{}: writing it failed -- {}", name, e)))?;
                at += n as u64;
                progress(at);
            }
        }
    }
    onto.finish().map_err(|e| stopped_or(alloc::format!("{}: writing it failed -- {}", name, e)))?;
    /* What is past the disk, given back: the device's to say what it reads as. */
    let rest = device - size;
    let given_back = rest != 0 && gives_back && q.discard(size, rest).is_ok();
    let ns = kcore::time::boot_time_ns().saturating_sub(started);
    let _ = writeln!(work, "hv: {}: {} MiB written in {}.{} s{}", name, size.div_ceil(MIB), ns / NS_PER_SEC,
                     ns % NS_PER_SEC / NS_PER_TENTH,
                     if rest == 0 { "" } else if given_back { ", the rest of the device given back to it" }
                     else { ", the rest of the device left as it was" });
    Ok(Poured { size, rest })
}

/// `from`, an image, converted into `to`, a new raw disk: Ok once the disk
/// is written and synced, or why not -- and `to` then removed. Its lines,
/// its progress and whether to stop are `work`'s.
pub fn convert(from: &str, to: &str, work: &mut dyn Work) -> Result<(), String> {
    let file = File::open(from, false).map_err(|e| alloc::format!("{}: {}", from, e))?;
    let len = file.size().map_err(|e| alloc::format!("{}: {}", from, e))?;
    let mut image = Image { file, len };

    let mut head = Vec::new();
    let take = len.min(diskimage::detect::HEAD as u64) as usize;
    if head.try_reserve_exact(take).is_err() {
        return Err(String::from("out of memory"));
    }
    head.resize(take, 0);
    diskimage::Source::read_at(&mut image, 0, &mut head).map_err(|e| alloc::format!("{}: {}", from, e))?;
    let format = detect(&head);
    drop(head);
    if format != Format::Qcow2 {
        return Err(refused(from, format));
    }
    let img = Qcow2::open(&mut image).map_err(|e| said(e, from, to))?;

    if kcore::fs::size(to).is_ok() {
        return Err(alloc::format!("{} is there already -- not written over", to));
    }
    /* Made here, and so this conversion's to remove if it fails. */
    kcore::fs::create(to, &[]).map_err(|e| alloc::format!("{} could not be made -- {}", to, e))?;
    let disk = match File::open(to, true) {
        Ok(disk) => disk,
        Err(e) => {
            let _ = kcore::fs::remove(to);
            return Err(alloc::format!("{} could not be opened -- {}", to, e));
        }
    };
    let _ = writeln!(work, "hv: {}: qcow2 version {}, a disk of {} MiB in clusters of {} KiB{} -- into {}", from,
                     img.version(), img.size().div_ceil(MIB), img.cluster_size() / KIB,
                     if img.extended_l2() { " with subclusters" } else { "" }, to);
    if img.dirty() {
        let _ = writeln!(work, "hv: {}: QEMU did not close it cleanly -- its data is read as its tables have it", from);
    }

    let started = kcore::time::boot_time_ns();
    let stopped = Cell::new(false);
    let result = disk
        .set_len(img.size())
        .map_err(|e| alloc::format!("{}: {} bytes do not fit there -- {}", to, img.size(), e))
        .and_then(|()| {
            let size = img.size();
            let mut reported = started;
            let mut progress = |at: u64| {
                /* Asked after every cluster: the next write the disk is
                 * handed is refused once the work is to stop. */
                if work.cancelled() {
                    stopped.set(true);
                    return;
                }
                work.progress(at, size);
                let now = kcore::time::boot_time_ns();
                let every = (now.saturating_sub(started) / REPORT_SHARE).clamp(REPORT_MIN_NS, REPORT_MAX_NS);
                if now.saturating_sub(reported) >= every && at < size {
                    reported = now;
                    let _ = writeln!(work, "hv: {} of {} MiB of the disk, {} s", at / MIB, size.div_ceil(MIB),
                                     now.saturating_sub(started) / NS_PER_SEC);
                }
            };
            let mut sink = Disk { file: &disk, stopped: &stopped };
            img.convert(&mut image, &mut sink, &mut progress).map_err(|e| match stopped.get() {
                true => String::from(work::STOPPED),
                false => said(e, from, to),
            })
        })
        .and_then(|stats| {
            disk.sync().map_err(|e| alloc::format!("{}: syncing it failed -- {}", to, e))?;
            Ok(stats)
        });
    /* Closed before it may be removed. */
    drop(disk);
    match result {
        Ok(stats) => {
            report(&img, &stats, to, kcore::time::boot_time_ns().saturating_sub(started), work);
            Ok(())
        }
        Err(e) => {
            if kcore::fs::remove(to).is_err() {
                let _ = writeln!(work, "hv: {} could not be removed -- what is in it is not the disk", to);
            }
            Err(e)
        }
    }
}

/// What a failure is, said with the file -- or the device -- it is of.
fn said(e: Error<kcore::error::Error>, from: &str, to: &str) -> String {
    match e {
        Error::Image(bad) => alloc::format!("{}: {}", from, bad),
        Error::Read(e) => alloc::format!("{}: reading it failed -- {}", from, e),
        Error::Write(e) => alloc::format!("{}: writing it failed -- {} (is the filesystem full?)", to, e),
        Error::NoMemory => String::from("out of memory"),
    }
}

/// Why a file that is not qcow2 is not converted, and what to do with it.
pub(crate) fn refused(from: &str, format: Format) -> String {
    match format {
        Format::Raw => alloc::format!("{} is {} already -- a guest is given it as it is: disk={}", from,
                                      format.name(), from),
        Format::Iso9660 => alloc::format!("{} is {} -- a guest reads it as it is: disk={}:ro", from, format.name(), from),
        Format::Gzip | Format::Xz | Format::Zstd | Format::Bzip2 => {
            alloc::format!("{} is {} -- unpack it first: what is in it may be a disk", from, format.name())
        }
        Format::Unknown => alloc::format!("{} is no image this knows -- a raw disk with no partition table is given \
                                           to a guest as it is", from),
        _ => alloc::format!("{} is {}, which this does not convert -- only qcow2: qemu-img convert -O raw it \
                             elsewhere", from, format.name()),
    }
}

/// What was converted, in a line.
fn report(img: &Qcow2, stats: &Stats, to: &str, ns: u64, work: &mut dyn Work) {
    let _ = writeln!(work, "hv: {}: {} MiB written of {} MiB, the rest holes -- {} clusters inflated, {} copied, {} \
                           zeros, {} not there -- in {}.{} s",
                     to, stats.written.div_ceil(MIB), img.size().div_ceil(MIB), stats.compressed, stats.stored,
                     stats.zeros, stats.holes, ns / NS_PER_SEC, ns % NS_PER_SEC / NS_PER_TENTH);
}
