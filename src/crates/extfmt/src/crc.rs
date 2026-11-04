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

/// The tables of slicing by 8: `[0]` a byte's, as `table32` has it, and
/// `[k]` what the byte does to the value with `k` more bytes after it --
/// `[k][i]` is `[k - 1][i]` carried on over one zero byte.
const fn tables32() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    t[0] = table32();
    let mut i = 0;
    while i < 256 {
        let mut k = 1;
        while k < 8 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            k += 1;
        }
        i += 1;
    }
    t
}

static CRC32C_TABLES: [[u32; 256]; 8] = tables32();
static CRC16_TABLE: [u16; 256] = table16();

/// `crc` carried on over `data`: eight bytes at a time, each of them looked
/// up in its own table at once, where a byte at a time waits on the last
/// lookup for the next -- 4 KiB in 1.5 us on an M4 Pro where a byte at a time
/// took 6.8. Every extent node a walk reads is held to its checksum, and on
/// the EX44 a guest's flush after a 2 GiB first write spent 48 ms of its 54
/// in such walks, one an extent. The bytes past the last eight, one at a
/// time.
pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    let t = &CRC32C_TABLES;
    let mut c = crc;
    let mut words = data.chunks_exact(8);
    for w in &mut words {
        let lo = u32::from_le_bytes([w[0], w[1], w[2], w[3]]) ^ c;
        let hi = u32::from_le_bytes([w[4], w[5], w[6], w[7]]);
        c = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xFF) as usize]
            ^ t[2][((hi >> 8) & 0xFF) as usize]
            ^ t[1][((hi >> 16) & 0xFF) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in words.remainder() {
        c = t[0][((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
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
