//! The kernel's random pool (src/crates/base/random) as the layers and the loadable
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

/// A word from the pool -- or, while nothing has seeded it, one from the
/// cycle counter and the clock, scrambled: varying, not secret -- said once
/// where it was taken (`said`, the call site's own flag). For a value that
/// has to differ from one use to the next more than it has to be secret: a
/// DHCP transaction's id, a DNS query's, TCP's initial sequence number. An
/// `unwrap_or(constant)` there made every one of them the same on a machine
/// whose pool was not seeded, with nothing said where it was used.
pub fn random_u64_or_clock(what: &str, said: &core::sync::atomic::AtomicBool) -> u64 {
    if let Some(r) = random_u64() {
        return r;
    }
    if !said.swap(true, core::sync::atomic::Ordering::Relaxed) {
        crate::klog!(0, "random: the pool is unseeded -- {} from the clock, which is no secret", what);
    }
    /* SplitMix64's finaliser over the two clocks */
    let mut z = crate::time::cycle_counter() ^ crate::time::boot_time_ns().rotate_left(32);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
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

/// As the boot's probe (hal::probe_hw_random) found it: None before then.
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
