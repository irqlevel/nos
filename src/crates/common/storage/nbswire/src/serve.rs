//! A cs's half of the wire (plans/nbs.md, "The cs"): a client's request
//! done on the cs's store, checked first with the host's mds where the
//! store's engine has not checked it -- a chunk it does not hold yet, an
//! epoch other than the one it knows. The one copy that nbs-cs.ko serves
//! with and fuzz/nbs's cluster target's cs's do; how the mds is asked is
//! the caller's (`Asker`).
//!
//! Every request is an `async fn` over the store's (N12c): many in flight
//! together on the cs's one task, a question to the mds awaited by the
//! request that asked it alone. What they share (`Requests`) -- the checks
//! vouched for, each chunk's order -- is borrowed between two awaits, never
//! across one.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::future::{poll_fn, ready, Future};
use core::task::Poll;

use nbscs::{ChunkRef, Device, Error, SlotState, Store};

use crate::{Op, Question, Refusal, Relay, Reply, Request, Told, LIST_MAX};

/// How a cs asks its host's mds: a question, and what it is told -- None
/// where there is no mds, or it said nothing in time. nbs-cs.ko's sends it
/// and is answered by the mds's message to its id; the fuzzer's answers
/// from its model.
pub trait Asker {
    fn ask(&self, question: Question) -> impl Future<Output = Option<Told>> + '_;
}

/// An mds that answers at once: `f`'s word, for a driver whose model of the
/// mds is a function of the question.
pub struct AskNow<F>(pub RefCell<F>);

impl<F: FnMut(Question) -> Option<Told>> AskNow<F> {
    pub fn new(f: F) -> AskNow<F> {
        AskNow(RefCell::new(f))
    }
}

impl<F: FnMut(Question) -> Option<Told>> Asker for AskNow<F> {
    fn ask(&self, question: Question) -> impl Future<Output = Option<Told>> + '_ {
        let told = (self.0.borrow_mut())(question);
        ready(told)
    }
}

/// Checks the mds vouched for, kept at most: past it, forgotten all at once
/// and asked again.
pub const VOUCHED_MAX: usize = 65536;

/// What a check the mds vouched for was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Check {
    volume: u32,
    generation: u64,
    chunk: u32,
    incarnation: u32,
    set: u32,
    version: u32,
}

/// A joiner the host's mds vouched for (N9): the set, the joiner, the set's
/// version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Join {
    set: u32,
    joiner: u32,
    version: u32,
}

/// What a cs's requests in flight share (N12c): the checks the host's mds
/// vouched for -- a request of one is done without asking again -- and
/// each chunk's order: a chunk's requests one at a time, each from its
/// first poll to its answer, its questions to the mds among it, in the
/// order they came. Without the order a request asked again by its client
/// -- a write sent twice, the first answered -- could end after the
/// client's next request of the chunk, a close or a newer write, had been
/// answered, and undo it: the chunk open again, or older data over newer
/// (found by fuzz/nbs's cluster target once its cs's asked their mds over
/// its network). The engine's own turns order a chunk's requests only
/// inside the store, not across a question to the mds.
#[derive(Default)]
pub struct Requests {
    inner: RefCell<Sets>,
    order: RefCell<BTreeMap<u32, VecDeque<u64>>>,
    next: Cell<u64>,
    released: Cell<u64>,
    relayed: RefCell<BTreeMap<u32, Relayed>>,
}

/// A volume's writer's writes passed along their chains, at this cs (N13):
/// its generation; its mark -- every request of its below it answered --
/// and the ids above the mark applied here, each with its chunk and the
/// joins of its set begun here when it was. A write that comes again --
/// along its chain, and directly once the chain was slow -- is applied once:
/// the copy that comes late may come after the client's next write of the
/// same blocks, and would put the older data back over it (found by
/// fuzz/nbs's cluster target with chains, at once). But not across a join
/// of its set begun since: its sectors not marked by it, the join's copy
/// may have written the source's data over them, and the client, which
/// sends it again because it is not done, is to have it written again --
/// and marked (found by the cluster target: a member taken out and back
/// answered a write the copy had undone as done).
struct Relayed {
    generation: u64,
    mark: u64,
    done: BTreeMap<u64, Applied>,
}

/// A relayed write applied: its chunk, and the joins of its set begun here
/// when it was (`Store::joins_begun`).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Applied {
    chunk: u32,
    incarnation: u32,
    joins: u64,
}

/// The ids a writer's relayed writes keep above its mark, at most: past
/// it, refused for now until the mark moves on.
pub const RELAYED_MAX: usize = 65536;

/// A chunk's order held, or waited for: let go when dropped.
struct InOrder<'a> {
    requests: &'a Requests,
    chunk: u32,
    ticket: u64,
}

impl Drop for InOrder<'_> {
    fn drop(&mut self) {
        let r = self.requests;
        r.released.set(r.released.get().wrapping_add(1));
        let mut order = r.order.borrow_mut();
        if let Some(q) = order.get_mut(&self.chunk) {
            q.retain(|&t| t != self.ticket);
            if q.is_empty() {
                order.remove(&self.chunk);
            }
        }
    }
}

#[derive(Default)]
struct Sets {
    checks: BTreeSet<Check>,
    joins: BTreeSet<Join>,
}

impl Requests {
    pub fn new() -> Requests {
        Requests::default()
    }

    /// Chunk `chunk`'s turn, once every request of it that came before has
    /// ended: taken at the request's first poll, so in the order they came.
    async fn in_order(&self, chunk: u32) -> InOrder<'_> {
        let ticket = self.next.get();
        self.next.set(ticket.wrapping_add(1));
        self.order.borrow_mut().entry(chunk).or_default().push_back(ticket);
        let held = InOrder { requests: self, chunk, ticket };
        poll_fn(|_| {
            let first = self.order.borrow().get(&chunk).and_then(|q| q.front().copied());
            if first == Some(ticket) { Poll::Ready(()) } else { Poll::Pending }
        }).await;
        held
    }

    /// Chunks' turns let go, counted: an executor that polls its requests
    /// again until this, the store's and the devices' moves stay put has
    /// polled every one that could go on.
    pub fn moves(&self) -> u64 {
        self.released.get()
    }

    /// Chunks' turns held or waited for: none once every request is over.
    pub fn in_use(&self) -> usize {
        self.order.borrow().values().map(|q| q.len()).sum()
    }

    /// Whether write `id` of volume `volume`'s writer at `generation`, come
    /// with mark `mark`, was applied here before as `now` has it -- the same
    /// chunk, no join of its set begun since -- or is below the mark,
    /// answered, and is answered as done, not written again. None where the
    /// writer has too many ids above its mark kept: refused for now.
    fn relayed_seen(&self, volume: u32, generation: u64, id: u64, mark: u64, now: Applied) -> Option<bool> {
        let mut t = self.relayed.borrow_mut();
        let e = t.entry(volume).or_insert(Relayed { generation, mark: 0, done: BTreeMap::new() });
        if generation > e.generation {
            *e = Relayed { generation, mark: 0, done: BTreeMap::new() };
        }
        /* An older writer's: the fence refuses it */
        if generation < e.generation {
            return Some(false);
        }
        if mark > e.mark {
            e.mark = mark;
            e.done = e.done.split_off(&mark);
        }
        if id < e.mark || e.done.get(&id) == Some(&now) {
            return Some(true);
        }
        if e.done.len() >= RELAYED_MAX {
            return None;
        }
        Some(false)
    }

    fn relayed_done(&self, volume: u32, generation: u64, id: u64, applied: Applied) {
        if let Some(e) = self.relayed.borrow_mut().get_mut(&volume) {
            if e.generation == generation && id >= e.mark {
                e.done.insert(id, applied);
            }
        }
    }

    fn has_check(&self, key: &Check) -> bool {
        self.inner.borrow().checks.contains(key)
    }

    fn add_check(&self, key: Check) {
        let mut v = self.inner.borrow_mut();
        if v.checks.len() >= VOUCHED_MAX {
            v.checks.clear();
        }
        v.checks.insert(key);
    }

    fn has_join(&self, key: &Join) -> bool {
        self.inner.borrow().joins.contains(key)
    }

    fn add_join(&self, key: Join) {
        let mut v = self.inner.borrow_mut();
        if v.joins.len() >= VOUCHED_MAX {
            v.joins.clear();
        }
        v.joins.insert(key);
    }
}

/// What a pass over a cs's garbage did (`collect`).
#[derive(Default, Debug)]
pub struct Collected {
    /// The chunks freed, each at its incarnation.
    pub freed: Vec<(u32, u32)>,
    /// The volumes whose fences were taken off.
    pub unfenced: Vec<u32>,
    /// A chunk, or a volume's fence, the store failed to let go of, and why.
    pub failed: Vec<(u32, Error)>,
}

/// The chunks `store` holds that the metadata freed -- or whose number is
/// another chunk's now -- freed, and the fences of volumes gone taken off:
/// as the host's mds says, asked in lists of LIST_MAX, and nothing where it
/// says nothing. nbs-cs's every 30 s, and the cluster target's cs's.
///
/// A chunk is freed in its order among the requests (`Requests`): never
/// between a request's look at it and what the request does -- a read the
/// mds vouched for, its answer on its way as the metadata dropped the
/// chunk, would read zeros where the chunk held data, as a chunk freed
/// reads as one never written.
pub async fn collect<D: Device, A: Asker>(store: &Store<D>, requests: &Requests, ask: &A) -> Collected {
    let mut done = Collected::default();
    let held: Vec<(u32, u32)> = store.holdings().iter().map(|h| (h.chunk, h.incarnation)).collect();
    for batch in held.chunks(LIST_MAX) {
        let Some(Told::Gone(gone)) = ask.ask(Question::Live { chunks: batch.to_vec() }).await else { return done };
        for c in gone {
            let Some(&(_, incarnation)) = batch.iter().find(|(n, _)| *n == c) else { continue };
            let _order = requests.in_order(c).await;
            match store.free(c, incarnation).await {
                Ok(true) => done.freed.push((c, incarnation)),
                Ok(false) => {}
                Err(e) => done.failed.push((c, e)),
            }
        }
    }
    let volumes: Vec<u32> = store.fenced_volumes().iter().map(|&(v, _)| v).collect();
    for batch in volumes.chunks(LIST_MAX) {
        let Some(Told::Gone(gone)) = ask.ask(Question::Volumes { volumes: batch.to_vec() }).await else { return done };
        for v in gone {
            match store.drop_fence(v).await {
                Ok(()) => done.unfenced.push(v),
                Err(e) => done.failed.push((v, e)),
            }
        }
    }
    done
}

/// What a request was done to, owned past its message: a reply's shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Done {
    Fenced { generation: u64 },
    Done,
    Data(Vec<u8>),
    Closed(bool),
    Held(Vec<(u32, u32, u64)>),
    Chunks(Vec<(u32, u32, u32, u32)>),
    Sums(Vec<u32>),
    Scrub { open: bool, written: u32, sums: Vec<u32> },
}

impl Done {
    /// The reply it is, borrowing its data.
    pub fn reply(&self) -> Reply<'_> {
        match self {
            Done::Fenced { generation } => Reply::Fenced { generation: *generation },
            Done::Done => Reply::Done,
            Done::Data(d) => Reply::Data(d),
            Done::Closed(c) => Reply::Closed(*c),
            Done::Held(h) => Reply::Held(h.clone()),
            Done::Chunks(c) => Reply::Chunks(c.clone()),
            Done::Sums(s) => Reply::Sums(s.clone()),
            Done::Scrub { open, written, sums } => Reply::Scrub { open: *open, written: *written, sums: sums.clone() },
        }
    }
}

/// `req` done on `store`, or why not. `ask` asks the host's mds a question;
/// what it is told is awaited by this request alone.
///
/// A chunk the cs found open when it started -- its writer's data not
/// flushed may be gone here while the other replicas have it -- is refused
/// to that same writer as `Failed`, which takes this replica out of its set
/// (N8): never read or written as if it were the others' (`found_open`) --
/// nor flushed for it, as a flush done would say that what it wrote here
/// before is durable (found by fuzz/nbs's cluster target). But not where the
/// cs is the set's joiner (N9): written and not read, its copy pulls every
/// chunk it found open -- the mds asked first where the store has not been
/// told yet, as a member back's first write comes (found by fuzz/nbs's join
/// target: refused, the writer asked it out, and its join was dropped every
/// time it was made). That same writer is the volume's as well as the
/// generation's (`found_open_by`): generations are counted by volume, and a
/// clone's writer at the generation another volume's left the chunk open at
/// is not refused for it (N10d).
pub async fn serve<D: Device, A: Asker>(store: &Store<D>, vouched: &Requests, req: &Request<'_>, ask: &A)
    -> Result<Done, Refusal> {
    serve_noting(store, vouched, req, ask, &Cell::new(0)).await
}

/// `serve`, and for a write, the joins of its chunk's set the cs had begun
/// as it was marked put in `joins` (`Store::write_marked`, N13).
async fn serve_noting<D: Device, A: Asker>(store: &Store<D>, vouched: &Requests, req: &Request<'_>, ask: &A,
                                           joins: &Cell<u64>) -> Result<Done, Refusal> {
    let (v, g) = (req.volume, req.generation);
    let _order = match req.op {
        Op::Write { chunk, .. } | Op::Read { chunk, .. } | Op::Close { chunk, .. } => {
            Some(vouched.in_order(chunk.chunk).await)
        }
        Op::CopyUp { to, .. } => Some(vouched.in_order(to.chunk).await),
        _ => None,
    };
    if let Op::Write { chunk, position, index, .. } | Op::Read { chunk, position, index, .. }
         | Op::Close { chunk, position, index } = req.op {
        if g != 0 && store.found_open_by(v, chunk.chunk, chunk.incarnation) == Some(g)
            && store.joins(chunk.epoch.set).is_none()
        {
            let writing = !matches!(req.op, Op::Read { .. });
            let told = if writing {
                ask.ask(Question::Verify { volume: v, generation: g, chunk, position, index, writing }).await
            } else {
                None
            };
            match told {
                Some(Told::VerifiedJoining { since }) => store.join(chunk.epoch.set, since),
                /* The mds's own reason -- an epoch moved on, a writer fenced:
                   told as it is, for the client to act on */
                Some(Told::Refused(why)) => return Err(why),
                _ => return Err(Refusal::Failed),
            }
        }
    }
    if let Op::Flush = req.op {
        if g != 0 && store.found_open_in(v, g) {
            return Err(Refusal::Failed);
        }
    }
    match req.op {
        Op::Fence => store.fence(v, g).await.map(|g| Done::Fenced { generation: g }).map_err(Refusal::of),
        Op::Write { chunk, position, index, offset, data } => {
            let check = Check::of(v, g, chunk);
            let known = vouched.knows(store, &check, &chunk);
            let marked = match store.write_marked(v, g, chunk, offset, data, known).await {
                Err(e) if unchecked(&e, known) => {
                    verify(store, vouched, ask, check, position, index, true).await?;
                    store.write_marked(v, g, chunk, offset, data, true).await.map_err(Refusal::of)?
                }
                done => done.map_err(Refusal::of)?,
            };
            joins.set(marked);
            Ok(Done::Done)
        }
        Op::Read { chunk, position, index, offset, len } => {
            let mut buf = Vec::new();
            if buf.try_reserve_exact(len as usize).is_err() {
                return Err(Refusal::NotReady);
            }
            buf.resize(len as usize, 0);
            let check = Check::of(v, g, chunk);
            let known = vouched.knows(store, &check, &chunk);
            match store.read(v, g, chunk, offset, &mut buf, known).await {
                Err(e) if unchecked(&e, known) => {
                    verify(store, vouched, ask, check, position, index, false).await?;
                    store.read(v, g, chunk, offset, &mut buf, true).await.map_err(Refusal::of)?;
                }
                done => done.map_err(Refusal::of)?,
            }
            Ok(Done::Data(buf))
        }
        Op::Flush => store.flush_for(v, g).await.map(|()| Done::Done).map_err(Refusal::of),
        Op::Close { chunk, position, index } => {
            let check = Check::of(v, g, chunk);
            let known = vouched.knows(store, &check, &chunk);
            match store.close(v, g, chunk).await {
                Err(e) if unchecked(&e, known) => {
                    verify(store, vouched, ask, check, position, index, true).await?;
                    store.close(v, g, chunk).await.map(Done::Closed).map_err(Refusal::of)
                }
                done => done.map(Done::Closed).map_err(Refusal::of),
            }
        }
        Op::Held => Ok(Done::Held(store.holdings().iter()
            .filter(|h| h.volume == v && h.state == SlotState::Open)
            .take(LIST_MAX).map(|h| (h.chunk, h.incarnation, h.generation)).collect())),
        /* The leader's look: there, and serving -- the caller's not serving
           refused it before here */
        Op::Ping => Ok(Done::Done),
        /* A joiner's asks of this cs as a source (N9) */
        Op::Holdings { set, from, joiner, version, index } => {
            joined(vouched, ask, set, joiner, version, index).await?;
            Ok(Done::Chunks(store.holdings_in(set, from, LIST_MAX)))
        }
        Op::Pull { chunk, joiner, index, offset, len } => {
            joined(vouched, ask, chunk.epoch.set, joiner, chunk.epoch.version, index).await?;
            let mut buf = Vec::new();
            if buf.try_reserve_exact(len as usize).is_err() {
                return Err(Refusal::NotReady);
            }
            buf.resize(len as usize, 0);
            store.pull(chunk, offset, &mut buf).await.map_err(Refusal::of)?;
            Ok(Done::Data(buf))
        }
        Op::Sums { chunk, joiner, index, offset, len } => {
            joined(vouched, ask, chunk.epoch.set, joiner, chunk.epoch.version, index).await?;
            store.sums(chunk, offset, len).await.map(Done::Sums).map_err(Refusal::of)
        }
        /* The leader's scrub (N9e): sums and a record, nothing moved on and
           no data -- answered as a ping is, unvouched */
        Op::Scrub { chunk } => store.scrub(chunk).await
            .map(|(open, written, sums)| Done::Scrub { open, written, sums }).map_err(Refusal::of),
        Op::CopyUp { from, to, position, index } => {
            copied(store, ask, v, g, from, to, position, index).await?;
            Ok(Done::Done)
        }
    }
}

/// `req` done on `store` as `serve` does it -- and, a write passed along its
/// chain (N13), applied once: one that comes again, along its chain and
/// directly, answered as done, not written again over what may have come
/// after it (`Requests`'s relayed writes).
pub async fn serve_relayed<D: Device, A: Asker>(store: &Store<D>, requests: &Requests, req: &Request<'_>,
                                                relay: Option<&Relay<'_>>, ask: &A) -> Result<Done, Refusal> {
    let Some(relay) = relay else { return serve(store, requests, req, ask).await };
    let (v, g) = (req.volume, req.generation);
    /* Only a write is relayed (`decode_relayed`) */
    let Op::Write { chunk, .. } = req.op else { return serve(store, requests, req, ask).await };
    let now = Applied { chunk: chunk.chunk, incarnation: chunk.incarnation, joins: store.joins_begun(chunk.epoch.set) };
    match requests.relayed_seen(v, g, req.id, relay.mark, now) {
        /* Applied before -- and answered as the store would answer it now:
           refused to a writer a newer fence has taken the volume from */
        Some(true) => {
            let fence = store.fence_of(v);
            return if fence > g { Err(Refusal::Fenced { generation: fence }) } else { Ok(Done::Done) };
        }
        None => return Err(Refusal::NotReady),
        Some(false) => {}
    }
    /* With the joins begun as it was marked: one begun while it waited for
       its turn or the mds -- or by the mds's answer -- marked it, one begun
       as its data went to the device did not */
    let joins = Cell::new(0);
    let done = serve_noting(store, requests, req, ask, &joins).await;
    if matches!(done, Ok(Done::Done)) {
        requests.relayed_done(v, g, req.id, Applied { joins: joins.get(), ..now });
    }
    done
}

/// A copy-up's copy done on the store (N10) -- where the cs does not hold
/// its new chunk yet, asked of the mds first, every time: once per copy-up,
/// and done again only where the copy was lost.
#[allow(clippy::too_many_arguments)]
async fn copied<D: Device, A: Asker>(store: &Store<D>, ask: &A, volume: u32, generation: u64, from: ChunkRef,
                                     to: ChunkRef, position: u32, index: u64) -> Result<(), Refusal> {
    match store.copy_up(volume, generation, from, to, false).await {
        Err(Error::Unverified) => {}
        done => return done.map_err(Refusal::of),
    }
    match ask.ask(Question::VerifyCopy { volume, generation, position, from, to, index }).await {
        Some(Told::Verified) => store.copy_up(volume, generation, from, to, true).await.map_err(Refusal::of),
        Some(Told::Refused(why)) => Err(why),
        _ => Err(Refusal::NoMds),
    }
}

/// Cs `joiner` as set `set`'s joiner at its version `version`, vouched for
/// by the host's mds -- asked once, then kept: a version gone stale is
/// refused by the store, which knows the chunk's.
async fn joined<A: Asker>(vouched: &Requests, ask: &A, set: u32, joiner: u32, version: u32, index: u64)
    -> Result<(), Refusal> {
    let key = Join { set, joiner, version };
    if vouched.has_join(&key) {
        return Ok(());
    }
    match ask.ask(Question::VerifyJoin { set, joiner, version, index }).await {
        Some(Told::Verified) => {
            vouched.add_join(key);
            Ok(())
        }
        Some(Told::Refused(why)) => Err(why),
        _ => Err(Refusal::NoMds),
    }
}

impl Check {
    fn of(volume: u32, generation: u64, chunk: ChunkRef) -> Check {
        Check { volume, generation, chunk: chunk.chunk, incarnation: chunk.incarnation, set: chunk.epoch.set,
                version: chunk.epoch.version }
    }
}

impl Requests {
    /// Whether the mds vouched for `check` before -- taken only for a chunk
    /// the cs holds (N10): one freed since -- a copy-up done, the old chunk
    /// named by no map -- reads as zeros, which it vouched for of no chunk.
    fn knows<D: Device>(&self, store: &Store<D>, check: &Check, chunk: &ChunkRef) -> bool {
        self.has_check(check) && store.holding(chunk.chunk).is_some_and(|h| h.incarnation == chunk.incarnation)
    }
}

/// Whether the engine's refusal of a request it was not told was vouched
/// for sends the request to the mds: a chunk it does not hold, an epoch it
/// does not know.
fn unchecked(e: &Error, known: bool) -> bool {
    !known && matches!(e, Error::Unverified | Error::Epoch { .. })
}

/// `check` asked of the mds, for the request to be done again once it
/// vouches -- where it says the cs is the set's joiner, what clients write
/// to the set marked from now on, before the request is done again.
async fn verify<D: Device, A: Asker>(store: &Store<D>, vouched: &Requests, ask: &A, check: Check, position: u32,
                                     index: u64, writing: bool) -> Result<(), Refusal> {
    let chunk = ChunkRef { chunk: check.chunk, incarnation: check.incarnation,
                           epoch: nbscs::Epoch { set: check.set, version: check.version } };
    let question = Question::Verify { volume: check.volume, generation: check.generation, chunk, position, index,
                                      writing };
    match ask.ask(question).await {
        Some(told @ (Told::Verified | Told::VerifiedJoining { .. })) => {
            if let Told::VerifiedJoining { since } = told {
                store.join(check.set, since);
            }
            vouched.add_check(check);
            Ok(())
        }
        Some(Told::Refused(why)) => Err(why),
        _ => Err(Refusal::NoMds),
    }
}
