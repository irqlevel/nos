//! The storage in memory against a model of its log: appends over its end
//! and over what it has, snapshots taken in the middle of logs of tens of
//! thousands of entries and snapshots sent -- every term and every range of
//! entries what the model says, and nothing allocated past a block of the
//! kernel's heap however long the log (`heap`, which `guarded` holds every
//! run to).

use std::collections::BTreeMap;

use raft::{ConfState, Entry, EntryKind, MemStorage, Snapshot, SnapshotData, SnapshotMeta, Storage, StorageError};

use crate::rng::Rng;

/// The most entries one append writes.
const MAX_APPEND: u64 = 2500;

struct Model {
    /// The snapshot's index and term.
    snap: (u64, u64),
    /// Every entry after it: its term and its one byte.
    log: BTreeMap<u64, (u64, u8)>,
}

impl Model {
    fn last(&self) -> u64 {
        self.log.keys().next_back().copied().unwrap_or(self.snap.0)
    }

    fn term(&self, i: u64) -> Option<u64> {
        if i == self.snap.0 {
            return Some(self.snap.1);
        }
        self.log.get(&i).map(|e| e.0)
    }
}

fn conf() -> ConfState {
    ConfState { voters: vec![1], learners: vec![] }
}

/// `iterations` operations from `seed`.
pub fn run(seed: u64, iterations: u64) -> Result<(), String> {
    let mut r = Rng::new(seed ^ 0x570E);
    let mut s = MemStorage::new();
    let mut m = Model { snap: (0, 0), log: BTreeMap::new() };
    for i in 0..iterations {
        let last = m.last();
        match r.below(40) {
            0..=29 => {
                /* Mostly at the end, so that the log grows long. */
                let first = m.snap.0 + 1;
                let start = if r.chance(70) { last + 1 } else { r.range(first, last + 1) };
                let n = r.range(1, MAX_APPEND);
                let mut term = m.term(start - 1).unwrap_or(0) + r.below(2);
                let ents: Vec<Entry> = (0..n)
                    .map(|k| {
                        if r.chance(1) {
                            term += 1;
                        }
                        Entry { term, index: start + k, kind: EntryKind::Normal, data: vec![(start + k) as u8 ^ term as u8] }
                    })
                    .collect();
                s.append(&ents).map_err(|e| format!("iteration {}: append at {}: {:?}", i, start, e))?;
                m.log.split_off(&start);
                for e in &ents {
                    m.log.insert(e.index, (e.term, e.data[0]));
                }
            }
            30..=37 if last > m.snap.0 => {
                let at = r.range(m.snap.0 + 1, last);
                s.create_snapshot(at, conf(), SnapshotData::default()).map_err(|e| format!("iteration {}: snapshot at {}: {:?}", i, at, e))?;
                let term = m.term(at).ok_or("a model with no term")?;
                m.log = m.log.split_off(&(at + 1));
                m.snap = (at, term);
            }
            _ => {
                let at = m.snap.0 + r.range(1, 3000);
                let term = m.snap.1 + r.below(3);
                let snap = Snapshot { meta: SnapshotMeta { index: at, term, conf: conf() }, data: SnapshotData::default() };
                s.apply_snapshot(snap).map_err(|e| format!("iteration {}: snapshot sent at {}: {:?}", i, at, e))?;
                m.log.clear();
                m.snap = (at, term);
            }
        }
        check(&s, &m, &mut r).map_err(|e| format!("iteration {}: {}", i, e))?;
    }
    Ok(())
}

fn check(s: &MemStorage, m: &Model, r: &mut Rng) -> Result<(), String> {
    let last = m.last();
    if s.first_index() != m.snap.0 + 1 || s.last_index() != last {
        return Err(format!("log {}..={}, the model's {}..={}", s.first_index(), s.last_index(), m.snap.0 + 1, last));
    }
    if m.snap.0 > 0 && s.term(m.snap.0 - 1) != Err(StorageError::Compacted) {
        return Err("a term before the snapshot".into());
    }
    if s.term(last + 1) != Err(StorageError::Unavailable) {
        return Err("a term past the end".into());
    }
    for _ in 0..16 {
        let i = r.range(m.snap.0, last);
        if s.term(i).ok() != m.term(i) {
            return Err(format!("term at {}: {:?}, the model's {:?}", i, s.term(i), m.term(i)));
        }
    }
    if last > m.snap.0 {
        let lo = r.range(m.snap.0 + 1, last);
        let hi = r.range(lo, (lo + 300).min(last)) + 1;
        let mut out = Vec::new();
        s.entries(lo, hi, r.range(0, 64), &mut out).map_err(|e| format!("entries {}..{}: {:?}", lo, hi, e))?;
        if out.is_empty() || out.len() as u64 > hi - lo {
            return Err(format!("entries {}..{}: {} of them", lo, hi, out.len()));
        }
        for (k, e) in out.iter().enumerate() {
            if e.index != lo + k as u64 || m.log.get(&e.index) != Some(&(e.term, e.data[0])) {
                return Err(format!("entry {}: {:?}, the model's {:?}", e.index, e, m.log.get(&e.index)));
            }
        }
    }
    Ok(())
}
