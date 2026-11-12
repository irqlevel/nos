//! The cs's task: the devices the metadata gives this host's cs taken in,
//! and the requests of NBS's clients served from them -- many at once, on
//! this one task, which never waits on a device or on the mds (N12c).
//!
//! - **The loop**: asleep on the task's one event, which its mailbox and
//!   its devices' queues signal; awake, it takes what came -- a leader's
//!   look answered there and then, whatever the devices are doing; a
//!   request kept, in order, for a future of its own; an answer of the
//!   mds's handed to the question that asked it -- and polls every future
//!   again while anything moved: a device's request reaped, a turn let go,
//!   a round or a flush ended (`Store::moves`, the devices' own). A future
//!   is nbswire's `serve` of one request, or one of the cs's own two tasks
//!   below; each is pending where it waits, and nothing it waits for is
//!   waited for anywhere else. Up to IN_FLIGHT_MAX requests at once, the
//!   rest kept; a step of one not done in 30 s fails its device.
//! - **Its devices**, from this host's mds (`Question::Identity`), asked
//!   again every few seconds: each taken in -- opened, or formatted where it
//!   is blank -- and one that is NBS's but another cluster's or cs's, or
//!   holds something else, left as it is, said, and tried again each time:
//!   `nbs devices add ... wipe` clears it, a claim that was in the way goes.
//! - **Requests** on `nbs.cs`, each answered to the mailbox it came from --
//!   and only once every device the metadata gives the cs is taken in: with
//!   one missing, a chunk on it would read as nothing, and its first write
//!   would make it a second slot, which the device coming back would
//!   contend with. Until then, `NotReady`. A request is done by nbswire's
//!   `serve`, the one copy the cluster fuzz target's cs's run too: one the
//!   engine has not checked -- a chunk it does not hold, an epoch other than
//!   the one it knows -- is asked of the mds (`Question::Verify`) first, its
//!   answer awaited by that request alone.
//! - **What the metadata no longer has** freed every half minute: chunks
//!   freed or their number another chunk's now, fences of volumes gone.
//! - **The joins** this cs is the joiner of (N9), asked of the mds with its
//!   devices: each copied by nbswire's `copy`, its pulls sent to the set's
//!   active members and their answers handed back, its end asked of the
//!   mds. A join found as the cs starts is given up: what clients wrote to
//!   it was marked in memory alone, and a copy without those marks could
//!   write over them.
//!
//! A client's requests and their answers go between hosts by the channel's
//! bulk connection (`TAG_BULK`), all of them, so that they keep their order
//! and their data do not hold up raft's messages.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::future::{poll_fn, Future};
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use kcore::ipc::{Limits, Mailbox, Message, Scope, TAG_BULK};
use kcore::klog;
use kcore::time::{boot_time_ns, Duration};
use nbscs::{Device, Identity as DevIdentity, OpenError, SlotState, Store};
use nbswire::copy::{self, Copy};
use nbswire::serve::{collect, serve, Asker, Requests};
use nbswire::{Answer, Answered, Op, Query, Question, Refusal, Reply, Request, Told, CS_NAME, TAG_ANSWER,
              TAG_QUERY, TAG_REQUEST, TAG_TOLD};

use crate::disk::QueueDevice;
use crate::{DeviceShown, Shared, Shown, CHUNKS_SHOWN};

/// How often the devices and the joins are asked of the mds again, and
/// the gone freed; and the devices, while the cs does not serve -- a device
/// just added taken in within a second, before the leader's controller
/// takes the cs's not serving for its being down, two seconds on, and has
/// its places joined again for nothing.
const IDENTITY_EVERY_MS: u64 = 5_000;
const IDENTITY_MISSING_MS: u64 = 1_000;
const GARBAGE_EVERY_MS: u64 = 30_000;
/// How long the mds is waited for.
const MDS_WAIT_MS: u64 = 5_000;
/// How long the task sleeps at most: the grain of every time it keeps.
const TICK_MS: u64 = 100;
/// Requests in flight at once, at most; and those kept waiting for their
/// turn to begin: past it, refused for now.
const IN_FLIGHT_MAX: usize = 64;
const BACKLOG_MAX: usize = 1024;
/// The cs's mailbox: the requests it holds at most, and their bytes.
const MAILBOX_MESSAGES: u32 = 512;
const MAILBOX_BYTES: usize = 16 << 20;
/// Messages taken from the mailbox in a turn, at most.
const TAKE_MAX: usize = 2 * MAILBOX_MESSAGES as usize;
/// Answers to the copies' requests kept for them at most: past it dropped,
/// and asked again by the copy.
const COPY_ANSWERS_MAX: usize = 256;
/// Passes over the futures in a turn, at most, while something moves.
const PASSES_MAX: usize = 64;
/// How often what the shell shows is made again, at most: it walks the chunks.
const SHOW_EVERY_MS: u64 = 1_000;
const NS_PER_MS: u64 = 1_000_000;
/// An answer's tag, as it goes; and a request's, a copy's pull.
const ANSWER: u32 = TAG_ANSWER | TAG_BULK;
const REQUEST: u32 = TAG_REQUEST | TAG_BULK;
/// How long a copy done, or given up, waits before its end is asked of the
/// mds again where it could not be.
const END_AGAIN_MS: u64 = 1_000;
/// How long the requests in flight at the stop are given to end.
const STOP_GRACE_MS: u64 = 1_000;

/// A future the task drives: a request's, or one of the cs's own tasks.
type Task = Pin<Box<dyn Future<Output = ()>>>;

/// What the cs's futures share: each cell borrowed between two awaits,
/// never across one.
struct Cs {
    shared: Arc<Shared>,
    mb: Mailbox,
    store: Store<QueueDevice>,
    requests: Requests,
    /// The devices' buffers freed or reaped, counted (`disk.rs`).
    moves: Rc<Cell<u64>>,
    /// The questions out to the mds, by id: what each was told, once it was.
    next_id: Cell<u64>,
    told: RefCell<BTreeMap<u64, Option<Told>>>,
    /// Answers to the copies' requests, for the joins' task.
    copy_answers: RefCell<VecDeque<Message>>,
    /// Whose this host's cs is, as the mds last said.
    cluster: Cell<[u8; 16]>,
    cs: Cell<Option<u32>>,
    /// The metadata's devices taken in, and those that would not be: the
    /// device, its id, why.
    taken: RefCell<BTreeSet<u32>>,
    trouble: RefCell<Vec<(u32, String, String)>>,
    idents: RefCell<Vec<(u32, String)>>,
    /// Every device the metadata last gave the cs taken in: requests served.
    serving: Cell<bool>,
    /// A fence, a close, a free or a device since what the shell shows was
    /// made: made again at the turn's end rather than at the next second.
    changed: Cell<bool>,
    served: Cell<u64>,
    refused: Cell<u64>,
    /// Requests refused for the backlog full.
    overflowed: Cell<u64>,
}

/// A question out to the mds: its id let go of however the ask ends.
struct Asked<'a> {
    cs: &'a Cs,
    id: u64,
}

impl Drop for Asked<'_> {
    fn drop(&mut self) {
        self.cs.told.borrow_mut().remove(&self.id);
    }
}

impl Asker for Cs {
    fn ask(&self, question: Question) -> impl Future<Output = Option<Told>> + '_ {
        self.ask_mds(question)
    }
}

/// A request's id, where its bytes have one.
fn request_id(m: &Message) -> u64 {
    m.data.get(..8).map_or(0, |b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

/// Ready once `at` has come: a task's sleep, until a turn after it.
async fn until(at: u64) {
    poll_fn(|_| if boot_time_ns() >= at { Poll::Ready(()) } else { Poll::Pending }).await
}

pub fn run(shared: Arc<Shared>) {
    let mb = match Mailbox::new_on(Limits { messages: MAILBOX_MESSAGES, bytes: MAILBOX_BYTES }, &shared.event) {
        Ok(mb) => mb,
        Err(e) => {
            klog!(0, "nbs-cs: no mailbox: {}", e.name());
            shared.say("no mailbox -- the cs serves nothing");
            return;
        }
    };
    if let Err(e) = mb.register(CS_NAME, Scope::Cluster) {
        klog!(0, "nbs-cs: {} not named: {}", CS_NAME, e.name());
        shared.say("its mailbox not named -- another cs on this host?");
        return;
    }
    let cs = Rc::new(Cs {
        shared, mb, store: Store::new(), requests: Requests::new(), moves: Rc::new(Cell::new(0)), next_id: Cell::new(1),
        told: RefCell::new(BTreeMap::new()), copy_answers: RefCell::new(VecDeque::new()), cluster: Cell::new([0; 16]),
        cs: Cell::new(None), taken: RefCell::new(BTreeSet::new()), trouble: RefCell::new(Vec::new()),
        idents: RefCell::new(Vec::new()), serving: Cell::new(false), changed: Cell::new(false),
        served: Cell::new(0), refused: Cell::new(0), overflowed: Cell::new(0),
    });
    let mut own: Vec<Task> = Vec::new();
    own.push(Box::pin(devices(cs.clone())));
    own.push(Box::pin(joins(cs.clone())));
    let mut requests: Vec<Task> = Vec::new();
    let mut backlog: VecDeque<Message> = VecDeque::new();
    let mut shown_at = 0u64;
    while !cs.shared.stopping() {
        let mut busy = cs.take_messages(&mut backlog, false);
        while requests.len() < IN_FLIGHT_MAX {
            let Some(m) = backlog.pop_front() else { break };
            requests.push(Box::pin(request(cs.clone(), m)));
        }
        busy |= cs.drive(&mut [&mut requests, &mut own]);
        busy |= !backlog.is_empty() && requests.len() < IN_FLIGHT_MAX;
        if cs.changed.get() || boot_time_ns() >= shown_at {
            cs.changed.set(false);
            shown_at = boot_time_ns() + SHOW_EVERY_MS * NS_PER_MS;
            cs.show();
        }
        if !busy {
            cs.shared.event.wait_for(Duration::from_millis(TICK_MS));
        }
    }
    /* The stop: the cs's own tasks dropped -- a device's taking in, a
       copy's step given up; the requests in flight a moment to end, their
       answers sent, and the rest refused or dropped, their clients asking
       again elsewhere */
    drop(own);
    drop(backlog);
    let grace = boot_time_ns() + STOP_GRACE_MS * NS_PER_MS;
    while !requests.is_empty() && boot_time_ns() < grace {
        let mut none: VecDeque<Message> = VecDeque::new();
        cs.take_messages(&mut none, true);
        if !cs.drive(&mut [&mut requests]) {
            cs.shared.event.wait_for(Duration::from_millis(TICK_MS));
        }
    }
    drop(requests);
    /* Everything written so far durable before the queues go: its every
       step bounded by the device's time-out */
    let flushing = cs.clone();
    let mut last: Vec<Task> = Vec::new();
    last.push(Box::pin(async move {
        let _ = flushing.store.flush().await;
    }));
    while !last.is_empty() {
        if !cs.drive(&mut [&mut last]) && !last.is_empty() {
            cs.shared.event.wait_for(Duration::from_millis(TICK_MS));
        }
    }
}

/// A request, served by nbswire's `serve` and answered.
async fn request(cs: Rc<Cs>, m: Message) {
    let Some(req) = Request::decode(&m.data) else {
        cs.answer(&m, Answer { id: request_id(&m), reply: Reply::Refused(Refusal::Bad) });
        return;
    };
    if !cs.serving.get() {
        cs.answer(&m, Answer { id: req.id, reply: Reply::Refused(Refusal::NotReady) });
        return;
    }
    if matches!(req.op, Op::Fence | Op::Close { .. } | Op::CopyUp { .. }) {
        cs.changed.set(true);
    }
    match serve(&cs.store, &cs.requests, &req, &*cs).await {
        Ok(done) => cs.answer(&m, Answer { id: req.id, reply: done.reply() }),
        Err(why) => cs.answer(&m, Answer { id: req.id, reply: Reply::Refused(why) }),
    }
}

/// The cs's devices: taken in as the mds says, every few seconds; and what
/// the metadata no longer has freed, every half minute.
async fn devices(cs: Rc<Cs>) {
    let mut identity_at = 0u64;
    let mut garbage_at = 0u64;
    loop {
        if boot_time_ns() >= identity_at {
            cs.refresh().await;
            let every = if cs.serving.get() { IDENTITY_EVERY_MS } else { IDENTITY_MISSING_MS };
            identity_at = boot_time_ns() + every * NS_PER_MS;
        }
        let taken = !cs.taken.borrow().is_empty();
        if taken && boot_time_ns() >= garbage_at {
            garbage_at = boot_time_ns() + GARBAGE_EVERY_MS * NS_PER_MS;
            cs.collect().await;
        }
        until(boot_time_ns() + TICK_MS * NS_PER_MS).await;
    }
}

/// The joins this cs is the joiner of: asked of the mds every few seconds
/// while it serves, each copied -- the answers to its requests handed to
/// it as they come, its step taken every tick.
async fn joins(cs: Rc<Cs>) {
    let mut j = Joins { copies: Vec::new(), copy_id: 1, seen: false, end_at: 0 };
    let mut joins_at = 0u64;
    loop {
        if cs.serving.get() && boot_time_ns() >= joins_at {
            joins_at = boot_time_ns() + IDENTITY_EVERY_MS * NS_PER_MS;
            j.refresh(&cs).await;
        }
        loop {
            let next = cs.copy_answers.borrow_mut().pop_front();
            let Some(m) = next else { break };
            j.answer(&cs, &m).await;
        }
        if cs.serving.get() {
            j.turn(&cs).await;
        }
        let at = boot_time_ns() + TICK_MS * NS_PER_MS;
        poll_fn(|_| {
            if !cs.copy_answers.borrow().is_empty() || boot_time_ns() >= at { Poll::Ready(()) } else { Poll::Pending }
        }).await;
    }
}

impl Cs {
    /* ---- the loop ---- */

    /// What has moved since: the store's, the devices' and the chunks'
    /// orders'.
    fn moves(&self) -> u64 {
        self.store.moves().wrapping_add(self.moves.get()).wrapping_add(self.requests.moves())
    }

    /// Every future of `lists` polled, again while anything moved -- those
    /// done dropped: true where the passes ran out with things still moving.
    fn drive(&self, lists: &mut [&mut Vec<Task>]) -> bool {
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..PASSES_MAX {
            let before = self.moves();
            let mut ended = false;
            for list in lists.iter_mut() {
                list.retain_mut(|t| match t.as_mut().poll(&mut cx) {
                    Poll::Ready(()) => {
                        ended = true;
                        false
                    }
                    Poll::Pending => true,
                });
            }
            if !ended && self.moves() == before {
                return false;
            }
        }
        true
    }

    /// What has come, taken: an answer of the mds's handed to its question,
    /// an answer to a copy's request kept for the joins' task, a leader's
    /// look answered at once, a request kept in `backlog` -- or, past its
    /// bounds or at the stop, refused for now. Whether the mailbox may hold
    /// more than this turn took.
    fn take_messages(&self, backlog: &mut VecDeque<Message>, stopping: bool) -> bool {
        let mut bytes: usize = backlog.iter().map(|m| m.data.len()).sum();
        for _ in 0..TAKE_MAX {
            let Some(m) = self.mb.try_recv() else { return false };
            match m.tag & !TAG_BULK {
                TAG_TOLD => self.answered(&m),
                TAG_ANSWER => {
                    let mut answers = self.copy_answers.borrow_mut();
                    if answers.len() < COPY_ANSWERS_MAX {
                        answers.push_back(m);
                    }
                }
                TAG_REQUEST => {
                    let look = Request::decode(&m.data).is_some_and(|r| matches!(r.op, Op::Ping));
                    if look {
                        let reply = if self.serving.get() { Reply::Done } else { Reply::Refused(Refusal::NotReady) };
                        self.answer(&m, Answer { id: request_id(&m), reply });
                    } else if !stopping && backlog.len() < BACKLOG_MAX && bytes < MAILBOX_BYTES {
                        bytes += m.data.len();
                        backlog.push_back(m);
                    } else {
                        self.overflowed.set(self.overflowed.get() + 1);
                        self.answer(&m, Answer { id: request_id(&m), reply: Reply::Refused(Refusal::NotReady) });
                    }
                }
                /* A late answer of the mds's, or the channel's own: dropped */
                _ => {}
            }
        }
        true
    }

    /* ---- the mds ---- */

    /// `question` asked of this host's mds, and its answer: None when there
    /// is no mds, or it said nothing in time.
    async fn ask_mds(&self, question: Question) -> Option<Told> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let bytes = Query { id, question }.encode();
        self.mb.send_to("nbs.mds", TAG_QUERY, 0, &bytes).ok()?;
        self.told.borrow_mut().insert(id, None);
        let _asked = Asked { cs: self, id };
        let end = boot_time_ns() + MDS_WAIT_MS * NS_PER_MS;
        poll_fn(|_| {
            if let Some(told) = self.told.borrow_mut().get_mut(&id).and_then(Option::take) {
                return Poll::Ready(Some(told));
            }
            if boot_time_ns() >= end { Poll::Ready(None) } else { Poll::Pending }
        }).await
    }

    /// An answer of the mds's, handed to the question it answers: dropped
    /// where that one is no longer out.
    fn answered(&self, m: &Message) {
        let Some(a) = Answered::decode(&m.data) else { return };
        if let Some(slot) = self.told.borrow_mut().get_mut(&a.id) {
            *slot = Some(a.told);
        }
    }

    /// The devices the metadata gives this host's cs, taken in where they
    /// are not yet -- requests refused for now while one is missing.
    async fn refresh(&self) {
        let Some(Told::Identity(id)) = self.ask(Question::Identity).await else {
            if self.taken.borrow().is_empty() {
                self.shared.say("waiting for this host's mds");
            }
            return;
        };
        let Some(id) = id else {
            self.shared.say("no device of this host's in the metadata yet -- `nbs devices add`");
            return;
        };
        if self.cs.get().is_some_and(|cs| cs != id.cs) || (self.cs.get().is_some() && self.cluster.get() != id.cluster) {
            /* The metadata says this host's cs is another than the one its
               devices were taken in as: nothing more taken in, said */
            self.shared.say("the metadata names another cs for this host than its devices say -- reload the module");
            return;
        }
        self.cluster.set(id.cluster);
        self.cs.set(Some(id.cs));
        let missing = |cs: &Cs| id.devices.iter().filter(|d| !cs.taken.borrow().contains(&d.device)).count();
        if missing(self) != 0 {
            self.serving.set(false);
        }
        for d in &id.devices {
            if self.taken.borrow().contains(&d.device) {
                if let Some(i) = self.store.device_index(d.device) {
                    self.store.set_active(i, !d.draining);
                }
                continue;
            }
            let ident = DevIdentity { cluster: id.cluster, cs: id.cs, device: d.device };
            match self.take(&d.ident, ident, d.slots).await {
                Ok(how) => {
                    klog!(1, "nbs-cs: device {} ({}) {}", d.device, d.ident, how);
                    self.taken.borrow_mut().insert(d.device);
                    self.changed.set(true);
                    self.idents.borrow_mut().push((d.device, d.ident.clone()));
                    self.trouble.borrow_mut().retain(|(n, _, _)| *n != d.device);
                    if let Some(i) = self.store.device_index(d.device) {
                        self.store.set_active(i, !d.draining);
                    }
                }
                /* Said once for each reason, and tried again next time */
                Err(why) => {
                    let mut trouble = self.trouble.borrow_mut();
                    if !trouble.iter().any(|(n, _, w)| *n == d.device && *w == why) {
                        klog!(0, "nbs-cs: device {} ({}) not taken: {}", d.device, d.ident, why);
                        trouble.retain(|(n, _, _)| *n != d.device);
                        trouble.push((d.device, d.ident.clone(), why));
                    }
                }
            }
        }
        let missing = missing(self);
        let serving = !id.devices.is_empty() && missing == 0;
        self.serving.set(serving);
        if serving {
            self.shared.say("serving");
        } else {
            self.shared.say(&alloc::format!("not serving: {} of its {} devices not taken in -- `nbscs` says why",
                                            missing, id.devices.len()));
        }
    }

    /// Device `name` taken in as `ident`'s: opened, or formatted if blank.
    async fn take(&self, name: &str, ident: DevIdentity, slots: u32) -> Result<&'static str, String> {
        let dev = QueueDevice::open(name, &self.shared.event, self.moves.clone())?;
        let have = nbscs::device_slots(dev.bytes());
        if have != slots {
            return Err(alloc::format!("{} slots on it, {} in the metadata -- the device changed size", have, slots));
        }
        match self.store.open_device(dev, ident).await {
            Ok((_, rep)) => {
                if rep.lost != 0 {
                    klog!(0, "nbs-cs: {}: {} slots' records lost, those slots never used again", name, rep.lost);
                }
                Ok("opened")
            }
            Err((dev, OpenError::Blank)) => {
                let format = kcore::random::random_u64().filter(|&f| f != 0)
                    .ok_or_else(|| String::from("no randomness for its format's id"))?;
                self.store.format_device(dev, ident, format).await.map(|_| "formatted")
                    .map_err(|(_, e)| alloc::format!("not formatted: {:?}", e))
            }
            Err((_, OpenError::Foreign(other))) => Err(alloc::format!(
                "NBS's device of another cluster or cs (its cs {}, device {}) -- a disk moved here", other.cs,
                other.device)),
            Err((_, OpenError::Corrupt(why))) => Err(alloc::format!("{} -- it is left as it is", why)),
            Err((_, OpenError::Small)) => Err(String::from("too small for a slot")),
            Err((_, OpenError::Io)) => Err(String::from("it failed to read")),
        }
    }

    /// Chunks the metadata freed, or whose number is another chunk's now,
    /// freed here; fences of volumes gone taken off (nbswire's `collect`).
    async fn collect(&self) {
        let done = collect(&self.store, &self.requests, self).await;
        for &(c, inc) in &done.freed {
            klog!(2, "nbs-cs: chunk {} ({}) freed", c, inc);
        }
        for &(n, e) in &done.failed {
            klog!(0, "nbs-cs: chunk {} or volume {}'s fence not let go of: {:?}", n, n, e);
        }
        if !done.freed.is_empty() || !done.unfenced.is_empty() {
            self.changed.set(true);
        }
    }

    /* ---- requests ---- */

    fn answer(&self, m: &Message, a: Answer<'_>) {
        /* A read's answer is a piece's data: refused for now where the heap
           has no room for it -- its client asks again in a moment (#97) --
           never the host's panic */
        let (bytes, refused) = match a.try_encode() {
            Some(b) => (b, matches!(a.reply, Reply::Refused(_))),
            None => (Answer { id: a.id, reply: Reply::Refused(Refusal::NotReady) }.encode(), true),
        };
        if refused {
            self.refused.set(self.refused.get() + 1);
        } else {
            self.served.set(self.served.get() + 1);
        }
        let _ = self.mb.reply(m, ANSWER, &bytes);
    }

    /// What the shell shows of the cs.
    fn show(&self) {
        let idents = self.idents.borrow();
        let devices: Vec<DeviceShown> = self.store.devices().iter().map(|d| DeviceShown {
            device: d.device,
            ident: idents.iter().find(|(n, _)| *n == d.device).map(|(_, i)| i.clone()).unwrap_or_default(),
            slots: d.slots,
            free: d.free,
            lost: d.lost,
            failed: d.failed,
            active: d.active,
            trouble: None,
        }).chain(self.trouble.borrow().iter().map(|(n, ident, why)| DeviceShown {
            device: *n, ident: ident.clone(), slots: 0, free: 0, lost: 0, failed: true, active: false,
            trouble: Some(why.clone()),
        })).collect();
        drop(idents);
        let mut held = self.store.holdings();
        let open = held.iter().filter(|h| h.state == SlotState::Open).count() as u64;
        let count = held.len() as u64;
        held.truncate(CHUNKS_SHOWN);
        self.shared.show(Shown { cs: self.cs.get(), devices, held: count, open, chunks: held,
                                 fences: self.store.fenced_volumes(), served: self.served.get(),
                                 refused: self.refused.get() + self.overflowed.get() });
    }
}

/* ---- joins (N9) ---- */

/// The joins' task's own: the copies, their requests' ids, whether the
/// joins found as it started were given up, when a copy's end may be asked
/// again.
struct Joins {
    copies: Vec<Copy>,
    copy_id: u64,
    seen: bool,
    end_at: u64,
}

impl Joins {
    /// The joins this cs is the joiner of, as the mds has them: a copy
    /// begun for a new one -- its writes marked from now on -- refreshed for
    /// one it has, and dropped for one gone, its marks with it.
    async fn refresh(&mut self, cs: &Cs) {
        let Some(me) = cs.cs.get() else { return };
        let Some(Told::Joins(views)) = cs.ask(Question::Joins).await else { return };
        if !self.seen {
            self.seen = true;
            for v in &views {
                klog!(0, "nbs-cs: the join of set {} given up: the cs started again", v.set);
                self.end(cs, v.set, v.since, false, None).await;
            }
            return;
        }
        let mut kept = Vec::new();
        for c in core::mem::take(&mut self.copies) {
            if views.iter().any(|v| v.set == c.set() && v.since == c.since()) {
                kept.push(c);
            } else {
                klog!(1, "nbs-cs: the join of set {} is over", c.set());
                cs.store.leave(c.set());
            }
        }
        self.copies = kept;
        /* Marks a client's write began for a join that is over */
        for (set, since) in cs.store.joined() {
            if !views.iter().any(|v| v.set == set && v.since == since) {
                cs.store.leave(set);
            }
        }
        let now = boot_time_ns();
        for v in views {
            match self.copies.iter_mut().find(|c| c.set() == v.set && c.since() == v.since) {
                Some(c) => c.refresh(v),
                None => {
                    klog!(1, "nbs-cs: joining set {} in place {} from its {} active members", v.set, v.place,
                          v.sources.len());
                    cs.store.join(v.set, v.since);
                    self.copies.push(Copy::new(v, me, now));
                }
            }
        }
    }

    /// The copies' turn: what each asks sent, and each done or given up
    /// asked of the mds.
    async fn turn(&mut self, cs: &Cs) {
        let now = boot_time_ns();
        let mut out = Vec::new();
        for c in self.copies.iter_mut() {
            c.step(&cs.store, now, &mut self.copy_id, &mut out).await;
        }
        Joins::send(cs, out);
        if boot_time_ns() < self.end_at {
            return;
        }
        let ended: Vec<(u32, u32, bool, copy::Stats)> = self.copies.iter().filter_map(|c| match c.state() {
            copy::State::Done => Some((c.set(), c.since(), true, c.stats)),
            copy::State::Failed(_) => Some((c.set(), c.since(), false, c.stats)),
            _ => None,
        }).collect();
        for (set, since, done, stats) in ended {
            self.end(cs, set, since, done, Some(stats)).await;
        }
    }

    /// Join `set` begun at `since` ended -- `done`, the cs taking its place,
    /// or given up -- as the mds proposes it: its copy and its marks dropped
    /// once that is applied, or once it is not the join it was; asked again
    /// in a moment where the mds could not. A join done says what its copy
    /// did: a member back pulls whole only what it may have missed (N9f).
    async fn end(&mut self, cs: &Cs, set: u32, since: u32, done: bool, stats: Option<copy::Stats>) {
        let question = if done { Question::JoinDone { set, since } } else { Question::GiveUp { set, since } };
        match cs.ask(question).await {
            Some(Told::Verified) => {
                match (done, stats) {
                    (true, Some(s)) => klog!(0, "nbs-cs: set {} joined: this cs is its member now -- {} chunks copied, \
                                                {} of them found as the source's, {} bytes pulled, {} granules \
                                                pulled again", set, s.chunks, s.kept, s.bytes, s.again),
                    (true, None) => klog!(0, "nbs-cs: set {} joined: this cs is its member now", set),
                    (false, _) => {}
                }
            }
            Some(Told::Refused(Refusal::NotTheJoiner)) => klog!(1, "nbs-cs: the join of set {} was over", set),
            _ => {
                self.end_at = boot_time_ns() + END_AGAIN_MS * NS_PER_MS;
                return;
            }
        }
        self.copies.retain(|c| !(c.set() == set && c.since() == since));
        cs.store.leave(set);
        cs.changed.set(true);
    }

    /// An answer to a copy's request.
    async fn answer(&mut self, cs: &Cs, m: &Message) {
        let Some(a) = Answer::decode(&m.data) else { return };
        let now = boot_time_ns();
        let mut out = Vec::new();
        if let Some(c) = self.copies.iter_mut().find(|c| c.sent(a.id)) {
            c.answer(&cs.store, a.id, &a.reply, now, &mut out).await;
            c.step(&cs.store, now, &mut self.copy_id, &mut out).await;
        }
        Joins::send(cs, out);
    }

    fn send(cs: &Cs, out: Vec<copy::Out>) {
        for o in out {
            match o {
                /* A source on another host always: a set's three are on
                   three, and its joiner is none of them. Not sent -- no way
                   there now -- it is asked again once its answer is late */
                copy::Out::Send { host, bytes } => {
                    let to = alloc::format!("{}@{}", CS_NAME, host);
                    let _ = cs.mb.send_to(&to, REQUEST, 0, &bytes);
                }
                copy::Out::Say(line) => klog!(0, "{}", line),
            }
        }
    }
}
