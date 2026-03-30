/// Fixed-size inline bitmap for slot allocation.
///
/// `W` is the number of `u64` words in the storage (capacity = `W * 64` bits).
/// Does not allocate from the heap.
///
/// Bit value `1` means **used** (allocated), `0` means **free**.
///
/// # Example
/// ```ignore
/// /* 64-slot bitmap: W = 1 (1 × 64 bits) */
/// let mut map = BitMap::<1>::new();
/// let slot = map.alloc().expect("no free slots");
/// /* ... use slot ... */
/// map.free(slot);
/// ```
pub struct BitMap<const W: usize> {
    words: [u64; W],
}

impl<const W: usize> BitMap<W> {
    pub const fn new() -> Self {
        Self { words: [0u64; W] }
    }

    /// Total number of bits (slots) in this bitmap.
    pub const fn capacity() -> usize {
        W * 64
    }

    /// Mark bit `bit` as used (set to 1).  Panics if `bit >= capacity()`.
    pub fn set(&mut self, bit: usize) {
        assert!(bit < W * 64);
        self.words[bit / 64] |= 1u64 << (bit % 64);
    }

    /// Mark bit `bit` as free (set to 0).  Panics if `bit >= capacity()`.
    pub fn clear(&mut self, bit: usize) {
        assert!(bit < W * 64);
        self.words[bit / 64] &= !(1u64 << (bit % 64));
    }

    /// Returns `true` if bit `bit` is used.  Panics if `bit >= capacity()`.
    pub fn is_set(&self, bit: usize) -> bool {
        assert!(bit < W * 64);
        (self.words[bit / 64] >> (bit % 64)) & 1 != 0
    }

    /// Find the index of the first free (zero) bit.
    /// Returns `None` if all bits are used.
    pub fn find_first_zero(&self) -> Option<usize> {
        for (wi, &word) in self.words.iter().enumerate() {
            if word != u64::MAX {
                return Some(wi * 64 + word.trailing_ones() as usize);
            }
        }
        None
    }

    /// Allocate a free slot: find the first zero bit, mark it used, and
    /// return its index.  Returns `None` if all slots are used.
    pub fn alloc(&mut self) -> Option<usize> {
        let bit = self.find_first_zero()?;
        self.set(bit);
        Some(bit)
    }

    /// Free a previously allocated slot.  Panics if `bit >= capacity()`.
    pub fn free(&mut self, bit: usize) {
        self.clear(bit);
    }

    /// Return the number of currently allocated (set) bits.
    pub fn count_used(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Return `true` if all bits are free.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// Return `true` if all bits are used.
    pub fn is_full(&self) -> bool {
        self.find_first_zero().is_none()
    }
}
