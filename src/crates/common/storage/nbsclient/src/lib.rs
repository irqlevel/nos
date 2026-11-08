//! NBS's client (plans/nbs.md, "The client, in hv"; step N8): a volume
//! attached on this host, read and written through the cs's that hold its
//! chunks, as a state machine. It is fed what comes -- its host's mds's
//! answers, the cs's -- and the time, and says what to send and which of
//! its driver's operations are done; the driver is hv's `nbs:` disk, `nbs
//! io` or nbsload over ipc, or fuzz/nbs's cluster target over a network of
//! its own. No kernel in it.
//!
//! A session's life:
//! 1. **Attach** at the host's mds: for writing at a new generation --
//!    `force` takes the volume from a writer elsewhere -- or read-only.
//! 2. **The map**, read from the mds at the attach's entry or later: each
//!    place's chunk, its incarnation, its set; each set's cs's and their
//!    states; each cs's host.
//! 3. **Fences**: every cs of the volume's sets fenced at the generation, so
//!    that an older writer's requests are refused from here on.
//! 4. **Reconcile**: a chunk left open on any replica -- by a writer that
//!    stopped uncleanly -- made the same on all of them, read from one and
//!    written to every one, then closed (rule 6).
//! 5. **Serve**, until **stop**: flush, every chunk written closed, detach.
//!
//! What it holds to (plans/nbs.md, "Replication: the rules"):
//! - A write is done only once every replica of its chunk's set that is
//!   not out has it. One that has not answered in 0.5 s is asked out at the
//!   mds, whose leader takes it out once the cs does not answer it either;
//!   the write is done when the mds says it is out -- never on a set the mds
//!   would not vouch for. Meanwhile the request is sent again every second.
//! - Two writes that overlap are never out at once: the second waits.
//! - A flush waits for every write before it, then asks every cs that has
//!   acknowledged a write since the last flush began to make its writes
//!   durable; it is done once they have, and once every flush before it is
//!   -- which flushes what was acknowledged before that one began. One that
//!   failed fails those after it that were out with it.
//! - A read goes to one active replica, this host's where it has one, and
//!   to another when that one does not answer.
//! - A chunk written and flushed, and not written since for a while, is
//!   closed on every replica, so that a crash leaves few chunks to
//!   reconcile. A close of the driver's flushes, as a flush does, then
//!   closes every chunk written before it at once.
//! - An operation not done in a minute fails, said -- a stalled disk is
//!   reported, not left hanging.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use nbscs::{ChunkRef, Epoch, SLOT};
use nbswire::{Answer, Ask, Asking, Op, Refusal, Reply, ReplicaState, Request, Said, Saying, DATA_MAX, MAP_MAX};

/// How long a replica may be silent before it is asked out (decision 6).
pub const SILENT_NS: u64 = 500_000_000;
/// How often a request not answered is sent again.
pub const RESEND_NS: u64 = 1_000_000_000;
/// How long before an ask refused for now is asked again.
pub const AGAIN_NS: u64 = 200_000_000;
/// How long an operation may take before it fails.
pub const DEADLINE_NS: u64 = 60_000_000_000;
/// How long the attach is asked for before the session fails: a session
/// that could not attach in that long asks no more, so that an attach of
/// its arriving late never takes the volume from a writer since.
pub const ATTACH_NS: u64 = 10_000_000_000;
/// How long a chunk written and flushed stays open with nothing written to it.
pub const CLOSE_IDLE_NS: u64 = 3_000_000_000;
/// What an operation's offset and length are whole multiples of.
pub const BLOCK: u64 = 4096;
/// Tags of the session's own operations -- a reconcile's reads and writes.
const INTERNAL: u64 = 1 << 63;
/// Why a session fails when a chunk's last active replica refuses it: the
/// replica restarted with the chunk open at this attach's generation, so
/// what was written to it since the last flush may be gone, and no other
/// replica has it. Going on would read back as written what may not be;
/// an attach of its own reconciles the chunk from what the replica holds.
pub const LOST_LAST: &str = "a chunk's last active replica restarted and may have lost what was written to it since \
                             the last flush -- the volume is to be attached again";
const REPLICAS: usize = nbsmeta::REPLICAS;

/// Where a message goes: this host's mds (`TAG_ASK`), or a cs by its number
/// (`TAG_REQUEST`, bulk), on the host `host_of` names. A cs's request goes
/// with its id as the message's reference, so that a bounce of it is told
/// apart (`on_bounce`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum To {
    Mds,
    Cs(u32),
}

/// What the session asks its driver to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    Send { to: To, id: u64, bytes: Vec<u8> },
    /// Operation `tag` done, well or not.
    Done { tag: u64, ok: bool },
    /// A line for the log: a replica taken out, the session failed.
    Say(String),
}

/// An operation of the driver's: `tag` is its own, and names its buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserOp {
    Read { tag: u64, offset: u64, len: u32 },
    Write { tag: u64, offset: u64, len: u32 },
    Flush { tag: u64 },
    /// A flush, then every chunk written closed on its replicas: the
    /// volume attached still.
    Close { tag: u64 },
}

/// The driver's buffers, by an operation's tag: a write's data, and where
/// a read's goes.
pub trait Buffers {
    fn data(&self, tag: u64) -> &[u8];
    fn fill(&mut self, tag: u64, at: usize, bytes: &[u8]);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Attaching,
    Mapping,
    Fencing,
    Reconciling,
    Serving,
    Stopping,
    Detached,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SetInfo {
    version: u32,
    cs: [u32; REPLICAS],
    replicas: [ReplicaState; REPLICAS],
    /// The cs being copied onto (N9): written, not read.
    joiner: Option<u32>,
}

impl SetInfo {
    /// Its replicas that are written: the active members, and the joiner --
    /// a write it missed would be missing from what its copy takes over.
    fn written(&self) -> impl Iterator<Item = u32> + '_ {
        (0..REPLICAS).filter(|&i| self.replicas[i] != ReplicaState::Down).map(|i| self.cs[i]).chain(self.joiner)
    }

    fn active(&self, cs: u32) -> bool {
        (0..REPLICAS).any(|i| self.cs[i] == cs && self.replicas[i] == ReplicaState::Active)
    }

    fn written_to(&self, cs: u32) -> bool {
        self.joiner == Some(cs) || (0..REPLICAS).any(|i| self.cs[i] == cs && self.replicas[i] != ReplicaState::Down)
    }
}

/// A volume's map, as the mds told it.
#[derive(Default)]
struct Map {
    volume: u32,
    size: u64,
    /// Each place's chunk, its incarnation, its set.
    places: Vec<(u32, u32, u32)>,
    sets: BTreeMap<u32, SetInfo>,
    hosts: BTreeMap<u32, String>,
}

impl Map {
    fn chunk(&self, place: u32) -> Option<ChunkRef> {
        let &(chunk, incarnation, set) = self.places.get(place as usize)?;
        let s = self.sets.get(&set)?;
        Some(ChunkRef { chunk, incarnation, epoch: Epoch { set, version: s.version } })
    }

    fn set_of(&self, place: u32) -> Option<(u32, SetInfo)> {
        let &(_, _, set) = self.places.get(place as usize)?;
        Some((set, *self.sets.get(&set)?))
    }

    /// Every cs some set of the volume writes to.
    fn written_cs(&self) -> BTreeSet<u32> {
        self.sets.values().flat_map(|s| s.written().collect::<Vec<u32>>()).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    Flush,
    /// A flush, then the closes.
    Close,
}

impl Kind {
    /// Ordered, failed and done as a flush is.
    fn flushes(self) -> bool {
        matches!(self, Kind::Flush | Kind::Close)
    }
}

struct OpState {
    tag: u64,
    kind: Kind,
    offset: u64,
    len: u32,
    /// Its pieces not done yet; a flush's cs's not answered.
    pieces: u32,
    waiting: BTreeSet<u32>,
    ok: bool,
    started: u64,
    /// Out to the cs's: neither queued behind an overlap nor parked.
    out: bool,
    /// A close's places being closed, once its flush is done.
    closing: Option<BTreeSet<u32>>,
}

/// An operation's part inside one chunk, and at most what a request
/// carries.
struct Piece {
    op: u64,
    place: u32,
    inside: u32,
    len: u32,
    /// Where in the operation's buffer.
    at: u32,
    /// A write's replicas still to answer; a read's one asked now.
    waiting: BTreeSet<u32>,
    /// A read's replicas tried.
    tried: BTreeSet<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum What {
    Piece(u64),
    Fence,
    Flush(u64),
    Held,
    Close(u32),
}

/// A request out to a cs.
struct Sub {
    cs: u32,
    what: What,
    bytes: Vec<u8>,
    sent: u64,
    first: u64,
    /// When it is to go again: a refusal for now, or a bounce.
    again: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MdsWant {
    Attach,
    Map,
    Detach,
}

/// The ask of the mds the session's phase waits on.
struct MdsAsk {
    id: u64,
    want: MdsWant,
    bytes: Vec<u8>,
    sent: u64,
    again: Option<u64>,
}

/// A replica asked out.
struct DownAsk {
    id: u64,
    bytes: Vec<u8>,
    sent: u64,
    again: Option<u64>,
    /// Asked out as refusing this attach, not as silent.
    refused: bool,
    /// Its refusal said in the log already.
    said: bool,
}

/// A chunk being made the same on its replicas: its place, the piece being
/// copied, and whether that piece is being read or written.
struct Mend {
    place: u32,
    piece: u32,
    writing: bool,
    op: Option<u64>,
    /// Closes out, once its pieces are copied.
    closing: BTreeSet<u32>,
}

pub struct Session {
    name: String,
    me: String,
    writable: bool,
    force: bool,
    /// The session's own, made by its driver: what its attach asked again is
    /// known by.
    token: u64,
    /// When the attach was first asked.
    attach_from: u64,
    phase: Phase,
    generation: u64,
    /// The metadata's entry the map was read at, at least: what every
    /// request carries.
    index: u64,
    map: Map,
    /// A map being read, page by page.
    reading: Option<Map>,
    next_id: u64,
    mds: Option<MdsAsk>,
    downs: BTreeMap<(u32, u32), DownAsk>,
    subs: BTreeMap<u64, Sub>,
    pieces: BTreeMap<u64, Piece>,
    next_piece: u64,
    ops: BTreeMap<u64, OpState>,
    next_op: u64,
    /// Operations waiting their turn, in order.
    queue: VecDeque<u64>,
    /// Pieces waiting for the map read again.
    parked: Vec<u64>,
    fenced: BTreeSet<u32>,
    held: BTreeMap<u32, Option<Vec<(u32, u32, u64)>>>,
    /// The cs's written to since the last flush began.
    dirty: BTreeSet<u32>,
    /// Places written and not closed: when last written, and whether a
    /// flush since has made them durable.
    open: BTreeMap<u32, (u64, bool)>,
    /// Closes out for places idle: place -> cs's not answered.
    closing: BTreeMap<u32, BTreeSet<u32>>,
    mend: VecDeque<u32>,
    mending: Option<Mend>,
    scratch: Vec<u8>,
    /// The operation `stop` was asked by, once it is.
    stop: Option<u64>,
    /// Read pieces each cs has answered, for a person: where a read went.
    reads: BTreeMap<u32, u64>,
    stop_flush: Option<u64>,
}

impl Session {
    /// A session of volume `name` on host `me`, `token` its own -- 64 random
    /// bits, never 0: started by `start`.
    pub fn new(name: &str, me: &str, writable: bool, force: bool, token: u64) -> Session {
        Session {
            name: String::from(name), me: String::from(me), writable, force, token, attach_from: 0,
            phase: Phase::Attaching, generation: 0,
            index: 0, map: Map::default(), reading: None, next_id: 1, mds: None, downs: BTreeMap::new(),
            subs: BTreeMap::new(), pieces: BTreeMap::new(), next_piece: 1, ops: BTreeMap::new(), next_op: 1,
            queue: VecDeque::new(), parked: Vec::new(), fenced: BTreeSet::new(), held: BTreeMap::new(),
            dirty: BTreeSet::new(), open: BTreeMap::new(), closing: BTreeMap::new(), mend: VecDeque::new(),
            mending: None, scratch: Vec::new(), stop: None, stop_flush: None, reads: BTreeMap::new(),
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// The volume's size in bytes, once its map is read.
    pub fn size(&self) -> u64 {
        self.map.size
    }

    pub fn volume(&self) -> u32 {
        self.map.volume
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The host cs `cs` is on.
    pub fn host_of(&self, cs: u32) -> Option<&str> {
        self.map.hosts.get(&cs).map(|h| h.as_str())
    }

    /// Read pieces each cs has answered, by the cs's number.
    pub fn reads(&self) -> &BTreeMap<u32, u64> {
        &self.reads
    }

    /// Operations not done yet.
    pub fn busy(&self) -> usize {
        self.ops.values().filter(|o| o.tag & INTERNAL == 0).count()
    }

    /// What the session waits on, for a person: its operations, requests,
    /// closes and asks out.
    pub fn report(&self) -> String {
        let mut s = alloc::format!("{:?}, generation {}: {} operations", self.phase, self.generation, self.ops.len());
        for (id, o) in &self.ops {
            s += &alloc::format!(" [#{} {:?} {}+{} pieces {} waiting {:?} out {}]", id, o.kind, o.offset, o.len,
                                 o.pieces, o.waiting, o.out);
        }
        s += &alloc::format!("; pieces {:?}; queue {:?}; parked {:?}; requests",
                             self.pieces.iter().map(|(id, p)| (*id, p.op, p.place)).collect::<Vec<_>>(), self.queue,
                             self.parked);
        for (id, sub) in &self.subs {
            s += &alloc::format!(" [#{} cs {} {:?}]", id, sub.cs, sub.what);
        }
        s += &alloc::format!("; open {:?}; closing {:?}; asked out {:?}; stop {:?}/{:?}; mds {:?}", self.open,
                             self.closing, self.downs.keys().collect::<Vec<_>>(), self.stop, self.stop_flush,
                             self.mds.as_ref().map(|m| m.want));
        s
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /* ---- the mds ---- */

    fn ask_mds(&mut self, want: MdsWant, ask: Ask, now: u64, out: &mut Vec<Out>) {
        let id = self.take_id();
        let bytes = Asking { id, ask }.encode();
        out.push(Out::Send { to: To::Mds, id, bytes: bytes.clone() });
        self.mds = Some(MdsAsk { id, want, bytes, sent: now, again: None });
    }

    /// The session started: the volume attached.
    pub fn start(&mut self, now: u64, out: &mut Vec<Out>) {
        self.attach_from = now;
        let ask = Ask::Attach { name: self.name.clone(), writable: self.writable, force: self.force,
                                token: self.token };
        self.ask_mds(MdsWant::Attach, ask, now, out);
    }

    fn ask_map(&mut self, from: u32, now: u64, out: &mut Vec<Out>) {
        let ask = Ask::Map { name: self.name.clone(), from, count: MAP_MAX, index: self.index };
        self.ask_mds(MdsWant::Map, ask, now, out);
    }

    /// What this host's mds said.
    pub fn on_mds(&mut self, bytes: &[u8], now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(s) = Saying::decode(bytes) else { return };
        if self.mds.as_ref().is_some_and(|m| m.id == s.id) {
            self.on_mds_ask(s.said, now, bufs, out);
            return;
        }
        let Some(&key) = self.downs.iter().find(|(_, d)| d.id == s.id).map(|(k, _)| k) else { return };
        let (refused, said) = self.downs.get(&key).map_or((false, false), |d| (d.refused, d.said));
        match s.said {
            Said::Down { version, index } => {
                self.downs.remove(&key);
                self.index = self.index.max(index);
                self.taken_out(key.0, key.1, version, now, bufs, out);
            }
            Said::NotNow(_) | Said::Behind { .. } => {
                if let Some(d) = self.downs.get_mut(&key) {
                    d.again = Some(now + AGAIN_NS);
                }
            }
            Said::NotTheWriter => {
                /* Attached elsewhere since: what this one writes is fenced */
                self.fail("the volume was attached elsewhere: this attach is fenced", out);
            }
            Said::LastReplica if refused => {
                /* It refuses this attach, and nothing else has the chunk */
                out.push(Out::Say(alloc::format!("nbs: cs {}, set {}'s last active replica, refuses volume {}'s \
                                                  attach at generation {}", key.1, key.0, self.name,
                                                 self.generation)));
                self.fail(LOST_LAST, out);
            }
            Said::LastReplica | Said::Refused(_) => {
                /* Silent, and its set's last active replica -- or no longer
                   this set's: it stays in, the request goes on being sent,
                   and it is asked about again a second on */
                if !said {
                    let why = match &s.said {
                        Said::Refused(why) => why.as_str(),
                        _ => "the set's last active replica: writes wait for it",
                    };
                    out.push(Out::Say(alloc::format!("nbs: cs {} of set {} not taken out: {}", key.1, key.0, why)));
                }
                if let Some(d) = self.downs.get_mut(&key) {
                    d.again = Some(now + RESEND_NS);
                    d.said = true;
                }
            }
            _ => {}
        }
    }

    fn on_mds_ask(&mut self, said: Said, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(want) = self.mds.as_ref().map(|m| m.want) else { return };
        match (want, said) {
            (_, Said::NotNow(_)) | (_, Said::Behind { .. }) => {
                if let Some(m) = self.mds.as_mut() {
                    m.again = Some(now + AGAIN_NS);
                }
            }
            (MdsWant::Attach, Said::Attached { generation, index }) => {
                self.mds = None;
                self.generation = generation;
                self.index = index;
                self.phase = Phase::Mapping;
                self.reading = Some(Map::default());
                self.ask_map(0, now, out);
            }
            (MdsWant::Map, Said::Map { volume, size_mib, places, index, from, map, hosts }) => {
                self.mds = None;
                self.index = self.index.max(index);
                let r = self.reading.get_or_insert_with(Map::default);
                if from as usize != r.places.len() {
                    /* A page not the next: read again from the start */
                    *r = Map::default();
                    self.ask_map(0, now, out);
                    return;
                }
                r.volume = volume;
                r.size = size_mib << 20;
                for p in &map {
                    r.places.push((p.chunk.chunk, p.chunk.incarnation, p.chunk.epoch.set));
                    r.sets.insert(p.chunk.epoch.set, SetInfo { version: p.chunk.epoch.version, cs: p.cs,
                                                               replicas: p.replicas, joiner: p.joiner });
                }
                for (cs, host) in hosts {
                    r.hosts.insert(cs, host);
                }
                if (r.places.len() as u32) < places && !map.is_empty() {
                    let next = r.places.len() as u32;
                    self.ask_map(next, now, out);
                    return;
                }
                let read = self.reading.take().unwrap_or_default();
                self.map = read;
                self.mapped(now, bufs, out);
            }
            (MdsWant::Detach, Said::Detached { .. }) => {
                self.mds = None;
                self.phase = Phase::Detached;
                if let Some(tag) = self.stop.take() {
                    out.push(Out::Done { tag, ok: true });
                }
            }
            (_, Said::Refused(why)) => {
                self.mds = None;
                if want == MdsWant::Detach {
                    /* Let go of already, or never had: detached all the same */
                    self.phase = Phase::Detached;
                    if let Some(tag) = self.stop.take() {
                        out.push(Out::Done { tag, ok: true });
                    }
                    return;
                }
                self.fail(&why, out);
            }
            _ => {}
        }
    }

    /// The map read: on with the session's start, or the parked pieces sent
    /// again.
    fn mapped(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        match self.phase {
            Phase::Mapping => {
                self.phase = Phase::Fencing;
                if !self.writable {
                    /* A read-only attach fences nothing, writes nothing */
                    self.serve(now, bufs, out);
                    return;
                }
                for cs in self.map.written_cs() {
                    self.send(cs, What::Fence, now, bufs, out);
                }
                self.fences_done(now, bufs, out);
            }
            _ => {
                let parked = core::mem::take(&mut self.parked);
                for pid in parked {
                    self.send_piece(pid, now, bufs, out);
                }
            }
        }
    }

    /// Asked again where a page of it was refused: the map, from the start.
    fn refresh(&mut self, now: u64, out: &mut Vec<Out>) {
        if self.mds.is_some() {
            return;
        }
        self.reading = Some(Map::default());
        self.ask_map(0, now, out);
    }

    /* ---- requests to cs's ---- */

    /// The bytes of a request of `what`, its id left 0: a write's data from
    /// its operation's buffer -- the driver's, or the session's own for a
    /// reconcile's copy.
    fn request(&self, what: What, bufs: &dyn Buffers) -> Option<Vec<u8>> {
        let (volume, generation, index) = (self.map.volume, if self.writable { self.generation } else { 0 },
                                           self.index);
        let op = match what {
            What::Fence => Op::Fence,
            What::Held => Op::Held,
            What::Flush(_) => Op::Flush,
            What::Close(place) => Op::Close { chunk: self.map.chunk(place)?, position: place, index },
            What::Piece(pid) => {
                let p = self.pieces.get(&pid)?;
                let o = self.ops.get(&p.op)?;
                let chunk = self.map.chunk(p.place)?;
                match o.kind {
                    Kind::Read => Op::Read { chunk, position: p.place, index, offset: p.inside, len: p.len },
                    _ => {
                        let all = if o.tag & INTERNAL != 0 { &self.scratch[..] } else { bufs.data(o.tag) };
                        let data = all.get(p.at as usize..(p.at + p.len) as usize)?;
                        let op = Op::Write { chunk, position: p.place, index, offset: p.inside, data };
                        return Some(Request { id: 0, volume, generation, op }.encode());
                    }
                }
            }
        };
        Some(Request { id: 0, volume, generation, op }.encode())
    }

    /// A request to cs `cs` made and sent.
    fn send(&mut self, cs: u32, what: What, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(mut bytes) = self.request(what, bufs) else { return };
        let id = self.take_id();
        /* The id is the request's first eight bytes */
        bytes[..8].copy_from_slice(&id.to_le_bytes());
        out.push(Out::Send { to: To::Cs(cs), id, bytes: bytes.clone() });
        self.subs.insert(id, Sub { cs, what, bytes, sent: now, first: now, again: None });
    }

    fn send_piece(&mut self, pid: u64, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(p) = self.pieces.get(&pid) else { return };
        let Some(o) = self.ops.get(&p.op) else { return };
        let Some((_, set)) = self.map.set_of(p.place) else {
            self.piece_done(pid, false, now, bufs, out);
            return;
        };
        match o.kind {
            Kind::Read => {
                /* This host's active replica, or the first one not tried */
                let mut pick = None;
                for cs in set.cs.iter().copied().filter(|&c| set.active(c)) {
                    if self.map.hosts.get(&cs).is_some_and(|h| *h == self.me) && !p.tried.contains(&cs) {
                        pick = Some(cs);
                        break;
                    }
                }
                let pick = pick.or_else(|| set.cs.iter().copied().find(|&c| set.active(c) && !p.tried.contains(&c)))
                    .or_else(|| set.cs.iter().copied().find(|&c| set.active(c)));
                let Some(cs) = pick else {
                    self.piece_done(pid, false, now, bufs, out);
                    return;
                };
                if let Some(p) = self.pieces.get_mut(&pid) {
                    p.tried.insert(cs);
                    p.waiting = BTreeSet::from([cs]);
                }
                self.send(cs, What::Piece(pid), now, bufs, out);
            }
            _ => {
                let to: Vec<u32> = set.written().collect();
                if let Some(p) = self.pieces.get_mut(&pid) {
                    p.waiting = to.iter().copied().collect();
                }
                for cs in to {
                    self.send(cs, What::Piece(pid), now, bufs, out);
                }
            }
        }
    }

    /// A cs's answer to a request.
    pub fn on_cs(&mut self, bytes: &[u8], now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(a) = Answer::decode(bytes) else { return };
        let Some(sub) = self.subs.get(&a.id) else { return };
        let (cs, what) = (sub.cs, sub.what);
        if let Reply::Refused(why) = a.reply {
            self.refused(a.id, cs, what, why, now, bufs, out);
            return;
        }
        self.subs.remove(&a.id);
        match (what, a.reply) {
            (What::Piece(pid), Reply::Done) => {
                /* Dirty from its answer on: a flush launched before it came
                   took the cs's then, and one launched after flushes this */
                self.dirty.insert(cs);
                self.replica_has(pid, cs, now, bufs, out);
            }
            (What::Piece(pid), Reply::Data(d)) => {
                let Some(p) = self.pieces.get(&pid) else { return };
                let Some(o) = self.ops.get(&p.op) else { return };
                if d.len() != p.len as usize {
                    self.piece_done(pid, false, now, bufs, out);
                    return;
                }
                let (tag, at) = (o.tag, p.at as usize);
                *self.reads.entry(cs).or_default() += 1;
                if tag & INTERNAL != 0 {
                    if let Some(to) = self.scratch.get_mut(at..at + d.len()) {
                        to.copy_from_slice(d);
                    }
                } else {
                    bufs.fill(tag, at, d);
                }
                self.piece_done(pid, true, now, bufs, out);
            }
            (What::Fence, Reply::Fenced { generation }) => {
                if generation != self.generation {
                    self.fail("the volume is fenced at a newer generation: attached elsewhere", out);
                    return;
                }
                self.fenced.insert(cs);
                self.fences_done(now, bufs, out);
            }
            (What::Held, Reply::Held(list)) => {
                self.held.insert(cs, Some(list));
                self.helds_done(now, bufs, out);
            }
            (What::Flush(op), Reply::Done) => {
                if let Some(o) = self.ops.get_mut(&op) {
                    o.waiting.remove(&cs);
                }
                self.flush_progress(op, now, bufs, out);
            }
            (What::Close(place), Reply::Closed(_)) => self.closed(place, cs, now, bufs, out),
            _ => {}
        }
    }

    /// A request the channel gave back, by the id it went with: a cs that is
    /// not there is as one that does not answer -- asked out at once.
    pub fn on_bounce(&mut self, id: u64, gone: bool, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(sub) = self.subs.get_mut(&id) else { return };
        sub.again = Some(now + AGAIN_NS);
        if gone {
            let (cs, what) = (sub.cs, sub.what);
            sub.first = sub.first.min(now.saturating_sub(SILENT_NS));
            self.silent(cs, what, now, bufs, out);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn refused(&mut self, id: u64, cs: u32, what: What, why: Refusal, now: u64, bufs: &mut dyn Buffers,
               out: &mut Vec<Out>) {
        match why {
            Refusal::Fenced { .. } | Refusal::NotAttached => {
                self.fail("the volume was attached elsewhere: this attach is fenced", out);
            }
            Refusal::NotFenced { .. } => {
                /* Fenced first, then this again: in that order on the one way
                   to the cs */
                self.subs.remove(&id);
                self.send(cs, What::Fence, now, bufs, out);
                self.send(cs, what, now, bufs, out);
            }
            Refusal::Epoch { .. } | Refusal::StaleChunk { .. } | Refusal::NotTheMap | Refusal::NotTheVolumes
            | Refusal::NotHere => {
                self.subs.remove(&id);
                match what {
                    What::Piece(pid) => {
                        if !self.parked.contains(&pid) {
                            self.parked.push(pid);
                        }
                    }
                    other => self.resend_later(cs, other, now, bufs, out),
                }
                self.refresh(now, out);
            }
            Refusal::Behind { .. } | Refusal::NotReady | Refusal::NoMds => {
                if let Some(s) = self.subs.get_mut(&id) {
                    s.again = Some(now + AGAIN_NS);
                }
            }
            Refusal::Failed | Refusal::Io => {
                /* The device the chunk is on failed there, or the cs lost what
                   it was given of it: asked out as the cs's own refusal */
                if let Some(s) = self.subs.get_mut(&id) {
                    s.again = Some(now + RESEND_NS);
                }
                self.ask_out(cs, what, true, now, bufs, out);
            }
            /* A joiner's (N9), never a client's: as a request the cs found bad */
            Refusal::NoRoom | Refusal::Bad | Refusal::TooManyFences | Refusal::NotHeld | Refusal::NotTheJoiner => {
                self.subs.remove(&id);
                out.push(Out::Say(alloc::format!("nbs: cs {} refused a request of volume {}: {}", cs, self.name,
                                                 why.says())));
                match what {
                    What::Piece(pid) => self.piece_done(pid, false, now, bufs, out),
                    What::Flush(op) => self.flush_failed(op, now, bufs, out),
                    _ => self.fail(why.says(), out),
                }
            }
        }
    }

    /// `what` sent to `cs` again once the map is read again.
    fn resend_later(&mut self, cs: u32, what: What, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        self.send(cs, what, now, bufs, out);
        if let Some((_, s)) = self.subs.iter_mut().rev().find(|(_, s)| s.cs == cs && s.what == what) {
            s.again = Some(now + AGAIN_NS);
        }
    }

    /// Replica `cs` has piece `pid`'s write.
    fn replica_has(&mut self, pid: u64, cs: u32, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(p) = self.pieces.get_mut(&pid) else { return };
        p.waiting.remove(&cs);
        if p.waiting.is_empty() {
            self.piece_done(pid, true, now, bufs, out);
        }
    }

    /* ---- a replica that does not answer ---- */

    /// Cs `cs` silent on a request: asked out of every set it is the
    /// request's -- a piece's chunk's, or for a cs-wide one every set of the
    /// volume it is written in.
    fn silent(&mut self, cs: u32, what: What, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        self.ask_out(cs, what, false, now, bufs, out);
    }

    /// Cs `cs` asked out of the sets `what` is of: silent, or -- `refused`
    /// -- refusing as failed.
    #[allow(clippy::too_many_arguments)]
    fn ask_out(&mut self, cs: u32, what: What, refused: bool, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let sets: Vec<u32> = match what {
            What::Piece(pid) => match self.pieces.get(&pid).and_then(|p| self.map.set_of(p.place)) {
                Some((set, info)) if info.written_to(cs) => alloc::vec![set],
                _ => Vec::new(),
            },
            _ => self.map.sets.iter().filter(|(_, s)| s.written_to(cs)).map(|(&id, _)| id).collect(),
        };
        /* A read moves to another replica meanwhile */
        if let What::Piece(pid) = what {
            let read = self.pieces.get(&pid).and_then(|p| self.ops.get(&p.op)).is_some_and(|o| o.kind == Kind::Read);
            if read && self.pieces.get(&pid).is_some_and(|p| p.waiting.contains(&cs)) {
                let others = self.pieces.get(&pid).and_then(|p| self.map.set_of(p.place))
                    .is_some_and(|(_, s)| s.cs.iter().any(|&c| c != cs && s.active(c)));
                if others {
                    self.drop_subs(cs, what);
                    self.send_piece(pid, now, bufs, out);
                }
            }
        }
        for set in sets {
            /* Asked already -- but one asked as silent is asked again as
               refusing, which the leader need not look at */
            if self.downs.get(&(set, cs)).is_some_and(|d| d.refused || !refused) {
                continue;
            }
            let id = self.take_id();
            let ask = Ask::Down { set, cs, refused, name: self.name.clone(), generation: self.generation };
            let bytes = Asking { id, ask }.encode();
            out.push(Out::Send { to: To::Mds, id, bytes: bytes.clone() });
            self.downs.insert((set, cs), DownAsk { id, bytes, sent: now, again: None, refused, said: false });
        }
    }

    fn drop_subs(&mut self, cs: u32, what: What) {
        self.subs.retain(|_, s| !(s.cs == cs && s.what == what));
    }

    /// Replica `cs` of set `set` out, the set at `version`: nothing more of
    /// that set waits for it.
    #[allow(clippy::too_many_arguments)]
    fn taken_out(&mut self, set: u32, cs: u32, version: u32, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if let Some(s) = self.map.sets.get_mut(&set) {
            /* Its join dropped where it was the joiner -- a member back
               whose copy is given up stays down */
            if s.joiner == Some(cs) {
                s.joiner = None;
            } else {
                for i in 0..REPLICAS {
                    if s.cs[i] == cs {
                        s.replicas[i] = ReplicaState::Down;
                    }
                }
            }
            s.version = s.version.max(version);
        }
        out.push(Out::Say(alloc::format!("nbs: cs {} of set {} taken out of volume {}'s chunks there (version {})",
                                         cs, set, self.name, version)));
        /* Pieces of that set waiting for it */
        let waiting: Vec<u64> = self.pieces.iter()
            .filter(|(_, p)| p.waiting.contains(&cs) && self.map.set_of(p.place).is_some_and(|(s, _)| s == set))
            .map(|(&id, _)| id).collect();
        for pid in waiting {
            self.subs.retain(|_, s| !(s.cs == cs && s.what == What::Piece(pid)));
            let read = self.pieces.get(&pid).and_then(|p| self.ops.get(&p.op)).is_some_and(|o| o.kind == Kind::Read);
            if read {
                self.send_piece(pid, now, bufs, out);
            } else {
                self.replica_has(pid, cs, now, bufs, out);
            }
        }
        /* A cs-wide request: not needed once no set of the volume writes it */
        if !self.map.written_cs().contains(&cs) {
            let cs_wide: Vec<(u64, What)> = self.subs.iter()
                .filter(|(_, s)| s.cs == cs && !matches!(s.what, What::Piece(_)))
                .map(|(&id, s)| (id, s.what)).collect();
            for (id, what) in cs_wide {
                self.subs.remove(&id);
                match what {
                    What::Fence => {
                        self.fences_done(now, bufs, out);
                    }
                    What::Held => {
                        self.held.insert(cs, Some(Vec::new()));
                        self.helds_done(now, bufs, out);
                    }
                    What::Flush(op) => {
                        if let Some(o) = self.ops.get_mut(&op) {
                            o.waiting.remove(&cs);
                        }
                        self.flush_progress(op, now, bufs, out);
                    }
                    What::Close(place) => self.closed(place, cs, now, bufs, out),
                    What::Piece(_) => {}
                }
            }
        }
    }

    /* ---- the start ---- */

    /// Every cs written to fenced -- or out: on to the reconcile.
    fn fences_done(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if self.phase != Phase::Fencing {
            return;
        }
        let need = self.map.written_cs();
        if !need.iter().all(|cs| self.fenced.contains(cs)) {
            return;
        }
        self.phase = Phase::Reconciling;
        for &cs in &need {
            self.held.insert(cs, None);
        }
        for cs in need {
            self.send(cs, What::Held, now, bufs, out);
        }
        self.helds_done(now, bufs, out);
    }

    /// Every cs's open chunks of the volume told: each one open on any of
    /// them mended.
    fn helds_done(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if self.phase != Phase::Reconciling || self.mending.is_some() || !self.mend.is_empty() {
            return;
        }
        if self.held.values().any(|h| h.is_none()) {
            return;
        }
        let mut places = BTreeSet::new();
        for list in self.held.values().flatten() {
            for &(chunk, incarnation, _) in list {
                if let Some(p) = self.map.places.iter().position(|&(c, i, _)| c == chunk && i == incarnation) {
                    places.insert(p as u32);
                }
            }
        }
        self.held.clear();
        self.mend = places.into_iter().collect();
        if self.mend.is_empty() {
            self.serve(now, bufs, out);
            return;
        }
        out.push(Out::Say(alloc::format!("nbs: volume {}: {} chunks left open by its last writer, made the same on \
                                          their replicas", self.name, self.mend.len())));
        if self.scratch.try_reserve_exact(DATA_MAX as usize).is_err() {
            self.fail("no memory to mend the volume's open chunks", out);
            return;
        }
        self.scratch.resize(DATA_MAX as usize, 0);
        self.next_mend(now, bufs, out);
    }

    /// The next piece of the chunk being mended, or the next chunk.
    fn next_mend(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let pieces = (SLOT / u64::from(DATA_MAX)) as u32;
        loop {
            let Some(m) = self.mending.as_mut() else {
                let Some(place) = self.mend.pop_front() else {
                    self.scratch = Vec::new();
                    self.serve(now, bufs, out);
                    return;
                };
                self.mending = Some(Mend { place, piece: 0, writing: false, op: None, closing: BTreeSet::new() });
                continue;
            };
            if m.op.is_some() || !m.closing.is_empty() {
                return;
            }
            if m.piece < pieces {
                let offset = u64::from(m.place) * SLOT + u64::from(m.piece) * u64::from(DATA_MAX);
                let tag = INTERNAL | u64::from(m.piece);
                let kind = if m.writing { Kind::Write } else { Kind::Read };
                let op = self.begin(tag, kind, offset, DATA_MAX, now);
                if let Some(m) = self.mending.as_mut() {
                    m.op = Some(op);
                }
                self.launch(op, now, bufs, out);
                return;
            }
            /* Copied: closed on every replica written to */
            let place = m.place;
            let to: BTreeSet<u32> = self.map.set_of(place).map(|(_, s)| s.written().collect()).unwrap_or_default();
            if to.is_empty() {
                self.mending = None;
                continue;
            }
            if let Some(m) = self.mending.as_mut() {
                m.closing = to.clone();
            }
            for cs in to {
                self.send(cs, What::Close(place), now, bufs, out);
            }
            return;
        }
    }

    /// The reconcile's own operation done: on to its next step.
    fn mended_step(&mut self, ok: bool, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if !ok {
            self.fail("a chunk left open could not be made the same on its replicas", out);
            return;
        }
        if let Some(m) = self.mending.as_mut() {
            m.op = None;
            if m.writing {
                m.writing = false;
                m.piece += 1;
            } else {
                m.writing = true;
            }
        }
        self.next_mend(now, bufs, out);
    }

    /// Serving: the operations that waited for it started -- or the stop
    /// asked for meanwhile begun.
    fn serve(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if matches!(self.phase, Phase::Failed(_)) {
            return;
        }
        self.phase = Phase::Serving;
        self.pump(now, bufs, out);
        if let Some(tag) = self.stop.take() {
            self.stop(tag, now, bufs, out);
        }
    }

    /* ---- the driver's operations ---- */

    fn begin(&mut self, tag: u64, kind: Kind, offset: u64, len: u32, now: u64) -> u64 {
        let id = self.next_op;
        self.next_op += 1;
        self.ops.insert(id, OpState { tag, kind, offset, len, pieces: 0, waiting: BTreeSet::new(), ok: true,
                                      started: now, out: false, closing: None });
        id
    }

    /// An operation of the driver's: done at once where it cannot be --
    /// past the volume, not whole blocks, a write read-only -- or started in
    /// its turn.
    pub fn submit(&mut self, op: UserOp, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let (tag, kind, offset, len) = match op {
            UserOp::Read { tag, offset, len } => (tag, Kind::Read, offset, len),
            UserOp::Write { tag, offset, len } => (tag, Kind::Write, offset, len),
            UserOp::Flush { tag } => (tag, Kind::Flush, 0, 0),
            UserOp::Close { tag } => (tag, Kind::Close, 0, 0),
        };
        if tag & INTERNAL != 0 || matches!(self.phase, Phase::Failed(_) | Phase::Detached | Phase::Stopping) {
            out.push(Out::Done { tag, ok: false });
            return;
        }
        if !kind.flushes() {
            let bad = len == 0 || offset % BLOCK != 0 || u64::from(len) % BLOCK != 0
                || (self.phase == Phase::Serving && offset.checked_add(u64::from(len)).is_none_or(|e| e > self.map.size));
            if bad || (kind == Kind::Write && !self.writable) {
                out.push(Out::Done { tag, ok: false });
                return;
            }
        }
        let id = self.begin(tag, kind, offset, len, now);
        self.queue.push_back(id);
        self.pump(now, bufs, out);
    }

    /// The operations waiting their turn started, in order, while their turn
    /// has come: a write not over one out, a flush once every write before
    /// it is done.
    fn pump(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if self.phase != Phase::Serving && self.phase != Phase::Stopping && self.mending.is_none() {
            return;
        }
        let mut k = 0;
        while k < self.queue.len() {
            let id = self.queue[k];
            let Some(o) = self.ops.get(&id) else {
                self.queue.remove(k);
                continue;
            };
            /* Serving, or stopping: what was asked before the stop is done
               before it */
            let serving = matches!(self.phase, Phase::Serving | Phase::Stopping) || o.tag & INTERNAL != 0;
            let ready = match o.kind {
                Kind::Read => serving,
                Kind::Write => {
                    serving
                        && !self.ops.iter().any(|(&other, x)| other < id && x.kind == Kind::Write
                            && overlaps(x.offset, x.len, o.offset, o.len))
                }
                Kind::Flush | Kind::Close => !self.ops.iter().any(|(&other, x)| other < id && x.kind == Kind::Write),
            };
            if !ready {
                k += 1;
                continue;
            }
            self.queue.remove(k);
            self.launch(id, now, bufs, out);
        }
        self.maybe_stop(now, bufs, out);
    }

    /// Operation `id` out: a read's or a write's pieces sent, a flush's.
    fn launch(&mut self, id: u64, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(o) = self.ops.get_mut(&id) else { return };
        o.out = true;
        let (kind, offset, len) = (o.kind, o.offset, o.len);
        if kind.flushes() {
            let to = core::mem::take(&mut self.dirty);
            if let Some(o) = self.ops.get_mut(&id) {
                o.waiting = to.clone();
            }
            for cs in to {
                self.send(cs, What::Flush(id), now, bufs, out);
            }
            self.flush_progress(id, now, bufs, out);
            return;
        }
        if offset.checked_add(u64::from(len)).is_none_or(|e| e > self.map.size) {
            self.op_done(id, false, out);
            return;
        }
        let mut at = offset;
        let end = offset + u64::from(len);
        let mut made = Vec::new();
        while at < end {
            let place = (at / SLOT) as u32;
            let inside = at % SLOT;
            let n = (end - at).min(SLOT - inside).min(u64::from(DATA_MAX));
            let pid = self.next_piece;
            self.next_piece += 1;
            self.pieces.insert(pid, Piece { op: id, place, inside: inside as u32, len: n as u32,
                                            at: (at - offset) as u32, waiting: BTreeSet::new(),
                                            tried: BTreeSet::new() });
            made.push((pid, place));
            at += n;
        }
        if let Some(o) = self.ops.get_mut(&id) {
            o.pieces = made.len() as u32;
        }
        let mut reopened = false;
        for (pid, place) in made {
            if kind == Kind::Write {
                self.open.insert(place, (now, false));
                reopened |= self.closing.remove(&place).is_some();
            }
            self.send_piece(pid, now, bufs, out);
        }
        if reopened {
            self.closes_progress(now, bufs, out);
        }
    }

    fn piece_done(&mut self, pid: u64, ok: bool, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(p) = self.pieces.remove(&pid) else { return };
        self.subs.retain(|_, s| s.what != What::Piece(pid));
        self.parked.retain(|&x| x != pid);
        let Some(o) = self.ops.get_mut(&p.op) else { return };
        o.ok &= ok;
        o.pieces = o.pieces.saturating_sub(1);
        if o.pieces == 0 {
            let ok = o.ok;
            self.op_done(p.op, ok, out);
            self.after_op(p.op, ok, now, bufs, out);
        }
    }

    /// Flush `op` done once its cs's have answered and every flush before
    /// it is done -- each takes the cs's dirty when it begins, so what one
    /// flushes was written before the next one too. Those after it that
    /// waited for it only, done then as well.
    fn flush_progress(&mut self, op: u64, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(o) = self.ops.get(&op) else { return };
        if !o.waiting.is_empty() || o.closing.is_some()
            || self.ops.iter().any(|(&other, x)| other < op && x.kind.flushes())
        {
            return;
        }
        /* What was written before it is durable now */
        let (started, kind, ok) = (o.started, o.kind, o.ok);
        for v in self.open.values_mut() {
            if v.0 <= started {
                v.1 = true;
            }
        }
        if kind == Kind::Close && ok {
            /* Every chunk written closed on its replicas -- those a write
               out now holds open are written after it */
            let busy: BTreeSet<u32> = self.pieces.values().map(|p| p.place).collect();
            let places: BTreeSet<u32> = self.open.keys().copied().filter(|p| !busy.contains(p)).collect();
            self.close_idle(now, bufs, out, true);
            if let Some(o) = self.ops.get_mut(&op) {
                o.closing = Some(places);
            }
            self.close_progress(op, now, bufs, out);
            return;
        }
        self.flushed(op, ok, now, bufs, out);
    }

    /// Flush `op` done, well or not: and the flushes after it that waited
    /// for it alone.
    fn flushed(&mut self, op: u64, ok: bool, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        self.op_done(op, ok, out);
        self.after_op(op, ok, now, bufs, out);
        let next = self.ops.iter().find(|(_, x)| x.kind.flushes() && x.out).map(|(&id, _)| id);
        if let Some(next) = next {
            self.flush_progress(next, now, bufs, out);
        }
    }

    /// Close `op` done once the places it closes are closed -- or written
    /// again, or out of the map.
    fn close_progress(&mut self, op: u64, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let closing = &self.closing;
        let Some(o) = self.ops.get_mut(&op) else { return };
        let Some(places) = o.closing.as_mut() else { return };
        places.retain(|p| closing.contains_key(p));
        if !places.is_empty() {
            return;
        }
        let ok = o.ok;
        self.flushed(op, ok, now, bufs, out);
    }

    /// Flush `op` failed: and the flushes out after it with it, as what it
    /// was to flush was written before them too.
    fn flush_failed(&mut self, op: u64, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let later: Vec<u64> = self.ops.iter().filter(|(&id, x)| id >= op && x.kind.flushes() && x.out)
            .map(|(&id, _)| id).collect();
        for id in later {
            if let Some(o) = self.ops.get_mut(&id) {
                o.ok = false;
            }
        }
        let closing = match self.ops.get_mut(&op) {
            Some(o) => {
                o.waiting.clear();
                o.closing.take().is_some()
            }
            None => false,
        };
        self.subs.retain(|_, s| s.what != What::Flush(op));
        if closing {
            /* Its closes out: done with them failed */
            self.flushed(op, false, now, bufs, out);
            return;
        }
        self.flush_progress(op, now, bufs, out);
    }

    /// Operation `id` done: its driver told, unless it is the session's own.
    fn op_done(&mut self, id: u64, ok: bool, out: &mut Vec<Out>) {
        let Some(o) = self.ops.remove(&id) else { return };
        if o.tag & INTERNAL == 0 && Some(id) != self.stop_flush {
            out.push(Out::Done { tag: o.tag, ok });
        }
    }

    fn after_op(&mut self, id: u64, ok: bool, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if self.mending.as_ref().is_some_and(|m| m.op == Some(id)) {
            self.mended_step(ok, now, bufs, out);
        }
        if self.stop_flush == Some(id) {
            self.stop_flush = None;
            self.close_all(now, bufs, out);
            return;
        }
        self.pump(now, bufs, out);
    }

    /* ---- closes ---- */

    /// Close of `place` answered by `cs`.
    fn closed(&mut self, place: u32, cs: u32, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if let Some(m) = self.mending.as_mut() {
            if m.place == place && m.closing.remove(&cs) && m.closing.is_empty() {
                self.mending = None;
                self.next_mend(now, bufs, out);
                return;
            }
        }
        if let Some(w) = self.closing.get_mut(&place) {
            w.remove(&cs);
            if w.is_empty() {
                self.closing.remove(&place);
                self.open.remove(&place);
            }
        }
        self.closes_progress(now, bufs, out);
        self.maybe_stop(now, bufs, out);
    }

    /// Every close of the driver's that waits on its places, looked at.
    fn closes_progress(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let waiting: Vec<u64> = self.ops.iter().filter(|(_, o)| o.closing.is_some()).map(|(&id, _)| id).collect();
        for op in waiting {
            self.close_progress(op, now, bufs, out);
        }
    }

    /// Places written, flushed and idle: closed on every replica.
    fn close_idle(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>, all: bool) {
        let busy: BTreeSet<u32> = self.pieces.values().map(|p| p.place).collect();
        let idle: Vec<u32> = self.open.iter()
            .filter(|(p, &(when, flushed))| (all || (flushed && now >= when + CLOSE_IDLE_NS))
                && !busy.contains(p) && !self.closing.contains_key(p))
            .map(|(&p, _)| p).collect();
        for place in idle {
            let to: BTreeSet<u32> = self.map.set_of(place).map(|(_, s)| s.written().collect()).unwrap_or_default();
            if to.is_empty() {
                self.open.remove(&place);
                continue;
            }
            self.closing.insert(place, to.clone());
            for cs in to {
                self.send(cs, What::Close(place), now, bufs, out);
            }
        }
    }

    /* ---- the stop ---- */

    /// The session stopped once the operations out are done: a flush, every
    /// chunk written closed, the volume let go of -- operation `tag` done
    /// then.
    pub fn stop(&mut self, tag: u64, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        match self.phase {
            Phase::Failed(_) | Phase::Detached => {
                out.push(Out::Done { tag, ok: matches!(self.phase, Phase::Detached) });
                return;
            }
            /* Asked again: the first stands */
            Phase::Stopping => {
                out.push(Out::Done { tag, ok: false });
                return;
            }
            /* Still starting: begun once it serves, as the start leaves the
               volume as the stop wants it -- fenced, reconciled */
            Phase::Attaching | Phase::Mapping | Phase::Fencing | Phase::Reconciling => {
                self.stop = Some(tag);
                return;
            }
            Phase::Serving => {}
        }
        self.stop = Some(tag);
        if !self.writable {
            self.phase = Phase::Stopping;
            self.detach(now, out);
            return;
        }
        self.phase = Phase::Stopping;
        let id = self.begin(tag, Kind::Flush, 0, 0, now);
        self.stop_flush = Some(id);
        self.queue.push_back(id);
        self.pump(now, bufs, out);
    }

    fn close_all(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        self.maybe_stop(now, bufs, out);
    }

    /// Stopping, the stop's flush done and nothing out: every place written
    /// closed -- those a read kept busy when the flush was done too -- then
    /// the volume let go of.
    fn maybe_stop(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if self.phase != Phase::Stopping || self.stop_flush.is_some() || self.mds.is_some() {
            return;
        }
        if !self.ops.is_empty() {
            return;
        }
        if self.writable {
            self.close_idle(now, bufs, out, true);
        }
        if !self.closing.is_empty() || (self.writable && !self.open.is_empty()) {
            return;
        }
        self.detach(now, out);
    }

    fn detach(&mut self, now: u64, out: &mut Vec<Out>) {
        let generation = if self.writable { self.generation } else { 0 };
        let ask = Ask::Detach { name: self.name.clone(), generation };
        self.ask_mds(MdsWant::Detach, ask, now, out);
    }

    /// The session failed: every operation with it, said.
    fn fail(&mut self, why: &str, out: &mut Vec<Out>) {
        if matches!(self.phase, Phase::Failed(_)) {
            return;
        }
        out.push(Out::Say(alloc::format!("nbs: volume {}: {}", self.name, why)));
        self.phase = Phase::Failed(String::from(why));
        let ops: Vec<u64> = self.ops.keys().copied().collect();
        for id in ops {
            self.op_done(id, false, out);
        }
        self.queue.clear();
        self.pieces.clear();
        self.subs.clear();
        self.downs.clear();
        self.mds = None;
        if let Some(tag) = self.stop.take() {
            out.push(Out::Done { tag, ok: false });
        }
    }

    /* ---- time ---- */

    /// Time gone by: requests sent again, silent replicas asked out,
    /// operations past their deadline failed, idle chunks closed.
    pub fn tick(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if matches!(self.phase, Phase::Failed(_) | Phase::Detached) {
            return;
        }
        if self.phase == Phase::Attaching && now >= self.attach_from + ATTACH_NS {
            self.fail("the volume's attach not answered in time", out);
            return;
        }
        if let Some(m) = self.mds.as_mut() {
            let due = m.again.map_or(now >= m.sent + RESEND_NS, |a| now >= a);
            if due {
                m.sent = now;
                m.again = None;
                out.push(Out::Send { to: To::Mds, id: m.id, bytes: m.bytes.clone() });
            }
        }
        for d in self.downs.values_mut() {
            let due = d.again.map_or(now >= d.sent + RESEND_NS, |a| now >= a);
            if due {
                d.sent = now;
                d.again = None;
                out.push(Out::Send { to: To::Mds, id: d.id, bytes: d.bytes.clone() });
            }
        }
        let mut silent = Vec::new();
        for (&id, s) in self.subs.iter_mut() {
            let due = s.again.map_or(now >= s.sent + RESEND_NS, |a| now >= a);
            if due {
                s.sent = now;
                s.again = None;
                out.push(Out::Send { to: To::Cs(s.cs), id, bytes: s.bytes.clone() });
            }
            if now >= s.first + SILENT_NS {
                silent.push((s.cs, s.what));
            }
        }
        for (cs, what) in silent {
            self.silent(cs, what, now, bufs, out);
        }
        let late: Vec<u64> = self.ops.iter().filter(|(_, o)| o.out && now >= o.started + DEADLINE_NS)
            .map(|(&id, _)| id).collect();
        for id in late {
            /* Done meanwhile -- a flush after one failed here */
            if !self.ops.contains_key(&id) {
                continue;
            }
            out.push(Out::Say(alloc::format!("nbs: volume {}: an operation not done in {} s -- failed", self.name,
                                             DEADLINE_NS / 1_000_000_000)));
            if self.ops.get(&id).is_some_and(|o| o.kind.flushes()) {
                self.flush_failed(id, now, bufs, out);
                continue;
            }
            let pieces: Vec<u64> = self.pieces.iter().filter(|(_, p)| p.op == id).map(|(&k, _)| k).collect();
            for pid in pieces {
                self.pieces.remove(&pid);
                self.subs.retain(|_, s| s.what != What::Piece(pid));
            }
            self.subs.retain(|_, s| s.what != What::Flush(id));
            self.op_done(id, false, out);
            self.after_op(id, false, now, bufs, out);
        }
        if self.phase == Phase::Serving && self.writable {
            self.close_idle(now, bufs, out, false);
        }
        /* Looked at again each tick, so that no edge it waited for -- a map
           read again, a replica out -- leaves it stopping for good */
        self.maybe_stop(now, bufs, out);
    }
}

fn overlaps(a: u64, alen: u32, b: u64, blen: u32) -> bool {
    a < b + u64::from(blen) && b < a + u64::from(alen)
}
