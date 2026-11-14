//! The `join` target (N9b): one join's copy -- nbswire's `copy`, as
//! nbs-cs.ko runs it -- from a source, an active member of the set, onto the
//! joiner. Both are nbscs's engine over fuzz/nbs's disks, served by
//! nbswire's `serve` as a cs serves, and their hosts' mds is played by the
//! target from the set's state it keeps. The network delays, reorders,
//! drops and repeats, while:
//!
//! - writers of the map now write both, up to three writes apart in flight
//!   at once, each delivery anywhere among the copy's messages -- one to the
//!   joiner long before its other to the source;
//! - writers of a map from before the join -- of any version since the
//!   member went down -- write the source alone: taken where the source
//!   still knows the chunk at that epoch, refused once the copy has moved
//!   it on, and then written again as a writer of the map now;
//! - a writer taken over since, of a map from when a member back was still
//!   active, writes it now and then: the source, fenced against it, refuses
//!   the write, and so must the joiner, as of an epoch before its join --
//!   the writer after it has not fenced it yet;
//! - the set moves on now and then (another member down), and the copy's
//!   view is refreshed a while after;
//! - the join is given up now and then and made again under a new version,
//!   the writes between going to the source alone, and the copy begun a
//!   while after the join, the joiner's first writes vouched for before it;
//! - a sector of the chunk being copied rots now and then on the joiner,
//!   which the copy's check must find.
//!
//! In a third of the runs the joiner's power fails mid-copy, between any
//! two of its writes and flushes: it comes back with what its disk kept,
//! and its join is given up -- a joiner never resumes one -- and made again
//! under a new version.
//!
//! In about a third of the runs a member back joins its own place. It
//! holds the set's chunks as they were when it went down: some were written
//! since on the source alone, and some it found open, its last writes lost
//! to a power cut. Half of those hold a chunk of the set the source has none
//! of -- written by a writer cut off with the member, never reaching the
//! others -- which the copy drops as it ends (the review of 2026-11: kept,
//! it came up as the set's once the member was active again).
//!
//! A tenth of the runs each set out to reach one race a random run seldom
//! puts together (`Focus`): the power failing inside a close; a writer of a
//! map before the join at the epoch a chunk just closed was left at; one
//! between a member back's listing and its sums, at the epoch the sums of a
//! join before left the chunk at; a write on the joiner whose delivery to
//! the source is held past the copy, made before the copy began; and a
//! chunk found open part copied as the join is given up.
//!
//! At the end -- the copy done, every write delivered -- the joiner holds
//! every chunk of the set the source does, each the source's byte for byte;
//! and, the join over, a writer taken over since, of a map from when a
//! member back was still active, is refused every chunk the joiner holds,
//! at every version before the join: the copy moved each one's epoch on,
//! those its sums found the same among them (N13).

use std::cell::RefCell;
use std::rc::Rc;

use nbscs::{ChunkRef, Epoch, Identity, Store, SLOT};
use nbswire::copy::{self, Copy};
use nbswire::serve::{serve, AskNow, Requests};
use nbswire::{Answer, JoinView, Op, Question, Refusal, Reply, Request, Told};

use crate::cs::{Disk, Handle};
use crate::rng::{mix, Rng};

const CLUSTER: [u8; 16] = [3; 16];
const SET: u32 = 5;
const VOLUME: u32 = 9;
const GEN: u64 = 1;
/// A member back was active until `UNTIL`; the source was written alone
/// at the versions after it; the join is at `SINCE`.
const UNTIL: u32 = 3;
const SINCE: u32 = 5;
const SOURCE: u32 = 0;
const JOINER: u32 = 1;
const DISK_SLOTS: u64 = 12;
const SECTOR: u64 = 4096;
const MS: u64 = 1_000_000;
/// Steps the faults may come in; past them, the copy has this many more to
/// end in.
const FAULT_STEPS: u64 = 3000;
const END_STEPS: u64 = 40_000;

/// A message on its way: a request to the source, or its answer.
enum Msg {
    Request(Vec<u8>),
    Answer(Vec<u8>),
}

/// A write on its way: where, what, and the stores it is still to reach --
/// `hold`, to the source only once the copy is done.
struct Write {
    chunk: u32,
    offset: u32,
    data: Vec<u8>,
    version: u32,
    to_source: bool,
    to_joiner: bool,
    hold: bool,
}

/// The race a run sets out to reach, where it is one a random run seldom
/// puts together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    None,
    /// The power failing inside a close: armed two writes ahead as a sums
    /// answer comes.
    Close,
    /// A writer of a map before the join at the epoch the source knows a
    /// chunk at, as the copy closes it: the pull is to have moved it on.
    Moved,
    /// A member back given up once it has held a chunk to its source's by
    /// sums, and writers of maps before at the epochs the sums left, once
    /// the next copy has listed.
    Held,
    /// A write on the joiner before the next copy begins, held from the
    /// source past it: marked as the joiner's first write, or written over.
    Marks,
    /// A member back with every chunk found open, given up at its first
    /// pull: the next copy is to hold them all to the source's, what the
    /// cut lost among what differs.
    Suspect,
}

struct World {
    r: Rng,
    now: u64,
    trace: bool,
    source: Store<Handle>,
    joiner: Store<Handle>,
    joiner_disk: Rc<RefCell<Disk>>,
    sv: Requests,
    jv: Requests,
    /// The set as the mds has it: its version, its join's `since`, whether
    /// the join is on.
    version: u32,
    since: u32,
    joining: bool,
    own: bool,
    chunks: Vec<u32>,
    copy: Option<Copy>,
    /// A copy to be made at this step: the joiner's mds tells it late.
    copy_at: Option<u64>,
    /// The copy's view to be refreshed at this step.
    refresh_at: Option<u64>,
    next_id: u64,
    net: Vec<Msg>,
    writes: Vec<Write>,
    seq: u64,
    /// The chunks a sector of which rotted: once each, three at most -- the
    /// copy's check gives up a chunk that differs round after round.
    rotted: Vec<u32>,
    focus: Focus,
    /// Held writes delivered to the source from now on.
    release: bool,
    /// The chunk the copy worked on at the last step, and how many it had
    /// closed.
    last_working: Option<u32>,
    closed: u64,
    /// The chunks the copy of the join now has closed: what a cut must leave
    /// as they were, the source's -- one a join given up since closed may
    /// differ from the source by the writes between, to the source alone.
    closed_now: Vec<u32>,
    /// Writes that reached the source and not the joiner, whose power failed
    /// as they came: chunk, offset, length -- what the check after the cut
    /// leaves out.
    missed: Vec<(u32, u32, usize)>,
    /// The focus's race set up.
    injected: bool,
    /// The writes of a writer taken over (`zombie_write`), drawn of their
    /// own so that the runs go as they did, and their numbers.
    zr: Rng,
    zseq: u64,
    stats: [u64; 8],
}

/// `len` bytes no other write has: each 512 of them the write's number and
/// the sector's.
fn data(seq: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    for (k, piece) in v.chunks_mut(512).enumerate() {
        let tag = mix(seq, k as u64).to_le_bytes();
        for (i, b) in piece.iter_mut().enumerate() {
            *b = tag[i % 8] ^ (i as u8);
        }
    }
    v
}

fn cref(chunk: u32, version: u32) -> ChunkRef {
    ChunkRef { chunk, incarnation: 1, epoch: Epoch { set: SET, version } }
}

impl World {
    /// The source's mds: a chunk at the set's version now vouched for; the
    /// joiner vouched for at it while its join is on.
    fn source_mds(version: u32, joining: bool, q: Question) -> Option<Told> {
        let known = Epoch { set: SET, version };
        Some(match q {
            Question::Verify { chunk, .. } if chunk.epoch == known => Told::Verified,
            Question::Verify { .. } => Told::Refused(Refusal::Epoch { known }),
            Question::VerifyJoin { joiner, .. } if !joining || joiner != JOINER => Told::Refused(Refusal::NotTheJoiner),
            Question::VerifyJoin { version: v, .. } if v == version => Told::Verified,
            Question::VerifyJoin { .. } => Told::Refused(Refusal::Epoch { known }),
            _ => Told::Refused(Refusal::NoMds),
        })
    }

    /// The joiner's mds: a write at the set's version now vouched for as
    /// the joiner's while its join is on.
    fn joiner_mds(version: u32, since: u32, joining: bool, q: Question) -> Option<Told> {
        let known = Epoch { set: SET, version };
        Some(match q {
            Question::Verify { chunk, .. } if chunk.epoch != known => Told::Refused(Refusal::Epoch { known }),
            Question::Verify { writing: true, .. } if joining => Told::VerifiedJoining { since },
            _ => Told::Refused(Refusal::NotHere),
        })
    }

    fn view(&self) -> JoinView {
        JoinView { set: SET, place: 0, since: self.since, version: self.version, own: self.own,
                   until: if self.own { UNTIL } else { 0 }, sources: vec![(SOURCE, String::from("source"))],
                   index: 1, fences: vec![(VOLUME, GEN)] }
    }

    /// A client's write of the request `op` done on one store as its cs
    /// serves it.
    fn serve_write(&mut self, to_joiner: bool, chunk: u32, offset: u32, data: &[u8], version: u32)
        -> Result<(), Refusal> {
        let req = Request { id: 0, volume: VOLUME, generation: GEN,
                            op: Op::Write { chunk: cref(chunk, version), position: 0, index: 1, offset, data } };
        let (v, s, j) = (self.version, self.since, self.joining);
        let done = if to_joiner {
            nbscs::now(serve(&self.joiner, &self.jv, &req, &AskNow::new(|q| World::joiner_mds(v, s, j, q))))
        } else {
            nbscs::now(serve(&self.source, &self.sv, &req, &AskNow::new(|q| World::source_mds(v, j, q))))
        };
        done.map(|_| ())
    }

    /// A new write: of the map now -- both stores while the join is on, the
    /// source alone while it is not -- or, `old`, of a map before the join's
    /// version: the source alone. None where it would overlap one in flight,
    /// which a client orders after it.
    fn new_write(&mut self, old: bool) {
        let chunk = self.chunks[self.r.below(self.chunks.len() as u64) as usize];
        let sectors = SLOT / SECTOR;
        let first = self.r.below(sectors);
        let n = 1 + self.r.below(16.min(sectors - first));
        let (from, to) = ((first * SECTOR) as u32, ((first + n) * SECTOR) as u32);
        if self.writes.iter().any(|w| w.chunk == chunk && w.offset < to && from < w.offset + w.data.len() as u32) {
            return;
        }
        self.seq += 1;
        /* A writer whose map is of a version the member was down at, or of a
           join before this one -- most often the very epoch the source knows
           the chunk at, which it takes as it is */
        let lo = if self.own { UNTIL } else { 1 };
        let known = self.source.holding(chunk).map(|h| h.known.version).filter(|&v| v >= lo && v < self.since);
        let version = match known {
            _ if !old => self.version,
            Some(v) if self.r.chance(60) => v,
            _ => self.r.range(u64::from(lo), u64::from(self.since - 1)) as u32,
        };
        self.writes.push(Write { chunk, offset: from, data: data(self.seq, (n * SECTOR) as usize), version,
                                 to_source: true, to_joiner: self.joining && !old, hold: false });
        self.stats[if old { 1 } else { 0 }] += 1;
    }

    /// A writer of a map before the join's at version `version` -- the epoch
    /// the source knows `chunk` at -- its write delivered to the source now.
    fn old_write_now(&mut self, chunk: u32, version: u32) -> Result<(), String> {
        let first = self.r.below(SLOT / SECTOR - 16);
        let n = 1 + self.r.below(16);
        let (from, to) = ((first * SECTOR) as u32, ((first + n) * SECTOR) as u32);
        if self.writes.iter().any(|w| w.chunk == chunk && w.offset < to && from < w.offset + w.data.len() as u32) {
            return Ok(());
        }
        self.seq += 1;
        self.writes.push(Write { chunk, offset: from, data: data(self.seq, (n * SECTOR) as usize), version,
                                 to_source: true, to_joiner: false, hold: false });
        self.stats[1] += 1;
        let at = self.writes.len() - 1;
        self.deliver_write_at(at)
    }

    /// A writer taken over since, of a map from when a member back was still
    /// active -- version `UNTIL` or before -- writes it: the source, fenced
    /// against it, refuses the write, and the joiner, which the writer after
    /// it has not fenced yet, is to refuse it too, as of an epoch before its
    /// join. Taken, it would be marked as a client's, kept over the set's by
    /// the copy, and written again by nobody (found by the cluster target in
    /// N9d, which came on it at one seed in a thousand: aimed here).
    fn zombie_write(&mut self) -> Result<(), String> {
        let chunk = self.chunks[self.zr.below(self.chunks.len() as u64) as usize];
        let first = self.zr.below(SLOT / SECTOR - 16);
        let n = 1 + self.zr.below(16);
        let version = self.zr.range(1, u64::from(UNTIL)) as u32;
        self.zseq += 1;
        let d = data((0x7a6f_6d62 << 32) | self.zseq, (n * SECTOR) as usize);
        let done = self.serve_write(true, chunk, (first * SECTOR) as u32, &d, version);
        if self.trace {
            eprintln!("a writer taken over writes chunk {} at {} to the joiner, joining since {}: {:?}", chunk, version,
                      self.since, done);
        }
        if done.is_ok() {
            return Err(format!("the joiner, joining since {}, took a write of chunk {} at epoch {} of a writer taken \
                                over: its copy would keep it over the set's", self.since, chunk, version));
        }
        self.stats[7] += 1;
        Ok(())
    }

    /// The join over -- the joiner's marks dropped, as nbs-cs's at its end --
    /// a writer taken over since, of a map from when a member back was still
    /// active, writes every chunk the joiner holds, at every version before
    /// the join: refused each time. The joiner is a member now, which the
    /// writer after it has not fenced yet; taken, such a write would be on
    /// it alone, the members fenced against that writer refusing it. A
    /// member back held what its sums found the same at the epoch it last
    /// knew, and took it there (found by the cluster target in N13, at one
    /// seed in a thousand: aimed here).
    fn zombie_after(&mut self) -> Result<(), String> {
        self.joiner.leave(SET);
        self.joining = false;
        let held: Vec<u32> = self.joiner.holdings_in(SET, 0, nbswire::LIST_MAX).iter().map(|e| e.0).collect();
        for chunk in held {
            for version in 1..SINCE {
                self.zseq += 1;
                let d = data((0x7a6f_6d62 << 32) | self.zseq, SECTOR as usize);
                if self.serve_write(true, chunk, 0, &d, version).is_ok() {
                    return Err(format!("the join over, the joiner took a write of chunk {} at epoch {}, before its \
                                        join at {}, of a writer taken over", chunk, version, self.since));
                }
            }
        }
        Ok(())
    }

    /// Every write on its way delivered, but those held from the source:
    /// what a writer finishes as its join is given up.
    fn drain(&mut self) -> Result<(), String> {
        while let Some(at) = self.writes.iter().position(|w| !(w.hold && !self.release && !w.to_joiner)) {
            self.deliver_write_at(at)?;
        }
        Ok(())
    }

    /// One of the writes on their way delivered to one of the stores it is
    /// still to reach. Refused for an epoch gone, it is written again as a
    /// writer of the map now writes it: to both while the join is on.
    fn deliver_write(&mut self) -> Result<(), String> {
        if self.writes.is_empty() {
            return Ok(());
        }
        let at = self.r.below(self.writes.len() as u64) as usize;
        self.deliver_write_at(at)
    }

    /// The write at `at` delivered: to the joiner first where it is held.
    fn deliver_write_at(&mut self, at: usize) -> Result<(), String> {
        let w = self.writes.swap_remove(at);
        let held = w.hold && !self.release;
        if held && !w.to_joiner {
            self.writes.push(w);
            return Ok(());
        }
        let to_joiner = w.to_joiner && (!w.to_source || held || self.r.chance(70));
        let done = self.serve_write(to_joiner, w.chunk, w.offset, &w.data, w.version);
        if self.trace {
            eprintln!("write {:x} of chunk {} sectors {}..{} at {} to the {}: {:?}", mix(w.data.len() as u64,
                      u64::from(w.data[0])), w.chunk, w.offset / 512, (w.offset as usize + w.data.len()) / 512,
                      w.version, if to_joiner { "joiner" } else { "source" }, done);
        }
        let mut w = w;
        match done {
            Ok(()) => {
                if to_joiner {
                    w.to_joiner = false;
                } else {
                    w.to_source = false;
                }
            }
            Err(Refusal::Epoch { known }) => {
                if self.trace {
                    eprintln!("write of chunk {} at {} refused: the epoch is {}", w.chunk, w.version, known.version);
                }
                self.stats[2] += 1;
                w.version = self.version;
                w.to_source = true;
                w.to_joiner = self.joining;
            }
            /* Not the joiner now, or its power failed: written where it is
               still one */
            Err(Refusal::NotHere) if to_joiner => w.to_joiner = false,
            Err(_) if to_joiner && self.joiner_disk.borrow().dead() => {
                w.to_joiner = false;
                self.missed.push((w.chunk, w.offset, w.data.len()));
            }
            Err(why) => return Err(format!("a write of chunk {} refused: {:?}", w.chunk, why)),
        }
        if w.to_source || w.to_joiner {
            self.writes.push(w);
        }
        Ok(())
    }

    /// A message delivered: dropped, repeated, or done.
    fn deliver(&mut self) -> Result<(), String> {
        if self.net.is_empty() {
            return Ok(());
        }
        let at = self.r.below(self.net.len() as u64) as usize;
        let m = self.net.swap_remove(at);
        if self.r.chance(2) {
            return Ok(());
        }
        match m {
            Msg::Request(bytes) => {
                let Some(req) = Request::decode(&bytes) else { return Err(String::from("a request that is none")) };
                let (v, j) = (self.version, self.joining);
                let done = nbscs::now(serve(&self.source, &self.sv, &req, &AskNow::new(|q| World::source_mds(v, j, q))));
                let reply = match &done {
                    Ok(d) => d.reply(),
                    Err(why) => Reply::Refused(*why),
                };
                if self.trace {
                    let what = match &req.op {
                        Op::Pull { chunk, offset, len, .. } => format!("pull of chunk {} sectors {}..{} at {}",
                            chunk.chunk, offset / 512, (offset + len) / 512, chunk.epoch.version),
                        Op::Sums { chunk, .. } => format!("sums of chunk {} at {}", chunk.chunk, chunk.epoch.version),
                        other => format!("{:?}", other),
                    };
                    let said = match &reply {
                        Reply::Data(_) => String::from("data"),
                        other => format!("{:?}", other),
                    };
                    eprintln!("source served {}: {}", what, said);
                }
                let bytes = Answer { id: req.id, reply }.encode();
                if self.r.chance(2) {
                    self.net.push(Msg::Answer(bytes.clone()));
                }
                self.net.push(Msg::Answer(bytes));
            }
            Msg::Answer(bytes) => {
                let Some(a) = Answer::decode(&bytes) else { return Err(String::from("an answer that is none")) };
                let summing = self.copy.as_ref().is_some_and(|c| c.summing().is_some() && c.sent(a.id));
                if self.focus == Focus::Close && !self.injected && summing && matches!(a.reply, Reply::Sums { .. }) {
                    self.injected = true;
                    self.joiner_disk.borrow_mut().arm(2);
                }
                let mut out = Vec::new();
                if let Some(c) = self.copy.as_mut() {
                    nbscs::now(c.answer(&self.joiner, a.id, &a.reply, self.now, &mut out));
                }
                self.sent(out);
            }
        }
        Ok(())
    }

    fn sent(&mut self, out: Vec<copy::Out>) {
        for o in out {
            match o {
                copy::Out::Send { bytes, .. } => self.net.push(Msg::Request(bytes)),
                copy::Out::Say(line) => {
                    if self.trace {
                        eprintln!("{}", line);
                    }
                }
            }
        }
    }

    fn step_copy(&mut self) {
        let mut out = Vec::new();
        if let Some(c) = self.copy.as_mut() {
            nbscs::now(c.step(&self.joiner, self.now, &mut self.next_id, &mut out));
        }
        self.sent(out);
    }

    /// The copy begun, its writes marked from now on.
    fn begin(&mut self) {
        self.closed_now.clear();
        /* Fenced at the writer's generation first, as nbs-cs fences: it is
           already, so nothing to do */
        if !nbscs::now(copy::fence_for(&self.joiner, &self.view())) {
            panic!("the joiner not fenced");
        }
        self.joiner.join(SET, self.since);
        self.copy = Some(Copy::new(self.view(), JOINER, self.now));
    }

    /// A sector of the chunk being copied rotted on the joiner, where no
    /// client has written: the copy's check is to find it.
    fn rot(&mut self) {
        let Some(chunk) = self.copy.as_ref().and_then(|c| c.summing()) else { return };
        if self.rotted.contains(&chunk) || self.rotted.len() >= 3 {
            return;
        }
        let Some(h) = self.joiner.holding(chunk) else { return };
        let k = self.r.below(SLOT / SECTOR);
        let c = cref(chunk, self.version);
        if self.joiner.marked(&c, (k * SECTOR) as u32, SECTOR as u32) {
            return;
        }
        let Some(layout) = nbscs::layout(DISK_SLOTS * SLOT) else { return };
        let at = layout.data + u64::from(h.slot) * SLOT + k * SECTOR;
        let bit = self.r.below(SECTOR * 8);
        self.joiner_disk.borrow_mut().flip(at / SECTOR, bit);
        if self.trace {
            eprintln!("chunk {}'s sector {} rotted on the joiner", chunk, k * SECTOR / 512);
        }
        self.rotted.push(chunk);
        self.stats[3] += 1;
    }

    /// After a power cut of the joiner's: every chunk the copy closed, and
    /// no client opened since, the source's -- a close durable only after
    /// its data.
    fn check_closed(&mut self, id: Identity) -> Result<(), String> {
        let _big = crate::heap::Exempt::new();
        let disk = self.joiner_disk.clone();
        drop(std::mem::replace(&mut self.joiner, Store::new()));
        disk.borrow_mut().cut(&mut self.r);
        let s = Store::new();
        nbscs::now(s.open_device(Handle(disk), id, 0)).map_err(|(_, e)| format!("the joiner not opened after a cut: {:?}", e))?;
        self.joiner = s;
        if let Some(d) = self.joiner.devices().iter().find(|d| d.lost != 0) {
            return Err(format!("{} records lost on the joiner's device after a cut", d.lost));
        }
        let mut a = vec![0u8; SLOT as usize];
        let mut b = vec![0u8; SLOT as usize];
        for (chunk, ..) in self.joiner.holdings_in(SET, 0, nbswire::LIST_MAX) {
            if self.joiner.holding(chunk).is_none_or(|h| h.state != nbscs::SlotState::Written) {
                continue;
            }
            let c = cref(chunk, self.version);
            nbscs::now(self.source.read(VOLUME, GEN, c, 0, &mut a, true)).map_err(|e| format!("source's chunk {}: {:?}", chunk, e))?;
            nbscs::now(self.joiner.own_read(c, 0, &mut b)).map_err(|e| format!("joiner's chunk {}: {:?}", chunk, e))?;
            if a != b {
                return Err(format!("chunk {}, closed by the copy, is not the source's after a power cut", chunk));
            }
        }
        Ok(())
    }

    /// Every chunk the copy closed -- closed at no writer's generation, no
    /// client's write to it since, none on its way -- the source's after a
    /// power cut: a close durable only after its data.
    fn closed_kept(&mut self) -> Result<(), String> {
        let _big = crate::heap::Exempt::new();
        let mut a = vec![0u8; SLOT as usize];
        let mut b = vec![0u8; SLOT as usize];
        /* A cut loses no record: one of its two copies is whole whatever a
           cut does to the other (a check of what is held, with nothing held,
           passes) */
        if let Some(d) = self.joiner.devices().iter().find(|d| d.lost != 0) {
            return Err(format!("{} records lost on the joiner's device after a cut", d.lost));
        }
        for (chunk, ..) in self.joiner.holdings_in(SET, 0, nbswire::LIST_MAX) {
            let Some(h) = self.joiner.holding(chunk) else { continue };
            if h.state != nbscs::SlotState::Written || h.generation != 0 || !self.closed_now.contains(&chunk) {
                continue;
            }
            let c = cref(chunk, self.version);
            nbscs::now(self.source.read(VOLUME, GEN, c, 0, &mut a, true)).map_err(|e| format!("source's chunk {}: {:?}", chunk, e))?;
            nbscs::now(self.joiner.own_read(c, 0, &mut b)).map_err(|e| format!("joiner's chunk {}: {:?}", chunk, e))?;
            /* But the sectors of a write still on its way to one of them */
            let flying = |k: usize| self.writes.iter().map(|w| (w.chunk, w.offset, w.data.len()))
                .chain(self.missed.iter().copied())
                .any(|(c, at, len)| c == chunk && at as usize <= k * 512 && k * 512 < at as usize + len);
            if let Some(k) = (0..(SLOT / 512) as usize).find(|&k| !flying(k) && a[k * 512..(k + 1) * 512] != b[k * 512..(k + 1) * 512]) {
                return Err(format!("chunk {}, closed by the copy, is not the source's after a power cut: sector {}",
                                   chunk, k));
            }
        }
        Ok(())
    }

    /// The joiner's power failed: its store opened again from what its disk
    /// kept, its join given up -- its marks went with it -- and made again
    /// under a new version a while after.
    fn crash(&mut self, id: Identity, step: u64) -> Result<(), String> {
        let disk = self.joiner_disk.clone();
        drop(std::mem::replace(&mut self.joiner, Store::new()));
        disk.borrow_mut().cut(&mut self.r);
        let s = Store::new();
        nbscs::now(s.open_device(Handle(disk), id, 0)).map_err(|(_, e)| format!("the joiner not opened after a cut: {:?}", e))?;
        self.joiner = s;
        /* The chunk the copy was at among them: a close the cut cut short
           that left its record closed is to have left its data too */
        if let Some(chunk) = self.copy.as_ref().and_then(|c| c.working()) {
            self.closed_now.push(chunk);
        }
        self.closed_kept()?;
        self.jv = Requests::new();
        self.copy = None;
        self.closed_now.clear();
        self.missed.clear();
        self.refresh_at = None;
        self.version += 1;
        self.since = self.version;
        self.joining = true;
        self.copy_at = Some(step + self.r.range(1, 150));
        self.stats[6] += 1;
        if self.trace {
            eprintln!("step {}: the joiner's power failed; the join made again at {}", step, self.since);
        }
        Ok(())
    }

    /// Every chunk of the set the source holds, the joiner holds as the
    /// source does.
    fn check(&mut self) -> Result<(), String> {
        /* Whole chunks read by the fuzzer: its own, not the engine's */
        let _big = crate::heap::Exempt::new();
        let theirs = self.source.holdings_in(SET, 0, nbswire::LIST_MAX);
        let mine = self.joiner.holdings_in(SET, 0, nbswire::LIST_MAX);
        let held = |l: &[(u32, u32, u32, u32)]| l.iter().map(|e| e.0).collect::<Vec<u32>>();
        if held(&theirs) != held(&mine) {
            return Err(format!("the source holds chunks {:?}, the joiner {:?}", held(&theirs), held(&mine)));
        }
        let mut a = vec![0u8; SLOT as usize];
        let mut b = vec![0u8; SLOT as usize];
        for &(chunk, ..) in &theirs {
            let c = cref(chunk, self.version);
            nbscs::now(self.source.read(VOLUME, GEN, c, 0, &mut a, true)).map_err(|e| format!("source's chunk {}: {:?}", chunk, e))?;
            nbscs::now(self.joiner.read(VOLUME, GEN, c, 0, &mut b, true)).map_err(|e| format!("joiner's chunk {}: {:?}", chunk, e))?;
            if let Some(k) = (0..(SLOT / 512) as usize).find(|&k| a[k * 512..(k + 1) * 512] != b[k * 512..(k + 1) * 512]) {
                return Err(format!("chunk {}'s sector {} (of 512 bytes) differs on the joiner from the source's",
                                   chunk, k));
            }
        }
        Ok(())
    }
}

fn store(r: &mut Rng, cs: u32, device: u32) -> Result<(Store<Handle>, Rc<RefCell<Disk>>, Identity), String> {
    let disk = Rc::new(RefCell::new(Disk::new(SECTOR, DISK_SLOTS * SLOT)));
    let identity = Identity { cluster: CLUSTER, cs, device };
    let s = Store::new();
    nbscs::now(s.format_device(Handle(disk.clone()), identity, 1 + r.below(u64::MAX - 1)))
        .map_err(|(_, e)| format!("cs {}'s disk not formatted: {:?}", cs, e))?;
    nbscs::now(s.fence(VOLUME, GEN)).map_err(|e| format!("cs {} not fenced: {:?}", cs, e))?;
    Ok((s, disk, identity))
}

pub fn run(seed: u64, steps: u64, trace: bool) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x6a6f_696e));
    let (source, _sdisk, _) = store(&mut r, SOURCE, 1)?;
    let (mut joiner, jdisk, jid) = store(&mut r, JOINER, 2)?;
    /* A copy into a set not joined: refused, as its writes are not marked */
    if nbscs::now(joiner.copy_in(VOLUME, ChunkRef { chunk: 1, incarnation: 1, epoch: Epoch { set: SET + 1, version: 1 } }, 0,
                      &[0u8; SECTOR as usize])) != Err(nbscs::Error::Bad) {
        return Err(String::from("a copy into a set not joined taken"));
    }
    let focus = match r.below(10) {
        0 => Focus::Close,
        1 => Focus::Moved,
        2 => Focus::Held,
        3 => Focus::Marks,
        4 => Focus::Suspect,
        _ => Focus::None,
    };
    let own = matches!(focus, Focus::Held | Focus::Suspect)
        || (!matches!(focus, Focus::Marks | Focus::Close) && r.chance(33));
    let mut chunks: Vec<u32> = Vec::new();
    while chunks.len() < 1 + r.below(3) as usize {
        let c = r.below(40) as u32;
        if !chunks.contains(&c) {
            chunks.push(c);
        }
    }
    /* The set's history before the join: at the versions the member was
       active at, on both where it is a member back; after, on the source
       alone -- few, so that a member back has chunks it holds as they were */
    let mut seq = 0u64;
    let mut cut = false;
    /* Rejoins and power cuts at random in runs of no focus: in a focused
       run, a copy begun again lists the chunks anew, and the race is gone */
    let rejoins = focus == Focus::None && r.chance(50);
    for version in 1..SINCE {
        let writes = match focus {
            Focus::Held | Focus::Suspect if version >= UNTIL => 0,
            /* Every chunk written at the member's last version, six times:
               found open, and what the cut loses of it to be seen */
            Focus::Suspect if version == UNTIL - 1 => 6 * chunks.len() as u64,
            _ if own && version >= UNTIL => r.range(0, 1),
            _ => r.range(1, 6),
        };
        for k in 0..writes {
            let chunk = if focus == Focus::Suspect && version == UNTIL - 1 { chunks[k as usize % chunks.len()] }
                        else { chunks[r.below(chunks.len() as u64) as usize] };
            let first = r.below(SLOT / SECTOR - 8);
            let n = 1 + r.below(8);
            seq += 1;
            let d = data(seq ^ 0x5eed_0000, (n * SECTOR) as usize);
            let c = cref(chunk, version);
            nbscs::now(source.write(VOLUME, GEN, c, (first * SECTOR) as u32, &d, true))
                .map_err(|e| format!("the source's history not written: {:?}", e))?;
            if own && version < UNTIL {
                nbscs::now(joiner.write(VOLUME, GEN, c, (first * SECTOR) as u32, &d, true))
                    .map_err(|e| format!("the joiner's history not written: {:?}", e))?;
            }
        }
        /* The member's writes of its last version flushed, or lost to a
           cut: those chunks found open as it comes back */
        if own && version == UNTIL - 2 {
            nbscs::now(joiner.flush()).map_err(|e| format!("joiner not flushed: {:?}", e))?;
        }
        if own && version == UNTIL - 1 && focus != Focus::Held && (focus == Focus::Suspect || r.chance(50)) {
            cut = true;
        }
    }
    /* Data in every pull's range of every chunk, where a lost write of the
       copy's is to show: a chunk mostly zeros loses nothing to see */
    if focus == Focus::Close {
        for &chunk in &chunks {
            for k in 0..SLOT / u64::from(nbswire::DATA_MAX) {
                let at = k * u64::from(nbswire::DATA_MAX) + r.below(u64::from(nbswire::DATA_MAX) / SECTOR) * SECTOR;
                seq += 1;
                let d = data(seq ^ 0x5eed_0000, SECTOR as usize);
                nbscs::now(source.write(VOLUME, GEN, cref(chunk, SINCE - 1), at as u32, &d, true))
                    .map_err(|e| format!("the source's history not written: {:?}", e))?;
            }
        }
    }
    nbscs::now(source.flush()).map_err(|e| format!("source not flushed: {:?}", e))?;
    /* A member back's chunk of the set the source has none of, at a version
       it was active at, kept past any cut: of a randomness of its own, so
       that a run without one goes as it did */
    let mut sr = Rng::new(mix(seed, 0x7374_7261));
    if own && sr.chance(50) {
        let stray = 40 + sr.below(20) as u32;
        let d = data(0x57a7 ^ u64::from(stray), (2 * SECTOR) as usize);
        let at = (sr.below(SLOT / SECTOR - 2) * SECTOR) as u32;
        nbscs::now(joiner.write(VOLUME, GEN, cref(stray, sr.range(1, u64::from(UNTIL) - 1) as u32), at, &d, true))
            .map_err(|e| format!("the member back's own chunk not written: {:?}", e))?;
        nbscs::now(joiner.flush()).map_err(|e| format!("joiner not flushed: {:?}", e))?;
    }
    if cut {
        drop(joiner);
        jdisk.borrow_mut().cut(&mut r);
        let s = Store::new();
        nbscs::now(s.open_device(Handle(jdisk.clone()), jid, 0)).map_err(|(_, e)| format!("the joiner not opened again: {:?}", e))?;
        joiner = s;
    } else {
        nbscs::now(joiner.flush()).map_err(|e| format!("joiner not flushed: {:?}", e))?;
    }
    let mut w = World { r, now: 0, trace, source, joiner, joiner_disk: jdisk, sv: Requests::new(), jv: Requests::new(),
                        version: SINCE, since: SINCE, joining: true, own, chunks, copy: None, copy_at: None,
                        refresh_at: None, next_id: 1, net: Vec::new(), writes: Vec::new(), seq: 1 << 32, rotted: Vec::new(),
                        focus, release: false, last_working: None, closed: 0, closed_now: Vec::new(), missed: Vec::new(), injected: false,
                        zr: Rng::new(mix(seed, 0x7a6f_6d62)), zseq: 0, stats: [0; 8] };
    w.begin();
    let fuse_at = if focus == Focus::None && w.r.chance(33) { Some(w.r.range(20, 300)) } else { None };
    /* Or, where it is not, armed a few writes ahead as a chunk's sums are
       asked: a cut inside its close -- which is durable only after its data */
    let mut close_fuse = fuse_at.is_none() && focus == Focus::None && w.r.chance(30);
    /* The random writes, but where they would move on the epoch the focus's
       race is at */
    let writes_on = !matches!(focus, Focus::Moved | Focus::Held | Focus::Suspect | Focus::Close);
    /* A member back with chunks found open: its join given up as its first
       pull lands, half the time -- the next copy is to hold them all to the
       source's the same, what the cut lost among what differs */
    let mut early = own && cut && (focus == Focus::Suspect || w.r.chance(50));
    /* A member back given up once it has held a chunk by sums, or any
       joiner once it has pulled: the focus's race is after it */
    let mut give_up = matches!(focus, Focus::Held | Focus::Marks);
    let faults = steps.min(FAULT_STEPS);
    let mut step = 0u64;
    loop {
        step += 1;
        if step > faults + END_STEPS {
            let state = w.copy.as_ref().map(|c| format!("{:?}", c.state()));
            return Err(format!("the copy never ended: {:?}, {} messages on their way", state, w.net.len()));
        }
        let quiet = step > faults;
        w.now += if w.r.chance(2) { 2_500 * MS } else { w.r.below(40) * MS };
        if fuse_at == Some(step) {
            let n = w.r.range(1, 60);
            w.joiner_disk.borrow_mut().arm(n);
        }
        if close_fuse && w.copy.as_ref().is_some_and(|c| c.summing().is_some()) && w.r.chance(20) {
            close_fuse = false;
            let n = w.r.range(1, 6);
            w.joiner_disk.borrow_mut().arm(n);
        }
        if w.joiner_disk.borrow().dead() {
            w.crash(jid, step)?;
        }
        if w.copy_at.is_some_and(|at| step >= at) {
            w.copy_at = None;
            w.begin();
        }
        /* A copy gone stale has its view refreshed a while after, as
           nbs-cs's task asks its mds for its joins every five seconds */
        if w.refresh_at.is_none() && w.copy.as_ref().is_some_and(|c| *c.state() == copy::State::Stale) {
            w.refresh_at = Some(step + w.r.range(5, 100));
        }
        if w.refresh_at.is_some_and(|at| step >= at) {
            w.refresh_at = None;
            let v = w.view();
            if let Some(c) = w.copy.as_mut() {
                c.refresh(v);
            }
        }
        let ready = match focus {
            Focus::Held => w.copy.as_ref().is_some_and(|c| c.stats.kept > 0),
            _ => w.copy.as_ref().is_some_and(|c| c.stats.bytes > 0),
        };
        if give_up && ready {
            give_up = false;
            w.joining = false;
            w.copy = None;
            w.closed_now.clear();
            w.refresh_at = None;
            w.version += 1;
            w.since = w.version;
            w.joining = true;
            w.copy_at = Some(step + w.r.range(1, 150));
            w.stats[5] += 1;
        }
        /* A write on the joiner before the next copy begins, held from the
           source past it */
        if focus == Focus::Marks && !w.injected && !give_up && w.copy.is_none() && w.copy_at.is_some() {
            let before = w.writes.len();
            w.new_write(false);
            if w.writes.len() > before {
                w.injected = true;
                let at = w.writes.len() - 1;
                w.writes[at].hold = true;
                w.deliver_write_at(at)?;
            }
        }
        /* Writers of maps before, at the epochs the sums of the join before
           left, once the next copy has listed */
        if focus == Focus::Held && !w.injected && !give_up && w.copy.as_ref().is_some_and(|c| c.listed()) {
            w.injected = true;
            for chunk in w.chunks.clone() {
                let known = w.source.holding(chunk).map(|h| h.known.version);
                if let Some(v) = known.filter(|&v| v >= UNTIL && v < w.since) {
                    w.old_write_now(chunk, v)?;
                }
            }
        }
        /* A writer of a map before, at the epoch the source knows a chunk at,
           as the copy closes it */
        let closed = w.copy.as_ref().map_or(0, |c| c.stats.chunks);
        if closed > w.closed {
            if let Some(chunk) = w.last_working {
                w.closed_now.push(chunk);
            }
        }
        if focus == Focus::Moved && closed > w.closed {
            if let Some(chunk) = w.last_working {
                let known = w.source.holding(chunk).map(|h| h.known.version);
                if let Some(v) = known.filter(|&v| v < w.since) {
                    w.old_write_now(chunk, v)?;
                }
            }
        }
        w.closed = closed;
        w.last_working = w.copy.as_ref().and_then(|c| c.working());
        if early && w.copy.as_ref().is_some_and(|c| c.stats.bytes > 0) {
            early = false;
            w.joining = false;
            w.copy = None;
            w.closed_now.clear();
            w.refresh_at = None;
            w.version += 1;
            w.since = w.version;
            w.joining = true;
            w.copy_at = Some(step + w.r.range(1, 150));
            w.stats[5] += 1;
        }
        /* A writer taken over, of a map from when the member back was still
           active, now and then: on draws of its own */
        if own && !quiet && w.joiner.joins(SET).is_some() && !w.joiner_disk.borrow().dead() && w.zr.chance(2) {
            w.zombie_write()?;
        }
        match w.r.below(100) {
            0..=44 => w.deliver()?,
            45..=64 => w.step_copy(),
            65..=79 => {
                if !w.writes.is_empty() && (quiet || w.writes.len() >= 3 || w.r.chance(55)) {
                    w.deliver_write()?;
                } else if !quiet && writes_on {
                    let old = w.r.chance(30);
                    w.new_write(old);
                }
            }
            80..=81 if !quiet && !matches!(focus, Focus::Marks | Focus::Close) => {
                /* The set moved on: another member down */
                w.version += 1;
                w.refresh_at = Some(step + w.r.range(1, 60));
                w.stats[4] += 1;
                if w.trace {
                    eprintln!("step {}: the set moved on to {}", step, w.version);
                }
            }
            82 if rejoins && !quiet && w.joining && w.copy.is_some() && w.stats[5] < 2 => {
                /* Given up, and made again a while after under a new
                   version: the writes between to the source alone */
                w.joining = false;
                w.copy = None;
                w.closed_now.clear();
            w.closed_now.clear();
                w.refresh_at = None;
                w.stats[5] += 1;
                for _ in 0..w.r.range(0, 6) {
                    w.new_write(false);
                    w.drain()?;
                }
                w.version += 1;
                w.since = w.version;
                w.joining = true;
                w.copy_at = Some(step + w.r.range(1, 150));
                if w.trace {
                    eprintln!("step {}: the join given up and made again at {}", step, w.since);
                }
            }
            83..=84 if !quiet && focus != Focus::Close => w.rot(),
            _ => w.step_copy(),
        }
        /* Its end: the copy done, and every write delivered */
        let done = w.copy.as_ref().is_some_and(|c| *c.state() == copy::State::Done);
        if let Some(copy::State::Failed(why)) = w.copy.as_ref().map(|c| c.state().clone()) {
            /* Given up for the joiner's power failing under it: made again */
            if w.joiner_disk.borrow().dead() {
                w.crash(jid, step)?;
                continue;
            }
            return Err(format!("step {}: the copy gave up: {}", step, why));
        }
        if done && (quiet || w.r.chance(5)) {
            w.release = true;
            while !w.writes.is_empty() {
                w.deliver_write()?;
            }
            /* Its power failed as the last writes came: it joins again */
            if w.joiner_disk.borrow().dead() {
                w.crash(jid, step)?;
                continue;
            }
            w.check().map_err(|e| format!("step {}: {}", step, e))?;
            if w.r.chance(30) {
                w.check_closed(jid).map_err(|e| format!("step {}: {}", step, e))?;
            }
            w.zombie_after().map_err(|e| format!("step {}: {}", step, e))?;
            if std::env::var_os("NBS_JOIN_STATS").is_some() {
                let s = w.copy.as_ref().map(|c| c.stats);
                eprintln!("join seed {}: {:?}, own {}, cut {}, {} steps, writes {} old {} refused {}, rots {}, moves {}, \
                           rejoins {}, crashes {}, a writer taken over's refused {}, {:?}", seed, focus, own, cut, step,
                          w.stats[0], w.stats[1], w.stats[2], w.stats[3], w.stats[4], w.stats[5], w.stats[6],
                          w.stats[7], s);
            }
            return Ok(());
        }
    }
}
