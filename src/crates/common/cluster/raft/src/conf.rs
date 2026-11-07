//! A group's membership as a node holds it: who votes, who learns, and what
//! a majority is.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::types::{ChangeKind, ConfChange, ConfState, NodeId, NONE};

/// The most members a group has, voters and learners together: what a
/// message's membership carries at most, and what keeps the leader's
/// bookkeeping small.
pub const MAX_MEMBERS: usize = 64;

/// Why a membership change is refused by the membership itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeError {
    /// A voter made a voter again, a learner a learner.
    AlreadyMember,
    /// Removed, and not there.
    NotMember,
    /// The last voter taken away: a group of none.
    LastVoter,
    /// Past [`MAX_MEMBERS`].
    TooManyMembers,
    /// Member 0, which is nobody.
    Nobody,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Membership {
    voters: Vec<NodeId>,
    learners: Vec<NodeId>,
}

impl Membership {
    /// The membership a `ConfState` says, which [`valid`] has held to its
    /// rules -- sorted, disjoint, none of them 0, not too many.
    pub fn from_state(state: &ConfState) -> Membership {
        Membership { voters: state.voters.clone(), learners: state.learners.clone() }
    }

    pub fn to_state(&self) -> ConfState {
        ConfState { voters: self.voters.clone(), learners: self.learners.clone() }
    }

    pub fn voters(&self) -> &[NodeId] {
        &self.voters
    }

    pub fn is_voter(&self, id: NodeId) -> bool {
        self.voters.binary_search(&id).is_ok()
    }

    pub fn is_learner(&self, id: NodeId) -> bool {
        self.learners.binary_search(&id).is_ok()
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.is_voter(id) || self.is_learner(id)
    }

    /// Every member, voters first.
    pub fn members(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.voters.iter().chain(self.learners.iter()).copied()
    }

    /// The votes, or the replicas, a majority of the voters is.
    pub fn quorum(&self) -> usize {
        self.voters.len() / 2 + 1
    }

    /// The membership with `cc` made: or why not.
    pub fn changed(&self, cc: ConfChange) -> Result<Membership, ChangeError> {
        if cc.node == NONE {
            return Err(ChangeError::Nobody);
        }
        let mut next = self.clone();
        match cc.kind {
            ChangeKind::AddVoter => {
                if self.is_voter(cc.node) {
                    return Err(ChangeError::AlreadyMember);
                }
                remove(&mut next.learners, cc.node);
                insert(&mut next.voters, cc.node);
            }
            ChangeKind::AddLearner => {
                if self.is_learner(cc.node) {
                    return Err(ChangeError::AlreadyMember);
                }
                remove(&mut next.voters, cc.node);
                insert(&mut next.learners, cc.node);
            }
            ChangeKind::Remove => {
                if !self.contains(cc.node) {
                    return Err(ChangeError::NotMember);
                }
                remove(&mut next.voters, cc.node);
                remove(&mut next.learners, cc.node);
            }
        }
        if next.voters.is_empty() {
            return Err(ChangeError::LastVoter);
        }
        if next.voters.len() + next.learners.len() > MAX_MEMBERS {
            return Err(ChangeError::TooManyMembers);
        }
        Ok(next)
    }
}

/// Whether a `ConfState` keeps the rules a membership has: each list sorted
/// and without repeats, no member in both, none of them nobody, not more
/// than [`MAX_MEMBERS`] in all. An empty one is a node that knows of no group
/// yet -- a new member, waiting to be sent one.
pub fn valid(state: &ConfState) -> bool {
    let strictly_sorted = |v: &[NodeId]| v.windows(2).all(|w| w[0] < w[1]);
    strictly_sorted(&state.voters)
        && strictly_sorted(&state.learners)
        && state.voters.len() + state.learners.len() <= MAX_MEMBERS
        && !state.voters.contains(&NONE)
        && !state.learners.contains(&NONE)
        && state.learners.iter().all(|l| state.voters.binary_search(l).is_err())
}

fn insert(list: &mut Vec<NodeId>, id: NodeId) {
    if let Err(at) = list.binary_search(&id) {
        list.insert(at, id);
    }
}

fn remove(list: &mut Vec<NodeId>, id: NodeId) {
    if let Ok(at) = list.binary_search(&id) {
        list.remove(at);
    }
}

/// Changes one piece of a [`Changes`] holds: some 56 KiB of them.
const CHANGES_CHUNK: usize = 1024;

/// The membership each change in the log made, by the change's index,
/// oldest first: kept in pieces of [`CHANGES_CHUNK`], so that no allocation
/// of it grows with the log -- a group that changes its membership often
/// and takes a snapshot seldom has as many of them as it likes.
#[derive(Debug, Default)]
pub struct Changes {
    chunks: VecDeque<VecDeque<(u64, Membership)>>,
}

impl Changes {
    /// After every change held: `index` is past theirs.
    pub fn push(&mut self, index: u64, m: Membership) {
        match self.chunks.back_mut() {
            Some(last) if last.len() < CHANGES_CHUNK => last.push_back((index, m)),
            _ => {
                let mut chunk = VecDeque::new();
                chunk.push_back((index, m));
                self.chunks.push_back(chunk);
            }
        }
    }

    pub fn first(&self) -> Option<&(u64, Membership)> {
        self.chunks.front().and_then(VecDeque::front)
    }

    pub fn last(&self) -> Option<&(u64, Membership)> {
        self.chunks.back().and_then(VecDeque::back)
    }

    pub fn pop_front(&mut self) -> Option<(u64, Membership)> {
        let chunk = self.chunks.front_mut()?;
        let out = chunk.pop_front();
        if chunk.is_empty() {
            self.chunks.pop_front();
        }
        out
    }

    pub fn pop_back(&mut self) -> Option<(u64, Membership)> {
        let chunk = self.chunks.back_mut()?;
        let out = chunk.pop_back();
        if chunk.is_empty() {
            self.chunks.pop_back();
        }
        out
    }

    /// The membership the last change at or before `index` made: None
    /// where no change held comes that early.
    pub fn at(&self, index: u64) -> Option<&Membership> {
        let chunk = self.chunks.iter().rev().find(|c| c.front().is_some_and(|(i, _)| *i <= index))?;
        chunk.iter().rev().find(|(i, _)| *i <= index).map(|(_, m)| m)
    }

    pub fn clear(&mut self) {
        self.chunks.clear();
    }
}
