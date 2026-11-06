//! The log as a node sees it: what is durable (the caller's [`Storage`]),
//! what is not yet (the unstable tail, and a snapshot taken in but not yet
//! written), and how far it is committed and applied.

use alloc::vec::Vec;
use core::cell::Cell;
use core::cmp;

use crate::storage::{Storage, StorageError};
use crate::types::{Entry, Snapshot};

/// The entries and the snapshot a `Ready` has to hand the caller to write.
#[derive(Debug, Default)]
struct Unstable {
    snapshot: Option<Snapshot>,
    entries: Vec<Entry>,
    /// The index of `entries[0]`: the first entry not yet durable.
    offset: u64,
}

impl Unstable {
    fn first_index(&self) -> Option<u64> {
        self.snapshot.as_ref().map(|s| s.meta.index + 1)
    }

    fn last_index(&self) -> Option<u64> {
        if !self.entries.is_empty() {
            return Some(self.offset + self.entries.len() as u64 - 1);
        }
        self.snapshot.as_ref().map(|s| s.meta.index)
    }

    fn term(&self, index: u64) -> Option<u64> {
        if index < self.offset {
            return match &self.snapshot {
                Some(s) if s.meta.index == index => Some(s.meta.term),
                _ => None,
            };
        }
        let at = usize::try_from(index - self.offset).ok()?;
        self.entries.get(at).map(|e| e.term)
    }

    /// `ents`, one after another, put in place: after what is here, or over
    /// a tail of it, or over all of it.
    fn truncate_and_append(&mut self, ents: &[Entry]) {
        let Some(first) = ents.first() else { return };
        let after = first.index;
        if after == self.offset + self.entries.len() as u64 {
            self.entries.extend_from_slice(ents);
        } else if after <= self.offset {
            self.offset = after;
            self.entries.clear();
            self.entries.extend_from_slice(ents);
        } else {
            let keep = usize::try_from(after - self.offset).unwrap_or(usize::MAX).min(self.entries.len());
            self.entries.truncate(keep);
            self.entries.extend_from_slice(ents);
        }
    }

    /// The entries up to `index`, of `term`, written: no longer unstable --
    /// unless what is here at `index` now is another term's, written over
    /// since they were handed out.
    fn stable_to(&mut self, index: u64, term: u64) {
        if index < self.offset || self.term(index) != Some(term) {
            return;
        }
        let n = usize::try_from(index - self.offset + 1).unwrap_or(usize::MAX).min(self.entries.len());
        self.entries.drain(..n);
        self.offset = index + 1;
    }

    fn stable_snap_to(&mut self, index: u64) {
        if self.snapshot.as_ref().is_some_and(|s| s.meta.index == index) {
            self.snapshot = None;
        }
    }

    fn restore(&mut self, snap: Snapshot) {
        self.offset = snap.meta.index + 1;
        self.entries.clear();
        self.snapshot = Some(snap);
    }
}

pub(crate) struct RaftLog<S: Storage> {
    pub storage: S,
    unstable: Unstable,
    /// The highest entry known to be on a majority's disks.
    pub committed: u64,
    /// The highest entry the caller has been handed to apply.
    pub applied: u64,
    /// The first read of the log the storage failed: nothing read after it
    /// is believed, and the node stops (`RawNode::fault`). A `Cell`, since
    /// the reads that find it take the log by reference.
    fault: Cell<Option<StorageError>>,
}

impl<S: Storage> RaftLog<S> {
    pub fn new(storage: S, commit: u64) -> Result<RaftLog<S>, StorageError> {
        let first = storage.first_index();
        let last = storage.last_index();
        /* What a storage says is held to what a log can be, as a message is
         * (`node::MAX_TERM_OR_INDEX`): nothing done with it then overflows. */
        if first == 0 || first > crate::node::MAX_TERM_OR_INDEX || last >= crate::node::MAX_TERM_OR_INDEX
            || last + 1 < first || commit > last
        {
            return Err(StorageError::Corrupt);
        }
        /* The caller's state is the snapshot's: it applies from there on. */
        let applied = first - 1;
        Ok(RaftLog {
            storage,
            unstable: Unstable { snapshot: None, entries: Vec::new(), offset: last + 1 },
            committed: cmp::max(commit, applied),
            applied,
            fault: Cell::new(None),
        })
    }

    /// The first read the storage failed, if one has.
    pub fn fault(&self) -> Option<StorageError> {
        self.fault.get()
    }

    /// A read of what the log holds failed: kept, the first one.
    fn failed(&self, e: StorageError) {
        if self.fault.get().is_none() {
            self.fault.set(Some(e));
        }
    }

    /// The newest snapshot, to send: None while the storage has none yet
    /// to give (`Unavailable`, asked again later); any other error is a
    /// failed read.
    pub fn snapshot(&self) -> Option<Snapshot> {
        match self.storage.snapshot() {
            Ok(s) => Some(s),
            Err(StorageError::Unavailable) => None,
            Err(e) => {
                self.failed(e);
                None
            }
        }
    }

    pub fn first_index(&self) -> u64 {
        self.unstable.first_index().unwrap_or_else(|| self.storage.first_index())
    }

    pub fn last_index(&self) -> u64 {
        self.unstable.last_index().unwrap_or_else(|| self.storage.last_index())
    }

    /// The term of the entry at `index`, the snapshot's own index included;
    /// None outside the log -- and None where the storage fails to read
    /// one it holds, which is kept as the log's fault: whatever is decided
    /// on it is never handed out.
    pub fn term(&self, index: u64) -> Option<u64> {
        let dummy = self.first_index() - 1;
        if index < dummy || index > self.last_index() {
            return None;
        }
        if let Some(t) = self.unstable.term(index) {
            return Some(t);
        }
        match self.storage.term(index) {
            Ok(t) => Some(t),
            Err(e) => {
                self.failed(e);
                None
            }
        }
    }

    pub fn last_term(&self) -> u64 {
        self.term(self.last_index()).unwrap_or(0)
    }

    pub fn match_term(&self, index: u64, term: u64) -> bool {
        self.term(index) == Some(term)
    }

    /// Whether a log ending at `last_index`, of `last_term`, is at least as
    /// new as this one: what a vote is given for (the paper's 5.4.1).
    pub fn is_up_to_date(&self, last_index: u64, last_term: u64) -> bool {
        let my_term = self.last_term();
        last_term > my_term || (last_term == my_term && last_index >= self.last_index())
    }

    /// The first of `ents` whose term is not this log's at its index -- an
    /// entry to write over, or the first one new -- or None when every one
    /// of them is here already.
    pub fn find_conflict(&self, ents: &[Entry]) -> Option<u64> {
        ents.iter().find(|e| !self.match_term(e.index, e.term)).map(|e| e.index)
    }

    /// Searching back from `index`, the first entry whose term is no later
    /// than `term`, and that term: where a leader may look next for an entry
    /// a follower has (the follower's hint, and the leader's use of it).
    pub fn find_conflict_by_term(&self, index: u64, term: u64) -> (u64, u64) {
        let mut at = cmp::min(index, self.last_index());
        loop {
            match self.term(at) {
                Some(t) if t > term && at > 0 => at -= 1,
                Some(t) => return (at, t),
                None => return (at, 0),
            }
        }
    }

    /// Entries `lo..hi`, at most `max_count` and -- but for the first --
    /// `max_bytes` of them. Compacted, or beyond the log, is an error.
    pub fn entries(&self, lo: u64, hi: u64, max_bytes: u64, max_count: usize) -> Result<Vec<Entry>, StorageError> {
        let mut out = Vec::new();
        if lo >= hi {
            return Ok(out);
        }
        if lo < self.first_index() {
            return Err(StorageError::Compacted);
        }
        if hi > self.last_index() + 1 {
            return Err(StorageError::Unavailable);
        }
        let hi = cmp::min(hi, lo.saturating_add(max_count as u64));
        let offset = self.unstable.offset;
        if lo < offset {
            /* From the storage first: what is there below what is unstable --
             * all of it in the log, so a failure is a failed read. */
            if let Err(e) = self.storage.entries(lo, cmp::min(hi, offset), max_bytes, &mut out) {
                self.failed(e);
                return Err(e);
            }
            let got = out.len() as u64;
            if lo + got < cmp::min(hi, offset) {
                return Ok(out);
            }
        }
        if hi > offset {
            let mut bytes: u64 = out.iter().map(Entry::size).sum();
            let from = usize::try_from(cmp::max(lo, offset) - offset).unwrap_or(usize::MAX);
            let to = usize::try_from(hi - offset).unwrap_or(usize::MAX).min(self.unstable.entries.len());
            for e in self.unstable.entries.get(from..to).unwrap_or(&[]) {
                bytes = bytes.saturating_add(e.size());
                if bytes > max_bytes && !out.is_empty() {
                    break;
                }
                out.push(e.clone());
            }
        }
        Ok(out)
    }

    /// `ents` put after the entry at `ents[0].index - 1`, over whatever is
    /// there from that index on.
    pub fn append(&mut self, ents: &[Entry]) {
        self.unstable.truncate_and_append(ents);
    }

    pub fn unstable_entries(&self) -> &[Entry] {
        &self.unstable.entries
    }

    pub fn unstable_snapshot(&self) -> Option<&Snapshot> {
        self.unstable.snapshot.as_ref()
    }

    /// The last entry on the caller's disk and still this log's: entries
    /// handed out to write and not yet written, or written but since written
    /// over, are not. Nothing while a snapshot taken in is not yet written.
    pub fn stable_last_index(&self) -> u64 {
        if self.unstable.snapshot.is_some() {
            return 0;
        }
        cmp::min(self.storage.last_index(), self.unstable.offset - 1)
    }

    pub fn stable_to(&mut self, index: u64, term: u64) {
        self.unstable.stable_to(index, term);
    }

    pub fn stable_snap_to(&mut self, index: u64) {
        self.unstable.stable_snap_to(index);
    }

    /// The commit taken to `to`, which this log has: never back.
    pub fn commit_to(&mut self, to: u64) {
        if to > self.committed {
            self.committed = to;
        }
    }

    pub fn applied_to(&mut self, to: u64) {
        if to > self.applied {
            self.applied = to;
        }
    }

    /// Whether there are committed entries, on the disk, not yet handed out
    /// to apply.
    pub fn has_next_committed(&self) -> bool {
        let hi = cmp::min(self.committed, self.stable_last_index());
        hi > self.applied && self.applied + 1 >= self.first_index()
    }

    /// The committed entries on the disk that come next to apply, at most
    /// `max_bytes` and `max_count` of them but always one when there is one.
    pub fn next_committed(&self, max_bytes: u64, max_count: usize) -> Vec<Entry> {
        if !self.has_next_committed() {
            return Vec::new();
        }
        let hi = cmp::min(self.committed, self.stable_last_index()) + 1;
        self.entries(self.applied + 1, hi, max_bytes, max_count).unwrap_or_default()
    }

    /// The log made a snapshot a leader sent: nothing before it, nothing
    /// after it, committed to it.
    pub fn restore(&mut self, snap: Snapshot) {
        self.committed = snap.meta.index;
        self.unstable.restore(snap);
    }
}
