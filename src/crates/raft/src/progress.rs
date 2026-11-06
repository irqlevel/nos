//! A leader's view of each member it replicates to: how much of the log
//! it knows the member has, what to send it next, and how much is on its way.

use alloc::collections::VecDeque;

use crate::types::Snapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Where the member's log stops is not known: one append at a time,
    /// each answered before the next.
    Probe,
    /// The member keeps up: appends one after another, as many as the
    /// flow control lets out.
    Replicate,
    /// The member is behind what the log still has: a snapshot is on its
    /// way, a piece at a time.
    Snapshot,
}

/// The appends on their way to a member and not yet answered: the last
/// index and the bytes of each.
#[derive(Clone, Debug)]
pub struct Inflights {
    ring: VecDeque<(u64, u64)>,
    bytes: u64,
    max_count: usize,
    max_bytes: u64,
}

impl Inflights {
    pub fn new(max_count: usize, max_bytes: u64) -> Inflights {
        Inflights { ring: VecDeque::new(), bytes: 0, max_count, max_bytes }
    }

    pub fn full(&self) -> bool {
        self.ring.len() >= self.max_count || self.bytes >= self.max_bytes
    }

    pub fn add(&mut self, last: u64, bytes: u64) {
        self.ring.push_back((last, bytes));
        self.bytes = self.bytes.saturating_add(bytes);
    }

    /// Everything up to `index` answered.
    pub fn free_le(&mut self, index: u64) {
        while let Some(&(last, bytes)) = self.ring.front() {
            if last > index {
                break;
            }
            self.ring.pop_front();
            self.bytes = self.bytes.saturating_sub(bytes);
        }
    }

    /// The oldest append out given up as lost: room for one more.
    pub fn free_first(&mut self) {
        if let Some((_, bytes)) = self.ring.pop_front() {
            self.bytes = self.bytes.saturating_sub(bytes);
        }
    }

    pub fn reset(&mut self) {
        self.ring.clear();
        self.bytes = 0;
    }
}

/// A snapshot on its way to a member: which one, and how much of it the
/// member has said it has.
#[derive(Clone, Debug)]
pub struct Sending {
    pub snapshot: Snapshot,
    pub acked: u64,
}

#[derive(Clone, Debug)]
pub struct Progress {
    /// The last entry the member is known to have, as this log has it.
    pub matched: u64,
    /// The next entry to send it.
    pub next: u64,
    pub state: State,
    /// In `Probe`: an append is out and not answered -- until it is, or a
    /// heartbeat comes back.
    pub probe_sent: bool,
    pub inflights: Inflights,
    /// Heard from since CheckQuorum last looked.
    pub recent_active: bool,
    pub sending: Option<Sending>,
}

impl Progress {
    pub fn new(next: u64, max_inflight: usize, max_inflight_bytes: u64) -> Progress {
        Progress {
            matched: 0,
            next,
            state: State::Probe,
            probe_sent: false,
            inflights: Inflights::new(max_inflight, max_inflight_bytes),
            recent_active: false,
            sending: None,
        }
    }

    pub fn is_paused(&self) -> bool {
        match self.state {
            State::Probe => self.probe_sent,
            State::Replicate => self.inflights.full(),
            State::Snapshot => true,
        }
    }

    fn reset_state(&mut self, state: State) {
        self.probe_sent = false;
        self.state = state;
        self.inflights.reset();
        self.sending = None;
    }

    pub fn become_probe(&mut self) {
        /* Back from a snapshot: from what it now has, or the snapshot's end. */
        let after = match &self.sending {
            Some(s) if self.state == State::Snapshot => core::cmp::max(self.matched, s.snapshot.meta.index) + 1,
            _ => self.matched + 1,
        };
        self.reset_state(State::Probe);
        self.next = after;
    }

    pub fn become_replicate(&mut self) {
        self.reset_state(State::Replicate);
        self.next = self.matched + 1;
    }

    pub fn become_snapshot(&mut self, snapshot: Snapshot) {
        self.reset_state(State::Snapshot);
        self.sending = Some(Sending { snapshot, acked: 0 });
    }

    /// The member has everything up to `n`: whether that is news.
    pub fn maybe_update(&mut self, n: u64) -> bool {
        let updated = self.matched < n;
        if updated {
            self.matched = n;
            self.probe_sent = false;
        }
        if self.next < n + 1 {
            self.next = n + 1;
        }
        updated
    }

    /// An append whose `prev_index` was `rejected` refused, with the hint
    /// that the member's log has nothing past `hint` that matches: whether
    /// `next` moved back -- an answer to an append long overtaken does not.
    pub fn maybe_decr_to(&mut self, rejected: u64, hint: u64) -> bool {
        if self.state == State::Replicate {
            if rejected <= self.matched {
                return false;
            }
            self.next = self.matched + 1;
            return true;
        }
        if self.next == 0 || self.next - 1 != rejected {
            return false;
        }
        self.next = core::cmp::max(core::cmp::min(rejected, hint + 1), self.matched + 1);
        self.probe_sent = false;
        true
    }
}
