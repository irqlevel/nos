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
//!
//! Every message is read whole and held to its limits: one that is not a
//! message of these, or has bytes past its end, is none (`None`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

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
    NotTheVolumes,
    NoRoom,
    Bad,
    Failed,
    Io,
    TooManyFences,
    /// The mds: no such chunk at that place of that volume's map, at that
    /// incarnation.
    NotTheMap,
    /// The mds: this host's cs is not in the chunk's set.
    NotHere,
    /// The mds: the generation is not the volume's attach's.
    NotAttached,
    /// The mds has not applied as far as the request's index: asked again.
    Behind { applied: u64 },
    /// No mds on the host, or one in no cluster.
    NoMds,
    /// The cs is not serving yet: its devices not taken in.
    NotReady,
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
        Refusal::NotTheVolumes => w.u8(5),
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
    }
}

fn read_refusal(r: &mut Reader<'_>) -> Option<Refusal> {
    Some(match r.u8()? {
        1 => Refusal::Fenced { generation: r.u64()? },
        2 => Refusal::NotFenced { generation: r.u64()? },
        3 => Refusal::StaleChunk { incarnation: r.u32()? },
        4 => Refusal::Epoch { known: Epoch { set: r.u32()?, version: r.u32()? } },
        5 => Refusal::NotTheVolumes,
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
            E::NotTheVolumes => Refusal::NotTheVolumes,
            E::NoRoom => Refusal::NoRoom,
            E::Bad => Refusal::Bad,
            E::Failed => Refusal::Failed,
            E::Io => Refusal::Io,
            E::TooManyFences => Refusal::TooManyFences,
        }
    }

    /// What it says, as a person reads it.
    pub fn says(&self) -> &'static str {
        match self {
            Refusal::Fenced { .. } => "fenced: the volume is attached at a newer generation",
            Refusal::NotFenced { .. } => "not fenced at this generation yet",
            Refusal::StaleChunk { .. } => "the chunk's number is a newer chunk's",
            Refusal::Epoch { .. } => "the chunk is at another epoch",
            Refusal::NotTheVolumes => "the chunk is another volume's",
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
            _ => return None,
        };
        r.done().then_some(Answered { id, told })
    }
}
