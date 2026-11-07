//! The randomness an election timeout takes: SplitMix64 over the caller's
//! seed, so that a simulation runs the same every time and two members given
//! different seeds draw differently. No floats, nothing from outside.

/// Added to the state each draw: the golden ratio's 64 bits.
const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
/// SplitMix64's two multipliers and three shifts.
const MIX_A: u64 = 0xBF58_476D_1CE4_E5B9;
const MIX_B: u64 = 0x94D0_49BB_1331_11EB;
const SHIFT_A: u32 = 30;
const SHIFT_B: u32 = 27;
const SHIFT_C: u32 = 31;

#[derive(Clone, Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GAMMA);
        let mut z = self.0;
        z = (z ^ (z >> SHIFT_A)).wrapping_mul(MIX_A);
        z = (z ^ (z >> SHIFT_B)).wrapping_mul(MIX_B);
        z ^ (z >> SHIFT_C)
    }

    /// A draw in `0..n`, 0 for an `n` of 0.
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}
