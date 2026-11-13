//! The membership a group's leader keeps raft's to: the voters and learners
//! the group's state names, reached a change at a time -- what raftmember
//! does at every turn as the leader, here so that the rule is held by the
//! gate rather than by a kernel's boot.

use crate::types::{ChangeKind, ConfChange, ConfState, NodeId};

/// What the leader does next toward the membership its state names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// This change, proposed.
    Change(ConfChange),
    /// The leader itself is to leave the voters: the lead handed on first
    /// (`RawNode::successors`), whose taker then makes the change.
    HandLead,
    /// Nothing to change now.
    Nothing,
}

/// The next step from `conf` toward `voters` and `learners`, on the leader
/// `me`, in this order:
/// - a member neither names, taken out -- never the leader itself;
/// - the leader itself to leave the voters -- removed, or named a learner:
///   `HandLead` where `can_hand` (a voter that stays is heard from, or a
///   hand-over is on its way); where not, the changes below go on, so that
///   a voter that stays is made rather than the membership kept as it is
///   for good (a group of [a], b added as a learner and a removed at once,
///   stayed on a, which could neither hand the lead on nor make b a voter);
/// - a node named and not a member, added as a learner, whatever it is to
///   be;
/// - a learner named a voter, made one once it holds the log to within
///   `caught_up` entries of `committed` (`matched` says what a member holds);
/// - a voter named a learner made one -- not the leader, whose own goes by
///   the hand-over above.
pub fn next_change(conf: &ConfState, me: NodeId, voters: &[NodeId], learners: &[NodeId], can_hand: bool,
                   matched: &dyn Fn(NodeId) -> u64, committed: u64, caught_up: u64) -> Next
{
    let named = |n: &NodeId| voters.contains(n) || learners.contains(n);
    let member = |n: &NodeId| conf.voters.contains(n) || conf.learners.contains(n);
    if let Some(&n) = conf.voters.iter().chain(conf.learners.iter()).find(|n| **n != me && !named(n)) {
        return Next::Change(ConfChange { kind: ChangeKind::Remove, node: n });
    }
    if !voters.contains(&me) && can_hand {
        return Next::HandLead;
    }
    if let Some(&n) = voters.iter().chain(learners.iter()).find(|n| !member(n)) {
        return Next::Change(ConfChange { kind: ChangeKind::AddLearner, node: n });
    }
    if let Some(&n) = voters.iter().find(|n| conf.learners.contains(n)) {
        if matched(n).saturating_add(caught_up) >= committed {
            return Next::Change(ConfChange { kind: ChangeKind::AddVoter, node: n });
        }
        return Next::Nothing;
    }
    match learners.iter().find(|n| **n != me && conf.voters.contains(n)) {
        Some(&n) => Next::Change(ConfChange { kind: ChangeKind::AddLearner, node: n }),
        None => Next::Nothing,
    }
}
