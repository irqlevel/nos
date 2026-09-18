//! The partition table: what a disk's first sectors say about how it is cut
//! up, and a block device registered for each piece found.
//!
//! Both layouts a disk here can carry are read: an MBR, and a GPT when the
//! MBR is the protective one a GPT disk puts there. Nothing is written --
//! the kernel never edits a partition table -- so this is a reader of
//! untrusted bytes and nothing else, which is why it is the first of the
//! block layer to be in Rust.
//!
//! Called from the boot path (`kernel/main.cpp`, `arch/arm64/main_arm64.cpp`)
//! once the virtio disks are up, and again once the Rust drivers' disks are.

#![no_std]

extern crate alloc;

mod disklog;
mod part;
mod selftest;
mod shell;
mod table;

use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use kcore::block::Disk;
use kcore::cmd::{Command, Output};
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::trace;

/// Disks looked at, and partitions registered, at most. The kernel's device
/// table holds 48 in all (BlockDeviceTable::MaxDevices), which these fit
/// inside with room for the disks themselves.
const MAX_DISKS: usize = 8;
/// GPT routinely declares 128 slots. Eight is what a disk here actually
/// uses, and the cap is what bounds the work a bad table can ask for.
const PARTS_PER_DISK: u32 = 8;
const MAX_PARTITIONS: u32 = MAX_DISKS as u32 * PARTS_PER_DISK;

/* The MBR: a boot program, four entries, and a signature at the end of the
 * first sector. */
const MBR_SIZE: usize = 512;
const MBR_SIGNATURE_OFFSET: usize = 510;
const MBR_SIGNATURE: u16 = 0xAA55;
const MBR_PART_OFFSET: usize = 446;
const MBR_PART_SIZE: usize = 16;
const MBR_PARTS: usize = 4;
/* A GPT disk carries a protective MBR: one entry of this type covering the
 * whole disk, so a tool that only understands MBR sees an occupied disk
 * rather than an empty one. Finding it is what says to look at LBA 1. */
const MBR_TYPE_GPT_PROTECTIVE: u8 = 0xEE;

/* The GPT header, at LBA 1. The backup at the end of the disk is not
 * consulted: a disk whose primary header is damaged is not one to start
 * writing a log into on a guess. */
/// "EFI PART", read back as a little-endian u64
const GPT_SIGNATURE: u64 = 0x5452_4150_2049_4645;
const GPT_HEADER_MIN: usize = 92;
const GPT_ENTRY_MIN: usize = 128;
const GPT_GUID_LEN: usize = 16;
/// The most slots `partitions` will walk. The probe stops at PARTS_PER_DISK;
/// the viewer shows the whole of a normal table (128 slots is what tools
/// write) but not what a corrupt header asks for -- the count comes off the
/// disk, and u32::MAX of it would be a shell command that never returns.
const GPT_MAX_SHOWN: u32 = 128;

static PROBED: [AtomicUsize; MAX_DISKS] = {
    const ZERO: AtomicUsize = AtomicUsize::new(0);
    [ZERO; MAX_DISKS]
};
static PROBED_COUNT: AtomicU32 = AtomicU32::new(0);
static PART_COUNT: AtomicU32 = AtomicU32::new(0);

/// Look at every disk the kernel has that has not been looked at yet, and
/// register a block device for each partition found on it.
///
/// Safe to call more than once, and meant to be: the disks the Rust drivers
/// bring up are registered long after the virtio ones, and a block read
/// before the soft IRQ layer exists returns without having read anything --
/// so the boot calls this again once both are true.
#[no_mangle]
pub extern "C" fn rust_partitions_probe() {
    /* The count is read once: probing registers partitions, and probing
     * those would be probing our own output. */
    let devices = kcore::block::count();

    for index in 0..devices {
        let disk = match kcore::block::at(index) {
            Some(disk) => disk,
            None => continue,
        };

        /* A partition of a disk, ours or anyone's: not a disk to look at. */
        if disk.parent().is_some() {
            continue;
        }

        match remember(disk) {
            Seen::Already => continue,
            Seen::NoRoom => break,
            Seen::New => probe_disk(disk),
        }
    }

    trace!(0, "part: {} partitions on {} disks", PART_COUNT.load(Ordering::Relaxed),
        PROBED_COUNT.load(Ordering::Relaxed));
}

/// Set the layer up and put its commands in front of whoever runs one.
/// Called from `rust_init`, before anything can claim a device.
pub fn init() {
    if !table::claims_setup() {
        trace!(0, "block: no memory for the claim table -- writes will be refused");
    }

    register("disks", "disks - list block devices", table::dump);
    register("partitions", "partitions <disk> - show the partition table (MBR or GPT)", dump);
    register("diskread", "diskread <disk> <sector> - read sector", shell::diskread);
    register("diskwrite", "diskwrite <disk> <sector> <hex> - write sector", shell::diskwrite);
    register("disklog", "disklog - kernel log to disk area state", disklog::dump);
}

fn register(name: &'static str, help: &'static str, handler: fn(&str, &mut Output)) {
    match Command::register(name, help, move |args, out| handler(args, out)) {
        /* The command is the kernel's own and stays for good. */
        Ok(cmd) => core::mem::forget(cmd),
        Err(_) => trace!(0, "block: cannot register the {} command", name),
    }
}

enum Seen {
    /// A disk to look at, now noted as looked at
    New,
    /// Looked at by an earlier pass
    Already,
    /// No room left to remember another disk by
    NoRoom,
}

/// Take note of a disk about to be probed.
fn remember(disk: Disk) -> Seen {
    let probed = PROBED_COUNT.load(Ordering::Relaxed) as usize;
    for slot in PROBED.iter().take(probed) {
        if slot.load(Ordering::Relaxed) == disk.handle() {
            return Seen::Already;
        }
    }

    if probed >= MAX_DISKS {
        trace!(0, "part: {} disks looked at already, the rest are not", MAX_DISKS);
        return Seen::NoRoom;
    }

    PROBED[probed].store(disk.handle(), Ordering::Relaxed);
    PROBED_COUNT.store(probed as u32 + 1, Ordering::Relaxed);
    Seen::New
}

/// A sector into buf, which has to be exactly one sector long.
fn read_sector(disk: &Disk, lba: u64, buf: &mut [u8]) -> bool {
    disk.read(lba, buf).is_ok()
}

fn probe_disk(disk: Disk) {
    let sector_size = disk.sector_size() as usize;
    if sector_size < MBR_SIZE || sector_size > PAGE_SIZE {
        return;
    }

    /* The driver DMAs into this, so it is a DmaBuffer and not a Vec. */
    let mut buf = match DmaBuffer::new(1) {
        Some(buf) => buf,
        None => return,
    };

    if !read_sector(&disk, 0, &mut buf.as_mut_slice()[..sector_size]) {
        return;
    }

    let sector = &buf.as_slice()[..sector_size];
    if le16(sector, MBR_SIGNATURE_OFFSET) != MBR_SIGNATURE {
        return;
    }

    /* A GPT disk puts a protective MBR here, claiming the whole disk under
     * one entry of a reserved type. Taking that at face value would register
     * a "partition" spanning the disk and hide every real one. */
    for i in 0..MBR_PARTS {
        if mbr_entry(sector, i).part_type == MBR_TYPE_GPT_PROTECTIVE {
            probe_gpt(disk, sector_size);
            return;
        }
    }

    for i in 0..MBR_PARTS {
        let entry = mbr_entry(sector, i);
        if entry.part_type == 0 || entry.size == 0 {
            continue;
        }

        if !add_partition(disk, entry.start as u64, entry.size as u64, i as u32) {
            break;
        }
    }
}

fn probe_gpt(disk: Disk, sector_size: usize) {
    let mut buf = match DmaBuffer::new(1) {
        Some(buf) => buf,
        None => return,
    };

    if !read_sector(&disk, 1, &mut buf.as_mut_slice()[..sector_size]) {
        return;
    }

    let header = match gpt_header(&buf.as_slice()[..sector_size], sector_size) {
        Some(header) => header,
        None => return,
    };

    if !header.crc_ok {
        trace!(0, "part: GPT header checksum mismatch, disk left alone");
        return;
    }

    let per_sector = sector_size / header.entry_size;
    if per_sector == 0 {
        return;
    }

    let entries = core::cmp::min(header.entries, PARTS_PER_DISK);

    let mut ebuf = match DmaBuffer::new(1) {
        Some(buf) => buf,
        None => return,
    };

    for i in 0..entries as usize {
        /* One sector at a time rather than the whole array: the array is
         * 16 KiB at the usual 128 slots, and only the first few are ever
         * used on a disk anyone here would prepare. */
        if i % per_sector == 0 {
            let lba = header.entry_lba + (i / per_sector) as u64;
            if !read_sector(&disk, lba, &mut ebuf.as_mut_slice()[..sector_size]) {
                break;
            }
        }

        let at = (i % per_sector) * header.entry_size;
        let entry = &ebuf.as_slice()[at..at + GPT_ENTRY_MIN];

        /* An all-zero type GUID marks an unused slot. */
        if entry[..GPT_GUID_LEN].iter().all(|b| *b == 0) {
            continue;
        }

        let first = le64(entry, 32);
        let last = le64(entry, 40); /* inclusive */
        if last < first {
            continue;
        }

        if !add_partition(disk, first, last - first + 1, i as u32) {
            break;
        }
    }
}

/// Register one partition. False says to stop looking at this disk: there is
/// no room for another anywhere.
fn add_partition(disk: Disk, start: u64, count: u64, index: u32) -> bool {
    if start == 0 {
        /* Would alias the partition table itself. */
        trace!(0, "part: partition {} starts at LBA 0, skipped", index + 1);
        return true;
    }

    /* Checked, because these numbers come off the disk: a table claiming a
     * partition at the top of the address space must not wrap into looking
     * like it fits. */
    match start.checked_add(count) {
        Some(end) if end <= disk.sectors() => {}
        _ => {
            trace!(0, "part: partition {} is not inside the disk, skipped", index + 1);
            return true;
        }
    }

    if PART_COUNT.load(Ordering::Relaxed) >= MAX_PARTITIONS {
        trace!(0, "part: {} partitions already, the rest are not registered", MAX_PARTITIONS);
        return false;
    }

    let mut disk_name = [0u8; part::NAME_MAX];
    let disk_name = match disk.name(&mut disk_name) {
        Some(name) => name,
        None => return true,
    };

    let mut name = [0u8; part::NAME_MAX];
    let len = match partition_name(disk_name.as_bytes(), index + 1, &mut name) {
        Some(len) => len,
        None => return true,
    };

    if !part::register(disk, start, count, &name[..len]) {
        return true;
    }

    PART_COUNT.fetch_add(1, Ordering::Relaxed);
    trace!(0, "part: {} start {} size {}",
        core::str::from_utf8(&name[..len]).unwrap_or("?"), start, count);
    true
}

/// The disk's name with the partition's number after it -- `nvme0` and 1
/// make `nvme01` -- into out, whose length it returns. Two digits, because
/// GPT gives more slots than a single one covers.
fn partition_name(disk: &[u8], number: u32, out: &mut [u8; part::NAME_MAX]) -> Option<usize> {
    let digits = if number >= 10 { 2 } else { 1 };
    let len = disk.len() + digits;
    /* The name the kernel keeps is NUL-terminated, so the last byte is not
     * ours to use. */
    if number >= 100 || len >= part::NAME_MAX {
        return None;
    }

    out[..disk.len()].copy_from_slice(disk);
    if digits == 2 {
        out[disk.len()] = b'0' + (number / 10) as u8;
        out[disk.len() + 1] = b'0' + (number % 10) as u8;
    } else {
        out[disk.len()] = b'0' + number as u8;
    }
    Some(len)
}

struct MbrEntry {
    part_type: u8,
    start: u32,
    size: u32,
}

fn mbr_entry(sector: &[u8], index: usize) -> MbrEntry {
    let at = MBR_PART_OFFSET + index * MBR_PART_SIZE;
    MbrEntry {
        part_type: sector[at + 4],
        start: le32(sector, at + 8),
        size: le32(sector, at + 12),
    }
}

struct GptHeader {
    entry_lba: u64,
    entries: u32,
    entry_size: usize,
    crc_ok: bool,
}

/// What LBA 1 says, if it says anything this kernel can act on.
fn gpt_header(sector: &[u8], sector_size: usize) -> Option<GptHeader> {
    if le64(sector, 0) != GPT_SIGNATURE {
        return None;
    }

    let header_size = le32(sector, 12) as usize;
    if header_size < GPT_HEADER_MIN || header_size > sector_size {
        return None;
    }

    let entry_size = le32(sector, 84) as usize;
    if entry_size < GPT_ENTRY_MIN || entry_size > sector_size {
        return None;
    }

    /* The checksum is taken over the header with its own field zeroed, so it
     * is computed in three pieces rather than by editing the sector. */
    let stored = le32(sector, 16);
    let mut crc = kcore::crc32::crc32_update(0, &sector[..16]);
    crc = kcore::crc32::crc32_update(crc, &[0u8; 4]);
    crc = kcore::crc32::crc32_update(crc, &sector[20..header_size]);

    Some(GptHeader {
        entry_lba: le64(sector, 72),
        entries: le32(sector, 80),
        entry_size,
        crc_ok: crc == stored,
    })
}

fn le16(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}

fn le32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

fn le64(buf: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/* ---- the `partitions` command ---- */

fn dump(args: &str, out: &mut Output) {
    let name = args.split_whitespace().next().unwrap_or("");
    if name.is_empty() {
        let _ = writeln!(out, "usage: partitions <disk>");
        return;
    }

    let disk = match Disk::open(name) {
        Some(disk) => disk,
        None => {
            let _ = writeln!(out, "disk '{}' not found", name);
            return;
        }
    };

    /* A read transfers whole hardware sectors, which may be more than 512
     * (a 4K-LBA NVMe), so the buffer is sized from the device. */
    let sector_size = disk.sector_size() as usize;
    if sector_size < MBR_SIZE || sector_size > PAGE_SIZE {
        let _ = writeln!(out, "sector size {} is not one this can read", sector_size);
        return;
    }

    let mut buf = match DmaBuffer::new(1) {
        Some(buf) => buf,
        None => {
            let _ = writeln!(out, "no memory for a sector");
            return;
        }
    };

    if !read_sector(&disk, 0, &mut buf.as_mut_slice()[..sector_size]) {
        let _ = writeln!(out, "cannot read sector 0");
        return;
    }

    let sector = &buf.as_slice()[..sector_size];
    if le16(sector, MBR_SIGNATURE_OFFSET) != MBR_SIGNATURE {
        let _ = writeln!(out, "no partition table (MBR signature 0x{:04X})",
            le16(sector, MBR_SIGNATURE_OFFSET));
        return;
    }

    for i in 0..MBR_PARTS {
        if mbr_entry(sector, i).part_type == MBR_TYPE_GPT_PROTECTIVE {
            dump_gpt(&disk, sector_size, out);
            return;
        }
    }

    let _ = writeln!(out, "mbr");
    let _ = writeln!(out, "  #  type  lba start    sectors      size");
    for i in 0..MBR_PARTS {
        let entry = mbr_entry(sector, i);
        if entry.part_type == 0 && entry.size == 0 {
            continue;
        }

        let _ = writeln!(out, "  {}  0x{:02X}  {:<12} {:<12} {} MB",
            i + 1, entry.part_type, entry.start, entry.size,
            (entry.size as u64 * sector_size as u64) / (1024 * 1024));
    }
}

fn dump_gpt(disk: &Disk, sector_size: usize, out: &mut Output) {
    let mut buf = match DmaBuffer::new(1) {
        Some(buf) => buf,
        None => return,
    };

    if !read_sector(disk, 1, &mut buf.as_mut_slice()[..sector_size]) {
        let _ = writeln!(out, "cannot read the GPT header at LBA 1");
        return;
    }

    let header = match gpt_header(&buf.as_slice()[..sector_size], sector_size) {
        Some(header) => header,
        None => {
            let _ = writeln!(out, "protective MBR, but no GPT header this can read");
            return;
        }
    };

    let shown = core::cmp::min(header.entries, GPT_MAX_SHOWN);
    let _ = writeln!(out, "gpt, {} entries{}{}", header.entries,
        if header.crc_ok { "" } else { " (HEADER CHECKSUM MISMATCH)" },
        if shown < header.entries { " (showing the first 128)" } else { "" });
    let _ = writeln!(out, "  #  lba start    sectors      size      type guid");

    let per_sector = sector_size / header.entry_size;
    if per_sector == 0 {
        return;
    }

    let mut ebuf = match DmaBuffer::new(1) {
        Some(buf) => buf,
        None => return,
    };

    for i in 0..shown as usize {
        if i % per_sector == 0 {
            let lba = header.entry_lba + (i / per_sector) as u64;
            if !read_sector(disk, lba, &mut ebuf.as_mut_slice()[..sector_size]) {
                break;
            }
        }

        let at = (i % per_sector) * header.entry_size;
        let entry = &ebuf.as_slice()[at..at + GPT_ENTRY_MIN];
        if entry[..GPT_GUID_LEN].iter().all(|b| *b == 0) {
            continue;
        }

        let first = le64(entry, 32);
        let last = le64(entry, 40);
        if last < first {
            continue;
        }

        let sectors = last - first + 1;
        let _ = writeln!(out, "  {}  {:<12} {:<12} {:>5} MB  {}",
            i + 1, first, sectors,
            (sectors * sector_size as u64) / (1024 * 1024),
            Guid(&entry[..GPT_GUID_LEN]));
    }
}

/// A GPT GUID as tools print it: the first three fields little-endian, the
/// rest in the order they are stored.
struct Guid<'a>(&'a [u8]);

impl core::fmt::Display for Guid<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = self.0;
        write!(f, "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-",
            b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9])?;
        for byte in &b[10..16] {
            write!(f, "{:02X}", byte)?;
        }
        Ok(())
    }
}
