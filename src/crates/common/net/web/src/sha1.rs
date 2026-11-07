//! SHA-1 (FIPS 180-4), for what needs it and nothing that trusts it: a
//! WebSocket handshake's accept key (RFC 6455 4.2.2), which shows that the
//! server read the client's request as a WebSocket one -- a check that both
//! ends speak the protocol, not a defence against anyone.

pub struct Sha1 {
    h: [u32; 5],
    block: [u8; 64],
    /// Bytes in `block`.
    used: usize,
    /// Bytes hashed in all.
    total: u64,
}

impl Sha1 {
    pub fn new() -> Sha1 {
        Sha1 { h: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0], block: [0; 64], used: 0, total: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        while !data.is_empty() {
            let n = (64 - self.used).min(data.len());
            self.block[self.used..self.used + n].copy_from_slice(&data[..n]);
            self.used += n;
            data = &data[n..];
            if self.used == 64 {
                compress(&mut self.h, &self.block);
                self.used = 0;
            }
        }
    }

    pub fn finish(mut self) -> [u8; 20] {
        let bits = self.total.wrapping_mul(8);
        /* 0x80, then zeros to 56 bytes into a block, then the length */
        self.update(&[0x80]);
        while self.used != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 20];
        for (o, h) in out.chunks_exact_mut(4).zip(self.h.iter()) {
            o.copy_from_slice(&h.to_be_bytes());
        }
        out
    }
}

impl Default for Sha1 {
    fn default() -> Sha1 {
        Sha1::new()
    }
}

/// SHA-1 of `data`.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut s = Sha1::new();
    s.update(data);
    s.finish()
}

fn compress(h: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 80];
    for (i, word) in block.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *h;
    for (i, wi) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
            20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
            _ => (b ^ c ^ d, 0xCA62_C1D6),
        };
        let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = t;
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
}
