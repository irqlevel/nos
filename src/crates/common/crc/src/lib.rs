//! The cyclic redundancy checks the tree's formats are checked by, each in
//! one place:
//!
//! - [`crc32`]: CRC-32 (IEEE 802.3, the polynomial reflected as 0xEDB88320),
//!   what zlib computes -- GPT's header and partition array, the disk
//!   log's header, the `crc32` command;
//! - [`crc32c`]: CRC-32C (Castagnoli, reflected as 0x82F63B78) -- ext4's
//!   metadata_csum and its journal, raft's log;
//! - [`crc16`]: CRC-16 (x^16 + x^15 + x^2 + 1, reflected as 0xA001) -- ext4's
//!   gdt_csum on group descriptors.
//!
//! No kernel and no `unsafe` in it: the kernel's crates, the guest's hvboot
//! and the host's fuzzers and tests use the same functions. (A test's judge
//! that must share nothing with the code it judges -- fs-fuzz's ext4 check --
//! keeps its own, by design.)
//!
//! Each in two forms. `update` is the CRC as a message has it -- inverted
//! going in and coming out -- carried on: start from 0, feed the pieces in
//! order, and the result is the whole's. `raw` is the register itself, with
//! no inversion at either end, as ext4 chains it (`ext4_chksum`,
//! lib/crc16.c): `update(c, d) == !raw(!c, d)`.
//!
//! The 32-bit ones eight bytes a step, each looked up in a table of its own
//! made at compile time ("slicing by eight"), where a byte at a time waits
//! on the last lookup for the next: 4 KiB in 1.5 us on an M4 Pro where a
//! byte at a time took 6.8. The kernel keeps SSE off, so there is no CRC32
//! instruction to use, and nothing here needs one.

#![no_std]
#![forbid(unsafe_code)]

/// The eight tables of slicing by eight for a reflected polynomial: `[0]` a
/// byte's, and `[k]` what the byte does to the register with `k` more bytes
/// after it -- `[k][i]` is `[k - 1][i]` carried on over one zero byte.
const fn tables(poly: u32) -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ poly } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
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

/// The register `c` carried on over `data`: eight bytes a step, the bytes
/// past the last eight one at a time.
#[inline]
fn slice8(t: &[[u32; 256]; 8], mut c: u32, data: &[u8]) -> u32 {
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

/// CRC-32 (IEEE 802.3), zlib's: GPT, the disk log.
pub mod crc32 {
    static TABLES: [[u32; 256]; 8] = super::tables(0xEDB8_8320);

    /// The register carried on over `data`, with no inversion.
    #[inline]
    pub fn raw(crc: u32, data: &[u8]) -> u32 {
        super::slice8(&TABLES, crc, data)
    }

    /// The CRC of what came before -- 0 for nothing -- carried on over
    /// `data`: `update(update(0, a), b)` is the CRC of `a` then `b`.
    #[inline]
    pub fn update(crc: u32, data: &[u8]) -> u32 {
        !raw(!crc, data)
    }

    /// The CRC of `data`.
    #[inline]
    pub fn checksum(data: &[u8]) -> u32 {
        update(0, data)
    }

    /// A block's CRC with the four bytes of its own checksum field, at
    /// `hole`, counted as zeros -- how a checksum that lives inside what it
    /// covers is defined. None for a field that is not inside the block.
    pub fn with_hole(block: &[u8], hole: usize) -> Option<u32> {
        let end = hole.checked_add(4).filter(|&e| e <= block.len())?;
        let c = update(0, &block[..hole]);
        Some(update(update(c, &[0u8; 4]), &block[end..]))
    }
}

/// CRC-32C (Castagnoli): ext4's metadata_csum and journal, raft's log.
pub mod crc32c {
    static TABLES: [[u32; 256]; 8] = super::tables(0x82F6_3B78);

    /// The register carried on over `data`, with no inversion -- as ext4
    /// chains it, `ext4_chksum(crc, data)`.
    #[inline]
    pub fn raw(crc: u32, data: &[u8]) -> u32 {
        super::slice8(&TABLES, crc, data)
    }

    /// The CRC of what came before -- 0 for nothing -- carried on over
    /// `data`.
    #[inline]
    pub fn update(crc: u32, data: &[u8]) -> u32 {
        !raw(!crc, data)
    }

    /// The CRC of `data`.
    #[inline]
    pub fn checksum(data: &[u8]) -> u32 {
        update(0, data)
    }
}

/// CRC-16 (the reflected 0xA001, "CRC-16/ARC" with no inversion): ext4's
/// gdt_csum, which starts it at !0.
pub mod crc16 {
    const fn table() -> [u16; 256] {
        let mut t = [0u16; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u16;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xA001 } else { c >> 1 };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    }

    static TABLE: [u16; 256] = table();

    /// The register carried on over `data`, with no inversion -- lib/crc16.c's
    /// `crc16(crc, data)`.
    #[inline]
    pub fn raw(crc: u16, data: &[u8]) -> u16 {
        let mut c = crc;
        for &b in data {
            c = (c >> 8) ^ TABLE[((c ^ u16::from(b)) & 0xFF) as usize];
        }
        c
    }
}
