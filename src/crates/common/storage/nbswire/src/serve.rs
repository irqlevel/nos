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

/// The checks the host's mds vouched for: a request of one is done without
/// asking again.
#[derive(Default)]
pub struct Vouched {
    checks: BTreeSet<Check>,
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
/// before is durable (found by fuzz/nbs's cluster target).
pub fn serve<D: Device>(store: &mut Store<D>, vouched: &mut Vouched, req: &Request<'_>,
                        ask: &mut dyn FnMut(Question) -> Option<Told>) -> Result<Done, Refusal> {
    let (v, g) = (req.volume, req.generation);
    if let Op::Write { chunk, .. } | Op::Read { chunk, .. } | Op::Close { chunk, .. } = req.op {
        if g != 0 && store.found_open(chunk.chunk, chunk.incarnation) == Some(g) {
            return Err(Refusal::Failed);
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
        Some(Told::Verified) => {
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
