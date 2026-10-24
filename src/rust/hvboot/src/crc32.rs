//! CRC-32 (IEEE 802.3, reflected), which GPT's header and partition array
//! are checked by.

const POLY: u32 = 0xEDB8_8320;

/// Fed a piece at a time: what the CRC of all of them is.
pub struct Crc32(u32);

impl Crc32 {
    pub fn new() -> Crc32 {
        Crc32(!0)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut crc = self.0;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
            }
        }
        self.0 = crc;
    }

    pub fn finish(&self) -> u32 {
        !self.0
    }
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}
