//! What the module keeps in its folder -- /nbs/mds unless `insmod
//! nbs-mds.ko dir=` names another:
//!
//! - `member`, which member of which NBS cluster this host is: its
//!   incarnation, the cluster's id, this host's name and the cluster's name
//!   on its first line; then, for a host the cluster was offered to, a line
//!   for each member it was offered by -- a host and its incarnation -- what
//!   it knows the members by until its log has caught up. Written once, when
//!   the host makes the cluster or takes an offer, and never written over:
//!   the mark that this host is a member, so that a log in `raft/` with no
//!   `member` beside it is what an `init` or an offer that did not finish
//!   left, and the next one clears it (raftmember's `offered::MemberFile`);
//! - `raft/`, the member's log (`raft::log::DiskLog`), over raftmember's
//!   `KFiles`;
//! - `begun`, the mark that the member began its log, which raftmember
//!   writes before the log's first segment: `raft/` holding no log beside it
//!   is a log gone, and the member is not started as the one it was;
//! - `.sync`, an empty file a sync of the folder syncs.
//!
//! A host whose folder is lost comes back with none, and is offered the
//! cluster again as a new member: never the one it was.

pub const DEFAULT_DIR: &str = "/nbs/mds";
