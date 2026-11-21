//! What a mounted ext4 holds of its metadata in memory: the blocks staged in
//! the running group and not yet in the journal -- the newest copy of each,
//! what every read sees -- the blocks the journal has committed and no
//! checkpoint has written home yet -- the only current copy of each, until
//! one does -- and a few clean blocks, as their homes hold them, read
//! lately. A block may be both staged and committed: a checkpoint writes
//! home the committed copy, the state the journal holds, and never a staged
//! one, which no commit block yet vouches for.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

/// The clean blocks kept: a directory's, its inode table's, a bitmap.
const CLEAN: usize = 32;

pub(super) struct Cache {
    staged: BTreeMap<u64, Vec<u8>>,
    pinned: BTreeMap<u64, Vec<u8>>,
    clean: VecDeque<(u64, Vec<u8>)>,
}

impl Cache {
    pub fn new() -> Cache {
        Cache { staged: BTreeMap::new(), pinned: BTreeMap::new(), clean: VecDeque::new() }
    }

    /// Block `b`'s newest copy, if one is held: a staged one is newer than a
    /// committed one, which is newer than its home.
    pub fn get(&mut self, b: u64) -> Option<&[u8]> {
        if self.staged.contains_key(&b) {
            return self.staged.get(&b).map(Vec::as_slice);
        }
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
        if self.staged.contains_key(&b) || self.pinned.contains_key(&b) {
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

    /// Whatever clean copies of blocks `start..end` are held, dropped.
    pub fn forget_range(&mut self, start: u64, end: u64) {
        self.clean.retain(|(k, _)| !(start..end).contains(k));
    }

    /// A transaction's copy of `b`, staged: what the reads see from now on,
    /// and what the group's commit writes into the log.
    pub fn stage(&mut self, b: u64, data: Vec<u8>) {
        self.forget(b);
        self.staged.insert(b, data);
    }

    /// Blocks `start..end` freed by a transaction staged: their staged
    /// copies are nobody's, and no commit is to log them. A committed copy
    /// stays until the group that frees them commits: the state the journal
    /// holds still has them, and a checkpoint before then is to write it.
    pub fn unstage_range(&mut self, start: u64, end: u64) {
        let staged: Vec<u64> = self.staged.range(start..end).map(|(&b, _)| b).collect();
        for b in staged {
            self.staged.remove(&b);
        }
        self.clean.retain(|(k, _)| !(start..end).contains(k));
    }

    pub fn is_staged(&self, b: u64) -> bool {
        self.staged.contains_key(&b)
    }

    pub fn staged(&self) -> impl Iterator<Item = (&u64, &Vec<u8>)> {
        self.staged.iter()
    }

    pub fn staged_len(&self) -> usize {
        self.staged.len()
    }

    /// The group committed: every staged copy the current one now, until a
    /// checkpoint writes it home -- the blocks, in order, for the journal's
    /// record of where each block's newest copy is.
    pub fn commit_staged(&mut self) -> Vec<u64> {
        let staged = core::mem::take(&mut self.staged);
        let mut blocks = Vec::with_capacity(staged.len());
        for (b, data) in staged {
            blocks.push(b);
            self.pinned.insert(b, data);
        }
        blocks
    }

    /// A transaction's copy of `b`, committed: the current one, until a
    /// checkpoint writes it home.
    pub fn pin(&mut self, b: u64, data: Vec<u8>) {
        self.forget(b);
        self.staged.remove(&b);
        self.pinned.insert(b, data);
    }

    /// Blocks `start..end` freed, and the freeing committed: their committed
    /// copies are nobody's, and no checkpoint is to write them over whatever
    /// the blocks become.
    pub fn unpin_range(&mut self, start: u64, end: u64) {
        let pinned: Vec<u64> = self.pinned.range(start..end).map(|(&b, _)| b).collect();
        for b in pinned {
            self.pinned.remove(&b);
        }
        self.clean.retain(|(k, _)| !(start..end).contains(k));
    }

    pub fn pinned(&self) -> impl Iterator<Item = (&u64, &Vec<u8>)> {
        self.pinned.iter()
    }

    pub fn pinned_len(&self) -> usize {
        self.pinned.len()
    }

    /// Everything committed is home now: what is committed becomes clean,
    /// as much of it as the clean blocks keep -- a block staged since keeps
    /// its staged copy, which the reads see first.
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
        self.staged.clear();
        self.pinned.clear();
        self.clean.clear();
    }
}
