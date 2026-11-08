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
//!   refuses it (`nbsclient::LOST_LAST`), or once taken over;
//! - **a writer goes on and stops** with a host lost for good, as long as
//!   every chunk has a replica in on another: none of what it asks -- a
//!   write, a flush, a close -- fails at its deadline, and none waits on a
//!   cs no set of the volume writes any more (nbs-test's repair phase, where
//!   a close waited for good on a cs no set had in, shaped at the end of
//!   some runs: every chunk written, a host that holds some lost, every
//!   chunk written past it, then a flush and a close) -- and no flush goes
//!   to that cs at all once no set has it in.
//!
//! A writer that follows another holds what that one had done and not
//! flushed as a crash would leave it: there or not -- a replica that lost it
//! may be what its reconcile copied.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

use nbsclient::{Buffers, Out, Phase, Session, To, UserOp, BLOCK, LOST_LAST};
use nbscs::{ChunkRef, Epoch, Identity, Store, SLOT};
use nbsmeta::{Answer, Command, Replica, State};
use nbswire::serve::{serve, Vouched};
use nbswire::{Answer as CsAnswer, Ask, Asking, Reply, Request, Said, Saying};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum End {
    Client(usize),
    Cs(usize),
    Mds(usize),
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
                    let h = self.clients[c].host;
                    self.push(End::Client(c), End::Mds(h), bytes, id);
                }
                Out::Send { to: To::Cs(cs), id, bytes } => {
                    let Some(h) = self.host_of_cs(cs) else { return Err(format!("a request to cs {}, none", cs)) };
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
            _ => Ok(()),
        }
    }

    /// A request at host `h`'s cs: done by nbswire's `serve`, the cs asking
    /// the mds as its own host's -- or, its host down, lost or bounced.
    fn at_cs(&mut self, c: usize, h: usize, m: Msg) -> Result<(), String> {
        if self.owed == Some((c, h)) && Request::decode(&m.bytes).is_some_and(|r| matches!(r.op, nbswire::Op::Flush)) {
            self.count("flushes to a cs out of every set");
        }
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
        self.clients[c].ops.insert(tag, MOp { kind: K::Flush, offset: 0, len: 0, seq: self.seq, before: Vec::new() });
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
        self.clients[c].ops.insert(tag, MOp { kind, offset, len, seq, before });
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
        self.links.retain(|&(from, to), _| from != End::Cs(h) && to != End::Cs(h));
        self.down = Some(h);
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
        self.hosts[h].store = Some(store);
        self.hosts[h].vouched = Vouched::new();
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

    /// Whether client `c` serves: the writer, started, not stopping.
    fn serving(&self, c: usize) -> bool {
        !self.clients[c].zombie && self.clients[c].served && *self.clients[c].s.phase() == Phase::Serving
    }

    fn stat(&self, what: &str) -> u64 {
        self.stats.get(what).copied().unwrap_or(0)
    }

    /// The network let run as at the end -- a message at a time, time going
    /// by -- until `done` holds of client `c`, or long past any deadline.
    fn drive(&mut self, c: usize, done: fn(&World, usize) -> bool) -> Result<(), String> {
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
    fn lose_written(&mut self) -> Result<Option<usize>, String> {
        let Some(c) = self.current() else { return Ok(None) };
        let idle: fn(&World, usize) -> bool = |w, c| w.clients[c].ops.is_empty() || !w.serving(c);
        self.drive(c, |w, c| {
            w.serving(c) || w.clients[c].zombie || matches!(w.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached)
        })?;
        self.drive(c, idle)?;
        if !self.serving(c) {
            return Ok(None);
        }
        let places = (VOLUME_MIB << 20) / SLOT;
        for p in 0..places {
            let v = 1 + self.r.below(255) as u8;
            self.submit_op(c, K::Write(v), p * (SLOT / BLOCK), 1)?;
        }
        self.drive(c, idle)?;
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
        for p in 0..places {
            let v = 1 + self.r.below(255) as u8;
            self.submit_op(c, K::Write(v), p * (SLOT / BLOCK) + 1, 1)?;
        }
        self.drive(c, idle)?;
        /* Out of every set now, by the writes past it, and still owed a flush */
        if self.serving(c) && failed(self) == before && !self.holds_some(h) {
            self.owed = Some((c, h));
        }
        let sent = self.stat("flushes to a cs out of every set");
        self.submit_op(c, K::Flush, 0, 0)?;
        self.drive(c, idle)?;
        self.submit_op(c, K::Close, 0, 0)?;
        self.drive(c, idle)?;
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

    /// The writer now: the newest not lost or taken over.
    fn current(&self) -> Option<usize> {
        (0..self.clients.len()).rev().find(|&c| !self.clients[c].zombie)
    }
}

fn setup(r: &mut Rng) -> Result<World, String> {
    let mut w = World { r: Rng::new(r.below(u64::MAX)), now: 0, step: 0, trace: false, state: State::new(), index: 0,
                        hosts: Vec::new(), links: BTreeMap::new(), clients: Vec::new(), blocks: Blocks::default(),
                        seq: 1, down: None, served: 0, rest: None, owed: None, stats: BTreeMap::new() };
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
        w.hosts.push(Host { name: h, cs: cs.id, disk, identity, store: Some(store), vouched: Vouched::new() });
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
    if lost.is_none() && w.r.chance(30) {
        lost = w.lose_written().map_err(|e| format!("the end: {}", e))?;
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
