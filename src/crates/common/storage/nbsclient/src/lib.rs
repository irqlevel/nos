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
//! - A write to a place whose chunk is shared -- a snapshot's or a clone's
//!   too, as a copy-up pending's is -- is held while the chunk is copied up
//!   (N10): a new chunk asked of the mds (`CopyUp`), each active member of
//!   the set asked to copy the old one into it, and the mds told
//!   (`CopyDone`), after which the map names the new one and the write goes
//!   to it. A member that refuses as failed, or is silent, is asked out as
//!   for a write; a view behind the set, or a copy-up given up meanwhile --
//!   a join of the set begun -- has the copy-up asked again from its start
//!   once the map is read again: each step of it does nothing done before.
//! - A snapshot of the volume taken while it is attached pauses its writer
//!   (N10c2): asked by this host's mds, the session sends nothing new,
//!   closes every chunk it wrote -- each once the writes out to it are done,
//!   each close durable after its data -- and answers: a read, a flush or a
//!   close still out changes nothing the snapshot holds. Told to go on, it
//!   reads its map anew, at the entry the snapshot was applied at or after,
//!   before it writes: every chunk shared then. With no word in `PAUSE_NS`
//!   of having answered, it attaches anew (`Reattach`) -- a snapshot of its
//!   pause applied late would share chunks it went on writing, and the
//!   metadata refuses one at a generation no writer has. Its own attach
//!   only: one taken over meanwhile is refused, and fails, rather than take
//!   the volume back from the writer after it. A stop asked while paused is
//!   begun once the pause is over, as one asked while starting is: nothing
//!   let go of while the pause's snapshot may still be applied.

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
/// How long a session paused for a snapshot waits for word, once it has
/// answered, before it attaches again (N10c2).
pub const PAUSE_NS: u64 = 10_000_000_000;
/// What an operation's offset and length are whole multiples of.
pub const BLOCK: u64 = 4096;
/// Tags of the session's own operations -- a reconcile's reads and writes.
/// A driver's tag leaves the bit clear: `submit` and `stop` refuse one set.
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
    /// Behind the set: a Down answer came past the version after this view,
    /// so changes between are missing from it (N9d). No write is sent by
    /// it, nor done, until the map is read again.
    stale: bool,
}

/// A place's copy-up (N10): the writes to it held while the mds gives a new
/// chunk, each active member of the set copies the old one into it, and the
/// mds is told it done.
#[derive(Default)]
struct CopyUp {
    /// The ask of the mds out: `CopyUp`, or -- `done` -- `CopyDone`.
    ask: Option<CopyAsk>,
    /// The new chunk and its incarnation, once told.
    to: Option<(u32, u32)>,
    /// The members asked to copy, not answered.
    waiting: BTreeSet<u32>,
    /// To be asked again from its start once the map is read again.
    after_map: bool,
    /// The write pieces held for it.
    held: Vec<u64>,
}

struct CopyAsk {
    id: u64,
    bytes: Vec<u8>,
    sent: u64,
    again: Option<u64>,
    done: bool,
}

/// A pause for a snapshot of the volume (N10c2), as this host's mds asked
/// it: its number, where it is, and its answer, once sent -- and when.
struct Pause {
    pause: u64,
    stage: PauseStage,
    ask: Option<CopyAsk>,
    since: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PauseStage {
    /// Every chunk written closing, nothing new sent.
    Closing,
    /// Answered: told to go on, or attached again past `PAUSE_NS`.
    Answered,
    /// Told to go on: the map read anew first, asked at the snapshot's entry
    /// or after -- a read under way at its ask's.
    Map,
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
    /// The places whose chunk is shared (N10).
    shared: BTreeSet<u32>,
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
    /// A member's copy of a place's chunk into its copy-up's (N10).
    CopyUp(u32),
}

/// A request to a cs out: no bytes of it kept -- a piece's would be 256 KiB
/// a replica -- but what makes it again as it first went: its stamp, and
/// the piece's data, the driver's until the operation is done.
struct Sub {
    cs: u32,
    what: What,
    stamp: Stamp,
    sent: u64,
    /// When it first went: the cs is silent from then on.
    first: u64,
    /// Whether it has gone at all: one with no memory for it yet has not,
    /// and its cs is not silent for that.
    went: bool,
    /// When it is to go again: a refusal for now, a bounce, or no memory.
    again: Option<u64>,
}

/// What a request to a cs is made at, kept with it so that it goes again
/// as it first went though the map is read again meanwhile: the session's
/// generation, the map's index, and the chunks it names.
#[derive(Clone, Copy)]
struct Stamp {
    generation: u64,
    index: u64,
    chunk: Option<ChunkRef>,
    to: Option<ChunkRef>,
}

/// Why a request was not made: what it is of is gone -- its piece, or the
/// data of it -- or there is no memory for it now.
enum Unmade {
    Gone,
    NoMemory,
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
    /// The copy-ups under way, by place (N10).
    copies: BTreeMap<u32, CopyUp>,
    /// A pause for a snapshot, while one is (N10c2).
    pausing: Option<Pause>,
    /// Whether a request found no memory, since one last went: said once.
    starved: bool,
    /// The fuzzer's (#97): a write's request made until then finds no
    /// memory.
    #[cfg(feature = "starve")]
    starve_until: u64,
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
            copies: BTreeMap::new(), pausing: None, starved: false,
            #[cfg(feature = "starve")]
            starve_until: 0,
        }
    }

    /// The fuzzer's (#97): every write's request made until `until` finds no
    /// memory, as a kernel's heap may refuse one.
    #[cfg(feature = "starve")]
    pub fn starve_until(&mut self, until: u64) {
        self.starve_until = until;
    }

    /// Whether a request of `what` finds no memory now: the fuzzer's write
    /// starved (`starve_until`), and nothing else.
    #[cfg(feature = "starve")]
    fn starved_at(&self, what: What, now: u64) -> bool {
        now < self.starve_until
            && matches!(what, What::Piece(pid) if self.pieces.get(&pid).and_then(|p| self.ops.get(&p.op))
                .is_some_and(|o| o.kind == Kind::Write))
    }

    #[cfg(not(feature = "starve"))]
    fn starved_at(&self, _what: What, _now: u64) -> bool {
        false
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

    /// Whether it is paused for a snapshot (N10c2).
    pub fn paused(&self) -> bool {
        self.pausing.is_some()
    }

    /// Whether some set of the volume, as the session's view has it, writes
    /// cs `cs`: an active member of it, or its joiner.
    pub fn writes(&self, cs: u32) -> bool {
        self.map.written_cs().contains(&cs)
    }

    /// Whether set `set`, as the session's view has it, writes cs `cs`.
    pub fn writes_in(&self, set: u32, cs: u32) -> bool {
        self.map.sets.get(&set).is_some_and(|s| s.written_to(cs))
    }

    /// The session's view of the volume's sets, for a person: each set's
    /// version, its members and their states, its joiner, and whether the
    /// view is behind it.
    pub fn view(&self) -> String {
        let mut said = String::new();
        for (id, s) in &self.map.sets {
            let members: Vec<String> = (0..REPLICAS).map(|i| alloc::format!("{}{}", s.cs[i],
                if s.replicas[i] == ReplicaState::Active { "" } else { " down" })).collect();
            let _ = core::fmt::Write::write_fmt(&mut said, format_args!(
                "[set {} v{}: {}{}{}] ", id, s.version, members.join(", "),
                s.joiner.map_or(String::new(), |j| alloc::format!("; joiner {}", j)),
                if s.stale { "; behind" } else { "" }));
        }
        said
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
        if let Some(p) = &self.pausing {
            s += &alloc::format!("; paused for snapshot {} ({:?}) since {}", p.pause, p.stage, p.since);
        }
        for (place, c) in &self.copies {
            s += &alloc::format!("; copy-up of place {}: to {:?}, members {:?}, held {:?}, asking {:?}{}", place,
                                 c.to, c.waiting, c.held, c.ask.as_ref().map(|a| if a.done { "done" } else { "up" }),
                                 if c.after_map { ", after the map" } else { "" });
        }
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
        /* Detached or failed: an answer to an ask of before means nothing */
        if matches!(self.phase, Phase::Detached | Phase::Failed(_)) {
            return;
        }
        let Some(s) = Saying::decode(bytes) else { return };
        /* A pause for a snapshot (N10c2): asked unasked, and its end */
        match s.said {
            Said::Pause { pause } => return self.on_pause(pause, now, bufs, out),
            Said::GoOn { pause, index } => return self.on_go_on(pause, index, now, bufs, out),
            _ => {}
        }
        if self.mds.as_ref().is_some_and(|m| m.id == s.id) {
            self.on_mds_ask(s.said, now, bufs, out);
            return;
        }
        let copy = self.copies.iter().find(|(_, c)| c.ask.as_ref().is_some_and(|a| a.id == s.id)).map(|(&p, _)| p);
        if let Some(place) = copy {
            self.on_copy_said(place, s.said, now, bufs, out);
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
                /* Silent, and its set's last active replica: it stays in, the
                   request goes on being sent, and it is asked about again a
                   second on. Refused otherwise -- no replica of the set as the
                   mds has it, its place joined by another since (N9), or the
                   set no longer the volume's -- the view is behind: the map
                   read again, which drops the ask, and what waits for the cs
                   is done as it comes due, by a view that no longer writes
                   it there (found by fuzz/nbs's cluster target, N9d: a cs
                   lost for good, its places joined by others, and the
                   writer's stop waited for it for good) */
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
                if matches!(s.said, Said::Refused(_)) {
                    self.refresh(now, out);
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
                    if p.shared {
                        r.shared.insert(r.places.len() as u32);
                    }
                    r.places.push((p.chunk.chunk, p.chunk.incarnation, p.chunk.epoch.set));
                    r.sets.insert(p.chunk.epoch.set, SetInfo { version: p.chunk.epoch.version, cs: p.cs,
                                                               replicas: p.replicas, joiner: p.joiner,
                                                               stale: false });
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
                let before: BTreeMap<u32, u32> = self.map.sets.iter().map(|(&id, s)| (id, s.version)).collect();
                self.map = read;
                self.mapped(&before, now, bufs, out);
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

    /// The map read -- `before`, each set's version in the view before it:
    /// on with the session's start, or the parked pieces sent again, and the
    /// writes out by a view of their set that changed.
    fn mapped(&mut self, before: &BTreeMap<u32, u32>, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
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
                /* A cs the view writes now that the start has not fenced -- a
                   member joined meanwhile -- fenced: the start waits for it */
                if self.phase == Phase::Fencing && self.writable {
                    self.fence_unfenced(now, bufs, out);
                    self.fences_done(now, bufs, out);
                }
                /* A write out by a view of its set the map read finds changed:
                   sent again by the view now, waiting for the replicas it has
                   in -- a member or a joiner come since among them, one gone
                   not -- so that taking one out never leaves it done with
                   fewer than the set's view has */
                let changed: Vec<u64> = self.pieces.iter()
                    .filter(|(pid, p)| !self.parked.contains(pid)
                            && self.ops.get(&p.op).is_some_and(|o| o.kind != Kind::Read)
                            && self.map.set_of(p.place).is_some_and(|(set, s)| before.get(&set) != Some(&s.version)))
                    .map(|(&id, _)| id).collect();
                for pid in changed {
                    self.subs.retain(|_, s| s.what != What::Piece(pid));
                    self.send_piece(pid, now, bufs, out);
                }
                let parked = core::mem::take(&mut self.parked);
                for pid in parked {
                    self.send_piece(pid, now, bufs, out);
                }
                /* An ask to take a cs out of a set the view no longer writes it
                   in dropped: asked again, it would be refused for as long as
                   the session lives. What waits for that cs is settled as it
                   comes due: a request to a cs no set writes done as silent
                   (`ask_out`), a close it is not in the chunk's set for done
                   as refused so */
                let map = &self.map;
                self.downs.retain(|&(set, cs), _| map.sets.get(&set).is_some_and(|s| s.written_to(cs)));
                /* Copy-ups that waited for the map: asked again from their
                   start -- or, their chunk shared no longer as the map says
                   now, over */
                let again: Vec<u32> = self.copies.iter().filter(|(_, c)| c.after_map).map(|(&p, _)| p).collect();
                for place in again {
                    if let Some(c) = self.copies.get_mut(&place) {
                        c.after_map = false;
                    }
                    if self.map.shared.contains(&place) {
                        self.ask_copy(place, false, now, out);
                    } else if let Some(&(chunk, incarnation, _)) = self.map.places.get(place as usize) {
                        self.copied(place, chunk, incarnation, now, bufs, out);
                    }
                }
                /* Paused for a snapshot and told to go on: on, the map read
                   anew at the snapshot's entry or after */
                if self.pausing.as_ref().is_some_and(|p| p.stage == PauseStage::Map) {
                    self.unpause(now, bufs, out);
                }
            }
        }
    }

    /// A fence sent to every cs the view writes that has none, and none on
    /// its way: one come into the view since the start's fences went.
    /// Whether one is still not fenced.
    fn fence_unfenced(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) -> bool {
        let need: Vec<u32> = self.map.written_cs().into_iter().filter(|cs| !self.fenced.contains(cs)).collect();
        for &cs in &need {
            if !self.subs.values().any(|s| s.cs == cs && s.what == What::Fence) {
                self.send(cs, What::Fence, now, bufs, out);
            }
        }
        !need.is_empty()
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

    /// What a request of `what` is made at now: None where what it names
    /// is not in the map, or gone.
    fn stamp(&self, what: What) -> Option<Stamp> {
        let (chunk, to) = match what {
            What::Fence | What::Held | What::Flush(_) => (None, None),
            What::Close(place) => (Some(self.map.chunk(place)?), None),
            What::CopyUp(place) => {
                let from = self.map.chunk(place)?;
                let (chunk, incarnation) = self.copies.get(&place)?.to?;
                (Some(from), Some(ChunkRef { chunk, incarnation, epoch: from.epoch }))
            }
            What::Piece(pid) => (Some(self.map.chunk(self.pieces.get(&pid)?.place)?), None),
        };
        Some(Stamp { generation: if self.writable { self.generation } else { 0 }, index: self.index, chunk, to })
    }

    /// The bytes of a request of `what` made at `stamp`, its id left 0: a
    /// write's data from its operation's buffer -- the driver's, or the
    /// session's own for a reconcile's copy.
    fn request(&self, what: What, stamp: &Stamp, bufs: &dyn Buffers) -> Result<Vec<u8>, Unmade> {
        let (volume, generation, index) = (self.map.volume, stamp.generation, stamp.index);
        let named = |c: Option<ChunkRef>| c.ok_or(Unmade::Gone);
        let op = match what {
            What::Fence => Op::Fence,
            What::Held => Op::Held,
            What::Flush(_) => Op::Flush,
            What::Close(place) => Op::Close { chunk: named(stamp.chunk)?, position: place, index },
            What::CopyUp(place) => Op::CopyUp { from: named(stamp.chunk)?, to: named(stamp.to)?, position: place, index },
            What::Piece(pid) => {
                let p = self.pieces.get(&pid).ok_or(Unmade::Gone)?;
                let o = self.ops.get(&p.op).ok_or(Unmade::Gone)?;
                let chunk = named(stamp.chunk)?;
                match o.kind {
                    Kind::Read => Op::Read { chunk, position: p.place, index, offset: p.inside, len: p.len },
                    _ => {
                        let all = if o.tag & INTERNAL != 0 { &self.scratch[..] } else { bufs.data(o.tag) };
                        let data = all.get(p.at as usize..(p.at + p.len) as usize).ok_or(Unmade::Gone)?;
                        let op = Op::Write { chunk, position: p.place, index, offset: p.inside, data };
                        return Request { id: 0, volume, generation, op }.try_encode().ok_or(Unmade::NoMemory);
                    }
                }
            }
        };
        Ok(Request { id: 0, volume, generation, op }.encode())
    }

    /// A request to cs `cs` made and sent -- or, with no memory for it now,
    /// sent in a moment.
    fn send(&mut self, cs: u32, what: What, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(stamp) = self.stamp(what) else { return };
        let id = self.take_id();
        self.subs.insert(id, Sub { cs, what, stamp, sent: now, first: now, went: false, again: None });
        self.transmit(id, now, bufs, out);
    }

    /// Request `id` made again as it first was, and sent: its bytes the
    /// send's alone, kept by nobody once the driver has handed them on.
    fn transmit(&mut self, id: u64, now: u64, bufs: &dyn Buffers, out: &mut Vec<Out>) {
        let Some(s) = self.subs.get(&id) else { return };
        let (cs, what, stamp) = (s.cs, s.what, s.stamp);
        let made = if self.starved_at(what, now) { Err(Unmade::NoMemory) } else { self.request(what, &stamp, bufs) };
        match made {
            Ok(mut bytes) => {
                /* The id is the request's first eight bytes */
                bytes[..8].copy_from_slice(&id.to_le_bytes());
                out.push(Out::Send { to: To::Cs(cs), id, bytes });
                if let Some(s) = self.subs.get_mut(&id) {
                    s.sent = now;
                    s.again = None;
                    if !s.went {
                        s.went = true;
                        s.first = now;
                    }
                }
                self.starved = false;
            }
            Err(Unmade::Gone) => {
                self.subs.remove(&id);
            }
            Err(Unmade::NoMemory) => {
                if let Some(s) = self.subs.get_mut(&id) {
                    s.again = Some(now + AGAIN_NS);
                }
                if !self.starved {
                    self.starved = true;
                    out.push(Out::Say(alloc::format!("nbs: volume {}: no memory for a request to cs {} -- sent \
                                                      again in a moment", self.name, cs)));
                }
            }
        }
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
                /* No active replica in the view: it is behind the set, whose
                   last active member is never taken out -- a joiner become a
                   member since, its last one taken out of it meanwhile. The
                   read waits for the map read again, rather than fail (found
                   by the cluster target with joins, N9d: a reconcile's read
                   failed so, and the session with it); its deadline holds */
                let Some(cs) = pick else {
                    if !self.parked.contains(&pid) {
                        self.parked.push(pid);
                    }
                    self.refresh(now, out);
                    return;
                };
                if let Some(p) = self.pieces.get_mut(&pid) {
                    p.tried.insert(cs);
                    p.waiting = BTreeSet::from([cs]);
                }
                self.send(cs, What::Piece(pid), now, bufs, out);
            }
            _ => {
                /* A chunk shared, or a copy-up pending there (N10): the write
                   held while it is copied up */
                let place = p.place;
                if self.map.shared.contains(&place) {
                    self.hold_for_copy(pid, place, now, out);
                    return;
                }
                for c in self.copies.values_mut() {
                    c.held.retain(|&x| x != pid);
                }
                /* A view behind its set may lack a joiner: the write waits for
                   the map */
                if set.stale {
                    if !self.parked.contains(&pid) {
                        self.parked.push(pid);
                    }
                    return;
                }
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
            (What::CopyUp(place), Reply::Done) => {
                if let Some(c) = self.copies.get_mut(&place) {
                    c.waiting.remove(&cs);
                }
                self.copy_progress(place, now, out);
            }
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
            Refusal::Epoch { .. } | Refusal::StaleChunk { .. } | Refusal::NotTheMap | Refusal::NotHere => {
                self.subs.remove(&id);
                match what {
                    What::Piece(pid) => {
                        if !self.parked.contains(&pid) {
                            self.parked.push(pid);
                        }
                    }
                    /* The cs in the chunk's set neither as an active member
                       nor as its joiner, as the mds has it at the view's index
                       or after -- taken out, its place another's since:
                       nothing of the chunk there is to close (found by
                       fuzz/nbs's cluster target, N9d: the close sent again on
                       each map read and refused each time, and the writer's
                       stop waited for good) */
                    What::Close(place) if matches!(why, Refusal::NotHere) => self.closed(place, cs, now, bufs, out),
                    /* A copy-up's member by a view behind the set, or the
                       copy-up given up meanwhile: asked again from its start
                       once the map is read again */
                    What::CopyUp(place) => self.restart_copy(place),
                    other => self.resend_later(cs, other, now, bufs, out),
                }
                self.refresh(now, out);
            }
            Refusal::Behind { .. } | Refusal::NotReady | Refusal::NoMds => {
                if let Some(s) = self.subs.get_mut(&id) {
                    s.again = Some(now + AGAIN_NS);
                }
            }
            /* A member with no slot free yet for a copy-up's chunk, which the
               metadata counted room for: its slots of chunks gone are given
               back as it looks for them (N10) */
            Refusal::NoRoom if matches!(what, What::CopyUp(_)) => {
                if let Some(s) = self.subs.get_mut(&id) {
                    s.again = Some(now + RESEND_NS);
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
        /* Parked, it is sent again by the map read: done then, by its view */
        if p.waiting.is_empty() && !self.parked.contains(&pid) {
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
            What::CopyUp(place) => match self.map.set_of(place) {
                Some((set, info)) if info.written_to(cs) => alloc::vec![set],
                _ => Vec::new(),
            },
            _ => self.map.sets.iter().filter(|(_, s)| s.written_to(cs)).map(|(&id, _)| id).collect(),
        };
        /* A copy-up's member the view has out of the set: not waited on */
        if let (true, What::CopyUp(place)) = (sets.is_empty(), what) {
            self.drop_subs(cs, what);
            if let Some(c) = self.copies.get_mut(&place) {
                c.waiting.remove(&cs);
            }
            self.copy_progress(place, now, out);
            return;
        }
        /* A cs-wide request to a cs no set writes now: done, not waited on */
        if sets.is_empty() && !matches!(what, What::Piece(_)) {
            self.cs_wide_done(cs, now, bufs, out);
            return;
        }
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
        let Some(at) = self.map.sets.get(&set).map(|s| s.version) else { return };
        /* As old as the view or older: the view has what was done then */
        if version <= at {
            self.cs_wide_done(cs, now, bufs, out);
            return;
        }
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
            /* Each change raises a set's version by one: one past the view is
               this taking out alone. Further, the view lacks changes between
               -- a join begun -- and a write by it, at the version answered,
               taken where the copy has the chunk already, would miss the
               joiner: the set is read again, its writes held until then */
            if version == at + 1 {
                s.version = version;
            } else {
                s.stale = true;
            }
        }
        if self.map.sets.get(&set).is_some_and(|s| s.stale) {
            let writes: Vec<u64> = self.pieces.iter()
                .filter(|(_, p)| self.map.set_of(p.place).is_some_and(|(s, _)| s == set)
                        && self.ops.get(&p.op).is_some_and(|o| o.kind != Kind::Read))
                .map(|(&id, _)| id).collect();
            for pid in writes {
                if !self.parked.contains(&pid) {
                    self.parked.push(pid);
                }
            }
            self.refresh(now, out);
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
        /* Copy-ups of that set waiting for it: done without it, which copies
           the new chunk when it comes back, as any chunk written while it was
           out (N10) -- but by a view behind the set, asked again from their
           start once the map is read again: the members it asked may not be
           the set's, and one it lacks has copied nothing (found by fuzz/nbs's
           cluster target, N10c: the one member asked, down, taken out, and the
           copy-up done by none, the new chunk zeros under the writes after) */
        let stale = self.map.sets.get(&set).is_some_and(|s| s.stale);
        let copies: Vec<u32> = self.copies.iter()
            .filter(|(&place, c)| c.waiting.contains(&cs) && self.map.set_of(place).is_some_and(|(s, _)| s == set))
            .map(|(&place, _)| place).collect();
        for place in copies {
            if stale {
                self.restart_copy(place);
                continue;
            }
            self.drop_subs(cs, What::CopyUp(place));
            if let Some(c) = self.copies.get_mut(&place) {
                c.waiting.remove(&cs);
            }
            self.copy_progress(place, now, out);
        }
        self.cs_wide_done(cs, now, bufs, out);
    }

    /// The cs-wide requests to `cs` done, where no set of the volume writes it
    /// any more -- taken out before they went, or after: none is needed of a
    /// cs out of every set, and one no answer comes to would wait for good,
    /// no set being left to ask it out of.
    fn cs_wide_done(&mut self, cs: u32, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if self.map.written_cs().contains(&cs) {
            return;
        }
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
                What::CopyUp(place) => {
                    if let Some(c) = self.copies.get_mut(&place) {
                        c.waiting.remove(&cs);
                    }
                    self.copy_progress(place, now, out);
                }
                What::Piece(_) => {}
            }
        }
    }

    /* ---- copy-ups (N10) ---- */

    /// Write piece `pid` held for its place's copy-up -- begun, where none is
    /// under way.
    fn hold_for_copy(&mut self, pid: u64, place: u32, now: u64, out: &mut Vec<Out>) {
        let c = self.copies.entry(place).or_default();
        if !c.held.contains(&pid) {
            c.held.push(pid);
        }
        if c.ask.is_none() && c.to.is_none() && !c.after_map {
            self.ask_copy(place, false, now, out);
        }
    }

    /// Place `place`'s copy-up asked of the mds: a new chunk for it, or --
    /// `done` -- the copy done.
    fn ask_copy(&mut self, place: u32, done: bool, now: u64, out: &mut Vec<Out>) {
        let (name, generation) = (self.name.clone(), self.generation);
        let ask = match self.copies.get(&place).and_then(|c| c.to) {
            Some((chunk, _)) if done => Ask::CopyDone { name, place, generation, chunk },
            _ => Ask::CopyUp { name, place, generation },
        };
        let id = self.take_id();
        let bytes = Asking { id, ask }.encode();
        out.push(Out::Send { to: To::Mds, id, bytes: bytes.clone() });
        if let Some(c) = self.copies.get_mut(&place) {
            c.ask = Some(CopyAsk { id, bytes, sent: now, again: None, done });
        }
    }

    /// What the mds said of place `place`'s copy-up.
    fn on_copy_said(&mut self, place: u32, said: Said, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let done = self.copies.get(&place).and_then(|c| c.ask.as_ref()).is_some_and(|a| a.done);
        match said {
            Said::Chunk { chunk, incarnation, index } => {
                self.index = self.index.max(index);
                let at = self.map.places.get(place as usize).map(|&(c, i, _)| (c, i));
                if done || at == Some((chunk, incarnation)) {
                    /* Done -- or the chunk shared no longer: written as it is */
                    self.copied(place, chunk, incarnation, now, bufs, out);
                    return;
                }
                if let Some(c) = self.copies.get_mut(&place) {
                    c.ask = None;
                    c.to = Some((chunk, incarnation));
                }
                self.copy_members(place, now, bufs, out);
            }
            /* No leader, the mds behind, or the set being joined: asked again */
            Said::NotNow(_) | Said::Behind { .. } => {
                if let Some(a) = self.copies.get_mut(&place).and_then(|c| c.ask.as_mut()) {
                    a.again = Some(now + AGAIN_NS);
                }
            }
            Said::NotTheWriter => self.fail("the volume was attached elsewhere: this attach is fenced", out),
            /* Its copy-up given up meanwhile -- a join of the set begun: asked
               again from its start */
            Said::Refused(_) if done => {
                if let Some(c) = self.copies.get_mut(&place) {
                    c.ask = None;
                    c.to = None;
                }
                self.ask_copy(place, false, now, out);
            }
            /* No room for a new chunk, or none of the volume's: the writes held
               fail, said */
            Said::Refused(why) => {
                out.push(Out::Say(alloc::format!("nbs: volume {}: place {}'s chunk not copied up: {}", self.name,
                                                 place, why)));
                let held = self.copies.remove(&place).map(|c| c.held).unwrap_or_default();
                for pid in held {
                    self.piece_done(pid, false, now, bufs, out);
                }
            }
            _ => {}
        }
    }

    /// Each active member of place `place`'s set, as the view has it, asked to
    /// copy the place's chunk into its copy-up's: a view behind the set, or
    /// with none, has it asked again once the map is read again.
    fn copy_members(&mut self, place: u32, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let members: Vec<u32> = match self.map.set_of(place) {
            Some((_, set)) if !set.stale => set.cs.iter().copied().filter(|&c| set.active(c)).collect(),
            _ => Vec::new(),
        };
        if members.is_empty() {
            self.restart_copy(place);
            self.refresh(now, out);
            return;
        }
        if let Some(c) = self.copies.get_mut(&place) {
            c.waiting = members.iter().copied().collect();
        }
        for cs in members {
            self.send(cs, What::CopyUp(place), now, bufs, out);
        }
    }

    /// Place `place`'s copy-up told done to the mds once every member asked
    /// has copied, or is out.
    fn copy_progress(&mut self, place: u32, now: u64, out: &mut Vec<Out>) {
        let Some(c) = self.copies.get(&place) else { return };
        if c.to.is_some() && c.ask.is_none() && c.waiting.is_empty() && !c.after_map {
            self.ask_copy(place, true, now, out);
        }
    }

    /// Place `place`'s copy-up to be asked again from its start once the map
    /// is read again: its members' copies of now dropped.
    fn restart_copy(&mut self, place: u32) {
        self.subs.retain(|_, s| s.what != What::CopyUp(place));
        if let Some(c) = self.copies.get_mut(&place) {
            c.ask = None;
            c.to = None;
            c.waiting.clear();
            c.after_map = true;
        }
    }

    /// Place `place`'s copy-up over, chunk `chunk` the place's now: the map
    /// names it, shared no longer, and the writes held go to it.
    fn copied(&mut self, place: u32, chunk: u32, incarnation: u32, now: u64, bufs: &mut dyn Buffers,
              out: &mut Vec<Out>) {
        if let Some(p) = self.map.places.get_mut(place as usize) {
            p.0 = chunk;
            p.1 = incarnation;
        }
        self.map.shared.remove(&place);
        self.subs.retain(|_, s| s.what != What::CopyUp(place));
        let held = self.copies.remove(&place).map(|c| c.held).unwrap_or_default();
        for pid in held {
            self.send_piece(pid, now, bufs, out);
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
               before it -- and nothing of the driver's while paused for a
               snapshot (N10c2) */
            let serving = (matches!(self.phase, Phase::Serving | Phase::Stopping) && self.pausing.is_none())
                || o.tag & INTERNAL != 0;
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
        self.pause_step(now, bufs, out);
    }

    /// Operation `id` out: a read's or a write's pieces sent, a flush's.
    fn launch(&mut self, id: u64, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(o) = self.ops.get_mut(&id) else { return };
        o.out = true;
        let (kind, offset, len) = (o.kind, o.offset, o.len);
        if kind.flushes() {
            /* A cs taken out of every set it was written in holds nothing the
               volume counts on: not flushed */
            let written = self.map.written_cs();
            let to: BTreeSet<u32> = core::mem::take(&mut self.dirty).into_iter().filter(|cs| written.contains(cs)).collect();
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
        self.pause_step(now, bufs, out);
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

    /* ---- a pause for a snapshot (N10c2) ---- */

    /// This host's mds asks the session to pause for a snapshot of its
    /// volume: a writer serving takes it -- one starting, stopping, or
    /// paused already, not: the mds is answered by none, and gives up.
    fn on_pause(&mut self, pause: u64, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if !self.writable || self.phase != Phase::Serving || self.pausing.is_some() {
            return;
        }
        self.pausing = Some(Pause { pause, stage: PauseStage::Closing, ask: None, since: now });
        self.pause_step(now, bufs, out);
    }

    /// The pause on: every chunk written closed -- one a piece is out to once
    /// it is done, as a place busy stays open; each close durable after its
    /// data -- and once they are, answered.
    fn pause_step(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if self.pausing.as_ref().map(|p| p.stage) != Some(PauseStage::Closing) {
            return;
        }
        self.close_idle(now, bufs, out, true);
        if !self.closing.is_empty() || !self.open.is_empty() {
            return;
        }
        let Some(pause) = self.pausing.as_ref().map(|p| p.pause) else { return };
        let id = self.take_id();
        let bytes = Asking { id, ask: Ask::Paused { name: self.name.clone(), generation: self.generation, pause } }
            .encode();
        out.push(Out::Send { to: To::Mds, id, bytes: bytes.clone() });
        if let Some(p) = self.pausing.as_mut() {
            p.stage = PauseStage::Answered;
            p.ask = Some(CopyAsk { id, bytes, sent: now, again: None, done: false });
            p.since = now;
        }
    }

    /// Told to go on from pause `pause`: answered, the map read anew at the
    /// snapshot's entry `index` or after, before anything is written; not
    /// answered yet -- the mds gave up, and took no snapshot -- on at once.
    fn on_go_on(&mut self, pause: u64, index: u64, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        let Some(stage) = self.pausing.as_ref().filter(|p| p.pause == pause).map(|p| p.stage) else { return };
        match stage {
            PauseStage::Answered => {
                self.index = self.index.max(index);
                if let Some(p) = self.pausing.as_mut() {
                    p.stage = PauseStage::Map;
                    p.ask = None;
                }
                /* A read under way may be of before the snapshot: asked anew,
                   its answer that of an ask no longer out */
                self.reading = Some(Map::default());
                self.ask_map(0, now, out);
            }
            PauseStage::Map => {}
            PauseStage::Closing => self.unpause(now, bufs, out),
        }
    }

    /// The pause over: on -- and a stop asked meanwhile begun.
    fn unpause(&mut self, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        self.pausing = None;
        self.serve(now, bufs, out);
    }

    /// Paused and answered, with no word in `PAUSE_NS`: its own attach anew,
    /// at a new generation -- a snapshot of the pause applied late is refused
    /// then, where going on would have it share chunks written since. Its
    /// own only: a session taken over meanwhile is refused, and fails.
    fn attach_again(&mut self, now: u64, out: &mut Vec<Out>) {
        out.push(Out::Say(alloc::format!("nbs: volume {}: paused for a snapshot, and no word of it in {} s -- attached \
                                          again", self.name, PAUSE_NS / 1_000_000_000)));
        self.pausing = None;
        self.reading = None;
        self.subs.clear();
        self.downs.clear();
        self.copies.clear();
        self.fenced.clear();
        self.held.clear();
        self.phase = Phase::Attaching;
        self.attach_from = now;
        self.token = self.token.wrapping_add(1).max(1);
        let ask = Ask::Reattach { name: self.name.clone(), generation: self.generation, token: self.token };
        self.ask_mds(MdsWant::Attach, ask, now, out);
    }

    /* ---- the stop ---- */

    /// The session stopped once the operations out are done: a flush, every
    /// chunk written closed, the volume let go of -- operation `tag` done
    /// then. `tag` is the driver's, as an operation's is: one of the
    /// session's own is refused -- its flush would be done as an operation
    /// of the session's, told to nobody.
    pub fn stop(&mut self, tag: u64, now: u64, bufs: &mut dyn Buffers, out: &mut Vec<Out>) {
        if tag & INTERNAL != 0 {
            out.push(Out::Done { tag, ok: false });
            return;
        }
        if matches!(self.phase, Phase::Failed(_) | Phase::Detached) {
            out.push(Out::Done { tag, ok: matches!(self.phase, Phase::Detached) });
            return;
        }
        /* Asked again: the first stands */
        if self.stop.is_some() {
            out.push(Out::Done { tag, ok: false });
            return;
        }
        self.stop = Some(tag);
        /* Still starting, or paused for a snapshot (N10c2): begun once it
           serves -- the start leaves the volume as the stop wants it, fenced
           and reconciled; the pause's end, with nothing let go of while its
           snapshot may still be applied, and what was asked before the stop
           done before it, whether it goes on or attaches anew */
        if self.phase != Phase::Serving || self.pausing.is_some() {
            return;
        }
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
        self.copies.clear();
        self.pausing = None;
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
        for a in self.copies.values_mut().filter_map(|c| c.ask.as_mut()).chain(self.pausing.iter_mut().filter_map(|p| p.ask.as_mut())) {
            let due = a.again.map_or(now >= a.sent + RESEND_NS, |t| now >= t);
            if due {
                a.sent = now;
                a.again = None;
                out.push(Out::Send { to: To::Mds, id: a.id, bytes: a.bytes.clone() });
            }
        }
        if self.pausing.as_ref().is_some_and(|p| p.stage == PauseStage::Answered && now >= p.since + PAUSE_NS) {
            self.attach_again(now, out);
            return;
        }
        let mut silent = Vec::new();
        let mut due = Vec::new();
        for (&id, s) in self.subs.iter() {
            if s.again.map_or(now >= s.sent + RESEND_NS, |a| now >= a) {
                due.push(id);
            }
            if s.went && now >= s.first + SILENT_NS {
                silent.push((s.cs, s.what));
            }
        }
        for id in due {
            self.transmit(id, now, &*bufs, out);
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
