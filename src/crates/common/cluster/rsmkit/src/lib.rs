//! What a group's state machine over `raft::rsm` is made with besides its
//! own, written once -- NBS's metadata (`nbsmeta`) and vnet's state
//! (`vnetmeta`):
//!
//! - **its digests** ([`Checks`], [`digest`]): every member takes its state's
//!   digest at the same entries -- the SHA-256 of its snapshot's bytes, which
//!   are the state's in a fixed order -- the leader proposes its own, and a
//!   member whose own differs says so and is stopped: a determinism bug is a
//!   report, not a silent fork;
//! - **a snapshot read as a stream** ([`FromSnapshot`]): a state of tens of
//!   MiB never in one buffer, where the kernel's heap gives no block past
//!   512 KiB.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use raft::rsm::{Frozen, SnapshotReader};
use sha2::{Digest as _, Sha256};

/// A snapshot's reader, as `codec::Stream` reads it.
pub struct FromSnapshot<'r, 'a>(pub &'r mut SnapshotReader<'a>);

impl codec::Source for FromSnapshot<'_, '_> {
    fn fill(&mut self, buf: &mut [u8]) -> bool {
        self.0.read_exact(buf)
    }
}

/// How much a digest asks of the snapshot's stream at a time.
const DIGEST_STEP: usize = 64 * 1024;
/// Checkpoints a member keeps the digest of: a leader's for an older one
/// cannot be held to.
pub const RECENT: usize = 4;

/// The SHA-256 of a frozen state's bytes, read off it a step at a time:
/// never the whole of a big state in one buffer.
pub fn digest<F: Frozen>(mut f: F) -> [u8; 32] {
    let mut h = Sha256::new();
    let mut buf = Vec::with_capacity(DIGEST_STEP);
    loop {
        buf.clear();
        f.write(DIGEST_STEP, &mut buf);
        if buf.is_empty() {
            break;
        }
        h.update(&buf);
    }
    h.finalize().into()
}

/// A member's own record of its state's digests, kept out of the state:
/// the last few checkpoints', and the first leader's digest that differed.
#[derive(Clone, Debug, Default)]
pub struct Checks {
    recent: Vec<(u64, [u8; 32])>,
    diverged: Option<Diverged>,
    /// The checkpoints held to the leader's, and how many it had no digest of
    /// to hold to.
    pub held: u64,
    pub unheld: u64,
}

/// Where this member's state stopped being the leader's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Diverged {
    /// The checkpoint's entry.
    pub at: u64,
    pub mine: [u8; 32],
    pub theirs: [u8; 32],
    /// The entry the leader's digest came in.
    pub seen: u64,
}

impl Checks {
    /// The digest of checkpoint `at`, this member's own.
    pub fn push(&mut self, at: u64, digest: [u8; 32]) {
        if self.recent.len() >= RECENT {
            self.recent.remove(0);
        }
        self.recent.push((at, digest));
    }

    /// The leader's digest of checkpoint `at`, which came in at entry
    /// `seen`, held to this member's own where it has one.
    pub fn compare(&mut self, at: u64, theirs: &[u8; 32], seen: u64) {
        match self.recent.iter().find(|(i, _)| *i == at) {
            Some((_, mine)) if mine == theirs => self.held += 1,
            Some((_, mine)) => {
                if self.diverged.is_none() {
                    self.diverged = Some(Diverged { at, mine: *mine, theirs: *theirs, seen });
                }
            }
            None => self.unheld += 1,
        }
    }

    /// The newest checkpoint, and its digest.
    pub fn latest(&self) -> Option<(u64, [u8; 32])> {
        self.recent.last().copied()
    }

    /// This member's digest of checkpoint `at`, where it still has it.
    pub fn digest_at(&self, at: u64) -> Option<[u8; 32]> {
        self.recent.iter().find(|(i, _)| *i == at).map(|(_, d)| *d)
    }

    pub fn diverged(&self) -> Option<&Diverged> {
        self.diverged.as_ref()
    }
}
