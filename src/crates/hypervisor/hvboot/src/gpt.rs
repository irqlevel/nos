//! A disk's partitions, from its GPT -- header and array each checked by
//! their CRC, every LBA by the disk's size -- or else from a plain MBR's
//! four primary entries. A disk with neither is no partitions at all: the
//! caller looks at it whole, as a filesystem of its own may be.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crc::crc32::checksum as crc32;

pub const SECTOR: usize = 512;

const MBR_SIGNATURE: u16 = 0xAA55;
const MBR_ENTRIES: usize = 0x1BE;
const MBR_ENTRY_BYTES: usize = 16;
const MBR_PROTECTIVE: u8 = 0xEE;
const MBR_EXTENDED: [u8; 3] = [0x05, 0x0F, 0x85];

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_HEADER_MIN: usize = 92;
/// The most partition entries read, and the most bytes of them: a table
/// that claims more is not one.
const GPT_MAX_ENTRIES: u32 = 1024;
const GPT_MAX_ARRAY: usize = 1 << 20;
const GPT_ENTRY_MIN: u32 = 128;

/* GPT partition types this tells apart, as their GUIDs are laid out on the
 * disk (the first three fields little-endian). */
/// The Boot Loader Specification's extended boot partition, a /boot of its
/// own: where Ubuntu's kernel and GRUB's configuration are.
pub const TYPE_XBOOTLDR: [u8; 16] =
    [0xFF, 0xC2, 0x13, 0xBC, 0xE6, 0x59, 0x62, 0x42, 0xA3, 0x52, 0xB2, 0x75, 0xFD, 0x6F, 0x71, 0x72];
/// The EFI system partition, FAT: nothing this reads.
pub const TYPE_ESP: [u8; 16] =
    [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B];
/// The BIOS boot partition, GRUB's core image: no filesystem.
pub const TYPE_BIOS_BOOT: [u8; 16] =
    [0x48, 0x61, 0x68, 0x21, 0x49, 0x64, 0x6F, 0x6E, 0x74, 0x4E, 0x65, 0x65, 0x64, 0x45, 0x46, 0x49];

/// A partition: its number as the kernel and GRUB count them (1 up), where
/// it is, and its type.
#[derive(Clone, Debug)]
pub struct Partition {
    pub number: u32,
    pub first: u64,
    pub sectors: u64,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Gpt([u8; 16]),
    Mbr(u8),
}

impl Partition {
    /// Whether it can hold no filesystem this reads: the ESP, GRUB's BIOS
    /// boot partition.
    pub fn never_ext(&self) -> bool {
        matches!(self.kind, Kind::Gpt(t) if t == TYPE_ESP || t == TYPE_BIOS_BOOT)
    }
}

/// A disk's table as read: the partitions in it, and those passed over --
/// each by its number, and why: one that runs past the disk's end (an image
/// put on a disk smaller than it), or ends before it begins.
#[derive(Debug, Default)]
pub struct Table {
    pub parts: Vec<Partition>,
    pub passed_over: Vec<(u32, String)>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from(u32_at(b, o)) | (u64::from(u32_at(b, o + 4)) << 32)
}

/// Why a partition from `first` to `last`, inclusive, is passed over on a
/// disk of `sectors`.
fn past_end(first: u64, last: u64, sectors: u64) -> String {
    alloc::format!("sectors {} to {}, past the disk's end at {}", first, last, sectors)
}

/// The partitions of a disk of `sectors` sectors, `read` reading whole
/// sectors from a sector on. No partitions for a disk with no table.
pub fn partitions(sectors: u64, read: &mut dyn FnMut(u64, &mut [u8]) -> Result<(), String>)
    -> Result<Table, String>
{
    if sectors < 2 {
        return Ok(Table::default());
    }
    let mut mbr = [0u8; SECTOR];
    read(0, &mut mbr)?;
    if u16_at(&mbr, 510) != MBR_SIGNATURE {
        return Ok(Table::default());
    }
    let entries: [(u8, u64, u64); 4] = core::array::from_fn(|i| {
        let e = &mbr[MBR_ENTRIES + i * MBR_ENTRY_BYTES..MBR_ENTRIES + (i + 1) * MBR_ENTRY_BYTES];
        (e[4], u64::from(u32_at(e, 8)), u64::from(u32_at(e, 12)))
    });
    if entries.iter().any(|e| e.0 == MBR_PROTECTIVE) {
        return gpt(sectors, read);
    }
    let mut table = Table::default();
    for (i, &(kind, first, count)) in entries.iter().enumerate() {
        let number = i as u32 + 1;
        if kind == 0 || count == 0 || MBR_EXTENDED.contains(&kind) {
            continue;
        }
        if first.checked_add(count).is_none_or(|end| end > sectors) {
            /* Both from 32-bit fields: no overflow. */
            table.passed_over.push((number, past_end(first, first + count - 1, sectors)));
            continue;
        }
        table.parts.push(Partition { number, first, sectors: count, kind: Kind::Mbr(kind) });
    }
    Ok(table)
}

fn gpt(sectors: u64, read: &mut dyn FnMut(u64, &mut [u8]) -> Result<(), String>) -> Result<Table, String> {
    let mut h = [0u8; SECTOR];
    read(1, &mut h)?;
    if &h[0..8] != GPT_SIGNATURE {
        return Err(String::from("a protective MBR, but no GPT header at LBA 1"));
    }
    let size = u32_at(&h, 12) as usize;
    if !(GPT_HEADER_MIN..=SECTOR).contains(&size) {
        return Err(alloc::format!("a GPT header of {} bytes", size));
    }
    let want = u32_at(&h, 16);
    let mut copy = [0u8; SECTOR];
    copy[..size].copy_from_slice(&h[..size]);
    copy[16..20].fill(0);
    if crc32(&copy[..size]) != want {
        return Err(String::from("the GPT header's CRC does not match"));
    }
    if u64_at(&h, 24) != 1 {
        return Err(String::from("a GPT header that is not where it says it is"));
    }
    let array_lba = u64_at(&h, 72);
    let count = u32_at(&h, 80);
    let entry_size = u32_at(&h, 84);
    let array_crc = u32_at(&h, 88);
    if count > GPT_MAX_ENTRIES || entry_size < GPT_ENTRY_MIN || entry_size % 8 != 0 {
        return Err(alloc::format!("a GPT of {} entries of {} bytes", count, entry_size));
    }
    let bytes = count as usize * entry_size as usize;
    if bytes > GPT_MAX_ARRAY {
        return Err(alloc::format!("a GPT array of {} bytes", bytes));
    }
    let array_sectors = bytes.div_ceil(SECTOR) as u64;
    if array_lba < 2 || array_lba.checked_add(array_sectors).is_none_or(|end| end > sectors) {
        return Err(alloc::format!("a GPT array at LBA {}, past the disk", array_lba));
    }
    let mut array = vec![0u8; array_sectors as usize * SECTOR];
    read(array_lba, &mut array)?;
    if crc32(&array[..bytes]) != array_crc {
        return Err(String::from("the GPT array's CRC does not match"));
    }

    let mut table = Table::default();
    for i in 0..count as usize {
        let e = &array[i * entry_size as usize..i * entry_size as usize + GPT_ENTRY_MIN as usize];
        let mut kind = [0u8; 16];
        kind.copy_from_slice(&e[0..16]);
        if kind == [0; 16] {
            continue;
        }
        let number = i as u32 + 1;
        let (first, last) = (u64_at(e, 32), u64_at(e, 40));
        if first > last {
            table.passed_over.push((number, alloc::format!("its last sector, {}, before its first, {}", last, first)));
            continue;
        }
        if last >= sectors {
            table.passed_over.push((number, past_end(first, last, sectors)));
            continue;
        }
        table.parts.push(Partition { number, first, sectors: last - first + 1, kind: Kind::Gpt(kind) });
    }
    Ok(table)
}
