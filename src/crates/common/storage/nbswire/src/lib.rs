//! What NBS's services say to each other over the message channel
//! (plans/nbs.md; step N7).
//!
//! - **A client and a cs**: a client -- hv's disks from N8, `nbs io` before
//!   it -- sends a `Request` to `nbs.cs@<host>` (tag `TAG_REQUEST`), and is
//!   answered with an `Answer` of the request's id (`TAG_ANSWER`), to the
//!   mailbox it sent from.
//! - **A cs and its host's mds**: the cs asks `nbs.mds` on its own host a
//!   `Query` (`TAG_QUERY`) -- whose cs it is and which devices it has, whether
//!   a request it has not checked yet is the volume's and the chunk's as the
//!   metadata has them, which of its chunks and volumes are gone -- and is
//!   told (`TAG_TOLD`). The mds answers from its applied state, once it has
//!   applied as far as a request's index: before that, `Behind`.
//! - **A client and its host's mds** (N8): the client asks `nbs.mds` on its
//!   own host an `Asking` (`TAG_ASK`) -- a volume attached or detached, its
//!   map from a place on, a replica that did not answer taken out -- and is
//!   told a `Saying` (`TAG_SAID`).
//!
//! Every message is read whole and held to its limits: one that is not a
//! message of these, or has bytes past its end, is none (`None`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod copy;
pub mod mds;
pub mod scrub;
pub mod serve;

use alloc::string::String;
use alloc::vec::Vec;

use codec::{Reader, Writer};
use nbscs::{ChunkRef, Epoch};

/// A client's request to a cs, and the cs's answer: the cs's mailbox's tags.
pub const TAG_REQUEST: u32 = 1;
pub const TAG_ANSWER: u32 = 2;
/// A cs's question to its host's mds, on `nbs.mds`, and the mds's answer:
/// past the tags nbs-mds's member takes for its own (raftmember's
/// `TAG_APP` and the three after it).
pub const TAG_QUERY: u32 = 0x110;
pub const TAG_TOLD: u32 = 0x111;

/// The cs's mailbox's name, on every host that has one.
pub const CS_NAME: &str = "nbs.cs";
/// The mds's -- its member's -- the same on every host of the NBS cluster:
/// what a client of this host's asks (`TAG_ASK`).
pub const MDS_NAME: &str = "nbs.mds";

/// The most a write carries, or a read asks for: a request is one message.
pub const DATA_MAX: u32 = 256 * 1024;
/// The most chunks or volumes a question names, or an answer gives.
pub const LIST_MAX: usize = 4096;
/// The most a list asked for at once gives -- a set's chunks to its joiner,
/// a volume's open chunks to its writer's start, the chunks and volumes the
/// garbage pass asks about: a page of it, the rest asked from the last on.
/// A few in fuzz/nbs's builds, so that every list goes past its first page
/// there (`small_pages`); `LIST_MAX` everywhere else.
#[cfg(not(feature = "small_pages"))]
pub const PAGE: usize = LIST_MAX;
#[cfg(feature = "small_pages")]
pub const PAGE: usize = 3;
const _: () = assert!(PAGE <= LIST_MAX && PAGE <= nbscs::PAGE_OF_CHUNKS);
/// A device's id at most, as the metadata keeps it.
pub const IDENT_MAX: usize = nbsmeta::IDENT_MAX;

/// What a client asks of a cs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op<'a> {
    /// The volume fenced at the request's generation -- that of its
    /// writer's attach, applied at entry `index`, which the cs's mds
    /// vouches for: no other host, and no writer since taken over, fences a
    /// volume (the review of 2026-11: a deleted volume's old writer fenced
    /// itself anew and wrote a snapshot's chunks).
    Fence { index: u64 },
    /// `data` written at `offset` in the chunk, at map place `position` of
    /// the volume, as the metadata had it at entry `index`.
    Write { chunk: ChunkRef, position: u32, index: u64, offset: u32, data: &'a [u8] },
    /// `as_of`: a read-only session's -- the generation a writer would have
    /// been given as it attached (`Said::Attached`); a fence there or past
    /// it is a writer's attached since, and the read is refused as fenced.
    /// 0 for a writer's, whose generation the request carries.
    Read { chunk: ChunkRef, position: u32, index: u64, offset: u32, len: u32, as_of: u64 },
    /// Everything written so far durable.
    Flush,
    /// The chunk closed: every replica of it flushed.
    Close { chunk: ChunkRef, position: u32, index: u64 },
    /// The volume's chunks the cs holds open, for a reconcile, from chunk
    /// number `from` on: a page of them (`PAGE`), the next asked from the
    /// last on until one is short.
    Held { from: u32 },
    /// Whether the cs is there and serving: the leader's look at a cs a client
    /// says did not answer it (N8).
    Ping,
    /// A joiner's asks of a source, an active member of the set it joins
    /// (N9): each names the joiner, and the set's version it knows, which
    /// the source's mds vouches for -- at entry `index` at least.
    /// The set's chunks the source holds from chunk number `from` on.
    Holdings { set: u32, from: u32, joiner: u32, version: u32, index: u64 },
    /// `len` bytes of the chunk at `offset`: the chunk's epoch moved on at
    /// the source before it reads.
    Pull { chunk: ChunkRef, joiner: u32, index: u64, offset: u32, len: u32 },
    /// The CRC-32C of each `nbscs::GRANULE` of the chunk from `offset` for
    /// `len` bytes, its epoch moved on as a pull's.
    Sums { chunk: ChunkRef, joiner: u32, index: u64, offset: u32, len: u32 },
    /// The leader's scrub (N9e): the chunk's record and the sums of all of
    /// it, no epoch moved on.
    Scrub { chunk: ChunkRef },
    /// A copy-up's copy, the volume's writer's (N10): chunk `from`, at map
    /// place `position`, copied into the new chunk `to` of its set -- the
    /// copy-up pending in the metadata at entry `index` at least. Durable
    /// before it is answered; done again, nothing.
    CopyUp { from: ChunkRef, to: ChunkRef, position: u32, index: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    pub id: u64,
    pub volume: u32,
    /// The attach's generation: 0 for a read-only one.
    pub generation: u64,
    pub op: Op<'a>,
}

/// Why a request was refused: the cs's engine's reasons and its host's mds's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Fenced { generation: u64 },
    NotFenced { generation: u64 },
    StaleChunk { incarnation: u32 },
    /// The epoch the cs and its mds know the chunk at.
    Epoch { known: Epoch },
    NoRoom,
    Bad,
    Failed,
    Io,
    TooManyFences,
    /// The mds: no such chunk at that place of that volume's map, at that
    /// incarnation.
    NotTheMap,
    /// The mds: this host's cs is not in the chunk's set -- not an active
    /// member of it, where the request reads, or copies.
    NotHere,
    /// The mds: the generation is not the volume's attach's.
    NotAttached,
    /// The mds has not applied as far as the request's index: asked again.
    Behind { applied: u64 },
    /// No mds on the host, or one in no cluster.
    NoMds,
    /// The cs is not serving yet: its devices not taken in.
    NotReady,
    /// A pull or a sum of a chunk the cs does not hold (N9).
    NotHeld,
    /// The mds: the cs named is not the set's joiner, or not at that
    /// version (N9).
    NotTheJoiner,
}

/// What a cs answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply<'a> {
    /// Fenced: at this generation now.
    Fenced { generation: u64 },
    /// A write or a flush done.
    Done,
    Data(&'a [u8]),
    /// Whether the chunk was open.
    Closed(bool),
    /// The volume's chunks held open: number, incarnation, generation.
    Held(Vec<(u32, u32, u64)>),
    Refused(Refusal),
    /// A set's chunks the source holds (N9): number, incarnation, volume,
    /// the version each was last written at.
    Chunks(Vec<(u32, u32, u32, u32)>),
    /// A sum of each granule asked, and the generation the source's record
    /// of the chunk is open at, if it is: what the joiner keeps open too
    /// (`Store::close_copy`).
    Sums { sums: Vec<u32>, open: Option<u64> },
    /// What the scrub asked of a chunk: whether its record is open, the
    /// version it was last written at, and a sum of each granule.
    Scrub { open: bool, written: u32, sums: Vec<u32> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer<'a> {
    pub id: u64,
    pub reply: Reply<'a>,
}

/// What a cs asks its host's mds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Question {
    /// Whose cs this host's is, and which devices it has.
    Identity,
    /// A request the cs has not checked: the volume's attach at
    /// `generation` (0, read-only), the chunk at map place `position`, this
    /// cs in its set at its epoch -- at entry `index` at least.
    Verify { volume: u32, generation: u64, chunk: ChunkRef, position: u32, index: u64, writing: bool },
    /// Which of these chunks -- number, incarnation -- are gone: freed, or
    /// the number another chunk's now.
    Live { chunks: Vec<(u32, u32)> },
    /// Which of these volumes are gone.
    Volumes { volumes: Vec<u32> },
    /// A joiner's ask of this cs, a source (N9): cs `joiner` the joiner of
    /// set `set` at its version `version`, this cs an active member of it --
    /// at entry `index` at least.
    VerifyJoin { set: u32, joiner: u32, version: u32, index: u64 },
    /// The joins this host's cs is the joiner of.
    Joins,
    /// A copy-up's copy (N10): the copy-up pending at map place `position`
    /// of the volume, from chunk `from` to chunk `to`, asked by the volume's
    /// writer at `generation`, this cs an active member of their set at its
    /// version now -- at entry `index` at least.
    VerifyCopy { volume: u32, generation: u64, position: u32, from: ChunkRef, to: ChunkRef, index: u64 },
    /// The join of set `set`, begun at `since`, done: its end proposed --
    /// the cs takes its place -- and told once it is applied.
    JoinDone { set: u32, since: u32 },
    /// The join given up: the cs started again, and its marks are gone.
    GiveUp { set: u32, since: u32 },
    /// A fence asked of this cs (`Op::Fence`): the volume's writer of now at
    /// `generation`, as the metadata has it at entry `index` at least.
    VerifyFence { volume: u32, generation: u64, index: u64 },
    /// Device `device` of this host's cs found at format `format`: the
    /// metadata's record of it proposed (`DeviceFormatted`), and told once
    /// applied.
    Formatted { device: u32, format: u64 },
}

/// A join this host's cs is the joiner of (N9), as its mds tells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinView {
    pub set: u32,
    /// The member whose place it takes.
    pub place: u8,
    /// The version the join raised the set to, and the set's now.
    pub since: u32,
    pub version: u32,
    /// The place its own: a member back, which copies what it may have
    /// missed -- what was written at `until`, the last version it was active
    /// at, or after.
    pub own: bool,
    pub until: u32,
    /// The set's active members, the copy's sources, and their hosts.
    pub sources: Vec<(u32, String)>,
    /// The entry the mds has applied: a source's mds is asked to be as far.
    pub index: u64,
    /// The volumes with chunks in the set, each at its newest writer's
    /// generation: the joiner fenced so before its join's marks begin
    /// (`copy::fence_for`, N13). Out when the volume's writer of now fenced
    /// the set's members, it holds the fence of the one before, and that
    /// one, taken over, its view as new as the join, would write it there
    /// as nowhere else -- unasked, the chunks the copy moved on at the
    /// join's epoch (found by fuzz/nbs's cluster target).
    pub fences: Vec<(u32, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub id: u64,
    pub question: Question,
}

/// A device of this host's cs, as the metadata has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub ident: String,
    pub device: u32,
    pub slots: u32,
    pub draining: bool,
    /// The format the metadata has of it, 0 for none: one the cs reads
    /// blank, of a format recorded, is no device to format again
    /// (`nbscs::take_in`).
    pub format: u64,
}

/// Whose cs this host's is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub cluster: [u8; 16],
    pub cs: u32,
    pub devices: Vec<Device>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Told {
    /// None: the host has no cs in the metadata yet -- no device added.
    Identity(Option<Identity>),
    Verified,
    Refused(Refusal),
    /// The chunks of a `Live` that are gone.
    Gone(Vec<u32>),
    /// A `Verify` of a write by the set's joiner (N9) -- the join begun at
    /// `since`: verified, and the cs is to mark what clients write to the
    /// set from now on.
    VerifiedJoining { since: u32 },
    Joins(Vec<JoinView>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answered {
    pub id: u64,
    pub told: Told,
}

/* ---- the bytes ---- */

fn chunk_ref(w: &mut Writer, c: &ChunkRef) {
    w.u32(c.chunk);
    w.u32(c.incarnation);
    w.u32(c.epoch.set);
    w.u32(c.epoch.version);
}

fn read_chunk_ref(r: &mut Reader<'_>) -> Option<ChunkRef> {
    Some(ChunkRef { chunk: r.u32()?, incarnation: r.u32()?, epoch: Epoch { set: r.u32()?, version: r.u32()? } })
}

fn refusal(w: &mut Writer, why: &Refusal) {
    match why {
        Refusal::Fenced { generation } => {
            w.u8(1);
            w.u64(*generation);
        }
        Refusal::NotFenced { generation } => {
            w.u8(2);
            w.u64(*generation);
        }
        Refusal::StaleChunk { incarnation } => {
            w.u8(3);
            w.u32(*incarnation);
        }
        Refusal::Epoch { known } => {
            w.u8(4);
            w.u32(known.set);
            w.u32(known.version);
        }
        Refusal::NoRoom => w.u8(6),
        Refusal::Bad => w.u8(7),
        Refusal::Failed => w.u8(8),
        Refusal::Io => w.u8(9),
        Refusal::TooManyFences => w.u8(10),
        Refusal::NotTheMap => w.u8(11),
        Refusal::NotHere => w.u8(12),
        Refusal::NotAttached => w.u8(13),
        Refusal::Behind { applied } => {
            w.u8(14);
            w.u64(*applied);
        }
        Refusal::NoMds => w.u8(15),
        Refusal::NotReady => w.u8(16),
        Refusal::NotHeld => w.u8(17),
        Refusal::NotTheJoiner => w.u8(18),
    }
}

fn read_refusal(r: &mut Reader<'_>) -> Option<Refusal> {
    Some(match r.u8()? {
        1 => Refusal::Fenced { generation: r.u64()? },
        2 => Refusal::NotFenced { generation: r.u64()? },
        3 => Refusal::StaleChunk { incarnation: r.u32()? },
        4 => Refusal::Epoch { known: Epoch { set: r.u32()?, version: r.u32()? } },
        6 => Refusal::NoRoom,
        7 => Refusal::Bad,
        8 => Refusal::Failed,
        9 => Refusal::Io,
        10 => Refusal::TooManyFences,
        11 => Refusal::NotTheMap,
        12 => Refusal::NotHere,
        13 => Refusal::NotAttached,
        14 => Refusal::Behind { applied: r.u64()? },
        15 => Refusal::NoMds,
        16 => Refusal::NotReady,
        17 => Refusal::NotHeld,
        18 => Refusal::NotTheJoiner,
        _ => return None,
    })
}

impl Refusal {
    /// The engine's refusal, as the wire says it.
    pub fn of(e: nbscs::Error) -> Refusal {
        use nbscs::Error as E;
        match e {
            E::Fenced { generation } => Refusal::Fenced { generation },
            E::NotFenced { generation } => Refusal::NotFenced { generation },
            E::StaleChunk { incarnation } => Refusal::StaleChunk { incarnation },
            E::Epoch { known } => Refusal::Epoch { known },
            /* Asked of the mds by the cs before it answers: a client never
               sees it */
            E::Unverified => Refusal::NotReady,
            E::NoRoom => Refusal::NoRoom,
            E::Bad => Refusal::Bad,
            E::Failed => Refusal::Failed,
            E::Io => Refusal::Io,
            E::TooManyFences => Refusal::TooManyFences,
            E::NotHeld => Refusal::NotHeld,
            /* Asked again by the client, as for a cs not ready */
            E::NoMemory => Refusal::NotReady,
        }
    }

    /// What it says, as a person reads it.
    pub fn says(&self) -> &'static str {
        match self {
            Refusal::Fenced { .. } => "fenced: the volume is attached at a newer generation",
            Refusal::NotFenced { .. } => "not fenced at this generation yet",
            Refusal::StaleChunk { .. } => "the chunk's number is a newer chunk's",
            Refusal::Epoch { .. } => "the chunk is at another epoch",
            Refusal::NoRoom => "no room on the cs's devices",
            Refusal::Bad => "past the chunk, not whole sectors, or empty",
            Refusal::Failed => "the device the chunk is on failed",
            Refusal::Io => "a device failed under the request",
            Refusal::TooManyFences => "as many fences as a cs keeps",
            Refusal::NotTheMap => "no such chunk in that place of the volume, at that incarnation",
            Refusal::NotHere => "this host's cs is not in the chunk's set",
            Refusal::NotAttached => "the volume is not attached at that generation",
            Refusal::Behind { .. } => "the host's metadata is behind the request: ask again",
            Refusal::NoMds => "no metadata service on the host",
            Refusal::NotReady => "the cs is not serving yet",
            Refusal::NotHeld => "the cs holds no such chunk",
            Refusal::NotTheJoiner => "the cs named is not the set's joiner at that version",
        }
    }
}

/// The most a request is but for a write's data: its id, volume and
/// generation, its kind and the fields of the largest -- a copy-up's two
/// chunks, or a write's chunk, place and range.
const REQUEST_HEAD_MAX: usize = 128;

impl Request<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.encode_into(&mut w);
        w.buf
    }

    /// `encode`, refused rather than had where there is no memory for it:
    /// the whole of it asked for first -- a write's is a piece of up to
    /// `DATA_MAX` and its head, more than a kernel's heap may have at once.
    pub fn try_encode(&self) -> Option<Vec<u8>> {
        let data = match &self.op {
            Op::Write { data, .. } => data.len(),
            _ => 0,
        };
        let mut buf = Vec::new();
        buf.try_reserve_exact(REQUEST_HEAD_MAX + data).ok()?;
        let mut w = Writer { buf };
        self.encode_into(&mut w);
        Some(w.buf)
    }

    fn encode_into(&self, w: &mut Writer) {
        w.u64(self.id);
        w.u32(self.volume);
        w.u64(self.generation);
        match &self.op {
            Op::Fence { index } => {
                w.u8(1);
                w.u64(*index);
            }
            Op::Write { chunk, position, index, offset, data } => {
                w.u8(2);
                chunk_ref(w, chunk);
                w.u32(*position);
                w.u64(*index);
                w.u32(*offset);
                w.u32(data.len() as u32);
                w.fixed(data);
            }
            Op::Read { chunk, position, index, offset, len, as_of } => {
                w.u8(3);
                chunk_ref(w, chunk);
                w.u32(*position);
                w.u64(*index);
                w.u32(*offset);
                w.u32(*len);
                w.u64(*as_of);
            }
            Op::Flush => w.u8(4),
            Op::Close { chunk, position, index } => {
                w.u8(5);
                chunk_ref(w, chunk);
                w.u32(*position);
                w.u64(*index);
            }
            Op::Held { from } => {
                w.u8(6);
                w.u32(*from);
            }
            Op::Ping => w.u8(7),
            Op::Holdings { set, from, joiner, version, index } => {
                w.u8(8);
                w.u32(*set);
                w.u32(*from);
                w.u32(*joiner);
                w.u32(*version);
                w.u64(*index);
            }
            Op::Pull { chunk, joiner, index, offset, len } => {
                w.u8(9);
                chunk_ref(w, chunk);
                w.u32(*joiner);
                w.u64(*index);
                w.u32(*offset);
                w.u32(*len);
            }
            Op::Sums { chunk, joiner, index, offset, len } => {
                w.u8(10);
                chunk_ref(w, chunk);
                w.u32(*joiner);
                w.u64(*index);
                w.u32(*offset);
                w.u32(*len);
            }
            Op::Scrub { chunk } => {
                w.u8(11);
                chunk_ref(w, chunk);
            }
            Op::CopyUp { from, to, position, index } => {
                w.u8(12);
                chunk_ref(w, from);
                chunk_ref(w, to);
                w.u32(*position);
                w.u64(*index);
            }
        }
    }
}

impl<'a> Request<'a> {
    pub fn decode(b: &'a [u8]) -> Option<Request<'a>> {
        let mut r = Reader::new(b);
        let req = Request::decode_from(&mut r)?;
        r.done().then_some(req)
    }

    /// A request as a cs takes it (N13): one of its own, or a write passed
    /// along its replicas, with what its chain says after it.
    pub fn decode_relayed(b: &'a [u8]) -> Option<(Request<'a>, Option<Relay<'a>>)> {
        let mut r = Reader::new(b);
        let req = Request::decode_from(&mut r)?;
        if r.done() {
            return Some((req, None));
        }
        if !matches!(req.op, Op::Write { .. }) || r.u8()? != RELAY_MARK {
            return None;
        }
        let answer = relay_name(&mut r)?;
        let mark = r.u64()?;
        let count = r.u8()? as usize;
        if count > RELAY_HOPS_MAX {
            return None;
        }
        let mut next = Vec::new();
        next.try_reserve_exact(count).ok()?;
        for _ in 0..count {
            next.push(Hop { id: r.u64()?, mark: r.u64()?, host: relay_name(&mut r)? });
        }
        r.done().then_some((req, Some(Relay { answer, mark, next })))
    }

    fn decode_from(r: &mut Reader<'a>) -> Option<Request<'a>> {
        let id = r.u64()?;
        let volume = r.u32()?;
        let generation = r.u64()?;
        let op = match r.u8()? {
            1 => Op::Fence { index: r.u64()? },
            2 => {
                let chunk = read_chunk_ref(r)?;
                let (position, index, offset) = (r.u32()?, r.u64()?, r.u32()?);
                let len = r.u32()?;
                if len > DATA_MAX {
                    return None;
                }
                Op::Write { chunk, position, index, offset, data: r.bytes(len as usize)? }
            }
            3 => {
                let chunk = read_chunk_ref(r)?;
                let (position, index, offset, len, as_of) = (r.u32()?, r.u64()?, r.u32()?, r.u32()?, r.u64()?);
                if len > DATA_MAX {
                    return None;
                }
                Op::Read { chunk, position, index, offset, len, as_of }
            }
            4 => Op::Flush,
            5 => Op::Close { chunk: read_chunk_ref(r)?, position: r.u32()?, index: r.u64()? },
            6 => Op::Held { from: r.u32()? },
            7 => Op::Ping,
            8 => Op::Holdings { set: r.u32()?, from: r.u32()?, joiner: r.u32()?, version: r.u32()?, index: r.u64()? },
            code @ (9 | 10) => {
                let pull = code == 9;
                let chunk = read_chunk_ref(r)?;
                let (joiner, index, offset, len) = (r.u32()?, r.u64()?, r.u32()?, r.u32()?);
                /* A pull is data, as a read is; a sum reads at most a chunk */
                if (pull && len > DATA_MAX) || u64::from(len) > nbscs::SLOT {
                    return None;
                }
                if pull {
                    Op::Pull { chunk, joiner, index, offset, len }
                } else {
                    Op::Sums { chunk, joiner, index, offset, len }
                }
            }
            11 => Op::Scrub { chunk: read_chunk_ref(r)? },
            12 => Op::CopyUp { from: read_chunk_ref(r)?, to: read_chunk_ref(r)?, position: r.u32()?,
                               index: r.u64()? },
            _ => return None,
        };
        Some(Request { id, volume, generation, op })
    }
}

/// A write passed along its replicas (N13): the replicas after the one it
/// is sent to, in order, each with a request id of its own -- the client
/// knows each answer by it -- and the writer session's mailbox, by name
/// and host, for each replica to answer to: an ipc address is the host's
/// that holds it, and means nothing to a third. And `mark`: every request
/// of the session's to this replica below it has been answered -- a write
/// that comes again, along the chain and directly, is applied once
/// (`serve::Requests`'s relayed writes), and one late, under a newer write
/// of its blocks, never over it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relay<'a> {
    pub answer: &'a str,
    pub mark: u64,
    pub next: Vec<Hop<'a>>,
}

/// A replica a write is to be passed on to: its request's id and mark, and
/// the name of the host whose cs it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hop<'a> {
    pub id: u64,
    pub mark: u64,
    pub host: &'a str,
}

/// What follows a write that is passed on.
const RELAY_MARK: u8 = 0xC4;
/// A relay's name at most -- a mailbox's with its host's, or a host's.
pub const RELAY_NAME_MAX: usize = 128;
/// A relay's hops at most: a set's replicas but the first, and its joiner.
pub const RELAY_HOPS_MAX: usize = nbsmeta::REPLICAS;

fn relay_name<'a>(r: &mut Reader<'a>) -> Option<&'a str> {
    let len = r.u16()? as usize;
    if len == 0 || len > RELAY_NAME_MAX {
        return None;
    }
    core::str::from_utf8(r.bytes(len)?).ok()
}

fn relay_name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= RELAY_NAME_MAX
}

impl Request<'_> {
    /// A write's bytes with `relay` after them -- what a replica of its
    /// chain is sent; refused rather than had where there is no memory for
    /// it, as `try_encode` is. None for a request that is not a write, or a
    /// relay that is not one.
    pub fn try_encode_relayed(&self, relay: &Relay<'_>) -> Option<Vec<u8>> {
        let Op::Write { data, .. } = &self.op else { return None };
        if relay.next.len() > RELAY_HOPS_MAX || !relay_name_ok(relay.answer)
            || relay.next.iter().any(|h| !relay_name_ok(h.host))
        {
            return None;
        }
        let tail = 12 + relay.answer.len() + relay.next.iter().map(|h| 18 + h.host.len()).sum::<usize>();
        let mut buf = Vec::new();
        buf.try_reserve_exact(REQUEST_HEAD_MAX + data.len() + tail).ok()?;
        let mut w = Writer { buf };
        self.encode_into(&mut w);
        w.u8(RELAY_MARK);
        w.u16(relay.answer.len() as u16);
        w.fixed(relay.answer.as_bytes());
        w.u64(relay.mark);
        w.u8(relay.next.len() as u8);
        for h in &relay.next {
            w.u64(h.id);
            w.u64(h.mark);
            w.u16(h.host.len() as u16);
            w.fixed(h.host.as_bytes());
        }
        Some(w.buf)
    }
}

/// What the cs a write came to with `relay` passes on (N13): the next
/// replica's host, and the write as that replica is to take it -- its own
/// id, the hops after it, the same mailbox to answer to. None where this is
/// the chain's last, or there is no memory for it now: that replica is then
/// sent the write by its client, which waits for its answer.
pub fn relay_on(req: &Request<'_>, relay: &Relay<'_>) -> Option<(String, Vec<u8>)> {
    let (first, rest) = relay.next.split_first()?;
    let mut next = Vec::new();
    next.try_reserve_exact(rest.len()).ok()?;
    next.extend_from_slice(rest);
    let on = Relay { answer: relay.answer, mark: first.mark, next };
    let bytes = Request { id: first.id, volume: req.volume, generation: req.generation, op: req.op.clone() }
        .try_encode_relayed(&on)?;
    Some((String::from(first.host), bytes))
}

/// The most an answer is but for what it carries -- a read's data, a list:
/// its id, its kind, a count, a refusal's fields.
const ANSWER_HEAD_MAX: usize = 64;

impl Answer<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.encode_into(&mut w);
        w.buf
    }

    /// `encode`, refused rather than had where there is no memory for it:
    /// the whole of it asked for first -- a read's carries a piece of up to
    /// `DATA_MAX`, more than a kernel's heap may have at once.
    pub fn try_encode(&self) -> Option<Vec<u8>> {
        let body = match &self.reply {
            Reply::Data(d) => d.len(),
            Reply::Held(list) => list.len() * 16,
            Reply::Chunks(list) => list.len() * 16,
            Reply::Sums { sums, .. } => sums.len() * 4,
            Reply::Scrub { sums, .. } => sums.len() * 4,
            _ => 0,
        };
        let mut buf = Vec::new();
        buf.try_reserve_exact(ANSWER_HEAD_MAX + body).ok()?;
        let mut w = Writer { buf };
        self.encode_into(&mut w);
        Some(w.buf)
    }

    fn encode_into(&self, w: &mut Writer) {
        w.u64(self.id);
        match &self.reply {
            Reply::Fenced { generation } => {
                w.u8(1);
                w.u64(*generation);
            }
            Reply::Done => w.u8(2),
            Reply::Data(d) => {
                w.u8(3);
                w.u32(d.len() as u32);
                w.fixed(d);
            }
            Reply::Closed(c) => {
                w.u8(4);
                w.u8(u8::from(*c));
            }
            Reply::Held(list) => {
                w.u8(5);
                w.u32(list.len() as u32);
                for &(c, i, g) in list {
                    w.u32(c);
                    w.u32(i);
                    w.u64(g);
                }
            }
            Reply::Refused(why) => {
                w.u8(6);
                refusal(w, why);
            }
            Reply::Chunks(list) => {
                w.u8(7);
                w.u32(list.len() as u32);
                for &(c, i, v, at) in list {
                    w.u32(c);
                    w.u32(i);
                    w.u32(v);
                    w.u32(at);
                }
            }
            Reply::Sums { sums, open } => {
                w.u8(8);
                w.u32(sums.len() as u32);
                for &s in sums {
                    w.u32(s);
                }
                match open {
                    None => w.u8(0),
                    Some(g) => {
                        w.u8(1);
                        w.u64(*g);
                    }
                }
            }
            Reply::Scrub { open, written, sums } => {
                w.u8(9);
                w.u8(u8::from(*open));
                w.u32(*written);
                w.u32(sums.len() as u32);
                for &s in sums {
                    w.u32(s);
                }
            }
        }
    }
}

impl<'a> Answer<'a> {
    pub fn decode(b: &'a [u8]) -> Option<Answer<'a>> {
        let mut r = Reader::new(b);
        let id = r.u64()?;
        let reply = match r.u8()? {
            1 => Reply::Fenced { generation: r.u64()? },
            2 => Reply::Done,
            3 => {
                let len = r.u32()?;
                if len > DATA_MAX {
                    return None;
                }
                Reply::Data(r.bytes(len as usize)?)
            }
            4 => Reply::Closed(match r.u8()? {
                0 => false,
                1 => true,
                _ => return None,
            }),
            5 => {
                let n = r.u32()? as usize;
                /* Each entry is 16 bytes: a count past what is there is none */
                if n > LIST_MAX || n * 16 > r.remaining() {
                    return None;
                }
                let mut list = Vec::new();
                for _ in 0..n {
                    list.push((r.u32()?, r.u32()?, r.u64()?));
                }
                Reply::Held(list)
            }
            6 => Reply::Refused(read_refusal(&mut r)?),
            7 => {
                let n = r.u32()? as usize;
                /* Each entry is 16 bytes: a count past what is there is none */
                if n > LIST_MAX || n * 16 > r.remaining() {
                    return None;
                }
                let mut list = Vec::new();
                for _ in 0..n {
                    list.push((r.u32()?, r.u32()?, r.u32()?, r.u32()?));
                }
                Reply::Chunks(list)
            }
            8 => {
                let n = r.u32()? as usize;
                if n > SUMS_MAX || n * 4 > r.remaining() {
                    return None;
                }
                let mut sums = Vec::new();
                for _ in 0..n {
                    sums.push(r.u32()?);
                }
                let open = match r.u8()? {
                    0 => None,
                    1 => Some(r.u64()?),
                    _ => return None,
                };
                Reply::Sums { sums, open }
            }
            9 => {
                let open = match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                let written = r.u32()?;
                let n = r.u32()? as usize;
                if n > SUMS_MAX || n * 4 > r.remaining() {
                    return None;
                }
                let mut sums = Vec::new();
                for _ in 0..n {
                    sums.push(r.u32()?);
                }
                Reply::Scrub { open, written, sums }
            }
            _ => return None,
        };
        r.done().then_some(Answer { id, reply })
    }
}

impl Query {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u64(self.id);
        match &self.question {
            Question::Identity => w.u8(1),
            Question::Verify { volume, generation, chunk, position, index, writing } => {
                w.u8(2);
                w.u32(*volume);
                w.u64(*generation);
                chunk_ref(&mut w, chunk);
                w.u32(*position);
                w.u64(*index);
                w.u8(u8::from(*writing));
            }
            Question::Live { chunks } => {
                w.u8(3);
                w.u32(chunks.len() as u32);
                for &(c, i) in chunks {
                    w.u32(c);
                    w.u32(i);
                }
            }
            Question::Volumes { volumes } => {
                w.u8(4);
                w.u32(volumes.len() as u32);
                for &v in volumes {
                    w.u32(v);
                }
            }
            Question::VerifyJoin { set, joiner, version, index } => {
                w.u8(5);
                w.u32(*set);
                w.u32(*joiner);
                w.u32(*version);
                w.u64(*index);
            }
            Question::Joins => w.u8(6),
            Question::JoinDone { set, since } => {
                w.u8(7);
                w.u32(*set);
                w.u32(*since);
            }
            Question::GiveUp { set, since } => {
                w.u8(8);
                w.u32(*set);
                w.u32(*since);
            }
            Question::VerifyCopy { volume, generation, position, from, to, index } => {
                w.u8(9);
                w.u32(*volume);
                w.u64(*generation);
                w.u32(*position);
                chunk_ref(&mut w, from);
                chunk_ref(&mut w, to);
                w.u64(*index);
            }
            Question::VerifyFence { volume, generation, index } => {
                w.u8(10);
                w.u32(*volume);
                w.u64(*generation);
                w.u64(*index);
            }
            Question::Formatted { device, format } => {
                w.u8(11);
                w.u32(*device);
                w.u64(*format);
            }
        }
        w.buf
    }

    pub fn decode(b: &[u8]) -> Option<Query> {
        let mut r = Reader::new(b);
        let id = r.u64()?;
        let question = match r.u8()? {
            1 => Question::Identity,
            2 => {
                let (volume, generation) = (r.u32()?, r.u64()?);
                let chunk = read_chunk_ref(&mut r)?;
                let (position, index) = (r.u32()?, r.u64()?);
                let writing = match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                Question::Verify { volume, generation, chunk, position, index, writing }
            }
            3 => {
                let n = r.u32()? as usize;
                if n > LIST_MAX || n * 8 > r.remaining() {
                    return None;
                }
                let mut chunks = Vec::new();
                for _ in 0..n {
                    chunks.push((r.u32()?, r.u32()?));
                }
                Question::Live { chunks }
            }
            4 => {
                let n = r.u32()? as usize;
                if n > LIST_MAX || n * 4 > r.remaining() {
                    return None;
                }
                let mut volumes = Vec::new();
                for _ in 0..n {
                    volumes.push(r.u32()?);
                }
                Question::Volumes { volumes }
            }
            5 => Question::VerifyJoin { set: r.u32()?, joiner: r.u32()?, version: r.u32()?, index: r.u64()? },
            6 => Question::Joins,
            7 => Question::JoinDone { set: r.u32()?, since: r.u32()? },
            8 => Question::GiveUp { set: r.u32()?, since: r.u32()? },
            9 => {
                let (volume, generation, position) = (r.u32()?, r.u64()?, r.u32()?);
                let (from, to) = (read_chunk_ref(&mut r)?, read_chunk_ref(&mut r)?);
                Question::VerifyCopy { volume, generation, position, from, to, index: r.u64()? }
            }
            10 => Question::VerifyFence { volume: r.u32()?, generation: r.u64()?, index: r.u64()? },
            11 => Question::Formatted { device: r.u32()?, format: r.u64()? },
            _ => return None,
        };
        r.done().then_some(Query { id, question })
    }
}

impl Answered {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u64(self.id);
        match &self.told {
            Told::Identity(None) => w.u8(1),
            Told::Identity(Some(i)) => {
                w.u8(2);
                w.fixed(&i.cluster);
                w.u32(i.cs);
                w.u8(i.devices.len() as u8);
                for d in &i.devices {
                    w.str(&d.ident);
                    w.u32(d.device);
                    w.u32(d.slots);
                    w.u8(u8::from(d.draining));
                    w.u64(d.format);
                }
            }
            Told::Verified => w.u8(3),
            Told::Refused(why) => {
                w.u8(4);
                refusal(&mut w, why);
            }
            Told::Gone(list) => {
                w.u8(5);
                w.u32(list.len() as u32);
                for &c in list {
                    w.u32(c);
                }
            }
            Told::VerifiedJoining { since } => {
                w.u8(6);
                w.u32(*since);
            }
            Told::Joins(joins) => {
                w.u8(7);
                w.u32(joins.len() as u32);
                for j in joins {
                    w.u32(j.set);
                    w.u8(j.place);
                    w.u32(j.since);
                    w.u32(j.version);
                    w.u8(u8::from(j.own));
                    w.u32(j.until);
                    w.u8(j.sources.len() as u8);
                    for (cs, host) in &j.sources {
                        w.u32(*cs);
                        w.str(host);
                    }
                    w.u64(j.index);
                    w.u32(j.fences.len() as u32);
                    for &(volume, generation) in &j.fences {
                        w.u32(volume);
                        w.u64(generation);
                    }
                }
            }
        }
        w.buf
    }

    pub fn decode(b: &[u8]) -> Option<Answered> {
        let mut r = Reader::new(b);
        let id = r.u64()?;
        let told = match r.u8()? {
            1 => Told::Identity(None),
            2 => {
                let cluster = r.fixed::<16>()?;
                let cs = r.u32()?;
                let n = usize::from(r.u8()?);
                if n > nbsmeta::DEVICES_MAX {
                    return None;
                }
                let mut devices = Vec::new();
                for _ in 0..n {
                    let ident = String::from(r.str(IDENT_MAX)?);
                    let (device, slots) = (r.u32()?, r.u32()?);
                    let draining = match r.u8()? {
                        0 => false,
                        1 => true,
                        _ => return None,
                    };
                    devices.push(Device { ident, device, slots, draining, format: r.u64()? });
                }
                Told::Identity(Some(Identity { cluster, cs, devices }))
            }
            3 => Told::Verified,
            4 => Told::Refused(read_refusal(&mut r)?),
            5 => {
                let n = r.u32()? as usize;
                if n > LIST_MAX || n * 4 > r.remaining() {
                    return None;
                }
                let mut list = Vec::new();
                for _ in 0..n {
                    list.push(r.u32()?);
                }
                Told::Gone(list)
            }
            6 => Told::VerifiedJoining { since: r.u32()? },
            7 => {
                let n = r.u32()? as usize;
                /* Each join is 24 bytes at the least */
                if n > LIST_MAX || n * 24 > r.remaining() {
                    return None;
                }
                let mut joins = Vec::new();
                for _ in 0..n {
                    let (set, place, since, version) = (r.u32()?, r.u8()?, r.u32()?, r.u32()?);
                    let own = match r.u8()? {
                        0 => false,
                        1 => true,
                        _ => return None,
                    };
                    let until = r.u32()?;
                    let k = usize::from(r.u8()?);
                    if usize::from(place) >= nbsmeta::REPLICAS || k > nbsmeta::REPLICAS {
                        return None;
                    }
                    let mut sources = Vec::new();
                    for _ in 0..k {
                        let cs = r.u32()?;
                        sources.push((cs, String::from(r.str(nbsmeta::NAME_MAX)?)));
                    }
                    let index = r.u64()?;
                    let f = r.u32()? as usize;
                    /* Each fence is 12 bytes */
                    if f > LIST_MAX || f * 12 > r.remaining() {
                        return None;
                    }
                    let mut fences = Vec::new();
                    fences.try_reserve_exact(f).ok()?;
                    for _ in 0..f {
                        fences.push((r.u32()?, r.u64()?));
                    }
                    joins.push(JoinView { set, place, since, version, own, until, sources, index, fences });
                }
                Told::Joins(joins)
            }
            _ => return None,
        };
        r.done().then_some(Answered { id, told })
    }
}

/* ---- a client and its host's mds (N8) ---- */

/// A client's ask of its host's mds, on `nbs.mds`, and what it is told:
/// past the tags of the cs's questions.
pub const TAG_ASK: u32 = 0x112;
pub const TAG_SAID: u32 = 0x113;
/// The most places of a volume's map an ask is told at once.
pub const MAP_MAX: u32 = 4096;
/// The most sums an answer gives: a chunk's granules (N9).
pub const SUMS_MAX: usize = (nbscs::SLOT / nbscs::GRANULE as u64) as usize;
/// The longest reason a refusal is told, in bytes: past it, cut short at a
/// character's edge.
pub const REASON_MAX: usize = 255;

fn reason(s: &str) -> &str {
    let mut n = s.len().min(REASON_MAX);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// A member of a chunk's set, as a client is told it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaState {
    /// Written and read.
    Active,
    /// Taken out: neither.
    Down,
}

/// What a place's joiner is on the wire when it has none.
pub const NO_JOINER: u32 = u32::MAX;

/// A place of a volume's map: the chunk there, as a request names it, its
/// set's three cs's, each with its state, and the set's joiner -- a cs
/// being copied onto, written and not read (N9). `shared`: the chunk is
/// another map's too -- a snapshot's or a clone's, and so of every copy-up
/// pending (N10): its writer copies it up before it writes there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub chunk: ChunkRef,
    pub cs: [u32; nbsmeta::REPLICAS],
    pub replicas: [ReplicaState; nbsmeta::REPLICAS],
    pub joiner: Option<u32>,
    pub shared: bool,
}

/// What a client -- hv's disk, `nbs io`, nbsload -- asks its host's mds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Volume `name` attached on this host: for writing, at a new generation
    /// -- `force` takes it from whoever has it -- or read-only; `token` the
    /// session's own, so that the attach asked again is answered as it was.
    Attach { name: String, writable: bool, force: bool, token: u64 },
    /// Volume `name` let go of: the writer of `generation`, or with 0 a
    /// reader; `token` the session's, kept by the metadata so that an attach
    /// of its applied late attaches the volume to nobody.
    Detach { name: String, generation: u64, token: u64 },
    /// `count` places of volume `name`'s map from place `from`, as the
    /// metadata has them at entry `index` at least.
    Map { name: String, from: u32, count: u32, index: u64 },
    /// Replica `cs` of set `set` taken out: it did not answer the client --
    /// or, `refused`, it refused as failed. Asked as volume `name`'s writer
    /// at `generation`, of the set at `version` as its map has it: refused as
    /// stale once the set has moved on, so that an ask late -- the member
    /// back by a join since, or joining -- takes nothing out again (the
    /// review of 2026-11).
    Down { set: u32, cs: u32, refused: bool, name: String, generation: u64, version: u32 },
    /// A new chunk for place `place` of volume `name`, whose chunk is shared
    /// (N10): asked by its writer at `generation` before it writes there,
    /// told the chunk each active member of the set is to copy the old one
    /// into -- or the old one, shared no longer.
    CopyUp { name: String, place: u32, generation: u64 },
    /// The copy-up at place `place` done: the map names chunk `chunk` there
    /// -- at `incarnation`, the one its members copied into: a number is
    /// taken again once its chunk is freed, and a copy-up given up and asked
    /// anew may be given the number it had (`Command::CopyDone`).
    CopyDone { name: String, place: u32, generation: u64, chunk: u32, incarnation: u32 },
    /// Paused for snapshot `pause` (N10c2): every chunk the writer at
    /// `generation` wrote closed, nothing new sent.
    Paused { name: String, generation: u64, pause: u64 },
    /// The writer's own attach at `generation` anew (N10c2): a pause with no
    /// word -- refused where it is no longer the writer. `token` its
    /// session's now, by which it is asked again.
    Reattach { name: String, generation: u64, token: u64 },
    /// The writer of volume `name` at `generation` there: said every few
    /// seconds, answered by nothing, so that this host's mds started again
    /// knows the session a snapshot pauses (the review of 2026-11: one that
    /// asked nothing of the mds was not known to it, and its volume took no
    /// snapshot until its guest started again).
    Here { name: String, generation: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asking {
    pub id: u64,
    pub ask: Ask,
}

/// What a client is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Said {
    /// Attached: the generation -- a reader's, the generation a writer
    /// would be given next, which its reads carry as `as_of` -- and the
    /// entry it was applied at; `first`: the volume's first attach for
    /// writing, made empty, no clone -- everything of it reads as zeros.
    Attached { generation: u64, index: u64, first: bool },
    /// Let go of, at entry `index`.
    Detached { index: u64 },
    /// Volume `volume`, `size_mib` MiB of `places` places: those asked for,
    /// from place `from`, at entry `index`; and the hosts of the cs's they
    /// name.
    Map { volume: u32, size_mib: u64, places: u32, index: u64, from: u32, map: Vec<Place>, hosts: Vec<(u32, String)> },
    /// The replica is out: its set's version now, at entry `index`.
    Down { version: u32, index: u64 },
    /// The mds has not applied as far as the ask's entry: asked again.
    Behind { applied: u64 },
    /// Not done now, and why -- no leader yet, the leader looking at the cs,
    /// the cs answering the leader: asked again in a moment.
    NotNow(String),
    /// Refused, and why.
    Refused(String),
    /// The replica asked out is its set's last active one: it stays in.
    LastReplica,
    /// The one asking is not the volume's writer: attached elsewhere since.
    NotTheWriter,
    /// A copy-up's chunk (N10): the chunk, its incarnation, and the entry the
    /// answer was applied at.
    Chunk { chunk: u32, incarnation: u32, index: u64 },
    /// The writer's session asked to pause for a snapshot of its volume
    /// (N10c2), by this host's mds: unasked, as `pause`.
    Pause { pause: u64 },
    /// The pause over: the map read again at entry `index` at least -- every
    /// chunk shared there where the snapshot was taken.
    GoOn { pause: u64, index: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saying {
    pub id: u64,
    pub said: Said,
}

fn replica_state(s: ReplicaState) -> u8 {
    match s {
        ReplicaState::Active => 0,
        ReplicaState::Down => 2,
    }
}

fn read_replica_state(c: u8) -> Option<ReplicaState> {
    Some(match c {
        0 => ReplicaState::Active,
        2 => ReplicaState::Down,
        _ => return None,
    })
}

/// A place's bytes: its chunk, its three cs's and their states, its joiner,
/// whether it is shared.
const PLACE_BYTES: usize = 16 + 4 * nbsmeta::REPLICAS + nbsmeta::REPLICAS + 4 + 1;

impl Asking {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u64(self.id);
        match &self.ask {
            Ask::Attach { name, writable, force, token } => {
                w.u8(1);
                w.str(name);
                w.u8(u8::from(*writable) | u8::from(*force) << 1);
                w.u64(*token);
            }
            Ask::Detach { name, generation, token } => {
                w.u8(2);
                w.str(name);
                w.u64(*generation);
                w.u64(*token);
            }
            Ask::Map { name, from, count, index } => {
                w.u8(3);
                w.str(name);
                w.u32(*from);
                w.u32(*count);
                w.u64(*index);
            }
            Ask::Down { set, cs, refused, name, generation, version } => {
                w.u8(4);
                w.u32(*set);
                w.u32(*cs);
                w.u8(u8::from(*refused));
                w.str(name);
                w.u64(*generation);
                w.u32(*version);
            }
            Ask::CopyUp { name, place, generation } => {
                w.u8(5);
                w.str(name);
                w.u32(*place);
                w.u64(*generation);
            }
            Ask::CopyDone { name, place, generation, chunk, incarnation } => {
                w.u8(6);
                w.str(name);
                w.u32(*place);
                w.u64(*generation);
                w.u32(*chunk);
                w.u32(*incarnation);
            }
            Ask::Paused { name, generation, pause } => {
                w.u8(7);
                w.str(name);
                w.u64(*generation);
                w.u64(*pause);
            }
            Ask::Reattach { name, generation, token } => {
                w.u8(8);
                w.str(name);
                w.u64(*generation);
                w.u64(*token);
            }
            Ask::Here { name, generation } => {
                w.u8(9);
                w.str(name);
                w.u64(*generation);
            }
        }
        w.buf
    }

    pub fn decode(b: &[u8]) -> Option<Asking> {
        let mut r = Reader::new(b);
        let id = r.u64()?;
        let ask = match r.u8()? {
            1 => {
                let name = String::from(r.str(nbsmeta::VOLUME_NAME_MAX)?);
                let flags = r.u8()?;
                if flags & !3 != 0 {
                    return None;
                }
                Ask::Attach { name, writable: flags & 1 != 0, force: flags & 2 != 0, token: r.u64()? }
            }
            2 => {
                let name = String::from(r.str(nbsmeta::VOLUME_NAME_MAX)?);
                Ask::Detach { name, generation: r.u64()?, token: r.u64()? }
            }
            3 => {
                let name = String::from(r.str(nbsmeta::VOLUME_NAME_MAX)?);
                let (from, count, index) = (r.u32()?, r.u32()?, r.u64()?);
                if count > MAP_MAX {
                    return None;
                }
                Ask::Map { name, from, count, index }
            }
            4 => {
                let (set, cs) = (r.u32()?, r.u32()?);
                let refused = match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                let name = String::from(r.str(nbsmeta::VOLUME_NAME_MAX)?);
                Ask::Down { set, cs, refused, name, generation: r.u64()?, version: r.u32()? }
            }
            5 => {
                let name = String::from(r.str(nbsmeta::VOLUME_NAME_MAX)?);
                Ask::CopyUp { name, place: r.u32()?, generation: r.u64()? }
            }
            6 => {
                let name = String::from(r.str(nbsmeta::VOLUME_NAME_MAX)?);
                let (place, generation, chunk) = (r.u32()?, r.u64()?, r.u32()?);
                Ask::CopyDone { name, place, generation, chunk, incarnation: r.u32()? }
            }
            7 => {
                let name = String::from(r.str(nbsmeta::VOLUME_NAME_MAX)?);
                Ask::Paused { name, generation: r.u64()?, pause: r.u64()? }
            }
            8 => {
                let name = String::from(r.str(nbsmeta::VOLUME_NAME_MAX)?);
                Ask::Reattach { name, generation: r.u64()?, token: r.u64()? }
            }
            9 => {
                let name = String::from(r.str(nbsmeta::VOLUME_NAME_MAX)?);
                Ask::Here { name, generation: r.u64()? }
            }
            _ => return None,
        };
        r.done().then_some(Asking { id, ask })
    }
}

impl Saying {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u64(self.id);
        match &self.said {
            Said::Attached { generation, index, first } => {
                w.u8(1);
                w.u64(*generation);
                w.u64(*index);
                w.u8(u8::from(*first));
            }
            Said::Detached { index } => {
                w.u8(2);
                w.u64(*index);
            }
            Said::Map { volume, size_mib, places, index, from, map, hosts } => {
                w.u8(3);
                w.u32(*volume);
                w.u64(*size_mib);
                w.u32(*places);
                w.u64(*index);
                w.u32(*from);
                w.u32(map.len() as u32);
                for p in map {
                    chunk_ref(&mut w, &p.chunk);
                    for &c in &p.cs {
                        w.u32(c);
                    }
                    for &s in &p.replicas {
                        w.u8(replica_state(s));
                    }
                    w.u32(p.joiner.unwrap_or(NO_JOINER));
                    w.u8(u8::from(p.shared));
                }
                w.u8(hosts.len() as u8);
                for (cs, host) in hosts {
                    w.u32(*cs);
                    w.str(host);
                }
            }
            Said::Down { version, index } => {
                w.u8(4);
                w.u32(*version);
                w.u64(*index);
            }
            Said::Behind { applied } => {
                w.u8(5);
                w.u64(*applied);
            }
            Said::NotNow(why) => {
                w.u8(6);
                w.str(reason(why));
            }
            Said::Refused(why) => {
                w.u8(7);
                w.str(reason(why));
            }
            Said::LastReplica => w.u8(8),
            Said::NotTheWriter => w.u8(9),
            Said::Chunk { chunk, incarnation, index } => {
                w.u8(10);
                w.u32(*chunk);
                w.u32(*incarnation);
                w.u64(*index);
            }
            Said::Pause { pause } => {
                w.u8(11);
                w.u64(*pause);
            }
            Said::GoOn { pause, index } => {
                w.u8(12);
                w.u64(*pause);
                w.u64(*index);
            }
        }
        w.buf
    }

    pub fn decode(b: &[u8]) -> Option<Saying> {
        let mut r = Reader::new(b);
        let id = r.u64()?;
        let said = match r.u8()? {
            1 => {
                let (generation, index) = (r.u64()?, r.u64()?);
                let first = match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                Said::Attached { generation, index, first }
            }
            2 => Said::Detached { index: r.u64()? },
            3 => {
                let (volume, size_mib, places, index, from) = (r.u32()?, r.u64()?, r.u32()?, r.u64()?, r.u32()?);
                let n = r.u32()?;
                /* A count past what is there, or past what is told at once, is none */
                if n > MAP_MAX || n as usize * PLACE_BYTES > r.remaining() {
                    return None;
                }
                let mut map = Vec::new();
                for _ in 0..n {
                    let chunk = read_chunk_ref(&mut r)?;
                    let mut cs = [0u32; nbsmeta::REPLICAS];
                    for c in cs.iter_mut() {
                        *c = r.u32()?;
                    }
                    let mut replicas = [ReplicaState::Active; nbsmeta::REPLICAS];
                    for s in replicas.iter_mut() {
                        *s = read_replica_state(r.u8()?)?;
                    }
                    let joiner = match r.u32()? {
                        NO_JOINER => None,
                        j if cs.contains(&j) && replicas[cs.iter().position(|&c| c == j)?] != ReplicaState::Down => {
                            return None;
                        }
                        j => Some(j),
                    };
                    let shared = match r.u8()? {
                        0 => false,
                        1 => true,
                        _ => return None,
                    };
                    map.push(Place { chunk, cs, replicas, joiner, shared });
                }
                let k = usize::from(r.u8()?);
                if k > nbsmeta::CS_MAX {
                    return None;
                }
                let mut hosts = Vec::new();
                for _ in 0..k {
                    let cs = r.u32()?;
                    hosts.push((cs, String::from(r.str(nbsmeta::NAME_MAX)?)));
                }
                Said::Map { volume, size_mib, places, index, from, map, hosts }
            }
            4 => {
                let version = r.u32()?;
                Said::Down { version, index: r.u64()? }
            }
            5 => Said::Behind { applied: r.u64()? },
            6 => Said::NotNow(String::from(r.str(REASON_MAX)?)),
            7 => Said::Refused(String::from(r.str(REASON_MAX)?)),
            8 => Said::LastReplica,
            9 => Said::NotTheWriter,
            10 => Said::Chunk { chunk: r.u32()?, incarnation: r.u32()?, index: r.u64()? },
            11 => Said::Pause { pause: r.u64()? },
            12 => Said::GoOn { pause: r.u64()?, index: r.u64()? },
            _ => return None,
        };
        r.done().then_some(Saying { id, said })
    }
}
