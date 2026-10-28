//! What a mounted ext4 holds of its metadata in memory: the blocks the
//! journal has committed and no checkpoint has written home yet -- the only
//! current copy of each, until one does -- and a few clean blocks, as their
//! homes hold them, read lately.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

/// The clean blocks kept: a directory's, its inode table's, a bitmap.
const CLEAN: usize = 32;

pub(super) struct Cache {
    pinned: BTreeMap<u64, Vec<u8>>,
    clean: VecDeque<(u64, Vec<u8>)>,
}

impl Cache {
    pub fn new() -> Cache {
        Cache { pinned: BTreeMap::new(), clean: VecDeque::new() }
    }

    /// Block `b`'s newest copy, if one is held: a pinned one is newer than
    /// its home.
    pub fn get(&mut self, b: u64) -> Option<&[u8]> {
        if self.pinned.contains_key(&b) {
            return self.pinned.get(&b).map(Vec::as_slice);
        }
        let at = self.clean.iter().position(|(k, _)| *k == b)?;
        let entry = self.clean.remove(at)?;
        self.clean.push_back(entry);
        self.clean.back().map(|(_, d)| d.as_slice())
    }

    /// A block as its home holds it, just read.
    pub fn insert_clean(&mut self, b: u64, data: &[u8]) {
        if self.pinned.contains_key(&b) {
            return;
        }
        self.forget(b);
        if self.clean.len() >= CLEAN {
            self.clean.pop_front();
        }
        self.clean.push_back((b, data.to_vec()));
    }

    /// Whatever clean copy of `b` is held, dropped: its home is about to
    /// change, or to mean nothing.
    pub fn forget(&mut self, b: u64) {
        self.clean.retain(|(k, _)| *k != b);
    }

    /// A transaction's copy of `b`, committed: the current one, until a
    /// checkpoint writes it home.
    pub fn pin(&mut self, b: u64, data: Vec<u8>) {
        self.forget(b);
        self.pinned.insert(b, data);
    }

    /// `b` freed: its committed copy is nobody's, and no checkpoint is to
    /// write it over whatever the block becomes.
    pub fn unpin(&mut self, b: u64) {
        self.pinned.remove(&b);
        self.forget(b);
    }

    pub fn pinned(&self) -> impl Iterator<Item = (&u64, &Vec<u8>)> {
        self.pinned.iter()
    }

    pub fn pinned_len(&self) -> usize {
        self.pinned.len()
    }

    /// Everything pinned is home now: what is pinned becomes clean, as much
    /// of it as the clean blocks keep.
    pub fn settle(&mut self) {
        let pinned = core::mem::take(&mut self.pinned);
        for (b, data) in pinned.into_iter().rev().take(CLEAN) {
            self.clean.retain(|(k, _)| *k != b);
            if self.clean.len() >= CLEAN {
                self.clean.pop_front();
            }
            self.clean.push_back((b, data));
        }
    }

    pub fn clear(&mut self) {
        self.pinned.clear();
        self.clean.clear();
    }
}
