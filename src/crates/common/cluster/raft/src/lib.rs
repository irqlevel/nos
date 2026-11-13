//! Raft, the consensus protocol, as a crate of its own: a group of members
//! that agree on one log of entries, so that every member applies the same
//! entries in the same order and none applied is ever lost while a majority
//! of the members lives.
//!
//! The protocol is a state machine with no I/O, in etcd's shape
//! ([`node::RawNode`]): the caller hands it the time as ticks (`tick`), what
//! arrives from the other members (`step`) and what its users ask for
//! (`propose`, `propose_change`, `read_index`, `transfer_leader`), and asks it
//! what to do (`ready`): the state and the entries to make durable, the
//! messages to send, the entries now committed to apply, the reads that may
//! now be served. Once the caller has done that, it says so (`advance`).
//! There is no clock, no network, no thread and no lock in here -- one owner
//! drives a node -- so the same code runs in a kernel module over its message
//! channel and in a host program over a simulated network (fuzz/raft).
//!
//! What is in it, by the paper and the dissertation (Ongaro, 2014):
//!
//! - elections with randomised timeouts, the randomness seeded by the caller;
//!   PreVote, so that a member cut off and back does not depose a leader
//!   that is fine; CheckQuorum, so that a leader that hears from no majority
//!   steps down;
//! - replication pipelined and held to a count and a number of bytes in
//!   flight a follower, a conflict found in few round trips (the follower's
//!   hint by term);
//! - an entry committed only by counting replicas of an entry of the
//!   leader's own term (the paper's figure 8);
//! - snapshots: the caller's bytes, sent to a follower too far behind in
//!   pieces;
//! - learners, which hold everything and are not counted: nobody asks them
//!   for a vote, nor counts an entry or a read by them (one that is asked
//!   answers, and keeps its vote, as any member does); membership changed one
//!   server at a time, each change in force as it is appended, with the 2015
//!   fix: no change before the leader has committed an entry of its term,
//!   and never two changes not yet committed;
//! - leadership transfer (TimeoutNow) and linearizable reads (ReadIndex).
//!
//! Entries are bytes the crate does not read. [`wire`] makes every message
//! bytes and takes it back, checking everything; [`storage`] is what a node
//! keeps durable, with a storage in memory for tests and small groups; and
//! [`log`] keeps it on a disk, in files of a directory the user hands it --
//! read back after a crash as the member left it, a torn tail cut. [`rsm`]
//! is what every user of it needs on top: a replicated state machine over
//! a node, a store and the user's state, with clients' sessions, reads and
//! snapshots taken as the log grows; and [`asks`] is a member's own
//! commands asked through such sessions, a session each, so that one asked
//! again as its leader steps down is applied once, whoever leads.
//!
//! `no_std` with `alloc`, no `unsafe`, no dependency but the tree's `crc`
//! (the log's CRC32C), and no allocation
//! larger than the 512 KiB the kernel's heap gives at once, however long the
//! log or busy the group: a list that grows with the log is kept in pieces,
//! every batch is held to a count as well as to bytes, a message fits one
//! block, and the configuration is refused where it would let one grow
//! past (docs/raft.md, "Limits"; fuzz/raft holds every allocation to it).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod conf;
mod progress;
mod raft_log;
mod rand;

pub mod asks;
pub mod keep;
pub mod log;
pub mod node;
pub mod recover;
pub mod rsm;
pub mod storage;
pub mod types;
pub mod wire;

pub use conf::{ChangeError, MAX_MEMBERS};
pub use node::{Config, Error, Ready, RawNode, ReadState, Refusal, Role, SoftState, Stats};
pub use log::{DiskLog, FileError, Files, LogConfig, LogError};
pub use rsm::{
    Event, Frozen, LogStore, Output, RestoreError, Rsm, RsmError, RsmStats, SnapshotReader, StateMachine, StoreError,
};
pub use storage::{MemStorage, RaftState, Storage, StorageError};
pub use types::*;
