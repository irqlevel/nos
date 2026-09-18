//! CRC-32 in its reflected 0xEDB88320 form -- what a GPT header carries over
//! itself and what nanofs puts on every block, and the same function the C++
//! side has as `Stdlib::Crc32`.
//!
//! Only the streaming form is here, because that is what those checksums
//! need: each covers a block with its own checksum field zeroed, so it is
//! taken in pieces rather than by editing the block that was read. A whole
//! buffer at once is `crc32_update(0, buf)`.

const POLY: u32 = 0xEDB8_8320;

const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Feeds more data into a CRC: start from 0, feed the pieces in order, and
/// the result is what the whole would give.
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut crc = crc ^ 0xFFFF_FFFF;
    for byte in data {
        crc = TABLE[((crc ^ *byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// A block's CRC with the four bytes of its own checksum field, at `hole`,
/// counted as zeros -- which is how a checksum that lives inside what it
/// covers is defined.
pub fn crc32_with_hole(buf: &[u8], hole: usize) -> u32 {
    if hole + 4 > buf.len() {
        return 0;
    }
    let mut crc = crc32_update(0, &buf[..hole]);
    crc = crc32_update(crc, &[0u8; 4]);
    crc32_update(crc, &buf[hole + 4..])
}
