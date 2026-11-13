//! A joiner's copy of a set (plans/nbs.md, "How three are kept"; N9): the
//! one copy nbs-cs.ko runs and fuzz/nbs's cluster target's cs's do. It has
//! no I/O of its own: what it sends is handed back, what is answered handed
//! in, and the store it copies into is lent to each call -- `step` and
//! `answer` are `async`, the store's operations awaited among the requests
//! the cs serves meanwhile (N12c); the copy itself is its driver's, one
//! call at a time.
//!
//! - **The set's chunks** a source holds, a page at a time
//!   (`Op::Holdings`), the source an active member of the set.
//! - **Each chunk pulled whole** (`Op::Pull`) into the store
//!   (`Store::copy_in`), where no client has written since the join began.
//!   A member back pulls whole only what it may have missed: a chunk the
//!   source last wrote at or after the version the member went down at, or
//!   one the member does not hold. The others -- one it found open when its
//!   devices were taken in among them, its last writes lost -- it holds to
//!   the source's by sums all the same, copying only what differs: a sum
//!   finds what a power cut lost as it finds a write it missed. Taken out,
//!   it kept each chunk at the epoch
//!   it last knew, and a writer since taken over, its view as old, could
//!   write it there -- the members fenced against that writer refuse it,
//!   the member out was not fenced (found by fuzz/nbs's cluster target,
//!   N9d). The sums move the chunk's epoch on at the source, so that no
//!   writer of the map before the join writes it past them -- and closing
//!   it moves it on here (`Store::close_copy`), so that none writes it here
//!   once the join is done (N13).
//! - **A chunk pulled is held to the source's** by sums (`Op::Sums`). A
//!   granule whose sums differ is pulled again and held to the source's a
//!   sector at a time, the sectors a client has written since the join
//!   began left out -- a sum alone cannot leave them out, and a granule
//!   with one of them would go unchecked. A sector that differs is copied
//!   in and read back; one that does not read back as copied is a round,
//!   and past `ROUNDS_MAX` rounds the join is given up, and said. A chunk
//!   held to its source's is closed (`Store::close_copy`).
//! - **What the source has not**: every chunk done, what this cs still
//!   holds of the set at an epoch before its join -- a chunk the source
//!   listed none of, which reads as zeros on the set's members -- dropped
//!   (`Store::drop_stale`). A member back kept a chunk a writer cut off with
//!   it had written there alone, and, made active, read it back where the
//!   others read zeros (found by the review of 2026-11).
//! - **Its end**: then `State::Done`, and the driver asks the joiner's mds
//!   to end the join.
//!
//! A refusal that says the set moved on, or that the join is not the
//! joiner's, stops it until its view is refreshed (`refresh`). A source
//! that fails, or is silent, is left for another of the set's active
//! members.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use nbscs::{ChunkRef, Device, Epoch, SlotState, Store, GRANULE, SLOT};

use crate::{JoinView, Op, Refusal, Reply, Request, DATA_MAX, PAGE};

/// A pull's bytes.
const PULL: u32 = DATA_MAX;
/// Pulls in flight at once.
pub const WINDOW: usize = 4;
/// How long a request is waited for before everything in flight is sent
/// again, and how many such waits in a row leave a source for another.
pub const ANSWER_NS: u64 = 2_000_000_000;
const SILENT_MAX: u32 = 3;
/// How long a refusal to be asked again later holds the copy back.
pub const AGAIN_NS: u64 = 200_000_000;
/// A copy that has had no answer it could use for this long is given up.
pub const STUCK_NS: u64 = 5 * 60 * 1_000_000_000;
/// The rounds of pulling again a chunk's granules that differ, at most.
pub const ROUNDS_MAX: u32 = 3;
/// A chunk's bytes, as a request's length; a sector of the smallest kind,
/// what a granule is held to its source's by.
const CHUNK: u32 = SLOT as u32;
const SECTOR: usize = nbscs::SECTOR as usize;

/// What the copy asks its driver to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    /// Request `bytes` sent to the cs on host `host`.
    Send { host: String, bytes: Vec<u8> },
    /// A line said.
    Say(String),
}

/// Where the copy is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Copying,
    /// The set moved on, or the join may be over, or every source failed:
    /// nothing sent until its view is refreshed.
    Stale,
    /// Every chunk copied and held to its source's: the join's end to be
    /// asked.
    Done,
    /// Given up, and why.
    Failed(String),
}

/// What it has done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Chunks copied and held to their source's; of them, chunks a member
    /// back held that its sums found the source's.
    pub chunks: u64,
    pub kept: u64,
    pub bytes: u64,
    /// Granules pulled again for differing from the source's.
    pub again: u64,
    /// Chunks this cs held of the set from before the join that the source
    /// had not: dropped.
    pub dropped: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    List,
    Pull { offset: u32, len: u32, version: u32 },
    /// A granule held to the source's a sector at a time.
    Verify { offset: u32, len: u32, version: u32, rounds: u32 },
    Sums,
}

#[derive(Clone, Copy)]
struct Sent {
    ask: Ask,
    at: u64,
}

/// The chunk being copied.
struct Work {
    chunk: u32,
    incarnation: u32,
    volume: u32,
    /// Pulled up to `next`; ranges to pull again; granules to hold to the
    /// source's, each with its rounds so far.
    next: u32,
    again: Vec<(u32, u32)>,
    verify: Vec<(u32, u32, u32)>,
    /// A chunk a member back holds: not pulled, held to the source's by
    /// its sums.
    held: bool,
    /// Its sums compared: the granules that differed held to the source's
    /// a sector at a time, and then it is closed.
    summed: bool,
    /// Its sums asked.
    checking: bool,
}

/// One join's copy.
pub struct Copy {
    view: JoinView,
    me: u32,
    source: usize,
    silent: u32,
    from: u32,
    listed: bool,
    page: VecDeque<(u32, u32, u32, u32)>,
    work: Option<Work>,
    sent: BTreeMap<u64, Sent>,
    hold_until: u64,
    progress_at: u64,
    state: State,
    pub stats: Stats,
}

/// The sectors of this cs's copy of chunk `c` at `offset` that differ from
/// `theirs`, those a client has written since the join began left out: read
/// into `mine`.
async fn differing<D: Device>(store: &Store<D>, c: ChunkRef, offset: u32, mine: &mut [u8], theirs: &[u8])
    -> Result<u64, nbscs::Error> {
    store.own_read(c, offset, mine).await?;
    Ok(mine.chunks(SECTOR).zip(theirs.chunks(SECTOR)).enumerate()
        .filter(|(k, (a, b))| a != b && !store.marked(&c, offset + (k * SECTOR) as u32, SECTOR as u32))
        .count() as u64)
}

/// The joiner fenced at every volume of join `view`'s set at its newest
/// writer's generation, durable, before its join's marks begin and again as
/// its view is refreshed (N13): false where one could not be had -- no room
/// in the fence table, a device failed -- the join not begun then, looked
/// at again next time. A writer taken over while this cs was out holds its
/// fence still; its view as new as the join, its writes would be taken
/// here alone -- unasked, at the epoch the copy moved the chunks on to --
/// the members its successor fenced refusing them.
pub async fn fence_for<D: Device>(store: &Store<D>, view: &JoinView) -> bool {
    for &(volume, generation) in &view.fences {
        if store.fence_of(volume) >= generation {
            continue;
        }
        if store.fence(volume, generation).await.is_err() {
            return false;
        }
    }
    true
}

impl Copy {
    /// The copy of join `view` by cs `me`, begun at `now`. The driver has
    /// fenced the store (`fence_for`) and had it mark the set's writes
    /// first (`Store::join`).
    pub fn new(view: JoinView, me: u32, now: u64) -> Copy {
        Copy { view, me, source: 0, silent: 0, from: 0, listed: false,
               page: VecDeque::new(), work: None, sent: BTreeMap::new(), hold_until: 0, progress_at: now,
               state: State::Copying, stats: Stats::default() }
    }

    pub fn set(&self) -> u32 {
        self.view.set
    }

    pub fn since(&self) -> u32 {
        self.view.since
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Whether request `id` is one of this copy's in flight.
    pub fn sent(&self, id: u64) -> bool {
        self.sent.contains_key(&id)
    }

    /// The host of the source asked now: whose answers alone are this
    /// copy's.
    pub fn source_host(&self) -> Option<&str> {
        self.view.sources.get(self.source).map(|(_, h)| h.as_str())
    }

    /// Whether the set's chunks have all been listed.
    pub fn listed(&self) -> bool {
        self.listed
    }

    /// The chunk being copied, if one is.
    pub fn working(&self) -> Option<u32> {
        self.work.as_ref().map(|w| w.chunk)
    }

    /// The chunk whose sums are asked, if one is: pulled whole, its check to
    /// come.
    pub fn summing(&self) -> Option<u32> {
        self.work.as_ref().filter(|w| w.checking && !w.held && !w.summed).map(|w| w.chunk)
    }

    /// Its view refreshed -- the same join, the set's version now and its
    /// active members now: what was in flight is asked again at the new
    /// version.
    pub fn refresh(&mut self, view: JoinView) {
        self.view = view;
        self.requeue();
        if self.source >= self.view.sources.len() {
            self.source = 0;
        }
        if self.state == State::Stale && !self.view.sources.is_empty() {
            self.state = State::Copying;
        }
    }

    /// What it asks now: each turn, and after an answer.
    pub async fn step<D: Device>(&mut self, store: &Store<D>, now: u64, next_id: &mut u64, out: &mut Vec<Out>) {
        if !matches!(self.state, State::Copying | State::Stale) {
            return;
        }
        if now >= self.progress_at + STUCK_NS {
            self.fail("no answer it could use in five minutes".into(), out);
            return;
        }
        if self.state != State::Copying {
            return;
        }
        /* What in flight has not been answered in time: everything sent
           again, and a source silent too often left for another */
        if self.sent.values().any(|s| now >= s.at + ANSWER_NS) {
            self.silent += 1;
            self.requeue();
            if self.silent >= SILENT_MAX {
                self.next_source();
            }
        }
        if now < self.hold_until || self.state != State::Copying {
            return;
        }
        if self.work.is_none() && !self.next_chunk(store) {
            if !self.listed {
                if !self.sent.values().any(|s| s.ask == Ask::List) {
                    let op = Op::Holdings { set: self.view.set, from: self.from, joiner: self.me,
                                            version: self.view.version, index: self.view.index };
                    self.send(op, Ask::List, now, next_id, out);
                }
            } else if self.sent.is_empty() && self.sweep(store, out).await {
                self.state = State::Done;
            }
            return;
        }
        /* The chunk's pulls, up to the window: granules to hold to the
           source's first */
        loop {
            let pulls = self.sent.values().filter(|s| matches!(s.ask, Ask::Pull { .. } | Ask::Verify { .. })).count();
            let Some(w) = self.work.as_mut() else { return };
            if pulls >= WINDOW {
                break;
            }
            let version = self.view.version;
            let (offset, len, ask) = if let Some((offset, len, rounds)) = w.verify.pop() {
                (offset, len, Ask::Verify { offset, len, version, rounds })
            } else if let Some((offset, len)) = w.again.pop() {
                (offset, len, Ask::Pull { offset, len, version })
            } else if w.next < CHUNK {
                let (offset, len) = (w.next, PULL.min(CHUNK - w.next));
                w.next += len;
                (offset, len, Ask::Pull { offset, len, version })
            } else {
                break;
            };
            let op = Op::Pull { chunk: self.chunk_ref(), joiner: self.me, index: self.view.index, offset, len };
            self.send(op, ask, now, next_id, out);
        }
        /* Every range pulled -- none, for a chunk held -- its sums asked;
           or, its sums compared and what differed held to the source's, it
           is done */
        let idle = self.sent.is_empty();
        let Some(w) = self.work.as_mut() else { return };
        if idle && w.again.is_empty() && w.verify.is_empty() && w.next >= CHUNK && w.summed {
            self.close(store, out).await;
            return;
        }
        if idle && w.again.is_empty() && w.verify.is_empty() && w.next >= CHUNK && !w.checking {
            w.checking = true;
            let (chunk, joiner, index) = (self.chunk_ref(), self.me, self.view.index);
            self.send(Op::Sums { chunk, joiner, index, offset: 0, len: CHUNK }, Ask::Sums, now, next_id, out);
        }
    }

    /// Request `id`'s answer, `reply`.
    pub async fn answer<D: Device>(&mut self, store: &Store<D>, id: u64, reply: &Reply<'_>, now: u64,
                                   out: &mut Vec<Out>) {
        /* A request sent again since, or of a source left: dropped */
        let Some(sent) = self.sent.remove(&id) else { return };
        if !matches!(self.state, State::Copying | State::Stale) {
            return;
        }
        self.silent = 0;
        if let Reply::Refused(why) = reply {
            self.refused(sent.ask, *why, now, out);
            return;
        }
        match (sent.ask, reply) {
            (Ask::List, Reply::Chunks(list)) => {
                self.progress_at = now;
                self.page.extend(list.iter().copied());
                match list.last() {
                    Some(&(c, ..)) if list.len() >= PAGE && c < u32::MAX => self.from = c + 1,
                    _ => self.listed = true,
                }
            }
            (Ask::Pull { offset, len, version }, Reply::Data(d)) if d.len() == len as usize => {
                let Some(w) = self.work.as_ref() else { return };
                let c = ChunkRef { chunk: w.chunk, incarnation: w.incarnation,
                                   epoch: Epoch { set: self.view.set, version } };
                match store.copy_in(w.volume, c, offset, d).await {
                    Ok(()) => {
                        self.progress_at = now;
                        self.stats.bytes += u64::from(len);
                    }
                    /* A client wrote the chunk here at a newer version: the
                       range pulled again once the view has it */
                    Err(nbscs::Error::Epoch { .. }) => {
                        self.again(offset, len);
                        self.state = State::Stale;
                    }
                    Err(nbscs::Error::NoMemory) => {
                        self.again(offset, len);
                        self.hold_until = now + AGAIN_NS;
                    }
                    Err(e) => self.fail(alloc::format!("chunk {} not copied in: {:?}", c.chunk, e), out),
                }
            }
            (Ask::Verify { offset, len, version, rounds }, Reply::Data(d)) if d.len() == len as usize => {
                self.progress_at = now;
                self.verify(store, offset, d, version, rounds, out).await;
            }
            (Ask::Sums, Reply::Sums(theirs)) => {
                self.progress_at = now;
                self.check(store, theirs, out).await;
            }
            /* An answer that is not the request's: the source left */
            _ => {
                self.requeue_one(sent.ask);
                out.push(Out::Say(alloc::format!("nbs-cs: set {}'s source answered a {:?} with another answer",
                                                 self.view.set, sent.ask)));
                self.next_source();
            }
        }
    }

    /* ---- its parts ---- */

    /// The next chunk of the page taken up: whether there was one.
    fn next_chunk<D: Device>(&mut self, store: &Store<D>) -> bool {
        let Some((chunk, incarnation, volume, written)) = self.page.pop_front() else { return false };
        let held = store.holding(chunk)
            .is_some_and(|h| h.incarnation == incarnation && h.volume == volume && h.state != SlotState::Lost);
        let held = self.view.own && written < self.view.until && held;
        self.work = Some(Work { chunk, incarnation, volume, next: if held { CHUNK } else { 0 }, again: Vec::new(),
                                verify: Vec::new(), held, summed: false, checking: false });
        true
    }

    fn chunk_ref(&self) -> ChunkRef {
        let (chunk, incarnation) = self.work.as_ref().map_or((0, 0), |w| (w.chunk, w.incarnation));
        ChunkRef { chunk, incarnation, epoch: Epoch { set: self.view.set, version: self.view.version } }
    }

    /// The chunk pulled held to the source's sums `theirs`: closed where
    /// every granule no client has written is the same, those that are
    /// not pulled again.
    async fn check<D: Device>(&mut self, store: &Store<D>, theirs: &[u32], out: &mut Vec<Out>) {
        let c = self.chunk_ref();
        let mine = match store.own_sums(c, 0, CHUNK).await {
            Ok(m) => m,
            Err(e) => return self.fail(alloc::format!("chunk {}'s copy not read back: {:?}", c.chunk, e), out),
        };
        if mine.len() != theirs.len() {
            return self.fail(alloc::format!("chunk {}: {} sums from the source, {} here", c.chunk, theirs.len(),
                                            mine.len()), out);
        }
        let differ: Vec<(u32, u32, u32)> = mine.iter().zip(theirs.iter()).enumerate()
            .filter(|(_, (a, b))| a != b).map(|(k, _)| (k as u32 * GRANULE, GRANULE, 0)).collect();
        let Some(w) = self.work.as_mut() else { return };
        w.summed = true;
        if differ.is_empty() {
            if w.held {
                self.stats.kept += 1;
            }
            self.close(store, out).await;
            return;
        }
        /* Held to the source's a sector at a time, and then closed */
        w.verify = differ;
    }

    /// What this cs holds of the set at an epoch before its join, dropped as
    /// the copy ends: every chunk the copy took it moved on to the join's
    /// epoch, and a client writes the joiner at that epoch or after, so what
    /// is left before it the source listed none of. Whether it was done.
    async fn sweep<D: Device>(&mut self, store: &Store<D>, out: &mut Vec<Out>) -> bool {
        let (set, since) = (self.view.set, self.view.since);
        let mut from = 0u32;
        loop {
            let page = store.stale_in(set, since, from, PAGE);
            let Some(&last) = page.last() else { return true };
            for &c in &page {
                match store.drop_stale(c, set, since).await {
                    Ok(true) => self.stats.dropped += 1,
                    Ok(false) => {}
                    Err(e) => {
                        self.fail(alloc::format!("chunk {}, the source's none, not dropped: {:?}", c, e), out);
                        return false;
                    }
                }
            }
            if page.len() < PAGE || last == u32::MAX {
                return true;
            }
            from = last + 1;
        }
    }

    /// The chunk copied and held to its source's: closed, and the next one.
    async fn close<D: Device>(&mut self, store: &Store<D>, out: &mut Vec<Out>) {
        let c = self.chunk_ref();
        if let Err(e) = store.close_copy(c).await {
            return self.fail(alloc::format!("chunk {}'s copy not closed: {:?}", c.chunk, e), out);
        }
        self.stats.chunks += 1;
        self.work = None;
    }

    /// A granule of the chunk at `offset` held to the source's data `theirs`
    /// a sector at a time, those a client has written since the join began
    /// left out: one that differs copied in and read back. One that does not
    /// read back as copied is a round, asked again; past `ROUNDS_MAX`, the
    /// join is given up.
    async fn verify<D: Device>(&mut self, store: &Store<D>, offset: u32, theirs: &[u8], version: u32, rounds: u32,
                               out: &mut Vec<Out>) {
        let Some(w) = self.work.as_ref() else { return };
        let volume = w.volume;
        let c = ChunkRef { chunk: w.chunk, incarnation: w.incarnation, epoch: Epoch { set: self.view.set, version } };
        let mut mine = Vec::new();
        if mine.try_reserve_exact(theirs.len()).is_err() {
            self.verify_again(offset, theirs.len() as u32, rounds);
            return;
        }
        mine.resize(theirs.len(), 0);
        let before = match differing(store, c, offset, &mut mine, theirs).await {
            Ok(n) => n,
            Err(e) => return self.fail(alloc::format!("chunk {}'s copy not read back: {:?}", c.chunk, e), out),
        };
        if before == 0 {
            return;
        }
        self.stats.again += before;
        match store.copy_in(volume, c, offset, theirs).await {
            Ok(()) => {}
            /* A client wrote the chunk here at a newer version: held again
               once the view has it */
            Err(nbscs::Error::Epoch { .. }) => {
                self.verify_again(offset, theirs.len() as u32, rounds);
                self.state = State::Stale;
                return;
            }
            Err(nbscs::Error::NoMemory) => {
                self.verify_again(offset, theirs.len() as u32, rounds);
                return;
            }
            Err(e) => return self.fail(alloc::format!("chunk {} not copied in again: {:?}", c.chunk, e), out),
        }
        match differing(store, c, offset, &mut mine, theirs).await {
            Ok(0) => {}
            Ok(n) if rounds + 1 >= ROUNDS_MAX => {
                self.fail(alloc::format!("chunk {}: {} sectors at {} do not read back as copied, {} times", c.chunk,
                                         n, offset, ROUNDS_MAX), out);
            }
            Ok(_) => self.verify_again(offset, theirs.len() as u32, rounds + 1),
            Err(e) => self.fail(alloc::format!("chunk {}'s copy not read back: {:?}", c.chunk, e), out),
        }
    }

    fn verify_again(&mut self, offset: u32, len: u32, rounds: u32) {
        if let Some(w) = self.work.as_mut() {
            w.verify.push((offset, len, rounds));
        }
    }

    fn refused(&mut self, ask: Ask, why: Refusal, now: u64, out: &mut Vec<Out>) {
        self.requeue_one(ask);
        match why {
            /* The set moved on, or the join is over: the view refreshed */
            Refusal::Epoch { .. } | Refusal::NotTheJoiner => self.state = State::Stale,
            Refusal::Behind { .. } | Refusal::NotReady | Refusal::NoMds => self.hold_until = now + AGAIN_NS,
            /* The chunk freed, or its number another's now: nothing to copy --
               and its asks still in flight dropped, so that their answers are
               of none: taken for the next chunk's, a late refusal dropped that
               one too, and a late pull's data would be written into it (found
               by fuzz/nbs's cluster target, N10c: the source's garbage pass
               freed a chunk a copy-up had left, under the join) */
            Refusal::NotHeld | Refusal::StaleChunk { .. } if ask != Ask::List => {
                self.progress_at = now;
                self.work = None;
                self.sent.retain(|_, s| s.ask == Ask::List);
            }
            _ => {
                out.push(Out::Say(alloc::format!("nbs-cs: set {}'s source refused the copy: {}", self.view.set,
                                                 why.says())));
                self.next_source();
            }
        }
    }

    fn again(&mut self, offset: u32, len: u32) {
        if let Some(w) = self.work.as_mut() {
            w.again.push((offset, len));
        }
    }

    /// An ask not answered put back, to be asked again.
    fn requeue_one(&mut self, ask: Ask) {
        match ask {
            Ask::Pull { offset, len, .. } => self.again(offset, len),
            Ask::Verify { offset, len, rounds, .. } => self.verify_again(offset, len, rounds),
            Ask::Sums => {
                if let Some(w) = self.work.as_mut() {
                    w.checking = false;
                }
            }
            Ask::List => {}
        }
    }

    /// Everything in flight put back.
    fn requeue(&mut self) {
        for (_, s) in core::mem::take(&mut self.sent) {
            self.requeue_one(s.ask);
        }
    }

    /// The next of the set's active members as the source; past the last,
    /// the view refreshed first.
    fn next_source(&mut self) {
        self.silent = 0;
        self.requeue();
        self.source += 1;
        if self.source >= self.view.sources.len() {
            self.source = 0;
            self.state = State::Stale;
        }
    }

    fn fail(&mut self, why: String, out: &mut Vec<Out>) {
        out.push(Out::Say(alloc::format!("nbs-cs: the join of set {} given up: {}", self.view.set, why)));
        self.sent.clear();
        self.state = State::Failed(why);
    }

    fn send(&mut self, op: Op<'_>, ask: Ask, now: u64, next_id: &mut u64, out: &mut Vec<Out>) {
        let Some((_, host)) = self.view.sources.get(self.source) else {
            self.state = State::Stale;
            return;
        };
        let id = *next_id;
        *next_id += 1;
        let bytes = Request { id, volume: 0, generation: 0, op }.encode();
        self.sent.insert(id, Sent { ask, at: now });
        out.push(Out::Send { host: host.clone(), bytes });
    }
}
