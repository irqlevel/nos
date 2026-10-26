//! Timing jitter: what is left on a machine with no random instruction and no
//! virtio-rng, which is the only reason it exists. Each bit is the low bit of
//! how long a fixed piece of work took, measured with the cycle counter and
//! debiased by von Neumann extraction (keep a differing pair, discard an
//! equal one); what varies between two measurements is the cache and
//! store-buffer state, the branch predictor, an SMT sibling, and any
//! interrupt that lands in the middle.
//!
//! This is the weakest source here by a wide margin and the hardest to put a
//! number on -- see docs/random.md. It fails rather than returning biased
//! bytes when the counter is too coarse to show any variance at all.

use kcore::time::cycle_counter;
use kcore::trace;

use crate::Source;

/// The walked buffer: bigger than a cache line by enough that a pass over it
/// touches many
const SCRATCH_WORDS: usize = 1024;
const SCRATCH_MASK: usize = SCRATCH_WORDS - 1;
/// 64 bytes -- one cache line per step on both arches
const STRIDE_WORDS: usize = 16;
const WALK_STEPS: usize = 64;
const WALK_STEPS_MASK: u32 = 63;
/// A byte needs eight surviving pairs; give up rather than spin forever on a
/// counter whose deltas never differ
const MAX_PAIRS_PER_BYTE: usize = 256;

pub struct Jitter;

impl Source for Jitter {
    fn fill(&'static self, buf: &mut [u8]) -> bool {
        collect(buf)
    }
}

/// What a measurement walks, and what steers the walk: the caller's for the
/// length of a call -- 4 KiB of its stack -- so that two reseeds at once
/// share nothing and need no lock.
struct Walk {
    scratch: [u32; SCRATCH_WORDS],
    acc: u32,
}

fn collect(buf: &mut [u8]) -> bool {
    if buf.is_empty() {
        return false;
    }

    let mut walk = Walk { scratch: [0; SCRATCH_WORDS], acc: cycle_counter() as u32 };
    /* The walk is what a measurement times. With its address handed to
     * something the compiler cannot see into, the compiler must take it that
     * the cycle-counter calls on either side may read it, and so can move
     * none of the walk's loads and stores out from between them. */
    core::hint::black_box(&mut walk);

    for byte in buf.iter_mut() {
        match walk.byte() {
            Some(value) => *byte = value,
            None => {
                /* Every measurement took exactly as long as the one before
                 * it: the counter is too coarse to see this much work, or it
                 * is not running. Biased bytes would be worse than none. */
                trace!(0, "Jitter: no variance in {} cycle-counter measurements", 2 * MAX_PAIRS_PER_BYTE);
                return false;
            }
        }
    }
    true
}

impl Walk {
    fn byte(&mut self) -> Option<u8> {
        let mut value = 0u8;
        let mut bits = 0;
        for _ in 0..MAX_PAIRS_PER_BYTE {
            if bits == 8 {
                break;
            }
            let a = self.sample();
            let b = self.sample();
            /* Von Neumann: a pair that agrees says nothing about the bias
             * behind it, a pair that differs is an unbiased bit whichever way
             * the source leans. */
            if a == b {
                continue;
            }
            value = (value << 1) | a as u8;
            bits += 1;
        }
        (bits == 8).then_some(value)
    }

    /// The low bit of how long one walk took
    fn sample(&mut self) -> u32 {
        let t0 = cycle_counter();

        /* A dependent read-modify-write walk, one cache line per step, with
         * a trip count that depends on the state the previous measurement
         * left. */
        let steps = WALK_STEPS + (self.acc & WALK_STEPS_MASK) as usize;
        let mut idx = self.acc as usize & SCRATCH_MASK;
        for i in 0..steps {
            self.acc = self.acc.wrapping_add(self.scratch[idx]).wrapping_add(i as u32);
            self.scratch[idx] = self.acc;
            idx = (idx + STRIDE_WORDS) & SCRATCH_MASK;
        }

        let t1 = cycle_counter();
        let delta = t1.wrapping_sub(t0);

        /* Fold the measurement back in, so the next walk takes a different
         * path and a different amount of time. */
        self.acc ^= delta as u32 ^ t1 as u32;
        (delta & 1) as u32
    }
}
