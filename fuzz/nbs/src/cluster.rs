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
//!   asks a cs out of a set only where its view writes it there;
//! - **a snapshot reads for good what the volume held at it** (N10c):
//!   taken between two writers -- the one stopped, a flush and its closes
//!   done, the next one started -- some dropped later, two kept at most; the
//!   writers after it copy up each chunk it shares before they write there,
//!   through joins, crashes and takeovers; and each block of it, on every
//!   active replica up, reads the same, one of what the volume could hold
//!   then, at every look -- after it is taken, now and then, and at the end.
//!   Each cs's garbage pass runs as nbs-cs's, freeing what copy-ups and
//!   deletes leave. A snapshot is taken now and then of the volume while it
//!   is attached too (N10c2): its writer paused as the writer's host's mds
//!   pauses it -- every chunk closed when it answers, the snapshot applied at
//!   its generation, the word to go on now and then lost, the snapshot now
//!   and then applied late, past the session's wait, when its attach again
//!   is to have it refused; a pause not answered in five seconds given up; a
//!   writer taken over while it paused answering all the same, what is open
//!   then the writer's after it, and its snapshot refused;
//!   at some ends the writer stopped while paused, a write held behind the
//!   pause, the stop begun once it is over;
//! - **a stop is done once**, and as its session ends: detached, or failed
//!   (a driver tears its session down at it); asked again, done at once and
//!   not ok, the first standing; asked with a tag of the session's own,
//!   refused at once, the session as it was;
//! - **a clone holds what its snapshot held and its writer wrote** (N10d2):
//!   one of the newest snapshot, made once the snapshot's blocks were read,
//!   written and read by a writer of its own -- `End::Clone` on the
//!   network, apart from the volume's writers -- its reads one of what its
//!   model allows, its writes on every replica in, and at the end every
//!   block on every active replica as its writer left it; dropped now and
//!   then, and at once where its writer is lost, let go of by force then.
//!
//! An operation fails only at its deadline or with its session, but for a
//! flush or a close out with one before it that failed at its deadline: the
//! client fails those with it, as what it was to flush was written before
//! them too.
//!
//! A writer that follows another holds what that one had done and not
//! flushed as a crash would leave it: there or not -- a replica that lost it
//! may be what its reconcile copied.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use nbsclient::{Buffers, Out, Phase, Session, To, UserOp, BLOCK, DEADLINE_NS, LOST_LAST, PAUSE_NS};
use nbscs::{ChunkRef, Epoch, Identity, SlotState, Store, SLOT};
use nbsmeta::repair::{self, Seen};
use nbsmeta::{Answer, Command, Replica, State};
use nbswire::copy::{self, Copy};
use nbswire::scrub::{self, Scrub};
use nbswire::serve::{collect, serve, serve_relayed, AskNow, Asker, Collected, Done, Requests};
use nbswire::{Answer as CsAnswer, Answered, Ask, Asking, Query, Question, Refusal, Reply, Request, Said, Saying, Told};

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
/// How long the writer's write requests find no memory in the end that
/// starves it (#97): longer than a cs is given to answer.
const STARVED_NS: u64 = 3 * nbsclient::SILENT_NS;
/// The writes made while it is starved.
const STARVED_WRITES: u64 = 6;
/// A host's disk: room for the volume's chunks, the copy-ups after two
/// snapshots, and some, in sectors of 4 KiB.
const DISK_SLOTS: u64 = 20;
const DISK_SECTOR: u64 = 4096;
const MS: u64 = 1_000_000;
/// How long a close asked alone of a few places may take: its windows' round
/// trips -- well under the 3 s a place idles before it is closed of itself.
const CLOSE_ALONE_NS: u64 = 2_500 * MS;
/// The places a session closes at once, at most: nbsclient's `CLOSING_MAX`
/// as the fuzzer builds it (`small_windows`).
const CLOSING_FUZZ: usize = 2;
/// How long a cs waits for its mds's answer, as nbs-cs does.
const MDS_WAIT: u64 = 5_000 * MS;
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
/// The leader's scrub's pace here (N9e): a chunk's look every four seconds
/// of the network's time -- each reads it whole on every member it is
/// closed on.
const SCRUB_EVERY: u64 = 4_000 * MS;
/// How often each cs's garbage pass runs, in the network's time.
const GARBAGE_EVERY: u64 = 3_000 * MS;
/// The snapshots kept at once, at most: the oldest dropped for a new one.
const SNAPS_KEPT: usize = 2;
/// The clone's name (N10d2): one at a time, of a snapshot of the volume.
const CLONE: &str = "w";
/// How long the writer's host's mds waits for its pause to be answered
/// before it gives the snapshot up (N10c2).
const PAUSE_WAIT_NS: u64 = 5_000 * MS;
/// The tags of a writer's stop: at the end, for a snapshot between two
/// writers, and the end's asked again -- above any operation's, and the
/// driver's as nbsdrive's are: the top bit, which marks the session's own
/// operations, clear. With it set, a stop's flush done out of turn would be
/// told to nobody, and no judge here would see it.
const STOP: u64 = (1 << 62) | 1;
const SNAP_STOP: u64 = (1 << 62) | 2;
const STOP_AGAIN: u64 = (1 << 62) | 3;
/// A tag of the session's own: a stop asked with it is refused.
const STOP_OWN: u64 = (1 << 63) | 1;

/// A pause for a snapshot of the volume while attached (N10c2): the client
/// asked, the pause's number, and when.
struct Pausing {
    c: usize,
    pause: u64,
    asked_at: u64,
}

/// A snapshot of a pause applied late, past the session's wait (N10c2):
/// when, at the generation the session paused at -- refused once it has
/// attached again -- and what it would hold.
struct Late {
    at: u64,
    generation: u64,
    may: BTreeMap<u64, BTreeSet<u8>>,
}

/// A snapshot of the volume, taken between two writers (N10c): for each
/// block written by then, and each written first since, what it may hold --
/// the last write done before it, or one failed or lost after that, or zeros
/// -- and the value a look found first, which every look after must find.
struct Snap {
    name: String,
    may: BTreeMap<u64, BTreeSet<u8>>,
    seen: BTreeMap<u64, u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum End {
    Client(usize),
    Cs(usize),
    Mds(usize),
    /// A host's cs as the joiner of its sets: its copies' requests to their
    /// sources, and the answers.
    Copy(usize),
    /// A clone's writer (N10d2).
    Clone(usize),
}

/// A clone of a snapshot of the volume, and its writer (N10d2): a session
/// on a host, as a client's is, apart from the volume's writers -- one a
/// clone, the clone dropped with it.
struct CloneW {
    host: usize,
    s: Session,
    bufs: Bufs,
    ops: BTreeMap<u64, MOp>,
    next_tag: u64,
    /// Lost with its host, or failed as its chunk's last replica refused
    /// it: driven no more, the clone let go of by force and dropped.
    dead: bool,
    /// Its stop done: once, as its session ends.
    stop_done: bool,
    /// When a flush of its last failed at its deadline: those out with it
    /// fail then too.
    flush_failed_at: Option<u64>,
    /// Its attaches anew for a write failed at its deadline, counted.
    anew: u64,
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
    store: Option<Rc<Store<Handle>>>,
    vouched: Rc<Requests>,
    /// Its cs's requests in flight (N12c), as nbs-cs's are -- each pending
    /// while a question of its to the mds is out -- and those questions.
    flights: Vec<Flight>,
    asks: Rc<Asks>,
    /// Its garbage pass under way, if one is: a future of its cs's own, as
    /// nbs-cs's is, its questions over the network and its frees in each
    /// chunk's order among the requests.
    garbage: Option<Pin<Box<dyn Future<Output = Collected>>>>,
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

/// A request at a host's cs, in flight there (N12c): the end it answers,
/// its message's id and bytes -- for the judge at its end -- and its
/// future, nbswire's `serve` of it.
struct Flight {
    from: End,
    msg: u64,
    bytes: Vec<u8>,
    fut: Pin<Box<dyn Future<Output = Result<Done, Refusal>>>>,
}

/// The session a write passed along its chain answers (N13): the name it
/// carries -- `w<client>@<host>` or `k<clone>@<host>` -- as the target names
/// its sessions.
/// A write piece of a session whose writes go along chains sent without
/// its mark (N13): served as written once more each time it came, and a
/// copy of it late, by another way than a newer write of its blocks, would
/// put older data over that one.
fn unmarked(chains: bool, bytes: &[u8], who: &str, id: u64, cs: u32) -> Result<(), String> {
    match Request::decode_relayed(bytes) {
        Some((r, None)) if chains && matches!(r.op, nbswire::Op::Write { .. }) => {
            Err(format!("{} sent write #{} to cs {} without its mark, its writes going along chains", who, id, cs))
        }
        _ => Ok(()),
    }
}

fn named_end(answer: &str) -> Option<End> {
    let (name, _) = answer.split_once('@')?;
    let n: usize = name.get(1..)?.parse().ok()?;
    match name.as_bytes().first()? {
        b'w' => Some(End::Client(n)),
        b'k' => Some(End::Clone(n)),
        _ => None,
    }
}

/// A host's cs's questions to its mds (N12c), as nbs-cs's go: each a
/// message of the network's -- queued here by the request that asks it,
/// put on its way by the host, answered by the mds when it gets there, as
/// the state is then -- and its answer, when it comes, the asking request's
/// alone. One unanswered in MDS_WAIT is answered None; one answered later,
/// dropped.
#[derive(Default)]
struct Asks {
    now: Cell<u64>,
    next: Cell<u64>,
    outbox: RefCell<Vec<(u64, Question)>>,
    /// Each out: when it was asked, and what it was told once it was.
    out: RefCell<BTreeMap<u64, (u64, Option<Told>)>>,
}

/// A question out: its id let go of however its ask ends.
struct Asked<'a>(&'a Asks, u64);

impl Drop for Asked<'_> {
    fn drop(&mut self) {
        self.0.out.borrow_mut().remove(&self.1);
    }
}

impl Asker for Asks {
    fn ask(&self, question: Question) -> impl Future<Output = Option<Told>> + '_ {
        async move {
            let id = self.next.get() + 1;
            self.next.set(id);
            let at = self.now.get();
            self.outbox.borrow_mut().push((id, question));
            self.out.borrow_mut().insert(id, (at, None));
            let _asked = Asked(self, id);
            std::future::poll_fn(|_| {
                let mut out = self.out.borrow_mut();
                match out.get_mut(&id) {
                    Some((_, told)) if told.is_some() => Poll::Ready(told.take()),
                    Some((at, _)) if self.now.get() >= *at + MDS_WAIT => Poll::Ready(None),
                    _ => Poll::Pending,
                }
            }).await
        }
    }
}

impl Asks {
    /// What the mds told question `id`, handed to it: dropped where it is
    /// out no more.
    fn told(&self, id: u64, told: Told) {
        if let Some((_, slot)) = self.out.borrow_mut().get_mut(&id) {
            *slot = Some(told);
        }
    }

    /// Everything out forgotten: its host crashed.
    fn clear(&self) {
        self.outbox.borrow_mut().clear();
        self.out.borrow_mut().clear();
    }
}

/// A chunk the target rotted (N9e): on one member, or on two -- a sector
/// each -- when the scrub found it, and whether its members have been taken
/// down for it, as a person would, half a minute after.
struct Rot {
    hosts: Vec<usize>,
    chunk: u32,
    set: u32,
    found: Option<u64>,
    taken: bool,
}

/// How long after the scrub's finding the members rotted are taken down:
/// past the scrub's next round over the chunk, which is to say nothing again.
const TAKE_DOWN_AFTER: u64 = 30_000 * MS;

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
    /// Its writes, flushes and closes failed: an end's judge holds the
    /// writer's own, not a session's taken over that fails meanwhile.
    failed: u64,
    /// Its stop done: once, as its session ends.
    stop_done: bool,
    /// When a flush of its last failed at its deadline: those out with it
    /// fail then too.
    flush_failed_at: Option<u64>,
    /// Its asks of a cs out of a set, each by its id: one asked again goes
    /// by the same.
    downs_asked: BTreeSet<u64>,
    /// While its writes are starved of memory, until then (#97): and the
    /// new asks out it made meanwhile.
    starved_until: u64,
    asked_starved: u64,
    /// The generation it last came to serve at: a newer one is its own
    /// attach anew.
    served_gen: u64,
    /// Its attaches anew for a write failed at its deadline, counted.
    anew: u64,
    /// The generation a write of its failed at its deadline at: no close of
    /// that generation's is to go after it -- the session attaches anew,
    /// its reconcile to make the chunk's replicas the same.
    failed_write_gen: Option<u64>,
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
    /// The host lost for good at the end, once it is: the leader's look at
    /// a cs up is answered from then on, as nbs-cs's task answers it
    /// whatever it is doing (N12c) -- one taken out by a look that missed
    /// would leave a set's last active replica on the host lost, which the
    /// end's premise, every chunk with a replica in on another host, rules
    /// out.
    lost_for_good: Option<usize>,
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
    /// The leader's scrub (N9e), its asks and answers on their way -- each
    /// with its host and whether it is to the cs -- delivered by a
    /// randomness of their own, and its request numbers.
    scrub: Scrub,
    scrub_q: Vec<(usize, bool, Vec<u8>)>,
    scrub_r: Rng,
    scrub_id: u64,
    rotted: Vec<Rot>,
    /// The snapshots kept, and their randomness of their own (N10c); the
    /// next one's number; when the garbage passes run next.
    snaps: Vec<Snap>,
    snap_r: Rng,
    snap_next: u32,
    garbage_at: u64,
    /// A pause for a snapshot under way, a late snapshot to come, and the
    /// next pause's number (N10c2); the writer of a pause given up, and its
    /// generation then -- which it is to go on at, attaching again for none.
    pausing: Option<Pausing>,
    late: Option<Late>,
    pause_next: u64,
    given_up: Option<(usize, u64, u64)>,
    /// A host whose cs is cut off from every host but its own: nothing on
    /// its links to and from others delivered while it lasts -- its host
    /// up, so that the leader's look keeps it in.
    cut: Option<usize>,
    /// The word a pause's answer gets, as an end shaped it: 0 the snapshot
    /// applied late, 1 the word lost, 2 the word said.
    pause_word: Option<u64>,
    /// The clones' writers, one live at a time (N10d2); what the clone may
    /// hold -- its snapshot's blocks as they read, its writer's writes since
    /// -- and its randomness of its own.
    clones: Vec<CloneW>,
    cmodel: Blocks,
    clone_r: Rng,
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
        /* Closes let out a window at a time (N13b): never more places
           closing at once than the window */
        if self.clients[c].s.closing() > CLOSING_FUZZ {
            return Err(format!("client {} closing {} places at once, past the window of {}", c,
                               self.clients[c].s.closing(), CLOSING_FUZZ));
        }
        for o in out {
            match o {
                Out::Send { to: To::Mds, id, bytes } => {
                    if self.trace {
                        if let Some(a @ (Ask::CopyUp { .. } | Ask::CopyDone { .. })) = Asking::decode(&bytes).map(|a| a.ask) {
                            eprintln!("  [{}] client {} asks #{}: {:?}", self.step, c, id, a);
                        }
                    }
                    /* A cs asked out of a set only where the view writes it
                       there: one the view has out already -- read since, its
                       place another's -- would be asked about for as long as
                       the session lives */
                    if let Some(Ask::Down { set, cs, .. }) = Asking::decode(&bytes).map(|a| a.ask) {
                        if !self.clients[c].s.writes_in(set, cs) {
                            return Err(format!("client {} asked cs {} out of set {}, which its view does not \
                                                write it in -- {}", c, cs, set, self.clients[c].s.view()));
                        }
                        /* A new ask, not one asked again: an ask the leader
                           said not now to goes again by its id */
                        if self.clients[c].downs_asked.insert(id) {
                            if self.now < self.clients[c].starved_until {
                                self.clients[c].asked_starved += 1;
                            }
                            if self.trace {
                                eprintln!("  [{}] client {} asks cs {} of set {} out, at {} ms: {}", self.step, c, cs,
                                          set, self.now / MS, self.clients[c].s.report());
                            }
                        }
                    }
                    let h = self.clients[c].host;
                    self.push(End::Client(c), End::Mds(h), bytes, id);
                }
                Out::Send { to: To::Cs(cs), id, bytes } => {
                    let Some(h) = self.host_of_cs(cs) else { return Err(format!("a request to cs {}, none", cs)) };
                    /* A write of a chain (N13): its head, or one of its
                       replicas sent it directly, its chain slow */
                    if let Some((_, Some(relay))) = Request::decode_relayed(&bytes) {
                        self.count(if relay.next.is_empty() { "a chain's writes sent directly" } else { "chains begun" });
                    }
                    unmarked(self.clients[c].s.chains(), &bytes, &format!("client {}", c), id, cs)?;
                    if let (Some(g), Some(r)) = (self.clients[c].failed_write_gen, Request::decode(&bytes)) {
                        if r.generation == g && matches!(r.op, nbswire::Op::Close { .. }) {
                            return Err(format!("client {} closed a chunk at generation {} after a write of that \
                                                generation failed at its deadline: its replicas perhaps apart, \
                                                closed so for good", c, g));
                        }
                    }
                    /* Judged as sent: one sent before, while the view had the
                       cs in, may get there after */
                    if self.owed == Some((c, h)) && Request::decode(&bytes).is_some_and(|r| {
                        matches!(r.op, nbswire::Op::Flush)
                    }) {
                        self.count("flushes to a cs out of every set");
                    }
                    if self.trace {
                        let what: String = Request::decode_relayed(&bytes).map_or(String::from("?"), |(r, _)| match &r.op {
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
                    if line.contains("attached anew") {
                        self.clients[c].anew += 1;
                        self.count("writes failed at their deadline, the session attached anew");
                    }
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
        self.replicas_in(VOLUME, b, generation)
    }

    /// The same of block `b` of volume `name`'s map -- a snapshot's (N10c).
    fn replicas_in(&self, name: &str, b: u64, generation: u64) -> Option<(u32, ChunkRef, u32, Vec<usize>)> {
        let v = self.state.volume(name)?;
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
            if generation != 0 && store.found_open_by(v.id, c, ch.incarnation) == Some(generation) {
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
        nbscs::now(store.read(volume, 0, chunk, inside, &mut buf, true)).map_err(|e| format!("{:?}", e))?;
        let v = buf[0];
        if buf.iter().any(|&x| x != v) {
            return Err(format!("not one value: {:?}...", &buf[..8]));
        }
        Ok(v)
    }

    /// Whether a write of value `value` to blocks `first`+`n` of the volume,
    /// failed at its deadline, left their replicas apart: a block of it on
    /// some active replica up of its chunk and not on another -- what the
    /// session's attach anew is for. One that reached none of them, or each,
    /// left none apart, and neither did one whose value a block had before
    /// (unseen then, and so judged less, never wrongly). A replica found
    /// apart for another reason -- its unflushed writes lost in a crash --
    /// has not the value either way.
    fn apart(&mut self, first: u64, n: u64, value: u8) -> bool {
        for b in first..first + n {
            let Some((volume, chunk, inside, _)) = self.replicas(b, 0) else { continue };
            let Some(set) = self.state.sets().get(chunk.epoch.set as usize).cloned() else { continue };
            let mut has = BTreeSet::new();
            for i in 0..nbsmeta::REPLICAS {
                if set.replicas[i] != Replica::Active {
                    continue;
                }
                let Some(h) = self.host_of_cs(set.cs[i]) else { continue };
                if self.hosts[h].store.is_none() {
                    continue;
                }
                if let Ok(v) = self.block_on(h, volume, chunk, inside) {
                    has.insert(v == value);
                }
            }
            if has.len() > 1 {
                return true;
            }
        }
        false
    }

    /// Whether a flush or a close of `op` failing now, before its own
    /// deadline, is one the client fails with a flush before it that failed
    /// at its deadline -- out with it: the client's rule. `failed_at` is when
    /// a flush of the session's last failed at its deadline; `ops` what it
    /// has out still.
    fn failed_with_one_before(&self, op: &MOp, failed_at: Option<u64>, ops: &BTreeMap<u64, MOp>) -> bool {
        matches!(op.kind, K::Flush | K::Close)
            && (failed_at == Some(self.now)
                || ops.values().any(|o| matches!(o.kind, K::Flush | K::Close) && o.at <= op.at
                    && self.now >= o.at + DEADLINE_NS))
    }

    fn done(&mut self, c: usize, tag: u64, ok: bool) -> Result<(), String> {
        /* A stop: done once, and as its session ends -- the volume let go
           of, or the session failed: a driver tears its session down at it */
        if tag == STOP || tag == SNAP_STOP {
            let phase = self.clients[c].s.phase();
            if self.clients[c].stop_done || !matches!(phase, Phase::Detached | Phase::Failed(_))
                || ok != (*phase == Phase::Detached)
            {
                return Err(format!("client {}'s stop done {}{}, {:?}: {}", c, ok,
                                   if self.clients[c].stop_done { " a second time" } else { "" }, phase,
                                   self.clients[c].s.report()));
            }
            self.clients[c].stop_done = true;
            return Ok(());
        }
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
        let anew = self.clients[c].served
            && matches!(self.clients[c].s.phase(), Phase::Attaching | Phase::Fencing | Phase::Reconciling);
        let lost = self.clients[c].zombie || matches!(self.clients[c].s.phase(), Phase::Failed(_)) || anew;
        if !ok && op.kind != K::Read {
            self.clients[c].failed += 1;
        }
        /* A write failed at its deadline that left its replicas apart: the
           session attaches anew, closing nothing of that generation after
           it (N13c). One that reached none -- held all its life for a
           copy-up a stuck join held back -- fails as any other */
        if let K::Write(value) = op.kind {
            if !ok && self.now >= op.at + DEADLINE_NS && !self.clients[c].zombie && self.apart(first, n, value) {
                self.clients[c].failed_write_gen = Some(generation);
                self.count("writes failed at their deadline, their replicas apart");
            }
        }
        if !ok && matches!(op.kind, K::Flush | K::Close) && self.now >= op.at + DEADLINE_NS {
            self.clients[c].flush_failed_at = Some(self.now);
        }
        if !ok && !lost && self.now < op.at + DEADLINE_NS
            && !self.failed_with_one_before(&op, self.clients[c].flush_failed_at, &self.clients[c].ops)
        {
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

    /// A close of client `c`'s asked with nothing else out and done: every
    /// place written closed on every active replica up of its chunk -- none
    /// open at the session's generation. Its closes go a window at a time
    /// (nbsclient's `CLOSING_MAX`, two places here): one done when its first
    /// window was would leave the rest open (N13b).
    fn closed_all(&self, c: usize) -> Result<(), String> {
        let per = SLOT / BLOCK;
        let generation = self.clients[c].s.generation();
        let places: BTreeSet<u64> = self.blocks.history.keys().map(|b| b / per).collect();
        let Some(v) = self.state.volume(VOLUME) else { return Ok(()) };
        for place in &places {
            let Some(&chunk) = v.map.get(*place as usize) else { continue };
            let Some(ch) = self.state.chunks().get(chunk as usize) else { continue };
            let Some(set) = self.state.sets().get(ch.set as usize) else { continue };
            for i in 0..nbsmeta::REPLICAS {
                if set.replicas[i] != Replica::Active {
                    continue;
                }
                let Some(h) = self.host_of_cs(set.cs[i]) else { continue };
                let Some(store) = self.hosts[h].store.as_ref() else { continue };
                /* One found open as its cs restarted is that cs's to refuse */
                if store.found_open(chunk, ch.incarnation).is_some() {
                    continue;
                }
                if store.holding(chunk).is_some_and(|x| {
                    x.incarnation == ch.incarnation && x.state == SlotState::Open && x.generation == generation
                }) {
                    return Err(format!("step {}: client {}'s close done, nothing else out, and place {}'s chunk {} \
                                        open on host {} at generation {}", self.step, c, place, chunk,
                                       self.hosts[h].name, generation));
                }
            }
        }
        Ok(())
    }

    /// Every replica the metadata has in, of every block written, alike.
    fn same(&mut self, generation: u64, when: &str) -> Result<(), String> {
        self.same_of(generation, when, false)
    }

    /// `same`, but for the blocks a write is out to: a session come to
    /// serve again sends at once what was asked while it attached anew.
    fn same_settled(&mut self, generation: u64, when: &str) -> Result<(), String> {
        self.same_of(generation, when, true)
    }

    fn same_of(&mut self, generation: u64, when: &str, settled: bool) -> Result<(), String> {
        let blocks: Vec<u64> = self.blocks.history.iter()
            .filter(|(_, h)| !settled || !h.iter().any(|w| w.2 == WState::Out))
            .map(|(&b, _)| b).collect();
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

    /// Whether the link from `from` to `to` is cut: one end the cut host's
    /// cs, the other not its host's mds.
    fn is_cut(&self, from: End, to: End) -> bool {
        let Some(x) = self.cut else { return false };
        match (from, to) {
            (End::Cs(h), End::Mds(m)) | (End::Mds(m), End::Cs(h)) if h == x => m != x,
            (End::Cs(h), _) | (_, End::Cs(h)) => h == x,
            _ => false,
        }
    }

    /// One message delivered, of a pair picked at random.
    fn deliver(&mut self) -> Result<(), String> {
        let keys: Vec<(End, End)> = self.links.iter().filter(|(&(f, t), q)| !q.is_empty() && !self.is_cut(f, t))
            .map(|(&k, _)| k).collect();
        let Some(&(from, to)) = self.r.pick(&keys).as_ref() else { return Ok(()) };
        let Some(m) = self.links.get_mut(&(from, to)).and_then(|q| q.pop_front()) else { return Ok(()) };
        match (from, to) {
            (End::Client(_) | End::Clone(_), End::Cs(h)) => self.at_cs(from, h, m),
            /* A write passed along its chain (N13) */
            (End::Cs(_), End::Cs(h)) => self.at_cs(from, h, m),
            /* A cs's question to its host's mds, and the answer (N12c) */
            (End::Cs(h), End::Mds(_)) => self.mds_for_cs(h, m),
            (End::Mds(_), End::Cs(h)) => self.told_cs(h, m),
            (End::Client(_) | End::Clone(_), End::Mds(_)) => self.at_mds(from, m),
            /* A clone's writer (N10d2), as a client */
            (End::Cs(_), End::Clone(k)) | (End::Mds(_), End::Clone(k)) if self.clones[k].dead => Ok(()),
            (End::Cs(_) | End::Mds(_), End::Clone(k)) => {
                let now = self.now;
                let mut out = Vec::new();
                let cl = &mut self.clones[k];
                if matches!(from, End::Cs(_)) {
                    cl.s.on_cs(&m.bytes, now, &mut cl.bufs, &mut out);
                } else {
                    cl.s.on_mds(&m.bytes, now, &mut cl.bufs, &mut out);
                }
                self.clone_outs(k, out)
            }
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
        self.at_cs(End::Copy(j), s, m)
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
                nbscs::now(c.answer(store, a.id, &a.reply, now, &mut out));
                nbscs::now(c.step(store, now, copy_id, &mut out));
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
        /* Each view's fences held to the metadata: every volume a writer has
           had with a chunk in the set, at its newest generation (N13) */
        for v in &views {
            for vol in self.state.volumes().iter() {
                let has = vol.map.iter().any(|&c| {
                    self.state.chunks().get(c as usize).is_some_and(|ch| !ch.is_free() && ch.set == v.set)
                });
                if vol.generation != 0 && has && !v.fences.contains(&(vol.id, vol.generation)) {
                    return Err(format!("host {}'s join of set {}: volume {} at generation {} not among its fences {:?}",
                                       name, v.set, vol.id, vol.generation, v.fences));
                }
            }
        }
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
            /* Fenced at its volumes' newest generations first, as nbs-cs
               fences (N13); and held to it */
            if !nbscs::now(copy::fence_for(store, &v)) {
                continue;
            }
            if let Some(&(volume, g)) = v.fences.iter().find(|&&(volume, g)| store.fence_of(volume) < g) {
                return Err(format!("host {}, joining set {}, fenced at {} for volume {}, the newest writer's {}",
                                   name, v.set, store.fence_of(volume), volume, g));
            }
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
                nbscs::now(c.step(store, now, copy_id, &mut out));
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

    /// The leader's scrub's turn (N9e): what it asks queued, and a few of
    /// its asks and answers on their way delivered, by its own randomness.
    fn scrub_turn(&mut self) -> Result<(), String> {
        let now = self.now;
        /* What the scrub found taken down, as a person would, a while on */
        for k in 0..self.rotted.len() {
            let due = self.rotted[k].found.is_some_and(|at| now >= at + TAKE_DOWN_AFTER) && !self.rotted[k].taken;
            if !due {
                continue;
            }
            self.rotted[k].taken = true;
            let set = self.rotted[k].set;
            for h in self.rotted[k].hosts.clone() {
                let cs = self.hosts[h].cs;
                let version = self.state.sets().get(set as usize).map_or(0, |x| x.version);
                let cmd = Command::ReplicaBad { set, cs, version };
                let answer = self.apply(cmd.clone());
                if self.trace {
                    eprintln!("  [{}] {:?}, as a person would: {:?}", self.step, cmd, answer);
                }
                if let Answer::Refused(why) = answer {
                    return Err(format!("{:?}, of what the scrub found, refused: {:?}", cmd, why));
                }
            }
            self.count("rotted copies the scrub found");
        }
        let mut out = Vec::new();
        self.scrub.tick(&self.state, now, &mut self.scrub_id, &mut out);
        self.scrub_out(out)?;
        /* One at a time, now and then: a member's answer may wait through a
           pause of the network's while the writer writes and closes the
           chunk, which a look's answers taken at one instant would never
           show */
        for _ in 0..u64::from(self.scrub_r.chance(15)) {
            if self.scrub_q.is_empty() {
                break;
            }
            let k = self.scrub_r.below(self.scrub_q.len() as u64) as usize;
            let (h, to_cs, bytes) = self.scrub_q.swap_remove(k);
            if to_cs {
                let World { hosts, state, index, scrub_q, .. } = self;
                let host = &mut hosts[h];
                let Some(store) = host.store.as_mut() else { continue };
                let Some(req) = Request::decode(&bytes) else { continue };
                let (name, applied) = (host.name, *index);
                let ask = AskNow::new(|q| Some(nbswire::mds::told(state, name, applied, &q)));
                let answer = match nbscs::now(serve(store, &host.vouched, &req, &ask)) {
                    Ok(done) => CsAnswer { id: req.id, reply: done.reply() }.encode(),
                    Err(why) => CsAnswer { id: req.id, reply: Reply::Refused(why) }.encode(),
                };
                scrub_q.push((h, false, answer));
            } else {
                let Some(a) = CsAnswer::decode(&bytes) else { continue };
                let mut out = Vec::new();
                self.scrub.answer(a.id, &a.reply, &self.state, now, &mut out);
                self.scrub_out(out)?;
            }
        }
        Ok(())
    }

    /// What the scrub asks done: its requests queued for their hosts, a
    /// member it found bad taken down -- only one whose copy the target
    /// rotted -- and its lines said.
    fn scrub_out(&mut self, out: Vec<scrub::Out>) -> Result<(), String> {
        for o in out {
            match o {
                scrub::Out::Send { host, bytes } => {
                    let Some(h) = self.hosts.iter().position(|x| x.name == host) else {
                        return Err(format!("the scrub asked host {}, none", host));
                    };
                    if self.hosts[h].store.is_some() {
                        self.scrub_q.push((h, true, bytes));
                    }
                }
                scrub::Out::Found { chunk, set, version, odd } => {
                    /* Of the set as it is: a look the set moved on under is
                       no finding of now */
                    let now_at = self.state.sets().get(set as usize).map(|s| s.version);
                    if now_at != Some(version) {
                        return Err(format!("the scrub found chunk {} of set {} at version {}, the set at {:?}", chunk, set,
                                           version, now_at));
                    }
                    /* Only what the target rotted, said once: a member rotted
                       alone the one unlike the two others, two rotted no two
                       alike */
                    let at = self.rotted.iter().position(|x| x.chunk == chunk && x.set == set && !x.taken);
                    let right = at.is_some_and(|k| {
                        let r = &self.rotted[k];
                        r.found.is_none() && match r.hosts.as_slice() {
                            [h] => odd.and_then(|cs| self.host_of_cs(cs)) == Some(*h),
                            _ => odd.is_none(),
                        }
                    });
                    let (Some(k), true) = (at, right) else {
                        let copies = self.copies_of_set(set);
                        return Err(format!("the scrub found chunk {} of set {} at version {} unlike on its members (odd \
                                            {:?}), which the target did not rot so, or found it again -- the members' \
                                            copies, granules unlike the first's: {}; rotted {:?}", chunk, set, version,
                                           odd, copies,
                                           self.rotted.iter().map(|x| (x.hosts.iter().map(|&h| self.hosts[h].name)
                                                                       .collect::<Vec<_>>(), x.chunk, x.set, x.found))
                                               .collect::<Vec<_>>()));
                    };
                    self.rotted[k].found = Some(self.now);
                }
                scrub::Out::Say(line) => {
                    if self.trace {
                        eprintln!("  [{}] {}", self.step, line);
                    }
                }
            }
        }
        Ok(())
    }

    /// Set `set`'s members' copies of each chunk of the volume in it, for a
    /// finding: by member, its record and the granules unlike the first
    /// member's.
    fn copies_of_set(&mut self, set: u32) -> String {
        let Some(v) = self.state.volume(VOLUME) else { return String::new() };
        let Some(st) = self.state.sets().get(set as usize).cloned() else { return String::new() };
        let chunks: Vec<u32> = v.map.iter().copied()
            .filter(|&c| self.state.chunks().get(c as usize).is_some_and(|ch| ch.set == set)).collect();
        let mut said = Vec::new();
        for c in chunks {
            let Some(ch) = self.state.chunks().get(c as usize) else { continue };
            let chunk = ChunkRef { chunk: c, incarnation: ch.incarnation, epoch: Epoch { set, version: st.version } };
            let mut first: Option<Vec<u32>> = None;
            for i in 0..nbsmeta::REPLICAS {
                let cs = st.cs[i];
                let Some(h) = self.host_of_cs(cs) else { continue };
                let got = self.hosts[h].store.as_mut().map(|x| nbscs::now(x.scrub(chunk)));
                let line = match got {
                    Some(Ok((open, written, sums))) => {
                        let differ: Vec<usize> = match &first {
                            Some(f) => (0..sums.len()).filter(|&k| f.get(k) != sums.get(k)).collect(),
                            None => Vec::new(),
                        };
                        if first.is_none() {
                            first = Some(sums);
                        }
                        format!("cs {} ({:?}) open {} written v{} differs at {:?}", cs, st.replicas[i], open, written,
                                differ)
                    }
                    Some(Err(e)) => format!("cs {}: {:?}", cs, e),
                    None => format!("cs {}: down", cs),
                };
                said.push(format!("chunk {} {}", c, line));
            }
        }
        said.join("; ")
    }

    /// A sector of a chunk closed on its every member rotted on one of them
    /// (N9e), a host up and not drained: what the scrub is to find. Whether
    /// there was one to rot.
    fn rot_one(&mut self, r: &mut Rng) -> bool {
        let Some(v) = self.state.volume(VOLUME) else { return false };
        let Some(layout) = nbscs::layout(DISK_SLOTS * SLOT) else { return false };
        let mut can = Vec::new();
        for &c in v.map.iter() {
            let Some(ch) = self.state.chunks().get(c as usize) else { continue };
            let Some(set) = self.state.sets().get(ch.set as usize) else { continue };
            if set.joiner.is_some() || set.replicas.iter().any(|&x| x != Replica::Active) {
                continue;
            }
            let hosts: Vec<usize> = set.cs.iter().filter_map(|&cs| self.host_of_cs(cs)).collect();
            let closed = hosts.len() == nbsmeta::REPLICAS && hosts.iter().all(|&h| {
                self.hosts[h].store.as_ref().and_then(|st| st.holding(c))
                    .is_some_and(|x| x.incarnation == ch.incarnation && x.state == nbscs::SlotState::Written)
            });
            if !closed {
                continue;
            }
            for &h in &hosts {
                if Some(h) != self.drained {
                    can.push((h, c, ch.set));
                }
            }
        }
        let Some((h, c, set)) = r.pick(&can) else { return false };
        /* Now and then a second member of the set rotted too, elsewhere in
           the chunk: no two alike then */
        let mut hosts = vec![h];
        if r.chance(25) {
            let others: Vec<usize> = can.iter().filter(|&&(x, cc, _)| cc == c && x != h).map(|&(x, _, _)| x).collect();
            if let Some(g) = r.pick(&others) {
                hosts.push(g);
            }
        }
        for (n, &h) in hosts.iter().enumerate() {
            let Some(slot) = self.hosts[h].store.as_ref().and_then(|st| st.holding(c)).map(|x| x.slot) else {
                return false;
            };
            /* The second in the chunk's other half: never the same sector */
            let half = SLOT / DISK_SECTOR / 2;
            let k = n as u64 * half + r.below(half);
            let at = layout.data + u64::from(slot) * SLOT + k * DISK_SECTOR;
            let bit = r.below(DISK_SECTOR * 8);
            self.hosts[h].disk.borrow_mut().flip(at / DISK_SECTOR, bit);
            if self.trace {
                eprintln!("  [end] chunk {} rotted on host {} (cs {}) at {} of its slot {}", c, self.hosts[h].name,
                          self.hosts[h].cs, k * DISK_SECTOR, slot);
            }
        }
        if self.trace {
            let copies = self.copies_of_set(set);
            eprintln!("  [end] the set's copies now: {}", copies);
        }
        self.count(if hosts.len() == 1 { "chunks rotted on a member" } else { "chunks rotted on two members" });
        self.rotted.push(Rot { hosts, chunk: c, set, found: None, taken: false });
        true
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
        while !self.sets_whole() || self.rotted.iter().any(|x| !x.taken) {
            if self.now >= until {
                if let Some(x) = self.rotted.iter().find(|x| !x.taken) {
                    return Err(format!("chunk {} rotted on {:?}: not found by the scrub {} s after the end -- {:?}",
                                       x.chunk, x.hosts.iter().map(|&h| self.hosts[h].name).collect::<Vec<_>>(),
                                       SETTLE_NS / 1_000_000_000, self.scrub.stats));
                }
                return Err(format!("the sets not at three {} s after the end: {}", SETTLE_NS / 1_000_000_000,
                                   self.sets_said()));
            }
            /* Time skips ahead only where nothing is on its way -- the scrub's
               asks and answers neither: those wait their turn tick by tick,
               and a look of a dozen ticks across skips timed out unanswered,
               every look of the quiet end let be (found once N10c's
               snapshots made the rounds longer than the end's wait) */
            self.now += 5 * MS;
            if self.links.values().all(|q| q.is_empty()) && self.scrub_q.is_empty() {
                self.now += 100 * MS;
            }
            self.deliver()?;
            self.tick()?;
        }
        self.count("ends with every set at three");
        Ok(())
    }

    /// Time on until every set is whole and nothing is on its way, or
    /// SETTLE_NS on: whether it came to that.
    fn quiet(&mut self) -> Result<bool, String> {
        let until = self.now + SETTLE_NS;
        while !(self.sets_whole() && self.links.values().all(|q| q.is_empty()) && self.scrub_q.is_empty()) {
            if self.now >= until {
                return Ok(false);
            }
            self.now += 5 * MS;
            self.deliver()?;
            self.tick()?;
        }
        Ok(true)
    }

    /// The session of `e` -- a client, or a clone's writer -- with its
    /// buffers, and its host.
    fn session_of(&mut self, e: End) -> Option<(&mut Session, &mut Bufs, usize)> {
        match e {
            End::Client(c) => self.clients.get_mut(c).map(|cl| (&mut cl.s, &mut cl.bufs, cl.host)),
            End::Clone(k) => self.clones.get_mut(k).map(|cl| (&mut cl.s, &mut cl.bufs, cl.host)),
            _ => None,
        }
    }

    /// The outputs of `e`'s session, taken as a client's or a clone's.
    fn outs_of(&mut self, e: End, out: Vec<Out>) -> Result<(), String> {
        match e {
            End::Client(c) => self.outs(c, out),
            End::Clone(k) => self.clone_outs(k, out),
            _ => Ok(()),
        }
    }

    /// A request at host `h`'s cs: done by nbswire's `serve`, the cs asking
    /// the mds as its own host's, over the network (N12c) -- many in flight
    /// at once, each answered as it ends; or, its host down, lost or
    /// bounced.
    fn at_cs(&mut self, from: End, h: usize, m: Msg) -> Result<(), String> {
        let host = &mut self.hosts[h];
        let Some(store) = host.store.clone() else {
            if self.r.chance(50) {
                self.count("bounces");
                let now = self.now;
                let mut out = Vec::new();
                if let Some((s, bufs, _)) = self.session_of(from) {
                    s.on_bounce(m.id, true, now, bufs, &mut out);
                }
                return self.outs_of(from, out);
            }
            return Ok(());
        };
        let (vouched, asks) = (host.vouched.clone(), host.asks.clone());
        /* A write passed along its chain: on to the next replica as it is
           taken, before its own write is done (N13) */
        let mut on = None;
        if let Some((req, Some(relay))) = Request::decode_relayed(&m.bytes) {
            if let Some((next, bytes)) = nbswire::relay_on(&req, &relay) {
                on = Some((next, bytes, relay.next[0].id));
            }
        }
        if self.trace {
            let what: String = Request::decode_relayed(&m.bytes).map_or(String::from("none"), |(r, relay)| {
                let along = relay.map_or(String::new(), |x| format!(" along a chain, mark {}, {} on", x.mark,
                                                                      x.next.len()));
                let op: String = match &r.op {
                    nbswire::Op::Write { chunk, offset, data, .. } => {
                        format!("write chunk {} at {} +{}, epoch {}.{}", chunk.chunk, offset, data.len(),
                                chunk.epoch.set, chunk.epoch.version)
                    }
                    other => format!("{:?}", other).chars().take(110).collect(),
                };
                format!("#{}{} {}", r.id, along, op)
            });
            eprintln!("  [{}] cs {} (host {}) gets from {:?}: {}", self.step, host.cs, host.name, from, what);
        }
        let bytes = m.bytes.clone();
        let fut = Box::pin(async move {
            let Some((req, relay)) = Request::decode_relayed(&bytes) else { return Err(Refusal::Bad) };
            serve_relayed(&store, &vouched, &req, relay.as_ref(), &*asks).await
        });
        host.flights.push(Flight { from, msg: m.id, bytes: m.bytes, fut });
        if let Some((next, bytes, id)) = on {
            let Some(to) = self.hosts.iter().position(|x| x.name == next) else {
                return Err(format!("a write passed on to host {}, none", next));
            };
            self.count("writes passed on along a chain");
            self.push(End::Cs(h), End::Cs(to), bytes, id);
        }
        self.poll_cs(h)
    }

    /// Host `h`'s cs's requests in flight polled: those that end answered,
    /// the questions asked on their way.
    fn poll_cs(&mut self, h: usize) -> Result<(), String> {
        let now = self.now;
        let host = &mut self.hosts[h];
        host.asks.now.set(now);
        let mut cx = Context::from_waker(Waker::noop());
        let mut ended = Vec::new();
        /* Again while one ends: its chunk's next may go on */
        loop {
            let before = ended.len();
            let mut k = 0;
            while k < host.flights.len() {
                match host.flights[k].fut.as_mut().poll(&mut cx) {
                    Poll::Ready(result) => {
                        let f = host.flights.remove(k);
                        ended.push((f.from, f.msg, f.bytes, result));
                    }
                    Poll::Pending => k += 1,
                }
            }
            if ended.len() == before {
                break;
            }
        }
        let collected = match host.garbage.as_mut().map(|g| g.as_mut().poll(&mut cx)) {
            Some(Poll::Ready(done)) => {
                host.garbage = None;
                Some(done)
            }
            _ => None,
        };
        /* The pass's frees may let a request of their chunk go on */
        if collected.is_some() {
            let mut k = 0;
            while k < host.flights.len() {
                match host.flights[k].fut.as_mut().poll(&mut cx) {
                    Poll::Ready(result) => {
                        let f = host.flights.remove(k);
                        ended.push((f.from, f.msg, f.bytes, result));
                    }
                    Poll::Pending => k += 1,
                }
            }
        }
        let asked = std::mem::take(&mut *host.asks.outbox.borrow_mut());
        for (id, question) in asked {
            self.push(End::Cs(h), End::Mds(h), Query { id, question }.encode(), id);
        }
        if let Some(done) = collected {
            let name = self.hosts[h].name;
            if let Some((n, e)) = done.failed.first() {
                return Err(format!("host {}'s cs did not let go of {}: {:?}", name, n, e));
            }
            if self.trace && !done.freed.is_empty() {
                eprintln!("  [{}] host {} frees {:?}", self.step, name, done.freed);
            }
            *self.stats.entry("chunks freed by a garbage pass").or_default() += done.freed.len() as u64;
        }
        if ended.len() > 1 {
            self.count("requests ended together at a cs");
        }
        for (from, msg, bytes, result) in ended {
            self.served_at_cs(from, h, msg, &bytes, result)?;
        }
        Ok(())
    }

    /// A cs's question at its host's mds: answered as the state is now, the
    /// answer on its way back.
    fn mds_for_cs(&mut self, h: usize, m: Msg) -> Result<(), String> {
        let Some(q) = Query::decode(&m.bytes) else { return Err(String::from("a cs's question that is none")) };
        let told = nbswire::mds::told(&self.state, self.hosts[h].name, self.index, &q.question);
        self.count("cs questions answered");
        self.push(End::Mds(h), End::Cs(h), Answered { id: q.id, told }.encode(), q.id);
        Ok(())
    }

    /// The mds's answer at the cs that asked: handed to its question, and
    /// the requests in flight polled.
    fn told_cs(&mut self, h: usize, m: Msg) -> Result<(), String> {
        if self.hosts[h].store.is_none() {
            return Ok(());
        }
        let Some(a) = Answered::decode(&m.bytes) else { return Err(String::from("an mds's answer that is none")) };
        self.hosts[h].asks.told(a.id, a.told);
        self.poll_cs(h)
    }

    /// A request ended at host `h`'s cs, `result`: held to the fence, and
    /// answered to `from`.
    fn served_at_cs(&mut self, from: End, h: usize, msg: u64, bytes: &[u8], result: Result<Done, Refusal>)
        -> Result<(), String> {
        /* A write passed along its chain answers the session it names */
        let relayed = Request::decode_relayed(bytes).and_then(|(_, r)| r.map(|r| named_end(r.answer)));
        let from = match relayed {
            Some(Some(end)) => end,
            Some(None) => return Err(String::from("a write passed on names no session of the target's")),
            None => from,
        };
        let World { hosts, state, served, .. } = self;
        let host = &hosts[h];
        let name = host.name;
        let answer = match Request::decode_relayed(bytes).map(|(r, _)| r) {
            None => CsAnswer { id: 0, reply: Reply::Refused(nbswire::Refusal::Bad) }.encode(),
            Some(req) => {
                if self.trace {
                    let what = match &req.op {
                        nbswire::Op::Write { chunk, offset, data, .. } => {
                            format!("write chunk {} at {} +{}", chunk.chunk, offset, data.len())
                        }
                        other => format!("{:?}", other).chars().take(60).collect(),
                    };
                    let said = match &result {
                        Ok(nbswire::serve::Done::Data(d)) => format!("data {} bytes", d.len()),
                        Ok(d) => format!("{:?}", d).chars().take(60).collect(),
                        Err(e) => format!("refused {:?}", e),
                    };
                    eprintln!("  [{}] cs {} (host {}) for {:?} at generation {}: #{} {} -> {}", self.step,
                              host.cs, name, from, req.generation, req.id, what, said);
                }
                /* Fenced: once a newer writer serves, no active replica does
                   an older generation's write or flush -- of the volume's
                   writers: a clone's generations are its own */
                if matches!(from, End::Client(_)) && result.is_ok() && req.generation != 0 && req.generation < *served {
                    let active = |set: u32| state.sets().get(set as usize).is_some_and(|s| {
                        (0..nbsmeta::REPLICAS).any(|i| s.cs[i] == host.cs && s.replicas[i] != Replica::Down)
                    });
                    let of_volume = |set: u32| state.volume(VOLUME).is_some_and(|v| v.map.iter().any(|&c| {
                        state.chunks().get(c as usize).is_some_and(|ch| ch.set == set)
                    }));
                    let wrong = match &req.op {
                        nbswire::Op::Write { chunk, .. } | nbswire::Op::CopyUp { to: chunk, .. } => {
                            active(chunk.epoch.set)
                        }
                        nbswire::Op::Flush => (0..state.sets().len() as u32).any(|s| active(s) && of_volume(s)),
                        _ => false,
                    };
                    if wrong {
                        return Err(format!("cs {} (host {}), an active replica, did a {} of generation {} once \
                                            generation {} serves", host.cs, name,
                                           match req.op {
                                               nbswire::Op::Flush => "flush",
                                               nbswire::Op::CopyUp { .. } => "copy-up",
                                               _ => "write",
                                           },
                                           req.generation, served));
                    }
                }
                match result {
                    Ok(done) => CsAnswer { id: req.id, reply: done.reply() }.encode(),
                    Err(why) => CsAnswer { id: req.id, reply: Reply::Refused(why) }.encode(),
                }
            }
        };
        self.push(End::Cs(h), from, answer, msg);
        Ok(())
    }

    /// A client's ask at its host's mds, answered as nbs-mds's member does --
    /// the leader's look at a cs being whether its host is up.
    fn at_mds(&mut self, from: End, m: Msg) -> Result<(), String> {
        let Some(a) = Asking::decode(&m.bytes) else { return Ok(()) };
        let Some(at) = self.session_of(from).map(|(_, _, h)| h) else { return Ok(()) };
        let host = HOSTS[at].to_string();
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
                if up && !refused && (self.lost_for_good.is_some() || self.cut.is_some() || !self.r.chance(10)) {
                    self.count("downs refused: the cs answers the leader");
                    Said::NotNow(String::from("the cs answers the leader"))
                } else {
                    let answer = self.apply(Command::ReplicaDown { set, cs, refused, name, generation });
                    if self.trace {
                        eprintln!("  [{}] mds: {:?} asks cs {} of set {} out (refused {}): {:?}", self.step, from, cs,
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
            /* A writer's own attach anew, a pause with no word (N10c2) */
            Ask::Reattach { name, generation, token } => {
                match self.apply(Command::Reattach { name, host, generation, token }) {
                    Answer::Attached { generation } => {
                        self.count("writers attached anew");
                        Said::Attached { generation, index: self.index }
                    }
                    Answer::Refused(why) => {
                        self.count("writers attached anew refused");
                        Said::Refused(why.says().to_string())
                    }
                    other => Said::Refused(format!("{:?}", other)),
                }
            }
            /* A pause answered (N10c2) */
            /* A pause answered: of the volume's writers alone, whom the
               writer's host's mds pauses */
            Ask::Paused { generation, pause, .. } => match from {
                End::Client(c) => return self.paused(c, a.id, generation, pause),
                _ => return Err(format!("{:?} answered a pause it was never asked", from)),
            },
            /* A copy-up's asks (N10), answered as nbs-mds's member does */
            ask @ (Ask::CopyUp { .. } | Ask::CopyDone { .. }) => {
                let done = matches!(ask, Ask::CopyDone { .. });
                /* A copy-up's end that moves the map on -- asked again once
                   it has, the writes held for it may be out to the new chunk */
                let ended = match &ask {
                    Ask::CopyDone { name, place, chunk, .. } if self.state.volume(name)
                        .and_then(|v| v.map.get(*place as usize)).is_some_and(|&c| c != *chunk) => {
                        Some((name.clone(), *place))
                    }
                    _ => None,
                };
                let asked = match &ask {
                    Ask::CopyUp { name, place, generation } => format!("{} place {} at {}", name, place, generation),
                    Ask::CopyDone { name, place, generation, chunk, incarnation } => {
                        format!("{} place {} at {}, chunk {}.{}", name, place, generation, chunk, incarnation)
                    }
                    _ => String::new(),
                };
                let cmd = match ask {
                    Ask::CopyUp { name, place, generation } => Command::CopyUp { name, place, generation },
                    Ask::CopyDone { name, place, generation, chunk, incarnation } => {
                        Command::CopyDone { name, place, generation, chunk, incarnation }
                    }
                    _ => return Ok(()),
                };
                let answer = self.apply(cmd);
                if self.trace {
                    eprintln!("  [{}] mds: {:?} asks a copy-up{} (#{}: {}): {:?}", self.step, from,
                              if done { "'s end" } else { "" }, a.id, asked, answer);
                }
                match answer {
                    Answer::Chunk { chunk, incarnation } => {
                        if let Some((name, place)) = &ended {
                            self.copied_alike(name, *place)?;
                        }
                        self.count(if done { "copy-ups done" } else { "copy-ups asked" });
                        Said::Chunk { chunk, incarnation, index: self.index }
                    }
                    Answer::Refused(why @ (nbsmeta::Refusal::Joining | nbsmeta::Refusal::Draining)) => {
                        self.count(if why == nbsmeta::Refusal::Joining {
                            "copy-ups asked while the set is joined"
                        } else {
                            "copy-ups asked while a member is drained"
                        });
                        Said::NotNow(why.says().to_string())
                    }
                    Answer::Refused(nbsmeta::Refusal::NotTheWriter) => Said::NotTheWriter,
                    Answer::Refused(why) => {
                        self.count(if done { "copy-ups' ends refused" } else { "copy-ups refused" });
                        Said::Refused(why.says().to_string())
                    }
                    other => Said::Refused(format!("{:?}", other)),
                }
            }
        };
        self.push(End::Mds(at), from, Saying { id: a.id, said }.encode(), m.id);
        Ok(())
    }

    /// A copy-up of volume `name`'s place `place` done (N10c): every block
    /// written there read alike on every member of its set active now, its
    /// host up, and as one of what the volume could hold there -- the new
    /// chunk the map names now, which the writes held for it have not
    /// reached yet: a member that never copied reads zeros. The client ends one only once each member
    /// its view has active has copied: taken out, the rule left a clone's
    /// chunk all zeros where a member the view had come to since never
    /// copied, which its race came at one seed in a thousand; N13's chains
    /// moved that race past the gate's, so it is judged here, as the mds
    /// takes the end.
    fn copied_alike(&mut self, name: &str, place: u32) -> Result<(), String> {
        let per = SLOT / BLOCK;
        let model = if name == VOLUME { &self.blocks } else { &self.cmodel };
        let blocks: Vec<(u64, BTreeSet<u8>)> = model.history.keys().copied().filter(|&b| b / per == u64::from(place))
            .map(|b| {
                let mut may = model.readable(b, model.acked(b));
                may.extend(model.durable(b));
                (b, may)
            }).collect();
        for (b, may) in blocks {
            let Some((volume, chunk, inside, _)) = self.replicas_in(name, b, 0) else { continue };
            let Some(set) = self.state.sets().get(chunk.epoch.set as usize).cloned() else { continue };
            let mut seen: Option<(usize, u8)> = None;
            for i in 0..nbsmeta::REPLICAS {
                if set.replicas[i] != Replica::Active {
                    continue;
                }
                let Some(h) = self.host_of_cs(set.cs[i]) else { continue };
                if self.hosts[h].store.is_none() {
                    continue;
                }
                let v = self.block_on(h, volume, chunk, inside)
                    .map_err(|e| format!("step {}: a copy-up's block {} on host {}: {}", self.step, b,
                                         self.hosts[h].name, e))?;
                if !may.contains(&v) {
                    return Err(format!("step {}: a copy-up of {}'s place {} ended, and its block {} is {} on host {} \
                                        -- an active member of set {} -- where the volume could hold {:?}", self.step,
                                       name, place, b, v, self.hosts[h].name, chunk.epoch.set, may));
                }
                match seen {
                    Some((g, w)) if w != v => {
                        return Err(format!("step {}: a copy-up of {}'s place {} ended, and its block {} is {} on host \
                                            {} and {} on host {} -- active members of set {} both", self.step, name,
                                           place, b, w, self.hosts[g].name, v, self.hosts[h].name, chunk.epoch.set));
                    }
                    _ => seen = Some((h, v)),
                }
            }
        }
        Ok(())
    }

    /* ---- what happens ---- */

    fn tick(&mut self) -> Result<(), String> {
        /* Each cs's requests in flight: a question unanswered in MDS_WAIT
           answered None */
        for h in 0..self.hosts.len() {
            if self.hosts[h].store.is_some() && (!self.hosts[h].flights.is_empty() || self.hosts[h].garbage.is_some()) {
                self.poll_cs(h)?;
            }
        }
        self.control()?;
        self.scrub_turn()?;
        self.garbage()?;
        self.pause_turn()?;
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
        /* The clones' writers (N10d2) */
        for k in 0..self.clones.len() {
            if self.clones[k].dead {
                continue;
            }
            let now = self.now;
            let mut out = Vec::new();
            let cl = &mut self.clones[k];
            cl.s.tick(now, &mut cl.bufs, &mut out);
            self.clone_outs(k, out)?;
        }
        Ok(())
    }

    /// Each cs up's garbage pass, as nbs-cs's (N10c): the chunks the
    /// metadata freed given back -- a copy-up's old chunk named by no map, a
    /// snapshot's dropped.
    fn garbage(&mut self) -> Result<(), String> {
        if self.now < self.garbage_at {
            return Ok(());
        }
        self.garbage_at = self.now + GARBAGE_EVERY;
        for h in 0..self.hosts.len() {
            let host = &mut self.hosts[h];
            /* One pass at a time: one still waiting for its mds, or for a
               chunk's request, goes on */
            let Some(store) = host.store.clone().filter(|_| host.garbage.is_none()) else { continue };
            let (requests, asks) = (host.vouched.clone(), host.asks.clone());
            host.garbage = Some(Box::pin(async move { collect(&store, &requests, &*asks).await }));
            self.poll_cs(h)?;
        }
        Ok(())
    }

    /* ---- snapshots (N10c) ---- */

    /// A snapshot of the volume between two writers: the writer stopped --
    /// what it was asked done, a flush, its chunks closed, the volume let go
    /// of -- the snapshot taken, the oldest dropped first where two are
    /// kept, held to at once; a writer started again by force on a host up.
    fn snapshot(&mut self) -> Result<(), String> {
        let Some(c) = self.current().filter(|&c| self.serving(c)) else { return Ok(()) };
        let mut out = Vec::new();
        let now = self.now;
        {
            let cl = &mut self.clients[c];
            cl.s.stop(SNAP_STOP, now, &mut cl.bufs, &mut out);
        }
        self.outs(c, out)?;
        self.drive(c, &|w, c| matches!(w.clients[c].s.phase(), Phase::Detached | Phase::Failed(_)))?;
        self.clients[c].zombie = true;
        match self.clients[c].s.phase() {
            Phase::Detached => self.stop_said(c)?,
            /* As it may -- check_failed said so: no snapshot, another writer */
            Phase::Failed(_) => return self.stop_said(c),
            _ => return Err(format!("client {} not stopped for a snapshot: {}", c, self.clients[c].s.report())),
        }
        if self.trace {
            eprintln!("  [{}] client {} stopped for a snapshot", self.step, c);
        }
        let may = self.snapshot_may();
        match self.take_snapshot(0, may, "a snapshot taken")? {
            Answer::Volume { .. } => {}
            other => return Err(format!("a snapshot of the volume, let go of, answered {:?}", other)),
        }
        let up: Vec<usize> = (0..HOSTS.len()).filter(|&h| Some(h) != self.down).collect();
        match self.snap_r.pick(&up) {
            Some(h) => self.writer(h),
            None => Ok(()),
        }
    }

    /// What each block written may hold in a snapshot taken now: the last
    /// write done, or one failed or lost since.
    fn snapshot_may(&self) -> BTreeMap<u64, BTreeSet<u8>> {
        self.blocks.history.keys().map(|&b| {
            let mut m = self.blocks.failed_since_done(b);
            m.insert(self.blocks.acked(b).1);
            (b, m)
        }).collect()
    }

    /// The snapshot `name` applied at `generation`, as `may` holds: kept --
    /// the oldest dropped first where two are -- and held to at once.
    fn take_snapshot(&mut self, generation: u64, may: BTreeMap<u64, BTreeSet<u8>>, when: &str)
        -> Result<Answer, String> {
        if self.snaps.len() >= SNAPS_KEPT {
            self.drop_snapshot(0)?;
        }
        let name = format!("s{}", self.snap_next);
        self.snap_next += 1;
        let answer = self.apply(Command::Snapshot { volume: String::from(VOLUME), name: name.clone(), generation });
        if self.trace {
            eprintln!("  [{}] snapshot {} of the volume at generation {} ({}): {:?}", self.step, name, generation, when,
                      answer);
        }
        if let Answer::Volume { .. } = answer {
            self.snaps.push(Snap { name, may, seen: BTreeMap::new() });
            self.count("snapshots taken");
            self.snapshots_hold(when)?;
        }
        Ok(answer)
    }

    /// Client `c`'s stop done, its session over: what a driver waits for.
    fn stop_said(&self, c: usize) -> Result<(), String> {
        if self.clients[c].stop_done {
            return Ok(());
        }
        Err(format!("client {} over, {:?}, and its stop never done", c, self.clients[c].s.phase()))
    }

    /// The writer paused for a snapshot, as the writer's host's mds asks it
    /// (N10c2): one pause at a time.
    fn pause_writer(&mut self) -> Result<(), String> {
        if self.pausing.is_some() || self.late.is_some() {
            return Ok(());
        }
        /* Not of a session still paused from before -- its word lost, it is
           to attach again of that one */
        let Some(c) = self.current().filter(|&c| self.serving(c) && !self.clients[c].s.paused()) else { return Ok(()) };
        let pause = self.pause_next;
        self.pause_next += 1;
        let h = self.clients[c].host;
        self.push(End::Mds(h), End::Client(c), Saying { id: 0, said: Said::Pause { pause } }.encode(), 0);
        self.pausing = Some(Pausing { c, pause, asked_at: self.now });
        if self.trace {
            eprintln!("  [{}] mds: client {} asked to pause for a snapshot ({})", self.step, c, pause);
        }
        self.count("pauses asked");
        Ok(())
    }

    /// Client `c`'s answer, ask `id`, to pause `pause` at `generation`: every
    /// chunk of the volume closed on every active replica up, then the
    /// snapshot at that generation -- answered with the word to go on, the
    /// word now and then lost, or the snapshot now and then applied late.
    /// One of a pause given up, or done, is answered by none: the session
    /// goes on by a word, or by attaching again.
    fn paused(&mut self, c: usize, id: u64, generation: u64, pause: u64) -> Result<(), String> {
        if self.trace {
            eprintln!("  [{}] mds: client {} paused for snapshot {} at generation {}{}", self.step, c, pause, generation,
                      if self.pausing.as_ref().is_some_and(|p| p.c == c && p.pause == pause) { "" } else { ", unknown" });
        }
        if !self.pausing.as_ref().is_some_and(|p| p.c == c && p.pause == pause) {
            return Ok(());
        }
        self.pausing = None;
        /* The volume's writer still: every chunk closed. Taken over while it
           paused -- at rest, or cut off -- what is open is the writer's after
           it, and its snapshot is to be refused: no writer has its generation */
        let writer = self.state.volume(VOLUME).and_then(|v| v.writer.as_ref()).is_some_and(|w| w.generation == generation);
        if writer {
            self.closed_at_pause(c)?;
        } else {
            self.count("pauses answered by a writer taken over");
        }
        let may = self.snapshot_may();
        let h = self.clients[c].host;
        match self.pause_word.take().unwrap_or_else(|| self.snap_r.below(10)) {
            0 => {
                self.count("snapshots of a pause to be applied late");
                self.late = Some(Late { at: self.now + PAUSE_NS + 2_000 * MS, generation, may });
            }
            k => {
                let answer = self.take_snapshot(generation, may, "a writer paused")?;
                if !writer && matches!(answer, Answer::Volume { .. }) {
                    return Err(format!("client {}, taken over, paused at generation {}, and its snapshot was taken", c,
                                       generation));
                }
                if k == 1 {
                    self.count("words to go on lost");
                } else {
                    let index = self.index;
                    self.push(End::Mds(h), End::Client(c), Saying { id, said: Said::GoOn { pause, index } }.encode(), id);
                }
                if !matches!(answer, Answer::Volume { .. }) {
                    self.count("snapshots of a pause refused");
                }
            }
        }
        Ok(())
    }

    /// Every chunk of the volume closed on every active replica up: what
    /// writer `c`, paused for a snapshot, has left (N10c2).
    fn closed_at_pause(&self, c: usize) -> Result<(), String> {
        let Some(v) = self.state.volume(VOLUME) else { return Ok(()) };
        for (place, &chunk) in v.map.iter().enumerate() {
            let Some(ch) = self.state.chunks().get(chunk as usize) else { continue };
            let Some(set) = self.state.sets().get(ch.set as usize) else { continue };
            for i in 0..nbsmeta::REPLICAS {
                if set.replicas[i] == Replica::Down {
                    continue;
                }
                let Some(h) = self.host_of_cs(set.cs[i]) else { continue };
                let Some(store) = self.hosts[h].store.as_ref() else { continue };
                if let Some(x) = store.holding(chunk).filter(|x| x.incarnation == ch.incarnation) {
                    if x.state == nbscs::SlotState::Open {
                        return Err(format!("client {} paused for a snapshot, and place {}'s chunk {} is open on host \
                                            {} at generation {}", c, place, chunk, self.hosts[h].name, x.generation));
                    }
                }
            }
        }
        Ok(())
    }

    /// A pause not answered in time given up -- no snapshot proposed, so the
    /// session may go on at once -- and a late snapshot applied when due:
    /// refused, the session having attached again, or held to what the
    /// volume held at its pause (N10c2).
    fn pause_turn(&mut self) -> Result<(), String> {
        if let Some(p) = self.pausing.as_ref().filter(|p| self.now >= p.asked_at + PAUSE_WAIT_NS) {
            let (c, pause) = (p.c, p.pause);
            self.pausing = None;
            self.count("pauses given up");
            self.given_up = Some((c, self.clients[c].s.generation(), self.clients[c].anew));
            if self.trace {
                eprintln!("  [{}] mds: client {}'s pause {} given up: {}", self.step, c, pause, self.clients[c].s.report());
            }
            let (h, index) = (self.clients[c].host, self.index);
            self.push(End::Mds(h), End::Client(c), Saying { id: 0, said: Said::GoOn { pause, index } }.encode(), 0);
        }
        /* The writer of a pause given up goes on at its generation: none was
           taken, so it attaches again for none */
        if let Some((c, generation, anew)) = self.given_up {
            let s = &self.clients[c].s;
            /* Attached anew for a write failed at its deadline meanwhile:
               not for the pause */
            if self.clients[c].zombie || matches!(s.phase(), Phase::Failed(_) | Phase::Detached)
                || self.clients[c].anew != anew
            {
                self.given_up = None;
            } else if s.generation() != generation {
                return Err(format!("client {}, its pause given up, attached again: generation {}, {} before", c,
                                   s.generation(), generation));
            } else if *s.phase() == Phase::Serving && !s.paused() {
                self.given_up = None;
            }
        }
        if let Some(l) = self.late.take() {
            if self.now < l.at {
                self.late = Some(l);
                return Ok(());
            }
            match self.take_snapshot(l.generation, l.may, "a pause's, applied late")? {
                Answer::Refused(nbsmeta::Refusal::NotTheWriter) => self.count("snapshots of a pause applied late refused"),
                Answer::Volume { .. } => self.count("snapshots of a pause applied late taken"),
                _ => {}
            }
        }
        Ok(())
    }

    /// Snapshot `k` dropped: its chunks counted down, those no map names
    /// freed -- by the garbage passes on the cs's.
    fn drop_snapshot(&mut self, k: usize) -> Result<(), String> {
        let s = self.snaps.remove(k);
        match self.apply(Command::VolumeDelete { name: s.name.clone() }) {
            Answer::Volume { .. } => {}
            other => return Err(format!("snapshot {} not dropped: {:?}", s.name, other)),
        }
        if self.trace {
            eprintln!("  [{}] snapshot {} dropped", self.step, s.name);
        }
        self.count("snapshots dropped");
        Ok(())
    }

    /* ---- the clone (N10d2) ---- */

    /// The clone's turn, on draws of its own: one made of the newest snapshot
    /// now and then, its writer's operations, the clone dropped now and then
    /// -- or at once, its writer lost with its host or failed.
    fn clone_turn(&mut self) -> Result<(), String> {
        let Some(k) = self.clones.len().checked_sub(1).filter(|_| self.state.volume(CLONE).is_some()) else {
            if self.clone_r.below(1000) < 4 {
                self.make_clone()?;
            }
            return Ok(());
        };
        if self.clones[k].dead {
            return self.drop_clone();
        }
        if *self.clones[k].s.phase() == Phase::Serving && self.clones[k].s.busy() < 6 && self.clone_r.chance(12) {
            self.clone_op(k)?;
        }
        /* Stopped with every host up, as the volume's writer is for a
           snapshot between two: a chunk whose last replica in is on a host
           down holds a stop up until it is back */
        if self.clone_r.below(1000) < 2 && self.down.is_none() {
            self.drop_clone()?;
        }
        Ok(())
    }

    /// A clone made of the newest snapshot -- once each block of it that may
    /// hold anything but zeros has been read, so that what the clone holds
    /// where its writer has not written is known -- its own host one up, and
    /// its writer started there.
    fn make_clone(&mut self) -> Result<(), String> {
        let Some(snap) = self.snaps.last() else { return Ok(()) };
        let mut base = BTreeMap::new();
        for (&b, may) in &snap.may {
            match snap.seen.get(&b) {
                Some(&v) => {
                    base.insert(b, v);
                }
                None if may.iter().all(|&v| v == 0) => {}
                None => return Ok(()),
            }
        }
        let up: Vec<usize> = (0..HOSTS.len()).filter(|&h| Some(h) != self.down).collect();
        let Some(h) = self.clone_r.pick(&up) else { return Ok(()) };
        let from = snap.name.clone();
        match self.apply(Command::Clone { from: from.clone(), name: String::from(CLONE), host: HOSTS[h].to_string() }) {
            Answer::Volume { .. } => {}
            other => return Err(format!("the clone of snapshot {} not made: {:?}", from, other)),
        }
        /* What it holds where its writer has not written: its snapshot's,
           there for good -- as a write done and flushed before any */
        self.cmodel = Blocks::default();
        for (b, v) in base.into_iter().filter(|&(_, v)| v != 0) {
            self.cmodel.history.insert(b, vec![(0, v, WState::Done)]);
            self.cmodel.flushed.insert(b, (BTreeSet::from([v]), 1));
        }
        let token = 1 + self.clone_r.below(u64::MAX - 1);
        let mut s = Session::new(CLONE, HOSTS[h], true, false, token);
        let k = self.clones.len();
        /* Its writes passed along their replicas (N13), now and then */
        if self.clone_r.chance(70) {
            s.answer_at(&format!("k{}", k));
        }
        let mut out = Vec::new();
        s.start(self.now, &mut out);
        self.clones.push(CloneW { host: h, s, bufs: Bufs::default(), ops: BTreeMap::new(), next_tag: 1, dead: false,
                                  stop_done: false, flush_failed_at: None, anew: 0 });
        self.count("clones made");
        if self.trace {
            eprintln!("  [{}] clone {} made of snapshot {}, its writer {} on host {}", self.step, CLONE, from, k,
                      HOSTS[h]);
        }
        self.clone_outs(k, out)
    }

    /// An operation of the clone's writer: a write, a read, or a flush.
    fn clone_op(&mut self, k: usize) -> Result<(), String> {
        let blocks = VOLUME_MIB << 20 >> 12;
        let first = self.clone_r.below(blocks.min(3 * SLOT / BLOCK));
        let n = (1 + self.clone_r.below(4)).min(blocks - first);
        let kind = match self.clone_r.below(10) {
            0..=4 => K::Write(1 + self.clone_r.below(255) as u8),
            5..=8 => K::Read,
            _ => K::Flush,
        };
        let tag = self.clones[k].next_tag;
        self.clones[k].next_tag += 1;
        let (offset, len) = (first * BLOCK, (n * BLOCK) as u32);
        let seq = self.seq;
        let mut before = Vec::new();
        let user = match kind {
            K::Write(v) => {
                self.seq += 1;
                self.clones[k].bufs.0.insert(tag, vec![v; len as usize]);
                for b in first..first + n {
                    self.cmodel.history.entry(b).or_default().push((seq, v, WState::Out));
                }
                UserOp::Write { tag, offset, len }
            }
            K::Read => {
                self.clones[k].bufs.0.insert(tag, vec![0u8; len as usize]);
                before = (first..first + n).map(|b| self.cmodel.acked(b)).collect();
                UserOp::Read { tag, offset, len }
            }
            K::Flush | K::Close => UserOp::Flush { tag },
        };
        if self.trace {
            eprintln!("  [{}] the clone's writer {}: {:?} seq {} blocks {}+{} submitted", self.step, k, kind, seq,
                      first, n);
        }
        self.clones[k].ops.insert(tag, MOp { kind, offset, len, seq, before, at: self.now });
        let now = self.now;
        let mut out = Vec::new();
        let cl = &mut self.clones[k];
        cl.s.submit(user, now, &mut cl.bufs, &mut out);
        self.clone_outs(k, out)
    }

    /// The clone's writer's outputs: its messages queued, its operations
    /// checked.
    fn clone_outs(&mut self, k: usize, out: Vec<Out>) -> Result<(), String> {
        for o in out {
            match o {
                Out::Send { to: To::Mds, id, bytes } => {
                    if let Some(Ask::Down { set, cs, .. }) = Asking::decode(&bytes).map(|a| a.ask) {
                        if !self.clones[k].s.writes_in(set, cs) {
                            return Err(format!("the clone's writer {} asked cs {} out of set {}, which its view does \
                                                not write it in -- {}", k, cs, set, self.clones[k].s.view()));
                        }
                    }
                    let h = self.clones[k].host;
                    self.push(End::Clone(k), End::Mds(h), bytes, id);
                }
                Out::Send { to: To::Cs(cs), id, bytes } => {
                    let Some(h) = self.host_of_cs(cs) else { return Err(format!("a request to cs {}, none", cs)) };
                    unmarked(self.clones[k].s.chains(), &bytes, &format!("the clone's writer {}", k), id, cs)?;
                    if self.trace {
                        if let Some(r) = Request::decode(&bytes).filter(|r| matches!(r.op, nbswire::Op::CopyUp { .. })) {
                            eprintln!("  [{}] the clone's writer {} sends #{} to cs {}: {:?} -- view {}", self.step, k,
                                      r.id, cs, r.op, self.clones[k].s.view());
                        }
                    }
                    self.push(End::Clone(k), End::Cs(h), bytes, id);
                }
                Out::Done { tag, ok } => self.clone_done(k, tag, ok)?,
                Out::Say(line) => {
                    /* Attached anew: what its writer had done and not flushed
                       as the reconcile's source holds it, there or not */
                    if line.contains("attached anew") {
                        self.clones[k].anew += 1;
                        self.cmodel.lose_unflushed();
                        self.count("a clone's writes failed at their deadline, its session attached anew");
                    }
                    if self.trace {
                        eprintln!("  [{}] the clone's writer {}: {}", self.step, k, line);
                    }
                }
            }
        }
        self.clone_failed(k)
    }

    /// The clone's writer failed: only as its chunk's last active replica
    /// refuses it -- driven no more then, the clone dropped -- or once
    /// dropped.
    fn clone_failed(&mut self, k: usize) -> Result<(), String> {
        let Phase::Failed(why) = self.clones[k].s.phase() else { return Ok(()) };
        if self.clones[k].dead {
            return Ok(());
        }
        if why != LOST_LAST {
            return Err(format!("the clone's writer {} failed: {}", k, why));
        }
        self.count("clone writers failed: the last replica lost");
        self.clones[k].dead = true;
        Ok(())
    }

    /// An operation of the clone's writer's done: held to the clone's model
    /// as the volume's writers' are to the volume's.
    fn clone_done(&mut self, k: usize, tag: u64, ok: bool) -> Result<(), String> {
        if tag == STOP {
            let phase = self.clones[k].s.phase();
            if self.clones[k].stop_done || !matches!(phase, Phase::Detached | Phase::Failed(_))
                || ok != (*phase == Phase::Detached)
            {
                return Err(format!("the clone's writer {}'s stop done {}, {:?}: {}", k, ok, phase,
                                   self.clones[k].s.report()));
            }
            self.clones[k].stop_done = true;
            return Ok(());
        }
        let Some(op) = self.clones[k].ops.remove(&tag) else { return Ok(()) };
        let data = self.clones[k].bufs.0.remove(&tag).unwrap_or_default();
        let (first, n) = (op.offset / BLOCK, u64::from(op.len) / BLOCK);
        let anew = self.clones[k].anew > 0
            && matches!(self.clones[k].s.phase(), Phase::Attaching | Phase::Fencing | Phase::Reconciling);
        let lost = self.clones[k].dead || matches!(self.clones[k].s.phase(), Phase::Failed(_)) || anew;
        if !ok && matches!(op.kind, K::Flush | K::Close) && self.now >= op.at + DEADLINE_NS {
            self.clones[k].flush_failed_at = Some(self.now);
        }
        if !ok && !lost && self.now < op.at + DEADLINE_NS
            && !self.failed_with_one_before(&op, self.clones[k].flush_failed_at, &self.clones[k].ops)
        {
            return Err(format!("the clone's writer {}: {:?} of blocks {}+{} failed {} ms after it was asked, before \
                                its deadline", k, op.kind, first, n, (self.now - op.at) / MS));
        }
        match op.kind {
            K::Write(v) => {
                for b in first..first + n {
                    if let Some(w) = self.cmodel.history.get_mut(&b).and_then(|h| h.iter_mut().find(|w| w.0 == op.seq)) {
                        w.2 = if ok { WState::Done } else { WState::Failed };
                    }
                }
                self.count(if ok { "clone writes done" } else { "clone writes failed" });
                if !ok {
                    return Ok(());
                }
                /* Every replica of the clone's in holds it -- but where a
                   later write was done since */
                let generation = self.clones[k].s.generation();
                for b in first..first + n {
                    if self.cmodel.history.get(&b).is_some_and(|h| h.iter().any(|w| w.0 > op.seq)) {
                        continue;
                    }
                    let Some((volume, chunk, inside, hosts)) = self.replicas_in(CLONE, b, generation) else { continue };
                    for h in hosts {
                        let got = self.block_on(h, volume, chunk, inside);
                        if got != Ok(v) {
                            return Err(format!("the clone's write of {} to block {} done, and host {} holds {:?}", v,
                                               b, self.hosts[h].name, got));
                        }
                    }
                }
            }
            K::Read => {
                self.count(if ok { "clone reads done" } else { "clone reads failed" });
                if !ok {
                    return Ok(());
                }
                for i in 0..n {
                    let b = first + i;
                    let got = &data[(i * BLOCK) as usize..((i + 1) * BLOCK) as usize];
                    let v = got[0];
                    if got.iter().any(|&x| x != v) {
                        return Err(format!("the clone's read of block {}: not one value", b));
                    }
                    let may = self.cmodel.readable(b, op.before[i as usize]);
                    if !may.contains(&v) {
                        return Err(format!("the clone's read of block {} gave {}, which may be {:?}", b, v, may));
                    }
                }
            }
            K::Flush | K::Close => {
                self.count(if ok { "clone flushes done" } else { "clone flushes failed" });
                if !ok {
                    return Ok(());
                }
                /* What was done before it durable -- or what came after that
                   and may be there: failed, or out still */
                let blocks: Vec<u64> = self.cmodel.history.keys().copied().collect();
                for b in blocks {
                    let h = &self.cmodel.history[&b];
                    let last = h.iter().rposition(|w| w.0 < op.seq && w.2 == WState::Done);
                    let mut may = BTreeSet::from([last.map_or(0, |i| h[i].1)]);
                    for w in h[last.map_or(0, |i| i + 1)..].iter().filter(|w| w.0 < op.seq && w.2 != WState::Done) {
                        may.insert(w.1);
                    }
                    self.cmodel.flushed.insert(b, (may, op.seq));
                }
            }
        }
        Ok(())
    }

    /// The clone's writer gone with its host: its writes out failed -- there
    /// or not -- and it driven no more, the clone to be dropped.
    fn lose_clone(&mut self, k: usize) {
        self.count("clone writers lost");
        let ops: Vec<MOp> = std::mem::take(&mut self.clones[k].ops).into_values().collect();
        for op in ops {
            if let K::Write(_) = op.kind {
                let first = op.offset / BLOCK;
                for b in first..first + u64::from(op.len) / BLOCK {
                    if let Some(w) = self.cmodel.history.get_mut(&b).and_then(|h| h.iter_mut().find(|w| w.0 == op.seq)) {
                        w.2 = WState::Failed;
                    }
                }
            }
        }
        self.clones[k].dead = true;
    }

    /// The clone's writer stopped -- what it was asked done, a flush, its
    /// chunks closed, the clone let go of -- its stop done once, as it ends.
    fn stop_clone(&mut self, k: usize) -> Result<(), String> {
        let now = self.now;
        let mut out = Vec::new();
        {
            let cl = &mut self.clones[k];
            cl.s.stop(STOP, now, &mut cl.bufs, &mut out);
        }
        self.clone_outs(k, out)?;
        for _ in 0..30_000 {
            if self.clones[k].dead || matches!(self.clones[k].s.phase(), Phase::Detached | Phase::Failed(_)) {
                break;
            }
            self.now += 5 * MS;
            if self.links.values().all(|q| q.is_empty()) {
                self.now += 200 * MS;
            }
            self.deliver()?;
            self.tick()?;
        }
        match self.clones[k].s.phase() {
            Phase::Detached | Phase::Failed(_) if !self.clones[k].stop_done => {
                Err(format!("the clone's writer {} over, and its stop never done", k))
            }
            Phase::Detached | Phase::Failed(_) => Ok(()),
            _ if self.clones[k].dead => Ok(()),
            _ => Err(format!("the clone's writer {} not stopped: {}", k, self.clones[k].s.report())),
        }
    }

    /// The clone dropped: its writer stopped, or -- lost or failed -- the
    /// clone let go of by force on a host up, as a person would; then
    /// deleted, its chunks freed by the cs's garbage passes where no map
    /// names them.
    fn drop_clone(&mut self) -> Result<(), String> {
        let Some(k) = self.clones.len().checked_sub(1) else { return Ok(()) };
        if self.state.volume(CLONE).is_none() {
            return Ok(());
        }
        if !self.clones[k].dead && !matches!(self.clones[k].s.phase(), Phase::Detached | Phase::Failed(_)) {
            self.stop_clone(k)?;
        }
        if self.state.volume(CLONE).is_some_and(|v| v.writer.is_some()) {
            let up: Vec<usize> = (0..HOSTS.len()).filter(|&h| Some(h) != self.down).collect();
            let h = self.clone_r.pick(&up).unwrap_or(0);
            let host = HOSTS[h].to_string();
            let token = 1 + self.clone_r.below(u64::MAX - 1);
            let attached = self.apply(Command::Attach { name: String::from(CLONE), host: host.clone(), writable: true,
                                                        force: true, token });
            let Answer::Attached { generation } = attached else {
                return Err(format!("the clone, its writer lost, not taken by force: {:?}", attached));
            };
            match self.apply(Command::Detach { name: String::from(CLONE), host, generation }) {
                Answer::Refused(why) => return Err(format!("the clone, taken by force, not let go of: {:?}", why)),
                _ => self.count("clones let go of by force"),
            }
        }
        match self.apply(Command::VolumeDelete { name: String::from(CLONE) }) {
            Answer::Volume { .. } => {}
            other => return Err(format!("the clone not dropped: {:?}", other)),
        }
        self.clones[k].dead = true;
        self.cmodel = Blocks::default();
        self.count("clones dropped");
        if self.trace {
            eprintln!("  [{}] clone {} dropped", self.step, CLONE);
        }
        Ok(())
    }

    /// The clone at the end, where there is one: its writer stopped, and
    /// each block it holds by its model -- its snapshot's, or written since
    /// -- read on every active replica of its chunk as the last write done
    /// there, or one that failed since, the same on all but where one did;
    /// then dropped.
    fn clone_end(&mut self) -> Result<(), String> {
        let Some(k) = self.clones.len().checked_sub(1).filter(|_| self.state.volume(CLONE).is_some()) else {
            return Ok(());
        };
        if !self.clones[k].dead {
            self.stop_clone(k)?;
        }
        if !self.clones[k].dead && *self.clones[k].s.phase() == Phase::Detached {
            let blocks: Vec<u64> = self.cmodel.history.keys().copied().collect();
            for b in blocks {
                let Some((volume, chunk, inside, hosts)) = self.replicas_in(CLONE, b, 0) else { continue };
                let failed = self.cmodel.failed_since_done(b);
                let mut may = failed.clone();
                may.insert(self.cmodel.acked(b).1);
                let mut seen: Option<(usize, u8)> = None;
                for h in hosts {
                    let v = self.block_on(h, volume, chunk, inside)
                        .map_err(|e| format!("the end: the clone's block {} on host {}: {}", b, self.hosts[h].name, e))?;
                    if !may.contains(&v) {
                        return Err(format!("the end: the clone's block {} is {} on host {}, which may be {:?}", b, v,
                                           self.hosts[h].name, may));
                    }
                    match seen {
                        Some((g, w)) if w != v && failed.is_empty() => {
                            return Err(format!("the end: the clone's block {} is {} on host {} and {} on host {}", b,
                                               w, self.hosts[g].name, v, self.hosts[h].name));
                        }
                        _ => seen = Some((h, v)),
                    }
                }
            }
            self.count("clones held to at the end");
        }
        self.drop_clone()
    }

    /// Every snapshot reads what the volume held at it: each block it is
    /// held to, on every active replica up of its chunk, one of what it may
    /// hold, the same on all, and the same as at the first look.
    fn snapshots_hold(&mut self, when: &str) -> Result<(), String> {
        for k in 0..self.snaps.len() {
            let name = self.snaps[k].name.clone();
            let blocks: Vec<u64> = self.snaps[k].may.keys().copied().collect();
            for b in blocks {
                let Some((volume, chunk, inside, hosts)) = self.replicas_in(&name, b, 0) else {
                    return Err(format!("{}: snapshot {}'s block {} is in no chunk", when, name, b));
                };
                for h in hosts {
                    let v = self.block_on(h, volume, chunk, inside)
                        .map_err(|e| format!("{}: snapshot {}'s block {} on host {}: {}", when, name, b,
                                             self.hosts[h].name, e))?;
                    let snap = &mut self.snaps[k];
                    if !snap.may.get(&b).is_some_and(|m| m.contains(&v)) {
                        return Err(format!("{}: snapshot {}'s block {} is {} on host {} (chunk {}), and the volume \
                                            held {:?} there when it was taken", when, name, b, v,
                                           self.hosts[h].name, chunk.chunk, snap.may.get(&b)));
                    }
                    match snap.seen.get(&b) {
                        Some(&was) if was != v => {
                            return Err(format!("{}: snapshot {}'s block {} is {} on host {} (chunk {}), {} before",
                                               when, name, b, v, self.hosts[h].name, chunk.chunk, was));
                        }
                        Some(_) => {}
                        None => {
                            snap.seen.insert(b, v);
                        }
                    }
                }
            }
        }
        if !self.snaps.is_empty() {
            self.count("snapshots looked at");
        }
        Ok(())
    }

    /// Client `c` come to serve -- in a delivery or a tick before: every
    /// replica alike.
    fn served(&mut self, c: usize, _was: Phase) -> Result<(), String> {
        /* Serving again at a newer generation -- its own attach anew, for a
           write failed at its deadline or a pause given up: what it had done
           and not flushed as the reconcile's source holds it, there or not,
           and the newer generation the one fences hold to */
        let generation = self.clients[c].s.generation();
        if *self.clients[c].s.phase() == Phase::Serving && self.clients[c].served && !self.clients[c].zombie
            && generation > self.clients[c].served_gen
        {
            self.clients[c].served_gen = generation;
            self.clients[c].failed_write_gen = None;
            self.served = self.served.max(generation);
            self.blocks.lose_unflushed();
            self.count("sessions serving again, attached anew");
            self.same_settled(generation, "a writer come to serve again")?;
            return Ok(());
        }
        if *self.clients[c].s.phase() == Phase::Serving && !self.clients[c].served && !self.clients[c].zombie {
            self.clients[c].served = true;
            self.clients[c].served_gen = generation;
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
                    /* Written first since a snapshot: zeros in it (N10c) */
                    if !self.blocks.history.contains_key(&b) {
                        for snap in self.snaps.iter_mut() {
                            snap.may.entry(b).or_insert_with(|| BTreeSet::from([0]));
                        }
                    }
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
        /* Its requests in flight and its garbage pass gone first: each
           holds the store */
        self.hosts[h].flights.clear();
        self.hosts[h].garbage = None;
        self.hosts[h].asks.clear();
        self.hosts[h].store = None;
        let r = &mut self.r;
        self.hosts[h].disk.borrow_mut().cut(r);
        self.links.retain(|&(from, to), _| {
            from != End::Cs(h) && to != End::Cs(h) && from != End::Copy(h) && to != End::Copy(h)
        });
        self.down = Some(h);
        self.scrub_q.retain(|&(x, _, _)| x != h);
        self.hosts[h].down_since = Some(self.now);
        self.hosts[h].copies.clear();
        let writers: Vec<usize> = (0..self.clients.len()).filter(|&c| self.clients[c].host == h && !self.clients[c].dead)
            .collect();
        for c in writers {
            self.lose_client(c);
        }
        let clones: Vec<usize> = (0..self.clones.len()).filter(|&k| self.clones[k].host == h && !self.clones[k].dead)
            .collect();
        for k in clones {
            self.lose_clone(k);
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
        let store = Store::new();
        let handle = Handle(self.hosts[h].disk.clone());
        if let Err((_, e)) = nbscs::now(store.open_device(handle, self.hosts[h].identity)) {
            return Err(format!("host {}'s disk not opened again: {:?}", self.hosts[h].name, e));
        }
        if self.trace {
            let open: Vec<(u32, u64)> = store.holdings().iter().filter(|x| x.state == nbscs::SlotState::Open)
                .map(|x| (x.chunk, x.generation)).collect();
            eprintln!("  [{}] host {} finds open (chunk, generation): {:?}", self.step, self.hosts[h].name, open);
        }
        self.hosts[h].store = Some(Rc::new(store));
        self.hosts[h].vouched = Rc::new(Requests::new());
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
        /* Its writes passed along their replicas (N13), now and then: the
           rest to each replica directly, as `nbs io`'s go */
        if Rng::new(mix(token, 0x6368_6169_6e)).chance(70) {
            s.answer_at(&format!("w{}", c));
        }
        let mut out = Vec::new();
        s.start(self.now, &mut out);
        self.clients.push(Client { host: h, s, bufs: Bufs::default(), ops: BTreeMap::new(), next_tag: 1, zombie: false,
                                   dead: false, served: false, unflushed: Vec::new(), quiet: false,
                                   shown: Phase::Attaching, failed: 0, stop_done: false,
                                   flush_failed_at: None, downs_asked: BTreeSet::new(), starved_until: 0,
                                   asked_starved: 0, served_gen: 0, anew: 0, failed_write_gen: None });
        self.outs(c, out)
    }

    /// Whether every chunk of the volume has a replica in on a host other
    /// than `h`: a writer can do without `h`.
    fn whole_without(&self, h: usize) -> bool {
        self.volume_whole_without(VOLUME, h)
    }

    /// `whole_without`, of volume `name`'s chunks: the clone's too, where it
    /// is to be stopped with `h` gone (N10d2).
    fn volume_whole_without(&self, name: &str, h: usize) -> bool {
        let Some(v) = self.state.volume(name) else { return false };
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
            if self.links.iter().all(|(&(f, t), q)| q.is_empty() || self.is_cut(f, t)) {
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
        self.lost_for_good = Some(h);
        self.count("hosts lost for good, written past");
        let failed = |w: &World| w.clients[c].failed;
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

    /// A writer's flush to a replica that restarted with chunks it had open
    /// (N8's rule, which the runs reached once in a long while once N10c's
    /// snapshots changed them): what the replica's disk cache lost may be
    /// what the writer wrote since its last flush, so it refuses the flush as
    /// failed and is taken out -- a flush done there would say durable what
    /// is gone. A flush, then every chunk written; a host that holds some, not
    /// the writer's, and that every chunk can do without, crashed and back at
    /// once, its chunks found open; the writer's flush; and the host crashed
    /// and back again, held then -- as a replica in -- to what was flushed.
    /// Whether it was shaped.
    fn found_open_flush(&mut self) -> Result<bool, String> {
        let Some(c) = self.current() else { return Ok(false) };
        let idle = |w: &World, c: usize| w.clients[c].ops.is_empty() || !w.serving(c);
        self.drive(c, &|w, c| {
            w.serving(c) || w.clients[c].zombie || matches!(w.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached)
        })?;
        self.drive(c, &idle)?;
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        /* Flushed first: what the crash may lose is the writes below alone */
        self.submit_op(c, K::Flush, 0, 0)?;
        self.drive(c, &idle)?;
        let places = (VOLUME_MIB << 20) / SLOT;
        for p in 0..places {
            for k in 0..3 {
                let v = 1 + self.r.below(255) as u8;
                self.submit_op(c, K::Write(v), p * (SLOT / BLOCK) + 5 + k, 1)?;
            }
        }
        self.drive(c, &idle)?;
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        let me = self.clients[c].host;
        let hosts: Vec<usize> = (0..HOSTS.len()).filter(|&h| h != me && self.holds_some(h) && self.whole_without(h))
            .collect();
        let Some(x) = self.r.pick(&hosts) else { return Ok(false) };
        self.crash(x)?;
        self.now += 300 * MS;
        self.restart(x)?;
        self.submit_op(c, K::Flush, 0, 0)?;
        self.drive(c, &idle)?;
        if !self.serving(c) {
            return Ok(false);
        }
        self.crash(x)?;
        self.now += 300 * MS;
        self.restart(x)?;
        self.count("flushes shaped to a replica back with chunks found open");
        Ok(true)
    }

    /// Writes made while the writer's heap has no room for a request (#97):
    /// every host up, each write's requests refused memory for longer than
    /// a cs is given to answer -- sent again in a moment, not taken for the
    /// cs's silence: no cs asked out anew meanwhile -- then let be: every
    /// write done, none failed, each read back as written. The blocks are
    /// written and flushed unstarved first: a chunk whose last replica lost
    /// what it was given fails the session at its first write, which is no
    /// starving's. Drawn of `r`, its own.
    fn starved_writes(&mut self, r: &mut Rng) -> Result<bool, String> {
        let Some(c) = self.current() else { return Ok(false) };
        let idle = |w: &World, c: usize| w.clients[c].ops.is_empty() || !w.serving(c);
        self.drive(c, &|w, c| {
            w.serving(c) || w.clients[c].zombie || matches!(w.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached)
        })?;
        self.drive(c, &idle)?;
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        let blocks = VOLUME_MIB << 20 >> 12;
        let picks: Vec<(u64, u64)> = (0..STARVED_WRITES).map(|_| (r.below(blocks - 4), 1 + r.below(4))).collect();
        for &(first, n) in &picks {
            self.submit_op(c, K::Write(1 + r.below(255) as u8), first, n)?;
        }
        self.submit_op(c, K::Flush, 0, 0)?;
        self.drive(c, &idle)?;
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        /* The cluster quiet first -- every set whole, nothing on its way:
           a join's copy filling the network holds a cs's answer past the
           0.5 s it is given, which takes a live cs out rightly, and is no
           starving's (found once N13's chains changed what the network
           carries: a close's answer late behind two joins' pulls) */
        if !self.quiet()? || !self.serving(c) {
            return Ok(false);
        }
        let failed = self.clients[c].failed;
        let until = self.now + STARVED_NS;
        self.clients[c].s.starve_until(until);
        self.clients[c].starved_until = until;
        self.clients[c].asked_starved = 0;
        for &(first, n) in &picks {
            self.submit_op(c, K::Write(1 + r.below(255) as u8), first, n)?;
        }
        self.drive(c, &idle)?;
        self.clients[c].starved_until = 0;
        let starved = self.clients[c].s.report();
        if !self.serving(c) || self.clients[c].failed != failed {
            return Err(format!("client {}'s writes starved of memory failed, every host up: {}", c, starved));
        }
        if self.clients[c].asked_starved != 0 {
            return Err(format!("client {} asked {} cs's out while its writes were starved of memory, every host up: \
                                {}", c, self.clients[c].asked_starved, starved));
        }
        for &(first, n) in &picks {
            self.submit_op(c, K::Read, first, n)?;
        }
        self.drive(c, &idle)?;
        self.count("writes starved of memory, sent again");
        Ok(true)
    }

    /// A write failed at its deadline (N13's open question, decided: the
    /// session attaches anew). One active replica of a chunk, on another
    /// host than the writer's, is cut off from every host but its own --
    /// its host up, so that the leader's look keeps it in, as it keeps a
    /// slow one -- while the writer writes the chunk: the write waits past
    /// its deadline and fails, taken by the replicas it reached and not by
    /// the one cut off. The session attaches anew -- no close of the old
    /// generation's after the write failed, judged at every send -- and,
    /// the cut healed, serves again at a newer generation, its reconcile
    /// having made the chunk the same on its replicas. Drawn of `r`.
    fn cut_write(&mut self, r: &mut Rng) -> Result<bool, String> {
        let Some(c) = self.current() else { return Ok(false) };
        let idle = |w: &World, c: usize| w.clients[c].ops.is_empty() || !w.serving(c);
        self.drive(c, &|w, c| {
            w.serving(c) || w.clients[c].zombie || matches!(w.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached)
        })?;
        self.drive(c, &idle)?;
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        let blocks = VOLUME_MIB << 20 >> 12;
        let first = r.below(blocks - 4);
        let Some((_, chunk, _, _)) = self.replicas(first, 0) else { return Ok(false) };
        let Some(set) = self.state.sets().get(chunk.epoch.set as usize).cloned() else { return Ok(false) };
        let me = self.clients[c].host;
        let others: Vec<usize> = (0..nbsmeta::REPLICAS).filter(|&i| set.replicas[i] == Replica::Active)
            .filter_map(|i| self.host_of_cs(set.cs[i])).filter(|&h| h != me && self.hosts[h].store.is_some())
            .collect();
        if others.is_empty() {
            return Ok(false);
        }
        let x = others[r.below(others.len() as u64) as usize];
        let (anew, generation, tag) = (self.clients[c].anew, self.clients[c].s.generation(), self.clients[c].next_tag);
        let value = 1 + r.below(255) as u8;
        self.cut = Some(x);
        if self.trace {
            eprintln!("  [{}] host {}'s cs cut off, client {} writing block {}", self.step, self.hosts[x].name, c, first);
        }
        self.submit_op(c, K::Write(value), first, 1)?;
        let ended = |w: &World, c: usize| {
            w.clients[c].anew != anew || !w.clients[c].ops.contains_key(&tag) || w.clients[c].zombie
                || matches!(w.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached)
        };
        self.drive(c, &ended)?;
        /* Judged where the write left the replicas apart -- on some, not on
           the one cut off. One the session never sent -- paused for a
           snapshot as it came, attached again, and its fence of the replica
           cut off waiting as the leader, on its host, finds it up -- reached
           none, and needs no attach anew */
        let apart = self.apart(first, 1, value);
        self.cut = None;
        if self.clients[c].zombie || matches!(self.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached) {
            return Ok(false);
        }
        if self.clients[c].anew == anew && !apart {
            return Ok(false);
        }
        if self.clients[c].anew == anew {
            return Err(format!("client {}'s write to block {}, host {}'s replica cut off, ended without its session \
                                attaching anew: {}", c, first, self.hosts[x].name, self.clients[c].s.report()));
        }
        /* The cut healed: on to serve again, at a newer generation */
        self.drive(c, &|w, c| {
            (w.serving(c) && w.clients[c].s.generation() > generation) || w.clients[c].zombie
                || matches!(w.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached)
        })?;
        if self.clients[c].zombie || matches!(self.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached) {
            return Ok(false);
        }
        if !self.serving(c) || self.clients[c].s.generation() <= generation {
            return Err(format!("client {}, attached anew past its write's deadline, never served again: {}", c,
                               self.clients[c].s.report()));
        }
        self.count("writes cut off past their deadline, the session attached anew");
        Ok(true)
    }

    /// A close of the writer's asked with nothing else out, after a block
    /// written into each of a few places: done in a moment, every place
    /// written closed on every active replica -- its closes going two places
    /// at a time here, window after window, each as the one before is done:
    /// not each after the places' 3 s of idleness. Drawn of `r`.
    fn close_all_written(&mut self, r: &mut Rng) -> Result<bool, String> {
        let Some(c) = self.current() else { return Ok(false) };
        let idle = |w: &World, c: usize| w.clients[c].ops.is_empty() || !w.serving(c);
        self.drive(c, &|w, c| {
            w.serving(c) || w.clients[c].zombie || matches!(w.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached)
        })?;
        self.drive(c, &idle)?;
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        let per = SLOT / BLOCK;
        let places = (VOLUME_MIB << 20) / SLOT;
        for place in 0..places {
            if r.chance(70) {
                self.submit_op(c, K::Write(1 + r.below(255) as u8), place * per + r.below(per), 1)?;
            }
        }
        self.drive(c, &idle)?;
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        let (tag, asked) = (self.clients[c].next_tag, self.now);
        self.submit_op(c, K::Close, 0, 0)?;
        self.drive(c, &|w, c| !w.clients[c].ops.contains_key(&tag) || !w.serving(c))?;
        if !self.serving(c) || self.down.is_some() || self.clients[c].ops.contains_key(&tag) {
            return Ok(false);
        }
        self.closed_all(c)?;
        if self.now - asked >= CLOSE_ALONE_NS {
            return Err(format!("client {}'s close asked alone took {} ms: its windows waited on more than their \
                                answers", c, (self.now - asked) / MS));
        }
        self.count("closes asked alone, every place closed");
        Ok(true)
    }

    /// A flush after one that failed (#95): the last active replica of a
    /// chunk's set, on host `x` -- its two others taken out, as the writer
    /// would ask -- holds a write not yet flushed when `x` crashes, every
    /// write its cache held lost; a flush waits for it to its deadline and
    /// fails. A write to a chunk `x` holds nothing of, and a flush after it
    /// -- asked while the first is out, or after it failed -- is not done
    /// before `x` is flushed: the write on `x` is lost, and a flush done says
    /// durable what was written before it. `x` back, the restart's judge
    /// holds each block it has to what the last flush done made durable.
    /// The controller held off meanwhile, so that `x` stays the last
    /// active. Drawn of `r`, its own.
    fn flush_after_failed(&mut self, r: &mut Rng) -> Result<bool, String> {
        let Some(c) = self.current() else { return Ok(false) };
        let idle = |w: &World, c: usize| w.clients[c].ops.is_empty() || !w.serving(c);
        self.drive(c, &|w, c| {
            w.serving(c) || w.clients[c].zombie || matches!(w.clients[c].s.phase(), Phase::Failed(_) | Phase::Detached)
        })?;
        self.drive(c, &idle)?;
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        /* A place whose set's three are active with no joiner, one of them on
           a host other than the writer's -- and a place whose set has
           nothing on that host */
        let me = self.clients[c].host;
        let Some(map) = self.state.volume(VOLUME).map(|v| v.map.clone()) else { return Ok(false) };
        let set_of = |w: &World, chunk: u32| w.state.chunks().get(chunk as usize).map(|ch| ch.set);
        let mut pick = None;
        'places: for (p, &chunk) in map.iter().enumerate() {
            let Some(set) = set_of(self, chunk) else { continue };
            let Some(s) = self.state.sets().get(set as usize) else { continue };
            if s.joiner.is_some() || s.replicas.iter().any(|&x| x != Replica::Active) {
                continue;
            }
            for i in 0..nbsmeta::REPLICAS {
                let Some(x) = self.host_of_cs(s.cs[i]) else { continue };
                if x == me {
                    continue;
                }
                let xcs = self.hosts[x].cs;
                let far = map.iter().enumerate().find(|&(_, &qc)| set_of(self, qc).and_then(|qs| self.state.sets()
                    .get(qs as usize)).is_some_and(|t| !t.cs.contains(&xcs) && t.joiner.is_none_or(|j| j.cs != xcs)));
                if let Some((q, _)) = far {
                    pick = Some((p as u64, set, i, x, q as u64));
                    break 'places;
                }
            }
        }
        let Some((p, set, i, x, q)) = pick else { return Ok(false) };
        let held = self.plan_at;
        self.plan_at = u64::MAX;
        let shaped = self.flush_after_failed_at(c, r, p, set, i, x, q);
        self.plan_at = held;
        shaped
    }

    #[allow(clippy::too_many_arguments)]
    fn flush_after_failed_at(&mut self, c: usize, r: &mut Rng, p: u64, set: u32, i: usize, x: usize, q: u64)
                             -> Result<bool, String> {
        let idle = |w: &World, c: usize| w.clients[c].ops.is_empty() || !w.serving(c);
        let per = SLOT / BLOCK;
        let (bp, bq) = (p * per + r.below(per), q * per + r.below(per));
        /* Written and flushed: durable on the set's three */
        self.submit_op(c, K::Write(1 + r.below(255) as u8), bp, 1)?;
        self.submit_op(c, K::Flush, 0, 0)?;
        self.drive(c, &idle)?;
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        /* The set's two others taken out, as its writer asks: x its last
           active */
        let generation = self.clients[c].s.generation();
        let Some(others) = self.state.sets().get(set as usize)
            .map(|s| (0..nbsmeta::REPLICAS).filter(|&k| k != i).map(|k| s.cs[k]).collect::<Vec<u32>>()) else {
            return Ok(false);
        };
        for cs in others {
            if !matches!(self.apply(Command::ReplicaDown { set, cs, refused: false, name: String::from(VOLUME),
                                                           generation }), Answer::Set { .. }) {
                return Ok(false);
            }
        }
        /* Written there again: to x alone, once the view is read again */
        self.submit_op(c, K::Write(1 + r.below(255) as u8), bp, 1)?;
        self.drive(c, &idle)?;
        if !self.serving(c) || self.down.is_some() {
            return Ok(false);
        }
        /* x crashes, every write its cache held lost -- that one with them */
        self.hosts[x].disk.borrow_mut().lose_next();
        self.crash(x)?;
        /* A flush, x's to make: it waits for x, which no ask takes out, to
           its deadline -- and a write elsewhere and a flush after it, asked
           while that one is out or after it failed */
        let early = r.chance(50);
        self.submit_op(c, K::Flush, 0, 0)?;
        if !early {
            self.drive(c, &idle)?;
        }
        self.submit_op(c, K::Write(1 + r.below(255) as u8), bq, 1)?;
        self.submit_op(c, K::Flush, 0, 0)?;
        self.drive(c, &idle)?;
        /* x back: what it has held to what the last flush done made durable */
        self.restart(x)?;
        self.count(if early { "flushes asked while one before was failing" } else { "flushes asked after one failed" });
        Ok(true)
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
                        scrub: Scrub::new(SCRUB_EVERY), scrub_q: Vec::new(), scrub_r: Rng::new(0), scrub_id: 1,
                        rotted: Vec::new(), snaps: Vec::new(), snap_r: Rng::new(0), snap_next: 1, garbage_at: 0, lost_for_good: None,
                        pausing: None, late: None, pause_next: 1, given_up: None, cut: None, pause_word: None,
                        clones: Vec::new(), cmodel: Blocks::default(), clone_r: Rng::new(0),
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
        let disk = Rc::new(RefCell::new(Disk::new(DISK_SECTOR, bytes)));
        let store = Store::new();
        if let Err((_, e)) = nbscs::now(store.format_device(Handle(disk.clone()), identity, 1 + r.below(u64::MAX - 1))) {
            return Err(format!("disk of {} not formatted: {:?}", h, e));
        }
        w.hosts.push(Host { name: h, cs: cs.id, disk, identity, store: Some(Rc::new(store)),
                            vouched: Rc::new(Requests::new()), flights: Vec::new(), asks: Rc::new(Asks::default()),
                            garbage: None,
                            down_since: None, copies: Vec::new(), joins_seen: false, joins_at: 0, copy_id: 1 });
    }
    Ok(w)
}

pub fn run(seed: u64, steps: u64, trace: bool) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x636c_7573));
    let mut w = setup(&mut r)?;
    w.trace = trace;
    /* The scrub's randomness of its own, so that the run goes as it would
       without it */
    w.scrub_r = Rng::new(mix(seed, 0x7363_7275));
    /* The snapshots' too (N10c) */
    w.snap_r = Rng::new(mix(seed, 0x736e_6170));
    w.clone_r = Rng::new(mix(seed, 0x636c_6f6e));
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
        /* A snapshot between two writers now and then, one dropped, and every
           snapshot looked at (N10c): of a randomness of their own */
        if w.snap_r.below(1000) < 4 {
            if w.snap_r.chance(50) {
                w.pause_writer().map_err(|e| format!("step {}: {}", step, e))?;
            } else if w.down.is_none() && w.rest.is_none() && w.pausing.is_none() && w.late.is_none() {
                w.snapshot().map_err(|e| format!("step {}: {}", step, e))?;
            }
        } else if !w.snaps.is_empty() && w.snap_r.below(1000) < 2 {
            let k = w.snap_r.below(w.snaps.len() as u64) as usize;
            w.drop_snapshot(k).map_err(|e| format!("step {}: {}", step, e))?;
        }
        if !w.snaps.is_empty() && w.snap_r.chance(1) {
            w.snapshots_hold("a look").map_err(|e| format!("step {}: {}", step, e))?;
        }
        /* A clone of the newest snapshot, its writer's operations, the clone
           dropped (N10d2): of a randomness of their own */
        w.clone_turn().map_err(|e| format!("step {}: {}", step, e))?;
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
       it may, and every replica on a host up alike. The clone first, where
       there is one -- whole without the host lost too: its writer stopped,
       held to its model, dropped (N10d2) */
    let mut lost = None;
    if let Some(h) = w.down {
        if w.r.chance(50) && w.whole_without(h)
            && (w.state.volume(CLONE).is_none() || w.volume_whole_without(CLONE, h))
        {
            w.count("hosts lost for good");
            if w.trace {
                eprintln!("  [end] host {} lost for good", w.hosts[h].name);
            }
            lost = Some(h);
            w.lost_for_good = lost;
        } else {
            w.restart(h).map_err(|e| format!("the end: {}", e))?;
        }
    }
    w.clone_end().map_err(|e| format!("the end: {}", e))?;
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
        shaped = true;
        if ends.chance(50) {
            w.close_drained().map_err(|e| format!("the end: {}", e))?;
        } else {
            w.source_found_open().map_err(|e| format!("the end: {}", e))?;
        }
    }
    /* A flush to a replica back with chunks found open (N10c), drawn of its
       own so that the ends above go as they did */
    if !shaped && lost.is_none() && Rng::new(mix(seed, 0x666f_666c)).chance(30) {
        w.found_open_flush().map_err(|e| format!("the end: {}", e))?;
    }
    /* A flush after one that failed (#95), drawn of its own, so that the
       ends it does not shape go as they did */
    let mut flushes = Rng::new(mix(seed, 0x666c_6166));
    if !shaped && lost.is_none() && flushes.chance(30) {
        w.flush_after_failed(&mut flushes).map_err(|e| format!("the end: {}", e))?;
    }
    /* Writes whose requests find no memory for a while (#97), drawn of
       their own, so that the ends they do not shape go as they did */
    let mut starve = Rng::new(mix(seed, 0x7374_6172));
    if !shaped && lost.is_none() && starve.chance(30) {
        w.starved_writes(&mut starve).map_err(|e| format!("the end: {}", e))?;
    }
    /* A close asked alone, window after window: drawn of its own too */
    let mut alone = Rng::new(mix(seed, 0x636c_6f73));
    if !shaped && lost.is_none() && alone.chance(30) {
        w.close_all_written(&mut alone).map_err(|e| format!("the end: {}", e))?;
    }
    /* A write past its deadline, a replica cut off (N13's decision): drawn
       of its own too */
    let mut cut = Rng::new(mix(seed, 0x6375_7477));
    if !shaped && lost.is_none() && cut.chance(15) {
        w.cut_write(&mut cut).map_err(|e| format!("the end: {}", e))?;
    }
    /* The stop asked of a writer paused for a snapshot, a write held behind
       the pause -- the word to go on said, lost, or the snapshot applied
       late -- and the stop asked again (N10c2): drawn of their own, so that
       the ends they do not shape go as they did */
    let mut stops = Rng::new(mix(seed, 0x7374_6f70));
    let mut pause_first = stops.chance(35);
    let again = stops.chance(25);
    let own = stops.chance(25);
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
        if std::mem::take(&mut pause_first) && w.serving(c) && !w.clients[c].s.paused() && w.pausing.is_none()
            && w.late.is_none()
        {
            w.pause_word = Some(stops.below(3));
            w.pause_writer().map_err(|e| format!("the end: {}", e))?;
            w.drive(c, &|w, c| w.clients[c].s.paused() || !w.serving(c) || w.pausing.is_none())
                .map_err(|e| format!("the end: {}", e))?;
            if w.clients[c].s.paused() {
                w.count("stops asked of a writer paused for a snapshot");
                let first = stops.below((VOLUME_MIB << 20 >> 12).min(3 * SLOT / BLOCK));
                w.submit_op(c, K::Write(1 + stops.below(255) as u8), first, 1).map_err(|e| format!("the end: {}", e))?;
            }
        }
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
            /* A stop with a tag of the session's own: refused at once, the
               session as it was */
            if own {
                let was = cl.s.phase().clone();
                let mut refused = Vec::new();
                cl.s.stop(STOP_OWN, now, &mut cl.bufs, &mut refused);
                if refused != [Out::Done { tag: STOP_OWN, ok: false }] || *cl.s.phase() != was {
                    return Err(format!("the end: client {}'s stop with a tag of the session's own: {:?}, {:?} then, \
                                        {:?} before", c, refused, cl.s.phase(), was));
                }
            }
            cl.s.stop(STOP, now, &mut cl.bufs, &mut out);
            /* Asked again: done at once, the first standing -- not ok, but
               where the session is over already */
            if again {
                let mut twice = Vec::new();
                cl.s.stop(STOP_AGAIN, now, &mut cl.bufs, &mut twice);
                let ok = *cl.s.phase() == Phase::Detached;
                if twice != [Out::Done { tag: STOP_AGAIN, ok }] {
                    return Err(format!("the end: client {}'s stop asked again: {:?}", c, twice));
                }
            }
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
        if matches!(phase, Phase::Detached | Phase::Failed(_)) {
            w.stop_said(c).map_err(|e| format!("the end: {}", e))?;
        }
        match phase {
            Phase::Detached => {
                w.settle(lost).map_err(|e| format!("the end: {}", e))?;
                w.snapshots_hold("the end")?;
                /* Every set whole, no copy to come: a sector of a closed chunk
                   rotted on one member, now and then (N9e) -- the scrub to
                   find it, the member taken down as a person would, its join
                   back to copy it again. Rotted before, a copy from it could
                   make two members alike in rot, which no look can tell */
                if lost.is_none() && ends.chance(40) && w.rot_one(&mut ends) {
                    w.settle(lost).map_err(|e| format!("the end: {}", e))?;
                }
                let generation = w.clients[c].s.generation();
                w.same(generation, "the end, detached")?;
                if std::env::var_os("NBS_CLUSTER_STATS").is_some() {
                    eprintln!("cluster seed {}: {:?}", seed, w.stats);
                }
                /* Chains that went somewhere: a cs that passes nothing on
                   leaves every chain to the clients' direct sends, which
                   nothing else here would tell (N13) */
                let begun = w.stats.get("chains begun").copied().unwrap_or(0);
                if begun >= 20 && w.stats.get("writes passed on along a chain").copied().unwrap_or(0) == 0 {
                    return Err(format!("{} chains begun, no write passed on along one", begun));
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
