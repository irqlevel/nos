//! CRC-32 in its reflected 0xEDB88320 form -- what a GPT header carries over
//! itself, and the same function the C++ side has as `Stdlib::Crc32`.
//!
//! Only the streaming form is here, because that is what a GPT header needs:
//! its checksum covers the header with the checksum field itself zeroed, so
//! it is taken in three pieces rather than by editing the sector that was
//! read. A whole buffer at once is `crc32_update(0, buf)`.

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
