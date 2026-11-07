//! A member made a group of itself alone, for a group whose majority is
//! gone for good (plans/cluster.md, "When the majority is gone"): every
//! other member taken out of its log's membership by changes appended to
//! it, one a member, and the log committed to its end there and then.
//!
//! Unsafe by design, as etcd's `--force-new-cluster` is: what the group
//! had committed and this member had not is gone, and what this member had
//! appended and the group never committed is committed now. The members
//! taken out are never to come back with what they hold -- their logs and
//! this one's may now differ where both say committed -- so whoever calls
//! this keeps them out for good (the cluster's registry removes them, and
//! their keys with them).
//!
//! It works on a member's store while no member runs over it: the member
//! is started again over the store it hands back, the one voter of its
//! group, and elects itself.

use alloc::vec::Vec;

use crate::conf::Membership;
use crate::node::{Config, Error, RawNode};
use crate::rsm::{LogStore, StoreError};
use crate::types::{ChangeKind, ConfChange, Entry, EntryKind, HardState, NodeId};
use crate::wire;

/// What `force_alone` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forced {
    /// The members taken out, in the order their changes were appended:
    /// the voters, then the learners.
    pub removed: Vec<NodeId>,
    /// What the log had committed before.
    pub committed_before: u64,
    /// Its last entry now, and what it has committed.
    pub last: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForceError {
    /// The store holds no member's state, or one that cannot be read.
    Node(Error),
    /// Not a voter of the membership in force: a learner, or a member that
    /// was never let in, is no group's last voter.
    NotAVoter,
    /// The store took the changes, or the commit, only in part: what it
    /// holds is what a failed write leaves, which opening it again says.
    Store(StoreError),
}

/// Member `me`'s store made the log of a group of `me` alone (above). The
/// store back, durable, and what was done; nothing written when it is
/// refused before writing.
pub fn force_alone<S: LogStore>(store: S, me: NodeId) -> Result<(S, Forced), ForceError> {
    let node = RawNode::new(Config::new(me), store).map_err(ForceError::Node)?;
    let conf = node.membership();
    if !conf.voters.contains(&me) {
        return Err(ForceError::NotAVoter);
    }
    let others: Vec<NodeId> = conf.voters.iter().chain(conf.learners.iter()).copied().filter(|&n| n != me).collect();
    let committed_before = node.committed();
    let mut last = node.last_index();
    /* The changes' term: the member's own, never below its last entry's, so
       that the log's terms still never go down */
    let term = node.term().max(node.term_at(last).unwrap_or(0));
    let vote = node.vote();
    let mut store = node.into_storage();
    let mut membership = Membership::from_state(&conf);
    let mut entries = Vec::with_capacity(others.len());
    for &n in &others {
        let cc = ConfChange { kind: ChangeKind::Remove, node: n };
        /* A member, taken out while `me` stays a voter: never refused */
        membership = membership.changed(cc).map_err(|_| ForceError::NotAVoter)?;
        last += 1;
        entries.push(Entry { term, index: last, kind: EntryKind::Change, data: wire::encode_change(&cc) });
    }
    store.append(&entries).map_err(ForceError::Store)?;
    store.set_hard_state(HardState { term, vote, commit: last }).map_err(ForceError::Store)?;
    store.sync().map_err(ForceError::Store)?;
    Ok((store, Forced { removed: others, committed_before, last }))
}
