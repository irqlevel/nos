//! Sent buffers waiting for `GetStatus` to hand them back (docs/spec/connectx3.md
//! 6.2 step 4). Every token `Transmit` was given comes back exactly once, in
//! send order: none is ever dropped (review finding 6aeccbfbe450, #37). The list
//! stays bounded by backpressure instead: `admits` refuses a new send while the
//! frames in flight plus the tokens nobody has collected fill `cap`, and
//! `Transmit` answers `NOT_READY` until the caller calls `GetStatus`. No UEFI
//! here: `eth.rs` keeps one per port; the host tests are `test/sent.rs`.

use alloc::collections::VecDeque;

pub struct Sent {
    done: VecDeque<usize>,
    cap: usize,
}

impl Sent {
    pub fn new(cap: usize) -> Sent {
        Sent { done: VecDeque::new(), cap }
    }

    /// May one more frame be queued, with `in_flight` already on the ring?
    /// Holding to this keeps `in_flight + len` ≤ `cap`, so `push` never has
    /// to make room.
    pub fn admits(&self, in_flight: usize) -> bool {
        in_flight + self.done.len() < self.cap
    }

    /// A frame's send completed: its token waits for `take`.
    pub fn push(&mut self, token: usize) {
        self.done.push_back(token);
    }

    /// The oldest token not yet handed back.
    pub fn take(&mut self) -> Option<usize> {
        self.done.pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.done.is_empty()
    }
}
