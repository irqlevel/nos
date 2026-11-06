//! What a node keeps durable, as it reads it: the hard state, the log after
//! the newest snapshot, and that snapshot. The caller writes it -- what a
//! `Ready` hands it -- before it says anything; the node only reads it.
//!
//! [`MemStorage`] holds it all in memory, in pieces: for tests, for a
//! simulation's members (whose crash keeps exactly what was written), and
//! for a group small enough to keep its log so.

use alloc::vec::Vec;

use crate::types::{
    ChangeKind, ConfChange, ConfState, Entry, EntryKind, HardState, NodeId, Snapshot, SnapshotData, SnapshotMeta,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    /// Before the newest snapshot: only the snapshot has it now.
    Compacted,
    /// After the last entry.
    Unavailable,
    /// A snapshot no newer than the one there.
    SnapshotOutOfDate,
    /// The storage is not a log: it says what cannot be.
    Corrupt,
}

/// What a node starts from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RaftState {
    pub hard_state: HardState,
    /// The membership as of the newest snapshot; the changes in the log after
    /// it the node reads itself.
    pub conf: ConfState,
}

pub trait Storage {
    fn initial_state(&self) -> Result<RaftState, StorageError>;
    /// Entries `lo..hi` into `out`, stopping once they come to more than
    /// `max_bytes` -- but always at least one when `lo < hi`.
    fn entries(&self, lo: u64, hi: u64, max_bytes: u64, out: &mut Vec<Entry>) -> Result<(), StorageError>;
    /// The term of the entry at `index`: the newest snapshot's for its own
    /// index.
    fn term(&self, index: u64) -> Result<u64, StorageError>;
    /// The first entry the log has: the newest snapshot's index and one.
    fn first_index(&self) -> u64;
    /// The last entry the log has -- the snapshot's index when it has none.
    fn last_index(&self) -> u64;
    /// The newest snapshot.
    fn snapshot(&self) -> Result<Snapshot, StorageError>;
}

/// Entries a piece of the log holds: 1024, some 48 KiB of them besides their
/// data -- so that no allocation of the log's is larger, however long it is.
const CHUNK: usize = 1024;

/// The term a group's first entries are of: before any election.
pub const BOOTSTRAP_TERM: u64 = 1;

/// The entries a new group's log starts with, the same on each of its
/// first members: a membership change for each voter, then for each
/// learner, of term 1 -- committed, since every first member has them.
/// What a storage of the caller's own writes, as `MemStorage::bootstrap`
/// does, with a hard state of term 1 committed to the last of them.
pub fn bootstrap_entries(voters: &[NodeId], learners: &[NodeId]) -> Vec<Entry> {
    let mut voters = voters.to_vec();
    voters.sort_unstable();
    voters.dedup();
    let mut learners: Vec<NodeId> = learners.iter().copied().filter(|l| voters.binary_search(l).is_err()).collect();
    learners.sort_unstable();
    learners.dedup();
    let changes = voters
        .iter()
        .map(|&node| ConfChange { kind: ChangeKind::AddVoter, node })
        .chain(learners.iter().map(|&node| ConfChange { kind: ChangeKind::AddLearner, node }));
    changes
        .enumerate()
        .map(|(i, cc)| Entry {
            term: BOOTSTRAP_TERM,
            index: i as u64 + 1,
            kind: EntryKind::Change,
            data: crate::wire::encode_change(&cc),
        })
        .collect()
}

/// A storage in memory.
#[derive(Clone, Debug, Default)]
pub struct MemStorage {
    hard: HardState,
    snapshot: Snapshot,
    /// The entries after the snapshot, in order, `CHUNK` a piece, every piece
    /// full but the last -- the first `skip` places of the first piece held
    /// by entries a snapshot has taken since, their data given back: so
    /// that a snapshot drops whole pieces and moves nothing.
    chunks: Vec<Vec<Entry>>,
    skip: usize,
    count: u64,
}

impl MemStorage {
    /// Nothing at all: a node that is to be sent its group.
    pub fn new() -> MemStorage {
        MemStorage::default()
    }

    /// A new group's first members, before anything has happened: each
    /// member's log starts with the changes that made the group
    /// ([`bootstrap_entries`]), committed -- so that a member added later,
    /// sent the log from its first entry, makes the same membership of it.
    pub fn bootstrap(voters: &[NodeId], learners: &[NodeId]) -> MemStorage {
        let entries = bootstrap_entries(voters, learners);
        let mut s = MemStorage::new();
        let last = entries.last().map_or(0, |e| e.index);
        for e in entries {
            s.push(e);
        }
        s.hard = HardState { term: BOOTSTRAP_TERM, vote: crate::types::NONE, commit: last };
        s
    }

    pub fn hard_state(&self) -> HardState {
        self.hard
    }

    pub fn set_hard_state(&mut self, hs: HardState) {
        self.hard = hs;
    }

    pub fn snapshot_meta(&self) -> &SnapshotMeta {
        &self.snapshot.meta
    }

    fn get(&self, index: u64) -> Option<&Entry> {
        let first = self.first_index();
        if index < first || index > self.last_index() {
            return None;
        }
        let at = usize::try_from(index - first).ok()?.checked_add(self.skip)?;
        self.chunks.get(at / CHUNK)?.get(at % CHUNK)
    }

    /// Every entry after the snapshot dropped: what a log read back does
    /// with an entry below its snapshot written after entries past it --
    /// which replaced them when it was written (`raft::log`).
    pub(crate) fn drop_entries(&mut self) {
        self.truncate(self.first_index());
    }

    /// Keeps the entries before `index`, and drops the rest.
    fn truncate(&mut self, index: u64) {
        let first = self.first_index();
        let keep = index.saturating_sub(first).min(self.count);
        let Some(places) = usize::try_from(keep).ok().and_then(|k| k.checked_add(self.skip)) else { return };
        let chunks = places.div_ceil(CHUNK);
        self.chunks.truncate(chunks);
        if let Some(last) = self.chunks.last_mut() {
            last.truncate(places - (chunks - 1) * CHUNK);
        }
        self.count = keep;
    }

    fn push(&mut self, e: Entry) {
        match self.chunks.last_mut() {
            Some(last) if last.len() < CHUNK => last.push(e),
            _ => {
                let mut chunk = Vec::with_capacity(CHUNK);
                chunk.push(e);
                self.chunks.push(chunk);
            }
        }
        self.count += 1;
    }

    /// Entries as a `Ready` hands them: in order, one after another, the
    /// first of them at most the last there is and one. What they replace is
    /// dropped; what comes before the snapshot is skipped.
    pub fn append(&mut self, entries: &[Entry]) -> Result<(), StorageError> {
        let Some(first_new) = entries.first() else { return Ok(()) };
        if entries.windows(2).any(|w| w[1].index != w[0].index + 1) || first_new.index > self.last_index() + 1 {
            return Err(StorageError::Corrupt);
        }
        let first = self.first_index();
        let skip = usize::try_from(first.saturating_sub(first_new.index)).map_err(|_| StorageError::Corrupt)?;
        let Some(rest) = entries.get(skip..) else { return Ok(()) };
        let Some(head) = rest.first() else { return Ok(()) };
        self.truncate(head.index);
        for e in rest {
            self.push(e.clone());
        }
        Ok(())
    }

    /// A snapshot a leader sent, made the whole of the log: what was before
    /// it is gone, and so is everything after.
    pub fn apply_snapshot(&mut self, snap: Snapshot) -> Result<(), StorageError> {
        if snap.meta.index <= self.snapshot.meta.index {
            return Err(StorageError::SnapshotOutOfDate);
        }
        self.snapshot = snap;
        self.chunks.clear();
        self.skip = 0;
        self.count = 0;
        if self.hard.commit < self.snapshot.meta.index {
            self.hard.commit = self.snapshot.meta.index;
        }
        Ok(())
    }

    /// The caller's state as of entry `index` made the newest snapshot, its
    /// membership `conf` as of that entry, and the entries up to it dropped.
    pub fn create_snapshot(&mut self, index: u64, conf: ConfState, data: SnapshotData) -> Result<(), StorageError> {
        if index <= self.snapshot.meta.index {
            return Err(StorageError::SnapshotOutOfDate);
        }
        if index > self.last_index() {
            return Err(StorageError::Unavailable);
        }
        let term = self.term(index)?;
        /* The entries up to `index` gone: whole pieces dropped, the rest of
         * them skipped in the first piece left and their data given back --
         * nothing after them moved, so nothing is allocated however long
         * the log goes on past the snapshot. */
        let gone = index - self.snapshot.meta.index;
        let Some(places) = usize::try_from(gone).ok().and_then(|g| g.checked_add(self.skip)) else {
            return Err(StorageError::Corrupt);
        };
        let whole = (places / CHUNK).min(self.chunks.len());
        self.chunks.drain(..whole);
        self.skip = places - whole * CHUNK;
        if let Some(first) = self.chunks.first_mut() {
            for e in first.iter_mut().take(self.skip) {
                e.data = Vec::new();
            }
        }
        self.count -= gone;
        self.snapshot = Snapshot { meta: SnapshotMeta { index, term, conf }, data };
        Ok(())
    }
}

impl Storage for MemStorage {
    fn initial_state(&self) -> Result<RaftState, StorageError> {
        Ok(RaftState { hard_state: self.hard, conf: self.snapshot.meta.conf.clone() })
    }

    fn entries(&self, lo: u64, hi: u64, max_bytes: u64, out: &mut Vec<Entry>) -> Result<(), StorageError> {
        if lo < self.first_index() {
            return Err(StorageError::Compacted);
        }
        if hi > self.last_index() + 1 {
            return Err(StorageError::Unavailable);
        }
        let mut bytes = 0u64;
        for i in lo..hi {
            let Some(e) = self.get(i) else { return Err(StorageError::Corrupt) };
            bytes = bytes.saturating_add(e.size());
            if bytes > max_bytes && i > lo {
                break;
            }
            out.push(e.clone());
        }
        Ok(())
    }

    fn term(&self, index: u64) -> Result<u64, StorageError> {
        if index == self.snapshot.meta.index {
            return Ok(self.snapshot.meta.term);
        }
        if index < self.snapshot.meta.index {
            return Err(StorageError::Compacted);
        }
        self.get(index).map(|e| e.term).ok_or(StorageError::Unavailable)
    }

    fn first_index(&self) -> u64 {
        self.snapshot.meta.index + 1
    }

    fn last_index(&self) -> u64 {
        self.snapshot.meta.index + self.count
    }

    fn snapshot(&self) -> Result<Snapshot, StorageError> {
        Ok(self.snapshot.clone())
    }
}
