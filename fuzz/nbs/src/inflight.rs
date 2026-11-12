//! The `inflight` target (N12b): the cs's engine (src/crates/common/storage/
//! nbscs) with many requests' operations in flight at once on one task, as
//! nbs-cs.ko's event-driven loop has them (N12c) -- over disks whose
//! requests are in flight too, done in whatever order the fuzzer picks,
//! with a write cache: a write done sits in it until a flush begun after it
//! is done, and a power cut, with requests out, keeps, loses or tears at
//! its sectors each write the cache holds and each still in flight.
//!
//! The fuzzer is the cs's clients and its mds: volumes fenced at their
//! generations, chunks written a sector at a time with a value of their own,
//! read, closed and flushed -- by the newest writer of a volume, and now
//! and then by one a newer fence has taken it from.
//!
//! Held to, as it goes:
//! - a read answered with the last write of its chunk answered before it was
//!   asked: a chunk's requests one at a time, in the order they came;
//! - no write of a generation answered once a newer fence of its volume has
//!   been, and one asked after it refused as fenced;
//! - every request answered once nothing more is asked -- nothing waits for
//!   ever -- and then no turn held and no round running;
//! - a write's count of the joins of its set begun as it was marked
//!   (`Store::write_marked`, N13) the one at its mark: where no join has
//!   begun since and one is on, it marked the write. Joins of the chunks'
//!   sets are begun and ended at random, among the requests in flight.
//!
//! And after every cut, the cs opened again: no record lost (never both
//! copies of a page in flight), every fence answered there, every chunk
//! written and not closed since -- a close asked after its write -- open,
//! every sector of a chunk closed reading what it held at the close, and
//! every other sector what it held at the last flush or close answered after
//! its write, or a value written since.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use nbscs::{ChunkRef, Device, Epoch, Error, Identity, IoError, SlotState, Store, PAGE, SLOT};

use crate::rng::{mix, Rng};

const VOLUMES: u32 = 2;
const CHUNKS: u32 = 6;
/// The sectors of a chunk the fuzzer writes: few, so that writes meet.
const SECTORS: u32 = 6;

/* ---- the disk ---- */

/// A request on its way.
enum Req {
    Read { at: u64, len: usize },
    Write { at: u64, data: Vec<u8> },
    /// Covers the writes done before it was asked: the cache's below `upto`.
    Flush { upto: u64 },
}

struct Flying {
    req: Req,
    /// Done: what it read, for a read.
    done: Option<Vec<u8>>,
}

/// A disk with requests in flight and a write cache.
pub(crate) struct ADisk {
    sector: u64,
    bytes: u64,
    /// Where the cs's own -- superblock, fences, table -- ends.
    own_end: u64,
    /// Durable sectors; absent ones zeros.
    durable: BTreeMap<u64, Vec<u8>>,
    /// The writes done and not yet durable, by the order they were done in.
    cache: Vec<(u64, u64, Vec<u8>)>,
    next_seq: u64,
    flying: BTreeMap<u64, Flying>,
    next_id: u64,
}

impl ADisk {
    fn new(sector: u64, bytes: u64) -> ADisk {
        let own_end = nbscs::layout(bytes).map_or(bytes, |l| l.data);
        ADisk { sector, bytes, own_end, durable: BTreeMap::new(), cache: Vec::new(), next_seq: 1,
                flying: BTreeMap::new(), next_id: 1 }
    }

    /// `data` at `at` onto the medium, where `keep` says of each sector.
    fn lay(durable: &mut BTreeMap<u64, Vec<u8>>, sector: u64, at: u64, data: &[u8], keep: &mut dyn FnMut() -> bool) {
        for (k, s) in data.chunks(sector as usize).enumerate() {
            if !keep() {
                continue;
            }
            let n = at / sector + k as u64;
            if s.iter().all(|&b| b == 0) {
                durable.remove(&n);
            } else {
                durable.insert(n, s.to_vec());
            }
        }
    }

    /// What a read at `at` of `len` bytes finds: the medium, the cache over
    /// it in the order it was done.
    fn read_now(&self, at: u64, len: usize) -> Vec<u8> {
        let s = self.sector;
        let mut buf = vec![0u8; len];
        let (first, end) = (at / s, (at + len as u64) / s);
        for (&n, v) in self.durable.range(first..end) {
            let k = ((n - first) * s) as usize;
            buf[k..k + s as usize].copy_from_slice(v);
        }
        for (_, wat, data) in &self.cache {
            let (from, to) = (wat / s, wat / s + data.len() as u64 / s);
            let (a, b) = (from.max(first), to.min(end));
            if a < b {
                buf[((a - first) * s) as usize..((b - first) * s) as usize]
                    .copy_from_slice(&data[((a - from) * s) as usize..((b - from) * s) as usize]);
            }
        }
        buf
    }

    /// Request `id` done.
    fn complete(&mut self, id: u64) {
        let Some(f) = self.flying.get(&id) else { return };
        if f.done.is_some() {
            return;
        }
        let out = match &f.req {
            Req::Read { at, len } => self.read_now(*at, *len),
            Req::Write { at, data } => {
                let (at, data) = (*at, data.clone());
                let seq = self.next_seq;
                self.next_seq += 1;
                self.cache.push((seq, at, data));
                Vec::new()
            }
            Req::Flush { upto } => {
                let upto = *upto;
                let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.cache).into_iter()
                    .partition(|(seq, _, _)| *seq < upto);
                for (_, at, data) in now {
                    ADisk::lay(&mut self.durable, self.sector, at, &data, &mut || true);
                }
                self.cache = later;
                Vec::new()
            }
        };
        if let Some(f) = self.flying.get_mut(&id) {
            f.done = Some(out);
        }
    }

    /// Requests not yet done.
    fn waiting(&self) -> Vec<u64> {
        self.flying.iter().filter(|(_, f)| f.done.is_none()).map(|(&id, _)| id).collect()
    }

    /// The power cut: every write the cache holds, and every write in
    /// flight, kept, lost or torn at its sectors; reads and flushes out
    /// gone with it.
    fn cut(&mut self, r: &mut Rng) {
        let mut writes: Vec<(u64, Vec<u8>)> = std::mem::take(&mut self.cache).into_iter()
            .map(|(_, at, data)| (at, data)).collect();
        for f in std::mem::take(&mut self.flying).into_values() {
            if let (Req::Write { at, data }, None) = (f.req, f.done) {
                writes.push((at, data));
            }
        }
        for (at, data) in writes {
            match r.below(10) {
                0..=4 => ADisk::lay(&mut self.durable, self.sector, at, &data, &mut || true),
                5..=7 => {}
                _ => {
                    let mut t = Rng::new(r.next());
                    ADisk::lay(&mut self.durable, self.sector, at, &data, &mut || t.chance(50));
                }
            }
            /* Now and then one of the cs's own sectors torn inside: new up to
               a byte, as it was past it -- a record across the tear garbage,
               which only its other copy, never in flight with it, saves */
            if at < self.own_end && r.chance(30) {
                let s = self.sector as usize;
                let k = r.below((data.len() / s) as u64) as usize;
                let n = at / self.sector + k as u64;
                let mut v = self.durable.get(&n).cloned().unwrap_or_else(|| vec![0; s]);
                let cut_at = 1 + r.below(s as u64 - 1) as usize;
                v[..cut_at].copy_from_slice(&data[k * s..k * s + cut_at]);
                if v.iter().all(|&b| b == 0) {
                    self.durable.remove(&n);
                } else {
                    self.durable.insert(n, v);
                }
            }
        }
    }
}

/// A disk as the engine has it: each request in flight until the fuzzer
/// says it is done.
#[derive(Clone)]
pub(crate) struct AHandle(Rc<RefCell<ADisk>>);

/// A request's future: asked at its first poll, ready once done.
struct Ask<'a> {
    disk: &'a AHandle,
    req: Option<Req>,
    id: Option<u64>,
    into: Option<&'a mut [u8]>,
}

impl Future for Ask<'_> {
    type Output = Result<(), IoError>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let mut d = this.disk.0.borrow_mut();
        let id = match this.id {
            Some(id) => id,
            None => {
                let mut req = this.req.take().expect("a request asked once");
                if let Req::Flush { upto } = &mut req {
                    *upto = d.next_seq;
                }
                let id = d.next_id;
                d.next_id += 1;
                d.flying.insert(id, Flying { req, done: None });
                this.id = Some(id);
                return Poll::Pending;
            }
        };
        match d.flying.get(&id) {
            /* Gone with a cut: the request failed */
            None => Poll::Ready(Err(IoError)),
            Some(f) if f.done.is_none() => Poll::Pending,
            Some(_) => {
                let f = d.flying.remove(&id).expect("a request done");
                if let (Some(into), Some(data)) = (this.into.take(), f.done) {
                    into.copy_from_slice(&data);
                }
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl Device for AHandle {
    fn bytes(&self) -> u64 {
        self.0.borrow().bytes
    }

    fn sector(&self) -> u64 {
        self.0.borrow().sector
    }

    fn read<'a>(&'a self, offset: u64, buf: &'a mut [u8]) -> impl Future<Output = Result<(), IoError>> + 'a {
        let len = buf.len();
        Ask { disk: self, req: Some(Req::Read { at: offset, len }), id: None, into: Some(buf) }
    }

    fn write<'a>(&'a self, offset: u64, data: &'a [u8]) -> impl Future<Output = Result<(), IoError>> + 'a {
        Ask { disk: self, req: Some(Req::Write { at: offset, data: data.to_vec() }), id: None, into: None }
    }

    fn flush(&self) -> impl Future<Output = Result<(), IoError>> + '_ {
        Ask { disk: self, req: Some(Req::Flush { upto: 0 }), id: None, into: None }
    }
}

/* ---- the run ---- */

/// What a request was.
#[derive(Clone, Copy, Debug)]
enum What {
    Write { volume: u32, generation: u64, chunk: u32, k: u32, value: u8 },
    Read { volume: u32, generation: u64, chunk: u32, k: u32 },
    Close { volume: u32, generation: u64, chunk: u32 },
    Flush { volume: u32, generation: u64 },
    Fence { volume: u32, generation: u64 },
}

/// A request's answer.
#[derive(Debug)]
enum Said {
    Unit(Result<(), Error>),
    Wrote(Result<u64, Error>),
    Read(Result<u8, Error>),
    Closed(Result<bool, Error>),
    Fenced(Result<u64, Error>),
}

type Fut = Pin<Box<dyn Future<Output = Said>>>;

struct Out {
    what: What,
    /// When it was asked.
    asked: u64,
    fut: Fut,
}

/// A sector, as the fuzzer has it.
#[derive(Clone, Default)]
struct MSec {
    /// What the cs holds now, where known: the last write of it answered.
    now: Option<u8>,
    /// The writes of it answered: each when asked, when answered, and its
    /// value.
    answered: Vec<(u64, u64, u8)>,
    /// The value a cut must leave at the least: as of the last flush or
    /// close answered after a write; and when that write was asked.
    durable: u8,
    durable_asked: u64,
    /// Every value asked since that a cut may leave, with when asked.
    since: Vec<(u64, u8)>,
}

/// A chunk, as the fuzzer has it.
#[derive(Clone, Default)]
struct MChunk {
    sectors: Vec<MSec>,
    /// The last write of it answered, when; the last close asked, when; and
    /// the last close answered as closing it, when asked and answered.
    written: Option<u64>,
    close_asked: Option<u64>,
    closed: Option<(u64, u64)>,
}

struct Run {
    r: Rng,
    disks: Vec<AHandle>,
    identities: Vec<Identity>,
    store: Rc<Store<AHandle>>,
    out: Vec<Out>,
    clock: u64,
    /// Each volume's newest generation fenced: asked, and answered.
    generation: [u64; VOLUMES as usize + 1],
    fenced: [u64; VOLUMES as usize + 1],
    chunks: Vec<MChunk>,
    stats: BTreeMap<&'static str, u64>,
    trace: bool,
}

fn volume_of(chunk: u32) -> u32 {
    1 + chunk % VOLUMES
}

fn cref(chunk: u32) -> ChunkRef {
    ChunkRef { chunk, incarnation: 1, epoch: Epoch { set: 1 + chunk, version: 1 } }
}

/// `f` driven to its end over disks whose every request is done as it is
/// asked: an open's, a format's.
fn drive<T>(f: impl Future<Output = T>, disks: &[AHandle]) -> T {
    let mut f = Box::pin(f);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..1_000_000 {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        for d in disks {
            let ids = d.0.borrow().waiting();
            for id in ids {
                d.0.borrow_mut().complete(id);
            }
        }
    }
    panic!("an operation driven alone never ended");
}

impl Run {
    fn count(&mut self, what: &'static str) {
        *self.stats.entry(what).or_default() += 1;
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// A request asked of the store, its future kept.
    fn ask(&mut self, what: What) {
        let st = self.store.clone();
        let fut: Fut = match what {
            What::Write { volume, generation, chunk, k, value } => {
                let s = self.disks[0].0.borrow().sector as usize;
                let data = vec![value; s];
                Box::pin(async move {
                    Said::Wrote(st.write_marked(volume, generation, cref(chunk), k * s as u32, &data, true).await)
                })
            }
            What::Read { volume, generation, chunk, k } => {
                let s = self.disks[0].0.borrow().sector as usize;
                Box::pin(async move {
                    let mut buf = vec![0u8; s];
                    let got = st.read(volume, generation, cref(chunk), k * s as u32, &mut buf, true).await;
                    Said::Read(got.and_then(|()| {
                        /* A sector of one value, as every write writes it */
                        if buf.iter().all(|&b| b == buf[0]) { Ok(buf[0]) } else { Err(Error::Bad) }
                    }))
                })
            }
            What::Close { volume, generation, chunk } => {
                Box::pin(async move { Said::Closed(st.close(volume, generation, cref(chunk)).await) })
            }
            What::Flush { volume, generation } => {
                Box::pin(async move { Said::Unit(st.flush_for(volume, generation).await) })
            }
            What::Fence { volume, generation } => Box::pin(async move { Said::Fenced(st.fence(volume, generation).await) }),
        };
        let asked = self.tick();
        if let What::Write { chunk, k, value, .. } = what {
            self.chunks[chunk as usize].sectors[k as usize].since.push((asked, value));
        }
        if let What::Close { chunk, .. } = what {
            self.chunks[chunk as usize].close_asked = Some(asked);
        }
        if self.trace {
            eprintln!("[{}] ask {:?}", asked, what);
        }
        self.out.push(Out { what, asked, fut });
        let n = self.out.len() as u64;
        let peak = self.stats.entry("peak out").or_default();
        *peak = (*peak).max(n);
    }

    /// A join of a chunk's set begun, or the one on ended (N13): what a
    /// write's count of the joins begun as it was marked is held to.
    fn join_turn(&mut self) {
        let set = cref(self.r.below(u64::from(CHUNKS)) as u32).epoch.set;
        self.store.leave(set);
        if self.r.chance(70) {
            self.store.join(set, 1);
            self.count("joins begun");
        }
    }

    /// Every request in flight polled, in the order asked: those that end
    /// held to the model.
    fn poll(&mut self) -> Result<(), String> {
        let mut cx = Context::from_waker(Waker::noop());
        let mut k = 0;
        while k < self.out.len() {
            match self.out[k].fut.as_mut().poll(&mut cx) {
                Poll::Pending => k += 1,
                Poll::Ready(said) => {
                    let o = self.out.remove(k);
                    self.answered(o.what, o.asked, said)?;
                }
            }
        }
        Ok(())
    }

    fn answered(&mut self, what: What, asked: u64, said: Said) -> Result<(), String> {
        let at = self.tick();
        if self.trace {
            eprintln!("[{}] {:?} asked at {}: {:?}", at, what, asked, said);
        }
        match (what, said) {
            (What::Write { volume, generation, chunk, k, value }, Said::Wrote(got)) => {
                let fenced = self.fenced[volume as usize];
                match got {
                    Ok(joins) => {
                        if generation < fenced {
                            return Err(format!("a write of volume {} at {} answered done after its fence at {} was \
                                                answered", volume, generation, fenced));
                        }
                        /* Counted as it was marked: a join on now and none
                           begun since -- the one on then -- marked it */
                        let (cr, sector) = (cref(chunk), self.disks[0].0.borrow().sector as u32);
                        if joins == self.store.joins_begun(cr.epoch.set) && self.store.joins(cr.epoch.set).is_some()
                            && !self.store.marked(&cr, k * sector, sector)
                        {
                            return Err(format!("a write of chunk {}'s sector {} counted {} joins of its set begun as \
                                                it was marked, as many as now, and the join on now has not marked it",
                                               chunk, k, joins));
                        }
                        self.count("writes");
                        let c = &mut self.chunks[chunk as usize];
                        c.written = Some(asked);
                        let s = &mut c.sectors[k as usize];
                        s.now = Some(value);
                        s.answered.push((asked, at, value));
                    }
                    Err(Error::Fenced { .. }) if generation < self.generation[volume as usize] => {
                        self.count("writes fenced")
                    }
                    Err(e) => return Err(format!("write {:?}: {:?}", what, e)),
                }
            }
            (What::Read { volume, generation, chunk, k }, Said::Read(got)) => match got {
                Ok(v) => {
                    self.count("reads");
                    let s = &self.chunks[chunk as usize].sectors[k as usize];
                    if let Some(want) = s.now {
                        if v != want {
                            return Err(format!("chunk {} sector {} read {} asked at {}, its last write {}", chunk, k, v,
                                               asked, want));
                        }
                    } else if v != s.durable && !s.since.iter().any(|&(_, x)| x == v) {
                        return Err(format!("chunk {} sector {} read {}, never written there", chunk, k, v));
                    }
                }
                Err(Error::Fenced { .. }) if generation < self.generation[volume as usize] => self.count("reads fenced"),
                Err(e) => return Err(format!("read {:?}: {:?}", what, e)),
            },
            (What::Close { volume, generation, chunk }, Said::Closed(got)) => match got {
                Ok(true) => {
                    self.count("closes");
                    let c = &mut self.chunks[chunk as usize];
                    c.closed = Some((asked, at));
                    /* Every write of it asked before the close done before it,
                       in its turn: what it holds now durable */
                    for s in c.sectors.iter_mut() {
                        if let Some(&(wasked, _, v)) = s.answered.iter().filter(|(a, _, _)| *a < asked).last() {
                            if wasked > s.durable_asked {
                                s.durable = v;
                                s.durable_asked = wasked;
                            }
                        }
                    }
                }
                Ok(false) => self.count("closes of nothing"),
                Err(Error::Fenced { .. }) if generation < self.generation[volume as usize] => self.count("closes fenced"),
                Err(e) => return Err(format!("close {:?}: {:?}", what, e)),
            },
            (What::Flush { volume, generation }, Said::Unit(got)) => match got {
                Ok(()) => {
                    self.count("flushes");
                    /* Every write answered before it was asked durable -- not
                       one asked before it and answered after: those run with
                       it, a volume's requests together */
                    for (n, c) in self.chunks.iter_mut().enumerate() {
                        if volume_of(n as u32) != volume {
                            continue;
                        }
                        for s in c.sectors.iter_mut() {
                            if let Some(&(wasked, _, v)) = s.answered.iter().filter(|(_, w, _)| *w < asked).last() {
                                if wasked > s.durable_asked {
                                    s.durable = v;
                                    s.durable_asked = wasked;
                                }
                            }
                        }
                    }
                }
                Err(Error::Fenced { .. }) if generation < self.generation[volume as usize] => {
                    self.count("flushes fenced")
                }
                Err(e) => return Err(format!("flush {:?}: {:?}", what, e)),
            },
            (What::Fence { volume, generation }, Said::Fenced(got)) => match got {
                Ok(g) if g >= generation => {
                    self.count("fences");
                    let f = &mut self.fenced[volume as usize];
                    *f = (*f).max(g);
                }
                Err(Error::Fenced { .. }) => self.count("fences refused"),
                other => return Err(format!("fence {:?}: {:?}", what, other)),
            },
            (w, s) => return Err(format!("{:?} answered {:?}", w, s)),
        }
        Ok(())
    }

    /// Some requests the disks hold done, in any order.
    fn disks_turn(&mut self) {
        for d in self.disks.clone() {
            let mut ids = d.0.borrow().waiting();
            let n = self.r.below(ids.len() as u64 + 1) as usize;
            for _ in 0..n {
                let i = self.r.below(ids.len() as u64) as usize;
                let id = ids.swap_remove(i);
                d.0.borrow_mut().complete(id);
            }
        }
    }

    fn new_request(&mut self) {
        let chunk = self.r.below(u64::from(CHUNKS)) as u32;
        let volume = volume_of(chunk);
        let gen_now = self.generation[volume as usize];
        /* Now and then a writer a newer fence has taken the volume from */
        let generation = if gen_now > 1 && self.r.chance(8) { gen_now - 1 } else { gen_now };
        let k = self.r.below(u64::from(SECTORS)) as u32;
        let what = match self.r.below(100) {
            0..=44 => What::Write { volume, generation, chunk, k, value: 1 + self.r.below(255) as u8 },
            45..=69 => What::Read { volume, generation, chunk, k },
            70..=82 => What::Close { volume, generation, chunk },
            83..=96 => What::Flush { volume, generation },
            _ => {
                let g = gen_now + 1;
                self.generation[volume as usize] = g;
                What::Fence { volume, generation: g }
            }
        };
        self.ask(what);
    }

    /// The power cut: every request out gone, the cs opened again over what
    /// the disks kept, and held to what was answered.
    fn cut(&mut self) -> Result<(), String> {
        self.count("cuts");
        if self.disks.iter().any(|d| {
            let d = d.0.borrow();
            !d.cache.is_empty() || d.flying.values().any(|f| matches!(f.req, Req::Write { .. }) && f.done.is_none())
        }) {
            self.count("cuts with writes out");
        }
        self.out.clear();
        for d in self.disks.clone() {
            let mut r = Rng::new(self.r.next());
            d.0.borrow_mut().cut(&mut r);
        }
        let store = Store::new();
        for (i, d) in self.disks.clone().iter().enumerate() {
            match drive(store.open_device(d.clone(), self.identities[i]), &self.disks) {
                Ok((_, rep)) if rep.lost == 0 => {}
                Ok((_, rep)) => return Err(format!("disk {} opened with {} records lost", i, rep.lost)),
                Err((_, e)) => return Err(format!("disk {} not opened again: {:?}", i, e)),
            }
        }
        self.store = Rc::new(store);
        for v in 1..=VOLUMES {
            let f = self.store.fence_of(v);
            if f < self.fenced[v as usize] {
                return Err(format!("volume {} fenced at {} after the cut, answered at {}", v, f,
                                   self.fenced[v as usize]));
            }
            /* What was asked may have been done: the cs's own now */
            self.fenced[v as usize] = f;
            self.generation[v as usize] = f;
        }
        for n in 0..CHUNKS {
            self.chunk_after_cut(n)?;
        }
        Ok(())
    }

    fn chunk_after_cut(&mut self, n: u32) -> Result<(), String> {
        let held = self.store.holding(n);
        let c = self.chunks[n as usize].clone();
        /* Written, and no close asked after: open */
        if let (Some(w), Some(h)) = (c.written, held) {
            if c.close_asked.is_none_or(|a| a < w) && h.state != SlotState::Open {
                return Err(format!("chunk {} written at {} and not closed since: {:?} after the cut", n, w, h.state));
            }
        }
        if c.written.is_some() && held.is_none() {
            return Err(format!("chunk {} written at {:?}, not held after the cut", n, c.written));
        }
        let volume = volume_of(n);
        let generation = self.generation[volume as usize];
        let s = self.disks[0].0.borrow().sector as usize;
        for k in 0..SECTORS {
            let mut buf = vec![0u8; s];
            let st = self.store.clone();
            drive(st.read(volume, generation, cref(n), k * s as u32, &mut buf, true), &self.disks)
                .map_err(|e| format!("chunk {} sector {} not read after the cut: {:?}", n, k, e))?;
            let v = buf[0];
            if buf.iter().any(|&b| b != v) {
                return Err(format!("chunk {} sector {} torn inside after the cut", n, k));
            }
            let sec = &c.sectors[k as usize];
            /* Closed, and nothing written since its close was asked: what
               it held then */
            let closed_clean = c.closed.is_some_and(|(a, _)| sec.since.iter().all(|&(w, _)| w < a));
            let ok = if closed_clean {
                v == sec.durable
            } else {
                v == sec.durable || sec.since.iter().any(|&(w, x)| w > sec.durable_asked && x == v)
            };
            if !ok {
                return Err(format!("chunk {} sector {} reads {} after the cut: durable {} (asked at {}), since {:?}, \
                                    closed {:?}", n, k, v, sec.durable, sec.durable_asked, sec.since, c.closed));
            }
            let m = &mut self.chunks[n as usize].sectors[k as usize];
            *m = MSec { now: Some(v), answered: Vec::new(), durable: v, durable_asked: 0, since: Vec::new() };
        }
        let m = &mut self.chunks[n as usize];
        m.written = None;
        m.close_asked = None;
        m.closed = None;
        Ok(())
    }

    /// Requests the disks were ever asked, all of them together.
    fn disk_asks(&self) -> u64 {
        self.disks.iter().map(|d| d.0.borrow().next_id).sum()
    }

    /// Nothing more asked: every request done, nothing left held -- a poll
    /// that ends no request and asks the disks nothing, with nothing on the
    /// disks to be done, is a wait for ever.
    fn calm(&mut self) -> Result<(), String> {
        for _ in 0..100_000 {
            let before = (self.out.len(), self.disk_asks());
            self.poll()?;
            if self.out.is_empty() {
                break;
            }
            let waiting: usize = self.disks.iter().map(|d| d.0.borrow().waiting().len()).sum();
            if waiting == 0 {
                /* A wait let go in one poll is seen by the waiter at the next:
                   a chain of them takes as many polls as it is long */
                let mut moved = (self.out.len(), self.disk_asks()) != before;
                for _ in 0..self.out.len() + 2 {
                    if moved {
                        break;
                    }
                    self.poll()?;
                    moved = (self.out.len(), self.disk_asks()) != before;
                }
                if !moved {
                    return Err(format!("{} requests wait on nothing the disks hold: {:?}; the store: {}",
                                       self.out.len(), self.out.iter().map(|o| (o.asked, o.what)).collect::<Vec<_>>(),
                                       self.store.held()));
                }
                continue;
            }
            self.disks_turn();
        }
        if !self.out.is_empty() {
            return Err(format!("{} requests never answered", self.out.len()));
        }
        if self.store.busy() != 0 {
            return Err(format!("every request answered, and {} turns or rounds still held", self.store.busy()));
        }
        Ok(())
    }
}

pub fn run(seed: u64, steps: u64, trace: bool) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x696e_666c));
    let sector = if r.chance(30) { PAGE } else { 512 };
    let disks = 1 + r.below(2) as usize;
    let mut handles = Vec::new();
    let mut identities = Vec::new();
    for i in 0..disks {
        let slots = 10 + r.below(10);
        handles.push(AHandle(Rc::new(RefCell::new(ADisk::new(sector, slots * SLOT)))));
        identities.push(Identity { cluster: [5; 16], cs: 2, device: 20 + i as u32 });
    }
    let store = Store::new();
    for (i, d) in handles.iter().enumerate() {
        if let Err((_, e)) = drive(store.format_device(d.clone(), identities[i], 1 + r.below(u64::MAX - 1)), &handles) {
            return Err(format!("disk {} not formatted: {:?}", i, e));
        }
    }
    for v in 1..=VOLUMES {
        drive(store.fence(v, 1), &handles).map_err(|e| format!("volume {} not fenced: {:?}", v, e))?;
    }
    let chunks = (0..CHUNKS).map(|_| MChunk { sectors: vec![MSec { now: Some(0), ..MSec::default() }; SECTORS as usize],
                                             ..MChunk::default() }).collect();
    let mut run = Run { r, disks: handles, identities, store: Rc::new(store), out: Vec::new(), clock: 0,
                        generation: [1; VOLUMES as usize + 1], fenced: [1; VOLUMES as usize + 1], chunks,
                        stats: BTreeMap::new(), trace };
    /* Bursts: requests asked faster than the disks do them, then let be */
    let busy = 1 + run.r.below(6);
    for step in 0..steps {
        if run.r.below(10) < busy {
            for _ in 0..1 + run.r.below(3) {
                run.new_request();
            }
        }
        run.poll()?;
        run.disks_turn();
        if run.r.chance(3) {
            run.join_turn();
        }
        run.poll()?;
        if run.r.chance(2) {
            run.cut().map_err(|e| format!("step {}: {}", step, e))?;
        }
    }
    run.calm()?;
    run.cut()?;
    if std::env::var_os("NBS_INFLIGHT_STATS").is_some() {
        eprintln!("inflight seed {}: sector {}, {} disks: {:?}", seed, sector, disks, run.stats);
    }
    Ok(())
}
