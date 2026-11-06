//! CRC-32C (Castagnoli, the polynomial 0x1EDC6F41 reflected as 0x82F63B78),
//! as iSCSI, ext4 and etcd's WAL use it: what the log's records and a
//! snapshot's pieces are checked by. In software, eight bytes a step over
//! eight tables made at compile time ("slicing by eight") -- the kernel keeps
//! SSE off, so there is no CRC32 instruction to use, and nothing here needs
//! one.

const POLY: u32 = 0x82F6_3B78;

const fn make_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut s = 1;
        while s < 8 {
            let prev = t[s - 1][i];
            t[s][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            s += 1;
        }
        i += 1;
    }
    t
}

static TABLES: [[u32; 256]; 8] = make_tables();

/// The CRC of `bytes` carried on from `crc`, the CRC of what came before
/// them -- 0 for nothing: `update(update(0, a), b)` is the CRC of `a` and
/// `b` one after the other.
pub fn update(crc: u32, bytes: &[u8]) -> u32 {
    let mut c = !crc;
    let mut chunks = bytes.chunks_exact(8);
    for w in &mut chunks {
        let lo = c ^ u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        c = TABLES[7][(lo & 0xFF) as usize]
            ^ TABLES[6][((lo >> 8) & 0xFF) as usize]
            ^ TABLES[5][((lo >> 16) & 0xFF) as usize]
            ^ TABLES[4][(lo >> 24) as usize]
            ^ TABLES[3][w[4] as usize]
            ^ TABLES[2][w[5] as usize]
            ^ TABLES[1][w[6] as usize]
            ^ TABLES[0][w[7] as usize];
    }
    for &b in chunks.remainder() {
        c = (c >> 8) ^ TABLES[0][((c ^ u32::from(b)) & 0xFF) as usize];
    }
    !c
}

/// The CRC of `bytes`.
pub fn crc32c(bytes: &[u8]) -> u32 {
    update(0, bytes)
}
