//! Partition tables: an MBR and a GPT made from fields the input chooses,
//! and what the kernel should register from a disk's first sectors, read
//! the way `block`'s probe documents it -- and no other way.

use kcore::crc32::crc32_update;

use crate::machine::disk::Media;

pub const MBR_SIGNATURE: u16 = 0xAA55;
pub const TYPE_GPT_PROTECTIVE: u8 = 0xEE;
pub const GPT_SIGNATURE: u64 = 0x5452_4150_2049_4645;
/// The slots of a GPT the probe looks at, at most.
pub const PARTS_PER_DISK: usize = 8;

#[derive(Clone, Copy, Debug, Default)]
pub struct MbrEntry {
    pub status: u8,
    pub kind: u8,
    pub start: u32,
    pub size: u32,
}

/// An MBR, 512 bytes: the four entries, and the signature.
pub fn mbr(entries: &[MbrEntry; 4], signature: u16) -> Vec<u8> {
    let mut s = vec![0u8; 512];
    for (i, e) in entries.iter().enumerate() {
        let at = 446 + 16 * i;
        s[at] = e.status;
        s[at + 4] = e.kind;
        s[at + 8..at + 12].copy_from_slice(&e.start.to_le_bytes());
        s[at + 12..at + 16].copy_from_slice(&e.size.to_le_bytes());
    }
    s[510..512].copy_from_slice(&signature.to_le_bytes());
    s
}

#[derive(Clone, Debug)]
pub struct GptHeader {
    pub signature: u64,
    pub header_size: u32,
    /// None: the right one.
    pub crc: Option<u32>,
    pub my_lba: u64,
    pub alt_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
    pub entry_lba: u64,
    pub entries: u32,
    pub entry_size: u32,
}

/// The header's bytes, into a sector of `sector_size`: the checksum over
/// `header_size` bytes -- as far as the sector goes -- unless the header
/// says which.
pub fn gpt_header(h: &GptHeader, sector_size: usize) -> Vec<u8> {
    let mut s = vec![0u8; sector_size.max(92)];
    s[0..8].copy_from_slice(&h.signature.to_le_bytes());
    s[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    s[12..16].copy_from_slice(&h.header_size.to_le_bytes());
    s[24..32].copy_from_slice(&h.my_lba.to_le_bytes());
    s[32..40].copy_from_slice(&h.alt_lba.to_le_bytes());
    s[40..48].copy_from_slice(&h.first_usable.to_le_bytes());
    s[48..56].copy_from_slice(&h.last_usable.to_le_bytes());
    s[72..80].copy_from_slice(&h.entry_lba.to_le_bytes());
    s[80..84].copy_from_slice(&h.entries.to_le_bytes());
    s[84..88].copy_from_slice(&h.entry_size.to_le_bytes());
    let span = (h.header_size as usize).min(s.len());
    let crc = h.crc.unwrap_or_else(|| {
        let mut crc = crc32_update(0, &s[..16]);
        crc = crc32_update(crc, &[0u8; 4]);
        crc32_update(crc, &s[20..span.max(20)])
    });
    s[16..20].copy_from_slice(&crc.to_le_bytes());
    s.truncate(sector_size);
    s
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GptEntry {
    pub kind: [u8; 16],
    pub first: u64,
    pub last: u64,
}

pub fn gpt_entry(e: &GptEntry, entry_size: usize) -> Vec<u8> {
    let mut v = vec![0u8; entry_size.max(128)];
    v[0..16].copy_from_slice(&e.kind);
    v[16] = 0x5A;
    v[32..40].copy_from_slice(&e.first.to_le_bytes());
    v[40..48].copy_from_slice(&e.last.to_le_bytes());
    v.truncate(entry_size);
    v
}

/* ---- what the probe should find ---- */

/// A partition the kernel should register: which slot of the table, and
/// where on its disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    pub slot: u32,
    pub start: u64,
    pub count: u64,
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("four bytes"))
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("eight bytes"))
}

/// What the probe should register of a disk of `sectors` sectors of
/// `sector_size` bytes holding `media`, before any limit of the table's --
/// in the order it registers them.
pub fn expect(media: &Media, sector_size: u64, sectors: u64) -> Vec<Found> {
    let mut out = Vec::new();
    let ss = sector_size as usize;
    if !(512..=4096).contains(&ss) || sectors == 0 {
        return out;
    }
    let sector = |lba: u64| -> Option<Vec<u8>> {
        if lba >= sectors {
            None
        } else {
            Some(media.bytes(lba.wrapping_mul(sector_size), ss))
        }
    };
    let Some(s0) = sector(0) else { return out };
    if le16(&s0, 510) != MBR_SIGNATURE {
        return out;
    }
    let entry = |i: usize| {
        let at = 446 + 16 * i;
        (s0[at + 4], le32(&s0, at + 8) as u64, le32(&s0, at + 12) as u64)
    };
    let fits = |start: u64, count: u64| start != 0 && start.checked_add(count).is_some_and(|end| end <= sectors);
    if (0..4).any(|i| entry(i).0 == TYPE_GPT_PROTECTIVE) {
        let Some(h) = sector(1) else { return out };
        if le64(&h, 0) != GPT_SIGNATURE {
            return out;
        }
        let header_size = le32(&h, 12) as usize;
        let entry_size = le32(&h, 84) as usize;
        if header_size < 92 || header_size > ss || entry_size < 128 || entry_size > ss {
            return out;
        }
        let mut crc = crc32_update(0, &h[..16]);
        crc = crc32_update(crc, &[0u8; 4]);
        crc = crc32_update(crc, &h[20..header_size]);
        if crc != le32(&h, 16) {
            return out;
        }
        let per_sector = ss / entry_size;
        let entry_lba = le64(&h, 72);
        let count = (le32(&h, 80) as usize).min(PARTS_PER_DISK);
        let mut block: Option<Vec<u8>> = None;
        for i in 0..count {
            if i % per_sector == 0 {
                /* A slot's sector past the disk, or one whose number does
                 * not fit, ends the table. */
                let lba = match entry_lba.checked_add((i / per_sector) as u64) {
                    Some(lba) => lba,
                    None => break,
                };
                block = sector(lba);
                if block.is_none() {
                    break;
                }
            }
            let b = block.as_ref().expect("read above");
            let e = &b[(i % per_sector) * entry_size..(i % per_sector) * entry_size + 128];
            if e[..16].iter().all(|x| *x == 0) {
                continue;
            }
            let (first, last) = (le64(e, 32), le64(e, 40));
            if last < first {
                continue;
            }
            let Some(count) = (last - first).checked_add(1) else { continue };
            if fits(first, count) {
                out.push(Found { slot: i as u32, start: first, count });
            }
        }
        return out;
    }
    for i in 0..4 {
        let (kind, start, count) = entry(i);
        if kind == 0 || count == 0 {
            continue;
        }
        if fits(start, count) {
            out.push(Found { slot: i as u32, start, count });
        }
    }
    out
}
