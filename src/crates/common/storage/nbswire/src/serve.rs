//! A cs's half of the wire (plans/nbs.md, "The cs"): a client's request
//! done on the cs's store, checked first with the host's mds where the
//! store's engine has not checked it -- a chunk it does not hold yet, an
//! epoch other than the one it knows. The one copy that nbs-cs.ko serves
//! with and fuzz/nbs's cluster target's cs's do; how the mds is asked is
//! the caller's.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use nbscs::{now, ChunkRef, Device, Error, SlotState, Store};

use crate::{Op, Question, Refusal, Reply, Request, Told, LIST_MAX};

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

/// The checks the host's mds vouched for: a request of one is done without
/// asking again.
#[derive(Default)]
pub struct Vouched {
    checks: BTreeSet<Check>,
    joins: BTreeSet<Join>,
}

impl Vouched {
    pub fn new() -> Vouched {
        Vouched::default()
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
/// as the host's mds says, asked `ask` in lists of LIST_MAX, and nothing
/// where it says nothing. nbs-cs's every 30 s, and the cluster target's
/// cs's.
pub fn collect<D: Device>(store: &mut Store<D>, ask: &mut dyn FnMut(Question) -> Option<Told>) -> Collected {
    let mut done = Collected::default();
    let held: Vec<(u32, u32)> = store.holdings().iter().map(|h| (h.chunk, h.incarnation)).collect();
    for batch in held.chunks(LIST_MAX) {
        let Some(Told::Gone(gone)) = ask(Question::Live { chunks: batch.to_vec() }) else { return done };
        for c in gone {
            let Some(&(_, incarnation)) = batch.iter().find(|(n, _)| *n == c) else { continue };
            match now(store.free(c, incarnation)) {
                Ok(true) => done.freed.push((c, incarnation)),
                Ok(false) => {}
                Err(e) => done.failed.push((c, e)),
            }
        }
    }
    let volumes: Vec<u32> = store.fenced_volumes().iter().map(|&(v, _)| v).collect();
    for batch in volumes.chunks(LIST_MAX) {
        let Some(Told::Gone(gone)) = ask(Question::Volumes { volumes: batch.to_vec() }) else { return done };
        for v in gone {
            match now(store.drop_fence(v)) {
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

/// `req` done on `store`, or why not. `ask` asks the host's mds a question
/// and waits for what it is told: None where there is no mds, or it said
/// nothing in time.
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
pub fn serve<D: Device>(store: &mut Store<D>, vouched: &mut Vouched, req: &Request<'_>,
                        ask: &mut dyn FnMut(Question) -> Option<Told>) -> Result<Done, Refusal> {
    let (v, g) = (req.volume, req.generation);
    if let Op::Write { chunk, position, index, .. } | Op::Read { chunk, position, index, .. }
         | Op::Close { chunk, position, index } = req.op {
        if g != 0 && store.found_open_by(v, chunk.chunk, chunk.incarnation) == Some(g)
            && store.joins(chunk.epoch.set).is_none()
        {
            let writing = !matches!(req.op, Op::Read { .. });
            match writing.then(|| ask(Question::Verify { volume: v, generation: g, chunk, position, index, writing }))
                .flatten() {
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
        Op::Fence => now(store.fence(v, g)).map(|g| Done::Fenced { generation: g }).map_err(Refusal::of),
        Op::Write { chunk, position, index, offset, data } => {
            checked(store, vouched, ask, v, g, chunk, position, index, true,
                    &mut |s, verified| now(s.write(v, g, chunk, offset, data, verified)))?;
            Ok(Done::Done)
        }
        Op::Read { chunk, position, index, offset, len } => {
            let mut buf = Vec::new();
            if buf.try_reserve_exact(len as usize).is_err() {
                return Err(Refusal::NotReady);
            }
            buf.resize(len as usize, 0);
            checked(store, vouched, ask, v, g, chunk, position, index, false,
                    &mut |s, verified| now(s.read(v, g, chunk, offset, &mut buf, verified)))?;
            Ok(Done::Data(buf))
        }
        Op::Flush => now(store.flush_for(v, g)).map(|()| Done::Done).map_err(Refusal::of),
        Op::Close { chunk, position, index } => {
            checked(store, vouched, ask, v, g, chunk, position, index, true, &mut |s, _| now(s.close(v, g, chunk)))
                .map(Done::Closed)
        }
        Op::Held => Ok(Done::Held(store.holdings().iter()
            .filter(|h| h.volume == v && h.state == SlotState::Open)
            .take(LIST_MAX).map(|h| (h.chunk, h.incarnation, h.generation)).collect())),
        /* The leader's look: there, and serving -- the caller's not serving
           refused it before here */
        Op::Ping => Ok(Done::Done),
        /* A joiner's asks of this cs as a source (N9) */
        Op::Holdings { set, from, joiner, version, index } => {
            joined(vouched, ask, set, joiner, version, index)?;
            Ok(Done::Chunks(store.holdings_in(set, from, LIST_MAX)))
        }
        Op::Pull { chunk, joiner, index, offset, len } => {
            joined(vouched, ask, chunk.epoch.set, joiner, chunk.epoch.version, index)?;
            let mut buf = Vec::new();
            if buf.try_reserve_exact(len as usize).is_err() {
                return Err(Refusal::NotReady);
            }
            buf.resize(len as usize, 0);
            now(store.pull(chunk, offset, &mut buf)).map_err(Refusal::of)?;
            Ok(Done::Data(buf))
        }
        Op::Sums { chunk, joiner, index, offset, len } => {
            joined(vouched, ask, chunk.epoch.set, joiner, chunk.epoch.version, index)?;
            now(store.sums(chunk, offset, len)).map(Done::Sums).map_err(Refusal::of)
        }
        /* The leader's scrub (N9e): sums and a record, nothing moved on and
           no data -- answered as a ping is, unvouched */
        Op::Scrub { chunk } => now(store.scrub(chunk))
            .map(|(open, written, sums)| Done::Scrub { open, written, sums }).map_err(Refusal::of),
        Op::CopyUp { from, to, position, index } => {
            copied(store, ask, v, g, from, to, position, index)?;
            Ok(Done::Done)
        }
    }
}

/// A copy-up's copy done on the store (N10) -- where the cs does not hold
/// its new chunk yet, asked of the mds first, every time: once per copy-up,
/// and done again only where the copy was lost.
#[allow(clippy::too_many_arguments)]
fn copied<D: Device>(store: &mut Store<D>, ask: &mut dyn FnMut(Question) -> Option<Told>, volume: u32,
                     generation: u64, from: ChunkRef, to: ChunkRef, position: u32, index: u64) -> Result<(), Refusal> {
    match now(store.copy_up(volume, generation, from, to, false)) {
        Err(Error::Unverified) => {}
        done => return done.map_err(Refusal::of),
    }
    match ask(Question::VerifyCopy { volume, generation, position, from, to, index }) {
        Some(Told::Verified) => now(store.copy_up(volume, generation, from, to, true)).map_err(Refusal::of),
        Some(Told::Refused(why)) => Err(why),
        _ => Err(Refusal::NoMds),
    }
}

/// Cs `joiner` as set `set`'s joiner at its version `version`, vouched for
/// by the host's mds -- asked once, then kept: a version gone stale is
/// refused by the store, which knows the chunk's.
fn joined(vouched: &mut Vouched, ask: &mut dyn FnMut(Question) -> Option<Told>, set: u32, joiner: u32, version: u32,
          index: u64) -> Result<(), Refusal> {
    let key = Join { set, joiner, version };
    if vouched.joins.contains(&key) {
        return Ok(());
    }
    match ask(Question::VerifyJoin { set, joiner, version, index }) {
        Some(Told::Verified) => {
            if vouched.joins.len() >= VOUCHED_MAX {
                vouched.joins.clear();
            }
            vouched.joins.insert(key);
            Ok(())
        }
        Some(Told::Refused(why)) => Err(why),
        _ => Err(Refusal::NoMds),
    }
}

/// `op` done on the store -- and where the engine has not checked the
/// request, asked of the mds first and done again once it vouches. What it
/// vouched for before is taken only for a chunk the cs holds (N10): one
/// freed since -- a copy-up done, the old chunk named by no map -- reads as
/// zeros, which it vouched for of no chunk.
#[allow(clippy::too_many_arguments)]
fn checked<D: Device, T>(store: &mut Store<D>, vouched: &mut Vouched, ask: &mut dyn FnMut(Question) -> Option<Told>,
                         volume: u32, generation: u64, chunk: ChunkRef, position: u32, index: u64, writing: bool,
                         op: &mut dyn FnMut(&mut Store<D>, bool) -> Result<T, Error>) -> Result<T, Refusal> {
    let key = Check { volume, generation, chunk: chunk.chunk, incarnation: chunk.incarnation, set: chunk.epoch.set,
                      version: chunk.epoch.version };
    let known = vouched.checks.contains(&key)
        && store.holding(chunk.chunk).is_some_and(|h| h.incarnation == chunk.incarnation);
    match op(store, known) {
        Ok(t) => return Ok(t),
        Err(Error::Unverified) | Err(Error::Epoch { .. }) if !known => {}
        Err(e) => return Err(Refusal::of(e)),
    }
    match ask(Question::Verify { volume, generation, chunk, position, index, writing }) {
        Some(told @ (Told::Verified | Told::VerifiedJoining { .. })) => {
            /* The set's joiner: what clients write to it marked from now
               on, before this write is done */
            if let Told::VerifiedJoining { since } = told {
                store.join(chunk.epoch.set, since);
            }
            if vouched.checks.len() >= VOUCHED_MAX {
                vouched.checks.clear();
            }
            vouched.checks.insert(key);
            op(store, true).map_err(Refusal::of)
        }
        Some(Told::Refused(why)) => Err(why),
        _ => Err(Refusal::NoMds),
    }
}
