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

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use kcore::events::SharedEvent;
use kcore::ipc::{Bounce, Error, Limits, Mailbox, TAG_BULK};
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
/// How long a start or a stop is waited for: the attach's own limit, then
/// the map, the fences and a reconcile of a whole volume's open chunks.
const START_NS: u64 = 120_000_000_000;
/// How long an operation is waited for: past the session's own deadline,
/// which fails it first.
const WAIT_NS: u64 = 2 * nbsclient::DEADLINE_NS;
/// The session's mailbox: room for the answers of the requests a disk keeps
/// out -- a read's data comes in pieces of up to 256 KiB.
const LIMITS: Limits = Limits { messages: 1024, bytes: 8 << 20 };

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
    pub fn open(name: &str, me: &str, writable: bool, force: bool) -> Result<Drive, String> {
        let mb = Mailbox::new(LIMITS).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
        Self::start_with(mb, name, me, writable, force)
    }

    /// As `open`, its mailbox made on `event`: what comes to the session
    /// signals it, so that a task waiting on it for more than the session
    /// -- a disk's, for its guest's requests -- wakes for either.
    pub fn open_on(name: &str, me: &str, writable: bool, force: bool, event: &SharedEvent) -> Result<Drive, String> {
        let mb = Mailbox::new_on(LIMITS, event).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
        Self::start_with(mb, name, me, writable, force)
    }

    fn start_with(mb: Mailbox, name: &str, me: &str, writable: bool, force: bool) -> Result<Drive, String> {
        let token = kcore::random::random_u64().filter(|&t| t != 0)
            .ok_or_else(|| String::from("no randomness for the session's token"))?;
        let mut d = Drive { s: Session::new(name, me, writable, force, token), mb, me: String::from(me),
                            bufs: Bufs::default(), done: BTreeMap::new(), next_tag: 1, last: String::new() };
        let mut out = Vec::new();
        d.s.start(boot_time_ns(), &mut out);
        d.handle(out);
        let end = boot_time_ns() + START_NS;
        loop {
            match d.s.phase() {
                Phase::Serving => return Ok(d),
                Phase::Failed(why) => return Err(why.clone()),
                Phase::Detached => return Err(String::from("detached before it served")),
                _ => {}
            }
            if boot_time_ns() >= end {
                return Err(alloc::format!("{} not serving in {} s: {}", name, START_NS / 1_000_000_000, d.s.report()));
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
            TAG_ANSWER => self.s.on_cs(&m.data, now, &mut self.bufs, out),
            TAG_SAID => self.s.on_mds(&m.data, now, &mut self.bufs, out),
            _ => {}
        }
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
