//! The target: mailboxes of the image's and of made-up modules, and what is
//! done with them, each step's answer and every queue held to a model.
//!
//! The model keeps what the channel promises: a message sent Ok is in its
//! mailbox, behind those sent before it, until it is taken or the mailbox
//! closes; a Down for every monitor whose mailbox goes, once, into the
//! place kept for it whatever else the queue holds; a name's coming and
//! going told to every watch of it that has room, and a watch that has
//! none ended with the message that says so; a module's names under its
//! own namespace; the host's queued bytes, mailboxes, names and watches as
//! counted. Where one step tells several -- a mailbox closing tells each of
//! its monitors, a name each of its watches -- it does so in the order the
//! channel keeps them in, which the model keeps the same way: a list each
//! is added to at its end and taken from by moving its last into the hole.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::machine::{sched, MODULES};
use crate::{reached, Input, Target};
use ffi::ipc as abi;
use ipc::{Addr, Bounce, Error, Limits, Mailbox, Payload, Reason, Scope, To};
use ipcwire as wire;
use ipcwire::route;

pub static ALL: &[Target] = &[
    Target { name: "ipc", run: run, max_len: 4096, gate: 6000 },
    Target { name: "wire", run: crate::wire::run, max_len: 4096, gate: 20000 },
];

/// Each step said on stderr, with IPC_FUZZ_SAY set: for a replay.
static SAY: AtomicBool = AtomicBool::new(false);

macro_rules! say {
    ($($fmt:tt)*) => {
        if SAY.load(Ordering::Relaxed) {
            eprintln!($($fmt)*);
        }
    };
}

const NAMES: [&str; 20] = ["kernel.a", "kernel.b", "nbs.mds", "nbs.cs", "nbs.cs.0", "modtest.a", "modtest.b", "sys.x",
                           "other.y", "bad name", "Nbs.x", "nbs..x", "", "nbs.x-y_z",
                           "host.ex44", "nbs.mds@ex44", "kernel.a@ax41", "x@", "@ex44", "nbs.mds@Bad"];
const PREFIXES: [&str; 11] = ["", "nbs.", "nbs.cs", "kernel.", "modtest", "sys", "Bad", "nbs..", "nbs.cs.0", "host.", "host.ex44"];
const TAGS: [u32; 5] = [1, 7, 0xFFFF_FFFF - 0x10000, ipc::TAG_SYSTEM, ipc::TAG_DOWN];
/// Peers' names: well made, and not.
const PEER_NAMES: [&str; 6] = ["ex44", "ax41", "a-b-1", "Bad", "", "ex44x"];
const PEER_BITS: u32 = 6;

fn number(i: usize, epoch: u16) -> u16 {
    (epoch << PEER_BITS) | (i as u16 + 1)
}

/// A host's name, the model's own: a DNS label, lowercase.
fn host_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 63 && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// A peer, as the model has it.
struct PeerM {
    name: String,
    epoch: u16,
    up: bool,
    sent: VecDeque<u64>,
}

/// A monitor of a mailbox on a peer, kept on this host.
struct RemoteM {
    id: u64,
    peer: u16,
    watcher: usize,
    target: u64,
}

/// A message as the model has it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Msg {
    from: u64,
    tag: u32,
    reference: u64,
    data: Vec<u8>,
}

impl Msg {
    fn kept(&self) -> bool {
        self.tag == ipc::TAG_DOWN || self.tag == ipc::TAG_WATCH_LOST
    }
}

/// Whose a mailbox is: the image's, or a made-up module's (`MODULES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Image,
    Module(usize),
}

/// A mailbox as the model has it.
struct Box_ {
    addr: u64,
    owner: Owner,
    limits: Limits,
    open: bool,
    queue: VecDeque<Msg>,
    normal: u32,
    bytes: usize,
    reserved: usize,
    kept_queued: usize,
    /// Its monitors, in the channel's order: (target, id).
    monitors: Vec<(u64, u64)>,
    /// Who monitors it: (id, watcher's index), in the channel's order.
    monitored_by: Vec<(u64, usize)>,
    /// Its watches' ids, in the channel's order.
    watches: Vec<u64>,
}

/// A watch, as the table keeps it.
struct WatchRec {
    id: u64,
    prefix: String,
    watcher: usize,
    lost_pending: bool,
}

struct Model {
    boxes: Vec<Box_>,
    /// The image's mailboxes themselves, by the model's index.
    image: Vec<Option<Mailbox>>,
    /// The names, in the table's order: (name, mailbox's index, scope).
    names: Vec<(String, usize, Scope)>,
    watches: Vec<WatchRec>,
    stopped: bool,
    /// The router: a mailbox's index.
    router: Option<usize>,
    /// The peers, by their numbers' index.
    peers: Vec<Option<PeerM>>,
    /// The remote monitors, in the table's order.
    remote: Vec<RemoteM>,
}

/// Whether `part` is one part of a name: a lowercase letter, then lowercase
/// letters, digits, `_` and `-`. The model's own, written from the doc.
fn part_ok(part: &str) -> bool {
    let b = part.as_bytes();
    !b.is_empty() && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

fn name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 48 && name.split('.').all(part_ok)
}

fn prefix_ok(prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let body = prefix.strip_suffix('.').unwrap_or(prefix);
    prefix.len() <= 48 && !body.is_empty() && body.split('.').all(part_ok)
}

/// `data` cut into one to four pieces, some of them empty, at random.
fn split<'a>(data: &'a [u8], r: &mut Input) -> Vec<&'a [u8]> {
    let n = 1 + r.below(4) as usize;
    let mut cuts: Vec<usize> = (1..n).map(|_| r.below(data.len() as u64 + 1) as usize).collect();
    cuts.sort_unstable();
    let mut out = Vec::new();
    let mut at = 0;
    for c in cuts {
        out.push(&data[at..c]);
        at = c;
    }
    out.push(&data[at..]);
    out
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(37) ^ seed).collect()
}

impl Model {
    fn find(&self, addr: u64) -> Option<usize> {
        self.boxes.iter().position(|b| b.addr == addr && b.open)
    }

    /// The peer `host` names, up and of that epoch.
    fn live(&self, host: u16) -> Option<usize> {
        let i = usize::from(host & ((1 << PEER_BITS) - 1));
        if i == 0 {
            return None;
        }
        let p = self.peers.get(i - 1)?.as_ref()?;
        (p.up && p.epoch == host >> PEER_BITS).then_some(i - 1)
    }

    fn live_by_name(&self, name: &str) -> Option<u16> {
        self.peers.iter().enumerate().find_map(|(i, p)| {
            let p = p.as_ref()?;
            (p.up && p.name == name).then(|| number(i, p.epoch))
        })
    }

    /// Whether `push` would take `len` bytes into `at`: its checks, nothing
    /// changed.
    fn would_push(&self, at: usize, len: usize) -> Result<(), Error> {
        let b = &self.boxes[at];
        if len > b.limits.bytes {
            return Err(Error::TooLarge);
        }
        if !b.open {
            return Err(Error::Gone);
        }
        if b.normal >= b.limits.messages || b.bytes + len > b.limits.bytes {
            return Err(Error::Full);
        }
        Ok(())
    }

    /// A routed message into the router `rt`: the head, then `body`.
    fn routed(&mut self, rt: usize, from: u64, head: &route::Head<'_>, body: &[u8]) -> Result<(), Error> {
        let mut data = route::encode(head).expect("a head the model makes").to_vec();
        data.extend_from_slice(body);
        self.push(rt, Msg { from, tag: ipc::TAG_ROUTE, reference: 0, data })
    }

    /// The peer at `i` lost, as `remote::lose` does it: its monitors' Downs
    /// and the watchers of it, for `tell`.
    fn lose(&mut self, i: usize, host: u16) -> Option<(String, Vec<(usize, u64)>)> {
        let p = self.peers[i].as_mut()?;
        p.up = false;
        p.sent.clear();
        let name = p.name.clone();
        let mut k = 0;
        while k < self.remote.len() {
            if self.remote[k].peer == host {
                let rec = self.remote.swap_remove(k);
                let data = (Reason::HostLost as u32).to_le_bytes().to_vec();
                self.push_kept(rec.watcher, Msg { from: rec.target, tag: ipc::TAG_DOWN, reference: rec.id, data });
            } else {
                k += 1;
            }
        }
        let key = format!("host.{}", name);
        let watchers = self.watchers_of(&key);
        Some((key, watchers))
    }

    /// A message into the queue's own places, as `push` does it.
    fn push(&mut self, at: usize, msg: Msg) -> Result<(), Error> {
        let b = &mut self.boxes[at];
        if msg.data.len() > b.limits.bytes {
            return Err(Error::TooLarge);
        }
        if !b.open {
            return Err(Error::Gone);
        }
        if b.normal >= b.limits.messages || b.bytes + msg.data.len() > b.limits.bytes {
            return Err(Error::Full);
        }
        b.normal += 1;
        b.bytes += msg.data.len();
        b.queue.push_back(msg);
        Ok(())
    }

    /// A message into a place kept for it, as `push_kept` does it.
    fn push_kept(&mut self, at: usize, msg: Msg) {
        let b = &mut self.boxes[at];
        if !b.open {
            return;
        }
        b.reserved = b.reserved.saturating_sub(1);
        b.kept_queued += 1;
        b.queue.push_back(msg);
    }

    /// The watchers of `name`, in the table's order.
    fn watchers_of(&self, name: &str) -> Vec<(usize, u64)> {
        self.watches.iter().filter(|w| name.starts_with(w.prefix.as_str())).map(|w| (w.watcher, w.id)).collect()
    }

    fn tell(&mut self, name: &str, addr: u64, tag: u32, watchers: Vec<(usize, u64)>) {
        for (w, id) in watchers {
            let msg = Msg { from: addr, tag, reference: id, data: name.as_bytes().to_vec() };
            if self.push(w, msg).is_err() {
                self.lose_watch(w, id);
            }
        }
    }

    fn lose_watch(&mut self, w: usize, id: u64) {
        let Some(i) = self.watches.iter().position(|r| r.id == id && r.watcher == w) else { return };
        let rec = self.watches.swap_remove(i);
        self.boxes[w].watches.retain(|&x| x != id);
        if rec.lost_pending {
            self.push_kept(w, Msg { from: 0, tag: ipc::TAG_WATCH_LOST, reference: id, data: Vec::new() });
        }
    }

    /// Mailbox `at` closed for `reason`, as `close` does it.
    fn close(&mut self, at: usize, reason: Reason) {
        if !self.boxes[at].open {
            return;
        }
        let addr = self.boxes[at].addr;
        let was_router = self.router == Some(at);
        if was_router {
            self.router = None;
        }
        /* Its address forgotten by every peer, its remote monitors taken */
        for p in self.peers.iter_mut().flatten() {
            p.sent.retain(|&a| a != addr);
        }
        let mut gone = Vec::new();
        let mut k = 0;
        while k < self.remote.len() {
            if self.remote[k].watcher == at {
                gone.push(self.remote.swap_remove(k));
            } else {
                k += 1;
            }
        }
        let router = self.router;
        /* Its name and the watches of it, its own watches */
        let name = self.names.iter().position(|n| n.1 == at).map(|i| self.names.swap_remove(i).0);
        let watchers = name.as_deref().map(|n| self.watchers_of(n)).unwrap_or_default();
        let mut i = 0;
        while i < self.watches.len() {
            if self.watches[i].watcher == at {
                self.watches.swap_remove(i);
            } else {
                i += 1;
            }
        }
        /* A router's close loses every peer up -- after its own watches and
         * remote monitors went, so it hears nothing of it */
        let mut lost = Vec::new();
        if was_router {
            for i in 0..self.peers.len() {
                let host = match &self.peers[i] {
                    Some(p) if p.up => number(i, p.epoch),
                    _ => continue,
                };
                if let Some(x) = self.lose(i, host) {
                    lost.push((x, host));
                }
            }
        }
        for ((key, watchers), host) in lost {
            self.tell(&key, Addr { host, slot: 0, generation: 0 }.to_u64(), ipc::TAG_NAME_DOWN, watchers);
        }
        for rec in &gone {
            let b = &mut self.boxes[at];
            b.reserved = b.reserved.saturating_sub(1);
            b.monitors.retain(|&(_, m)| m != rec.id);
        }
        if let Some(rt) = router {
            for rec in gone {
                let to = Addr { host: 0, ..Addr::from_u64(rec.target) }.to_u64();
                let head = route::Head { kind: route::DEMONITOR, peer: rec.peer, tag: 0, to: wire::To::Addr(to),
                                         reference: rec.id };
                let _ = self.routed(rt, addr, &head, &[]);
            }
        }
        /* The mailbox itself */
        let b = &mut self.boxes[at];
        b.open = false;
        b.queue.clear();
        b.normal = 0;
        b.bytes = 0;
        let monitored_by = std::mem::take(&mut b.monitored_by);
        let monitors = std::mem::take(&mut b.monitors);
        /* Its own monitors' records at their targets */
        for (target, id) in monitors {
            if let Some(t) = self.find(target) {
                if let Some(k) = self.boxes[t].monitored_by.iter().position(|r| r.0 == id) {
                    self.boxes[t].monitored_by.swap_remove(k);
                }
            }
        }
        /* The Downs it owes */
        for (id, w) in monitored_by {
            let mut data = (reason as u32).to_le_bytes().to_vec();
            data.truncate(4);
            self.push_kept(w, Msg { from: addr, tag: ipc::TAG_DOWN, reference: id, data });
        }
        if let Some(name) = name {
            self.tell(&name, addr, ipc::TAG_NAME_DOWN, watchers);
        }
    }

    fn queued_bytes(&self) -> usize {
        self.boxes.iter().filter(|b| b.open).map(|b| b.bytes).sum()
    }
}

/// The channel and the model compared: the counters, every mailbox, every
/// name.
fn compare(m: &Model) {
    let c = ipc::counters();
    let open = m.boxes.iter().filter(|b| b.open).count();
    invariant!(c.queued_bytes == m.queued_bytes(), "{} bytes queued, the model {}", c.queued_bytes, m.queued_bytes());
    invariant!(c.mailboxes == open, "{} mailboxes, the model {}", c.mailboxes, open);
    invariant!(c.names == m.names.len(), "{} names, the model {}", c.names, m.names.len());
    invariant!(c.watches == m.watches.len(), "{} watches, the model {}", c.watches, m.watches.len());
    for info in ipc::boxes() {
        let Some(at) = m.find(info.addr.to_u64()) else {
            panic!("invariant: mailbox {} the model does not have", info.addr);
        };
        let b = &m.boxes[at];
        invariant!(info.queued == b.normal && info.bytes == b.bytes, "mailbox {}: {} messages {} bytes, the model {} {}",
                   info.addr, info.queued, info.bytes, b.normal, b.bytes);
        invariant!(info.kept == b.reserved + b.kept_queued, "mailbox {}: {} places kept, the model {}", info.addr,
                   info.kept, b.reserved + b.kept_queued);
        invariant!(info.monitors == b.monitors.len() && info.monitored_by == b.monitored_by.len()
                   && info.watches == b.watches.len(), "mailbox {}: monitors, monitored by or watches", info.addr);
    }
    let peers: Vec<(u16, String, bool, usize)> = m.peers.iter().enumerate()
        .filter_map(|(i, p)| p.as_ref().map(|p| (number(i, p.epoch), p.name.clone(), p.up, p.sent.len()))).collect();
    invariant!(ipc::peers() == peers, "the peers: {:?}, the model {:?}", ipc::peers(), peers);
    let router = m.router.map(|rt| m.boxes[rt].addr);
    invariant!(ipc::router().map(|a| a.to_u64()) == router, "the router {:?}, the model {:?}", ipc::router(), router);
    let mut got: Vec<(String, u64)> = ipc::list("").into_iter().map(|n| (n.name, n.addr.to_u64())).collect();
    let mut want: Vec<(String, u64)> = m.names.iter().map(|n| (n.0.clone(), m.boxes[n.1].addr)).collect();
    got.sort();
    want.sort();
    invariant!(got == want, "the names: {:?}, the model {:?}", got, want);
}

/// An address that is the model's mailbox `at`'s, or one that never was.
fn addr_of(m: &Model, r: &mut Input) -> u64 {
    /* A mailbox on a peer: of one up, or of an older epoch */
    if r.chance(24) {
        let i = r.below(4) as usize;
        let epoch = m.peers[i].as_ref().map_or(1, |p| p.epoch).saturating_sub(r.below(2) as u16).max(1);
        return Addr { host: number(i, epoch), slot: 1 + r.u16() % 8, generation: 1 + u32::from(r.u8() % 3) }.to_u64();
    }
    if m.boxes.is_empty() || r.chance(16) {
        return match r.u8() % 3 {
            0 => 0,
            1 => Addr { host: 2, slot: 1, generation: 1 }.to_u64(),
            _ => Addr { host: 0, slot: r.u16() % 1100, generation: r.u32() }.to_u64(),
        };
    }
    m.boxes[r.below(m.boxes.len() as u64) as usize].addr
}

fn limits(r: &mut Input) -> Limits {
    match r.u8() % 8 {
        0 => Limits { messages: 0, bytes: 64 },
        1 => Limits { messages: 1, bytes: 16 },
        2 => Limits { messages: 2, bytes: 64 },
        3 => Limits { messages: ipc::MAX_QUEUE_MESSAGES + 1, bytes: 64 },
        _ => Limits { messages: 1 + u32::from(r.u8() % 8), bytes: 16 + r.below(4096) as usize },
    }
}

fn open(m: &mut Model, r: &mut Input) {
    let l = limits(r);
    let module = r.bool();
    let valid = l.messages != 0 && l.messages <= ipc::MAX_QUEUE_MESSAGES && l.bytes != 0;
    let (addr, owner) = if module {
        let which = r.below(MODULES.len() as u64) as usize;
        let at = MODULES[which].0 + r.below(MODULES[which].1 as u64) as usize;
        (abi::kernel_ipc_open(at, l.messages, l.bytes as u64), Owner::Module(which))
    } else {
        match Mailbox::new(l) {
            Ok(mb) => {
                let a = mb.addr().to_u64();
                m.image.push(Some(mb));
                (a, Owner::Image)
            }
            Err(e) => {
                invariant!(!valid || m.stopped, "a mailbox refused: {:?}", e);
                return;
            }
        }
    };
    if addr == 0 {
        invariant!(!valid || m.stopped, "a module's mailbox refused");
        return;
    }
    invariant!(valid && !m.stopped, "a mailbox made with limits {:?} after the halt {}", l, m.stopped);
    invariant!(m.find(addr).is_none(), "address {:x} given to two mailboxes", addr);
    if owner != Owner::Image {
        m.image.push(None);
    }
    say!("open {:x} {:?} {:?}", addr, owner, l);
    m.boxes.push(Box_ { addr, owner, limits: l, open: true, queue: VecDeque::new(), normal: 0, bytes: 0, reserved: 0,
                        kept_queued: 0, monitors: Vec::new(), monitored_by: Vec::new(), watches: Vec::new() });
    reached("a mailbox made");
}

fn close(m: &mut Model, r: &mut Input) {
    if m.boxes.is_empty() {
        return;
    }
    let at = r.below(m.boxes.len() as u64) as usize;
    say!("close {:x} (open {})", m.boxes[at].addr, m.boxes[at].open);
    match m.boxes[at].owner {
        Owner::Image => {
            /* Its handle dropped: the only way an image's mailbox closes */
            m.image[at] = None;
        }
        Owner::Module(_) => abi::kernel_ipc_close(m.boxes[at].addr),
    }
    m.close(at, Reason::Closed);
}

fn send(m: &mut Model, r: &mut Input) {
    if m.boxes.is_empty() {
        return;
    }
    let from = r.below(m.boxes.len() as u64) as usize;
    let name = NAMES[r.below(NAMES.len() as u64) as usize];
    /* Through the ABI a name of no bytes is no name: the send goes by `to` */
    let by_name = r.chance(64) && !(m.boxes[from].owner != Owner::Image && name.is_empty());
    let to = addr_of(m, r);
    let tag = TAGS[r.below(TAGS.len() as u64) as usize];
    let reference = r.u64();
    let len = if r.chance(16) { r.below(5000) as usize } else { r.below(80) as usize };
    let data = pattern(len, r.u8());
    /* What the model says the channel answers */
    let want = if !m.boxes[from].open {
        Err(Error::Gone)
    } else if tag >= ipc::TAG_SYSTEM {
        Err(Error::Invalid)
    } else if m.stopped {
        Err(Error::Stopped)
    } else {
        /* Where it goes: a mailbox here, or the router for a peer */
        enum Dest<'a> {
            Here(usize),
            Peer(u16, wire::To<'a>),
        }
        let target = if by_name {
            match name.split_once('@') {
                Some((nm, host)) => {
                    if !name_ok(nm) || !host_ok(host) {
                        Err(Error::Invalid)
                    } else if m.router.is_none() {
                        Err(Error::NoRoute)
                    } else {
                        m.live_by_name(host).map(|h| Dest::Peer(h, wire::To::Name(nm.as_bytes()))).ok_or(Error::NoRoute)
                    }
                }
                None => m.names.iter().find(|n| n.0 == name).map(|n| Dest::Here(n.1)).ok_or(Error::Gone),
            }
        } else if Addr::from_u64(to).host != 0 {
            let host = Addr::from_u64(to).host;
            if m.router.is_none() || m.live(host).is_none() {
                Err(Error::NoRoute)
            } else {
                Ok(Dest::Peer(host, wire::To::Addr(Addr { host: 0, ..Addr::from_u64(to) }.to_u64())))
            }
        } else {
            m.find(to).map(Dest::Here).ok_or(Error::Gone)
        };
        let sender = m.boxes[from].addr;
        target.and_then(|t| match t {
            Dest::Here(t) => m.push(t, Msg { from: sender, tag, reference, data: data.clone() }),
            Dest::Peer(host, to) => {
                /* The sender kept as one the peer may answer, first */
                if let Some(i) = m.live(host) {
                    if let Some(p) = m.peers[i].as_mut() {
                        if !p.sent.contains(&sender) {
                            if p.sent.len() >= ipc::MAX_SENT {
                                p.sent.pop_front();
                            }
                            p.sent.push_back(sender);
                        }
                    }
                }
                let rt = m.router.expect("a router, said above");
                let head = route::Head { kind: route::SEND, peer: host, tag, to, reference };
                let res = m.routed(rt, sender, &head, &data);
                if res.is_ok() {
                    reached("a message routed to a peer");
                }
                res
            }
        })
    };
    let got = match (m.boxes[from].owner, &m.image[from]) {
        (Owner::Image, Some(mb)) => {
            let target = if by_name { To::Name(name) } else { To::Addr(Addr::from_u64(to)) };
            mb.send(target, tag, reference, Payload::from_slice(&data).expect("a payload")).map_err(|e| e.0)
        }
        (Owner::Image, None) => {
            /* A closed image mailbox has no handle to send from */
            invariant!(want == Err(Error::Gone), "a send the model allows from a closed mailbox");
            return;
        }
        (Owner::Module(_), _) => {
            let (np, nl) = if by_name { (name.as_ptr(), name.len()) } else { (core::ptr::null(), 0) };
            // SAFETY: the name and the data are their lengths' bytes.
            let code = unsafe {
                abi::kernel_ipc_send(m.boxes[from].addr, to, np, nl, tag, reference, data.as_ptr(), data.len())
            };
            match code {
                0 => Ok(()),
                c => Err(error_of(c)),
            }
        }
    };
    say!("send {:x} -> {:x} {:?} len {} tag {:x}: {:?}", m.boxes[from].addr, to, if by_name { name } else { "" }, len, tag, got);
    invariant!(got == want, "send {} -> {:x}{}: {:?}, the model {:?}", m.boxes[from].addr, to,
               if by_name { name } else { "" }, got, want);
    if got.is_ok() {
        reached("a message sent");
    }
}

fn error_of(code: i32) -> Error {
    match code {
        abi::IPC_FULL => Error::Full,
        abi::IPC_GONE => Error::Gone,
        abi::IPC_NO_ROUTE => Error::NoRoute,
        abi::IPC_TOO_LARGE => Error::TooLarge,
        abi::IPC_NO_MEMORY => Error::NoMemory,
        abi::IPC_INVALID => Error::Invalid,
        abi::IPC_TAKEN => Error::Taken,
        abi::IPC_NOT_YOURS => Error::NotYours,
        abi::IPC_BUSY => Error::Busy,
        abi::IPC_STOPPED => Error::Stopped,
        c => panic!("invariant: code {} is no code of the channel's", c),
    }
}

/// The next message of mailbox `at` taken, as `take` does it, in the model.
fn take(m: &mut Model, at: usize) -> Option<Msg> {
    let b = &mut m.boxes[at];
    let msg = b.queue.pop_front()?;
    if msg.kept() {
        b.kept_queued -= 1;
    } else {
        b.normal -= 1;
        b.bytes -= msg.data.len();
    }
    if msg.tag == ipc::TAG_DOWN {
        b.monitors.retain(|&(a, id)| !(a == msg.from && id == msg.reference));
    }
    Some(msg)
}

fn recv(m: &mut Model, r: &mut Input) {
    if m.boxes.is_empty() {
        return;
    }
    let at = r.below(m.boxes.len() as u64) as usize;
    /* Past every payload the target sends, and under the heap's ceiling */
    let cap = if r.chance(32) { r.below(40) as usize } else { 1 << 16 };
    let front_len = m.boxes[at].queue.front().map(|f| f.data.len());
    match (m.boxes[at].owner, &m.image[at]) {
        (Owner::Image, Some(mb)) => {
            let got = mb.try_recv();
            let want = take(m, at);
            match (got, want) {
                (None, None) => {}
                (Some(g), Some(w)) => {
                    let g = Msg { from: g.from().to_u64(), tag: g.tag(), reference: g.reference(),
                                  data: g.payload().to_vec().expect("bytes") };
                    invariant!(g == w, "mailbox {:x} gave {:?}, the model {:?}", m.boxes[at].addr, g, w);
                    reached("a message received");
                }
                (g, w) => panic!("invariant: mailbox {:x} gave {}, the model {}", m.boxes[at].addr,
                                 if g.is_some() { "a message" } else { "none" }, if w.is_some() { "a message" } else { "none" }),
            }
        }
        (Owner::Image, None) => {}
        (Owner::Module(_), _) if r.chance(80) => recv_parts(m, r, at),
        (Owner::Module(_), _) => {
            let mut h = abi::IpcHeader::default();
            let mut buf = vec![0u8; cap];
            // SAFETY: `h` is a header, and `buf` has `cap` bytes.
            let got = unsafe { abi::kernel_ipc_recv(m.boxes[at].addr, 0, &mut h, buf.as_mut_ptr(), cap) };
            if !m.boxes[at].open {
                invariant!(got == abi::IPC_RECV_CLOSED, "a closed mailbox received from: {}", got);
                return;
            }
            match front_len {
                None => invariant!(got == abi::IPC_RECV_NONE, "an empty mailbox gave {}", got),
                Some(len) if len > cap => {
                    invariant!(got == abi::IPC_RECV_TOO_SMALL && h.len == len as u64, "a buffer too small: {} {}",
                               got, h.len);
                    reached("a buffer too small");
                }
                Some(_) => {
                    let w = take(m, at).expect("a front");
                    invariant!(got >= 0, "a message there not given: {}", got);
                    let g = Msg { from: h.from, tag: h.tag, reference: h.reference, data: buf[..got as usize].to_vec() };
                    invariant!(g == w, "mailbox {:x} gave {:?}, the model {:?}", m.boxes[at].addr, g, w);
                    reached("a message received through the ABI");
                }
            }
        }
    }
}

/// The front of a module's mailbox received in parts (`kernel_ipc_recv_part`):
/// each part the bytes the model has there, the message taken with its last
/// -- or left first, all of it, when the reader gives up part way -- and a
/// part past the end, or with a ticket not the first part's, refused.
fn recv_parts(m: &mut Model, r: &mut Input, at: usize) {
    let addr = m.boxes[at].addr;
    let mut h = abi::IpcHeader::default();
    let mut ticket = 0u64;
    let first = r.pick(&[0usize, 1, 7, 64, 1 << 16]);
    let mut buf = vec![0u8; first];
    // SAFETY: `h` and `ticket` are writable, `buf` has `first` bytes.
    let got = unsafe { abi::kernel_ipc_recv_part(addr, 0, &mut h, buf.as_mut_ptr(), first, 0, &mut ticket) };
    if !m.boxes[at].open {
        invariant!(got == abi::IPC_RECV_CLOSED, "a closed mailbox received from in parts: {}", got);
        return;
    }
    let Some(front) = m.boxes[at].queue.front().cloned() else {
        invariant!(got == abi::IPC_RECV_NONE, "an empty mailbox gave a part: {}", got);
        return;
    };
    let len = front.data.len();
    say!("recv_parts {:x}: first {} of {} -> {}", addr, first, len, got);
    invariant!(got >= 0 && h.len == len as u64 && h.from == front.from && h.tag == front.tag
               && h.reference == front.reference, "the first part of {:?}: {} bytes, header {} {:x} {:x} {}", front,
               got, h.len, h.from, h.tag, h.reference);
    let mut offset = got as usize;
    invariant!(offset == first.min(len) && buf[..offset] == front.data[..offset], "the first part's bytes");
    while offset < len {
        match r.u8() % 16 {
            /* Given up part way: the message stays first, all of it */
            0 => {
                reached("a receive in parts given up part way");
                return;
            }
            /* A ticket not the first part's, or a part past the end */
            1 => {
                /* At offset 0 a part starts the message afresh: a ticket is
                 * held to from the second on */
                let (o, t) = if r.bool() { (offset.max(1), ticket ^ 1) } else { (len + 1 + r.below(8) as usize, ticket) };
                let mut t = t;
                let mut b = [0u8; 8];
                // SAFETY: as above, `b` has 8 bytes.
                let got = unsafe { abi::kernel_ipc_recv_part(addr, 0, &mut h, b.as_mut_ptr(), 8, o as u64, &mut t) };
                invariant!(got == abi::IPC_RECV_STALE, "a stale part answered {}", got);
                reached("a stale part refused");
            }
            _ => {}
        }
        let want = r.pick(&[1usize, 3, 100, 4096, 1 << 16]).min(len - offset);
        let mut part = vec![0u8; want];
        // SAFETY: as above, `part` has `want` bytes.
        let got = unsafe {
            abi::kernel_ipc_recv_part(addr, 0, &mut h, part.as_mut_ptr(), want, offset as u64, &mut ticket)
        };
        invariant!(got == want as i64 && part[..] == front.data[offset..offset + want],
                   "a part of {} bytes at {} of {}: {}", want, offset, len, got);
        offset += want;
    }
    let w = take(m, at).expect("a front");
    invariant!(w == front, "the message received in parts is not the front's");
    reached("a message received in parts");
}

fn register(m: &mut Model, r: &mut Input) {
    if m.boxes.is_empty() {
        return;
    }
    let at = r.below(m.boxes.len() as u64) as usize;
    let name = NAMES[r.below(NAMES.len() as u64) as usize];
    let scope = if r.bool() { Scope::Cluster } else { Scope::Host };
    let owner = m.boxes[at].owner;
    let namespace = name.split('.').next().unwrap_or("");
    let mine = match owner {
        _ if namespace == "host" => false,
        Owner::Image => true,
        Owner::Module(i) => {
            let ns = MODULES[i].2.split('-').next().unwrap_or("");
            !["kernel", "ipc", "sys"].contains(&namespace) && namespace == ns
        }
    };
    let want = if !name_ok(name) {
        Err(Error::Invalid)
    } else if !mine {
        Err(Error::NotYours)
    } else if m.stopped {
        Err(Error::Stopped)
    } else if !m.boxes[at].open {
        Err(Error::Gone)
    } else if m.names.iter().any(|n| n.1 == at || n.0 == name) {
        Err(Error::Taken)
    } else {
        Ok(())
    };
    let got = match (owner, &m.image[at]) {
        (Owner::Image, Some(mb)) => mb.register(name, scope),
        (Owner::Image, None) => return,
        (Owner::Module(_), _) => {
            let s = if scope == Scope::Cluster { abi::IPC_SCOPE_CLUSTER } else { abi::IPC_SCOPE_HOST };
            // SAFETY: the name is its length's bytes.
            match unsafe { abi::kernel_ipc_register(m.boxes[at].addr, name.as_ptr(), name.len(), s) } {
                0 => Ok(()),
                c => Err(error_of(c)),
            }
        }
    };
    /* A closed module mailbox through the ABI: Gone before anything else */
    let want = if owner != Owner::Image && !m.boxes[at].open { Err(Error::Gone) } else { want };
    say!("register {:?} on {:x}: {:?}", name, m.boxes[at].addr, got);
    invariant!(got == want, "register {:?} on {:x}: {:?}, the model {:?}", name, m.boxes[at].addr, got, want);
    if got.is_ok() {
        m.names.push((name.to_string(), at, scope));
        let watchers = m.watchers_of(name);
        let addr = m.boxes[at].addr;
        m.tell(name, addr, ipc::TAG_NAME_UP, watchers);
        reached("a name registered");
    }
}

fn monitor(m: &mut Model, r: &mut Input) {
    if m.boxes.is_empty() {
        return;
    }
    let w = r.below(m.boxes.len() as u64) as usize;
    let target = addr_of(m, r);
    let got = match (m.boxes[w].owner, &m.image[w]) {
        (Owner::Image, Some(mb)) => mb.monitor(Addr::from_u64(target)).map_err(|_| ()),
        (Owner::Image, None) => return,
        (Owner::Module(_), _) => match abi::kernel_ipc_monitor(m.boxes[w].addr, target) {
            0 => Err(()),
            id => Ok(id),
        },
    };
    if !m.boxes[w].open || m.boxes[w].monitors.len() >= ipc::MAX_MONITORS {
        invariant!(got.is_err(), "a monitor made by a mailbox closed or at its monitors");
        return;
    }
    let host = Addr::from_u64(target).host;
    if host != 0 {
        let watcher = m.boxes[w].addr;
        match (m.router, m.live(host)) {
            (Some(rt), Some(_)) => {
                /* The router told; refused, the monitor is undone */
                let would = m.would_push(rt, route::HEAD);
                match (would, got) {
                    (Ok(()), Ok(id)) => {
                        let b = &mut m.boxes[w];
                        b.reserved += 1;
                        b.monitors.push((target, id));
                        m.remote.push(RemoteM { id, peer: host, watcher: w, target });
                        let to = Addr { host: 0, ..Addr::from_u64(target) }.to_u64();
                        let head = route::Head { kind: route::MONITOR, peer: host, tag: 0, to: wire::To::Addr(to),
                                                 reference: id };
                        m.routed(rt, watcher, &head, &[]).expect("a push said possible");
                        reached("a mailbox on a peer monitored");
                    }
                    (Err(_), Err(())) => reached("a remote monitor refused by a full router"),
                    (would, got) => panic!("invariant: a remote monitor answered {:?}, the router taking it {:?}", got,
                                           would),
                }
            }
            _ => {
                /* No router, or the peer not up: lost already */
                let Ok(id) = got else { panic!("invariant: a monitor of a host lost refused") };
                let b = &mut m.boxes[w];
                b.reserved += 1;
                b.monitors.push((target, id));
                let data = (Reason::HostLost as u32).to_le_bytes().to_vec();
                m.push_kept(w, Msg { from: target, tag: ipc::TAG_DOWN, reference: id, data });
                reached("a monitor of a host lost");
            }
        }
        return;
    }
    let Ok(id) = got else { panic!("invariant: a monitor refused") };
    let b = &mut m.boxes[w];
    b.reserved += 1;
    b.monitors.push((target, id));
    match m.find(target) {
        Some(t) => m.boxes[t].monitored_by.push((id, w)),
        None => {
            let data = (Reason::NoSuch as u32).to_le_bytes().to_vec();
            m.push_kept(w, Msg { from: target, tag: ipc::TAG_DOWN, reference: id, data });
            reached("a monitor of one gone");
        }
    }
}

fn demonitor(m: &mut Model, r: &mut Input) {
    let held: Vec<(usize, u64)> =
        m.boxes.iter().enumerate().flat_map(|(i, b)| b.monitors.iter().map(move |&(_, id)| (i, id))).collect();
    if held.is_empty() {
        return;
    }
    let (w, id) = held[r.below(held.len() as u64) as usize];
    match (m.boxes[w].owner, &m.image[w]) {
        (Owner::Image, Some(mb)) => mb.demonitor(id),
        (Owner::Image, None) => return,
        (Owner::Module(_), _) => abi::kernel_ipc_demonitor(m.boxes[w].addr, id),
    }
    if !m.boxes[w].open {
        return;
    }
    let Some(i) = m.boxes[w].monitors.iter().position(|&(_, x)| x == id) else { return };
    let target = m.boxes[w].monitors.swap_remove(i).0;
    if Addr::from_u64(target).host != 0 {
        let Some(k) = m.remote.iter().position(|rec| rec.id == id && rec.watcher == w) else { return };
        m.remote.swap_remove(k);
        m.boxes[w].reserved -= 1;
        if let Some(rt) = m.router {
            let to = Addr { host: 0, ..Addr::from_u64(target) }.to_u64();
            let head = route::Head { kind: route::DEMONITOR, peer: Addr::from_u64(target).host, tag: 0,
                                     to: wire::To::Addr(to), reference: id };
            let watcher = m.boxes[w].addr;
            let _ = m.routed(rt, watcher, &head, &[]);
        }
        reached("a remote monitor dropped");
        return;
    }
    if let Some(t) = m.find(target) {
        if let Some(k) = m.boxes[t].monitored_by.iter().position(|rec| rec.0 == id) {
            m.boxes[t].monitored_by.swap_remove(k);
            m.boxes[w].reserved -= 1;
            reached("a monitor dropped");
        }
    }
}

/// A mailbox made the router -- or refused, as one is while another is.
fn make_router(m: &mut Model, r: &mut Input) {
    if m.boxes.is_empty() {
        return;
    }
    let at = r.below(m.boxes.len() as u64) as usize;
    let want = if m.stopped {
        Err(Error::Stopped)
    } else if !m.boxes[at].open {
        Err(Error::Gone)
    } else if m.router.is_some_and(|rt| rt != at) {
        Err(Error::Busy)
    } else {
        Ok(())
    };
    let got = match (m.boxes[at].owner, &m.image[at]) {
        (Owner::Image, Some(mb)) => mb.route(),
        (Owner::Image, None) => return,
        (Owner::Module(_), _) => match abi::kernel_ipc_route(m.boxes[at].addr) {
            0 => Ok(()),
            c => Err(error_of(c)),
        },
    };
    /* A closed module mailbox through the ABI: Gone before anything else */
    let want = if m.boxes[at].owner != Owner::Image && !m.boxes[at].open { Err(Error::Gone) } else { want };
    say!("route {:x}: {:?}", m.boxes[at].addr, got);
    invariant!(got == want, "route {:x}: {:?}, the model {:?}", m.boxes[at].addr, got, want);
    if got.is_ok() {
        m.router = Some(at);
        reached("a router");
    }
}

/// The router -- or another, refused -- says a peer is up: its number.
fn peer_up(m: &mut Model, r: &mut Input) {
    let Some(at) = (if r.chance(32) || m.router.is_none() {
        (!m.boxes.is_empty()).then(|| r.below(m.boxes.len() as u64) as usize)
    } else {
        m.router
    }) else { return };
    let name = PEER_NAMES[r.below(PEER_NAMES.len() as u64) as usize];
    let want = if !host_ok(name) {
        Err(Error::Invalid)
    } else if m.stopped {
        Err(Error::Stopped)
    } else if m.router != Some(at) {
        Err(Error::NotYours)
    } else {
        match m.peers.iter().position(|p| p.as_ref().is_some_and(|p| p.name == name)) {
            Some(i) => {
                let p = m.peers[i].as_ref().expect("found");
                if p.up {
                    Ok(number(i, p.epoch))
                } else {
                    let epoch = if p.epoch >= (u16::MAX >> PEER_BITS) { 1 } else { p.epoch + 1 };
                    Ok(number(i, epoch))
                }
            }
            None => m.peers.iter().position(|p| p.is_none()).map(|i| number(i, 1)).ok_or(Error::Busy),
        }
    };
    let got = match (m.boxes[at].owner, &m.image[at]) {
        (Owner::Image, Some(mb)) => mb.peer_up(name),
        (Owner::Image, None) => return,
        (Owner::Module(_), _) => {
            // SAFETY: the name is its length's bytes.
            match unsafe { abi::kernel_ipc_peer_up(m.boxes[at].addr, name.as_ptr(), name.len()) } {
                0 => Err(Error::NotYours),
                n => Ok(n as u16),
            }
        }
    };
    say!("peer_up {:?} by {:x}: {:?}", name, m.boxes[at].addr, got);
    if m.boxes[at].owner != Owner::Image {
        /* The ABI says 0 for every refusal */
        invariant!(got.is_ok() == want.is_ok() && (got.is_err() || got == want),
                   "peer_up {:?}: {:?}, the model {:?}", name, got, want);
    } else {
        invariant!(got == want, "peer_up {:?}: {:?}, the model {:?}", name, got, want);
    }
    let Ok(host) = want else { return };
    let i = usize::from(host & ((1 << PEER_BITS) - 1)) - 1;
    let was_up = m.peers[i].as_ref().is_some_and(|p| p.up);
    if was_up {
        return;
    }
    match m.peers[i].as_mut() {
        Some(p) => {
            p.epoch = host >> PEER_BITS;
            p.up = true;
        }
        None => m.peers[i] = Some(PeerM { name: name.to_string(), epoch: 1, up: true, sent: VecDeque::new() }),
    }
    let key = format!("host.{}", name);
    let watchers = m.watchers_of(&key);
    m.tell(&key, Addr { host, slot: 0, generation: 0 }.to_u64(), ipc::TAG_NAME_UP, watchers);
    reached("a peer up");
}

/// The router says a peer is lost.
fn peer_lost(m: &mut Model, r: &mut Input) {
    let Some(at) = (if r.chance(32) || m.router.is_none() {
        (!m.boxes.is_empty()).then(|| r.below(m.boxes.len() as u64) as usize)
    } else {
        m.router
    }) else { return };
    let i = r.below(4) as usize;
    let host = number(i, m.peers[i].as_ref().map_or(1, |p| p.epoch));
    let want = if m.router != Some(at) {
        Err(Error::NotYours)
    } else if m.live(host).is_none() {
        Err(Error::Gone)
    } else {
        Ok(())
    };
    let got = match (m.boxes[at].owner, &m.image[at]) {
        (Owner::Image, Some(mb)) => mb.peer_lost(host),
        (Owner::Image, None) => return,
        (Owner::Module(_), _) => match abi::kernel_ipc_peer_lost(m.boxes[at].addr, u32::from(host)) {
            0 => Ok(()),
            c => Err(error_of(c)),
        },
    };
    let want = if m.boxes[at].owner != Owner::Image && !m.boxes[at].open { Err(Error::Gone) } else { want };
    say!("peer_lost {:x} by {:x}: {:?}", host, m.boxes[at].addr, got);
    invariant!(got == want, "peer_lost {:x}: {:?}, the model {:?}", host, got, want);
    if got.is_ok() {
        if let Some((key, watchers)) = m.lose(i, host) {
            m.tell(&key, Addr { host, slot: 0, generation: 0 }.to_u64(), ipc::TAG_NAME_DOWN, watchers);
        }
        reached("a peer lost");
    }
}

/// What a peer says, come through the router: to a name, or to an address
/// here -- one that sent there now and then.
fn deliver_remote(m: &mut Model, r: &mut Input) {
    let Some(at) = (if r.chance(32) || m.router.is_none() {
        (!m.boxes.is_empty()).then(|| r.below(m.boxes.len() as u64) as usize)
    } else {
        m.router
    }) else { return };
    let i = r.below(4) as usize;
    let host = number(i, m.peers[i].as_ref().map_or(1, |p| p.epoch));
    let from = match r.u8() % 8 {
        0 => Addr { host: 1, slot: 3, generation: 1 }.to_u64(),
        1 => 0,
        _ => Addr { host: 0, slot: 1 + r.u16() % 16, generation: 1 + u32::from(r.u8() % 3) }.to_u64(),
    };
    let by_name = r.bool();
    let name = NAMES[r.below(NAMES.len() as u64) as usize];
    let to = match m.peers[i].as_ref().and_then(|p| p.sent.front().copied()) {
        Some(a) if r.bool() => a,
        _ => addr_of(m, r),
    };
    let tag = TAGS[r.below(TAGS.len() as u64) as usize];
    let reference = r.u64();
    let data = pattern(r.below(100) as usize, r.u8());
    let want = if by_name && !name_ok(name) {
        Err(Error::Invalid)
    } else if tag >= ipc::TAG_SYSTEM {
        Err(Error::Invalid)
    } else if Addr::from_u64(from).host != 0 || Addr::from_u64(from).slot == 0 {
        Err(Error::Invalid)
    } else if m.stopped {
        Err(Error::Stopped)
    } else if m.router != Some(at) {
        Err(Error::NotYours)
    } else {
        match m.live(host) {
            None => Err(Error::NoRoute),
            Some(pi) => {
                let target = if by_name {
                    match m.names.iter().find(|n| n.0 == name) {
                        None => Err(Error::Gone),
                        Some(n) if n.2 != Scope::Cluster => Err(Error::NotYours),
                        Some(n) => Ok(m.boxes[n.1].addr),
                    }
                } else if Addr::from_u64(to).host != 0 {
                    Err(Error::Invalid)
                } else if !m.peers[pi].as_ref().is_some_and(|p| p.sent.contains(&to)) {
                    Err(Error::NotYours)
                } else {
                    Ok(to)
                };
                target.and_then(|a| m.find(a).ok_or(Error::Gone)).and_then(|t| {
                    let f = Addr::from_u64(from);
                    let from = Addr { host, slot: f.slot, generation: f.generation }.to_u64();
                    m.push(t, Msg { from, tag, reference, data: data.clone() })
                })
            }
        }
    };
    let got = match (m.boxes[at].owner, &m.image[at]) {
        (Owner::Image, Some(mb)) => {
            let target = if by_name { To::Name(name) } else { To::Addr(Addr::from_u64(to)) };
            mb.deliver_remote(host, from, target, tag, reference, Payload::from_slice(&data).expect("a payload"))
        }
        (Owner::Image, None) => return,
        (Owner::Module(_), _) => {
            let (np, nl) = if by_name { (name.as_ptr(), name.len()) } else { (core::ptr::null(), 0) };
            /* A name of no bytes through the ABI is the address */
            if by_name && name.is_empty() {
                return;
            }
            /* The data in pieces, as an assembled message has it */
            let cuts = split(&data, r);
            let pieces: Vec<abi::IpcPiece> = cuts.iter().map(|c| abi::IpcPiece { data: c.as_ptr(), len: c.len() })
                .collect();
            // SAFETY: the name is its length's bytes, and each piece names its own.
            let code = unsafe {
                abi::kernel_ipc_deliver_remote(m.boxes[at].addr, u32::from(host), from, to, np, nl, tag, reference,
                                               pieces.as_ptr(), pieces.len())
            };
            match code {
                0 => Ok(()),
                c => Err(error_of(c)),
            }
        }
    };
    let want = if m.boxes[at].owner != Owner::Image && !m.boxes[at].open { Err(Error::Gone) } else { want };
    say!("deliver_remote {:x} {:x} -> {:x}{}: {:?}", host, from, to, if by_name { name } else { "" }, got);
    invariant!(got == want, "deliver_remote from {:x}:{:x} to {:x}{}: {:?}, the model {:?}", host, from, to,
               if by_name { name } else { "" }, got, want);
    if got.is_ok() {
        reached("a message from a peer delivered");
    } else if got == Err(Error::NotYours) {
        reached("a peer refused where it may not deliver");
    }
}

/// The router says a message of one of this host's mailboxes did not get
/// to a peer's: told to its sender -- a mailbox that sent to that peer, and
/// that one alone -- if it has room.
fn bounced(m: &mut Model, r: &mut Input) {
    let Some(at) = (if r.chance(32) || m.router.is_none() {
        (!m.boxes.is_empty()).then(|| r.below(m.boxes.len() as u64) as usize)
    } else {
        m.router
    }) else { return };
    let i = r.below(4) as usize;
    let host = number(i, m.peers[i].as_ref().map_or(1, |p| p.epoch));
    let sender = match m.peers[i].as_ref().and_then(|p| p.sent.front().copied()) {
        Some(a) if r.bool() => a,
        _ => match r.u8() % 8 {
            0 => Addr { host: 1, slot: 3, generation: 1 }.to_u64(),
            1 => 0,
            _ => addr_of(m, r),
        },
    };
    let by_name = r.bool();
    let name = NAMES[r.below(NAMES.len() as u64) as usize];
    let target = match r.u8() % 8 {
        0 => Addr { host: 2, slot: 4, generation: 1 }.to_u64(),
        _ => Addr { host: 0, slot: 1 + r.u16() % 16, generation: 1 + u32::from(r.u8() % 3) }.to_u64(),
    };
    let tag = TAGS[r.below(TAGS.len() as u64) as usize];
    let reference = r.u64();
    let why = r.pick(&[Bounce::Full, Bounce::Gone, Bounce::Unsent]);
    let (s, t) = (Addr::from_u64(sender), Addr::from_u64(target));
    let want = if by_name && !name_ok(name) {
        Err(Error::Invalid)
    } else if tag >= ipc::TAG_SYSTEM || s.host != 0 || s.slot == 0 {
        Err(Error::Invalid)
    } else if !by_name && t.host != 0 {
        Err(Error::Invalid)
    } else if m.stopped {
        Err(Error::Stopped)
    } else if m.router != Some(at) {
        Err(Error::NotYours)
    } else {
        match m.live(host) {
            None => Err(Error::NoRoute),
            Some(pi) if !m.peers[pi].as_ref().is_some_and(|p| p.sent.contains(&sender)) => Err(Error::NotYours),
            Some(_) => match m.find(sender) {
                None => Err(Error::Gone),
                Some(k) => {
                    let from = if by_name {
                        Addr { host, slot: 0, generation: 0 }
                    } else {
                        Addr { host, slot: t.slot, generation: t.generation }
                    };
                    let mut data = why.code().to_le_bytes().to_vec();
                    data.extend_from_slice(&tag.to_le_bytes());
                    if by_name {
                        data.extend_from_slice(name.as_bytes());
                    }
                    m.push(k, Msg { from: from.to_u64(), tag: ipc::TAG_BOUNCED, reference, data })
                }
            },
        }
    };
    let got = match (m.boxes[at].owner, &m.image[at]) {
        (Owner::Image, Some(mb)) => {
            let to = if by_name { To::Name(name) } else { To::Addr(t) };
            mb.bounced(host, s, to, tag, reference, why)
        }
        (Owner::Image, None) => return,
        (Owner::Module(_), _) => {
            /* A name of no bytes through the ABI is the address */
            if by_name && name.is_empty() {
                return;
            }
            let (np, nl) = if by_name { (name.as_ptr(), name.len()) } else { (core::ptr::null(), 0) };
            // SAFETY: the name is its length's bytes.
            let code = unsafe {
                abi::kernel_ipc_bounced(m.boxes[at].addr, u32::from(host), sender, target, np, nl, tag, reference,
                                        why.code())
            };
            match code {
                0 => Ok(()),
                c => Err(error_of(c)),
            }
        }
    };
    let want = if m.boxes[at].owner != Owner::Image && !m.boxes[at].open { Err(Error::Gone) } else { want };
    say!("bounced {:x} {:x} -> {:x}{} {:?}: {:?}", host, sender, target, if by_name { name } else { "" }, why, got);
    invariant!(got == want, "bounced from {:x} to {:x}:{:x}{} ({:?}): {:?}, the model {:?}", sender, host, target,
               if by_name { name } else { "" }, why, got, want);
    match got {
        Ok(()) => reached("a bounce told to its sender"),
        Err(Error::NotYours) => reached("a bounce refused for a mailbox that never sent there"),
        Err(Error::Full) => reached("a bounce dropped for want of room"),
        _ => {}
    }
}

/// A peer says a mailbox a remote monitor watched has gone.
fn remote_down(m: &mut Model, r: &mut Input) {
    let Some(at) = (if r.chance(32) || m.router.is_none() {
        (!m.boxes.is_empty()).then(|| r.below(m.boxes.len() as u64) as usize)
    } else {
        m.router
    }) else { return };
    let (id, host) = match m.remote.get(r.below(m.remote.len() as u64 + 1) as usize) {
        Some(rec) if r.u8() > 16 => (rec.id, rec.peer),
        _ => (r.u64(), number(r.below(4) as usize, 1)),
    };
    let reason = r.pick(&[Reason::Closed, Reason::NoSuch, Reason::Unloaded]);
    let want = if m.router != Some(at) {
        Err(Error::NotYours)
    } else {
        m.remote.iter().position(|rec| rec.id == id && rec.peer == host).map(|_| ()).ok_or(Error::Gone)
    };
    let got = match (m.boxes[at].owner, &m.image[at]) {
        (Owner::Image, Some(mb)) => mb.remote_down(host, id, reason),
        (Owner::Image, None) => return,
        (Owner::Module(_), _) => match abi::kernel_ipc_remote_down(m.boxes[at].addr, u32::from(host), id, reason as u32) {
            0 => Ok(()),
            c => Err(error_of(c)),
        },
    };
    let want = if m.boxes[at].owner != Owner::Image && !m.boxes[at].open { Err(Error::Gone) } else { want };
    invariant!(got == want, "remote_down {:x} {}: {:?}, the model {:?}", host, id, got, want);
    if got.is_ok() {
        let k = m.remote.iter().position(|rec| rec.id == id && rec.peer == host).expect("found above");
        let rec = m.remote.swap_remove(k);
        let data = (reason as u32).to_le_bytes().to_vec();
        m.push_kept(rec.watcher, Msg { from: rec.target, tag: ipc::TAG_DOWN, reference: rec.id, data });
        reached("a peer's Down delivered");
    }
}

fn watch(m: &mut Model, r: &mut Input) {
    if m.boxes.is_empty() {
        return;
    }
    let w = r.below(m.boxes.len() as u64) as usize;
    let prefix = PREFIXES[r.below(PREFIXES.len() as u64) as usize];
    let got = match (m.boxes[w].owner, &m.image[w]) {
        (Owner::Image, Some(mb)) => mb.watch(prefix).map_err(|_| ()),
        (Owner::Image, None) => return,
        // SAFETY: the prefix is its length's bytes.
        (Owner::Module(_), _) => match unsafe { abi::kernel_ipc_watch(m.boxes[w].addr, prefix.as_ptr(), prefix.len()) } {
            0 => Err(()),
            id => Ok(id),
        },
    };
    let refused = !prefix_ok(prefix) || !m.boxes[w].open || m.boxes[w].watches.len() >= ipc::MAX_WATCHES_EACH
        || m.watches.len() >= ipc::MAX_WATCHES;
    invariant!(got.is_err() == refused, "watch {:?}: {:?}, the model refusing {}", prefix, got, refused);
    let Ok(id) = got else { return };
    m.boxes[w].reserved += 1;
    m.boxes[w].watches.push(id);
    m.watches.push(WatchRec { id, prefix: prefix.to_string(), watcher: w, lost_pending: true });
    reached("a watch");
}

fn unwatch(m: &mut Model, r: &mut Input) {
    let held: Vec<(usize, u64)> =
        m.boxes.iter().enumerate().flat_map(|(i, b)| b.watches.iter().map(move |&id| (i, id))).collect();
    if held.is_empty() {
        return;
    }
    let (w, id) = held[r.below(held.len() as u64) as usize];
    match (m.boxes[w].owner, &m.image[w]) {
        (Owner::Image, Some(mb)) => mb.unwatch(id),
        (Owner::Image, None) => return,
        (Owner::Module(_), _) => abi::kernel_ipc_unwatch(m.boxes[w].addr, id),
    }
    if !m.boxes[w].open {
        return;
    }
    let rec = m.watches.iter().position(|x| x.id == id && x.watcher == w).map(|i| m.watches.swap_remove(i));
    m.boxes[w].watches.retain(|&x| x != id);
    if rec.is_some_and(|x| x.lost_pending) {
        m.boxes[w].reserved -= 1;
    }
}

/// A made-up module unloaded: whatever it left open closed, in the table's
/// order, as the sweep after its exit does it.
fn unload(m: &mut Model, r: &mut Input) {
    let which = r.below(MODULES.len() as u64) as usize;
    ipc::module_gone(MODULES[which].2.as_bytes());
    let mut left: Vec<(u16, usize)> = m.boxes.iter().enumerate()
        .filter(|(_, b)| b.open && b.owner == Owner::Module(which))
        .map(|(i, b)| (Addr::from_u64(b.addr).slot, i)).collect();
    left.sort();
    for (_, at) in left {
        m.close(at, Reason::Unloaded);
        reached("a module's mailbox swept");
    }
}

/// How long a sleeper sleeps at most.
const SLEEP_NS: u64 = 50_000_000;

/// A sleeper's mailbox, and where it says what it got and that it is done.
type SleeperCtx = (u64, Arc<Mutex<Option<(i64, Vec<u8>)>>>, Arc<AtomicBool>);

/// The sleeper: a receive of up to `SLEEP_NS`, its answer handed back.
fn sleep_in((addr, res, fin): SleeperCtx) {
    let mut h = abi::IpcHeader::default();
    let mut buf = vec![0u8; 4096];
    // SAFETY: `h` is a header, and `buf` has 4096 bytes.
    let got = unsafe { abi::kernel_ipc_recv(addr, SLEEP_NS, &mut h, buf.as_mut_ptr(), buf.len()) };
    let n = usize::try_from(got).unwrap_or(0);
    say!("sleeper {:x}: {} from {:x} tag {:x} ref {:x} {:?}", addr, got, h.from, h.tag, h.reference, &buf[..n]);
    *res.lock().unwrap() = Some((got, buf[..n].to_vec()));
    fin.store(true, Ordering::Release);
}

/// A task asleep in a module mailbox's receive, and what wakes it: a
/// message, a wake, the mailbox closed, or its time running out.
fn sleeper(m: &mut Model, r: &mut Input) {
    let candidates: Vec<usize> = (0..m.boxes.len())
        .filter(|&i| m.boxes[i].open && m.boxes[i].owner != Owner::Image && m.boxes[i].queue.is_empty()).collect();
    if candidates.is_empty() {
        return;
    }
    let at = candidates[r.below(candidates.len() as u64) as usize];
    let addr = m.boxes[at].addr;
    let result: Arc<Mutex<Option<(i64, Vec<u8>)>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let (res, fin) = (result.clone(), done.clone());
    let Some(task) = kcore::task::spawn_with("ipc-sleeper", (addr, res, fin), sleep_in) else { return };
    /* Let it get to its sleep */
    sched::world_wait(sched::now() + 1_000_000);
    let how = r.u8() % 4;
    let mut sent = None;
    match how {
        0 => {
            /* A message from a module mailbox of the model's, or none */
            let data = pattern(r.below(100) as usize, r.u8());
            let from = m.boxes.iter().position(|b| b.open && b.owner != Owner::Image);
            if let Some(f) = from {
                // SAFETY: the data is its length's bytes; no name.
                let code = unsafe {
                    abi::kernel_ipc_send(m.boxes[f].addr, addr, core::ptr::null(), 0, 3, 9, data.as_ptr(), data.len())
                };
                let want = if m.stopped {
                    Err(Error::Stopped)
                } else {
                    m.push(at, Msg { from: m.boxes[f].addr, tag: 3, reference: 9, data: data.clone() })
                };
                invariant!((code == 0) == want.is_ok(), "a message to a sleeper: {} {:?}", code, want);
                if code == 0 {
                    sent = Some(data);
                }
            }
        }
        1 => abi::kernel_ipc_wake(addr),
        2 => {
            abi::kernel_ipc_close(addr);
            m.close(at, Reason::Closed);
        }
        _ => {}
    }
    while !done.load(Ordering::Acquire) {
        sched::world_wait(sched::now() + SLEEP_NS);
    }
    drop(task);
    let (got, data) = result.lock().unwrap().take().expect("the sleeper's answer");
    match (how, sent) {
        (0, Some(d)) => {
            let w = take(m, at).expect("the message sent");
            invariant!(got >= 0 && data == d && w.data == d, "the sleeper got {} bytes, sent {}", got, d.len());
            reached("a sleeper woken by a message");
        }
        (2, _) => {
            invariant!(got == abi::IPC_RECV_CLOSED, "a sleeper whose mailbox closed got {}", got);
            reached("a sleeper woken by a close");
        }
        _ => {
            invariant!(got == abi::IPC_RECV_NONE, "a sleeper with nothing for it got {}", got);
            reached("a sleeper woken with nothing");
        }
    }
}

fn run(r: &mut Input) {
    SAY.store(std::env::var_os("IPC_FUZZ_SAY").is_some(), Ordering::Relaxed);
    let mut m = Model { boxes: Vec::new(), image: Vec::new(), names: Vec::new(), watches: Vec::new(), stopped: false,
                        router: None, peers: (0..ipc::MAX_PEERS).map(|_| None).collect(), remote: Vec::new() };
    while let Some(op) = r.op(18) {
        match op {
            0 | 1 => open(&mut m, r),
            2 => close(&mut m, r),
            3 | 4 => send(&mut m, r),
            5 | 6 => recv(&mut m, r),
            7 => register(&mut m, r),
            8 => monitor(&mut m, r),
            9 => {
                if r.bool() {
                    demonitor(&mut m, r)
                } else {
                    unwatch(&mut m, r)
                }
            }
            10 => watch(&mut m, r),
            11 => {
                if r.chance(64) {
                    unload(&mut m, r)
                } else if r.chance(8) && !m.stopped {
                    ipc::stop();
                    m.stopped = true;
                    reached("the halt");
                } else {
                    sleeper(&mut m, r)
                }
            }
            12 => {
                let name = NAMES[r.below(NAMES.len() as u64) as usize];
                let want = m.names.iter().find(|n| n.0 == name).map(|n| m.boxes[n.1].addr);
                invariant!(ipc::lookup(name).map(|a| a.to_u64()) == want, "lookup {:?}", name);
            }
            13 => make_router(&mut m, r),
            14 => peer_up(&mut m, r),
            15 => {
                if r.chance(64) {
                    peer_lost(&mut m, r)
                } else {
                    remote_down(&mut m, r)
                }
            }
            _ => {
                if r.chance(64) {
                    bounced(&mut m, r)
                } else {
                    deliver_remote(&mut m, r)
                }
            }
        }
        say!("op {}: queued {} model {}", op, ipc::counters().queued_bytes, m.queued_bytes());
        compare(&m);
    }
    /* Everything closed at the end: nothing of it left anywhere */
    for at in 0..m.boxes.len() {
        if !m.boxes[at].open {
            continue;
        }
        match m.boxes[at].owner {
            Owner::Image => m.image[at] = None,
            Owner::Module(_) => abi::kernel_ipc_close(m.boxes[at].addr),
        }
        m.close(at, Reason::Closed);
    }
    compare(&m);
    let c = ipc::counters();
    invariant!(c.queued_bytes == 0 && c.mailboxes == 0 && c.names == 0 && c.watches == 0,
               "left at the end: {} bytes, {} mailboxes, {} names, {} watches", c.queued_bytes, c.mailboxes,
               c.names, c.watches);
}
