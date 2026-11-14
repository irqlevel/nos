//! NBS's client (nbsclient) driven over ipc (plans/nbs.md, N8): a volume's
//! session in a mailbox of its own. Its asks go to this host's mds
//! (`nbs.mds`, `TAG_ASK`); its requests go to the cs's -- `nbs.cs` here,
//! `nbs.cs@<host>` elsewhere -- by the bulk connection, so that a client's
//! requests keep their order between two hosts. What comes back is fed to
//! the session: a cs's answer, the mds's, and the channel's bounce of a
//! request, which is a cs not there. Its time is kept by a tick on every
//! turn. No module of its own: the modules that read and write volumes
//! link it -- nbs-mds.ko's `nbs io`, nbsload.ko, hv.ko's `nbs:` disks.
//!
//! [`Drive`] is the session and its mailbox, driven by whoever holds it and
//! nobody else: between its calls nothing of the session moves. Its blocking
//! calls -- [`Drive::open`], `read`, `write`, `flush`, `close`, `stop` --
//! wait for their own operation, for a shell's command or a load
//! generator's task. A disk's task submits many with [`Drive::submit`],
//! turns the session with [`Drive::turn`] and takes what is done with
//! [`Drive::take_done`].
//!
//! Every wait is bounded: an operation fails in a minute (the session's
//! deadline), a start or a stop in two, and a wait that outlives those says
//! what the session still waits on rather than hang.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use kcore::events::SharedEvent;
use kcore::ipc::{Bounce, Error, Limits, Mailbox, Scope, TAG_BULK};
use kcore::klog;
use kcore::time::{boot_time_ns, Duration};
use nbsclient::{Buffers, Out, Phase, Session, To, UserOp};
use nbswire::{CS_NAME, MDS_NAME, TAG_ANSWER, TAG_ASK, TAG_REQUEST, TAG_SAID};

/// How long a turn waits for something to come before the session's time
/// is kept anyway.
pub const TURN_NS: u64 = 50_000_000;
/// What an operation's offset and length are whole multiples of: the
/// volume's block, a disk's that is one.
pub const BLOCK: u64 = nbsclient::BLOCK;
/// A request to a cs as it goes: between hosts by the bulk connection.
const REQUEST: u32 = TAG_REQUEST | TAG_BULK;
/// How long a start or a stop is waited for with nothing moving: the
/// attach's own limit, then the map, the fences and the reconcile -- whose
/// every chunk mended counts as moving, so that one of thousands of chunks
/// open, 4 MiB each, goes on for as long as it moves (the review of
/// 2026-11: a big one was given up at 120 s, each try begun again).
const START_NS: u64 = 120_000_000_000;
/// How long an operation is waited for: past the session's own deadline,
/// which fails it first.
const WAIT_NS: u64 = 2 * nbsclient::DEADLINE_NS;
/// The session's mailbox: room for every answer its requests out can bring
/// by count -- closes are let out 128 places at a time (nbsclient's
/// `CLOSING_MAX`), writes and reads by the driver's depth (N13b: 1,024
/// places, overflowed by a stop's closes) -- and by bytes, the reads out
/// held to `READS_OUT`. Not bigger: a module's mailboxes hold 64 MiB
/// together (ipc's `OWNER_MAX_BYTES`), and hv's are a volume's each -- at 8
/// MiB, eight volumes. What it holds is taken at every turn.
const LIMITS: Limits = Limits { messages: 4096, bytes: 8 << 20 };
/// The bytes of reads the drive has out to its session at once, at most:
/// half the mailbox, less a MiB for everything else that comes to it -- so
/// that the answers of every read out fit come each twice (a request
/// unanswered a second is sent again, and the first answer may come after
/// all), with the writes', the closes', the reconcile's one read and the
/// mds's beside them. A read past it waits in the drive, in the order it
/// came, and is let out as reads before it end; one bigger than it goes
/// alone. Before, nothing held them: hv's disk keeps 64 reads of 256 KiB
/// out, 16 MiB of answers for a mailbox of 8, and an answer with no room
/// was lost -- its request sent again a second on, its replica asked out
/// where the leader's look found it silent too (the review of 2026-11,
/// m26). A cs slower than that second still has some answers come three
/// times, and one lost so is sent again as before.
const READS_OUT: u64 = (LIMITS.bytes / 2 - (1 << 20)) as u64;

/// The driver's buffers, by an operation's tag: a write's bytes, the room
/// a read's go to.
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

/// An operation done: its tag, whether it was done well, and its buffer --
/// a read's bytes, a write's back.
pub struct Done {
    pub tag: u64,
    pub ok: bool,
    pub data: Vec<u8>,
}

/// An operation of a driver's, as `submit` takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read { offset: u64, len: u32 },
    Write { offset: u64 },
    Flush,
    Close,
}

/// A volume's session over ipc.
pub struct Drive {
    s: Session,
    mb: Mailbox,
    /// This host's name: a cs here is reached by its bare name.
    me: String,
    bufs: Bufs,
    /// Operations done and not taken yet.
    done: BTreeMap<u64, bool>,
    next_tag: u64,
    /// The session's last line: why an operation failed, as it said it.
    last: String,
    /// The hosts, by their number on the channel, whose answers are taken
    /// as a cs's: the session's map's, each learned at its first answer.
    /// What another host sends is no cs's word, and dropped (the review of
    /// 2026-11): a request's id is a count, and the mds's word comes from
    /// this host alone.
    answer_hosts: BTreeSet<u16>,
    /// The reads out to the session, by tag, each's bytes; their sum; and
    /// those that wait for room (`READS_OUT`), in the order they came.
    reads: BTreeMap<u64, u32>,
    reads_out: u64,
    held: VecDeque<(u64, u64, u32)>,
}

/// Tags of the driver's own blocking calls: past those `submit` hands out.
const OWN: u64 = 1 << 62;

/// This host's name, as ipcnet has it: what `open` takes as `me`.
pub fn this_host() -> Result<String, String> {
    ipcctl::identity().map(|id| id.name)
}

impl Drive {
    /// Volume `name` attached on this host, `me` -- for writing, `force`
    /// taking it from a writer elsewhere, or read-only -- and its session
    /// serving: fenced, its open chunks reconciled. On an error the attach
    /// may stand: a writer's is taken by the next attach with `force`.
    /// `chains`: the namespace of the module that drives it, a writer's
    /// writes passed along their replicas (N13) -- each answering its
    /// mailbox named there; none, each sent to every replica directly.
    pub fn open(name: &str, me: &str, writable: bool, force: bool, chains: Option<&str>)
        -> Result<Drive, String> {
        let mb = Mailbox::new(LIMITS).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
        Self::start_with(mb, name, me, writable, force, chains)
    }

    /// As `open`, its mailbox made on `event`: what comes to the session
    /// signals it, so that a task waiting on it for more than the session
    /// -- a disk's, for its guest's requests -- wakes for either.
    pub fn open_on(name: &str, me: &str, writable: bool, force: bool, event: &SharedEvent, chains: Option<&str>)
        -> Result<Drive, String> {
        let mb = Mailbox::new_on(LIMITS, event).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
        Self::start_with(mb, name, me, writable, force, chains)
    }

    fn start_with(mb: Mailbox, name: &str, me: &str, writable: bool, force: bool, chains: Option<&str>)
        -> Result<Drive, String> {
        let token = kcore::random::random_u64().filter(|&t| t != 0)
            .ok_or_else(|| String::from("no randomness for the session's token"))?;
        let mut s = Session::new(name, me, writable, force, token);
        /* A writer's mailbox named for the cluster, its token in the name,
           under its module's namespace -- a module names mailboxes under
           its own alone (docs/ipc.md), and each part of a name begins with
           a letter (`s` before the token's hex): its writes passed along
           their replicas, each answering it there (N13). Where the name
           cannot be had, each sent directly, and said: as first written --
           `nbs.w.<hex>` -- hv's and nbsload's sessions were refused it, and
           most of nbs io's, and went direct in silence */
        if let (true, Some(ns)) = (writable, chains) {
            let named = alloc::format!("{}.nbsw.s{:016x}", ns, token);
            match mb.register(&named, Scope::Cluster) {
                Ok(()) => s.answer_at(&named),
                Err(e) => klog!(0, "nbsdrive: volume {}: the session's name {} not had ({}): its writes go to each \
                                    replica directly", name, named, e.name()),
            }
        }
        let mut d = Drive { s, mb, me: String::from(me), bufs: Bufs::default(), done: BTreeMap::new(), next_tag: 1,
                            last: String::new(), answer_hosts: BTreeSet::new(), reads: BTreeMap::new(), reads_out: 0,
                            held: VecDeque::new() };
        let mut out = Vec::new();
        d.s.start(boot_time_ns(), &mut out);
        d.handle(out);
        let mut end = boot_time_ns() + START_NS;
        let mut moved = d.s.progress();
        loop {
            match d.s.phase() {
                Phase::Serving => return Ok(d),
                Phase::Failed(why) => return Err(why.clone()),
                Phase::Detached => return Err(String::from("detached before it served")),
                _ => {}
            }
            if d.s.progress() != moved {
                moved = d.s.progress();
                end = boot_time_ns() + START_NS;
            }
            if boot_time_ns() >= end {
                return Err(alloc::format!("{} not serving, nothing moving in {} s: {}", name, START_NS / 1_000_000_000,
                                          d.s.report()));
            }
            d.turn(TURN_NS);
        }
    }

    /// The session, for its phase, size and report.
    pub fn session(&self) -> &Session {
        &self.s
    }

    /// Operation `op` started, its buffer `data` -- a write's bytes, or none
    /// for the rest: a read's room is made here. Its tag, which
    /// `take_done` gives back with it.
    pub fn submit(&mut self, op: Op, data: Vec<u8>) -> Result<u64, String> {
        let tag = self.next_tag;
        self.next_tag += 1;
        self.start(tag, op, data)
    }

    fn start(&mut self, tag: u64, op: Op, mut data: Vec<u8>) -> Result<u64, String> {
        let user = match op {
            Op::Read { offset, len } => {
                data.try_reserve_exact(len as usize).map_err(|_| String::from("no memory for the read"))?;
                data.resize(len as usize, 0);
                /* Past the bytes of reads out, or behind one that waits: it
                   waits its turn */
                if !self.held.is_empty() || (self.reads_out != 0 && self.reads_out + u64::from(len) > READS_OUT) {
                    self.held.try_reserve(1).map_err(|_| String::from("no memory for the read"))?;
                    self.bufs.0.insert(tag, data);
                    self.held.push_back((tag, offset, len));
                    return Ok(tag);
                }
                self.reads.insert(tag, len);
                self.reads_out += u64::from(len);
                UserOp::Read { tag, offset, len }
            }
            Op::Write { offset } => {
                let len = u32::try_from(data.len()).map_err(|_| String::from("a write past 4 GiB"))?;
                UserOp::Write { tag, offset, len }
            }
            Op::Flush => UserOp::Flush { tag },
            Op::Close => UserOp::Close { tag },
        };
        self.bufs.0.insert(tag, data);
        let mut out = Vec::new();
        self.s.submit(user, boot_time_ns(), &mut self.bufs, &mut out);
        self.handle(out);
        Ok(tag)
    }

    /// Whether an operation is done and not taken yet.
    pub fn has_done(&self) -> bool {
        !self.done.is_empty()
    }

    /// An operation done, if one is: its buffer with it.
    pub fn take_done(&mut self) -> Option<Done> {
        let (&tag, &ok) = self.done.iter().next()?;
        self.done.remove(&tag);
        let data = self.bufs.0.remove(&tag).unwrap_or_default();
        Some(Done { tag, ok, data })
    }

    /// One turn of the session: what comes in the next `wait_ns` -- and
    /// whatever else is there then -- fed to it, and its time kept.
    pub fn turn(&mut self, wait_ns: u64) {
        let mut out = Vec::new();
        if let Some(m) = self.mb.recv(Duration::from_nanos(wait_ns)) {
            self.feed(&m, &mut out);
            while let Some(m) = self.mb.try_recv() {
                self.feed(&m, &mut out);
            }
        }
        self.s.tick(boot_time_ns(), &mut self.bufs, &mut out);
        self.handle(out);
    }

    /// A message of the mailbox's, to the session.
    fn feed(&mut self, m: &kcore::ipc::Message, out: &mut Vec<Out>) {
        let now = boot_time_ns();
        if let Some(b) = m.bounced() {
            if b.tag & !TAG_BULK == TAG_REQUEST {
                self.s.on_bounce(m.reference, b.why == Bounce::Gone, now, &mut self.bufs, out);
            }
            return;
        }
        match m.tag & !TAG_BULK {
            TAG_ANSWER if self.from_a_cs(m) => self.s.on_cs(&m.data, now, &mut self.bufs, out),
            TAG_SAID if m.from.host() == 0 => self.s.on_mds(&m.data, now, &mut self.bufs, out),
            _ => {}
        }
    }

    /// Whether message `m` comes from a host a cs of the session's map is on:
    /// this one, or one learned so -- its name looked up at its first answer
    /// alone, a set's look after.
    fn from_a_cs(&mut self, m: &kcore::ipc::Message) -> bool {
        let h = m.from.host();
        if h == 0 || self.answer_hosts.contains(&h) {
            return true;
        }
        let Some(name) = kcore::ipc::peer_name(m.from) else { return false };
        if !self.s.knows_host(&name) {
            return false;
        }
        self.answer_hosts.insert(h);
        true
    }

    /// What the session asks done: its messages sent -- one the channel
    /// will not take now is as one bounced -- its operations noted done,
    /// its lines logged.
    fn handle(&mut self, mut out: Vec<Out>) {
        while !out.is_empty() {
            let mut more = Vec::new();
            for o in out {
                match o {
                    /* Not there now -- nbs-mds not loaded: asked again in a
                       second, as one not answered */
                    Out::Send { to: To::Mds, id, bytes } => {
                        let _ = self.mb.send_to(MDS_NAME, TAG_ASK, id, &bytes);
                    }
                    Out::Send { to: To::Cs(cs), id, bytes } => {
                        let Some(host) = self.s.host_of(cs) else { continue };
                        let to = if host == self.me {
                            String::from(CS_NAME)
                        } else {
                            alloc::format!("{}@{}", CS_NAME, host)
                        };
                        if let Err(e) = self.mb.send_to(&to, REQUEST, id, &bytes) {
                            /* No such name or way: the cs is not there; the
                               way full: sent again in a moment */
                            let gone = matches!(e, Error::Gone | Error::NoRoute);
                            self.s.on_bounce(id, gone, boot_time_ns(), &mut self.bufs, &mut more);
                        }
                    }
                    Out::Done { tag, ok } => {
                        self.done.insert(tag, ok);
                        if let Some(len) = self.reads.remove(&tag) {
                            self.reads_out -= u64::from(len);
                            self.let_out(&mut more);
                        }
                    }
                    Out::Say(line) => {
                        klog!(0, "{}", line);
                        self.last = line;
                    }
                }
            }
            out = more;
        }
    }

    /// The reads that wait, let out to the session in the order they came
    /// as far as `READS_OUT` has room -- the first of them alone whatever
    /// its size -- or failed, where the session is over and serves none.
    fn let_out(&mut self, out: &mut Vec<Out>) {
        if matches!(self.s.phase(), Phase::Failed(_) | Phase::Detached) {
            for (tag, ..) in core::mem::take(&mut self.held) {
                self.done.insert(tag, false);
            }
            return;
        }
        while let Some(&(tag, offset, len)) = self.held.front() {
            if self.reads_out != 0 && self.reads_out + u64::from(len) > READS_OUT {
                return;
            }
            self.held.pop_front();
            self.reads.insert(tag, len);
            self.reads_out += u64::from(len);
            self.s.submit(UserOp::Read { tag, offset, len }, boot_time_ns(), &mut self.bufs, out);
        }
    }

    /// Operation `op` of the driver's own, waited for: its buffer.
    fn wait(&mut self, op: Op, data: Vec<u8>) -> Result<Vec<u8>, String> {
        let tag = OWN | self.next_tag;
        self.next_tag += 1;
        self.start(tag, op, data)?;
        let end = boot_time_ns() + WAIT_NS;
        loop {
            if let Some(ok) = self.done.remove(&tag) {
                let data = self.bufs.0.remove(&tag).unwrap_or_default();
                if ok {
                    return Ok(data);
                }
                return Err(match self.s.phase() {
                    Phase::Failed(why) => why.clone(),
                    _ if self.last.is_empty() => String::from("failed"),
                    _ => self.last.clone(),
                });
            }
            if boot_time_ns() >= end {
                self.bufs.0.remove(&tag);
                return Err(alloc::format!("not done in {} s: {}", WAIT_NS / 1_000_000_000, self.s.report()));
            }
            self.turn(TURN_NS);
        }
    }

    /// `len` bytes at `offset`, read.
    pub fn read(&mut self, offset: u64, len: u32) -> Result<Vec<u8>, String> {
        self.wait(Op::Read { offset, len }, Vec::new())
    }

    /// `data` written at `offset`: on every replica in.
    pub fn write(&mut self, offset: u64, data: Vec<u8>) -> Result<(), String> {
        self.wait(Op::Write { offset }, data).map(|_| ())
    }

    /// What was written before, durable on every replica in.
    pub fn flush(&mut self) -> Result<(), String> {
        self.wait(Op::Flush, Vec::new()).map(|_| ())
    }

    /// What was written before durable, and every chunk written closed on
    /// its replicas: the volume attached still.
    pub fn close(&mut self) -> Result<(), String> {
        self.wait(Op::Close, Vec::new()).map(|_| ())
    }

    /// The session stopped -- what is out done, a flush, every chunk
    /// written closed -- and the volume let go of.
    pub fn stop(mut self) -> Result<(), String> {
        let tag = OWN | self.next_tag;
        self.next_tag += 1;
        let mut out = Vec::new();
        self.s.stop(tag, boot_time_ns(), &mut self.bufs, &mut out);
        self.handle(out);
        let end = boot_time_ns() + START_NS;
        loop {
            match self.s.phase() {
                Phase::Detached => return Ok(()),
                Phase::Failed(why) => return Err(why.clone()),
                _ => {}
            }
            if boot_time_ns() >= end {
                return Err(alloc::format!("not stopped in {} s: {}", START_NS / 1_000_000_000, self.s.report()));
            }
            self.turn(TURN_NS);
        }
    }
}
