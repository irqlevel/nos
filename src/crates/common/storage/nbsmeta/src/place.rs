//! Where a new chunk's replicas go (plans/nbs.md, "Where replicas go"):
//! one pure function of the committed state, the same in every member's
//! apply, so every mds reaches the same answer and the fuzzer can hold it to
//! the rules. In integers, in the table's own order: no float, no clock, no
//! randomness but a hash of numbers.
//!
//! 1. Three replicas on three hosts: a host is the failure domain, and one
//!    cs a host makes three cs's three hosts.
//! 2. Only cs's that take new chunks: their hosts members, their devices
//!    active, with room. Fewer than three that take any is the cluster's
//!    shape (`TooFewHosts`); three or more, full, its room (`NoRoom`).
//! 3. One replica on the volume's own host where it has room, so that its
//!    reads are local.
//! 4. The other two on the hosts with the most room left, ties broken by a
//!    hash of the volume's number, the chunk's place in it and the cs's: a
//!    volume's chunks spread over many sets, so a host lost is copied back
//!    from many at once.
//! 5. Room counted as reserved, never as used: a placement never takes more
//!    than a cs has, and a volume that does not fit is refused whole (the
//!    caller tries it on a copy first).
//! 6. Room kept to rebuild a host: with four hosts or more, a placement that
//!    would leave the others less free room than the biggest host's
//!    replicas take is refused. Counted in sum, which is what rebuilding
//!    onto any of them needs at least. With three hosts there is nowhere to
//!    rebuild to, and the rule does not apply.
//!
//! Picking the three with the most room first is what places the most: a
//! placement of `n` chunks fits exactly when the hosts' free room, each
//! counted up to `n`, adds up to `3n` at least, and taking the fullest first
//! never leaves room only one host can use.

use alloc::vec::Vec;

use crate::storage::REPLICAS;
use crate::State;

#[derive(Clone)]
struct Candidate {
    /// The cs's number.
    cs: u32,
    slots: u64,
    reserved: u64,
    free: u64,
}

/// A placement under way, over the room the state had when it began.
#[derive(Clone)]
pub(crate) struct Placer {
    candidates: Vec<Candidate>,
    /// Where the volume's own host's cs is among them.
    home: Option<usize>,
}

impl Placer {
    /// The cs's of member hosts with devices that take new chunks; `host`'s
    /// first where it is one of them.
    pub(crate) fn new(state: &State, host: &str) -> Placer {
        let mut candidates = Vec::new();
        for c in state.cs.iter() {
            let slots = c.slots();
            if slots == 0 || state.member(&c.host).is_none() {
                continue;
            }
            candidates.push(Candidate { cs: c.id, slots, reserved: c.reserved, free: slots.saturating_sub(c.reserved) });
        }
        let home = state.cs_of(host).and_then(|h| candidates.iter().position(|c| c.cs == h.id));
        Placer { candidates, home }
    }

    /// How many hosts take new chunks, room left on them or not.
    pub(crate) fn eligible(&self) -> usize {
        self.candidates.len()
    }

    /// The cs's of chunk `index` of volume `volume`, in increasing order --
    /// their room taken -- or None where three hosts have none left, and then
    /// nothing is taken.
    pub(crate) fn pick(&mut self, volume: u32, index: u64) -> Option<[u32; REPLICAS]> {
        let mut chosen = [usize::MAX; REPLICAS];
        let mut n = 0;
        if let Some(h) = self.home {
            if self.candidates[h].free > 0 {
                chosen[0] = h;
                n = 1;
            }
        }
        while n < REPLICAS {
            let mut best: Option<(u64, u64, usize)> = None;
            for (i, c) in self.candidates.iter().enumerate() {
                if c.free == 0 || chosen[..n].contains(&i) {
                    continue;
                }
                let key = (c.free, tie(volume, index, c.cs));
                if best.is_none_or(|(f, t, _)| key > (f, t)) {
                    best = Some((key.0, key.1, i));
                }
            }
            chosen[n] = best?.2;
            n += 1;
        }
        let mut cs = [0u32; REPLICAS];
        for (slot, &i) in cs.iter_mut().zip(chosen.iter()) {
            let c = &mut self.candidates[i];
            c.free -= 1;
            c.reserved += 1;
            *slot = c.cs;
        }
        cs.sort_unstable();
        Some(cs)
    }

    /// Rule 6: with four hosts or more, whether the others' free room could
    /// take every replica the biggest host has.
    pub(crate) fn leaves_rebuild_room(&self) -> bool {
        if self.candidates.len() < REPLICAS + 1 {
            return true;
        }
        let Some(biggest) = self.candidates.iter().enumerate().max_by_key(|(i, c)| (c.reserved, usize::MAX - i))
            .map(|(i, _)| i) else { return true };
        let others: u64 = self.candidates.iter().enumerate().filter(|(i, _)| *i != biggest)
            .map(|(_, c)| c.slots.saturating_sub(c.reserved)).sum();
        others >= self.candidates[biggest].reserved
    }
}

/// A tie broken: a hash of the volume, the chunk's place in it and the cs
/// (splitmix64's finalizer), the same on every member.
fn tie(volume: u32, index: u64, cs: u32) -> u64 {
    let mut z = (u64::from(volume) << 32 ^ index).wrapping_add(u64::from(cs).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
