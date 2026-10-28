//! The two checksums of the format: CRC32C (Castagnoli), which
//! metadata_csum -- and the journal -- put on everything, and the CRC16
//! that gdt_csum, metadata_csum's predecessor, puts on group descriptors.
//!
//! Both as the kernel computes them (`ext4_chksum`, lib/crc16.c): the
//! running value in and out, with no inversion at either end, so that one
//! call carries on where another stopped and a checksum over pieces --
//! an inode's number, then its bytes -- is a chain of calls. The usual
//! CRC32C of a message is `!crc32c(!0, message)`.

/// CRC32C's polynomial, bit-reversed.
const CASTAGNOLI: u32 = 0x82F6_3B78;
/// CRC16's (x^16 + x^15 + x^2 + 1), bit-reversed.
const CRC16_POLY: u16 = 0xA001;

const fn table32() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ CASTAGNOLI } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

const fn table16() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ CRC16_POLY } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC32C_TABLE: [u32; 256] = table32();
static CRC16_TABLE: [u16; 256] = table16();

/// `crc` carried on over `data`.
pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    let mut c = crc;
    for &b in data {
        c = CRC32C_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

/// `crc` carried on over `data`.
pub fn crc16(crc: u16, data: &[u8]) -> u16 {
    let mut c = crc;
    for &b in data {
        c = (c >> 8) ^ CRC16_TABLE[((c ^ u16::from(b)) & 0xFF) as usize];
    }
    c
}
