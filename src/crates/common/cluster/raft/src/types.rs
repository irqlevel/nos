//! What a group's members say to one another and keep: entries, the state
//! that must survive a crash, the membership, snapshots and messages.

use alloc::sync::Arc;
use alloc::vec::Vec;

/// A member of a group, as its users number them: never 0, which is
/// nobody ([`NONE`]).
pub type NodeId = u64;

/// Nobody: no vote cast, no leader known.
pub const NONE: NodeId = 0;

/// The most bytes in one piece of a snapshot's data: what one allocation
/// holds and what one message carries at most.
pub const PIECE_MAX: usize = 64 * 1024;

/// The largest snapshot there is: 16384 pieces, whose list is some 384 KiB,
/// a block of the kernel's heap.
pub const MAX_SNAPSHOT_BYTES: u64 = 1 << 30;

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    /// The caller's: bytes the crate does not read.
    Normal = 0,
    /// A membership change, its [`ConfChange`] as [`crate::wire`] writes it.
    Change = 1,
    /// The entry a leader appends as its term starts, so that it has an
    /// entry of its own term to commit by: nothing for the caller to apply.
    Noop = 2,
}

/// One entry of the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub term: u64,
    pub index: u64,
    pub kind: EntryKind,
    pub data: Vec<u8>,
}

impl Entry {
    /// The bytes it counts for in a message's and an apply's limits.
    pub fn size(&self) -> u64 {
        self.data.len() as u64
    }
}

/// What a member must have on its disk before it says anything: its term,
/// the member it voted for in that term, and how far it knows the log to be
/// committed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HardState {
    pub term: u64,
    pub vote: NodeId,
    pub commit: u64,
}

/// The group's members: the voters, whose majority commits and elects, and
/// the learners, which hold everything and vote on nothing. Each list sorted,
/// with no member in both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfState {
    pub voters: Vec<NodeId>,
    pub learners: Vec<NodeId>,
}

/// One server added or taken away -- the only membership change there is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// A voter: a new member, or a learner promoted.
    AddVoter = 0,
    /// A learner: a new member, or a voter that stops voting.
    AddLearner = 1,
    /// Out of the group.
    Remove = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfChange {
    pub kind: ChangeKind,
    pub node: NodeId,
}

/// Where a snapshot stands in the log: the last entry it includes, and the
/// membership as of that entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotMeta {
    pub index: u64,
    pub term: u64,
    pub conf: ConfState,
}

/// A snapshot's bytes -- the caller's state as of its index -- in pieces
/// of [`PIECE_MAX`], each piece full but the last: never one allocation
/// larger than a piece, and shared, so that a leader sending it to two
/// followers holds it once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotData {
    pieces: Arc<Vec<Vec<u8>>>,
    len: u64,
}

impl SnapshotData {
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn pieces(&self) -> &[Vec<u8>] {
        &self.pieces
    }

    /// Up to `max` bytes from `offset`, within the one piece that holds
    /// `offset`: what a message carries. Empty at or past the end.
    pub fn read(&self, offset: u64, max: usize) -> &[u8] {
        if offset >= self.len || max == 0 {
            return &[];
        }
        /* Every piece but the last is full, so a piece is found by division. */
        let Ok(at) = usize::try_from(offset) else { return &[] };
        let Some(piece) = self.pieces.get(at / PIECE_MAX) else { return &[] };
        let from = at % PIECE_MAX;
        let to = piece.len().min(from.saturating_add(max));
        piece.get(from..to).unwrap_or(&[])
    }
}

/// Why bytes were not taken into a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    /// Past the largest snapshot it takes: [`MAX_SNAPSHOT_BYTES`], or the
    /// limit it was made with.
    TooLarge,
    /// No memory for a piece: the heap refused it, which in a kernel is no
    /// reason to panic.
    NoMemory,
}

/// A snapshot's bytes as they come: written piece by piece, by whoever
/// makes the state's snapshot or by a follower taking one in. Every piece
/// is one allocation of [`PIECE_MAX`], asked of the heap so that a refusal
/// is an error, not a panic -- a snapshot is the one thing here sized by the
/// user's data, and the kernel's heap gives a size class only so many
/// blocks.
#[derive(Debug)]
pub struct SnapshotBuilder {
    pieces: Vec<Vec<u8>>,
    len: u64,
    limit: u64,
}

impl Default for SnapshotBuilder {
    fn default() -> SnapshotBuilder {
        SnapshotBuilder { pieces: Vec::new(), len: 0, limit: MAX_SNAPSHOT_BYTES }
    }
}

impl SnapshotBuilder {
    pub fn new() -> SnapshotBuilder {
        SnapshotBuilder::default()
    }

    /// One that takes no more than `limit` bytes: what a member may hold.
    pub fn with_limit(limit: u64) -> SnapshotBuilder {
        SnapshotBuilder { limit: limit.min(MAX_SNAPSHOT_BYTES), ..SnapshotBuilder::default() }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `bytes` after what is there: false, and nothing taken, where the
    /// snapshot would grow past its limit or the heap has no room for it.
    #[must_use]
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        self.try_push(bytes).is_ok()
    }

    /// `bytes` after what is there -- or why not, nothing taken: every piece
    /// it needs is allocated before a byte is copied.
    pub fn try_push(&mut self, mut bytes: &[u8]) -> Result<(), PushError> {
        if self.len.saturating_add(bytes.len() as u64) > self.limit {
            return Err(PushError::TooLarge);
        }
        let room = match self.pieces.last() {
            Some(last) if last.len() < PIECE_MAX => PIECE_MAX - last.len(),
            _ => 0,
        };
        let more = bytes.len().saturating_sub(room).div_ceil(PIECE_MAX);
        let mut fresh: Vec<Vec<u8>> = Vec::new();
        fresh.try_reserve_exact(more).map_err(|_| PushError::NoMemory)?;
        for _ in 0..more {
            let mut piece = Vec::new();
            piece.try_reserve_exact(PIECE_MAX).map_err(|_| PushError::NoMemory)?;
            fresh.push(piece);
        }
        self.pieces.try_reserve(more).map_err(|_| PushError::NoMemory)?;
        let n = room.min(bytes.len());
        if n > 0 {
            if let Some(last) = self.pieces.last_mut() {
                last.extend_from_slice(&bytes[..n]);
            }
            self.len += n as u64;
            bytes = &bytes[n..];
        }
        for mut piece in fresh {
            let n = PIECE_MAX.min(bytes.len());
            piece.extend_from_slice(&bytes[..n]);
            self.pieces.push(piece);
            self.len += n as u64;
            bytes = &bytes[n..];
        }
        Ok(())
    }

    pub fn finish(self) -> SnapshotData {
        SnapshotData { pieces: Arc::new(self.pieces), len: self.len }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub meta: SnapshotMeta,
    pub data: SnapshotData,
}

/// One message between two members of a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub from: NodeId,
    pub to: NodeId,
    /// The sender's term -- but a PreVote request's is the term its sender
    /// would stand in, and a PreVote granted answers with that same term.
    pub term: u64,
    pub body: Body,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Asks for a vote -- or with `pre`, whether the sender could win one:
    /// nothing is changed by that, on either side. `transfer`: the leader
    /// asked for this election, and a member that has heard from it takes
    /// the request even so.
    Vote { pre: bool, transfer: bool, last_index: u64, last_term: u64 },
    VoteResp { pre: bool, granted: bool },
    /// Entries to follow the one at `prev_index`, of `prev_term`, and how far
    /// the leader has committed.
    Append { prev_index: u64, prev_term: u64, commit: u64, entries: Vec<Entry> },
    /// Taken: `index` is the follower's last entry that matches. Refused:
    /// `index` is the `prev_index` refused, and the hint where the leader
    /// may look next -- the follower's last entry of a term no later than
    /// the one asked about. Refused with all three 0: the answer to a
    /// request of a term gone by -- an append, a heartbeat, a snapshot's
    /// piece, a vote -- which tells its sender the term there is now.
    AppendResp { reject: bool, index: u64, hint_index: u64, hint_term: u64 },
    /// The leader is there; and how far a follower may take the commit to,
    /// no further than what it is known to hold. `ctx` names a round of
    /// reads it confirms, 0 for none.
    Heartbeat { commit: u64, ctx: u64 },
    HeartbeatResp { ctx: u64 },
    /// A piece of a snapshot: `data` is its bytes from `offset`, of `total`.
    Snapshot { meta: SnapshotMeta, offset: u64, total: u64, data: Vec<u8> },
    /// The bytes of the snapshot at `index` the follower has: the next
    /// piece wanted starts at `next`. `reject`: it will not take it.
    SnapshotResp { index: u64, next: u64, reject: bool },
    /// From the leader handing over: stand now.
    TimeoutNow,
    /// From a follower: a read, to be confirmed by the leader.
    ReadIndex { ctx: u64 },
    /// The read confirmed (`ok`): the follower may serve it once it has
    /// applied `index`.
    ReadIndexResp { ctx: u64, index: u64, ok: bool },
}

impl Body {
    /// Whether only a leader sends it: a member that takes one knows who
    /// leads its term.
    pub fn is_from_leader(&self) -> bool {
        matches!(self, Body::Append { .. } | Body::Heartbeat { .. } | Body::Snapshot { .. } | Body::TimeoutNow
                       | Body::ReadIndexResp { .. })
    }
}
