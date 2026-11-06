//! A replicated state machine: a member of a group ([`RawNode`]) over a store
//! of its log ([`LogStore`]: [`MemStorage`], or the log on disk,
//! [`DiskLog`]) and the user's [`StateMachine`], with what every user of
//! Raft needs besides -- clients' sessions, so that a command retried is
//! applied once; reads, linearizable and at an index; and snapshots taken
//! as the log grows -- written once here rather than in each.
//!
//! Still no clock and no network. The caller hands it ticks, the messages
//! that arrive and its clients' requests, and calls [`Rsm::process`], which
//! makes durable what the node says -- through the store, the one thing
//! here that does I/O, over the caller's files -- applies what is
//! committed, serves the reads that may be served, takes the next steps of
//! a snapshot, and hands back the messages to send and the [`Event`]s to
//! answer clients with.
//!
//! **Sessions** are the dissertation's (Ongaro 2014, 6.3). A client
//! registers -- an entry, whose index is its id from then on -- and gives
//! each command a sequence number, one command at a time. A command
//! retried after a timeout or a leader's change, under the number its
//! session last applied, is answered from the session and not applied
//! again. The sessions are part of the replicated state: they go into the
//! snapshot, and are evicted least recently used -- in the log's order, so
//! every member evicts the same -- past [`MAX_SESSIONS`].
//!
//! **Reads**: [`Rsm::read`] is linearizable -- confirmed by the leader's
//! ReadIndex, served once applied as far as it confirms, failed back to
//! the caller if the leadership changes before. [`Rsm::read_at`] serves a
//! read on any member once it has applied as far as an index its client
//! names: its own last write's, say.
//!
//! **Snapshots** are begun once the entries applied since the last are as
//! large as the state -- so that a byte of log costs at most a byte of
//! snapshot -- with a floor. The state is frozen at the index applied then
//! ([`StateMachine::freeze`]), written a few pieces a call while applying
//! goes on, and handed to the store, which compacts the log.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::log::{DiskLog, Files, LogError};
use crate::node::{Config, Error, RawNode, Role, SoftState};
use crate::storage::{MemStorage, Storage, StorageError};
use crate::types::{
    ConfChange, ConfState, Entry, EntryKind, HardState, Message, NodeId, Snapshot, SnapshotBuilder, SnapshotData,
    PIECE_MAX,
};

/// The sessions kept at most: the least recently used goes for a new one.
pub const MAX_SESSIONS: usize = 1024;
/// The longest answer a session keeps for a retry: one longer is answered
/// once, and a retry of it is told it was applied and its answer is lost.
pub const MAX_ANSWER: usize = 1024;
/// Reads waiting, asked and not yet served, at most.
pub const MAX_READS: usize = 4096;
/// A snapshot is not taken before the log applied since the last is this
/// long, however small the state.
pub const SNAPSHOT_FLOOR: u64 = 1 << 20;
/// A snapshot's pieces written in one `process`.
const PIECES_A_CALL: usize = 4;
/// What an entry costs besides its data, counting toward a snapshot.
const ENTRY_OVERHEAD: u64 = 32;

const RSM_MAGIC: [u8; 4] = *b"RSM1";
const KIND_REGISTER: u8 = 1;
const KIND_COMMAND: u8 = 2;
const KIND_UNREGISTER: u8 = 3;

/// What a store of the log does besides being read: makes durable what a
/// `Ready` hands it.
pub trait LogStore: Storage {
    fn append(&mut self, entries: &[Entry]) -> Result<(), StoreError>;
    fn set_hard_state(&mut self, hs: HardState) -> Result<(), StoreError>;
    /// What was appended and set, on the medium.
    fn sync(&mut self) -> Result<(), StoreError>;
    /// A leader's snapshot made the whole of the log, durably.
    fn apply_snapshot(&mut self, snap: Snapshot) -> Result<(), StoreError>;
    /// The state as of `index` made the newest snapshot, durably, and the
    /// log up to it compacted.
    fn create_snapshot(&mut self, index: u64, conf: ConfState, data: SnapshotData) -> Result<(), StoreError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreError {
    Storage(StorageError),
    Log(LogError),
}

impl LogStore for MemStorage {
    fn append(&mut self, entries: &[Entry]) -> Result<(), StoreError> {
        MemStorage::append(self, entries).map_err(StoreError::Storage)
    }

    fn set_hard_state(&mut self, hs: HardState) -> Result<(), StoreError> {
        MemStorage::set_hard_state(self, hs);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), StoreError> {
        Ok(())
    }

    fn apply_snapshot(&mut self, snap: Snapshot) -> Result<(), StoreError> {
        MemStorage::apply_snapshot(self, snap).map_err(StoreError::Storage)
    }

    fn create_snapshot(&mut self, index: u64, conf: ConfState, data: SnapshotData) -> Result<(), StoreError> {
        MemStorage::create_snapshot(self, index, conf, data).map_err(StoreError::Storage)
    }
}

impl<F: Files> LogStore for DiskLog<F> {
    fn append(&mut self, entries: &[Entry]) -> Result<(), StoreError> {
        DiskLog::append(self, entries).map_err(StoreError::Log)
    }

    fn set_hard_state(&mut self, hs: HardState) -> Result<(), StoreError> {
        DiskLog::set_hard_state(self, hs).map_err(StoreError::Log)
    }

    fn sync(&mut self) -> Result<(), StoreError> {
        DiskLog::sync(self).map_err(StoreError::Log)
    }

    fn apply_snapshot(&mut self, snap: Snapshot) -> Result<(), StoreError> {
        DiskLog::apply_snapshot(self, snap).map_err(StoreError::Log)
    }

    fn create_snapshot(&mut self, index: u64, conf: ConfState, data: SnapshotData) -> Result<(), StoreError> {
        DiskLog::create_snapshot(self, index, conf, data).map_err(StoreError::Log)
    }
}

/// The user's state: what the group's log is the history of.
pub trait StateMachine {
    /// A view of the state as it was frozen, which applying does not change.
    type Frozen: Frozen;

    /// A command committed at `index`, applied: the answer its client gets.
    /// The same on every member, from the same state and command: nothing
    /// in it may depend on anything else -- a clock, a member's own id.
    fn apply(&mut self, index: u64, command: &[u8]) -> Vec<u8>;
    /// A read of the state as it is: the answer its client gets.
    fn query(&self, query: &[u8]) -> Vec<u8>;
    /// About how many bytes a snapshot of the state is: when to take one.
    fn size(&self) -> u64;
    /// The state as it is now, held as it is while applying goes on --
    /// copy-on-write, so that it costs little to make.
    fn freeze(&self) -> Self::Frozen;
    /// The state made the one a snapshot's bytes say -- what a `Frozen`
    /// wrote, from `r` on to its end. An error where they say nothing it
    /// could have written: the member stops.
    fn restore(&mut self, r: &mut SnapshotReader<'_>) -> Result<(), RestoreError>;
}

/// A frozen state, written out as a snapshot's bytes a call at a time.
pub trait Frozen {
    /// The snapshot's next bytes, at most `max` of them, after `out`'s:
    /// none once the whole of it is out.
    fn write(&mut self, max: usize, out: &mut Vec<u8>);
}

/// Bytes a state machine could not have written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestoreError(pub &'static str);

/// A snapshot's bytes read in order, across its pieces.
pub struct SnapshotReader<'a> {
    data: &'a SnapshotData,
    at: u64,
}

impl<'a> SnapshotReader<'a> {
    pub fn new(data: &'a SnapshotData) -> SnapshotReader<'a> {
        SnapshotReader { data, at: 0 }
    }

    /// Bytes not yet read.
    pub fn remaining(&self) -> u64 {
        self.data.len() - self.at
    }

    /// Up to `buf.len()` bytes: how many -- fewer only at the end.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let mut done = 0;
        while done < buf.len() {
            let got = self.data.read(self.at, buf.len() - done);
            if got.is_empty() {
                break;
            }
            buf[done..done + got.len()].copy_from_slice(got);
            done += got.len();
            self.at += got.len() as u64;
        }
        done
    }

    /// `buf` filled: false, with the reader past what there was, where
    /// there is not that much.
    pub fn read_exact(&mut self, buf: &mut [u8]) -> bool {
        self.read(buf) == buf.len()
    }

    pub fn u64(&mut self) -> Option<u64> {
        let mut b = [0u8; 8];
        self.read_exact(&mut b).then(|| u64::from_le_bytes(b))
    }

    pub fn u32(&mut self) -> Option<u32> {
        let mut b = [0u8; 4];
        self.read_exact(&mut b).then(|| u32::from_le_bytes(b))
    }
}

/// What the caller is to answer its clients with -- or ignore, where the
/// client is not its own: every member applies every entry, and says so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A session made for the request `request` that member `origin` was
    /// asked: `client` is the id its commands carry.
    Registered { origin: NodeId, request: u64, client: u64 },
    /// Client `client`'s command `seq`, applied at `index` -- or a retry of
    /// one applied before, answered from the session.
    Applied { client: u64, seq: u64, index: u64, answer: Vec<u8> },
    /// A command of a session that is gone -- evicted, ended, never made:
    /// not applied. The client registers again.
    Expired { client: u64, seq: u64 },
    /// A command under the number its session last applied: not applied,
    /// and its answer no longer kept.
    Stale { client: u64, seq: u64 },
    /// A retry of a command applied before whose answer was too long to
    /// keep: applied, once, and its answer lost.
    Lost { client: u64, seq: u64 },
    /// A read served.
    Read { request: u64, answer: Vec<u8> },
    /// A read that will not be served here: the leadership changed before
    /// it was confirmed. Asked again, of the leader there is now.
    ReadFailed { request: u64 },
}

/// What `process` hands back.
#[derive(Clone, Debug, Default)]
pub struct Output {
    /// To send, now that what they answer for is durable.
    pub messages: Vec<Message>,
    pub events: Vec<Event>,
    /// Whom the member follows and what it is, where that changed.
    pub soft_state: Option<SoftState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RsmError {
    /// The node's: not the leader (and whom it follows), busy, too large.
    Node(Error),
    /// The store failed: the member is stopped, and starts again over it.
    Store(StoreError),
    /// A snapshot the state machine, or this, could not read.
    Restore(RestoreError),
    /// A committed entry this cannot read: stopped, as every member would.
    Corrupt(&'static str),
    /// As many reads waiting as are kept.
    Busy,
    /// An earlier failure stopped the member.
    Stopped,
}

/// A client's session.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Session {
    /// The number of the last command applied; 0 for none.
    last: u64,
    /// The entry it was last used at: who goes first when there is no room.
    used: u64,
    /// The last command's answer, where it was short enough to keep.
    answer: Option<Vec<u8>>,
}

/// A snapshot being taken.
struct Taking<Z> {
    index: u64,
    conf: ConfState,
    frozen: Z,
    out: SnapshotBuilder,
}

/// A committed entry's payload, as this writes it.
enum Envelope<'a> {
    Register { origin: NodeId, request: u64 },
    Command { client: u64, seq: u64, command: &'a [u8] },
    Unregister { client: u64 },
}

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn decode(data: &[u8]) -> Option<Envelope<'_>> {
    let (&kind, rest) = data.split_first()?;
    match kind {
        KIND_REGISTER if rest.len() == 16 => Some(Envelope::Register { origin: le64(&rest[..8]), request: le64(&rest[8..]) }),
        KIND_COMMAND if rest.len() >= 16 => {
            Some(Envelope::Command { client: le64(&rest[..8]), seq: le64(&rest[8..16]), command: &rest[16..] })
        }
        KIND_UNREGISTER if rest.len() == 8 => Some(Envelope::Unregister { client: le64(rest) }),
        _ => None,
    }
}

pub struct Rsm<S: LogStore, M: StateMachine> {
    node: RawNode<S>,
    sm: M,
    sessions: BTreeMap<u64, Session>,
    /// The last entry applied.
    applied: u64,
    /// What the entries applied since the newest snapshot came to.
    log_bytes: u64,
    /// The least the log comes to before a snapshot is taken.
    floor: u64,
    /// Reads asked of the leader and not yet confirmed: by the context the
    /// node is handed, the caller's request and the query.
    asked: BTreeMap<u64, (u64, Vec<u8>)>,
    /// Reads confirmed, or asked at an index, waiting for the applied index
    /// to come as far: by that index and their order of arrival.
    waiting: BTreeMap<(u64, u64), (u64, Vec<u8>)>,
    next_ctx: u64,
    /// This start's own number, which every read's context the node is
    /// handed carries: an answer to a read asked before the member was
    /// started again -- a leader's, on its way through the restart -- names
    /// a context no read of this start's has.
    nonce: u64,
    taking: Option<Taking<M::Frozen>>,
    soft: SoftState,
    stopped: bool,
}

impl<S: LogStore, M: StateMachine> Rsm<S, M> {
    /// A member over what `store` holds -- its snapshot restored into `sm`,
    /// which is to be the state machine as new; what is committed past it
    /// is applied again as the node hands it out. `nonce` is this start's
    /// own: random, or a count of starts kept durably -- never the same as
    /// an earlier start's of this member, whose reads' answers may still be
    /// on their way. Starting from 1 every time, a read asked after a
    /// restart was answered with the index a read before it had been, and
    /// served state older than a write already acknowledged (fuzz/raft's
    /// rsm target, seed 63606).
    pub fn new(cfg: Config, store: S, sm: M, nonce: u64) -> Result<Rsm<S, M>, RsmError> {
        let snap = store.snapshot().map_err(|e| RsmError::Node(Error::Storage(e)))?;
        let node = RawNode::new(cfg, store).map_err(RsmError::Node)?;
        let soft = SoftState { leader: node.leader(), role: node.role() };
        let mut rsm = Rsm {
            node,
            sm,
            sessions: BTreeMap::new(),
            applied: snap.meta.index,
            log_bytes: 0,
            floor: SNAPSHOT_FLOOR,
            asked: BTreeMap::new(),
            waiting: BTreeMap::new(),
            next_ctx: 1,
            nonce,
            taking: None,
            soft,
            stopped: false,
        };
        if snap.meta.index > 0 {
            rsm.restore(&snap)?;
        }
        Ok(rsm)
    }

    /// The least the log applied since the last snapshot comes to before
    /// the next is taken: [`SNAPSHOT_FLOOR`] unless set.
    pub fn set_snapshot_floor(&mut self, bytes: u64) {
        self.floor = bytes;
    }

    pub fn node(&self) -> &RawNode<S> {
        &self.node
    }

    pub fn state(&self) -> &M {
        &self.sm
    }

    pub fn leader(&self) -> NodeId {
        self.node.leader()
    }

    pub fn role(&self) -> Role {
        self.node.role()
    }

    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// The sessions there are.
    pub fn sessions(&self) -> usize {
        self.sessions.len()
    }

    /// Whether a snapshot is being taken.
    pub fn taking_snapshot(&self) -> bool {
        self.taking.is_some()
    }

    /// The store back, the member gone: to start one again over it, as
    /// after a crash.
    pub fn into_store(self) -> S {
        self.node.into_storage()
    }

    fn check(&self) -> Result<(), RsmError> {
        if self.stopped { Err(RsmError::Stopped) } else { Ok(()) }
    }

    fn stop(&mut self, e: RsmError) -> RsmError {
        self.stopped = true;
        e
    }

    pub fn tick(&mut self) {
        if !self.stopped {
            self.node.tick();
        }
    }

    /// A message from a member of the group.
    pub fn step(&mut self, m: Message) -> Result<(), RsmError> {
        self.check()?;
        self.node.step(m).map_err(RsmError::Node)
    }

    /* ---- what clients ask ---- */

    /// A session for a client, asked of this member as request `request`:
    /// `Registered` once it is applied. The leader's alone to do.
    pub fn register(&mut self, request: u64) -> Result<u64, RsmError> {
        self.check()?;
        let mut data = Vec::with_capacity(17);
        data.push(KIND_REGISTER);
        data.extend_from_slice(&self.node.id().to_le_bytes());
        data.extend_from_slice(&request.to_le_bytes());
        self.node.propose(data).map_err(RsmError::Node)
    }

    /// Client `client`'s command `seq`: `Applied` -- or `Expired`,
    /// `Stale`, `Lost` -- once it is committed. A retry is the same `seq`
    /// again, of whichever member leads by then. The leader's alone to do.
    pub fn submit(&mut self, client: u64, seq: u64, command: &[u8]) -> Result<u64, RsmError> {
        self.check()?;
        let mut data = Vec::with_capacity(17 + command.len());
        data.push(KIND_COMMAND);
        data.extend_from_slice(&client.to_le_bytes());
        data.extend_from_slice(&seq.to_le_bytes());
        data.extend_from_slice(command);
        self.node.propose(data).map_err(RsmError::Node)
    }

    /// Client `client`'s session ended.
    pub fn unregister(&mut self, client: u64) -> Result<u64, RsmError> {
        self.check()?;
        let mut data = Vec::with_capacity(9);
        data.push(KIND_UNREGISTER);
        data.extend_from_slice(&client.to_le_bytes());
        self.node.propose(data).map_err(RsmError::Node)
    }

    /// A linearizable read, asked as request `request`: `Read` once the
    /// leader has confirmed it and this member has applied as far, or
    /// `ReadFailed` if the leadership changes before.
    pub fn read(&mut self, request: u64, query: Vec<u8>) -> Result<(), RsmError> {
        self.check()?;
        if self.asked.len() + self.waiting.len() >= MAX_READS {
            return Err(RsmError::Busy);
        }
        let ctx = self.nonce.wrapping_add(self.next_ctx);
        self.next_ctx += 1;
        self.node.read_index(ctx).map_err(RsmError::Node)?;
        self.asked.insert(ctx, (request, query));
        Ok(())
    }

    /// A read at an index: `Read` once this member has applied as far --
    /// no older than what was committed at `index`, on any member.
    pub fn read_at(&mut self, request: u64, index: u64, query: Vec<u8>) -> Result<(), RsmError> {
        self.check()?;
        if self.asked.len() + self.waiting.len() >= MAX_READS {
            return Err(RsmError::Busy);
        }
        let k = self.next_ctx;
        self.next_ctx += 1;
        self.waiting.insert((index, k), (request, query));
        Ok(())
    }

    pub fn propose_change(&mut self, cc: ConfChange) -> Result<u64, RsmError> {
        self.check()?;
        self.node.propose_change(cc).map_err(RsmError::Node)
    }

    pub fn transfer_leader(&mut self, to: NodeId) -> Result<(), RsmError> {
        self.check()?;
        self.node.transfer_leader(to).map_err(RsmError::Node)
    }

    pub fn campaign(&mut self) -> Result<(), RsmError> {
        self.check()?;
        self.node.campaign().map_err(RsmError::Node)
    }

    /* ---- the work ---- */

    /// Everything there is to do now, done: what the node says written and
    /// synced, what is committed applied, the reads that may be served
    /// served, a snapshot's next pieces written. What to send and whom to
    /// answer, back.
    pub fn process(&mut self) -> Result<Output, RsmError> {
        self.check()?;
        let mut out = Output::default();
        if let Some(e) = self.node.fault() {
            return Err(self.stop(RsmError::Node(Error::Storage(e))));
        }
        if self.node.has_ready() {
            let rd = self.node.ready();
            if let Some(snap) = rd.snapshot.clone() {
                /* A leader's: the log is it now, and the state too */
                self.taking = None;
                self.node.storage_mut().apply_snapshot(snap.clone()).map_err(|e| self.stop(RsmError::Store(e)))?;
                self.restore(&snap)?;
            }
            self.node.storage_mut().append(&rd.entries).map_err(|e| self.stop(RsmError::Store(e)))?;
            if let Some(hs) = rd.hard_state {
                self.node.storage_mut().set_hard_state(hs).map_err(|e| self.stop(RsmError::Store(e)))?;
            }
            self.node.storage_mut().sync().map_err(|e| self.stop(RsmError::Store(e)))?;
            out.messages = rd.messages;
            for e in rd.committed.iter() {
                self.apply(e, &mut out)?;
            }
            for rs in rd.read_states {
                if let Some(asked) = self.asked.remove(&rs.ctx) {
                    let k = self.next_ctx;
                    self.next_ctx += 1;
                    self.waiting.insert((rs.index, k), asked);
                }
            }
            if let Some(ss) = rd.soft_state {
                if ss.leader != self.soft.leader || ss.role != self.soft.role {
                    /* A read not yet confirmed never is once the leadership
                     * changed: the node let it go. Asked again, of the
                     * leader there is now. */
                    for (_, (request, _)) in core::mem::take(&mut self.asked) {
                        out.events.push(Event::ReadFailed { request });
                    }
                }
                self.soft = ss;
                out.soft_state = Some(ss);
            }
            self.node.advance();
        }
        self.serve_reads(&mut out);
        self.snapshot_step()?;
        Ok(out)
    }

    /// A committed entry applied.
    fn apply(&mut self, e: &Entry, out: &mut Output) -> Result<(), RsmError> {
        if e.index <= self.applied {
            return Ok(());
        }
        self.applied = e.index;
        self.log_bytes = self.log_bytes.saturating_add(e.data.len() as u64 + ENTRY_OVERHEAD);
        if e.kind != EntryKind::Normal {
            /* The leader's term begun, or a membership change the node has
             * taken already: nothing of the state machine's */
            return Ok(());
        }
        let Some(env) = decode(&e.data) else {
            return Err(self.stop(RsmError::Corrupt("a committed entry that is no request")));
        };
        match env {
            Envelope::Register { origin, request } => {
                if self.sessions.len() >= MAX_SESSIONS {
                    if let Some(oldest) = self.sessions.iter().min_by_key(|(c, s)| (s.used, **c)).map(|(c, _)| *c) {
                        self.sessions.remove(&oldest);
                    }
                }
                self.sessions.insert(e.index, Session { last: 0, used: e.index, answer: None });
                out.events.push(Event::Registered { origin, request, client: e.index });
            }
            Envelope::Command { client, seq, command } => match self.sessions.get_mut(&client) {
                None => out.events.push(Event::Expired { client, seq }),
                Some(s) if seq < s.last => out.events.push(Event::Stale { client, seq }),
                Some(s) if seq == s.last => {
                    s.used = e.index;
                    out.events.push(match &s.answer {
                        Some(a) => Event::Applied { client, seq, index: e.index, answer: a.clone() },
                        None => Event::Lost { client, seq },
                    });
                }
                Some(s) => {
                    let answer = self.sm.apply(e.index, command);
                    s.last = seq;
                    s.used = e.index;
                    s.answer = (answer.len() <= MAX_ANSWER).then(|| answer.clone());
                    out.events.push(Event::Applied { client, seq, index: e.index, answer });
                }
            },
            Envelope::Unregister { client } => {
                self.sessions.remove(&client);
            }
        }
        Ok(())
    }

    /// The reads whose index is applied, served in order.
    fn serve_reads(&mut self, out: &mut Output) {
        while let Some((&(index, _), _)) = self.waiting.first_key_value() {
            if index > self.applied {
                break;
            }
            let Some((_, (request, query))) = self.waiting.pop_first() else { break };
            out.events.push(Event::Read { request, answer: self.sm.query(&query) });
        }
    }

    /// The sessions as a snapshot carries them, before the state machine's
    /// bytes.
    fn write_sessions(&self, b: &mut SnapshotBuilder) -> bool {
        let mut head = Vec::with_capacity(8);
        head.extend_from_slice(&RSM_MAGIC);
        head.extend_from_slice(&(self.sessions.len() as u32).to_le_bytes());
        if !b.push(&head) {
            return false;
        }
        for (&client, s) in self.sessions.iter() {
            let mut rec = Vec::with_capacity(32 + s.answer.as_ref().map_or(0, |a| a.len()));
            rec.extend_from_slice(&client.to_le_bytes());
            rec.extend_from_slice(&s.last.to_le_bytes());
            rec.extend_from_slice(&s.used.to_le_bytes());
            match &s.answer {
                Some(a) => {
                    rec.extend_from_slice(&(a.len() as u32).to_le_bytes());
                    rec.extend_from_slice(a);
                }
                None => rec.extend_from_slice(&u32::MAX.to_le_bytes()),
            }
            if !b.push(&rec) {
                return false;
            }
        }
        true
    }

    /// A snapshot begun when the log calls for one, its next pieces
    /// written, and handed to the store once whole.
    fn snapshot_step(&mut self) -> Result<(), RsmError> {
        if self.taking.is_none() {
            let state = self.sm.size().saturating_add(self.sessions.len() as u64 * 32);
            let at = self.node.storage().snapshot().map(|s| s.meta.index).unwrap_or(0);
            if self.log_bytes < state.max(self.floor) || self.applied <= at {
                return Ok(());
            }
            let index = self.applied;
            let Some(conf) = self.node.conf_state_at(index) else { return Ok(()) };
            let mut out = SnapshotBuilder::new();
            if !self.write_sessions(&mut out) {
                return Ok(());
            }
            self.taking = Some(Taking { index, conf, frozen: self.sm.freeze(), out });
            self.log_bytes = 0;
        }
        let Some(t) = self.taking.as_mut() else { return Ok(()) };
        for _ in 0..PIECES_A_CALL {
            let mut piece = Vec::new();
            t.frozen.write(PIECE_MAX, &mut piece);
            if piece.is_empty() {
                let Some(t) = self.taking.take() else { return Ok(()) };
                return match self.node.storage_mut().create_snapshot(t.index, t.conf, t.out.finish()) {
                    Ok(()) => Ok(()),
                    /* A leader's snapshot came past it meanwhile */
                    Err(StoreError::Storage(StorageError::SnapshotOutOfDate))
                    | Err(StoreError::Log(LogError::Refused(StorageError::SnapshotOutOfDate))) => Ok(()),
                    Err(e) => Err(self.stop(RsmError::Store(e))),
                };
            }
            if !t.out.push(&piece) {
                /* Past the largest snapshot there is: given up */
                self.taking = None;
                return Ok(());
            }
        }
        Ok(())
    }

    /// The sessions and the state machine made what a snapshot says.
    fn restore(&mut self, snap: &Snapshot) -> Result<(), RsmError> {
        let mut r = SnapshotReader::new(&snap.data);
        let mut magic = [0u8; 4];
        if !r.read_exact(&mut magic) || magic != RSM_MAGIC {
            return Err(self.stop(RsmError::Restore(RestoreError("a snapshot this did not write"))));
        }
        let count = r.u32().map(|c| c as usize).unwrap_or(usize::MAX);
        if count > MAX_SESSIONS {
            return Err(self.stop(RsmError::Restore(RestoreError("more sessions than are kept"))));
        }
        let mut sessions = BTreeMap::new();
        for _ in 0..count {
            let (Some(client), Some(last), Some(used), Some(len)) = (r.u64(), r.u64(), r.u64(), r.u32()) else {
                return Err(self.stop(RsmError::Restore(RestoreError("a session cut short"))));
            };
            let answer = if len == u32::MAX {
                None
            } else if (len as usize) <= MAX_ANSWER {
                let mut a = alloc::vec![0u8; len as usize];
                if !r.read_exact(&mut a) {
                    return Err(self.stop(RsmError::Restore(RestoreError("a session cut short"))));
                }
                Some(a)
            } else {
                return Err(self.stop(RsmError::Restore(RestoreError("a session's answer past what is kept"))));
            };
            sessions.insert(client, Session { last, used, answer });
        }
        if let Err(e) = self.sm.restore(&mut r) {
            return Err(self.stop(RsmError::Restore(e)));
        }
        self.sessions = sessions;
        self.applied = snap.meta.index;
        self.log_bytes = 0;
        Ok(())
    }
}
