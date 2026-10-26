//! The CPU's own random instruction as an entropy source: RDRAND and RDSEED
//! on x86-64, RNDR and RNDRRS on arm64 (hal/random.h, reached through
//! `kcore::random`). The only source the bare-metal machines this kernel
//! boots on all share; docs/random.md has which has what.

use kcore::random::{hw_kind, hw_random, hw_random_seed, HwKind};
use kcore::trace;

use crate::Source;

pub struct Cpu;

/// The name the pool knows the instruction by, or None when the CPU has
/// none (or hwrng=off turned it away).
pub fn name() -> Option<&'static str> {
    match hw_kind() {
        HwKind::None => None,
        HwKind::RdRand => Some("rdrand"),
        HwKind::RdSeed => Some("rdseed"),
        HwKind::Rndr => Some("rndr"),
    }
}

/// Draws a few values and refuses the instruction if they all come back the
/// same. That is what a broken RDRAND looks like -- some AMD parts return
/// all-ones forever after a resume -- and what a hypervisor that stubs the
/// instruction out looks like too, and either way the CPUID bit says the
/// instruction is there.
pub fn self_test() -> bool {
    /* Eight draws: enough to tell a working source from one that answers the
     * same thing every time, cheap enough to do on every boot. */
    const DRAWS: usize = 8;

    let mut first = None;
    let mut differs = false;
    let mut ok = 0;
    for _ in 0..DRAWS {
        let Some(value) = hw_random() else { continue };
        ok += 1;
        match first {
            None => first = Some(value),
            Some(first) if first != value => differs = true,
            Some(_) => {}
        }
    }

    /* Half the draws failing is a source not worth having either: the retry
     * loops inside the HAL have already been patient. */
    if ok < DRAWS / 2 {
        trace!(0, "HwRandom: only {} of {} draws succeeded", ok, DRAWS);
        return false;
    }
    if !differs {
        trace!(0, "HwRandom: every draw returned {:#X}", first.unwrap_or(0));
        return false;
    }
    true
}

impl Source for Cpu {
    /// The conditioned entropy where the CPU has an instruction for it, the
    /// whitened output where it has not: what another generator wants to be
    /// keyed from.
    fn fill(&'static self, buf: &mut [u8]) -> bool {
        for piece in buf.chunks_mut(8) {
            let Some(value) = hw_random_seed() else { return false };
            piece.copy_from_slice(&value.to_ne_bytes()[..piece.len()]);
        }
        !buf.is_empty()
    }
}
