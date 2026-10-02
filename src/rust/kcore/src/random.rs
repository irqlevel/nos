//! The kernel's random pool (src/rust/random) as the layers and the loadable
//! modules reach it: through the C ABI, so that a fuzzer can stand in for it
//! and make what is random there the input's. And the CPU's own instruction
//! (hal/random.h), which the pool takes as one of its sources.

use ffi::random;

/// `buf` filled from the pool: false, and nothing written, while nothing has
/// seeded it -- or when `buf` is empty.
pub fn fill_random(buf: &mut [u8]) -> bool {
    if buf.is_empty() {
        return false;
    }
    unsafe { random::kernel_get_random(buf.as_mut_ptr(), buf.len()) != 0 }
}

pub fn random_u64() -> Option<u64> {
    let mut bytes = [0u8; core::mem::size_of::<u64>()];
    if fill_random(&mut bytes) {
        /* The bytes the pool wrote, as they lie: what writing through a
         * pointer to a u64 gave. */
        Some(u64::from_ne_bytes(bytes))
    } else {
        None
    }
}

/// Which random instruction the CPU has
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HwKind {
    /// None, or hwrng=off turned it away
    None,
    /// x86-64, RDRAND alone
    RdRand,
    /// x86-64, RDSEED and RDRAND
    RdSeed,
    /// arm64, RNDR and RNDRRS
    Rndr,
}

/// As the boot's probe (Hal::ProbeHwRandom) found it: None before then.
pub fn hw_kind() -> HwKind {
    match random::kernel_hw_random_kind() {
        1 => HwKind::RdRand,
        2 => HwKind::RdSeed,
        3 => HwKind::Rndr,
        _ => HwKind::None,
    }
}

/// The instruction's whitened output, or None when it would not give one --
/// or there is no instruction.
pub fn hw_random() -> Option<u64> {
    let draw = random::kernel_hw_random();
    (draw.ok == 1).then_some(draw.value)
}

/// Its raw conditioned entropy where the CPU has an instruction for that,
/// the whitened output where it has not: what another generator wants to be
/// keyed from.
pub fn hw_random_seed() -> Option<u64> {
    let draw = random::kernel_hw_random_seed();
    (draw.ok == 1).then_some(draw.value)
}
