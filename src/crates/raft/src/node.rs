//! One member of a group: the protocol, with no I/O ([`RawNode`]).
//!
//! The caller drives it:
//!
//! ```text
//! loop {
//!     every tick:            node.tick()
//!     a message arrives:     node.step(m)
//!     a user asks:           node.propose(data), propose_change(cc), read_index(ctx), transfer_leader(to)
//!     if node.has_ready() {
//!         let rd = node.ready();
//!         write rd.snapshot, rd.entries and rd.hard_state to the disk, and sync
//!         send rd.messages                 -- only now: a vote is on the disk before it is answered
//!         apply rd.committed; serve rd.read_states once applied that far
//!         node.advance();
//!     }
//! }
//! ```
//!
//! Nothing between `ready` and `advance` may step the node: what `advance`
//! marks written is what `ready` handed out.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;
use core::cmp;
use core::fmt;
use core::mem;

use crate::conf::{self, ChangeError, Changes, Membership};
use crate::progress::{Progress, State};
use crate::raft_log::RaftLog;
use crate::rand::Rng;
use crate::storage::{Storage, StorageError};
use crate::types::{
    Body, ChangeKind, ConfChange, ConfState, Entry, EntryKind, HardState, Message, NodeId, Snapshot,
    SnapshotBuilder, SnapshotMeta, MAX_SNAPSHOT_BYTES, NONE, PIECE_MAX,
};
use crate::wire;

/* What a configuration is unless its caller says otherwise. */
const ELECTION_TICKS: u32 = 10;
const HEARTBEAT_TICKS: u32 = 1;
const MAX_ENTRIES_PER_MSG: usize = 256;
const MAX_BYTES_PER_MSG: u64 = 256 * 1024;
const MAX_INFLIGHT_MSGS: usize = 64;
const MAX_INFLIGHT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 64 * 1024;
const MAX_UNCOMMITTED_ENTRIES: u64 = 4096;
const MAX_APPLY_BYTES: u64 = 1024 * 1024;
/// Appends on their way to one follower, at most whatever the caller says:
/// its window is a list a block of the kernel's heap holds.
const MAX_INFLIGHT_MSGS_LIMIT: usize = 4096;
/// Reads waiting for their round, at most: past them a read is refused.
const MAX_PENDING_READS: usize = 1024;
/// Messages one `Ready` hands out, at most: some 272 KiB of them, a block
/// of the kernel's heap (`Message` is 136 bytes).
const MAX_OUTBOX: usize = 2048;
const _: () = assert!(MAX_OUTBOX * core::mem::size_of::<Message>() <= 512 * 1024);
/// Past every term and every index a group will ever reach -- a term a
/// microsecond for longer than the sun has: a message naming one is
/// refused, so nothing done with them can overflow.
const MAX_TERM_OR_INDEX: u64 = 1 << 62;

/// How a member behaves: its id, its timing in ticks, and the limits it
/// keeps -- every one of them a bound on memory or on a message.
#[derive(Clone, Debug)]
pub struct Config {
    pub id: NodeId,
    /// Ticks without a word from a leader before an election: each member
    /// waits this many, and up to as many again at random.
    pub election_ticks: u32,
    /// Ticks between a leader's heartbeats.
    pub heartbeat_ticks: u32,
    /// Ask first whether an election could be won, and change nothing if
    /// not: a member cut off and back does not depose a working leader.
    pub pre_vote: bool,
    /// A leader that has not heard from a majority for an election's time
    /// steps down; a member that has heard from a leader that recently
    /// does not vote against it.
    pub check_quorum: bool,
    /// An append carries at most this many entries, and at most this many
    /// bytes of them but for its first.
    pub max_entries_per_msg: usize,
    pub max_bytes_per_msg: u64,
    /// Appends on their way to one follower, and their bytes, at most.
    pub max_inflight_msgs: usize,
    pub max_inflight_bytes: u64,
    /// The largest entry a proposal may make.
    pub max_entry_bytes: usize,
    /// Entries appended and not yet committed, at most -- a leader's
    /// proposals past it are refused (`Busy`), a follower's appends past
    /// it dropped and sent again.
    pub max_uncommitted_entries: u64,
    /// Bytes of committed entries a `Ready` hands to apply, at most (but
    /// always one entry).
    pub max_apply_bytes: u64,
    /// The bytes of a snapshot one message carries.
    pub snapshot_piece_bytes: usize,
    /// The largest snapshot a follower takes in.
    pub max_snapshot_bytes: u64,
    /// The election timeouts' randomness.
    pub seed: u64,
}

impl Config {
    pub fn new(id: NodeId) -> Config {
        Config {
            id,
            election_ticks: ELECTION_TICKS,
            heartbeat_ticks: HEARTBEAT_TICKS,
            pre_vote: true,
            check_quorum: true,
            max_entries_per_msg: MAX_ENTRIES_PER_MSG,
            max_bytes_per_msg: MAX_BYTES_PER_MSG,
            max_inflight_msgs: MAX_INFLIGHT_MSGS,
            max_inflight_bytes: MAX_INFLIGHT_BYTES,
            max_entry_bytes: MAX_ENTRY_BYTES,
            max_uncommitted_entries: MAX_UNCOMMITTED_ENTRIES,
            max_apply_bytes: MAX_APPLY_BYTES,
            snapshot_piece_bytes: PIECE_MAX,
            max_snapshot_bytes: MAX_SNAPSHOT_BYTES,
            seed: id,
        }
    }

    /// Whether it can be run by: or which of its values cannot.
    pub fn validate(&self) -> Result<(), Error> {
        let bad = |what| Err(Error::BadConfig(what));
        if self.id == NONE {
            return bad("an id of 0, which is nobody");
        }
        if self.heartbeat_ticks == 0 || self.election_ticks <= self.heartbeat_ticks {
            return bad("an election timeout no longer than a heartbeat");
        }
        if self.max_entries_per_msg == 0 || self.max_entries_per_msg > wire::MAX_ENTRIES {
            return bad("entries a message carries");
        }
        /* An append carries no more than this many bytes of entries but for
         * its first, and no entry is larger than an entry the wire carries:
         * so every append fits a message (`wire::MAX_APPEND_DATA`). */
        if self.max_entry_bytes > wire::MAX_ENTRY_BYTES || self.max_bytes_per_msg > wire::MAX_APPEND_DATA as u64 {
            return bad("bytes a message carries");
        }
        /* A follower's window, a list a block of the heap holds. */
        if self.max_inflight_msgs == 0 || self.max_inflight_msgs > MAX_INFLIGHT_MSGS_LIMIT || self.max_inflight_bytes == 0 {
            return bad("appends in flight");
        }
        /* The entries waiting to be written are a list, one block of the
         * heap: 48 bytes each besides their data. */
        if self.max_uncommitted_entries == 0 || self.max_uncommitted_entries > wire::MAX_ENTRIES as u64 {
            return bad("entries not yet committed");
        }
        if self.snapshot_piece_bytes == 0 || self.snapshot_piece_bytes > PIECE_MAX {
            return bad("bytes of a snapshot a message carries");
        }
        if self.max_snapshot_bytes > MAX_SNAPSHOT_BYTES {
            return bad("bytes of a snapshot taken in");
        }
        Ok(())
    }
}

/// What a member is in its term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Follower,
    /// Asking whether it could win an election, without standing yet.
    PreCandidate,
    Candidate,
    Leader,
}

/// What a member is and whom it follows: nothing of it need be durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoftState {
    pub leader: NodeId,
    pub role: Role,
}

/// A read confirmed: it may be served once everything up to `index` has
/// been applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadState {
    pub index: u64,
    pub ctx: u64,
}

/// What a member has to do, as `ready` hands it out.
#[derive(Clone, Debug, Default)]
pub struct Ready {
    /// Changed since the last `Ready`: whom it follows, what it is.
    pub soft_state: Option<SoftState>,
    /// Changed since the last `Ready`: to be written before any message is
    /// sent.
    pub hard_state: Option<HardState>,
    /// A snapshot a leader sent, to be written -- the log is it from now on
    /// -- and the caller's state restored from.
    pub snapshot: Option<Snapshot>,
    /// Entries to write, after whatever comes before their first's index and
    /// over whatever is from it on.
    pub entries: Vec<Entry>,
    /// To send once the rest is written.
    pub messages: Vec<Message>,
    /// Committed entries to apply, in order.
    pub committed: Vec<Entry>,
    pub read_states: Vec<ReadState>,
}

/// Why something was refused, or could not be done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Only the leader does it; the leader it knows, or `NONE`.
    NotLeader(NodeId),
    /// The leadership is being handed over: nothing is proposed meanwhile.
    TransferInProgress,
    /// An entry past `max_entry_bytes`.
    TooLarge,
    /// `max_uncommitted_entries` entries wait to be committed already.
    Busy,
    ChangeRefused(Refusal),
    /// Not a voter of the group.
    UnknownNode,
    BadConfig(&'static str),
    Storage(StorageError),
    /// A message for another member.
    NotForMe,
    /// A message that cannot be -- dropped and counted. A correct member
    /// never sends one.
    Invalid,
}

/// Why a membership change was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The leader has not committed an entry of its own term yet: a change
    /// made before that could overlap one an earlier leader left in the log
    /// uncommitted (the dissertation's 2015 erratum).
    NotYetCommittedInTerm,
    /// A change is in the log and not yet committed: one at a time.
    ChangePending,
    /// The leader made a learner or taken away: hand the leadership over
    /// first.
    ChangesLeader,
    Change(ChangeError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotLeader(NONE) => f.write_str("not the leader, and no leader is known"),
            Error::NotLeader(id) => write!(f, "not the leader: member {} is", id),
            Error::TransferInProgress => f.write_str("the leadership is being handed over"),
            Error::TooLarge => f.write_str("an entry larger than the group takes"),
            Error::Busy => f.write_str("too many entries wait to be committed"),
            Error::ChangeRefused(r) => write!(f, "membership change refused: {:?}", r),
            Error::UnknownNode => f.write_str("not a voter of the group"),
            Error::BadConfig(what) => write!(f, "a configuration that cannot run: {}", what),
            Error::Storage(e) => write!(f, "storage: {:?}", e),
            Error::NotForMe => f.write_str("a message for another member"),
            Error::Invalid => f.write_str("a message that cannot be"),
        }
    }
}

/// What a member has counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Messages that cannot be, dropped: none from a correct member.
    pub invalid: u64,
    pub not_for_me: u64,
    /// Messages of a term gone by.
    pub stale: u64,
    /// Vote requests not taken while a leader is heard from.
    pub ignored_in_lease: u64,
    /// Appends not taken in for room: their leader sends them again.
    pub dropped: u64,
    /// Messages not sent: more than a `Ready` hands out waited for one.
    pub unsent: u64,
    pub pre_campaigns: u64,
    pub campaigns: u64,
    pub leaderships: u64,
    pub reads_refused: u64,
}

/// What `ready` handed out, for `advance` to mark done.
#[derive(Clone, Copy, Debug)]
struct Handed {
    hard: HardState,
    entry: Option<(u64, u64)>,
    snapshot: Option<u64>,
    applied: Option<u64>,
}

/// A snapshot a follower is taking in, piece by piece.
#[derive(Debug)]
struct Incoming {
    meta: SnapshotMeta,
    total: u64,
    built: SnapshotBuilder,
}

/// A round of reads: the requests made up to `index`, and the voters that
/// have confirmed the leadership since.
#[derive(Debug)]
struct ReadRound {
    round: u64,
    index: u64,
    from: NodeId,
    ctx: u64,
    acks: BTreeSet<NodeId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Won,
    Lost,
    Pending,
}

pub struct RawNode<S: Storage> {
    cfg: Config,
    id: NodeId,
    term: u64,
    vote: NodeId,
    leader: NodeId,
    role: Role,
    log: RaftLog<S>,
    /// The membership in force: the latest in the log, committed or not.
    membership: Membership,
    /// The membership as of the log's first entry -- the snapshot's.
    base: Membership,
    /// The membership each change in the log after `base` made, by index.
    changes: Changes,
    /// A leader's view of every member.
    progress: BTreeMap<NodeId, Progress>,
    /// A candidate's answers.
    votes: BTreeMap<NodeId, bool>,
    election_elapsed: u32,
    heartbeat_elapsed: u32,
    randomized_timeout: u32,
    rng: Rng,
    lead_transferee: NodeId,
    /// The index of the entry a leader appended as its term began.
    term_start: u64,
    reads: VecDeque<ReadRound>,
    /// Reads asked of a leader that has not committed an entry of its term.
    waiting_reads: VecDeque<(NodeId, u64)>,
    next_round: u64,
    incoming: Option<Incoming>,
    msgs: Vec<Message>,
    read_states: Vec<crate::node::ReadState>,
    prev_soft: SoftState,
    prev_hard: HardState,
    handed: Option<Handed>,
    stats: Stats,
}

impl<S: Storage> RawNode<S> {
    /// A member over what `storage` has: a new one with nothing, a group's
    /// first members over [`crate::MemStorage::bootstrap`], or one starting
    /// again from what it had written.
    pub fn new(cfg: Config, storage: S) -> Result<RawNode<S>, Error> {
        cfg.validate()?;
        let state = storage.initial_state().map_err(Error::Storage)?;
        if !conf::valid(&state.conf) {
            return Err(Error::Storage(StorageError::Corrupt));
        }
        let hs = state.hard_state;
        let log = RaftLog::new(storage, hs.commit).map_err(Error::Storage)?;
        let base = Membership::from_state(&state.conf);
        let mut node = RawNode {
            id: cfg.id,
            rng: Rng::new(cfg.seed),
            cfg,
            term: hs.term,
            vote: hs.vote,
            leader: NONE,
            role: Role::Follower,
            log,
            membership: base.clone(),
            base,
            changes: Changes::default(),
            progress: BTreeMap::new(),
            votes: BTreeMap::new(),
            election_elapsed: 0,
            heartbeat_elapsed: 0,
            randomized_timeout: 0,
            lead_transferee: NONE,
            term_start: 0,
            reads: VecDeque::new(),
            waiting_reads: VecDeque::new(),
            next_round: 0,
            incoming: None,
            msgs: Vec::new(),
            read_states: Vec::new(),
            prev_soft: SoftState { leader: NONE, role: Role::Follower },
            prev_hard: hs,
            handed: None,
            stats: Stats::default(),
        };
        node.load_changes()?;
        let term = node.term;
        node.become_follower(term, NONE);
        Ok(node)
    }

    /// The membership changes in the log, read as a start finds them.
    fn load_changes(&mut self) -> Result<(), Error> {
        let mut at = self.log.first_index();
        let end = self.log.last_index() + 1;
        while at < end {
            let batch = self.log.entries(at, end, self.cfg.max_apply_bytes, wire::MAX_ENTRIES).map_err(Error::Storage)?;
            let Some(last) = batch.last() else { return Err(Error::Storage(StorageError::Corrupt)) };
            at = last.index + 1;
            for e in batch.iter().filter(|e| e.kind == EntryKind::Change) {
                if wire::decode_change(&e.data).is_err() {
                    return Err(Error::Storage(StorageError::Corrupt));
                }
                self.apply_change_entry(e.index, &e.data);
            }
        }
        Ok(())
    }

    /* ---- what it is ---- */

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn term(&self) -> u64 {
        self.term
    }

    pub fn vote(&self) -> NodeId {
        self.vote
    }

    pub fn leader(&self) -> NodeId {
        self.leader
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn committed(&self) -> u64 {
        self.log.committed
    }

    pub fn applied(&self) -> u64 {
        self.log.applied
    }

    pub fn first_index(&self) -> u64 {
        self.log.first_index()
    }

    pub fn last_index(&self) -> u64 {
        self.log.last_index()
    }

    /// The term of the entry at `index`, the snapshot's own included.
    pub fn term_at(&self, index: u64) -> Option<u64> {
        self.log.term(index)
    }

    /// The membership in force.
    pub fn membership(&self) -> ConfState {
        self.membership.to_state()
    }

    /// The membership as of entry `index` -- what a snapshot taken there
    /// says -- or None for an index the log no longer has.
    pub fn conf_state_at(&self, index: u64) -> Option<ConfState> {
        if index + 1 < self.log.first_index() || index > self.log.last_index() {
            return None;
        }
        Some(self.changes.at(index).unwrap_or(&self.base).to_state())
    }

    /// Entries `lo..hi` of the log, at most `max_bytes` of them but for
    /// the first, and at most [`wire::MAX_ENTRIES`].
    pub fn entries(&self, lo: u64, hi: u64, max_bytes: u64) -> Result<Vec<Entry>, Error> {
        self.log.entries(lo, hi, max_bytes, wire::MAX_ENTRIES).map_err(Error::Storage)
    }

    pub fn storage(&self) -> &S {
        &self.log.storage
    }

    /// The storage, for the caller to write what a `Ready` hands it and to
    /// take its snapshots in.
    pub fn storage_mut(&mut self) -> &mut S {
        &mut self.log.storage
    }

    /// The storage back -- what a crash would leave.
    pub fn into_storage(self) -> S {
        self.log.storage
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Ticks since the election timer last started over: for tests and for
    /// a diagnostic.
    pub fn election_elapsed(&self) -> u32 {
        self.election_elapsed
    }

    /// The ticks this member waits, this time, before it stands.
    pub fn randomized_election_timeout(&self) -> u32 {
        self.randomized_timeout
    }

    fn promotable(&self) -> bool {
        self.membership.is_voter(self.id)
    }

    fn in_lease(&self) -> bool {
        self.cfg.check_quorum && self.leader != NONE && self.election_elapsed < self.cfg.election_ticks
    }

    fn hard_state(&self) -> HardState {
        HardState { term: self.term, vote: self.vote, commit: self.log.committed }
    }

    fn soft_state(&self) -> SoftState {
        SoftState { leader: self.leader, role: self.role }
    }

    /* ---- what it is asked ---- */

    /// The time has moved on by a tick.
    pub fn tick(&mut self) {
        match self.role {
            Role::Leader => self.tick_leader(),
            _ => self.tick_election(),
        }
    }

    /// Stand now, as a timeout would have it: with PreVote, ask first.
    pub fn campaign(&mut self) -> Result<(), Error> {
        if !self.promotable() {
            return Err(Error::UnknownNode);
        }
        if self.role != Role::Leader {
            self.start_election();
        }
        Ok(())
    }

    /// `data` appended to the log, to be committed: its index. The leader's
    /// alone to do.
    pub fn propose(&mut self, data: Vec<u8>) -> Result<u64, Error> {
        self.check_proposal(data.len())?;
        let index = self.log.last_index() + 1;
        self.log.append(&[Entry { term: self.term, index, kind: EntryKind::Normal, data }]);
        self.broadcast_append();
        Ok(index)
    }

    /// A membership change appended to the log -- in force at once, on the
    /// leader and on each member as it appends it: its index.
    pub fn propose_change(&mut self, cc: ConfChange) -> Result<u64, Error> {
        self.check_proposal(0)?;
        if self.log.committed < self.term_start {
            return Err(Error::ChangeRefused(Refusal::NotYetCommittedInTerm));
        }
        if self.last_change_index() > self.log.committed {
            return Err(Error::ChangeRefused(Refusal::ChangePending));
        }
        if cc.node == self.id && cc.kind != ChangeKind::AddVoter {
            return Err(Error::ChangeRefused(Refusal::ChangesLeader));
        }
        self.membership.changed(cc).map_err(|e| Error::ChangeRefused(Refusal::Change(e)))?;
        let index = self.log.last_index() + 1;
        let data = wire::encode_change(&cc);
        self.log.append(&[Entry { term: self.term, index, kind: EntryKind::Change, data: data.clone() }]);
        self.apply_change_entry(index, &data);
        if self.maybe_commit() {
            self.on_committed();
        }
        self.broadcast_append();
        Ok(index)
    }

    fn check_proposal(&self, bytes: usize) -> Result<(), Error> {
        if self.role != Role::Leader {
            return Err(Error::NotLeader(self.leader));
        }
        if self.lead_transferee != NONE {
            return Err(Error::TransferInProgress);
        }
        if bytes > self.cfg.max_entry_bytes {
            return Err(Error::TooLarge);
        }
        if self.log.last_index() - self.log.committed >= self.cfg.max_uncommitted_entries {
            return Err(Error::Busy);
        }
        Ok(())
    }

    /// A linearizable read: once its `ReadState` comes back with `ctx`, and
    /// the caller has applied up to its index, what it reads is at least as
    /// new as everything committed before it was asked. A follower asks the
    /// leader; nothing comes back if the leadership changes meanwhile.
    pub fn read_index(&mut self, ctx: u64) -> Result<(), Error> {
        if self.role == Role::Leader {
            self.leader_read(self.id, ctx);
            return Ok(());
        }
        if self.leader == NONE {
            return Err(Error::NotLeader(NONE));
        }
        let lead = self.leader;
        self.send(lead, Body::ReadIndex { ctx });
        Ok(())
    }

    /// The leadership handed to voter `to`: brought up to date, then told
    /// to stand. Proposals are refused meanwhile; given up after an
    /// election's time.
    pub fn transfer_leader(&mut self, to: NodeId) -> Result<(), Error> {
        if self.role != Role::Leader {
            return Err(Error::NotLeader(self.leader));
        }
        if to == self.id || self.lead_transferee == to {
            return Ok(());
        }
        if !self.membership.is_voter(to) {
            return Err(Error::UnknownNode);
        }
        self.lead_transferee = to;
        self.election_elapsed = 0;
        let caught_up = self.progress.get(&to).is_some_and(|p| p.matched == self.log.last_index());
        if caught_up {
            self.send(to, Body::TimeoutNow);
        } else {
            self.maybe_send_append(to, true);
        }
        Ok(())
    }

    /* ---- the timers ---- */

    fn tick_election(&mut self) {
        self.election_elapsed = self.election_elapsed.saturating_add(1);
        if self.promotable() && self.election_elapsed >= self.randomized_timeout {
            self.election_elapsed = 0;
            self.start_election();
        }
    }

    fn tick_leader(&mut self) {
        self.heartbeat_elapsed = self.heartbeat_elapsed.saturating_add(1);
        self.election_elapsed = self.election_elapsed.saturating_add(1);
        if self.election_elapsed >= self.cfg.election_ticks {
            self.election_elapsed = 0;
            if self.cfg.check_quorum && !self.quorum_active() {
                let term = self.term;
                self.become_follower(term, NONE);
                return;
            }
            /* A handover not done in an election's time is given up. */
            self.lead_transferee = NONE;
        }
        if self.heartbeat_elapsed >= self.cfg.heartbeat_ticks {
            self.heartbeat_elapsed = 0;
            self.broadcast_heartbeat(0);
        }
    }

    /// Whether a majority of the voters has been heard from since the last
    /// look -- and the look starts over.
    fn quorum_active(&mut self) -> bool {
        let me = self.id;
        let active = self
            .membership
            .voters()
            .iter()
            .filter(|v| **v == me || self.progress.get(v).is_some_and(|p| p.recent_active))
            .count();
        for (id, pr) in self.progress.iter_mut() {
            if *id != me {
                pr.recent_active = false;
            }
        }
        active >= self.membership.quorum()
    }

    /* ---- the roles ---- */

    fn reset(&mut self, term: u64) {
        if self.term != term {
            self.term = term;
            self.vote = NONE;
            self.incoming = None;
        }
        self.leader = NONE;
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;
        self.randomized_timeout =
            self.cfg.election_ticks + self.rng.below(u64::from(self.cfg.election_ticks)) as u32;
        self.votes.clear();
        self.lead_transferee = NONE;
        self.progress.clear();
        self.reads.clear();
        self.waiting_reads.clear();
    }

    fn become_follower(&mut self, term: u64, leader: NodeId) {
        self.reset(term);
        self.role = Role::Follower;
        self.leader = leader;
    }

    fn start_election(&mut self) {
        if self.cfg.pre_vote {
            self.campaign_pre();
        } else {
            self.campaign_real(false);
        }
    }

    /// Whether this member could win an election in the next term: nothing
    /// changes, its term and its vote included, unless a majority says yes.
    fn campaign_pre(&mut self) {
        let term = self.term;
        self.reset(term);
        self.role = Role::PreCandidate;
        self.stats.pre_campaigns += 1;
        if self.poll(self.id, true) == Outcome::Won {
            self.campaign_real(false);
            return;
        }
        let (last_index, last_term) = (self.log.last_index(), self.log.last_term());
        let voters: Vec<NodeId> = self.membership.voters().iter().copied().filter(|v| *v != self.id).collect();
        for v in voters {
            self.send_term(v, term + 1, Body::Vote { pre: true, transfer: false, last_index, last_term });
        }
    }

    fn campaign_real(&mut self, transfer: bool) {
        let term = self.term + 1;
        self.reset(term);
        self.vote = self.id;
        self.role = Role::Candidate;
        self.stats.campaigns += 1;
        if self.poll(self.id, true) == Outcome::Won {
            self.become_leader();
            self.broadcast_append();
            return;
        }
        let (last_index, last_term) = (self.log.last_index(), self.log.last_term());
        let voters: Vec<NodeId> = self.membership.voters().iter().copied().filter(|v| *v != self.id).collect();
        for v in voters {
            self.send(v, Body::Vote { pre: false, transfer, last_index, last_term });
        }
    }

    /// A voter's answer counted, its first: whether the election is won,
    /// lost or still open.
    fn poll(&mut self, from: NodeId, granted: bool) -> Outcome {
        if self.membership.is_voter(from) {
            self.votes.entry(from).or_insert(granted);
        }
        let yes = self.votes.values().filter(|g| **g).count();
        let no = self.votes.len() - yes;
        let quorum = self.membership.quorum();
        if yes >= quorum {
            Outcome::Won
        } else if no >= quorum {
            Outcome::Lost
        } else {
            Outcome::Pending
        }
    }

    fn become_leader(&mut self) {
        let term = self.term;
        self.reset(term);
        self.role = Role::Leader;
        self.leader = self.id;
        self.stats.leaderships += 1;
        let last = self.log.last_index();
        let members: Vec<NodeId> = self.membership.members().collect();
        for id in members {
            self.progress.insert(id, Progress::new(last + 1, self.cfg.max_inflight_msgs, self.cfg.max_inflight_bytes));
        }
        let stable = self.log.stable_last_index();
        if let Some(me) = self.progress.get_mut(&self.id) {
            me.matched = stable;
            me.recent_active = true;
        }
        /* An entry of its own term, to commit by: until it is committed,
         * the leader knows of nothing committed in its term. */
        self.term_start = last + 1;
        self.log.append(&[Entry { term, index: last + 1, kind: EntryKind::Noop, data: Vec::new() }]);
    }

    /* ---- messages ---- */

    fn send(&mut self, to: NodeId, body: Body) {
        let term = self.term;
        self.send_term(to, term, body);
    }

    fn send_term(&mut self, to: NodeId, term: u64, body: Body) {
        /* Past what one `Ready` hands out, dropped as a network drops a
         * message -- Raft sends again what is lost -- so that a caller
         * that lets them wait does not make the list too large to hold. */
        if self.msgs.len() >= MAX_OUTBOX {
            self.stats.unsent += 1;
            return;
        }
        self.msgs.push(Message { from: self.id, to, term, body });
    }

    /// A read confirmed, for the caller: past what one `Ready` hands out,
    /// refused, as a read the leader had no room for is.
    fn push_read_state(&mut self, rs: ReadState) {
        if self.read_states.len() >= MAX_PENDING_READS {
            self.stats.reads_refused += 1;
            return;
        }
        self.read_states.push(rs);
    }

    /// A message from another member.
    pub fn step(&mut self, m: Message) -> Result<(), Error> {
        if m.to != self.id {
            self.stats.not_for_me += 1;
            return Err(Error::NotForMe);
        }
        if m.from == NONE || m.from == self.id || !self.well_formed(&m) {
            self.stats.invalid += 1;
            return Err(Error::Invalid);
        }
        let (from, term) = (m.from, m.term);
        if term > self.term {
            match &m.body {
                Body::Vote { pre, transfer, .. } => {
                    /* A member that hears from a leader votes against no
                     * one: a member cut off and back cannot depose it. */
                    if self.in_lease() && !*transfer {
                        self.stats.ignored_in_lease += 1;
                        return Ok(());
                    }
                    if !*pre {
                        self.become_follower(term, NONE);
                    }
                }
                /* A PreVote granted carries the term asked about, ahead of
                 * ours: it is counted, and changes nothing yet. */
                Body::VoteResp { pre: true, granted: true } => {}
                body => {
                    let lead = if body.is_from_leader() { from } else { NONE };
                    self.become_follower(term, lead);
                }
            }
        } else if term < self.term {
            match &m.body {
                /* A leader of a term gone by, told of ours, as the paper has
                 * every request answered: it may never hear of it otherwise
                 * -- PreVote and CheckQuorum keep our term from coming to it
                 * in a vote request, and a member whose membership leaves it
                 * out asks it for no vote at all -- and would lead on alone. */
                Body::Append { .. } | Body::Heartbeat { .. } | Body::Snapshot { .. } => {
                    self.send(from, Body::AppendResp { reject: true, index: 0, hint_index: 0, hint_term: 0 });
                }
                /* A candidate of a term gone by, told of ours (the paper's
                 * rule for any request): a voter of an older membership that
                 * stands in term after term ahead of it would otherwise keep
                 * it from ever catching up. */
                Body::Vote { pre, .. } => {
                    let pre = *pre;
                    self.send(from, Body::VoteResp { pre, granted: false });
                }
                _ => {}
            }
            self.stats.stale += 1;
            return Ok(());
        }
        match m.body {
            Body::Vote { pre, last_index, last_term, .. } => self.handle_vote(from, term, pre, last_index, last_term),
            body => match self.role {
                Role::Leader => self.step_leader(from, body),
                Role::PreCandidate | Role::Candidate => self.step_candidate(from, body),
                Role::Follower => self.step_follower(from, body),
            },
        }
        Ok(())
    }

    /// Whether a message is one a correct member could have sent: its
    /// entries one after another, their terms in order and none past the
    /// sender's, every membership change readable, a snapshot's piece
    /// inside the snapshot.
    fn well_formed(&self, m: &Message) -> bool {
        /* Terms and indexes no group ever reaches: whatever says one is not
         * a member's, and none of the arithmetic on them can overflow. */
        let sane = |v: u64| v < MAX_TERM_OR_INDEX;
        if !sane(m.term) {
            return false;
        }
        let fields_sane = match &m.body {
            Body::Vote { last_index, last_term, .. } => sane(*last_index) && sane(*last_term),
            Body::Append { prev_index, commit, .. } => sane(*prev_index) && sane(*commit),
            Body::AppendResp { index, hint_index, hint_term, .. } => sane(*index) && sane(*hint_index) && sane(*hint_term),
            Body::Heartbeat { commit, .. } => sane(*commit),
            Body::Snapshot { meta, .. } => sane(meta.index),
            Body::SnapshotResp { index, .. } => sane(*index),
            Body::ReadIndexResp { index, .. } => sane(*index),
            _ => true,
        };
        if !fields_sane {
            return false;
        }
        match &m.body {
            Body::Append { prev_index, prev_term, entries, .. } => {
                if *prev_term > m.term || entries.len() > wire::MAX_ENTRIES {
                    return false;
                }
                let mut expect = *prev_index;
                let mut last_term = *prev_term;
                for e in entries {
                    let Some(next) = expect.checked_add(1) else { return false };
                    expect = next;
                    if e.index != expect || e.term < last_term || e.term > m.term || e.data.len() > wire::MAX_ENTRY_BYTES {
                        return false;
                    }
                    if e.kind == EntryKind::Change && wire::decode_change(&e.data).is_err() {
                        return false;
                    }
                    last_term = e.term;
                }
                true
            }
            Body::Snapshot { meta, offset, total, data } => {
                meta.index > 0
                    && meta.term > 0
                    && meta.term <= m.term
                    && conf::valid(&meta.conf)
                    && data.len() <= PIECE_MAX
                    && offset.checked_add(data.len() as u64).is_some_and(|end| end <= *total)
            }
            Body::Vote { last_term, .. } => *last_term <= m.term,
            _ => true,
        }
    }

    fn handle_vote(&mut self, from: NodeId, term: u64, pre: bool, last_index: u64, last_term: u64) {
        let can_vote = self.vote == from || (self.vote == NONE && self.leader == NONE) || (pre && term > self.term);
        if can_vote && self.log.is_up_to_date(last_index, last_term) {
            /* Answered with the term asked about: a PreVote's is the one its
             * candidate would stand in. */
            self.send_term(from, term, Body::VoteResp { pre, granted: true });
            if !pre {
                /* A vote given starts the timer over: no election of its
                 * own while the candidate it voted for may still win. */
                self.election_elapsed = 0;
                self.vote = from;
            }
        } else {
            self.send(from, Body::VoteResp { pre, granted: false });
        }
    }

    fn step_candidate(&mut self, from: NodeId, body: Body) {
        match body {
            Body::Append { .. } | Body::Heartbeat { .. } | Body::Snapshot { .. } => {
                /* The term has a leader: follow it. */
                let term = self.term;
                self.become_follower(term, from);
                self.step_follower(from, body);
            }
            Body::VoteResp { pre, granted } => {
                /* An answer to what this member asked, and not to the other
                 * kind of question. */
                if pre != (self.role == Role::PreCandidate) {
                    return;
                }
                match self.poll(from, granted) {
                    Outcome::Won if self.role == Role::PreCandidate => self.campaign_real(false),
                    Outcome::Won => {
                        self.become_leader();
                        self.broadcast_append();
                    }
                    Outcome::Lost => {
                        let term = self.term;
                        self.become_follower(term, NONE);
                    }
                    Outcome::Pending => {}
                }
            }
            _ => {}
        }
    }

    fn step_follower(&mut self, from: NodeId, body: Body) {
        match body {
            Body::Append { prev_index, prev_term, commit, entries } => {
                self.election_elapsed = 0;
                self.leader = from;
                self.handle_append(from, prev_index, prev_term, commit, entries);
            }
            Body::Heartbeat { commit, ctx } => {
                self.election_elapsed = 0;
                self.leader = from;
                self.handle_heartbeat(from, commit, ctx);
            }
            Body::Snapshot { meta, offset, total, data } => {
                self.election_elapsed = 0;
                self.leader = from;
                self.handle_snapshot(from, meta, offset, total, data);
            }
            Body::TimeoutNow => {
                if self.promotable() {
                    self.campaign_real(true);
                }
            }
            Body::ReadIndexResp { ctx, index, ok } => {
                /* From the leader it asked: no one else answers a read. */
                if ok && from == self.leader {
                    self.push_read_state(ReadState { index, ctx });
                }
            }
            _ => {}
        }
    }

    fn step_leader(&mut self, from: NodeId, body: Body) {
        if let Some(pr) = self.progress.get_mut(&from) {
            pr.recent_active = true;
        }
        match body {
            Body::AppendResp { reject, index, hint_index, hint_term } => {
                self.handle_append_resp(from, reject, index, hint_index, hint_term)
            }
            Body::HeartbeatResp { ctx } => self.handle_heartbeat_resp(from, ctx),
            Body::SnapshotResp { index, next, reject } => self.handle_snapshot_resp(from, index, next, reject),
            Body::ReadIndex { ctx } => {
                if self.membership.contains(from) {
                    self.leader_read(from, ctx);
                }
            }
            /* What only a leader sends, from another member of this term:
             * two leaders in one term, which cannot be. */
            Body::Append { .. } | Body::Heartbeat { .. } | Body::Snapshot { .. } | Body::TimeoutNow
            | Body::ReadIndexResp { .. } => self.stats.invalid += 1,
            _ => {}
        }
    }

    /* ---- a follower ---- */

    fn handle_append(&mut self, from: NodeId, prev_index: u64, prev_term: u64, commit: u64, entries: Vec<Entry>) {
        if prev_index < self.log.committed {
            /* Behind what is committed here: say how far that is. */
            let committed = self.log.committed;
            self.send(from, Body::AppendResp { reject: false, index: committed, hint_index: 0, hint_term: 0 });
            return;
        }
        if self.log.match_term(prev_index, prev_term) {
            let last_new = prev_index + entries.len() as u64;
            if !self.append_entries(prev_index, &entries) {
                return;
            }
            /* No further than this append has shown to match the leader's:
             * what comes after it here may be another leader's. */
            self.log.commit_to(cmp::min(commit, last_new));
            self.send(from, Body::AppendResp { reject: false, index: last_new, hint_index: 0, hint_term: 0 });
        } else {
            let at = cmp::min(prev_index, self.log.last_index());
            let (hint_index, hint_term) = self.log.find_conflict_by_term(at, prev_term);
            self.send(from, Body::AppendResp { reject: true, index: prev_index, hint_index, hint_term });
        }
    }

    /// `entries`, after the one at `prev_index`, put in the log over
    /// whatever they conflict with: false where they cannot be taken now.
    fn append_entries(&mut self, prev_index: u64, entries: &[Entry]) -> bool {
        let Some(conflict) = self.log.find_conflict(entries) else { return true };
        if conflict <= self.log.committed {
            /* A committed entry written over: no correct leader asks it. */
            self.stats.invalid += 1;
            return false;
        }
        let Ok(start) = usize::try_from(conflict - prev_index - 1) else { return false };
        let Some(new) = entries.get(start..) else { return false };
        if self.log.unstable_entries().len() as u64 + new.len() as u64 > self.cfg.max_uncommitted_entries {
            self.stats.dropped += 1;
            return false;
        }
        self.truncate_changes(conflict);
        self.log.append(new);
        for e in new.iter().filter(|e| e.kind == EntryKind::Change) {
            self.apply_change_entry(e.index, &e.data);
        }
        true
    }

    fn handle_heartbeat(&mut self, from: NodeId, commit: u64, ctx: u64) {
        /* The leader sends no further than what it knows this log has. */
        if commit <= self.log.last_index() {
            self.log.commit_to(commit);
        } else {
            self.stats.invalid += 1;
        }
        self.send(from, Body::HeartbeatResp { ctx });
    }

    fn handle_snapshot(&mut self, from: NodeId, meta: SnapshotMeta, offset: u64, total: u64, data: Vec<u8>) {
        let index = meta.index;
        if index <= self.log.committed {
            let committed = self.log.committed;
            self.send(from, Body::AppendResp { reject: false, index: committed, hint_index: 0, hint_term: 0 });
            return;
        }
        let same = self.incoming.as_ref().is_some_and(|i| i.meta == meta && i.total == total);
        if !same {
            if total > self.cfg.max_snapshot_bytes {
                self.send(from, Body::SnapshotResp { index, next: 0, reject: true });
                return;
            }
            if offset != 0 {
                self.send(from, Body::SnapshotResp { index, next: 0, reject: false });
                return;
            }
            self.incoming = Some(Incoming { meta, total, built: SnapshotBuilder::new() });
        }
        let Some(inc) = self.incoming.as_mut() else { return };
        if offset != inc.built.len() {
            let next = inc.built.len();
            self.send(from, Body::SnapshotResp { index, next, reject: false });
            return;
        }
        if !inc.built.push(&data) {
            /* Past any snapshot there is: `total`, checked against the
             * configuration's limit, says it cannot be. */
            self.stats.invalid += 1;
            self.incoming = None;
            return;
        }
        if inc.built.len() < total {
            let next = inc.built.len();
            self.send(from, Body::SnapshotResp { index, next, reject: false });
            return;
        }
        let Some(inc) = self.incoming.take() else { return };
        let snapshot = Snapshot { meta: inc.meta, data: inc.built.finish() };
        let answer = if self.restore(snapshot) { self.log.last_index() } else { self.log.committed };
        self.send(from, Body::AppendResp { reject: false, index: answer, hint_index: 0, hint_term: 0 });
    }

    /// The log made the snapshot: true, or false where it needs nothing of
    /// it -- the log has its last entry, and only the commit moves on.
    fn restore(&mut self, snap: Snapshot) -> bool {
        if snap.meta.index <= self.log.committed {
            return false;
        }
        if self.log.match_term(snap.meta.index, snap.meta.term) {
            self.log.commit_to(snap.meta.index);
            return false;
        }
        self.base = Membership::from_state(&snap.meta.conf);
        self.changes.clear();
        self.membership = self.base.clone();
        self.log.restore(snap);
        true
    }

    /* ---- a leader ---- */

    /// The next append for member `to`, if its progress lets one out --
    /// or the snapshot, when the log no longer has what it needs. Whether a
    /// message went.
    fn maybe_send_append(&mut self, to: NodeId, allow_empty: bool) -> bool {
        let last = self.log.last_index();
        let Some(pr) = self.progress.get_mut(&to) else { return false };
        if pr.is_paused() {
            return false;
        }
        if pr.next > last + 1 {
            pr.next = last + 1;
        }
        let next = pr.next;
        let prev_index = next - 1;
        let prev_term = self.log.term(prev_index);
        let ents = self.log.entries(next, last + 1, self.cfg.max_bytes_per_msg, self.cfg.max_entries_per_msg);
        let (prev_term, entries) = match (prev_term, ents) {
            (Some(t), Ok(e)) => (t, e),
            _ => return self.start_snapshot(to),
        };
        if entries.is_empty() && !allow_empty {
            return false;
        }
        let bytes: u64 = entries.iter().map(Entry::size).sum();
        let sent_last = entries.last().map(|e| e.index);
        let Some(pr) = self.progress.get_mut(&to) else { return false };
        match pr.state {
            State::Replicate => {
                if let Some(sent_last) = sent_last {
                    pr.next = sent_last + 1;
                    pr.inflights.add(sent_last, bytes);
                }
            }
            State::Probe => pr.probe_sent = true,
            State::Snapshot => {}
        }
        let commit = self.log.committed;
        self.send(to, Body::Append { prev_index, prev_term, commit, entries });
        true
    }

    fn start_snapshot(&mut self, to: NodeId) -> bool {
        let Ok(snapshot) = self.log.storage.snapshot() else { return false };
        if snapshot.meta.index == 0 {
            return false;
        }
        let Some(pr) = self.progress.get_mut(&to) else { return false };
        pr.become_snapshot(snapshot);
        self.send_snapshot_piece(to);
        true
    }

    fn send_snapshot_piece(&mut self, to: NodeId) {
        let Some(sending) = self.progress.get(&to).and_then(|p| p.sending.as_ref()) else { return };
        let snap = &sending.snapshot;
        let data = snap.data.read(sending.acked, self.cfg.snapshot_piece_bytes).to_vec();
        let body = Body::Snapshot { meta: snap.meta.clone(), offset: sending.acked, total: snap.data.len(), data };
        self.send(to, body);
    }

    fn peers(&self) -> Vec<NodeId> {
        self.progress.keys().copied().filter(|id| *id != self.id).collect()
    }

    fn broadcast_append(&mut self) {
        for id in self.peers() {
            self.maybe_send_append(id, true);
        }
    }

    fn broadcast_heartbeat(&mut self, ctx: u64) {
        let committed = self.log.committed;
        for id in self.peers() {
            let Some(pr) = self.progress.get(&id) else { continue };
            let (commit, snapshotting) = (cmp::min(pr.matched, committed), pr.state == State::Snapshot);
            self.send(id, Body::Heartbeat { commit, ctx });
            if snapshotting && ctx == 0 {
                /* A piece lost on the way, sent again. */
                self.send_snapshot_piece(id);
            }
        }
    }

    fn handle_append_resp(&mut self, from: NodeId, reject: bool, index: u64, hint_index: u64, hint_term: u64) {
        let last = self.log.last_index();
        if reject {
            let next_probe =
                if hint_term > 0 { self.log.find_conflict_by_term(hint_index, hint_term).0 } else { hint_index };
            let next_probe = cmp::min(next_probe, last);
            let Some(pr) = self.progress.get_mut(&from) else { return };
            if pr.maybe_decr_to(index, next_probe) {
                if pr.state == State::Replicate {
                    pr.become_probe();
                }
                self.maybe_send_append(from, true);
            }
            return;
        }
        /* A member holding more of this term's log than the leader wrote:
         * an answer that cannot be. */
        if index > last {
            self.stats.invalid += 1;
            return;
        }
        let Some(pr) = self.progress.get_mut(&from) else { return };
        let was_paused = pr.is_paused();
        if !pr.maybe_update(index) {
            return;
        }
        match pr.state {
            State::Probe => pr.become_replicate(),
            State::Snapshot => {
                let done = pr.sending.as_ref().is_none_or(|s| pr.matched >= s.snapshot.meta.index);
                if done {
                    pr.become_probe();
                }
            }
            State::Replicate => pr.inflights.free_le(index),
        }
        if self.maybe_commit() {
            self.on_committed();
            self.broadcast_append();
        } else if was_paused {
            self.maybe_send_append(from, false);
        }
        while self.maybe_send_append(from, false) {}
        let last = self.log.last_index();
        if from == self.lead_transferee && self.progress.get(&from).is_some_and(|p| p.matched == last) {
            self.send(from, Body::TimeoutNow);
        }
    }

    fn handle_heartbeat_resp(&mut self, from: NodeId, ctx: u64) {
        let last = self.log.last_index();
        let Some(pr) = self.progress.get_mut(&from) else { return };
        pr.probe_sent = false;
        /* A follower that answers heartbeats and not the appends that fill
         * its window lost them: room for one more, which the gap behind it
         * has refused and so sent back to probing. */
        if pr.state == State::Replicate && pr.inflights.full() {
            pr.inflights.free_first();
        }
        let probing = pr.state == State::Probe;
        if pr.matched < last || probing {
            self.maybe_send_append(from, probing);
        }
        if ctx != 0 {
            self.ack_read(from, ctx);
        }
    }

    fn handle_snapshot_resp(&mut self, from: NodeId, index: u64, next: u64, reject: bool) {
        let Some(pr) = self.progress.get_mut(&from) else { return };
        if pr.state != State::Snapshot {
            return;
        }
        let Some(sending) = pr.sending.as_mut() else { return };
        if sending.snapshot.meta.index != index || next > sending.snapshot.data.len() {
            return;
        }
        if reject {
            pr.become_probe();
            return;
        }
        sending.acked = next;
        if next < sending.snapshot.data.len() {
            self.send_snapshot_piece(from);
        }
    }

    /// The commit moved to the highest entry a majority of the voters has,
    /// if that entry is of this term: counting replicas commits nothing of
    /// an earlier term, which a later leader could still write over (the
    /// paper's figure 8) -- that comes committed with an entry of this term.
    fn maybe_commit(&mut self) -> bool {
        let mut matched: Vec<u64> =
            self.membership.voters().iter().map(|v| self.progress.get(v).map_or(0, |p| p.matched)).collect();
        matched.sort_unstable_by(|a, b| b.cmp(a));
        let Some(&mci) = matched.get(self.membership.quorum() - 1) else { return false };
        if mci > self.log.committed && self.log.term(mci) == Some(self.term) {
            self.log.commit_to(mci);
            return true;
        }
        false
    }

    /// The commit moved: reads that waited for this term's first commit
    /// start their rounds.
    fn on_committed(&mut self) {
        if self.role != Role::Leader || self.log.committed < self.term_start {
            return;
        }
        while let Some((from, ctx)) = self.waiting_reads.pop_front() {
            self.leader_read(from, ctx);
        }
    }

    /* ---- reads ---- */

    fn leader_read(&mut self, from: NodeId, ctx: u64) {
        if self.log.committed < self.term_start {
            /* Not knowing yet how far the log is committed: after this
             * term's first commit. */
            if self.waiting_reads.len() < MAX_PENDING_READS {
                self.waiting_reads.push_back((from, ctx));
            } else {
                self.refuse_read(from, ctx);
            }
            return;
        }
        let index = self.log.committed;
        if self.membership.voters().len() == 1 && self.membership.is_voter(self.id) {
            self.release_read(from, ctx, index);
            return;
        }
        if self.reads.len() >= MAX_PENDING_READS {
            self.refuse_read(from, ctx);
            return;
        }
        /* A round of heartbeats a majority answers proves no other leader
         * was elected meanwhile: the read is as new as `index`. */
        self.next_round += 1;
        let round = self.next_round;
        let mut acks = BTreeSet::new();
        acks.insert(self.id);
        self.reads.push_back(ReadRound { round, index, from, ctx, acks });
        self.broadcast_heartbeat(round);
    }

    fn ack_read(&mut self, from: NodeId, round: u64) {
        if !self.membership.is_voter(from) {
            return;
        }
        let Some(at) = self.reads.iter().position(|r| r.round == round) else { return };
        let quorum = self.membership.quorum();
        let confirmed = {
            let r = &mut self.reads[at];
            r.acks.insert(from);
            r.acks.iter().filter(|id| self.membership.is_voter(**id)).count() >= quorum
        };
        if confirmed {
            /* And every round asked before it: the leadership was confirmed
             * after they were asked. */
            let done: Vec<ReadRound> = self.reads.drain(..=at).collect();
            for r in done {
                self.release_read(r.from, r.ctx, r.index);
            }
        }
    }

    fn release_read(&mut self, from: NodeId, ctx: u64, index: u64) {
        if from == self.id {
            self.push_read_state(ReadState { index, ctx });
        } else {
            self.send(from, Body::ReadIndexResp { ctx, index, ok: true });
        }
    }

    fn refuse_read(&mut self, from: NodeId, ctx: u64) {
        self.stats.reads_refused += 1;
        if from != self.id {
            self.send(from, Body::ReadIndexResp { ctx, index: 0, ok: false });
        }
    }

    /* ---- membership ---- */

    fn last_change_index(&self) -> u64 {
        self.changes.last().map_or(0, |(i, _)| *i)
    }

    /// The change in the entry at `index` made: in force from now on.
    fn apply_change_entry(&mut self, index: u64, data: &[u8]) {
        let next = match wire::decode_change(data) {
            Ok(cc) => self.membership.changed(cc).unwrap_or_else(|_| self.membership.clone()),
            Err(_) => self.membership.clone(),
        };
        self.changes.push(index, next.clone());
        self.set_membership(next);
    }

    /// The changes from `index` on are gone from the log: the membership
    /// goes back to the one before them.
    fn truncate_changes(&mut self, index: u64) {
        if self.changes.last().is_some_and(|(i, _)| *i >= index) {
            while self.changes.last().is_some_and(|(i, _)| *i >= index) {
                self.changes.pop_back();
            }
            let m = self.changes.last().map_or_else(|| self.base.clone(), |(_, m)| m.clone());
            self.set_membership(m);
        }
    }

    fn set_membership(&mut self, m: Membership) {
        self.membership = m;
        if self.role == Role::Leader {
            let last = self.log.last_index();
            let members: Vec<NodeId> = self.membership.members().collect();
            for id in &members {
                if !self.progress.contains_key(id) {
                    let pr = Progress::new(last + 1, self.cfg.max_inflight_msgs, self.cfg.max_inflight_bytes);
                    self.progress.insert(*id, pr);
                }
            }
            self.progress.retain(|id, _| members.contains(id));
        }
    }

    /// The changes the log no longer has before its first entry, folded
    /// into its base.
    fn fold_changes(&mut self) {
        let first = self.log.first_index();
        while self.changes.first().is_some_and(|(i, _)| *i < first) {
            if let Some((_, m)) = self.changes.pop_front() {
                self.base = m;
            }
        }
    }

    /* ---- ready ---- */

    /// Whether there is anything for the caller to do.
    pub fn has_ready(&self) -> bool {
        self.soft_state() != self.prev_soft
            || self.hard_state() != self.prev_hard
            || !self.msgs.is_empty()
            || !self.read_states.is_empty()
            || self.log.unstable_snapshot().is_some()
            || !self.log.unstable_entries().is_empty()
            || self.log.has_next_committed()
    }

    /// What there is to do. Call `advance` once it is done, before the next
    /// `ready` and before stepping the node again.
    pub fn ready(&mut self) -> Ready {
        let soft = self.soft_state();
        let hard = self.hard_state();
        let rd = Ready {
            soft_state: if soft != self.prev_soft { Some(soft) } else { None },
            hard_state: if hard != self.prev_hard { Some(hard) } else { None },
            snapshot: self.log.unstable_snapshot().cloned(),
            entries: self.log.unstable_entries().to_vec(),
            messages: mem::take(&mut self.msgs),
            committed: self.log.next_committed(self.cfg.max_apply_bytes, wire::MAX_ENTRIES),
            read_states: mem::take(&mut self.read_states),
        };
        self.prev_soft = soft;
        self.handed = Some(Handed {
            hard,
            entry: rd.entries.last().map(|e| (e.index, e.term)),
            snapshot: rd.snapshot.as_ref().map(|s| s.meta.index),
            applied: rd.committed.last().map(|e| e.index),
        });
        rd
    }

    /// The last `Ready` done: written, sent, applied.
    pub fn advance(&mut self) {
        let Some(h) = self.handed.take() else { return };
        self.prev_hard = h.hard;
        if let Some(index) = h.snapshot {
            self.log.stable_snap_to(index);
            self.log.applied_to(index);
        }
        if let Some((index, term)) = h.entry {
            self.log.stable_to(index, term);
        }
        if let Some(index) = h.applied {
            self.log.applied_to(index);
        }
        self.fold_changes();
        if self.role == Role::Leader {
            /* What is on the leader's own disk counts toward a majority, and
             * nothing before. */
            let stable = self.log.stable_last_index();
            let me = self.id;
            if let Some(pr) = self.progress.get_mut(&me) {
                pr.maybe_update(stable);
            }
            if self.maybe_commit() {
                self.on_committed();
                self.broadcast_append();
            }
        }
    }
}
