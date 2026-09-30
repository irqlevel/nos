//! The disk log's area, as scripts/disklog.py prepares it and reads it back:
//! a header in the first sector -- magic, version, the sector size, the
//! area's length, the boot's sequence number, how much text there is, a
//! CRC -- and the text from the second sector on.

use kcore::crc32::crc32_update;

pub const MAGIC: u64 = 0x0031_474F_4C53_4F4E;
pub const VERSION: u32 = 1;
pub const HEADER: usize = 48;
const CRC_AT: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub magic: u64,
    pub version: u32,
    pub sector_size: u32,
    pub area_sectors: u64,
    pub boot_seq: u64,
    pub log_bytes: u64,
}

/// The header's 48 bytes, its CRC right -- or, with `crc` given, that one.
pub fn header(h: &Header, crc: Option<u32>) -> Vec<u8> {
    let mut v = vec![0u8; HEADER];
    v[0..8].copy_from_slice(&h.magic.to_le_bytes());
    v[8..12].copy_from_slice(&h.version.to_le_bytes());
    v[12..16].copy_from_slice(&h.sector_size.to_le_bytes());
    v[16..24].copy_from_slice(&h.area_sectors.to_le_bytes());
    v[24..32].copy_from_slice(&h.boot_seq.to_le_bytes());
    v[32..40].copy_from_slice(&h.log_bytes.to_le_bytes());
    let sum = crc.unwrap_or_else(|| crc32_update(0, &v[..CRC_AT]));
    v[CRC_AT..CRC_AT + 4].copy_from_slice(&sum.to_le_bytes());
    v
}

/// The header in `sector`, and whether its CRC is right.
pub fn parse(sector: &[u8]) -> Option<(Header, bool)> {
    if sector.len() < HEADER {
        return None;
    }
    let u32_at = |at: usize| u32::from_le_bytes(sector[at..at + 4].try_into().expect("four bytes"));
    let u64_at = |at: usize| u64::from_le_bytes(sector[at..at + 8].try_into().expect("eight bytes"));
    let h = Header {
        magic: u64_at(0),
        version: u32_at(8),
        sector_size: u32_at(12),
        area_sectors: u64_at(16),
        boot_seq: u64_at(24),
        log_bytes: u64_at(32),
    };
    Some((h, crc32_update(0, &sector[..CRC_AT]) == u32_at(CRC_AT)))
}

/// Whether the kernel takes this sector as a prepared area's header, on a
/// device of `sector_size`-byte sectors, `sectors` of them.
pub fn prepared(sector: &[u8], sector_size: u64, sectors: u64) -> Option<Header> {
    if sector_size < HEADER as u64 || sector_size > 4096 {
        return None;
    }
    let (h, crc_ok) = parse(sector)?;
    if h.magic != MAGIC || h.version != VERSION || !crc_ok || h.sector_size as u64 != sector_size
        || h.area_sectors < 2 || h.area_sectors > sectors
    {
        return None;
    }
    Some(h)
}
