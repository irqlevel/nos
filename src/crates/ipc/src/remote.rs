//! What crosses to other hosts (plans/ipc.md, "How I4 is built"): the
//! router -- the mailbox that carries messages there, ipcnet's -- and the
//! peers it tells the channel of; a message for a peer handed to the router;
//! what comes from a peer, delivered only where a peer may deliver; and
//! monitors of mailboxes on peers, kept here, on the watcher's host, with
//! their Downs made and their places kept, so that a peer lost ends them
//! with nothing from the peer.
//!
//! A peer's number is this host's own: the host part of the addresses of
//! its mailboxes here, its index in the low bits and an epoch above, the
//! epoch bumped each time it comes up after it was lost -- so an address
//! held from before a loss is refused, never another mailbox's. On the wire
//! an address is always the owning host's (host 0), and the router
//! rewrites: a `to` going out loses its number, a `from` coming in gets the
//! sender's.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use ipcwire::route;

use super::{copy, count, next_id, push, push_kept, table, tell_watchers, watchers_of, Addr, Error, Mbox, Message,
            Payload, Reason, Scope, Table, DOWN_BYTES, MAX_MONITORS, MAX_PAYLOAD, STATS, TAG_DOWN, TAG_NAME_DOWN,
            TAG_NAME_UP, TAG_ROUTE, TAG_SYSTEM};

/// Peers at most: what the low bits of a number hold, less 0, this host.
pub const MAX_PEERS: usize = 63;
const PEER_BITS: u32 = 6;
const EPOCH_MAX: u16 = u16::MAX >> PEER_BITS;
/// The mailboxes a peer may answer, at most: the newest that sent there.
pub const MAX_SENT: usize = 256;
/// Monitors of mailboxes on peers, at most, the host's.
pub const MAX_REMOTE_MONITORS: usize = 1024;
/// A host's name at most: a DNS label's.
pub const HOST_NAME_MAX: usize = 63;
/// The prefix a host's coming and going is told under, to a watch of it.
pub const HOST_PREFIX: &str = "host.";

pub(crate) struct Peer {
    pub(crate) name: String,
    pub(crate) epoch: u16,
    pub(crate) up: bool,
    /// Mailboxes of this host that have sent to it, the oldest first: what
    /// it may deliver to by address -- a reply's way back.
    pub(crate) sent: VecDeque<Addr>,
}

/// A monitor of a mailbox on a peer, kept here.
pub(crate) struct RemoteMonitor {
    pub(crate) id: u64,
    /// The peer's number, its epoch too.
    pub(crate) peer: u16,
    pub(crate) watcher: Arc<Mbox>,
    pub(crate) down: Message,
}

/// Where a message for a peer goes there.
pub enum RemoteTo<'a> {
    /// The peer's own address of a mailbox: host 0.
    Addr(Addr),
    Name(&'a str),
}

fn number(index: usize, epoch: u16) -> u16 {
    (epoch << PEER_BITS) | (index as u16 + 1)
}

fn index_of(host: u16) -> Option<usize> {
    let i = usize::from(host & ((1 << PEER_BITS) - 1));
    if i == 0 { None } else { Some(i - 1) }
}

/// A host's name: a DNS label, lowercase.
pub fn valid_host(name: &str) -> bool {
    !name.is_empty() && name.len() <= HOST_NAME_MAX
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// The peer `host` names, up and of that epoch: its index.
pub(crate) fn live(t: &Table, host: u16) -> Option<usize> {
    let i = index_of(host)?;
    let p = t.peers.get(i)?.as_ref()?;
    (p.up && p.epoch == host >> PEER_BITS).then_some(i)
}

/// The peer named `name`, up: its number.
pub(crate) fn live_by_name(t: &Table, name: &str) -> Option<u16> {
    t.peers.iter().enumerate().find_map(|(i, p)| {
        let p = p.as_ref()?;
        (p.up && p.name == name).then(|| number(i, p.epoch))
    })
}

fn is_router(t: &Table, mbox: &Arc<Mbox>) -> bool {
    t.router.as_ref().is_some_and(|r| Arc::ptr_eq(r, mbox))
}

fn host_key(name: &str) -> Result<String, Error> {
    let mut key = String::new();
    key.try_reserve_exact(HOST_PREFIX.len() + name.len()).map_err(|_| Error::NoMemory)?;
    key.push_str(HOST_PREFIX);
    key.push_str(name);
    Ok(key)
}

/// `mbox` made the router. One at a time, until it closes.
pub(crate) fn route(mbox: &Arc<Mbox>) -> Result<(), Error> {
    let mut t = table().ok_or(Error::NoMemory)?.lock();
    if t.stopped {
        return Err(Error::Stopped);
    }
    let slot = usize::from(mbox.addr.slot);
    if !t.slots[slot].mbox.as_ref().is_some_and(|m| Arc::ptr_eq(m, mbox)) {
        return Err(Error::Gone);
    }
    match &t.router {
        Some(r) if !Arc::ptr_eq(r, mbox) => Err(Error::Busy),
        _ => {
            t.router = Some(mbox.clone());
            Ok(())
        }
    }
}

/// The peer `name` up: its number -- the one it has while it stays up, and
/// another, of a new epoch, once it was lost.
pub(crate) fn peer_up(router: &Arc<Mbox>, name: &str) -> Result<u16, Error> {
    if !valid_host(name) {
        return Err(Error::Invalid);
    }
    let copied = copy(name)?;
    let key = host_key(name)?;
    let table = table().ok_or(Error::NoMemory)?;
    let (host, watchers) = {
        let mut t = table.lock();
        if t.stopped {
            return Err(Error::Stopped);
        }
        if !is_router(&t, router) {
            return Err(Error::NotYours);
        }
        let at = t.peers.iter().position(|p| p.as_ref().is_some_and(|p| p.name == name));
        let host = match at {
            Some(i) => {
                let Some(p) = t.peers[i].as_mut() else { return Err(Error::Gone) };
                if p.up {
                    return Ok(number(i, p.epoch));
                }
                p.epoch = if p.epoch >= EPOCH_MAX { 1 } else { p.epoch + 1 };
                p.up = true;
                number(i, p.epoch)
            }
            None => {
                let i = t.peers.iter().position(|p| p.is_none()).ok_or(Error::Busy)?;
                t.peers[i] = Some(Peer { name: copied, epoch: 1, up: true, sent: VecDeque::new() });
                number(i, 1)
            }
        };
        (host, watchers_of(&t, &key))
    };
    count(&STATS.peers_up);
    tell_watchers(&key, Addr { host, slot: 0, generation: 0 }, TAG_NAME_UP, watchers);
    Ok(host)
}

/// The peer `host` lost: its monitors' Downs delivered -- HostLost -- and
/// what it may deliver to forgotten. Under the table's lock, a mailbox's
/// after it, as the channel's order is.
pub(crate) fn peer_lost(router: &Arc<Mbox>, host: u16) -> Result<(), Error> {
    let table = table().ok_or(Error::NoMemory)?;
    let (name, watchers) = {
        let mut t = table.lock();
        if !is_router(&t, router) {
            return Err(Error::NotYours);
        }
        let i = live(&t, host).ok_or(Error::Gone)?;
        lose(&mut t, i, host)
    };
    if let Some(key) = name.and_then(|n| host_key(&n).ok()) {
        tell_watchers(&key, Addr { host, slot: 0, generation: 0 }, TAG_NAME_DOWN, watchers);
    }
    Ok(())
}

/// The peer at `i` lost, the table held: its name and the watchers of it,
/// for the caller to tell with the lock let go.
fn lose(t: &mut Table, i: usize, host: u16) -> (Option<String>, Vec<(Arc<Mbox>, u64)>) {
    let Some(p) = t.peers[i].as_mut() else { return (None, Vec::new()) };
    p.up = false;
    p.sent.clear();
    let name = copy(&p.name).ok();
    let mut k = 0;
    while k < t.remote_monitors.len() {
        if t.remote_monitors[k].peer == host {
            let rec = t.remote_monitors.swap_remove(k);
            deliver(rec, Reason::HostLost);
        } else {
            k += 1;
        }
    }
    count(&STATS.peers_lost);
    let watchers = name.as_deref().and_then(|n| host_key(n).ok()).map(|key| watchers_of(t, &key)).unwrap_or_default();
    (name, watchers)
}

/// Every live peer lost: what a router's close leaves, with nothing to carry
/// messages there any more. The table held.
pub(crate) fn lose_all(t: &mut Table) -> Vec<(String, u16, Vec<(Arc<Mbox>, u64)>)> {
    let mut told = Vec::new();
    for i in 0..t.peers.len() {
        let Some(host) = t.peers[i].as_ref().filter(|p| p.up).map(|p| number(i, p.epoch)) else { continue };
        let (name, watchers) = lose(t, i, host);
        if let Some(name) = name {
            if told.try_reserve(1).is_ok() {
                told.push((name, host, watchers));
            }
        }
    }
    told
}

/// Told, with the table let go: what `lose_all` collected.
pub(crate) fn tell_lost(told: Vec<(String, u16, Vec<(Arc<Mbox>, u64)>)>) {
    for (name, host, watchers) in told {
        if let Ok(key) = host_key(&name) {
            tell_watchers(&key, Addr { host, slot: 0, generation: 0 }, TAG_NAME_DOWN, watchers);
        }
    }
}

/// A remote monitor's Down, its reason written in, into its kept place.
fn deliver(rec: RemoteMonitor, reason: Reason) {
    let RemoteMonitor { watcher, mut down, .. } = rec;
    down.payload.overwrite(&(reason as u32).to_le_bytes());
    count(&STATS.downs);
    push_kept(&watcher, down);
}

/// A routed message: the head, then `body`'s bytes, into the router.
fn hand(router: &Arc<Mbox>, from: Addr, head: &route::Head<'_>, body: Option<&Payload>)
    -> Result<(), Error>
{
    let bytes = route::encode(head).map_err(|_| Error::Invalid)?;
    let mut parts: Vec<&[u8]> = Vec::new();
    let n = body.map_or(0, |b| b.pieces().count());
    parts.try_reserve_exact(1 + n).map_err(|_| Error::NoMemory)?;
    parts.push(&bytes);
    if let Some(b) = body {
        parts.extend(b.pieces());
    }
    let payload = Payload::gather_within(&parts, MAX_PAYLOAD + route::HEAD)?;
    push(router, Message { from, tag: TAG_ROUTE, reference: 0, payload }).map_err(|(e, _)| e)
}

/// A message for the peer `host`, into the router -- its sender kept as one
/// the peer may answer, first, so that no answer can come back before it.
pub(crate) fn send(from: Addr, router: Arc<Mbox>, host: u16, to: RemoteTo<'_>, tag: u32, reference: u64,
                   payload: Payload) -> Result<(), (Error, Payload)> {
    let wire_to = match to {
        RemoteTo::Addr(a) => ipcwire::To::Addr(Addr { host: 0, ..a }.to_u64()),
        RemoteTo::Name(n) => ipcwire::To::Name(n.as_bytes()),
    };
    if from.host == 0 && from.slot != 0 {
        if let Some(table) = table() {
            let mut t = table.lock();
            if let Some(i) = live(&t, host) {
                if let Some(p) = t.peers[i].as_mut() {
                    if !p.sent.contains(&from) {
                        if p.sent.len() >= MAX_SENT {
                            p.sent.pop_front();
                        }
                        if p.sent.try_reserve(1).is_ok() {
                            p.sent.push_back(from);
                        }
                    }
                }
            }
        }
    }
    let head = route::Head { kind: route::SEND, peer: host, tag, to: wire_to, reference };
    match hand(&router, from, &head, Some(&payload)) {
        Ok(()) => {
            count(&STATS.sent);
            count(&STATS.routed);
            Ok(())
        }
        Err(e) => Err((e, payload)),
    }
}

/// What comes from the peer `host`: delivered to a `Cluster` name, or to a
/// mailbox that has sent to that peer -- `NotYours` for anything else, and
/// never a system message's tag.
pub(crate) fn deliver_remote(router: &Arc<Mbox>, host: u16, from: u64, to: RemoteTo<'_>, tag: u32,
                             reference: u64, payload: Payload) -> Result<(), Error> {
    if tag >= TAG_SYSTEM {
        return Err(Error::Invalid);
    }
    let from = Addr::from_u64(from);
    if from.host != 0 || from.slot == 0 {
        return Err(Error::Invalid);
    }
    let target = {
        let t = table().ok_or(Error::NoMemory)?.lock();
        if t.stopped {
            return Err(Error::Stopped);
        }
        if !is_router(&t, router) {
            return Err(Error::NotYours);
        }
        let i = live(&t, host).ok_or(Error::NoRoute)?;
        let addr = match to {
            RemoteTo::Name(n) => {
                let rec = t.names.iter().find(|r| r.name == n).ok_or(Error::Gone)?;
                if rec.scope != Scope::Cluster {
                    count(&STATS.refused_remote);
                    return Err(Error::NotYours);
                }
                rec.addr
            }
            RemoteTo::Addr(a) => {
                if a.host != 0 {
                    return Err(Error::Invalid);
                }
                if !t.peers[i].as_ref().is_some_and(|p| p.sent.contains(&a)) {
                    count(&STATS.refused_remote);
                    return Err(Error::NotYours);
                }
                a
            }
        };
        let slot = usize::from(addr.slot);
        if slot == 0 || slot >= t.slots.len() {
            return Err(Error::Gone);
        }
        t.slots[slot].mbox.as_ref().filter(|m| m.addr == addr).cloned().ok_or(Error::Gone)?
    };
    let from = Addr { host, slot: from.slot, generation: from.generation };
    match push(&target, Message { from, tag, reference, payload }) {
        Ok(()) => {
            count(&STATS.delivered_remote);
            Ok(())
        }
        Err((e, _)) => Err(e),
    }
}

/// `watcher` told once, with a Down, when `target` -- a mailbox on a peer --
/// goes, or its peer is lost: the monitor's id. The record and its Down are
/// kept here; the router is asked to have the peer watch.
pub(crate) fn monitor(watcher: &Arc<Mbox>, target: Addr) -> Result<u64, Error> {
    let id = next_id();
    let down = Message { from: target, tag: TAG_DOWN, reference: id, payload: Payload::from_slice(&[0; DOWN_BYTES])? };
    {
        let mut w = watcher.inner.lock();
        if w.closed {
            return Err(Error::Gone);
        }
        if w.monitors.len() >= MAX_MONITORS {
            return Err(Error::Busy);
        }
        w.monitors.try_reserve(1).map_err(|_| Error::NoMemory)?;
        w.keep_one(&watcher.limits)?;
        w.monitors.push((target, id));
    }
    let undo = |watcher: &Arc<Mbox>| {
        let mut w = watcher.inner.lock();
        w.reserved = w.reserved.saturating_sub(1);
        w.monitors.retain(|&(_, m)| m != id);
    };
    let table = table().ok_or(Error::NoMemory)?;
    let router = {
        let mut t = table.lock();
        match (t.router.clone(), live(&t, target.host)) {
            (Some(router), Some(_)) => {
                if t.remote_monitors.len() >= MAX_REMOTE_MONITORS || t.remote_monitors.try_reserve(1).is_err() {
                    drop(t);
                    undo(watcher);
                    return Err(Error::Busy);
                }
                t.remote_monitors.push(RemoteMonitor { id, peer: target.host, watcher: watcher.clone(), down });
                Some(router)
            }
            /* No router, or the peer not up: lost already -- its Down now */
            _ => {
                drop(t);
                deliver(RemoteMonitor { id, peer: target.host, watcher: watcher.clone(), down }, Reason::HostLost);
                None
            }
        }
    };
    let Some(router) = router else { return Ok(id) };
    let head = route::Head { kind: route::MONITOR, peer: target.host, tag: 0,
                             to: ipcwire::To::Addr(Addr { host: 0, ..target }.to_u64()), reference: id };
    let asked = hand(&router, watcher.addr, &head, None);
    /* The router's queue full, or a watcher that closed meanwhile -- after
     * its close took its remote monitors, before this one was there --
     * takes the record back */
    if asked.is_err() || watcher.inner.lock().closed {
        let rec = {
            let mut t = table.lock();
            t.remote_monitors.iter().position(|r| r.id == id).map(|i| t.remote_monitors.swap_remove(i))
        };
        drop(rec);
        undo(watcher);
        return Err(asked.err().unwrap_or(Error::Gone));
    }
    Ok(id)
}

/// A remote monitor dropped: its record and place given back, and the
/// router asked to have the peer stop watching -- as it can: a peer that
/// is not told says a Down nobody holds, which is dropped.
pub(crate) fn demonitor(watcher: &Arc<Mbox>, id: u64, target: Addr) {
    let Some(table) = table() else { return };
    let (rec, router) = {
        let mut t = table.lock();
        let rec = t.remote_monitors.iter().position(|r| r.id == id && Arc::ptr_eq(&r.watcher, watcher))
            .map(|i| t.remote_monitors.swap_remove(i));
        (rec, t.router.clone())
    };
    let Some(rec) = rec else { return };
    {
        let mut w = watcher.inner.lock();
        w.reserved = w.reserved.saturating_sub(1);
    }
    drop(rec);
    if let Some(router) = router {
        let head = route::Head { kind: route::DEMONITOR, peer: target.host, tag: 0,
                                 to: ipcwire::To::Addr(Addr { host: 0, ..target }.to_u64()), reference: id };
        let _ = hand(&router, watcher.addr, &head, None);
    }
}

/// A closing mailbox's remote monitors, taken from the table with it held,
/// and its address forgotten by every peer it had sent to.
pub(crate) fn forget(t: &mut Table, mbox: &Arc<Mbox>) -> Vec<RemoteMonitor> {
    for p in t.peers.iter_mut().flatten() {
        p.sent.retain(|&a| a != mbox.addr);
    }
    let mut gone = Vec::new();
    let mut k = 0;
    while k < t.remote_monitors.len() {
        if Arc::ptr_eq(&t.remote_monitors[k].watcher, mbox) {
            if gone.try_reserve(1).is_err() {
                break;
            }
            gone.push(t.remote_monitors.swap_remove(k));
        } else {
            k += 1;
        }
    }
    gone
}

/// What a closing mailbox's remote monitors leave: the router asked to have
/// their peers stop watching, as it can. The table let go.
pub(crate) fn unwatch_all(router: Option<Arc<Mbox>>, from: Addr, gone: Vec<RemoteMonitor>) {
    let Some(router) = router else { return };
    for rec in gone {
        let head = route::Head { kind: route::DEMONITOR, peer: rec.peer, tag: 0,
                                 to: ipcwire::To::Addr(Addr { host: 0, ..rec.down.from }.to_u64()),
                                 reference: rec.id };
        let _ = hand(&router, from, &head, None);
    }
}

/// The peer `host` says the mailbox the monitor `id` of this host's watched
/// has gone: its Down delivered.
pub(crate) fn remote_down(router: &Arc<Mbox>, host: u16, id: u64, reason: Reason) -> Result<(), Error> {
    let table = table().ok_or(Error::NoMemory)?;
    let rec = {
        let mut t = table.lock();
        if !is_router(&t, router) {
            return Err(Error::NotYours);
        }
        let i = t.remote_monitors.iter().position(|r| r.id == id && r.peer == host).ok_or(Error::Gone)?;
        t.remote_monitors.swap_remove(i)
    };
    deliver(rec, reason);
    Ok(())
}

/// The peers, for `ipc peers`: number, name, whether up, how many may be
/// answered.
pub fn peers() -> Vec<(u16, String, bool, usize)> {
    let Some(table) = table() else { return Vec::new() };
    let t = table.lock();
    let mut out = Vec::new();
    for (i, p) in t.peers.iter().enumerate() {
        let Some(p) = p else { continue };
        let Ok(name) = copy(&p.name) else { break };
        if out.try_reserve(1).is_err() {
            break;
        }
        out.push((number(i, p.epoch), name, p.up, p.sent.len()));
    }
    out
}

/// The router's address, if there is one.
pub fn router() -> Option<Addr> {
    table()?.lock().router.as_ref().map(|r| r.addr)
}
