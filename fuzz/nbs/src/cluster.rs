//! The `cluster` target: NBS end to end (plans/nbs.md, N8). The client
//! (src/crates/common/storage/nbsclient) writes and reads a volume through
//! cs's -- nbscs's engine, done by nbswire's `serve` as nbs-cs.ko does it --
//! asking its host's mds, whose answers are nbswire's `mds` over nbsmeta's
//! state, as nbs-mds.ko's are; the leader's look at a cs before taking it
//! out is the fuzzer's: the cs's host up. Over a network of the fuzzer's:
//! what goes between two ends in order, the pairs in any order, and hosts
//! that crash -- their disks' write caches kept, lost or torn -- and come
//! back; a writer that crashes, or is taken over by force from another host
//! while its messages are still about.
//!
//! Held to:
//! - **a write done** only once every replica the metadata has in holds it:
//!   read back from each then and there -- but one that restarted with the
//!   chunk open at the writer's generation, which refuses it from then on;
//! - **a read** every block of it the last write done there when it was
//!   asked, or one after that -- out, failed, or done since;
//! - **a flush**: a replica whose host crashed after it reads back what was
//!   flushed, or written since;
//! - **a writer taken over** fenced: once a newer writer serves, no active
//!   replica does a write or a flush of an older generation's -- held where
//!   the cs serves it. One out of its set does: nobody fences what nobody
//!   writes, and the old writer's map has it in still -- an acknowledgement
//!   late of what its active replicas had before the fence;
//! - **the replicas the same**: once a writer serves, and at the end, every
//!   replica the metadata has in reads every block written alike;
//! - **a writer fails** only as its chunk's last active replica, restarted,
//!   refuses it (`nbsclient::LOST_LAST`), or once taken over; and an
//!   operation of its -- a read, a write, a flush, a close -- only at its
//!   deadline, or with its session;
//! - **every set back at three** active members by the end: the leader's
//!   controller (N9c) runs here as in nbs-mds -- a cs down, up and out as
//!   its host is, its places joined by its own cs back or by another -- and
//!   each joiner's cs runs nbswire's copy from a source, as nbs-cs does,
//!   over the same network, under the writer's writes; and a host drained
//!   now and then, its places taken by others while it serves (N9d);
//! - **a writer goes on and stops** with a host lost for good, as long as
//!   every chunk has a replica in on another: none of what it asks -- a
//!   write, a flush, a close -- fails at its deadline, and none waits on a
//!   cs no set of the volume writes any more (nbs-test's repair phase, where
//!   a close waited for good on a cs no set had in, shaped at the end of
//!   some runs: every chunk written, a host that holds some lost, every
//!   chunk written past it, then a flush and a close) -- and no flush goes
//!   to that cs at all once no set has it in;
//! - **a writer stops** whose closes go, by a view from before, to a cs
//!   that is in their chunks' sets no more -- drained, its cs down in every
//!   set while the writer stalled, back with the chunks found open -- and
//!   asks a cs out of a set only where its view writes it there.
//!
//! A writer that follows another holds what that one had done and not
//! flushed as a crash would leave it: there or not -- a replica that lost it
//! may be what its reconcile copied.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

use nbsclient::{Buffers, Out, Phase, Session, To, UserOp, BLOCK, DEADLINE_NS, LOST_LAST};
use nbscs::{ChunkRef, Epoch, Identity, Store, SLOT};
use nbsmeta::repair::{self, Seen};
use nbsmeta::{Answer, Command, Replica, State};
use nbswire::copy::{self, Copy};
use nbswire::serve::{serve, Vouched};
use nbswire::{Answer as CsAnswer, Ask, Asking, Question, Reply, Request, Said, Saying, Told};

use crate::cs::{Disk, Handle};
use crate::rng::{mix, Rng};

/// Twice a set's replicas: the cs's a writer's start reaches by its
/// reconcile alone are not all there are, so that the fences it sends first
/// are needed.
const HOSTS: [&str; 6] = ["a", "b", "c", "d", "e", "f"];
const CLUSTER: [u8; 16] = [5; 16];
const VOLUME: &str = "v";
/// The volume: six chunks.
const VOLUME_MIB: u64 = 24;
/// A host's disk: room for the volume's chunks and some.
const DISK_SLOTS: u64 = 12;
const MS: u64 = 1_000_000;
/// How long a run lasts at most, in the network's time: well under an
/// operation's deadline.
const RUN_NS: u64 = 40_000 * MS;
/// Writers started at the end, one after another, for one to detach.
const END_WRITERS: usize = 3;
/// How long a writer let rest is left alone before it is taken over: past
/// the session's idle close, in steps' time.
const REST_STEPS: u64 = 400;
/// Messages delivered in a step, at most -- the network keeping up with
/// what the writer sends, mostly.
const DELIVERIES: u64 = 8;
/// The controller's thresholds here (N9d): a host is down 0.4 to 4.4 s of
/// the network's time, so a cs silent two seconds is down and four out --
/// its places taken by others -- and some two in five come back members
/// still, a chunk found open on them for their writer to meet.
const AFTER: repair::After = repair::After { down_ms: 2_000, out_ms: 4_000 };
/// How often the fuzzer's leader plans, and each cs looks at its joins.
const PLAN_EVERY: u64 = 500 * MS;
const JOINS_EVERY: u64 = 300 * MS;
/// How long the end waits for every set to be back at three, in the
/// network's time: past any copy's.
const SETTLE_NS: u64 = 600_000 * MS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum End {
    Client(usize),
    Cs(usize),
    Mds(usize),
    /// A host's cs as the joiner of its sets: its copies' requests to their
    /// sources, and the answers.
    Copy(usize),
}

struct Msg {
    bytes: Vec<u8>,
    id: u64,
}

struct Host {
    name: &'static str,
    cs: u32,
    disk: Rc<RefCell<Disk>>,
    identity: Identity,
    store: Option<Store<Handle>>,
    vouched: Vouched,
    /// When it went down, if it is: what the leader counts its silence from.
    down_since: Option<u64>,
    /// Its cs's copies -- one a join it is the joiner of -- as nbs-cs's;
    /// whether it has looked at its joins since it started -- at the first
    /// look it gives up every join it finds, as a joiner never resumes one
    /// -- and when it looks next; its copies' request numbers.
    copies: Vec<Copy>,
    joins_seen: bool,
    joins_at: u64,
    copy_id: u64,
}

/// The driver's buffers, by tag.
#[derive(Default)]
struct Bufs(BTreeMap<u64, Vec<u8>>);

impl Buffers for Bufs {
    fn data(&self, tag: u64) -> &[u8] {
        self.0.get(&tag).map_or(&[], |v| v.as_slice())
    }

    fn fill(&mut self, tag: u64, at: usize, bytes: &[u8]) {
        if let Some(d) = self.0.get_mut(&tag).and_then(|b| b.get_mut(at..at + bytes.len())) {
            d.copy_from_slice(bytes);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum K {
    Read,
    Write(u8),
    Flush,
    /// A flush, then every chunk written closed.
    Close,
}

struct MOp {
    kind: K,
    offset: u64,
    len: u32,
    /// The write sequence it was submitted at: a write's own, a read's or a
    /// flush's the next.
    seq: u64,
    /// A read's: each block's last write done when it was submitted -- its
    /// sequence and value.
    before: Vec<(u64, u8)>,
    /// When it was submitted, in the network's time.
    at: u64,
}

struct Client {
    host: usize,
    s: Session,
    bufs: Bufs,
    ops: BTreeMap<u64, MOp>,
    next_tag: u64,
    /// Taken over: driven no more, its messages still about.
    zombie: bool,
    /// Lost with its host: it does nothing more, and what comes to it is
    /// lost; what it sent to a cs may still get there.
    dead: bool,
    /// When it came to serve.
    served: bool,
    /// Its writes done since its last flush done: their sequences.
    unflushed: Vec<u64>,
    /// Taken over idle, and asked nothing until the new writer serves --
    /// then a flush: an idle guest whose host was cut off, syncing later.
    quiet: bool,
    /// Its phase as the trace said it last.
    shown: Phase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WState {
    Out,
    Done,
    Failed,
    /// Done by a writer since followed, and not flushed: there or not.
    Lost,
}

/// What the fuzzer knows of each block written: every write to it, the last
/// done, and what the last flush done made durable.
#[derive(Default)]
struct Blocks {
    /// Every write to a block: its sequence, value and state.
    history: BTreeMap<u64, Vec<(u64, u8, WState)>>,
    /// What the last flush done made durable -- the last write done before
    /// it, or one after that which failed, was lost or was still out -- and
    /// the sequence from which writes since may be there too.
    flushed: BTreeMap<u64, (BTreeSet<u8>, u64)>,
}

impl Blocks {
    /// The last write done to block `b`: its sequence and value.
    fn acked(&self, b: u64) -> (u64, u8) {
        self.history.get(&b).and_then(|h| h.iter().rev().find(|w| w.2 == WState::Done)).map_or((0, 0), |w| (w.0, w.1))
    }

    /// Writes failed or lost since the last done: they may be there.
    fn failed_since_done(&self, b: u64) -> BTreeSet<u8> {
        let Some(h) = self.history.get(&b) else { return BTreeSet::new() };
        let last = h.iter().rposition(|w| w.2 == WState::Done).map_or(0, |i| i + 1);
        h[last..].iter().filter(|w| matches!(w.2, WState::Failed | WState::Lost)).map(|w| w.1).collect()
    }

    /// What a read may find in block `b`, `before` the last write done when
    /// it was submitted: that, or any write after it -- out then, or since.
    /// The writes to a block go one at a time, in order.
    fn readable(&self, b: u64, before: (u64, u8)) -> BTreeSet<u8> {
        let mut s = BTreeSet::from([before.1]);
        if let Some(h) = self.history.get(&b) {
            for w in h.iter().filter(|w| w.0 > before.0) {
                s.insert(w.1);
            }
        }
        s
    }

    /// A writer followed by another: what was done and not flushed, there or
    /// not.
    fn lose_unflushed(&mut self) {
        for (b, h) in self.history.iter_mut() {
            let from = self.flushed.get(b).map_or(0, |f| f.1);
            for w in h.iter_mut().filter(|w| w.2 == WState::Done && w.0 >= from) {
                w.2 = WState::Lost;
            }
        }
    }

    /// What a replica that crashed may hold: what was flushed, or anything
    /// written since or still out, or any failed since the last done.
    fn durable(&self, b: u64) -> BTreeSet<u8> {
        let (mut s, from) = self.flushed.get(&b).cloned().unwrap_or((BTreeSet::from([0]), 0));
        s.extend(self.failed_since_done(b));
        if let Some(h) = self.history.get(&b) {
            for w in h {
                if w.0 >= from || w.2 == WState::Out {
                    s.insert(w.1);
                }
            }
        }
        s
    }
}

struct World {
    r: Rng,
    now: u64,
    step: u64,
    trace: bool,
    state: State,
    index: u64,
    hosts: Vec<Host>,
    links: BTreeMap<(End, End), VecDeque<Msg>>,
    clients: Vec<Client>,
    blocks: Blocks,
    seq: u64,
    /// The host crashed now, if one is: at most one at a time.
    down: Option<usize>,
    /// The newest generation a writer has come to serve at.
    served: u64,
    /// A writer to be taken over at rest: the step, and the writer -- asked
    /// to flush, then nothing, so that its chunks are closed by then.
    rest: Option<(u64, usize)>,
    /// A writer, and the host lost that its writes took out of every set
    /// while it was still owed a flush: no flush of that writer's goes there.
    owed: Option<(usize, usize)>,
    /// When the leader's controller plans next; the host drained, if one
    /// is -- one a run at most, a drain having no end.
    plan_at: u64,
    drained: Option<usize>,
    stats: BTreeMap<&'static str, u64>,
}

impl World {
    fn count(&mut self, what: &'static str) {
        *self.stats.entry(what).or_default() += 1;
    }

    fn apply(&mut self, cmd: Command) -> Answer {
        self.index += 1;
        self.state.apply_command(self.index, &cmd)
    }

    fn host_of_cs(&self, cs: u32) -> Option<usize> {
        self.hosts.iter().position(|h| h.cs == cs)
    }

    fn push(&mut self, from: End, to: End, bytes: Vec<u8>, id: u64) {
        self.links.entry((from, to)).or_default().push_back(Msg { bytes, id });
    }

    /// A client's outputs: its messages queued, its operations checked.
    fn outs(&mut self, c: usize, out: Vec<Out>) -> Result<(), String> {
        for o in out {
            match o {
                Out::Send { to: To::Mds, id, bytes } => {
                    /* A cs asked out of a set only where the view writes it
                       there: one the view has out already -- read since, its
                       place another's -- would be asked about for as long as
                       the session lives */
                    if let Some(Ask::Down { set, cs, .. }) = Asking::decode(&bytes).map(|a| a.ask) {
                        if !self.clients[c].s.writes_in(set, cs) {
                            return Err(format!("client {} asked cs {} out of set {}, which its view does not \
                                                write it in -- {}", c, cs, set, self.clients[c].s.view()));
                        }
                    }
                    let h = self.clients[c].host;
                    self.push(End::Client(c), End::Mds(h), bytes, id);
                }
                Out::Send { to: To::Cs(cs), id, bytes } => {
                    let Some(h) = self.host_of_cs(cs) else { return Err(format!("a request to cs {}, none", cs)) };
                    /* Judged as sent: one sent before, while the view had the
                       cs in, may get there after */
                    if self.owed == Some((c, h)) && Request::decode(&bytes).is_some_and(|r| {
                        matches!(r.op, nbswire::Op::Flush)
                    }) {
                        self.count("flushes to a cs out of every set");
                    }
                    if self.trace {
                        let what: String = Request::decode(&bytes).map_or(String::from("?"), |r| match &r.op {
                            nbswire::Op::Write { chunk, offset, data, .. } => {
                                format!("write chunk {} v{} at {} +{}", chunk.chunk, chunk.epoch.version, offset,
                                        data.len())
                            }
                            other => format!("{:?}", other).chars().take(40).collect(),
                        });
                        eprintln!("  [{}] client {} sends #{} to cs {}{}: {}", self.step, c, id, cs,
                                  if self.hosts[h].store.is_none() { " (down)" } else { "" }, what);
                    }
                    self.push(End::Client(c), End::Cs(h), bytes, id);
                }
                Out::Done { tag, ok } => self.done(c, tag, ok)?,
                Out::Say(line) => {
                    if self.trace {
                        eprintln!("  [{}] client {}: {}", self.step, c, line);
                    }
                }
            }
        }
        self.check_failed(c)
    }

    /// The writer failed: only as its chunk's last active replica refuses it
    /// -- driven no more then, another to start -- or once taken over.
    fn check_failed(&mut self, c: usize) -> Result<(), String> {
        let Phase::Failed(why) = self.clients[c].s.phase() else { return Ok(()) };
        if self.clients[c].zombie {
            return Ok(());
        }
        if why != LOST_LAST {
            return Err(format!("client {} failed: {}", c, why));
        }
        self.count("writers failed: the last replica lost");
        self.clients[c].zombie = true;
        Ok(())
    }

    /* ---- the checks ---- */

    /// The chunk block `b` is in, as the metadata has it now, and its
    /// replicas' hosts that are in -- each with whether it may be trusted:
    /// up, and not holding the chunk as it found it open at `generation`.
    fn replicas(&self, b: u64, generation: u64) -> Option<(u32, ChunkRef, u32, Vec<usize>)> {
        let v = self.state.volume(VOLUME)?;
        let at = b * BLOCK;
        let place = (at / SLOT) as usize;
        let inside = (at % SLOT) as u32;
        let c = *v.map.get(place)?;
        let ch = self.state.chunks().get(c as usize)?;
        let set = self.state.sets().get(ch.set as usize)?;
        let chunk = ChunkRef { chunk: c, incarnation: ch.incarnation, epoch: Epoch { set: ch.set, version: set.version } };
        let mut hosts = Vec::new();
        for i in 0..nbsmeta::REPLICAS {
            if set.replicas[i] == Replica::Down {
                continue;
            }
            let Some(h) = self.host_of_cs(set.cs[i]) else { continue };
            let Some(store) = self.hosts[h].store.as_ref() else { continue };
            if generation != 0 && store.found_open(c, ch.incarnation) == Some(generation) {
                continue;
            }
            hosts.push(h);
        }
        Some((v.id, chunk, inside, hosts))
    }

    /// Block `b` as host `h`'s store holds it: its one value.
    fn block_on(&mut self, h: usize, volume: u32, chunk: ChunkRef, inside: u32) -> Result<u8, String> {
        let Some(store) = self.hosts[h].store.as_mut() else { return Err(String::from("its host is down")) };
        let mut buf = vec![0u8; BLOCK as usize];
        store.read(volume, 0, chunk, inside, &mut buf, true).map_err(|e| format!("{:?}", e))?;
        let v = buf[0];
        if buf.iter().any(|&x| x != v) {
            return Err(format!("not one value: {:?}...", &buf[..8]));
        }
        Ok(v)
    }

    fn done(&mut self, c: usize, tag: u64, ok: bool) -> Result<(), String> {
        let Some(op) = self.clients[c].ops.remove(&tag) else { return Ok(()) };
        if self.trace {
            let first = self.clients[c].bufs.0.get(&tag).and_then(|b| b.first().copied());
            eprintln!("  [{}] client {}: {:?} seq {} blocks {}+{} done {} (first byte {:?})", self.step, c, op.kind, op.seq,
                      op.offset / BLOCK, u64::from(op.len) / BLOCK, ok, first);
        }
        let generation = self.clients[c].s.generation();
        let first = op.offset / BLOCK;
        let n = u64::from(op.len) / BLOCK;
        /* An operation fails only at its deadline, or with its session --
           taken over, or its chunk's last replica lost: a refusal sends it
           to another replica, or has the map read again (a read in a view
           with no active replica among them failed at once: found by this
           target, N9d) */
        let lost = self.clients[c].zombie || matches!(self.clients[c].s.phase(), Phase::Failed(_));
        if !ok && !lost && self.now < op.at + DEADLINE_NS {
            return Err(format!("client {}'s {:?} of blocks {}+{} failed {} ms after it was asked, before its deadline",
                               c, op.kind, first, n, (self.now - op.at) / MS));
        }
        match op.kind {
            K::Write(v) => {
                if ok && self.clients[c].zombie && self.clients[c + 1..].iter().any(|x| x.served) {
                    self.count("writes of a writer taken over done late");
                }
                for b in first..first + n {
                    if let Some(w) = self.blocks.history.get_mut(&b).and_then(|h| h.iter_mut().find(|w| w.0 == op.seq)) {
                        w.2 = if ok { WState::Done } else { WState::Failed };
                    }
                }
                self.count(if ok { "writes done" } else { "writes failed" });
                if !ok {
                    return Ok(());
                }
                self.clients[c].unflushed.push(op.seq);
                /* Every replica the metadata has in holds it -- but where a
                   newer writer wrote the block since */
                for b in first..first + n {
                    if self.blocks.history.get(&b).is_some_and(|h| h.iter().any(|w| w.0 > op.seq)) {
                        continue;
                    }
                    let Some((volume, chunk, inside, hosts)) = self.replicas(b, generation) else { continue };
                    for h in hosts {
                        let got = self.block_on(h, volume, chunk, inside);
                        if got != Ok(v) {
                            return Err(format!("client {}'s write of {} to block {} done, and host {} holds {:?}", c, v,
                                               b, self.hosts[h].name, got));
                        }
                    }
                }
            }
            K::Read => {
                self.count(if ok { "reads done" } else { "reads failed" });
                if !ok {
                    return Ok(());
                }
                let data = self.clients[c].bufs.0.remove(&tag).unwrap_or_default();
                for k in 0..n {
                    let b = first + k;
                    let got = &data[(k * BLOCK) as usize..((k + 1) * BLOCK) as usize];
                    let v = got[0];
                    if got.iter().any(|&x| x != v) {
                        return Err(format!("client {}'s read of block {}: not one value", c, b));
                    }
                    let may = self.blocks.readable(b, op.before[k as usize]);
                    if !may.contains(&v) {
                        return Err(format!("client {}'s read of block {} gave {}, which may be {:?}", c, b, v, may));
                    }
                }
            }
            K::Flush | K::Close => {
                self.count(match (op.kind, ok) {
                    (K::Close, true) => "closes done",
                    (K::Close, false) => "closes failed",
                    (_, true) => "flushes done",
                    _ => "flushes failed",
                });
                if !ok {
                    return Ok(());
                }
                /* One that made durable writes of its own -- not one with none
                   to make durable, done fenced or not */
                let covered = self.clients[c].unflushed.iter().filter(|&&s| s < op.seq).count();
                self.clients[c].unflushed.retain(|&s| s >= op.seq);
                if covered > 0 && self.clients[c].zombie && self.clients[c + 1..].iter().any(|x| x.served) {
                    self.count("flushes of a writer taken over done late");
                }
                /* A writer's flush is of its own session's writes: one done
                   once a newer writer serves makes none of that one's durable */
                if self.clients[c + 1..].iter().any(|x| x.served) {
                    return Ok(());
                }
                /* What was done before it is durable -- or what came after
                   that and may be there: failed, lost with a writer followed,
                   a writer's taken over still out */
                let blocks: Vec<u64> = self.blocks.history.keys().copied().collect();
                for b in blocks {
                    let h = &self.blocks.history[&b];
                    let last = h.iter().rposition(|w| w.0 < op.seq && w.2 == WState::Done);
                    let mut may = BTreeSet::from([last.map_or(0, |i| h[i].1)]);
                    for w in h[last.map_or(0, |i| i + 1)..].iter().filter(|w| w.0 < op.seq && w.2 != WState::Done) {
                        may.insert(w.1);
                    }
                    self.blocks.flushed.insert(b, (may, op.seq));
                }
            }
        }
        self.clients[c].bufs.0.remove(&tag);
        Ok(())
    }

    /// Every replica the metadata has in, of every block written, alike.
    fn same(&mut self, generation: u64, when: &str) -> Result<(), String> {
        let blocks: Vec<u64> = self.blocks.history.keys().copied().collect();
        for b in blocks {
            let Some((volume, chunk, inside, hosts)) = self.replicas(b, generation) else { continue };
            let mut seen: Option<(usize, u8)> = None;
            for h in hosts {
                let v = self.block_on(h, volume, chunk, inside)
                    .map_err(|e| format!("{}: block {} on host {}: {}", when, b, self.hosts[h].name, e))?;
                match seen {
                    Some((g, w)) if w != v => {
                        return Err(format!("{}: block {} is {} on host {} and {} on host {}", when, b, w,
                                           self.hosts[g].name, v, self.hosts[h].name));
                    }
                    _ => seen = Some((h, v)),
                }
            }
        }
        Ok(())
    }

    /* ---- the network ---- */

    /// One message delivered, of a pair picked at random.
    fn deliver(&mut self) -> Result<(), String> {
        let keys: Vec<(End, End)> = self.links.iter().filter(|(_, q)| !q.is_empty()).map(|(&k, _)| k).collect();
        let Some(&(from, to)) = self.r.pick(&keys).as_ref() else { return Ok(()) };
        let Some(m) = self.links.get_mut(&(from, to)).and_then(|q| q.pop_front()) else { return Ok(()) };
        match (from, to) {
            (End::Client(c), End::Cs(h)) => self.at_cs(c, h, m),
            (End::Cs(_), End::Client(c)) | (End::Mds(_), End::Client(c)) if self.clients[c].dead => Ok(()),
            (End::Cs(h), End::Client(c)) => {
                if self.trace {
                    let id = nbswire::Answer::decode(&m.bytes).map(|a| a.id);
                    eprintln!("  [{}] client {} hears cs {}'s answer #{:?}", self.step, c, self.hosts[h].cs, id);
                }
                let now = self.now;
                let mut out = Vec::new();
                let cl = &mut self.clients[c];
                cl.s.on_cs(&m.bytes, now, &mut cl.bufs, &mut out);
                self.outs(c, out)
            }
            (End::Client(c), End::Mds(_)) => self.at_mds(c, m),
            (End::Mds(_), End::Client(c)) => {
                let now = self.now;
                let mut out = Vec::new();
                let cl = &mut self.clients[c];
                cl.s.on_mds(&m.bytes, now, &mut cl.bufs, &mut out);
                self.outs(c, out)
            }
            (End::Copy(j), End::Cs(s)) => self.copy_request(j, s, m),
            (End::Cs(_), End::Copy(j)) => self.copy_answer(j, m),
            _ => Ok(()),
        }
    }

    /* ---- joins (N9d) ---- */

    /// A request of host `j`'s copy at host `s`'s cs, its source: done by
    /// `serve` as for a client, the cs asking its own host's mds -- or, its
    /// host down, lost: the copy asks again, or another source.
    fn copy_request(&mut self, j: usize, s: usize, m: Msg) -> Result<(), String> {
        let World { hosts, state, index, .. } = self;
        let host = &mut hosts[s];
        let Some(store) = host.store.as_mut() else { return Ok(()) };
        let name = host.name;
        let applied = *index;
        let answer = match Request::decode(&m.bytes) {
            None => CsAnswer { id: 0, reply: Reply::Refused(nbswire::Refusal::Bad) }.encode(),
            Some(req) => {
                let mut ask = |q| Some(nbswire::mds::told(state, name, applied, &q));
                let result = serve(store, &mut host.vouched, &req, &mut ask);
                if self.trace {
                    let what: String = format!("{:?}", req.op).chars().take(90).collect();
                    let said: String = match &result {
                        Ok(nbswire::serve::Done::Data(d)) => format!("data {} bytes", d.len()),
                        Ok(d) => format!("{:?}", d).chars().take(90).collect(),
                        Err(e) => format!("refused {:?}", e),
                    };
                    eprintln!("  [{}] cs {} (host {}) for host {}'s copy: #{} {} -> {}", self.step, host.cs, name,
                              HOSTS[j], req.id, what, said);
                }
                match result {
                    Ok(done) => CsAnswer { id: req.id, reply: done.reply() }.encode(),
                    Err(why) => CsAnswer { id: req.id, reply: Reply::Refused(why) }.encode(),
                }
            }
        };
        self.push(End::Cs(s), End::Copy(j), answer, m.id);
        Ok(())
    }

    /// An answer to host `j`'s copy: handed to the copy that asked, which
    /// goes on -- or lost, its host down since.
    fn copy_answer(&mut self, j: usize, m: Msg) -> Result<(), String> {
        let Some(a) = CsAnswer::decode(&m.bytes) else { return Ok(()) };
        let now = self.now;
        let mut out = Vec::new();
        {
            let Host { store, copies, copy_id, .. } = &mut self.hosts[j];
            let Some(store) = store.as_mut() else { return Ok(()) };
            if let Some(c) = copies.iter_mut().find(|c| c.sent(a.id)) {
                c.answer(store, a.id, &a.reply, now, &mut out);
                c.step(store, now, copy_id, &mut out);
            }
        }
        self.copy_out(j, out)
    }

    /// What host `j`'s copies ask sent to their sources.
    fn copy_out(&mut self, j: usize, out: Vec<copy::Out>) -> Result<(), String> {
        for o in out {
            match o {
                copy::Out::Send { host, bytes } => {
                    let Some(s) = self.hosts.iter().position(|x| x.name == host) else {
                        return Err(format!("host {}'s copy asked host {}, none", self.hosts[j].name, host));
                    };
                    let id = Request::decode(&bytes).map_or(0, |r| r.id);
                    self.push(End::Copy(j), End::Cs(s), bytes, id);
                }
                copy::Out::Say(line) => {
                    if self.trace {
                        eprintln!("  [{}] host {}'s copy: {}", self.step, self.hosts[j].name, line);
                    }
                }
            }
        }
        Ok(())
    }

    /// Host `h`'s cs's look at the joins it is the joiner of, as nbs-cs's:
    /// at its first since it started, each given up -- a joiner never
    /// resumes one -- then a copy begun for a new one, its writes marked
    /// from now on, refreshed for one it has, dropped for one over, its
    /// marks with it.
    fn joins(&mut self, h: usize) -> Result<(), String> {
        if self.hosts[h].store.is_none() || self.now < self.hosts[h].joins_at {
            return Ok(());
        }
        self.hosts[h].joins_at = self.now + JOINS_EVERY;
        let name = self.hosts[h].name;
        let Told::Joins(views) = nbswire::mds::told(&self.state, name, self.index, &Question::Joins) else {
            return Ok(());
        };
        if !self.hosts[h].joins_seen {
            self.hosts[h].joins_seen = true;
            for v in views {
                self.end_join(h, v.set, v.since, false);
            }
            return Ok(());
        }
        let now = self.now;
        let trace = self.trace;
        let Host { store, copies, cs, .. } = &mut self.hosts[h];
        let Some(store) = store.as_mut() else { return Ok(()) };
        let mut kept = Vec::new();
        for c in std::mem::take(copies) {
            if views.iter().any(|v| v.set == c.set() && v.since == c.since()) {
                kept.push(c);
            } else {
                store.leave(c.set());
            }
        }
        *copies = kept;
        for (set, since) in store.joined() {
            if !views.iter().any(|v| v.set == set && v.since == since) {
                store.leave(set);
            }
        }
        for v in views {
            match copies.iter_mut().find(|c| c.set() == v.set && c.since() == v.since) {
                Some(c) => c.refresh(v),
                None => {
                    if trace {
                        eprintln!("  host {} joins set {} in place {} (since {})", name, v.set, v.place, v.since);
                    }
                    store.join(v.set, v.since);
                    copies.push(Copy::new(v, *cs, now));
                }
            }
        }
        Ok(())
    }

    /// Host `h`'s copies' turn: what each asks sent; each done or given up
    /// ended, as its mds proposes it.
    fn copy_turn(&mut self, h: usize) -> Result<(), String> {
        let now = self.now;
        let mut out = Vec::new();
        {
            let Host { store, copies, copy_id, .. } = &mut self.hosts[h];
            let Some(store) = store.as_mut() else { return Ok(()) };
            for c in copies.iter_mut() {
                c.step(store, now, copy_id, &mut out);
            }
        }
        self.copy_out(h, out)?;
        let ended: Vec<(u32, u32, bool)> = self.hosts[h].copies.iter().filter_map(|c| match c.state() {
            copy::State::Done => Some((c.set(), c.since(), true)),
            copy::State::Failed(_) => Some((c.set(), c.since(), false)),
            _ => None,
        }).collect();
        for (set, since, done) in ended {
            self.end_join(h, set, since, done);
        }
        Ok(())
    }

    /// Host `h`'s join of `set` begun at `since` ended -- `done`, its cs
    /// taking the place, or given up -- as nbs-mds proposes it: `Joined` or
    /// `Unjoin`; the copy and its marks dropped, whatever the answer.
    fn end_join(&mut self, h: usize, set: u32, since: u32, done: bool) {
        let cs = self.hosts[h].cs;
        let cmd = if done { Command::Joined { set, cs, since } } else { Command::Unjoin { set, cs, since } };
        let answer = self.apply(cmd);
        if self.trace {
            eprintln!("  [{}] host {}'s join of set {} (since {}) {}: {:?}", self.step, self.hosts[h].name, set, since,
                      if done { "done" } else { "given up" }, answer);
        }
        self.count(match (done, &answer) {
            (true, Answer::Set { .. }) => "joins done",
            (false, Answer::Set { .. }) => "joins given up",
            _ => "joins over before their end",
        });
        let Host { store, copies, .. } = &mut self.hosts[h];
        copies.retain(|c| !(c.set() == set && c.since() == since));
        if let Some(store) = store.as_mut() {
            store.leave(set);
        }
    }

    /// The leader's controller (N9c), as nbs-mds runs it -- what it sees of
    /// a cs being whether its host is up, and since when it is not -- its
    /// proposals applied. One refused is a finding: the plan holds itself to
    /// the rules the state does (the meta target's progress rule).
    fn control(&mut self) -> Result<(), String> {
        if self.now < self.plan_at {
            return Ok(());
        }
        self.plan_at = self.now + PLAN_EVERY;
        let now = self.now;
        let seen: Vec<Seen> = self.hosts.iter().map(|h| Seen {
            cs: h.cs,
            answering: h.store.is_some(),
            silent_ms: h.down_since.map_or(0, |at| now.saturating_sub(at) / MS),
            maintenance: false,
        }).collect();
        for cmd in repair::plan(&self.state, &seen, AFTER) {
            let what = match cmd {
                Command::CsDown { .. } => "the controller's cs's down",
                Command::CsUp { .. } => "the controller's cs's up",
                Command::CsOut { .. } => "the controller's cs's out",
                Command::Join { .. } => "the controller's joins",
                _ => "the controller's other proposals",
            };
            let answer = self.apply(cmd.clone());
            if self.trace {
                eprintln!("  [{}] the leader proposes {:?}: {:?}", self.step, cmd, answer);
            }
            if let Answer::Refused(why) = answer {
                return Err(format!("the controller proposed {:?}, refused: {:?}", cmd, why));
            }
            self.count(what);
        }
        Ok(())
    }

    /// Whether every set of the volume's chunks has three active members,
    /// none of them on a cs drained, and no joiner.
    fn sets_whole(&self) -> bool {
        let Some(v) = self.state.volume(VOLUME) else { return true };
        let kept = |cs: u32| self.state.cs().get(cs as usize).is_some_and(|c| !c.draining());
        v.map.iter().all(|&c| {
            self.state.chunks().get(c as usize).and_then(|ch| self.state.sets().get(ch.set as usize))
                .is_some_and(|s| {
                    s.joiner.is_none() && s.replicas.iter().all(|&r| r == Replica::Active)
                        && s.cs.iter().all(|&cs| kept(cs))
                })
        })
    }

    /// Host `h`'s device drained: its cs takes no new chunks, and the
    /// controller has its places taken by others.
    fn drain(&mut self, h: usize) -> Result<(), String> {
        let name = self.hosts[h].name;
        let ident = self.state.cs_of(name).and_then(|c| c.devices.first().map(|d| d.ident.clone()))
            .ok_or_else(|| format!("host {} has no device to drain", name))?;
        match self.apply(Command::DeviceDrain { host: String::from(name), ident }) {
            Answer::Device { .. } => {}
            other => return Err(format!("host {}'s device not drained: {:?}", name, other)),
        }
        if self.trace {
            eprintln!("  [{}] host {} drained", self.step, name);
        }
        self.count("hosts drained");
        self.drained = Some(h);
        Ok(())
    }

    /// The sets of the volume, as the state has them, for a finding.
    fn sets_said(&self) -> String {
        let Some(v) = self.state.volume(VOLUME) else { return String::from("no volume") };
        let sets: BTreeSet<u32> = v.map.iter().filter_map(|&c| self.state.chunks().get(c as usize).map(|ch| ch.set))
            .collect();
        sets.iter().filter_map(|&id| self.state.sets().get(id as usize).map(|s| {
            format!("set {} v{}: {:?} {:?} joiner {:?}", id, s.version, s.cs, s.replicas, s.joiner)
        })).collect::<Vec<_>>().join("; ")
    }

    /// The end's last part: the network let run, the controller and the
    /// copies at work, until every set of the volume is at three active
    /// members with no joiner -- as it must be, every host up but `lost`,
    /// and room on the others -- within SETTLE_NS. Not where a set's
    /// members active are all on the host lost: its chunks are nowhere else.
    fn settle(&mut self, lost: Option<usize>) -> Result<(), String> {
        if let Some(h) = lost.filter(|&h| !self.whole_without(h)) {
            let _ = h;
            self.count("ends with a set on the host lost alone");
            return Ok(());
        }
        let until = self.now + SETTLE_NS;
        while !self.sets_whole() {
            if self.now >= until {
                return Err(format!("the sets not at three {} s after the end: {}", SETTLE_NS / 1_000_000_000,
                                   self.sets_said()));
            }
            self.now += 5 * MS;
            if self.links.values().all(|q| q.is_empty()) {
                self.now += 100 * MS;
            }
            self.deliver()?;
            self.tick()?;
        }
        self.count("ends with every set at three");
        Ok(())
    }

    /// A request at host `h`'s cs: done by nbswire's `serve`, the cs asking
    /// the mds as its own host's -- or, its host down, lost or bounced.
    fn at_cs(&mut self, c: usize, h: usize, m: Msg) -> Result<(), String> {
        let World { hosts, state, index, served, .. } = self;
        let host = &mut hosts[h];
        let Some(store) = host.store.as_mut() else {
            if self.r.chance(50) {
                self.count("bounces");
                let now = self.now;
                let mut out = Vec::new();
                let cl = &mut self.clients[c];
                cl.s.on_bounce(m.id, true, now, &mut cl.bufs, &mut out);
                return self.outs(c, out);
            }
            return Ok(());
        };
        let applied = *index;
        let name = host.name;
        let answer = match Request::decode(&m.bytes) {
            None => CsAnswer { id: 0, reply: Reply::Refused(nbswire::Refusal::Bad) }.encode(),
            Some(req) => {
                let mut ask = |q| Some(nbswire::mds::told(state, name, applied, &q));
                let result = serve(store, &mut host.vouched, &req, &mut ask);
                if self.trace {
                    let what = match &req.op {
                        nbswire::Op::Write { chunk, offset, data, .. } => {
                            format!("write chunk {} at {} +{}", chunk.chunk, offset, data.len())
                        }
                        other => format!("{:?}", other).chars().take(60).collect(),
                    };
                    let said = match &result {
                        Ok(nbswire::serve::Done::Data(_)) => String::from("data"),
                        Ok(d) => format!("{:?}", d).chars().take(60).collect(),
                        Err(e) => format!("refused {:?}", e),
                    };
                    eprintln!("  [{}] cs {} (host {}) for client {} at generation {}: #{} {} -> {}", self.step,
                              host.cs, name, c, req.generation, req.id, what, said);
                }
                /* Fenced: once a newer writer serves, no active replica does
                   an older generation's write or flush */
                if result.is_ok() && req.generation != 0 && req.generation < *served {
                    let active = |set: u32| state.sets().get(set as usize).is_some_and(|s| {
                        (0..nbsmeta::REPLICAS).any(|i| s.cs[i] == host.cs && s.replicas[i] != Replica::Down)
                    });
                    let of_volume = |set: u32| state.volume(VOLUME).is_some_and(|v| v.map.iter().any(|&c| {
                        state.chunks().get(c as usize).is_some_and(|ch| ch.set == set)
                    }));
                    let wrong = match &req.op {
                        nbswire::Op::Write { chunk, .. } => active(chunk.epoch.set),
                        nbswire::Op::Flush => (0..state.sets().len() as u32).any(|s| active(s) && of_volume(s)),
                        _ => false,
                    };
                    if wrong {
                        return Err(format!("cs {} (host {}), an active replica, did a {} of generation {} once \
                                            generation {} serves", host.cs, name,
                                           if matches!(req.op, nbswire::Op::Flush) { "flush" } else { "write" },
                                           req.generation, served));
                    }
                }
                match result {
                    Ok(done) => CsAnswer { id: req.id, reply: done.reply() }.encode(),
                    Err(why) => CsAnswer { id: req.id, reply: Reply::Refused(why) }.encode(),
                }
            }
        };
        self.push(End::Cs(h), End::Client(c), answer, m.id);
        Ok(())
    }

    /// A client's ask at its host's mds, answered as nbs-mds's member does --
    /// the leader's look at a cs being whether its host is up.
    fn at_mds(&mut self, c: usize, m: Msg) -> Result<(), String> {
        let Some(a) = Asking::decode(&m.bytes) else { return Ok(()) };
        let host = HOSTS[self.clients[c].host].to_string();
        let said = match a.ask {
            Ask::Attach { name, writable, force, token } => match self.apply(Command::Attach { name, host, writable,
                                                                                              force, token }) {
                Answer::Attached { generation } => Said::Attached { generation, index: self.index },
                Answer::Refused(why) => Said::Refused(why.says().to_string()),
                other => Said::Refused(format!("{:?}", other)),
            },
            Ask::Detach { name, generation } => match self.apply(Command::Detach { name, host, generation }) {
                Answer::Refused(why) => Said::Refused(why.says().to_string()),
                _ => Said::Detached { index: self.index },
            },
            Ask::Map { name, from, count, index } => nbswire::mds::map(&self.state, self.index, &name, from, count,
                                                                        index),
            Ask::Down { set, cs, refused, name, generation } => {
                let up = self.host_of_cs(cs).is_some_and(|h| self.hosts[h].store.is_some());
                if up && !refused && !self.r.chance(10) {
                    self.count("downs refused: the cs answers the leader");
                    Said::NotNow(String::from("the cs answers the leader"))
                } else {
                    let answer = self.apply(Command::ReplicaDown { set, cs, refused, name, generation });
                    if self.trace {
                        eprintln!("  [{}] mds: client {} asks cs {} of set {} out (refused {}): {:?}", self.step, c, cs,
                                  set, refused, answer);
                    }
                    match answer {
                        Answer::Set { version } => {
                            self.count("replicas taken out");
                            Said::Down { version, index: self.index }
                        }
                        Answer::Refused(nbsmeta::Refusal::LastReplica) => Said::LastReplica,
                        Answer::Refused(nbsmeta::Refusal::NotTheWriter) => {
                            self.count("downs refused: not the writer");
                            Said::NotTheWriter
                        }
                        Answer::Refused(why) => Said::Refused(why.says().to_string()),
                        other => Said::Refused(format!("{:?}", other)),
                    }
                }
            }
        };
        let h = self.clients[c].host;
        self.push(End::Mds(h), End::Client(c), Saying { id: a.id, said }.encode(), m.id);
        Ok(())
    }

    /* ---- what happens ---- */

    fn tick(&mut self) -> Result<(), String> {
        self.control()?;
        for h in 0..self.hosts.len() {
            self.joins(h)?;
            self.copy_turn(h)?;
        }
        for c in 0..self.clients.len() {
            if self.clients[c].dead {
                continue;
            }
            let now = self.now;
            let mut out = Vec::new();
            let cl = &mut self.clients[c];
            let was = cl.s.phase().clone();
            cl.s.tick(now, &mut cl.bufs, &mut out);
            self.outs(c, out)?;
            self.served(c, was)?;
            if let Some(v) = std::env::var_os("NBS_DEBUG_REPORT") {
                let v = v.to_string_lossy().to_string();
                let p: Vec<u64> = v.split(':').filter_map(|x| x.parse().ok()).collect();
                if p.len() == 3 && p[0] == c as u64 && self.step >= p[1] && self.step <= p[2] {
                    eprintln!("  [{}] (now {} ms) client {} report: {} -- view {}", self.step, self.now / MS, c,
                              self.clients[c].s.report(), self.clients[c].s.view());
                }
            }
            if self.trace && *self.clients[c].s.phase() != self.clients[c].shown {
                eprintln!("  [{}] client {}: {:?}", self.step, c, self.clients[c].s.phase());
                self.clients[c].shown = self.clients[c].s.phase().clone();
            }
        }
        Ok(())
    }

    /// Client `c` come to serve -- in a delivery or a tick before: every
    /// replica alike.
    fn served(&mut self, c: usize, _was: Phase) -> Result<(), String> {
        if *self.clients[c].s.phase() == Phase::Serving && !self.clients[c].served && !self.clients[c].zombie {
            self.clients[c].served = true;
            self.served = self.served.max(self.clients[c].s.generation());
            self.count("sessions serving");
            self.blocks.lose_unflushed();
            /* A writer taken over and not failed yet asked to flush now: its
               fences must refuse it */
            let old: Vec<usize> = (0..self.clients.len()).filter(|&z| {
                let x = &self.clients[z];
                z != c && x.zombie && !x.dead && *x.s.phase() == Phase::Serving
            }).collect();
            for z in old {
                if self.clients[z].quiet {
                    /* Its guest goes on: a write, then a sync -- fenced, both */
                    self.clients[z].quiet = false;
                    self.count("writes and flushes asked of a writer taken over at rest");
                    self.op(z, true)?;
                    self.flush_of(z)?;
                } else if self.r.chance(50) {
                    self.count("flushes asked of a writer taken over");
                    self.flush_of(z)?;
                }
            }
            let generation = self.clients[c].s.generation();
            self.same(generation, "a writer come to serve")?;
        }
        Ok(())
    }

    /// A flush of writer `c`'s.
    fn flush_of(&mut self, c: usize) -> Result<(), String> {
        let tag = self.clients[c].next_tag;
        self.clients[c].next_tag += 1;
        self.clients[c].ops.insert(tag, MOp { kind: K::Flush, offset: 0, len: 0, seq: self.seq, before: Vec::new(),
                                              at: self.now });
        let now = self.now;
        let mut out = Vec::new();
        let cl = &mut self.clients[c];
        cl.s.submit(UserOp::Flush { tag }, now, &mut cl.bufs, &mut out);
        self.outs(c, out)
    }

    /// An operation of the writer's -- a write or a flush alone, `writes`.
    fn op(&mut self, c: usize, writes: bool) -> Result<(), String> {
        let blocks = VOLUME_MIB << 20 >> 12;
        /* A few blocks, near each other, now and then across a chunk's end */
        let first = match self.r.below(4) {
            0 => SLOT / BLOCK - 1 - self.r.below(2),
            _ => self.r.below(blocks.min(3 * SLOT / BLOCK)),
        };
        let n = (1 + self.r.below(4)).min(blocks - first);
        let kind = match if writes && self.r.chance(80) { 0 } else if writes { 9 } else { self.r.below(10) } {
            0..=4 => K::Write(1 + self.r.below(255) as u8),
            5..=8 => K::Read,
            _ if self.r.chance(30) => K::Close,
            _ => K::Flush,
        };
        self.submit_op(c, kind, first, n)
    }

    /// An operation of `kind` on the `n` blocks from block `first` -- none,
    /// a flush's or a close's -- handed to client `c`, and to the model.
    fn submit_op(&mut self, c: usize, kind: K, first: u64, n: u64) -> Result<(), String> {
        let tag = self.clients[c].next_tag;
        self.clients[c].next_tag += 1;
        let (offset, len) = (first * BLOCK, (n * BLOCK) as u32);
        let seq = self.seq;
        let mut before = Vec::new();
        let user = match kind {
            K::Write(v) => {
                self.seq += 1;
                self.clients[c].bufs.0.insert(tag, vec![v; len as usize]);
                for b in first..first + n {
                    self.blocks.history.entry(b).or_default().push((seq, v, WState::Out));
                }
                UserOp::Write { tag, offset, len }
            }
            K::Read => {
                self.clients[c].bufs.0.insert(tag, vec![0u8; len as usize]);
                before = (first..first + n).map(|b| self.blocks.acked(b)).collect();
                UserOp::Read { tag, offset, len }
            }
            K::Flush => UserOp::Flush { tag },
            K::Close => UserOp::Close { tag },
        };
        if self.trace {
            eprintln!("  [{}] client {}: {:?} seq {} blocks {}+{} submitted", self.step, c, kind, seq, first, n);
        }
        self.clients[c].ops.insert(tag, MOp { kind, offset, len, seq, before, at: self.now });
        let now = self.now;
        let mut out = Vec::new();
        let cl = &mut self.clients[c];
        cl.s.submit(user, now, &mut cl.bufs, &mut out);
        self.outs(c, out)
    }

    /// Host `h` crashed: its cs gone, its disk's cache kept, lost or torn,
    /// its messages lost; its writer, if it had one, gone too.
    fn crash(&mut self, h: usize) -> Result<(), String> {
        self.count("hosts crashed");
        if self.trace {
            eprintln!("  [{}] host {} crashes", self.step, self.hosts[h].name);
        }
        self.hosts[h].store = None;
        let r = &mut self.r;
        self.hosts[h].disk.borrow_mut().cut(r);
        self.links.retain(|&(from, to), _| {
            from != End::Cs(h) && to != End::Cs(h) && from != End::Copy(h) && to != End::Copy(h)
        });
        self.down = Some(h);
        self.hosts[h].down_since = Some(self.now);
        self.hosts[h].copies.clear();
        let writers: Vec<usize> = (0..self.clients.len()).filter(|&c| self.clients[c].host == h && !self.clients[c].dead)
            .collect();
        for c in writers {
            self.lose_client(c);
        }
        Ok(())
    }

    /// Client `c` gone with its host: its writes out failed -- they may be
    /// there or not -- what comes to it lost, and what it sent its host's
    /// mds; what it sent a cs still on its way.
    fn lose_client(&mut self, c: usize) {
        self.count("writers lost");
        let ops: Vec<(u64, MOp)> = std::mem::take(&mut self.clients[c].ops).into_iter().collect();
        for (_, op) in ops {
            if let K::Write(_) = op.kind {
                let first = op.offset / BLOCK;
                for b in first..first + u64::from(op.len) / BLOCK {
                    if let Some(w) = self.blocks.history.get_mut(&b).and_then(|h| h.iter_mut().find(|w| w.0 == op.seq)) {
                        w.2 = WState::Failed;
                    }
                }
            }
        }
        self.clients[c].zombie = true;
        self.clients[c].dead = true;
        self.links.retain(|&(from, to), _| to != End::Client(c) && !(from == End::Client(c) && matches!(to, End::Mds(_))));
    }

    /// Host `h` back: its cs opened again from its disk, and every block it
    /// has in held to what a crash may leave.
    fn restart(&mut self, h: usize) -> Result<(), String> {
        self.count("hosts back");
        if self.trace {
            eprintln!("  [{}] host {} comes back", self.step, self.hosts[h].name);
        }
        let mut store = Store::new();
        let handle = Handle(self.hosts[h].disk.clone());
        if let Err((_, e)) = store.open_device(handle, self.hosts[h].identity) {
            return Err(format!("host {}'s disk not opened again: {:?}", self.hosts[h].name, e));
        }
        if self.trace {
            let open: Vec<(u32, u64)> = store.holdings().iter().filter(|x| x.state == nbscs::SlotState::Open)
                .map(|x| (x.chunk, x.generation)).collect();
            eprintln!("  [{}] host {} finds open (chunk, generation): {:?}", self.step, self.hosts[h].name, open);
        }
        self.hosts[h].store = Some(store);
        self.hosts[h].vouched = Vouched::new();
        self.hosts[h].down_since = None;
        self.hosts[h].joins_seen = false;
        self.hosts[h].joins_at = 0;
        self.down = None;
        /* Every block a replica the metadata has in here holds: what was
           flushed, or written since */
        let blocks: Vec<u64> = self.blocks.history.keys().copied().collect();
        for b in blocks {
            let Some((volume, chunk, inside, hosts)) = self.replicas(b, 0) else { continue };
            if !hosts.contains(&h) {
                continue;
            }
            let v = self.block_on(h, volume, chunk, inside)
                .map_err(|e| format!("host {} back: block {}: {}", self.hosts[h].name, b, e))?;
            let may = self.blocks.durable(b);
            if !may.contains(&v) {
                return Err(format!("host {} back: block {} is {}, which a crash may leave as {:?}", self.hosts[h].name,
                                   b, v, may));
            }
        }
        Ok(())
    }

    /// A writer on host `h`, taking the volume by force.
    fn writer(&mut self, h: usize) -> Result<(), String> {
        self.count("writers started");
        let c = self.clients.len();
        let token = 1 + self.r.below(u64::MAX - 1);
        let mut s = Session::new(VOLUME, HOSTS[h], true, true, token);
        let mut out = Vec::new();
        s.start(self.now, &mut out);
        self.clients.push(Client { host: h, s, bufs: Bufs::default(), ops: BTreeMap::new(), next_tag: 1, zombie: false,
                                   dead: false, served: false, unflushed: Vec::new(), quiet: false,
                                   shown: Phase::Attaching });
        self.outs(c, out)
    }

    /// Whether every chunk of the volume has a replica in on a host other
    /// than `h`: a writer can do without `h`.
    fn whole_without(&self, h: usize) -> bool {
        let Some(v) = self.state.volume(VOLUME) else { return false };
        v.map.iter().all(|&c| {
            self.state.chunks().get(c as usize).and_then(|ch| self.state.sets().get(ch.set as usize))
                .is_some_and(|s| (0..nbsmeta::REPLICAS).any(|i| {
                    s.replicas[i] != Replica::Down && self.host_of_cs(s.cs[i]).is_some_and(|x| x != h)
                }))
        })
    }

    /// Whether host `h` holds a replica in of some chunk of the volume.
    fn holds_some(&self, h: usize) -> bool {
        let Some(v) = self.state.volume(VOLUME) else { return false };
        v.map.iter().any(|&c| {
            self.state.chunks().get(c as usize).and_then(|ch| self.state.sets().get(ch.set as usize))
                .is_some_and(|s| (0..nbsmeta::REPLICAS).any(|i| {
                    s.replicas[i] != Replica::Down && self.host_of_cs(s.cs[i]) == Some(h)
                }))
        })
    }

    /// Whether host `h`'s cs is among the three of some set of the volume's
    /// chunks, in any state, or its joiner.
    fn in_some_set(&self, h: usize) -> bool {
        let Some(v) = self.state.volume(VOLUME) else { return false };
        let cs = self.hosts[h].cs;
        v.map.iter().any(|&c| {
            self.state.chunks().get(c as usize).and_then(|ch| self.state.sets().get(ch.set as usize))
                .is_some_and(|s| s.cs.contains(&cs) || s.joiner.is_some_and(|j| j.cs == cs))
        })
    }

    /// Whether client `c` serves: the writer, started, not stopping.
    fn serving(&self, c: usize) -> bool {
        !self.clients[c].zombie && self.clients[c].served && *self.clients[c].s.phase() == Phase::Serving
    }

    fn stat(&self, what: &str) -> u64 {
        self.stats.get(what).copied().unwrap_or(0)
    }

    /// The network let run as at the end -- a message at a time, time going
    /// by -- until `done` holds of client `c`, or long past any deadline.
    fn drive(&mut self, c: usize, done: &dyn Fn(&World, usize) -> bool) -> Result<(), String> {
        for _ in 0..30_000 {
            if done(self, c) {
                break;
            }
            self.now += 5 * MS;
            if self.links.values().all(|q| q.is_empty()) {
                self.now += 200 * MS;
            }
            self.deliver()?;
            self.tick()?;
        }
        Ok(())
    }

    /// nbs-test's repair phase, where a close waited for good on a cs no set
    /// had in any more, though it owed a flush: every chunk written, so that
    /// each replica in owes a flush; a host that holds some, and that every
    /// chunk can do without, lost for good; every chunk written past it, so
    /// that it is out of every set while still owed a flush; then a flush
    /// and a close, which must be done, as the writes past it must. The host
    /// lost, if the writer served to the loss.
    fn lose_written(&mut self, replaced: bool) -> Result<Option<usize>, String> {
        let Some(c) = self.current() else { return Ok(None) };
        let idle = |w: &World, c: usize| w.clients[c].ops.is_empty() || !w.serving(c);
        self.drive(c, &|w, c| {
            w.serving(c) || w.clients[c].zombie || matches!(w.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached)
        })?;
        self.drive(c, &idle)?;
        if !self.serving(c) {
            return Ok(None);
        }
        let places = (VOLUME_MIB << 20) / SLOT;
        for p in 0..places {
            let v = 1 + self.r.below(255) as u8;
            self.submit_op(c, K::Write(v), p * (SLOT / BLOCK), 1)?;
        }
        self.drive(c, &idle)?;
        let up: Vec<usize> = (0..HOSTS.len())
            .filter(|&h| h != self.clients[c].host && self.holds_some(h) && self.whole_without(h)).collect();
        let Some(h) = self.r.pick(&up) else { return Ok(None) };
        if !self.serving(c) {
            return Ok(None);
        }
        self.crash(h)?;
        self.count("hosts lost for good, written past");
        let failed = |w: &World| w.stat("writes failed") + w.stat("flushes failed") + w.stat("closes failed");
        let before = failed(self);
        /* Now and then the controller first: the host's cs down in every set
           before the writer touches them, its view not having read it -- a
           flush then goes there, owed one; the writes past it read the map
           again, dropping it, and the flush is to be settled by the view, in
           no set of which the host is */
        if self.r.chance(50) {
            let cs = self.hosts[h].cs;
            /* ... and, `replaced`, until its places are all others': the
               writer's asks to take it out are then refused as no replica
               of their sets, its view behind them (found by this target,
               N9d: asked again for good, the map never read again) */
            self.drive(c, &|w, c| {
                !w.serving(c) || if replaced {
                    !w.in_some_set(h)
                } else {
                    w.state.cs().get(cs as usize).is_some_and(|x| x.state != nbsmeta::CsState::Up)
                }
            })?;
            if replaced && !self.in_some_set(h) {
                self.count("flushes out to a host whose places are others'");
            }
            if self.serving(c) {
                self.count("flushes out to a host the controller has down");
                self.submit_op(c, K::Flush, 0, 0)?;
                /* Its places others': the flush waited for alone -- writes
                   past it would meet epochs the copies moved on, and read
                   the map again whatever the asks were answered */
                if replaced {
                    self.drive(c, &idle)?;
                }
            }
        }
        for p in 0..places {
            let v = 1 + self.r.below(255) as u8;
            self.submit_op(c, K::Write(v), p * (SLOT / BLOCK) + 1, 1)?;
        }
        self.drive(c, &idle)?;
        /* Out of every set now, by the writes past it, and still owed a flush
           -- as the writer's view has it too: the controller may have taken
           it out first, which the view has not read yet */
        if self.serving(c) && failed(self) == before && !self.holds_some(h)
            && !self.clients[c].s.writes(self.hosts[h].cs) {
            self.owed = Some((c, h));
        }
        let sent = self.stat("flushes to a cs out of every set");
        self.submit_op(c, K::Flush, 0, 0)?;
        self.drive(c, &idle)?;
        self.submit_op(c, K::Close, 0, 0)?;
        self.drive(c, &idle)?;
        self.owed = None;
        if self.stat("flushes to a cs out of every set") > sent {
            return Err(format!("host {} lost for good and out of every set: client {} sent it a flush all the same -- {}",
                               self.hosts[h].name, c, self.clients[c].s.report()));
        }
        if self.serving(c) && self.whole_without(h) && failed(self) > before {
            return Err(format!("host {} lost for good, every chunk with a replica in on another: client {}'s writes \
                                past it, its flush or its close failed -- {}", self.hosts[h].name, c,
                               self.clients[c].s.report()));
        }
        Ok(Some(h))
    }

    /// A close sent to a cs no longer in its chunk's set, by a view from
    /// before -- which nbsclient sent again on each map read for good, the
    /// cs refusing it as not in the set (`NotHere`; found by this target,
    /// N9d): every chunk written and flushed, so that each is open on its
    /// replicas; a host that holds some, and that every chunk can do
    /// without, crashed with them open and drained; the writer stalled while
    /// the rest goes on, until the controller has the host's cs down in
    /// every set -- drained, it joins none again -- its view from before;
    /// the host back with the chunks found open. Then the writer's closes go
    /// to it, and must be done. Whether it was shaped.
    fn close_drained(&mut self) -> Result<bool, String> {
        if self.drained.is_some() {
            return Ok(false);
        }
        let Some(c) = self.current() else { return Ok(false) };
        let idle = |w: &World, c: usize| w.clients[c].ops.is_empty() || !w.serving(c);
        self.drive(c, &|w, c| {
            w.serving(c) || w.clients[c].zombie || matches!(w.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached)
        })?;
        self.drive(c, &idle)?;
        if !self.serving(c) {
            return Ok(false);
        }
        let places = (VOLUME_MIB << 20) / SLOT;
        for p in 0..places {
            let v = 1 + self.r.below(255) as u8;
            self.submit_op(c, K::Write(v), p * (SLOT / BLOCK), 1)?;
        }
        self.drive(c, &idle)?;
        self.submit_op(c, K::Flush, 0, 0)?;
        self.drive(c, &idle)?;
        let up: Vec<usize> = (0..HOSTS.len())
            .filter(|&h| h != self.clients[c].host && self.holds_some(h) && self.whole_without(h)).collect();
        let Some(h) = self.r.pick(&up) else { return Ok(false) };
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        self.crash(h)?;
        self.drain(h)?;
        /* The writer stalled -- its timers not run, nothing of its about --
           the rest going on */
        for _ in 0..60_000 {
            if !self.holds_some(h) {
                break;
            }
            self.now += 5 * MS;
            if self.links.values().all(|q| q.is_empty()) {
                self.now += 100 * MS;
            }
            self.deliver()?;
            self.control()?;
            for x in 0..self.hosts.len() {
                self.joins(x)?;
                self.copy_turn(x)?;
            }
        }
        let out = !self.holds_some(h);
        self.restart(h)?;
        if out {
            self.count("closes shaped to a cs drained down in every set");
        }
        Ok(out)
    }

    /// A copy's first source restarted with chunks found open -- which may
    /// lack writes it acknowledged, what its disk's cache lost -- while a
    /// joiner copies its set: the copy is to take another source (found by
    /// this target, N9d: a joiner copied from such a replica, and once
    /// active held a write done as it was before). Every chunk written and
    /// flushed, then written again; a set's first active member -- the copy
    /// asks it first -- on a host that every chunk can do without, crashed
    /// and back before the controller would have its cs down, its chunks
    /// found open; the writer stalled; another member of the set drained,
    /// until its place is another's. Then the end goes on: every replica
    /// in alike. Whether it was shaped.
    fn source_found_open(&mut self) -> Result<bool, String> {
        if self.drained.is_some() {
            return Ok(false);
        }
        let Some(c) = self.current() else { return Ok(false) };
        let idle = |w: &World, c: usize| w.clients[c].ops.is_empty() || !w.serving(c);
        self.drive(c, &|w, c| {
            w.serving(c) || w.clients[c].zombie || matches!(w.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached)
        })?;
        self.drive(c, &idle)?;
        if !self.serving(c) {
            return Ok(false);
        }
        let places = (VOLUME_MIB << 20) / SLOT;
        for round in 0..2 {
            for p in 0..places {
                let v = 1 + self.r.below(255) as u8;
                self.submit_op(c, K::Write(v), p * (SLOT / BLOCK) + 2 + round, 1)?;
            }
            self.drive(c, &idle)?;
            if round == 0 {
                self.submit_op(c, K::Flush, 0, 0)?;
                self.drive(c, &idle)?;
            }
        }
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        /* A set whole, its first active member's host and another's */
        let Some(v) = self.state.volume(VOLUME) else { return Ok(false) };
        let sets: BTreeSet<u32> = v.map.iter().filter_map(|&ch| self.state.chunks().get(ch as usize).map(|x| x.set))
            .collect();
        let mut pairs = Vec::new();
        for set in sets {
            let Some(s) = self.state.sets().get(set as usize) else { continue };
            if s.joiner.is_some() || s.replicas.iter().any(|&r| r != Replica::Active) {
                continue;
            }
            let (Some(x), Some(y)) = (self.host_of_cs(s.cs[0]), self.host_of_cs(s.cs[1])) else { continue };
            if x != self.clients[c].host && self.whole_without(x) {
                pairs.push((set, x, y));
            }
        }
        let Some((set, x, y)) = self.r.pick(&pairs) else { return Ok(false) };
        self.crash(x)?;
        self.now += 300 * MS;
        self.restart(x)?;
        self.drain(y)?;
        let ycs = self.hosts[y].cs;
        /* The writer stalled, as in `close_drained` */
        for _ in 0..60_000 {
            if self.state.sets().get(set as usize).is_none_or(|s| !s.cs.contains(&ycs)) {
                break;
            }
            self.now += 5 * MS;
            if self.links.values().all(|q| q.is_empty()) {
                self.now += 100 * MS;
            }
            self.deliver()?;
            self.control()?;
            for h in 0..self.hosts.len() {
                self.joins(h)?;
                self.copy_turn(h)?;
            }
        }
        let shaped = self.state.sets().get(set as usize).is_some_and(|s| !s.cs.contains(&ycs));
        if shaped {
            self.count("copies shaped from a first source with chunks found open");
        }
        Ok(shaped)
    }

    /// The writer now: the newest not lost or taken over.
    fn current(&self) -> Option<usize> {
        (0..self.clients.len()).rev().find(|&c| !self.clients[c].zombie)
    }
}

fn setup(r: &mut Rng) -> Result<World, String> {
    let mut w = World { r: Rng::new(r.below(u64::MAX)), now: 0, step: 0, trace: false, state: State::new(), index: 0,
                        hosts: Vec::new(), links: BTreeMap::new(), clients: Vec::new(), blocks: Blocks::default(),
                        seq: 1, down: None, served: 0, rest: None, owed: None, plan_at: 0, drained: None,
                        stats: BTreeMap::new() };
    w.apply(Command::Init { cluster: CLUSTER, name: String::from("nbs"), host: String::from(HOSTS[0]), member: 1 });
    for (k, h) in HOSTS.iter().enumerate().skip(1) {
        w.apply(Command::Add { host: h.to_string(), member: 1 + k as u64 });
    }
    let bytes = DISK_SLOTS * SLOT;
    let slots = nbscs::device_slots(bytes);
    for h in HOSTS {
        match w.apply(Command::DeviceAdd { host: h.to_string(), ident: format!("disk:{}", h), slots }) {
            Answer::Device { .. } => {}
            other => return Err(format!("device of {} not added: {:?}", h, other)),
        }
    }
    let home = HOSTS[r.below(HOSTS.len() as u64) as usize];
    match w.apply(Command::VolumeCreate { name: String::from(VOLUME), size_mib: VOLUME_MIB, host: home.to_string() }) {
        Answer::Volume { .. } => {}
        other => return Err(format!("the volume not made: {:?}", other)),
    }
    for h in HOSTS {
        let cs = w.state.cs_of(h).ok_or_else(|| format!("no cs of {}", h))?;
        let identity = Identity { cluster: CLUSTER, cs: cs.id, device: cs.devices[0].id };
        let disk = Rc::new(RefCell::new(Disk::new(4096, bytes)));
        let mut store = Store::new();
        if let Err((_, e)) = store.format_device(Handle(disk.clone()), identity, 1 + r.below(u64::MAX - 1)) {
            return Err(format!("disk of {} not formatted: {:?}", h, e));
        }
        w.hosts.push(Host { name: h, cs: cs.id, disk, identity, store: Some(store), vouched: Vouched::new(),
                            down_since: None, copies: Vec::new(), joins_seen: false, joins_at: 0, copy_id: 1 });
    }
    Ok(w)
}

pub fn run(seed: u64, steps: u64, trace: bool) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x636c_7573));
    let mut w = setup(&mut r)?;
    w.trace = trace;
    let first = w.r.below(HOSTS.len() as u64) as usize;
    w.writer(first)?;
    let mut back_at = 0u64;
    for step in 1..=steps {
        w.step = step;
        if w.now >= RUN_NS {
            break;
        }
        /* Time goes by: mostly a little, now and then past a silence */
        w.now += if w.r.chance(2) { (300 + w.r.below(1200)) * MS } else { w.r.below(6) * MS };
        /* What is about delivered, a few at a time, in any order of pairs */
        for _ in 0..w.r.below(DELIVERIES) {
            w.deliver().map_err(|e| format!("step {}: {}", step, e))?;
        }
        match w.r.below(1000) {
            0..=239 => {
                if let Some(c) = w.current().filter(|_| w.rest.is_none()) {
                    if w.clients[c].served && w.clients[c].s.busy() < 6 {
                        w.op(c, false).map_err(|e| format!("step {}: {}", step, e))?;
                    }
                }
            }
            240..=299 => {
                /* A writer taken over written to still -- its host's driver
                   asks on until it learns it is fenced */
                let old: Vec<usize> = (0..w.clients.len()).filter(|&c| {
                    let x = &w.clients[c];
                    x.zombie && !x.dead && !x.quiet && x.served && *x.s.phase() == Phase::Serving && x.s.busy() < 6
                }).collect();
                if let Some(c) = w.r.pick(&old) {
                    w.count("asked of a writer taken over");
                    w.op(c, true).map_err(|e| format!("step {}: {}", step, e))?;
                }
            }
            300..=309 => {
                /* A host crashes -- one at a time, now and then -- and comes
                   back a while on */
                match w.down {
                    None if w.r.chance(30) => {
                        let h = w.r.below(HOSTS.len() as u64) as usize;
                        w.crash(h).map_err(|e| format!("step {}: {}", step, e))?;
                        back_at = step + 20 + w.r.below(200);
                    }
                    _ => {}
                }
            }
            313 => {
                /* A host drained, at most one a run */
                if w.drained.is_none() && w.r.chance(50) {
                    let h = w.r.below(HOSTS.len() as u64) as usize;
                    w.drain(h).map_err(|e| format!("step {}: {}", step, e))?;
                }
            }
            310..=312 => {
                /* The writer taken over by force from another host, its
                   messages still about -- now, or once it is at rest */
                if w.rest.is_none() && w.r.chance(50) && w.clients.len() < 6 {
                    if let Some(c) = w.current().filter(|&c| *w.clients[c].s.phase() == Phase::Serving) {
                        w.count("writers let rest to be taken over");
                        w.flush_of(c).map_err(|e| format!("step {}: {}", step, e))?;
                        w.rest = Some((step + REST_STEPS, c));
                    }
                } else if w.rest.is_none() && w.r.chance(30) && w.clients.len() < 6 {
                    /* Of a writer serving: one still starting would take the
                       volume back by force, as it was told to */
                    if let Some(c) = w.current().filter(|&c| *w.clients[c].s.phase() == Phase::Serving) {
                        let up: Vec<usize> = (0..HOSTS.len()).filter(|&h| Some(h) != w.down && h != w.clients[c].host)
                            .collect();
                        if let Some(h) = w.r.pick(&up) {
                            if w.trace {
                                eprintln!("  [{}] client {} taken over from host {}", step, c, HOSTS[h]);
                            }
                            w.count("takeovers");
                            w.clients[c].zombie = true;
                            w.clients[c].quiet = w.clients[c].s.busy() == 0 && w.r.chance(70);
                            w.writer(h).map_err(|e| format!("step {}: {}", step, e))?;
                        }
                    }
                }
            }
            _ => {}
        }
        if let Some(h) = w.down.filter(|_| step >= back_at) {
            w.restart(h).map_err(|e| format!("step {}: {}", step, e))?;
        }
        if let Some((at, c)) = w.rest.filter(|&(at, _)| step >= at) {
            let _ = at;
            w.rest = None;
            if w.current() == Some(c) && *w.clients[c].s.phase() == Phase::Serving {
                let up: Vec<usize> = (0..HOSTS.len()).filter(|&h| Some(h) != w.down && h != w.clients[c].host)
                    .collect();
                if let Some(h) = w.r.pick(&up) {
                    if w.trace {
                        eprintln!("  [{}] client {}, at rest, taken over from host {}", step, c, HOSTS[h]);
                    }
                    w.count("takeovers at rest");
                    w.clients[c].zombie = true;
                    w.clients[c].quiet = w.clients[c].s.busy() == 0;
                    w.writer(h).map_err(|e| format!("step {}: {}", step, e))?;
                }
            }
        }
        /* A writer lost with its host: another starts */
        if w.current().is_none() {
            let up: Vec<usize> = (0..HOSTS.len()).filter(|&h| Some(h) != w.down).collect();
            if let Some(h) = w.r.pick(&up) {
                w.writer(h).map_err(|e| format!("step {}: {}", step, e))?;
            }
        }
        w.tick().map_err(|e| format!("step {}: {}", step, e))?;
    }
    /* The end: the host down back -- or, where every chunk has a replica in
       on another, now and then lost for good -- the writer stopped, one
       started on a host up where there is none, or where the last failed as
       it may, and every replica on a host up alike */
    let mut lost = None;
    if let Some(h) = w.down {
        if w.r.chance(50) && w.whole_without(h) {
            w.count("hosts lost for good");
            if w.trace {
                eprintln!("  [end] host {} lost for good", w.hosts[h].name);
            }
            lost = Some(h);
        } else {
            w.restart(h).map_err(|e| format!("the end: {}", e))?;
        }
    }
    /* The ends shaped since N9d's second round drawn of their own, so that
       a run they do not shape goes as it did */
    let mut ends = Rng::new(mix(seed, 0x636c_6f73));
    let replaced = ends.chance(50);
    let mut shaped = false;
    if lost.is_none() && w.r.chance(30) {
        shaped = true;
        lost = w.lose_written(replaced).map_err(|e| format!("the end: {}", e))?;
    }
    if !shaped && lost.is_none() && ends.chance(40) {
        if ends.chance(50) {
            w.close_drained().map_err(|e| format!("the end: {}", e))?;
        } else {
            w.source_found_open().map_err(|e| format!("the end: {}", e))?;
        }
    }
    for _ in 0..END_WRITERS {
        let c = match w.current() {
            Some(c) => c,
            None => {
                let up: Vec<usize> = (0..HOSTS.len()).filter(|&h| Some(h) != w.down).collect();
                let h = w.r.pick(&up).ok_or_else(|| String::from("the end: no host up"))?;
                w.writer(h).map_err(|e| format!("the end: {}", e))?;
                w.clients.len() - 1
            }
        };
        /* Writes out when the stop is asked, the last ones over each other
           -- what was asked before it done before it */
        if w.clients[c].served && *w.clients[c].s.phase() == Phase::Serving {
            for _ in 0..w.r.below(5) {
                w.op(c, false).map_err(|e| format!("the end: {}", e))?;
            }
        }
        let mut out = Vec::new();
        let now = w.now;
        {
            let cl = &mut w.clients[c];
            cl.s.stop(u64::MAX, now, &mut cl.bufs, &mut out);
        }
        w.outs(c, out).map_err(|e| format!("the end: {}", e))?;
        for _ in 0..20_000 {
            if matches!(w.clients[c].s.phase(), Phase::Detached | Phase::Failed(_)) {
                break;
            }
            w.now += 5 * MS;
            if w.links.values().all(|q| q.is_empty()) {
                w.now += 200 * MS;
            }
            w.deliver().map_err(|e| format!("the end: {}", e))?;
            w.tick().map_err(|e| format!("the end: {}", e))?;
        }
        let phase = w.clients[c].s.phase().clone();
        match phase {
            Phase::Detached => {
                w.settle(lost).map_err(|e| format!("the end: {}", e))?;
                let generation = w.clients[c].s.generation();
                w.same(generation, "the end, detached")?;
                if std::env::var_os("NBS_CLUSTER_STATS").is_some() {
                    eprintln!("cluster seed {}: {:?}", seed, w.stats);
                }
                return Ok(());
            }
            /* As it may -- check_failed said so: another */
            Phase::Failed(_) => continue,
            /* Held up by a chunk whose replicas in are all on the host lost
               -- the writers' take-outs left it there: nowhere else to be */
            _ if lost.is_some_and(|h| !w.whole_without(h)) => {
                w.count("stops held by a chunk on the host lost alone");
                return Ok(());
            }
            _ => return Err(format!("the last writer not detached at the end: {}", w.clients[c].s.report())),
        }
    }
    Err(format!("no writer of {} at the end detached", END_WRITERS))
}
