//! `hv convert <image> <disk>`: a cloud's disk image -- QEMU's qcow2, which
//! Ubuntu, Debian and Fedora ship theirs in -- made the raw disk a guest is
//! given (`disk=`), by the `diskimage` crate. The disk is a new file, grown
//! to the image's size with a hole, and only what is not zeros is written
//! into it: Ubuntu's 3.5 GiB disk costs the 1.7 GiB it holds.
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
use kcore::consts::NS_PER_SEC;
use kcore::fs::File;

use crate::work::{self, Work};

const USAGE: &str = "hv convert <image> <disk> -- a qcow2 image made a raw disk, its holes kept";
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

/// `hv convert <image> <disk>`.
pub fn command(args: &str, work: &mut dyn Work) {
    let mut words = args.split_ascii_whitespace();
    let (Some(from), Some(to), None) = (words.next(), words.next(), words.next()) else {
        let _ = writeln!(work, "{}", USAGE);
        return;
    };
    if let Err(e) = convert(from, to, work) {
        let _ = writeln!(work, "hv: {}", e);
    }
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

/// What a failure is, said with the file it is of.
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
