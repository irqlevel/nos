//! The harness's own randomness: SplitMix64 from the seed a run is named
//! by, so that a run is the same every time it is made again.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0xD1B5_4A32_D192_ED03)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A draw in `0..n`; 0 for an `n` of 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    /// A draw in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    /// True `pct` times in a hundred.
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }

    pub fn pick<T: Copy>(&mut self, of: &[T]) -> Option<T> {
        if of.is_empty() { None } else { Some(of[self.below(of.len() as u64) as usize]) }
    }
}

/// A 64-bit mix of two values: a state's hash carried through its entries.
pub fn mix(h: u64, v: u64) -> u64 {
    let mut z = h ^ v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for chunk in b.chunks(8) {
        let mut a = [0u8; 8];
        a[..chunk.len()].copy_from_slice(chunk);
        h = mix(h, u64::from_le_bytes(a));
    }
    mix(h, b.len() as u64)
}
