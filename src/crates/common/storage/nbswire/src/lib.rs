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
/// A device's id at most, as the metadata keeps it.
pub const IDENT_MAX: usize = nbsmeta::IDENT_MAX;

/// What a client asks of a cs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op<'a> {
    /// The volume fenced at the request's generation.
    Fence,
    /// `data` written at `offset` in the chunk, at map place `position` of
    /// the volume, as the metadata had it at entry `index`.
    Write { chunk: ChunkRef, position: u32, index: u64, offset: u32, data: &'a [u8] },
    Read { chunk: ChunkRef, position: u32, index: u64, offset: u32, len: u32 },
    /// Everything written so far durable.
    Flush,
    /// The chunk closed: every replica of it flushed.
    Close { chunk: ChunkRef, position: u32, index: u64 },
    /// The volume's chunks the cs holds open, for a reconcile.
    Held,
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
    /// A sum of each granule asked.
    Sums(Vec<u32>),
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

impl Request<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u64(self.id);
        w.u32(self.volume);
        w.u64(self.generation);
        match &self.op {
            Op::Fence => w.u8(1),
            Op::Write { chunk, position, index, offset, data } => {
                w.u8(2);
                chunk_ref(&mut w, chunk);
                w.u32(*position);
                w.u64(*index);
                w.u32(*offset);
                w.u32(data.len() as u32);
                w.fixed(data);
            }
            Op::Read { chunk, position, index, offset, len } => {
                w.u8(3);
                chunk_ref(&mut w, chunk);
                w.u32(*position);
                w.u64(*index);
                w.u32(*offset);
                w.u32(*len);
            }
            Op::Flush => w.u8(4),
            Op::Close { chunk, position, index } => {
                w.u8(5);
                chunk_ref(&mut w, chunk);
                w.u32(*position);
                w.u64(*index);
            }
            Op::Held => w.u8(6),
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
                chunk_ref(&mut w, chunk);
                w.u32(*joiner);
                w.u64(*index);
                w.u32(*offset);
                w.u32(*len);
            }
            Op::Sums { chunk, joiner, index, offset, len } => {
                w.u8(10);
                chunk_ref(&mut w, chunk);
                w.u32(*joiner);
                w.u64(*index);
                w.u32(*offset);
                w.u32(*len);
            }
            Op::Scrub { chunk } => {
                w.u8(11);
                chunk_ref(&mut w, chunk);
            }
            Op::CopyUp { from, to, position, index } => {
                w.u8(12);
                chunk_ref(&mut w, from);
                chunk_ref(&mut w, to);
                w.u32(*position);
                w.u64(*index);
            }
        }
        w.buf
    }
}

impl<'a> Request<'a> {
    pub fn decode(b: &'a [u8]) -> Option<Request<'a>> {
        let mut r = Reader::new(b);
        let id = r.u64()?;
        let volume = r.u32()?;
        let generation = r.u64()?;
        let op = match r.u8()? {
            1 => Op::Fence,
            2 => {
                let chunk = read_chunk_ref(&mut r)?;
                let (position, index, offset) = (r.u32()?, r.u64()?, r.u32()?);
                let len = r.u32()?;
                if len > DATA_MAX {
                    return None;
                }
                Op::Write { chunk, position, index, offset, data: r.bytes(len as usize)? }
            }
            3 => {
                let chunk = read_chunk_ref(&mut r)?;
                let (position, index, offset, len) = (r.u32()?, r.u64()?, r.u32()?, r.u32()?);
                if len > DATA_MAX {
                    return None;
                }
                Op::Read { chunk, position, index, offset, len }
            }
            4 => Op::Flush,
            5 => Op::Close { chunk: read_chunk_ref(&mut r)?, position: r.u32()?, index: r.u64()? },
            6 => Op::Held,
            7 => Op::Ping,
            8 => Op::Holdings { set: r.u32()?, from: r.u32()?, joiner: r.u32()?, version: r.u32()?, index: r.u64()? },
            code @ (9 | 10) => {
                let pull = code == 9;
                let chunk = read_chunk_ref(&mut r)?;
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
            11 => Op::Scrub { chunk: read_chunk_ref(&mut r)? },
            12 => Op::CopyUp { from: read_chunk_ref(&mut r)?, to: read_chunk_ref(&mut r)?, position: r.u32()?,
                               index: r.u64()? },
            _ => return None,
        };
        r.done().then_some(Request { id, volume, generation, op })
    }
}

impl Answer<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
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
                refusal(&mut w, why);
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
            Reply::Sums(sums) => {
                w.u8(8);
                w.u32(sums.len() as u32);
                for &s in sums {
                    w.u32(s);
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
        w.buf
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
                Reply::Sums(sums)
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
                    devices.push(Device { ident, device, slots, draining });
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
                    joins.push(JoinView { set, place, since, version, own, until, sources, index: r.u64()? });
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
/// being copied onto, written and not read (N9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub chunk: ChunkRef,
    pub cs: [u32; nbsmeta::REPLICAS],
    pub replicas: [ReplicaState; nbsmeta::REPLICAS],
    pub joiner: Option<u32>,
}

/// What a client -- hv's disk, `nbs io`, nbsload -- asks its host's mds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Volume `name` attached on this host: for writing, at a new generation
    /// -- `force` takes it from whoever has it -- or read-only; `token` the
    /// session's own, so that the attach asked again is answered as it was.
    Attach { name: String, writable: bool, force: bool, token: u64 },
    /// Volume `name` let go of: the writer of `generation`, or with 0 a
    /// reader.
    Detach { name: String, generation: u64 },
    /// `count` places of volume `name`'s map from place `from`, as the
    /// metadata has them at entry `index` at least.
    Map { name: String, from: u32, count: u32, index: u64 },
    /// Replica `cs` of set `set` taken out: it did not answer the client --
    /// or, `refused`, it refused as failed. Asked as volume `name`'s writer
    /// at `generation`.
    Down { set: u32, cs: u32, refused: bool, name: String, generation: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asking {
    pub id: u64,
    pub ask: Ask,
}

/// What a client is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Said {
    /// Attached: the generation -- 0 read-only -- and the entry it was
    /// applied at.
    Attached { generation: u64, index: u64 },
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

/// A place's bytes: its chunk, its three cs's and their states, its joiner.
const PLACE_BYTES: usize = 16 + 4 * nbsmeta::REPLICAS + nbsmeta::REPLICAS + 4;

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
            Ask::Detach { name, generation } => {
                w.u8(2);
                w.str(name);
                w.u64(*generation);
            }
            Ask::Map { name, from, count, index } => {
                w.u8(3);
                w.str(name);
                w.u32(*from);
                w.u32(*count);
                w.u64(*index);
            }
            Ask::Down { set, cs, refused, name, generation } => {
                w.u8(4);
                w.u32(*set);
                w.u32(*cs);
                w.u8(u8::from(*refused));
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
                Ask::Detach { name, generation: r.u64()? }
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
                Ask::Down { set, cs, refused, name, generation: r.u64()? }
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
            Said::Attached { generation, index } => {
                w.u8(1);
                w.u64(*generation);
                w.u64(*index);
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
        }
        w.buf
    }

    pub fn decode(b: &[u8]) -> Option<Saying> {
        let mut r = Reader::new(b);
        let id = r.u64()?;
        let said = match r.u8()? {
            1 => {
                let generation = r.u64()?;
                Said::Attached { generation, index: r.u64()? }
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
                    map.push(Place { chunk, cs, replicas, joiner });
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
            _ => return None,
        };
        r.done().then_some(Saying { id, said })
    }
}
