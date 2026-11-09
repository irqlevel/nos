//! The cs's task: the devices the metadata gives this host's cs taken in,
//! and the requests of NBS's clients served from them.
//!
//! - **Its devices**, from this host's mds (`Question::Identity`), asked
//!   again every few seconds: each taken in -- opened, or formatted where it
//!   is blank -- and one that is NBS's but another cluster's or cs's, or
//!   holds something else, left as it is, said, and tried again each time:
//!   `nbs devices add ... wipe` clears it, a claim that was in the way goes.
//! - **Requests** on `nbs.cs`, one at a time, each answered to the mailbox
//!   it came from -- and only once every device the metadata gives the cs
//!   is taken in: with one missing, a chunk on it would read as nothing,
//!   and its first write would make it a second slot, which the device
//!   coming back would contend with. Until then, `NotReady`. A request is
//!   done by nbswire's `serve`, the one copy the cluster fuzz target's cs's
//!   run too: one the engine has not checked -- a chunk it does not hold, an
//!   epoch other than the one it knows -- is asked of the mds
//!   (`Question::Verify`) first.
//! - **What the metadata no longer has** freed every half minute: chunks
//!   freed or their number another chunk's now, fences of volumes gone.
//! - **The joins** this cs is the joiner of (N9), asked of the mds with its
//!   devices: each copied by nbswire's `copy`, its pulls sent to the set's
//!   active members and their answers handed back, its end asked of the
//!   mds. A join found as the cs starts is given up: what clients wrote to
//!   it was marked in memory alone, and a copy without those marks could
//!   write over them.
//!
//! While it waits for the mds, what else comes is kept, in order, and served
//! after. A client's requests and their answers go between hosts by the
//! channel's bulk connection (`TAG_BULK`), all of them, so that they keep
//! their order and their data do not hold up raft's messages.

use alloc::collections::{BTreeSet, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use kcore::ipc::{Limits, Mailbox, Message, Scope, TAG_BULK};
use kcore::klog;
use kcore::time::{boot_time_ns, Duration};
use nbscs::{Identity as DevIdentity, OpenError, SlotState, Store};
use nbswire::copy::{self, Copy};
use nbswire::serve::{serve, Vouched};
use nbswire::{Answer, Answered, Op, Query, Question, Refusal, Reply, Request, Told, CS_NAME, LIST_MAX, TAG_ANSWER,
              TAG_QUERY, TAG_REQUEST, TAG_TOLD};

use crate::disk::QueueDevice;
use crate::{DeviceShown, Shared, Shown, CHUNKS_SHOWN};

/// How often the devices are asked of the mds again, and the gone freed.
const IDENTITY_EVERY_MS: u64 = 5_000;
const GARBAGE_EVERY_MS: u64 = 30_000;
/// How long the mds is waited for.
const MDS_WAIT_MS: u64 = 5_000;
/// How long a turn waits for a request.
const TURN_MS: u64 = 500;
/// What came while the mds was asked, kept at most: past it, refused.
const BACKLOG_MAX: usize = 1024;
/// How often what the shell shows is made again, at most: it walks the chunks.
const SHOW_EVERY_MS: u64 = 1_000;
const NS_PER_MS: u64 = 1_000_000;
/// An answer's tag, as it goes; and a request's, a copy's pull.
const ANSWER: u32 = TAG_ANSWER | TAG_BULK;
const REQUEST: u32 = TAG_REQUEST | TAG_BULK;
/// How long a copy done, or given up, waits before its end is asked of the
/// mds again where it could not be.
const END_AGAIN_MS: u64 = 1_000;

/// The cs's mailbox, and its asks of this host's mds: a part of its own, so
/// that a request's check asks the mds while the store is lent to `serve`.
struct Link {
    shared: Arc<Shared>,
    mb: Mailbox,
    next_id: u64,
    backlog: VecDeque<Message>,
    /// Requests refused for a backlog full.
    overflowed: u64,
}

impl Link {
    fn stopping(&self) -> bool {
        self.shared.stopping()
    }

    /// `question` asked of this host's mds, and its answer: None when there
    /// is no mds, or it said nothing in time.
    fn ask(&mut self, question: Question) -> Option<Told> {
        let id = self.next_id;
        self.next_id += 1;
        let bytes = Query { id, question }.encode();
        self.mb.send_to("nbs.mds", TAG_QUERY, 0, &bytes).ok()?;
        let end = boot_time_ns() + MDS_WAIT_MS * NS_PER_MS;
        while boot_time_ns() < end && !self.stopping() {
            let Some(m) = self.mb.recv(Duration::from_millis(TURN_MS)) else { continue };
            if m.tag & !TAG_BULK == TAG_TOLD {
                if let Some(a) = Answered::decode(&m.data).filter(|a| a.id == id) {
                    return Some(a.told);
                }
                continue;
            }
            if self.backlog.len() < BACKLOG_MAX {
                self.backlog.push_back(m);
            } else if m.tag & !TAG_BULK == TAG_REQUEST {
                self.overflowed += 1;
                let _ = self.mb.reply(&m, ANSWER, &Answer { id: request_id(&m), reply: Reply::Refused(Refusal::NotReady) }
                    .encode());
            }
        }
        None
    }
}

/// A request's id, where its bytes have one.
fn request_id(m: &Message) -> u64 {
    m.data.get(..8).map_or(0, |b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

struct Server {
    link: Link,
    store: Store<QueueDevice>,
    vouched: Vouched,
    /// Whose this host's cs is, as the mds last said.
    cluster: [u8; 16],
    cs: Option<u32>,
    /// The metadata's devices taken in, and those that would not be: the
    /// device, its id, why.
    taken: BTreeSet<u32>,
    trouble: Vec<(u32, String, String)>,
    /// Every device the metadata last gave the cs taken in: requests served.
    serving: bool,
    /// A fence, a close, a free or a device since what the shell shows was
    /// made: made again at the turn's end rather than at the next second.
    changed: bool,
    idents: Vec<(u32, String)>,
    identity_at: u64,
    garbage_at: u64,
    shown_at: u64,
    served: u64,
    refused: u64,
    /// The joins this cs is the joiner of, each being copied (N9); the
    /// copies' request ids; whether the joins found as it started were
    /// given up; when a copy's end may be asked again.
    copies: Vec<Copy>,
    copy_id: u64,
    joins_seen: bool,
    end_at: u64,
}

pub fn run(shared: Arc<Shared>) {
    let mb = match Mailbox::new_on(Limits { messages: 512, bytes: 16 << 20 }, &shared.event) {
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
    let link = Link { shared, mb, next_id: 1, backlog: VecDeque::new(), overflowed: 0 };
    let mut s = Server {
        link, store: Store::new(), vouched: Vouched::new(), cluster: [0; 16], cs: None, taken: BTreeSet::new(),
        trouble: Vec::new(), serving: false, changed: false, idents: Vec::new(), identity_at: 0, garbage_at: 0,
        shown_at: 0, served: 0, refused: 0, copies: Vec::new(), copy_id: 1, joins_seen: false, end_at: 0,
    };
    s.main();
    /* Everything written so far durable before the queues go */
    let _ = s.store.flush();
}

impl Server {
    fn main(&mut self) {
        while !self.link.stopping() {
            let now = boot_time_ns();
            if now >= self.identity_at {
                self.identity_at = now + IDENTITY_EVERY_MS * NS_PER_MS;
                self.refresh();
                if self.serving {
                    self.joins();
                }
            }
            if now >= self.garbage_at && !self.taken.is_empty() {
                self.garbage_at = now + GARBAGE_EVERY_MS * NS_PER_MS;
                self.collect();
            }
            let m = match self.link.backlog.pop_front() {
                Some(m) => Some(m),
                None => self.link.mb.recv(Duration::from_millis(TURN_MS)),
            };
            if let Some(m) = m {
                match m.tag & !TAG_BULK {
                    TAG_REQUEST => self.request(&m),
                    TAG_ANSWER => self.copy_answer(&m),
                    /* A late answer of the mds's, or the channel's own: dropped */
                    _ => {}
                }
            }
            if self.serving {
                self.copy_turn();
            }
            if self.changed || boot_time_ns() >= self.shown_at {
                self.changed = false;
                self.shown_at = boot_time_ns() + SHOW_EVERY_MS * NS_PER_MS;
                self.show();
            }
        }
    }

    /* ---- the mds ---- */

    /// The devices the metadata gives this host's cs, taken in where they
    /// are not yet.
    fn refresh(&mut self) {
        let Some(Told::Identity(id)) = self.link.ask(Question::Identity) else {
            if self.taken.is_empty() {
                self.link.shared.say("waiting for this host's mds");
            }
            return;
        };
        let Some(id) = id else {
            self.link.shared.say("no device of this host's in the metadata yet -- `nbs devices add`");
            return;
        };
        if self.cs.is_some_and(|cs| cs != id.cs) || (self.cs.is_some() && self.cluster != id.cluster) {
            /* The metadata says this host's cs is another than the one its
               devices were taken in as: nothing more taken in, said */
            self.link.shared.say("the metadata names another cs for this host than its devices say -- reload the \
                                  module");
            return;
        }
        self.cluster = id.cluster;
        self.cs = Some(id.cs);
        for d in &id.devices {
            if self.taken.contains(&d.device) {
                if let Some(i) = self.store.device_index(d.device) {
                    self.store.set_active(i, !d.draining);
                }
                continue;
            }
            let ident = DevIdentity { cluster: id.cluster, cs: id.cs, device: d.device };
            match self.take(&d.ident, ident, d.slots) {
                Ok(how) => {
                    klog!(1, "nbs-cs: device {} ({}) {}", d.device, d.ident, how);
                    self.taken.insert(d.device);
                    self.changed = true;
                    self.idents.push((d.device, d.ident.clone()));
                    self.trouble.retain(|(n, _, _)| *n != d.device);
                    if let Some(i) = self.store.device_index(d.device) {
                        self.store.set_active(i, !d.draining);
                    }
                }
                /* Said once for each reason, and tried again next time */
                Err(why) => {
                    if !self.trouble.iter().any(|(n, _, w)| *n == d.device && *w == why) {
                        klog!(0, "nbs-cs: device {} ({}) not taken: {}", d.device, d.ident, why);
                        self.trouble.retain(|(n, _, _)| *n != d.device);
                        self.trouble.push((d.device, d.ident.clone(), why));
                    }
                }
            }
        }
        let missing = id.devices.iter().filter(|d| !self.taken.contains(&d.device)).count();
        self.serving = !id.devices.is_empty() && missing == 0;
        if self.serving {
            self.link.shared.say("serving");
        } else {
            self.link.shared.say(&alloc::format!("not serving: {} of its {} devices not taken in -- `nbscs` says why",
                                                 missing, id.devices.len()));
        }
    }

    /// Device `name` taken in as `ident`'s: opened, or formatted if blank.
    fn take(&mut self, name: &str, ident: DevIdentity, slots: u32) -> Result<&'static str, String> {
        let dev = QueueDevice::open(name, &self.link.shared.event)?;
        let have = nbscs::device_slots(nbscs::Device::bytes(&dev));
        if have != slots {
            return Err(alloc::format!("{} slots on it, {} in the metadata -- the device changed size", have, slots));
        }
        match self.store.open_device(dev, ident) {
            Ok((_, rep)) => {
                if rep.lost != 0 {
                    klog!(0, "nbs-cs: {}: {} slots' records lost, those slots never used again", name, rep.lost);
                }
                Ok("opened")
            }
            Err((dev, OpenError::Blank)) => {
                let format = kcore::random::random_u64().filter(|&f| f != 0)
                    .ok_or_else(|| String::from("no randomness for its format's id"))?;
                self.store.format_device(dev, ident, format).map(|_| "formatted")
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
    /// freed here; fences of volumes gone taken off.
    fn collect(&mut self) {
        let held: Vec<(u32, u32)> = self.store.holdings().iter().map(|h| (h.chunk, h.incarnation)).collect();
        for batch in held.chunks(LIST_MAX) {
            let Some(Told::Gone(gone)) = self.link.ask(Question::Live { chunks: batch.to_vec() }) else { return };
            for c in gone {
                if let Some(&(_, inc)) = batch.iter().find(|(n, _)| *n == c) {
                    match self.store.free(c, inc) {
                        Ok(true) => {
                            klog!(2, "nbs-cs: chunk {} ({}) freed", c, inc);
                            self.changed = true;
                        }
                        Ok(false) => {}
                        Err(e) => klog!(0, "nbs-cs: chunk {} not freed: {:?}", c, e),
                    }
                }
            }
        }
        let volumes: Vec<u32> = self.store.fenced_volumes().iter().map(|&(v, _)| v).collect();
        for batch in volumes.chunks(LIST_MAX) {
            let Some(Told::Gone(gone)) = self.link.ask(Question::Volumes { volumes: batch.to_vec() }) else { return };
            for v in gone {
                self.changed = true;
                if let Err(e) = self.store.drop_fence(v) {
                    klog!(0, "nbs-cs: volume {}'s fence not taken off: {:?}", v, e);
                }
            }
        }
    }

    /* ---- joins (N9) ---- */

    /// The joins this cs is the joiner of, as the mds has them: a copy
    /// begun for a new one -- its writes marked from now on -- refreshed for
    /// one it has, and dropped for one gone, its marks with it.
    fn joins(&mut self) {
        let (Some(me), Some(Told::Joins(views))) = (self.cs, self.link.ask(Question::Joins)) else { return };
        if !self.joins_seen {
            self.joins_seen = true;
            for v in &views {
                klog!(0, "nbs-cs: the join of set {} given up: the cs started again", v.set);
                self.end(v.set, v.since, false, None);
            }
            return;
        }
        let mut kept = Vec::new();
        for c in core::mem::take(&mut self.copies) {
            if views.iter().any(|v| v.set == c.set() && v.since == c.since()) {
                kept.push(c);
            } else {
                klog!(1, "nbs-cs: the join of set {} is over", c.set());
                self.store.leave(c.set());
            }
        }
        self.copies = kept;
        /* Marks a client's write began for a join that is over */
        for (set, since) in self.store.joined() {
            if !views.iter().any(|v| v.set == set && v.since == since) {
                self.store.leave(set);
            }
        }
        let now = boot_time_ns();
        for v in views {
            match self.copies.iter_mut().find(|c| c.set() == v.set && c.since() == v.since) {
                Some(c) => c.refresh(v),
                None => {
                    klog!(1, "nbs-cs: joining set {} in place {} from its {} active members", v.set, v.place,
                          v.sources.len());
                    self.store.join(v.set, v.since);
                    self.copies.push(Copy::new(v, me, now));
                }
            }
        }
    }

    /// The copies' turn: what each asks sent, and each done or given up
    /// asked of the mds.
    fn copy_turn(&mut self) {
        let now = boot_time_ns();
        let mut out = Vec::new();
        for c in self.copies.iter_mut() {
            c.step(&mut self.store, now, &mut self.copy_id, &mut out);
        }
        self.copy_out(out);
        if now < self.end_at {
            return;
        }
        let ended: Vec<(u32, u32, bool, copy::Stats)> = self.copies.iter().filter_map(|c| match c.state() {
            copy::State::Done => Some((c.set(), c.since(), true, c.stats)),
            copy::State::Failed(_) => Some((c.set(), c.since(), false, c.stats)),
            _ => None,
        }).collect();
        for (set, since, done, stats) in ended {
            self.end(set, since, done, Some(stats));
        }
    }

    /// Join `set` begun at `since` ended -- `done`, the cs taking its place,
    /// or given up -- as the mds proposes it: its copy and its marks dropped
    /// once that is applied, or once it is not the join it was; asked again
    /// in a moment where the mds could not. A join done says what its copy
    /// did: a member back pulls whole only what it may have missed (N9f).
    fn end(&mut self, set: u32, since: u32, done: bool, stats: Option<copy::Stats>) {
        let question = if done { Question::JoinDone { set, since } } else { Question::GiveUp { set, since } };
        match self.link.ask(question) {
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
        self.store.leave(set);
        self.changed = true;
    }

    /// An answer to a copy's request.
    fn copy_answer(&mut self, m: &Message) {
        let Some(a) = Answer::decode(&m.data) else { return };
        let now = boot_time_ns();
        let mut out = Vec::new();
        if let Some(c) = self.copies.iter_mut().find(|c| c.sent(a.id)) {
            c.answer(&mut self.store, a.id, &a.reply, now, &mut out);
            c.step(&mut self.store, now, &mut self.copy_id, &mut out);
        }
        self.copy_out(out);
    }

    fn copy_out(&mut self, out: Vec<copy::Out>) {
        for o in out {
            match o {
                /* A source on another host always: a set's three are on
                   three, and its joiner is none of them. Not sent -- no way
                   there now -- it is asked again once its answer is late */
                copy::Out::Send { host, bytes } => {
                    let to = alloc::format!("{}@{}", CS_NAME, host);
                    let _ = self.link.mb.send_to(&to, REQUEST, 0, &bytes);
                }
                copy::Out::Say(line) => klog!(0, "{}", line),
            }
        }
    }

    /* ---- requests ---- */

    fn answer(&mut self, m: &Message, a: Answer<'_>) {
        if matches!(a.reply, Reply::Refused(_)) {
            self.refused += 1;
        } else {
            self.served += 1;
        }
        let _ = self.link.mb.reply(m, ANSWER, &a.encode());
    }

    fn request(&mut self, m: &Message) {
        let Some(req) = Request::decode(&m.data) else {
            self.answer(m, Answer { id: request_id(m), reply: Reply::Refused(Refusal::Bad) });
            return;
        };
        if !self.serving {
            self.answer(m, Answer { id: req.id, reply: Reply::Refused(Refusal::NotReady) });
            return;
        }
        if matches!(req.op, Op::Fence | Op::Close { .. } | Op::CopyUp { .. }) {
            self.changed = true;
        }
        let link = &mut self.link;
        match serve(&mut self.store, &mut self.vouched, &req, &mut |q| link.ask(q)) {
            Ok(done) => self.answer(m, Answer { id: req.id, reply: done.reply() }),
            Err(why) => self.answer(m, Answer { id: req.id, reply: Reply::Refused(why) }),
        }
    }

    /// What the shell shows of the cs.
    fn show(&self) {
        let devices: Vec<DeviceShown> = self.store.devices().iter().map(|d| DeviceShown {
            device: d.device,
            ident: self.idents.iter().find(|(n, _)| *n == d.device).map(|(_, i)| i.clone()).unwrap_or_default(),
            slots: d.slots,
            free: d.free,
            lost: d.lost,
            failed: d.failed,
            active: d.active,
            trouble: None,
        }).chain(self.trouble.iter().map(|(n, ident, why)| DeviceShown {
            device: *n, ident: ident.clone(), slots: 0, free: 0, lost: 0, failed: true, active: false,
            trouble: Some(why.clone()),
        })).collect();
        let mut held = self.store.holdings();
        let open = held.iter().filter(|h| h.state == SlotState::Open).count() as u64;
        let count = held.len() as u64;
        held.truncate(CHUNKS_SHOWN);
        self.link.shared.show(Shown { cs: self.cs, devices, held: count, open, chunks: held,
                                      fences: self.store.fenced_volumes(), served: self.served,
                                      refused: self.refused + self.link.overflowed });
    }
}
