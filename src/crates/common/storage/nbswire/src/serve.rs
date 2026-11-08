//! A cs's half of the wire (plans/nbs.md, "The cs"): a client's request
//! done on the cs's store, checked first with the host's mds where the
//! store's engine has not checked it -- a chunk it does not hold yet, an
//! epoch other than the one it knows. The one copy that nbs-cs.ko serves
//! with and fuzz/nbs's cluster target's cs's do; how the mds is asked is
//! the caller's.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use nbscs::{ChunkRef, Device, Error, SlotState, Store};

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
    Written(u32),
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
            Done::Written(v) => Reply::Written(*v),
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
/// time it was made).
pub fn serve<D: Device>(store: &mut Store<D>, vouched: &mut Vouched, req: &Request<'_>,
                        ask: &mut dyn FnMut(Question) -> Option<Told>) -> Result<Done, Refusal> {
    let (v, g) = (req.volume, req.generation);
    if let Op::Write { chunk, position, index, .. } | Op::Read { chunk, position, index, .. }
         | Op::Close { chunk, position, index } = req.op {
        if g != 0 && store.found_open(chunk.chunk, chunk.incarnation) == Some(g)
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
        Op::Fence => store.fence(v, g).map(|now| Done::Fenced { generation: now }).map_err(Refusal::of),
        Op::Write { chunk, position, index, offset, data } => {
            checked(store, vouched, ask, v, g, chunk, position, index, true,
                    &mut |s, verified| s.write(v, g, chunk, offset, data, verified))?;
            Ok(Done::Done)
        }
        Op::Read { chunk, position, index, offset, len } => {
            let mut buf = Vec::new();
            if buf.try_reserve_exact(len as usize).is_err() {
                return Err(Refusal::NotReady);
            }
            buf.resize(len as usize, 0);
            checked(store, vouched, ask, v, g, chunk, position, index, false,
                    &mut |s, verified| s.read(v, g, chunk, offset, &mut buf, verified))?;
            Ok(Done::Data(buf))
        }
        Op::Flush => store.flush_for(v, g).map(|()| Done::Done).map_err(Refusal::of),
        Op::Close { chunk, position, index } => {
            checked(store, vouched, ask, v, g, chunk, position, index, true, &mut |s, _| s.close(v, g, chunk))
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
            store.pull(chunk, offset, &mut buf).map_err(Refusal::of)?;
            Ok(Done::Data(buf))
        }
        Op::Sums { chunk, joiner, index, offset, len } => {
            joined(vouched, ask, chunk.epoch.set, joiner, chunk.epoch.version, index)?;
            store.sums(chunk, offset, len).map(Done::Sums).map_err(Refusal::of)
        }
        Op::Touch { chunk, joiner, index } => {
            joined(vouched, ask, chunk.epoch.set, joiner, chunk.epoch.version, index)?;
            store.touch(chunk).map(Done::Written).map_err(Refusal::of)
        }
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
/// request, asked of the mds first and done again once it vouches.
#[allow(clippy::too_many_arguments)]
fn checked<D: Device, T>(store: &mut Store<D>, vouched: &mut Vouched, ask: &mut dyn FnMut(Question) -> Option<Told>,
                         volume: u32, generation: u64, chunk: ChunkRef, position: u32, index: u64, writing: bool,
                         op: &mut dyn FnMut(&mut Store<D>, bool) -> Result<T, Error>) -> Result<T, Refusal> {
    let key = Check { volume, generation, chunk: chunk.chunk, incarnation: chunk.incarnation, set: chunk.epoch.set,
                      version: chunk.epoch.version };
    let known = vouched.checks.contains(&key);
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
