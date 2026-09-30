//! The input, read as a stream of choices: past its end every read is 0,
//! which is always the ordinary choice, and `op` says there is no more.

pub struct Input<'a> {
    data: &'a [u8],
    at: usize,
}

/* Values that are where code goes wrong: the edges of each width, powers of
 * two and their neighbours, all ones. */
const EDGES16: [u16; 10] = [0, 1, 2, 0x7F, 0x80, 0xFF, 0x100, 0x7FFF, 0x8000, 0xFFFF];
const EDGES32: [u32; 14] = [0, 1, 2, 3, 0x7F, 0x80, 0xFF, 0x100, 0xFFFF, 0x1_0000, 0x7FFF_FFFF, 0x8000_0000,
                            0xFFFF_FFFE, 0xFFFF_FFFF];

impl<'a> Input<'a> {
    pub fn new(data: &'a [u8]) -> Input<'a> {
        Input { data, at: 0 }
    }

    /// Whether anything is left.
    pub fn more(&self) -> bool {
        self.at < self.data.len()
    }

    /// The next operation, of `n`, or None at the end of the input.
    pub fn op(&mut self, n: u8) -> Option<u8> {
        if self.at >= self.data.len() {
            return None;
        }
        Some(self.u8() % n)
    }

    pub fn u8(&mut self) -> u8 {
        let b = self.data.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        b
    }
    pub fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }
    pub fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }
    pub fn u64(&mut self) -> u64 {
        u64::from(self.u32()) | (u64::from(self.u32()) << 32)
    }
    pub fn bool(&mut self) -> bool {
        self.u8() & 1 != 0
    }
    /// True about one time in `n` (of 256): never past the end, whose
    /// zeros are the ordinary choice.
    pub fn chance(&mut self, n: u8) -> bool {
        self.more() && self.u8() < n
    }
    /// True about once in 4096: for what ends a run, which has to be much
    /// rarer than a run is long for the rest to be reached.
    pub fn rare(&mut self) -> bool {
        self.u16() >= 0xFFF0
    }
    /// A number below `n` (0 for none).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else if n <= 256 {
            u64::from(self.u8()) % n
        } else if n <= 1 << 16 {
            u64::from(self.u16()) % n
        } else {
            self.u64() % n
        }
    }
    /// A number in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    pub fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
    /// A 16-bit value: an edge, a power of two, or anything.
    pub fn value16(&mut self) -> u16 {
        match self.u8() % 4 {
            0 => self.pick(&EDGES16),
            1 => 1u16 << (self.u8() % 16),
            _ => self.u16(),
        }
    }
    /// A 32-bit value: an edge, a power of two, or anything.
    pub fn value32(&mut self) -> u32 {
        match self.u8() % 4 {
            0 => self.pick(&EDGES32),
            1 => 1u32 << (self.u8() % 32),
            _ => self.u32(),
        }
    }
    /// `n` bytes of the input's.
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.u8()).collect()
    }
}

/// `n` bytes from `seed`: varied, and not the input's to spend -- a frame's
/// payload drawn from the input a byte at a time would take most of it.
pub fn noise(seed: u32, n: usize) -> Vec<u8> {
    let mut x = u64::from(seed) | 1 << 40;
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(n);
    v
}
