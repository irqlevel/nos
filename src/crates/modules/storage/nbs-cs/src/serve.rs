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
//!   coming back would contend with. Until then, `NotReady`. One the engine has not checked -- a chunk it does not
//!   hold, an epoch other than the one it knows -- is asked of the mds
//!   (`Question::Verify`) first: the volume's, at that place of its map, at
//!   that incarnation, this cs in its set, at the request's entry at least.
//! - **What the metadata no longer has** freed every half minute: chunks
//!   freed or their number another chunk's now, fences of volumes gone.
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
use nbscs::{ChunkRef, Error as CsError, Identity as DevIdentity, OpenError, SlotState, Store};
use nbswire::{Answer, Answered, Op, Query, Question, Refusal, Reply, Request, Told, CS_NAME, LIST_MAX,
              TAG_ANSWER, TAG_QUERY, TAG_REQUEST, TAG_TOLD};

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
/// Checks the mds vouched for, kept at most.
const VERIFIED_MAX: usize = 65536;
/// How often what the shell shows is made again, at most: it walks the chunks.
const SHOW_EVERY_MS: u64 = 1_000;
const NS_PER_MS: u64 = 1_000_000;
/// An answer's tag, as it goes.
const ANSWER: u32 = TAG_ANSWER | TAG_BULK;

/// What a check the mds vouched for was for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Checked {
    volume: u32,
    generation: u64,
    chunk: u32,
    incarnation: u32,
    set: u32,
    version: u32,
}

struct Server {
    shared: Arc<Shared>,
    mb: Mailbox,
    store: Store<QueueDevice>,
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
    next_id: u64,
    backlog: VecDeque<Message>,
    verified: BTreeSet<Checked>,
    identity_at: u64,
    garbage_at: u64,
    shown_at: u64,
    served: u64,
    refused: u64,
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
    let mut s = Server {
        shared, mb, store: Store::new(), cluster: [0; 16], cs: None, taken: BTreeSet::new(), trouble: Vec::new(),
        serving: false, changed: false, idents: Vec::new(), next_id: 1, backlog: VecDeque::new(), verified: BTreeSet::new(), identity_at: 0,
        garbage_at: 0, shown_at: 0, served: 0, refused: 0,
    };
    s.main();
    /* Everything written so far durable before the queues go */
    let _ = s.store.flush();
}

impl Server {
    fn stopping(&self) -> bool {
        self.shared.stopping()
    }

    fn main(&mut self) {
        while !self.stopping() {
            let now = boot_time_ns();
            if now >= self.identity_at {
                self.identity_at = now + IDENTITY_EVERY_MS * NS_PER_MS;
                self.refresh();
            }
            if now >= self.garbage_at && !self.taken.is_empty() {
                self.garbage_at = now + GARBAGE_EVERY_MS * NS_PER_MS;
                self.collect();
            }
            let m = match self.backlog.pop_front() {
                Some(m) => Some(m),
                None => self.mb.recv(Duration::from_millis(TURN_MS)),
            };
            if let Some(m) = m {
                if m.tag & !TAG_BULK == TAG_REQUEST {
                    self.request(&m);
                }
                /* A late answer of the mds's, or the channel's own: dropped */
            }
            if self.changed || boot_time_ns() >= self.shown_at {
                self.changed = false;
                self.shown_at = boot_time_ns() + SHOW_EVERY_MS * NS_PER_MS;
                self.show();
            }
        }
    }

    /* ---- the mds ---- */

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
                self.answer_refused(&m, Refusal::NotReady);
            }
        }
        None
    }

    /// The devices the metadata gives this host's cs, taken in where they
    /// are not yet.
    fn refresh(&mut self) {
        let Some(Told::Identity(id)) = self.ask(Question::Identity) else {
            if self.taken.is_empty() {
                self.shared.say("waiting for this host's mds");
            }
            return;
        };
        let Some(id) = id else {
            self.shared.say("no device of this host's in the metadata yet -- `nbs devices add`");
            return;
        };
        if self.cs.is_some_and(|cs| cs != id.cs) || (self.cs.is_some() && self.cluster != id.cluster) {
            /* The metadata says this host's cs is another than the one its
               devices were taken in as: nothing more taken in, said */
            self.shared.say("the metadata names another cs for this host than its devices say -- reload the module");
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
            self.shared.say("serving");
        } else {
            self.shared.say(&alloc::format!("not serving: {} of its {} devices not taken in -- `nbscs` says why",
                                            missing, id.devices.len()));
        }
    }

    /// Device `name` taken in as `ident`'s: opened, or formatted if blank.
    fn take(&mut self, name: &str, ident: DevIdentity, slots: u32) -> Result<&'static str, String> {
        let dev = QueueDevice::open(name, &self.shared.event)?;
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
            let Some(Told::Gone(gone)) = self.ask(Question::Live { chunks: batch.to_vec() }) else { return };
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
            let Some(Told::Gone(gone)) = self.ask(Question::Volumes { volumes: batch.to_vec() }) else { return };
            for v in gone {
                self.changed = true;
                if let Err(e) = self.store.drop_fence(v) {
                    klog!(0, "nbs-cs: volume {}'s fence not taken off: {:?}", v, e);
                }
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
        let _ = self.mb.reply(m, ANSWER, &a.encode());
    }

    fn answer_refused(&mut self, m: &Message, why: Refusal) {
        /* The request's id, where its bytes have one */
        let id = m.data.get(..8).map_or(0, |b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]));
        self.answer(m, Answer { id, reply: Reply::Refused(why) });
    }

    fn request(&mut self, m: &Message) {
        let Some(req) = Request::decode(&m.data) else {
            self.answer_refused(m, Refusal::Bad);
            return;
        };
        if !self.serving {
            self.answer_refused(m, Refusal::NotReady);
            return;
        }
        let (v, g, id) = (req.volume, req.generation, req.id);
        match req.op {
            Op::Fence => {
                self.changed = true;
                let reply = match self.store.fence(v, g) {
                    Ok(now) => Reply::Fenced { generation: now },
                    Err(e) => Reply::Refused(Refusal::of(e)),
                };
                self.answer(m, Answer { id, reply });
            }
            Op::Write { chunk, position, index, offset, data } => {
                let r = self.checked(v, g, chunk, position, index, true, |s, verified| {
                    s.store.write(v, g, chunk, offset, data, verified)
                });
                let reply = match r {
                    Ok(()) => Reply::Done,
                    Err(why) => Reply::Refused(why),
                };
                self.answer(m, Answer { id, reply });
            }
            Op::Read { chunk, position, index, offset, len } => {
                let mut buf = Vec::new();
                if buf.try_reserve_exact(len as usize).is_err() {
                    self.answer_refused(m, Refusal::NotReady);
                    return;
                }
                buf.resize(len as usize, 0);
                let r = self.checked(v, g, chunk, position, index, false, |s, verified| {
                    s.store.read(v, g, chunk, offset, &mut buf, verified)
                });
                match r {
                    Ok(()) => {
                        let a = Answer { id, reply: Reply::Data(&buf) };
                        self.served += 1;
                        let _ = self.mb.reply(m, ANSWER, &a.encode());
                    }
                    Err(why) => self.answer(m, Answer { id, reply: Reply::Refused(why) }),
                }
            }
            Op::Flush => {
                let reply = match self.store.flush() {
                    Ok(()) => Reply::Done,
                    Err(e) => Reply::Refused(Refusal::of(e)),
                };
                self.answer(m, Answer { id, reply });
            }
            Op::Close { chunk, position, index } => {
                self.changed = true;
                let r = self.checked(v, g, chunk, position, index, true, |s, _| s.store.close(v, g, chunk));
                let reply = match r {
                    Ok(closed) => Reply::Closed(closed),
                    Err(why) => Reply::Refused(why),
                };
                self.answer(m, Answer { id, reply });
            }
            Op::Held => {
                let open: Vec<(u32, u32, u64)> = self.store.holdings().iter()
                    .filter(|h| h.volume == v && h.state == SlotState::Open)
                    .take(LIST_MAX).map(|h| (h.chunk, h.incarnation, h.generation)).collect();
                self.answer(m, Answer { id, reply: Reply::Held(open) });
            }
        }
    }

    /// `op` done on the engine -- and, where the engine has not checked the
    /// request, asked of the mds first and done again once it vouches.
    #[allow(clippy::too_many_arguments)]
    fn checked<T>(&mut self, volume: u32, generation: u64, chunk: ChunkRef, position: u32, index: u64, writing: bool,
                  mut op: impl FnMut(&mut Server, bool) -> Result<T, CsError>) -> Result<T, Refusal> {
        let key = Checked { volume, generation, chunk: chunk.chunk, incarnation: chunk.incarnation,
                            set: chunk.epoch.set, version: chunk.epoch.version };
        let known = self.verified.contains(&key);
        match op(self, known) {
            Ok(t) => return Ok(t),
            Err(CsError::Unverified) | Err(CsError::Epoch { .. }) if !known => {}
            Err(e) => return Err(Refusal::of(e)),
        }
        match self.ask(Question::Verify { volume, generation, chunk, position, index, writing }) {
            Some(Told::Verified) => {
                if self.verified.len() >= VERIFIED_MAX {
                    self.verified.clear();
                }
                self.verified.insert(key);
                op(self, true).map_err(Refusal::of)
            }
            Some(Told::Refused(why)) => Err(why),
            _ => Err(Refusal::NoMds),
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
        self.shared.show(Shown { cs: self.cs, devices, held: count, open, chunks: held,
                                 fences: self.store.fenced_volumes(), served: self.served, refused: self.refused });
    }
}
