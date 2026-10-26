//! NAT: the machines behind one device of this stack -- the hypervisor's
//! guests, behind `hv0` -- reach the world through another, from that
//! device's address, as a home router masquerades its LAN.
//!
//! A guest's packet for somewhere off its subnet comes to the stack on the
//! inner device, sent to its gateway. Before the protocols see it,
//! [`intercept`] gives its flow a mapping -- an external port of this
//! machine's, `PORT_BASE` up -- rewrites its source to the outer device's
//! address and that port, and sends it out of the outer device to the next
//! hop there. What the outer device receives for one of those ports, from
//! the host and port the flow went to, is rewritten back and sent to the
//! guest; so is an ICMP error that quotes one of the flow's packets. What
//! comes for a port no mapping has is the stack's own, as before -- a port
//! `hv forward` listens on, say -- and so is whatever a guest sends to an
//! address of this machine's.
//!
//! What it takes: TCP from a SYN on (a segment of a flow it does not know is
//! dropped, not mapped), UDP from any datagram, ICMP echo requests; not
//! fragments, and nothing else. An answer is let in only from the address
//! and port its flow went to (RFC 4787's "address and port-dependent
//! filtering"). A mapping lasts as long as its flow is seen either way, and
//! RFC 5382's and 4787's times after that: two hours for a TCP connection
//! that has been answered, four minutes before that and after a FIN, ten
//! seconds after a reset; five minutes for UDP, one for ICMP.
//!
//! The table is taken whole when NAT goes on, and nothing on the way
//! allocates but the frame a packet goes on in: `ENTRIES` mappings, an
//! external port each, and a hash of the guests' side of them. Two devices'
//! receive paths use it and the shell looks at it, so it is under a lock --
//! held for the lookup, never for the frame, which is built and sent with the
//! lock down.
//!
//! Where a guest may go is the world past this machine's edge, not what is
//! behind it: a packet for a cloud's metadata server and link-local's rest
//! (169.254/16), a private network (10/8, 172.16/12, 192.168/16),
//! carrier-grade NAT's shared space (100.64/10) or the uplink's own subnet --
//! the host's LAN -- is kept from the world (`kept`), and its guest answered
//! with an ICMP error, so that it fails at once rather than waiting out its
//! retries. `nat allow` lets a network through; the DNS server the guests
//! are given is theirs to ask at port 53, wherever it is.

use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use kcore::cmd::Output;
use kcore::consts::NS_PER_SEC;
use kcore::sync::SpinLock;
use kcore::time;
use kcore::trace;

use crate::device::{Device, DEVICES};
use crate::frame::Frame;
use crate::wire::{self, eth, icmp, ip, tcp, udp, Ipv4, Mac, ETH_HDR_LEN, ETH_TYPE_IP,
                  ICMP_HDR_LEN, IP_HDR_LEN, IP_PROTO_ICMP, IP_PROTO_TCP, IP_PROTO_UDP,
                  UDP_HDR_LEN};

/// Mappings at once, and the external ports they take: one each, the same
/// numbers for TCP, UDP and ICMP echo identifiers. Below the TCP stack's own
/// ephemeral ports, and above the ports this machine's services listen on.
pub const ENTRIES: usize = 4096;
pub const PORT_BASE: u16 = 32768;
pub const PORT_LAST: u16 = PORT_BASE + (ENTRIES - 1) as u16;
const BUCKETS: usize = 4096;
/// No entry: the end of a hash chain, of the free list, or of a host's list.
const NONE: u16 = u16::MAX;
/// The inner hosts that hold mappings at once: the guests -- the switch has
/// sixteen ports -- with room to spare. A packet of one past them has no
/// mapping made for it, and is counted as `full`.
const HOSTS: usize = 64;

const _: () = assert!(ENTRIES < NONE as usize && BUCKETS.is_power_of_two());
/* A host's slot is an entry's byte; its count, a u16, holds the whole table. */
const _: () = assert!(HOSTS <= u8::MAX as usize + 1 && ENTRIES <= u16::MAX as usize);
const _: () = assert!(PORT_LAST < crate::tcp::EPHEMERAL_BASE);
/* One offset for the ports of both transports. */
const _: () = assert!(tcp::SRC_PORT == udp::SRC_PORT && tcp::DST_PORT == udp::DST_PORT);

/// How long a mapping outlives the last packet of its flow.
const TCP_ESTABLISHED_NS: u64 = 7440 * NS_PER_SEC;
const TCP_TRANSITORY_NS: u64 = 240 * NS_PER_SEC;
const TCP_RESET_NS: u64 = 10 * NS_PER_SEC;
const UDP_NS: u64 = 300 * NS_PER_SEC;
const ICMP_NS: u64 = 60 * NS_PER_SEC;

/* What a TCP mapping has seen of its connection. */
const SEEN_ANSWER: u8 = 1 << 0;
const SEEN_FIN: u8 = 1 << 1;
const SEEN_RST: u8 = 1 << 2;

/// The start of a transport header an ICMP error quotes at the least --
/// both ports, or an echo's identifier -- and all NAT reads of it.
const QUOTED_L4: usize = 8;

/// The most mappings `nat` lists.
const SHOWN: usize = 32;

#[derive(Clone, Copy)]
struct Entry {
    /// The protocol; 0 for a free entry.
    proto: u8,
    /// What a TCP entry has seen: `SEEN_*`.
    seen: u8,
    inner_ip: u32,
    /// The guest's port, or its echo identifier.
    inner_port: u16,
    /// Where the guest is on the inner device.
    inner_mac: Mac,
    remote_ip: u32,
    /// The far end's port; 0 for an echo.
    remote_port: u16,
    /// When it runs out: boot time, in ns.
    expires: u64,
    /// The next entry in its hash chain -- or, free, on the free list.
    next: u16,
    /// Its host's slot (`Table::hosts`), and its neighbours on that host's
    /// list, the one used less lately and the one used more.
    host: u8,
    older: u16,
    newer: u16,
}

const FREE: Entry = Entry {
    proto: 0, seen: 0, inner_ip: 0, inner_port: 0, inner_mac: [0; 6], remote_ip: 0,
    remote_port: 0, expires: 0, next: NONE, host: 0, older: NONE, newer: NONE,
};

/// An inner host's share of the table: its mappings, listed from the one
/// used least lately to the one used last, and how many -- a slot with none
/// is free.
#[derive(Clone, Copy)]
struct Host {
    ip: u32,
    count: u16,
    lru: u16,
    mru: u16,
}

const NO_HOST: Host = Host { ip: 0, count: 0, lru: NONE, mru: NONE };

impl Entry {
    fn lifetime(&self) -> u64 {
        match self.proto {
            IP_PROTO_TCP if self.seen & SEEN_RST != 0 => TCP_RESET_NS,
            IP_PROTO_TCP if self.seen & SEEN_FIN != 0 => TCP_TRANSITORY_NS,
            IP_PROTO_TCP if self.seen & SEEN_ANSWER != 0 => TCP_ESTABLISHED_NS,
            IP_PROTO_TCP => TCP_TRANSITORY_NS,
            IP_PROTO_UDP => UDP_NS,
            _ => ICMP_NS,
        }
    }
}

/// The guests' side of a flow and where it goes: what a mapping is found by
/// on the way out.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    proto: u8,
    inner_ip: u32,
    inner_port: u16,
    remote_ip: u32,
    remote_port: u16,
}

impl Key {
    fn of(e: &Entry) -> Key {
        Key { proto: e.proto, inner_ip: e.inner_ip, inner_port: e.inner_port,
              remote_ip: e.remote_ip, remote_port: e.remote_port }
    }

    fn bucket(&self) -> usize {
        let mut h = u64::from(self.proto);
        for v in [u64::from(self.inner_ip), u64::from(self.inner_port),
                  u64::from(self.remote_ip), u64::from(self.remote_port)] {
            h = (h ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        (h >> 32) as usize & (BUCKETS - 1)
    }
}

/// The mappings: entry `i` is external port `PORT_BASE + i`. And each inner
/// host's share of them: when every one is live and another flow wants one,
/// the host that holds the most gives up the one it used least lately -- so
/// a guest that opens flows as fast as it can takes them from itself, not
/// from the rest, as a table that refused every new flow once full let it.
struct Table {
    entries: Vec<Entry>,
    buckets: Vec<u16>,
    hosts: [Host; HOSTS],
    /// The first free entry.
    free: u16,
    active: usize,
    /// With none free: when the first mapping there is can run out, and so
    /// the soonest a sweep can find one. A table full of live flows is not
    /// swept for every packet that wants a mapping.
    sweep_at: u64,
}

/// A mapping made: its entry, and whether a live flow's was taken for it.
struct Made {
    at: usize,
    taken: bool,
}

impl Table {
    fn new() -> Option<Table> {
        let mut entries = Vec::new();
        entries.try_reserve_exact(ENTRIES).ok()?;
        entries.resize(ENTRIES, FREE);
        for (i, e) in entries.iter_mut().enumerate() {
            e.next = if i + 1 < ENTRIES { (i + 1) as u16 } else { NONE };
        }
        let mut buckets = Vec::new();
        buckets.try_reserve_exact(BUCKETS).ok()?;
        buckets.resize(BUCKETS, NONE);
        Some(Table { entries, buckets, hosts: [NO_HOST; HOSTS], free: 0, active: 0, sweep_at: 0 })
    }

    /// The live mapping for `key`. One that has run out is let go here.
    fn find(&mut self, key: &Key, now: u64) -> Option<usize> {
        let mut at = self.buckets[key.bucket()];
        while at != NONE {
            let i = usize::from(at);
            if Key::of(&self.entries[i]) == *key {
                if self.entries[i].expires <= now {
                    self.release(i);
                    return None;
                }
                return Some(i);
            }
            at = self.entries[i].next;
        }
        None
    }

    /// The slot of the host at `ip`, if it holds a mapping.
    fn host(&self, ip: u32) -> Option<usize> {
        self.hosts.iter().position(|h| h.count != 0 && h.ip == ip)
    }

    /// Entry `i` onto the end of host `h`'s list: the one used last.
    fn link(&mut self, i: usize, h: usize) {
        let mru = self.hosts[h].mru;
        let e = &mut self.entries[i];
        e.host = h as u8;
        e.older = mru;
        e.newer = NONE;
        if mru == NONE {
            self.hosts[h].lru = i as u16;
        } else {
            self.entries[usize::from(mru)].newer = i as u16;
        }
        self.hosts[h].mru = i as u16;
        self.hosts[h].count += 1;
    }

    /// Entry `i` off its host's list; a host left with none frees its slot.
    fn unlink(&mut self, i: usize) {
        let e = self.entries[i];
        let h = usize::from(e.host);
        if e.older == NONE {
            self.hosts[h].lru = e.newer;
        } else {
            self.entries[usize::from(e.older)].newer = e.newer;
        }
        if e.newer == NONE {
            self.hosts[h].mru = e.older;
        } else {
            self.entries[usize::from(e.newer)].older = e.older;
        }
        self.hosts[h].count -= 1;
    }

    /// Entry `i` to run out at `when` -- and the next sweep no later: a flow
    /// made or kept after the last one may run out before what it found, a
    /// UDP flow's five minutes among TCP's two hours, and a full table must
    /// not count it live past then.
    fn expire_at(&mut self, i: usize, when: u64) {
        self.entries[i].expires = when;
        self.sweep_at = self.sweep_at.min(when);
    }

    /// Entry `i` used by a packet of its flow, either way: the last on its
    /// host's list.
    fn touch(&mut self, i: usize) {
        let h = usize::from(self.entries[i].host);
        if usize::from(self.hosts[h].mru) != i {
            self.unlink(i);
            self.link(i, h);
        }
    }

    /// Entry `i` out of its chain and its host's list, and onto the free
    /// list.
    fn release(&mut self, i: usize) {
        let bucket = Key::of(&self.entries[i]).bucket();
        let mut at = self.buckets[bucket];
        let mut prev = NONE;
        while at != NONE && usize::from(at) != i {
            prev = at;
            at = self.entries[usize::from(at)].next;
        }
        /* A live entry is in its chain: nothing else unlinks one. */
        if at != NONE {
            let next = self.entries[i].next;
            if prev == NONE {
                self.buckets[bucket] = next;
            } else {
                self.entries[usize::from(prev)].next = next;
            }
        }
        self.unlink(i);
        self.entries[i] = Entry { next: self.free, ..FREE };
        self.free = i as u16;
        self.active -= 1;
    }

    /// Every mapping of `inner_ip`'s freed, live or not, and how many there
    /// were: its guest is gone, and the address goes to the next guest on
    /// its port, which must not be handed the answers to the flows of the
    /// one before -- a TCP connection's sequence numbers among them.
    fn forget(&mut self, inner_ip: u32) -> usize {
        let Some(h) = self.host(inner_ip) else { return 0 };
        let mut n = 0;
        while self.hosts[h].count != 0 {
            self.release(usize::from(self.hosts[h].lru));
            n += 1;
        }
        n
    }

    /// Every mapping that has run out, freed; and when the first of the rest
    /// will.
    fn sweep(&mut self, now: u64) {
        let mut soonest = u64::MAX;
        for i in 0..ENTRIES {
            let e = &self.entries[i];
            if e.proto == 0 {
                continue;
            }
            if e.expires <= now {
                self.release(i);
            } else {
                soonest = soonest.min(e.expires);
            }
        }
        self.sweep_at = soonest;
    }

    /// A new mapping for `key`, for its caller to give a time: a free entry;
    /// none free, one whose flow has run out; every one live, the one used
    /// least lately of the host that holds the most -- the asker's own when
    /// it holds as many as any, and of several holding the most the lowest
    /// address's. None for a host past the `HOSTS` that hold mappings.
    fn insert(&mut self, key: &Key, mac: Mac, now: u64) -> Option<Made> {
        let no_slot = |t: &Table| t.host(key.inner_ip).is_none() && !t.hosts.iter().any(|h| h.count == 0);
        /* What has run out first: an entry for the flow, and a slot for its
         * host, if every one holds only flows gone quiet. */
        if (self.free == NONE || no_slot(self)) && now >= self.sweep_at {
            self.sweep(now);
        }
        if no_slot(self) {
            return None;
        }
        let taken = self.free == NONE;
        if taken {
            let mine = self.host(key.inner_ip).map_or(0, |h| self.hosts[h].count);
            let most = self.hosts.iter().map(|h| h.count).max().unwrap_or(0);
            let victim = if mine >= most {
                self.host(key.inner_ip)
            } else {
                self.hosts.iter().enumerate().filter(|(_, h)| h.count == most)
                    .min_by_key(|(_, h)| h.ip).map(|(j, _)| j)
            }?;
            self.release(usize::from(self.hosts[victim].lru));
        }
        let h = match self.host(key.inner_ip) {
            Some(h) => h,
            None => {
                let h = self.hosts.iter().position(|h| h.count == 0)?;
                self.hosts[h] = Host { ip: key.inner_ip, ..NO_HOST };
                h
            }
        };
        let i = usize::from(self.free);
        self.free = self.entries[i].next;
        let bucket = key.bucket();
        self.entries[i] = Entry {
            proto: key.proto, seen: 0, inner_ip: key.inner_ip, inner_port: key.inner_port,
            inner_mac: mac, remote_ip: key.remote_ip, remote_port: key.remote_port,
            expires: now, next: self.buckets[bucket], ..FREE
        };
        self.buckets[bucket] = i as u16;
        self.active += 1;
        self.link(i, h);
        Some(Made { at: i, taken })
    }
}

/// NAT while it is on: the two devices, and the table.
struct On {
    inner: &'static Device,
    outer: &'static Device,
    table: Table,
}

/// NAT's one instance (`abi::nat`).
pub struct Nat {
    on: SpinLock<Option<On>>,
    /// The networks `nat allow` let through the deny list -- kept while NAT
    /// goes off and on, as the host's word on where its guests may go. Its
    /// room is all taken up front: the receive path reads it, and an allow
    /// allocates nothing.
    allowed: SpinLock<Allowed>,
}

/* The two devices while NAT is on, for the receive path's one look at every
 * packet -- `Device::handle`s, 0 while it is off. Stored under the lock, with
 * the table they go with. */
static INNER: AtomicUsize = AtomicUsize::new(0);
static OUTER: AtomicUsize = AtomicUsize::new(0);

/// What NAT has done since the kernel started.
pub struct Counters {
    /// Packets sent on to the world, and back to a guest.
    pub out: AtomicUsize,
    pub back: AtomicUsize,
    /// Mappings made.
    pub mapped: AtomicUsize,
    /// Packets that wanted a mapping and could not have one: from a host
    /// past the `HOSTS` that hold mappings.
    pub full: AtomicUsize,
    /// Mappings made by taking a live flow's, every one being live: from
    /// the host that holds the most.
    pub taken: AtomicUsize,
    /// Packets whose next hop's address ARP did not have yet.
    pub no_hop: AtomicUsize,
    /// Packets there was no frame for, or no room on the device.
    pub no_frame: AtomicUsize,
    /// Packets NAT does not take: another protocol, a fragment, no time to
    /// live left, a TCP segment of no flow it knows.
    pub refused: AtomicUsize,
    /// Packets for where the guests are kept from (`kept`), each answered
    /// with an ICMP error.
    pub denied: AtomicUsize,
}

pub static COUNTERS: Counters = Counters {
    out: AtomicUsize::new(0),
    back: AtomicUsize::new(0),
    mapped: AtomicUsize::new(0),
    full: AtomicUsize::new(0),
    taken: AtomicUsize::new(0),
    no_hop: AtomicUsize::new(0),
    no_frame: AtomicUsize::new(0),
    refused: AtomicUsize::new(0),
    denied: AtomicUsize::new(0),
};

fn count(counter: &AtomicUsize) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Why NAT would not go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatError {
    /// No device the traffic could go out through: none has a gateway.
    NoUplink,
    /// The two are one device, or one of them has no address.
    Devices,
    /// It is on already.
    Busy,
    NoMemory,
}

/// One mapping, as the shell shows it.
#[derive(Clone, Copy)]
pub struct Mapping {
    pub proto: u8,
    pub inner_ip: u32,
    pub inner_port: u16,
    pub remote_ip: u32,
    pub remote_port: u16,
    pub port: u16,
    /// Nanoseconds before it runs out.
    pub left_ns: u64,
}

const NO_MAPPING: Mapping = Mapping {
    proto: 0, inner_ip: 0, inner_port: 0, remote_ip: 0, remote_port: 0, port: 0, left_ns: 0,
};

impl Nat {
    pub fn new() -> Option<Nat> {
        let allowed = Allowed { nets: [(0, 0); ALLOWED_MAX], count: 0 };
        Some(Nat { on: SpinLock::new(None)?, allowed: SpinLock::new(allowed)? })
    }

    /// Let the guests reach the network `net`/`mask` though the deny list
    /// keeps them from it: false when `ALLOWED_MAX` are let through already.
    /// One let through already is there once.
    pub fn allow(&self, net: u32, mask: u32) -> bool {
        let net = (net & mask, mask);
        let mut a = self.allowed.lock();
        if a.nets[..a.count].contains(&net) {
            return true;
        }
        if a.count == ALLOWED_MAX {
            return false;
        }
        let n = a.count;
        a.nets[n] = net;
        a.count = n + 1;
        true
    }

    /// Take `net`/`mask` back off what `allow` let through: whether it was
    /// there.
    pub fn disallow(&self, net: u32, mask: u32) -> bool {
        let net = (net & mask, mask);
        let mut a = self.allowed.lock();
        let n = a.count;
        let Some(i) = a.nets[..n].iter().position(|x| *x == net) else { return false };
        a.nets[i] = a.nets[n - 1];
        a.count = n - 1;
        true
    }

    /// What `allow` let through, into `out`: how many.
    pub fn allowed(&self, out: &mut [(u32, u32); ALLOWED_MAX]) -> usize {
        let a = self.allowed.lock();
        out[..a.count].copy_from_slice(&a.nets[..a.count]);
        a.count
    }

    /// Whether a guest's packet of `proto` for `dst`, at `port`, is kept
    /// from the world through `outer` (`kept`).
    fn keeps(&self, outer: &Device, proto: u8, dst: u32, port: u16) -> bool {
        let a = self.allowed.lock();
        kept(proto, dst, port, (outer.ip(), outer.mask()), &a.nets[..a.count], crate::dns::upstream())
    }

    /// NAT on, from `inner` out through `outer` -- or, with none named,
    /// through the device the default route is on: that device.
    pub fn enable(&self, inner: &'static Device, outer: Option<&'static Device>)
        -> Result<&'static Device, NatError>
    {
        let outer = match outer {
            Some(o) => o,
            None => DEVICES.uplink(inner).ok_or(NatError::NoUplink)?,
        };
        if core::ptr::eq(inner, outer) || inner.ip() == 0 || outer.ip() == 0 {
            return Err(NatError::Devices);
        }
        if self.on.lock().is_some() {
            return Err(NatError::Busy);
        }
        /* Made with the lock down: it allocates. */
        let table = Table::new().ok_or(NatError::NoMemory)?;
        let mut on = self.on.lock();
        if on.is_some() {
            drop(on);
            drop(table);
            return Err(NatError::Busy);
        }
        *on = Some(On { inner, outer, table });
        OUTER.store(outer.handle(), Ordering::Release);
        INNER.store(inner.handle(), Ordering::Release);
        drop(on);
        trace!(0, "nat: on, {} out through {} ({})", name(inner), name(outer), Ipv4(outer.ip()));
        /* Where the guests' first packets go -- the gateway, and the DNS
         * server they are given when it is on the outer device's own subnet
         * -- asked for now if it has to be, rather than by those packets,
         * which would be dropped while it is. */
        if let Some(arp) = crate::abi::arp_table() {
            let dns = crate::dns::upstream();
            let dns_hop = if dns != 0 && on_subnet(outer, dns) { dns } else { 0 };
            for hop in [outer.gw(), dns_hop] {
                if hop != 0 && arp.lookup(hop).is_none() {
                    arp.ask(&outer.as_nic(), hop);
                }
            }
        }
        Ok(outer)
    }

    /// NAT off, if it is on for `inner`: whether it was. The table goes with
    /// the lock down.
    pub fn disable(&self, inner: &'static Device) -> bool {
        let old = {
            let mut on = self.on.lock();
            match on.as_ref() {
                Some(o) if core::ptr::eq(o.inner, inner) => {
                    INNER.store(0, Ordering::Release);
                    OUTER.store(0, Ordering::Release);
                    on.take()
                }
                _ => None,
            }
        };
        let was = old.is_some();
        drop(old);
        if was {
            trace!(0, "nat: off");
        }
        was
    }

    /// Every mapping of the guest at `inner_ip` behind `inner` forgotten,
    /// if NAT is on for `inner`: how many there were. A pass over the whole
    /// table under its lock -- for a guest that has gone, not for a packet.
    pub fn forget(&self, inner: &'static Device, inner_ip: u32) -> Option<usize> {
        let mut on = self.on.lock();
        let o = on.as_mut()?;
        if !core::ptr::eq(o.inner, inner) {
            return None;
        }
        Some(o.table.forget(inner_ip))
    }

    /// While it is on: the two devices and how many mappings there are; and
    /// the first of the live ones, into `out`, with how many went there.
    pub fn state(&self, out: &mut [Mapping])
        -> Option<(&'static Device, &'static Device, usize, usize)>
    {
        let now = time::boot_time_ns();
        let on = self.on.lock();
        let o = on.as_ref()?;
        let mut shown = 0;
        for (i, e) in o.table.entries.iter().enumerate() {
            if shown == out.len() {
                break;
            }
            if e.proto == 0 || e.expires <= now {
                continue;
            }
            out[shown] = Mapping {
                proto: e.proto, inner_ip: e.inner_ip, inner_port: e.inner_port,
                remote_ip: e.remote_ip, remote_port: e.remote_port,
                port: PORT_BASE + i as u16, left_ns: e.expires - now,
            };
            shown += 1;
        }
        Some((o.inner, o.outer, o.table.active, shown))
    }
}

fn name(dev: &Device) -> &str {
    core::str::from_utf8(dev.name()).unwrap_or("?")
}

/* ---- where a guest may go ---- */

/// The networks a guest is kept from unless `nat allow` lets it through, as
/// (network, mask): link-local, where a cloud's metadata server answers --
/// 169.254.169.254, the host VM's credentials behind it -- the private
/// networks, where the host's own LAN and its neighbours are, and
/// carrier-grade NAT's shared space, where some clouds keep theirs. The
/// uplink's own subnet is kept too, whatever it is (`kept`).
const DENIED: [(u32, u32); 5] = [
    (0xA9FE_0000, 0xFFFF_0000), /* 169.254/16 */
    (0x0A00_0000, 0xFF00_0000), /* 10/8 */
    (0xAC10_0000, 0xFFF0_0000), /* 172.16/12 */
    (0xC0A8_0000, 0xFFFF_0000), /* 192.168/16 */
    (0x6440_0000, 0xFFC0_0000), /* 100.64/10 */
];
/// Where the DNS server the guests are given is theirs to ask, wherever it
/// is: on QEMU's user network 10.0.2.3, on a cloud its metadata server.
const DNS_PORT: u16 = 53;
/// The most networks `nat allow` lets through at once.
pub const ALLOWED_MAX: usize = 16;

/// The networks `nat allow` let through, as (network, mask).
struct Allowed {
    nets: [(u32, u32); ALLOWED_MAX],
    count: usize,
}

/// Whether a guest's packet of `proto` for `dst`, at TCP's or UDP's port
/// `port`, is kept from the world: for a denied network, or the uplink's own
/// subnet (`uplink`, as network and mask; a mask of 0 for none), unless one
/// of `allowed` takes it in -- or it is for `dns`, the DNS server the guests
/// are given, at its port. Pure, for the boot self-test.
fn kept(proto: u8, dst: u32, port: u16, uplink: (u32, u32), allowed: &[(u32, u32)], dns: u32) -> bool {
    if dns != 0 && dst == dns && port == DNS_PORT && (proto == IP_PROTO_UDP || proto == IP_PROTO_TCP) {
        return false;
    }
    let (net, mask) = uplink;
    let denied = DENIED.iter().any(|&(n, m)| dst & m == n) || (mask != 0 && dst & mask == net & mask);
    denied && !allowed.iter().any(|&(n, m)| dst & m == n)
}

/// A network a person wrote -- `a.b.c.d`, or `a.b.c.d/n` -- as (network,
/// mask), the address's host bits cleared.
fn parse_net(text: &str) -> Option<(u32, u32)> {
    const BITS: u32 = 32;
    let (addr, bits) = match text.split_once('/') {
        Some((a, b)) => (a, b.parse::<u32>().ok().filter(|&b| b <= BITS)?),
        None => (text, BITS),
    };
    let ip = wire::parse_ipv4(addr.as_bytes())?;
    let mask = if bits == 0 { 0 } else { u32::MAX << (BITS - bits) };
    Some((ip & mask, mask))
}

/// A network as `a.b.c.d/n`.
struct Net((u32, u32));

impl core::fmt::Display for Net {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (net, mask) = self.0;
        write!(f, "{}/{}", Ipv4(net), mask.count_ones())
    }
}

/// What the receive path of `dev` asks of every IPv4 frame before the
/// protocols see it: true when NAT has taken it -- sent it on, or dropped it
/// for good -- and the stack is not to look at it.
#[inline]
pub fn intercept(dev: &'static Device, frame: &[u8]) -> bool {
    let inner = INNER.load(Ordering::Acquire);
    if inner == 0 {
        return false;
    }
    let handle = dev.handle();
    if handle == inner {
        outbound(dev, frame)
    } else if handle == OUTER.load(Ordering::Acquire) {
        inbound(dev, frame)
    } else {
        false
    }
}

/// An IPv4 packet whose lengths hold -- the header's and the packet's, in
/// the frame: what NAT reads of it, and whether it can be rewritten at all.
struct Packet {
    ihl: usize,
    len: usize,
    src: u32,
    dst: u32,
    proto: u8,
    ttl: u8,
    /// Not a fragment, and its header's checksum right: a packet NAT can
    /// take. The header's checksum is summed again after the rewrite, which
    /// would hide a corruption.
    whole: bool,
}

fn parse(frame: &[u8]) -> Option<Packet> {
    if frame.len() < ETH_HDR_LEN + IP_HDR_LEN {
        return None;
    }
    let p = &frame[ETH_HDR_LEN..];
    let ihl = ip::header_len(p);
    let len = usize::from(ip::total_len(p));
    if ip::version(p) != 4 || ihl == 0 || len < ihl || ETH_HDR_LEN + len > frame.len() {
        return None;
    }
    Some(Packet { ihl, len, src: ip::src(p), dst: ip::dst(p), proto: ip::protocol(p),
                  ttl: p[ip::TTL], whole: !ip::is_fragment(p) && wire::checksum(&p[..ihl]) == 0 })
}

fn on_subnet(dev: &Device, addr: u32) -> bool {
    let mask = dev.mask();
    mask != 0 && addr & mask == dev.ip() & mask
}

/// Whether a packet for `dst` may go out into the world at all: not for
/// "this network" or loopback, not multicast, reserved or the broadcast,
/// and not the broadcast of the outer device's own subnet.
fn routable(outer: &Device, dst: u32) -> bool {
    const THIS_NET: u32 = 0;
    const LOOPBACK: u32 = 127;
    const MULTICAST: u32 = 0xE000_0000;
    let first = dst >> 24;
    let mask = outer.mask();
    first != THIS_NET && first != LOOPBACK && dst < MULTICAST
        && !(mask != 0 && on_subnet(outer, dst) && dst | mask == u32::MAX)
}

/// A guest's packet for the world: mapped, rewritten and sent out of the
/// outer device -- or, when it cannot be, dropped: it is not the stack's,
/// which would answer it as if it were the host it is for. False for what is
/// the stack's own -- whatever is not from a guest, or is for the guests'
/// subnet or for this machine.
fn outbound(inner: &'static Device, frame: &[u8]) -> bool {
    let Some(p) = parse(frame) else { return false };
    let Some(outer) = DEVICES.by_handle(OUTER.load(Ordering::Acquire)) else { return false };
    if !on_subnet(inner, p.src) || p.src == inner.ip() || on_subnet(inner, p.dst)
        || DEVICES.is_local(p.dst) || !routable(outer, p.dst)
    {
        return false;
    }
    if !p.whole {
        count(&COUNTERS.refused);
        return true;
    }
    let l4 = &frame[ETH_HDR_LEN + p.ihl..ETH_HDR_LEN + p.len];
    let (port, remote_port, flags) = match p.proto {
        IP_PROTO_TCP if l4.len() >= tcp::HDR_LEN => {
            (tcp::src_port(l4), tcp::dst_port(l4), tcp::flags(l4))
        }
        IP_PROTO_UDP if l4.len() >= UDP_HDR_LEN => (udp::src_port(l4), udp::dst_port(l4), 0),
        IP_PROTO_ICMP if l4.len() >= ICMP_HDR_LEN && icmp::kind(l4) == icmp::ECHO_REQUEST => {
            (icmp::id(l4), 0, 0)
        }
        _ => {
            count(&COUNTERS.refused);
            return true;
        }
    };
    let Some(nat) = crate::abi::nat() else { return false };
    /* Where the guests are kept from: answered, so that it fails now. */
    if nat.keeps(outer, p.proto, p.dst, remote_port) {
        count(&COUNTERS.denied);
        reject(inner, frame, &p);
        return true;
    }
    if p.ttl <= 1 {
        count(&COUNTERS.refused);
        return true;
    }
    let key = Key { proto: p.proto, inner_ip: p.src, inner_port: port, remote_ip: p.dst, remote_port };
    let mac = eth::src(frame);
    let now = time::boot_time_ns();
    let starts = p.proto == IP_PROTO_TCP && flags & (tcp::SYN | tcp::ACK_FLAG) == tcp::SYN;

    let (outer, external) = {
        let mut on = nat.on.lock();
        let Some(o) = on.as_mut() else { return false };
        let i = match o.table.find(&key, now) {
            Some(i) => i,
            /* A TCP flow is mapped from its SYN. A segment of one there is
             * no mapping for -- its time ran out -- goes nowhere. */
            None if p.proto == IP_PROTO_TCP && !starts => {
                count(&COUNTERS.refused);
                return true;
            }
            None => match o.table.insert(&key, mac, now) {
                Some(made) => {
                    count(&COUNTERS.mapped);
                    if made.taken {
                        count(&COUNTERS.taken);
                    }
                    made.at
                }
                None => {
                    count(&COUNTERS.full);
                    return true;
                }
            },
        };
        o.table.touch(i);
        let e = &mut o.table.entries[i];
        if p.proto == IP_PROTO_TCP {
            /* A connection starting again on the mapping the last one left:
             * its times are a new connection's. */
            if starts {
                e.seen = 0;
            }
            if flags & tcp::FIN != 0 {
                e.seen |= SEEN_FIN;
            }
            if flags & tcp::RST != 0 {
                e.seen |= SEEN_RST;
            }
        }
        e.inner_mac = mac;
        let when = now.saturating_add(e.lifetime());
        o.table.expire_at(i, when);
        (o.outer, PORT_BASE + i as u16)
    };

    /* The next hop's address from the cache, never waited for here: a miss
     * asks for it and drops this packet, which its sender sends again. */
    let Some(arp) = crate::abi::arp_table() else { return true };
    let Some(hop) = arp.lookup_or_ask(&outer.as_nic(), outer.route_ip(p.dst)) else {
        count(&COUNTERS.no_hop);
        return true;
    };
    let Some(mut out) = copy(&frame[..ETH_HDR_LEN + p.len]) else {
        count(&COUNTERS.no_frame);
        return true;
    };
    {
        let buf = out.data_mut();
        eth::write(buf, &hop, &outer.mac(), ETH_TYPE_IP);
        masquerade(&mut buf[ETH_HDR_LEN..], p.ihl, p.proto, outer.ip(), external);
    }
    count(if outer.as_nic().transmit(out) { &COUNTERS.out } else { &COUNTERS.no_frame });
    true
}

/// What an answer is to NAT: its protocol, the external port it came to and
/// where it came from -- or, for an ICMP error, the same of the packet it
/// quotes, and the length of that packet's header.
struct Answer {
    proto: u8,
    port: u16,
    far_ip: u32,
    far_port: u16,
    quoted_ihl: Option<usize>,
}

/// What an ICMP error says it is about, when that is a packet NAT sent out
/// of `outer`. The message's checksum is checked first: it is summed again
/// over what changes in it, which would hide a corruption.
fn quoted(outer: &Device, message: &[u8]) -> Option<Answer> {
    if wire::checksum(message) != 0 {
        return None;
    }
    let q = &message[ICMP_HDR_LEN..];
    let qihl = ip::header_len(q);
    let first = wire::be16(q, ip::FRAG_OFF) & ip::FRAG_OFFSET_MASK == 0;
    if ip::version(q) != 4 || qihl == 0 || q.len() < qihl + QUOTED_L4 || !first
        || ip::src(q) != outer.ip()
    {
        return None;
    }
    let l4 = &q[qihl..];
    let proto = ip::protocol(q);
    let (port, far_port) = match proto {
        IP_PROTO_TCP | IP_PROTO_UDP => (udp::src_port(l4), udp::dst_port(l4)),
        IP_PROTO_ICMP if icmp::kind(l4) == icmp::ECHO_REQUEST => (icmp::id(l4), 0),
        _ => return None,
    };
    Some(Answer { proto, port, far_ip: ip::dst(q), far_port, quoted_ihl: Some(qihl) })
}

/// An answer to one of the mapped ports, from where its flow went -- or an
/// ICMP error about one of the flow's packets: rewritten back and sent to
/// its guest. False for anything else, which is the stack's.
fn inbound(outer: &'static Device, frame: &[u8]) -> bool {
    let Some(p) = parse(frame) else { return false };
    if p.dst != outer.ip() || !p.whole {
        return false;
    }
    let l4 = &frame[ETH_HDR_LEN + p.ihl..ETH_HDR_LEN + p.len];
    let answer = match p.proto {
        IP_PROTO_TCP if l4.len() >= tcp::HDR_LEN => Answer {
            proto: p.proto, port: tcp::dst_port(l4), far_ip: p.src, far_port: tcp::src_port(l4),
            quoted_ihl: None,
        },
        IP_PROTO_UDP if l4.len() >= UDP_HDR_LEN => Answer {
            proto: p.proto, port: udp::dst_port(l4), far_ip: p.src, far_port: udp::src_port(l4),
            quoted_ihl: None,
        },
        IP_PROTO_ICMP if l4.len() >= ICMP_HDR_LEN && icmp::kind(l4) == icmp::ECHO_REPLY => Answer {
            proto: p.proto, port: icmp::id(l4), far_ip: p.src, far_port: 0, quoted_ihl: None,
        },
        IP_PROTO_ICMP if l4.len() >= ICMP_HDR_LEN + IP_HDR_LEN + QUOTED_L4
            && matches!(icmp::kind(l4), icmp::DEST_UNREACH | icmp::TIME_EXCEEDED) =>
        {
            match quoted(outer, l4) {
                Some(a) => a,
                None => return false,
            }
        }
        _ => return false,
    };
    if !(PORT_BASE..=PORT_LAST).contains(&answer.port) {
        return false;
    }
    let Some(nat) = crate::abi::nat() else { return false };
    let now = time::boot_time_ns();

    let (inner, inner_ip, inner_port, inner_mac) = {
        let mut on = nat.on.lock();
        let Some(o) = on.as_mut() else { return false };
        let at = usize::from(answer.port - PORT_BASE);
        let e = &mut o.table.entries[at];
        if e.proto != answer.proto || e.remote_ip != answer.far_ip
            || e.remote_port != answer.far_port || e.expires <= now
        {
            return false;
        }
        /* An error says nothing of the flow being alive, and a packet that
         * is dropped here no more. */
        let alive = answer.quoted_ihl.is_none() && p.ttl > 1;
        if alive {
            if e.proto == IP_PROTO_TCP {
                let flags = tcp::flags(l4);
                e.seen |= SEEN_ANSWER;
                if flags & tcp::FIN != 0 {
                    e.seen |= SEEN_FIN;
                }
                if flags & tcp::RST != 0 {
                    e.seen |= SEEN_RST;
                }
            }
        }
        let to = (o.inner, e.inner_ip, e.inner_port, e.inner_mac);
        if alive {
            let when = now.saturating_add(e.lifetime());
            o.table.expire_at(at, when);
            o.table.touch(at);
        }
        to
    };
    if p.ttl <= 1 {
        count(&COUNTERS.refused);
        return true;
    }

    let Some(mut out) = copy(&frame[..ETH_HDR_LEN + p.len]) else {
        count(&COUNTERS.no_frame);
        return true;
    };
    {
        let buf = out.data_mut();
        eth::write(buf, &inner_mac, &inner.mac(), ETH_TYPE_IP);
        let packet = &mut buf[ETH_HDR_LEN..];
        match answer.quoted_ihl {
            None => unmasquerade(packet, p.ihl, p.proto, inner_ip, inner_port),
            Some(qihl) => unmasquerade_error(packet, p.ihl, qihl, answer.proto, inner_ip, inner_port),
        }
    }
    count(if inner.as_nic().transmit(out) { &COUNTERS.back } else { &COUNTERS.no_frame });
    true
}

/// `bytes`, in a frame of the pool's own.
fn copy(bytes: &[u8]) -> Option<Frame> {
    let mut out = Frame::alloc_tx(bytes.len())?;
    out.fill(bytes).then_some(out)
}

/// What a guest is answered with for a packet to where it is kept from: a
/// destination unreachable, communication administratively prohibited
/// (RFC 1812 5.2.7.1), from the inner device's address -- its gateway's --
/// to its MAC, quoting the packet's header and the eight bytes after it
/// (RFC 792). Its connect then fails at once, "no route to host", rather
/// than wait out its retries. Nothing is sent when there is no frame for it;
/// and it never answers an ICMP error, NAT taking none of a guest's.
fn reject(inner: &'static Device, frame: &[u8], p: &Packet) {
    const ADMIN_PROHIBITED: u8 = 13;
    const MAX_IHL: usize = 60;
    let quoted = (p.ihl + QUOTED_L4).min(p.len);
    let icmp_len = ICMP_HDR_LEN + quoted;
    let total = ETH_HDR_LEN + IP_HDR_LEN + icmp_len;
    let mut buf = [0u8; ETH_HDR_LEN + IP_HDR_LEN + ICMP_HDR_LEN + MAX_IHL + QUOTED_L4];
    eth::write(&mut buf, &eth::src(frame), &inner.mac(), ETH_TYPE_IP);
    ip::write(&mut buf[ETH_HDR_LEN..], IP_PROTO_ICMP, inner.ip(), p.src, icmp_len, 0);
    let at = ETH_HDR_LEN + IP_HDR_LEN;
    buf[at + ICMP_HDR_LEN..total].copy_from_slice(&frame[ETH_HDR_LEN..ETH_HDR_LEN + quoted]);
    icmp::write(&mut buf[at..], icmp::DEST_UNREACH, ADMIN_PROHIBITED, 0, 0, icmp_len);
    if let Some(f) = copy(&buf[..total]) {
        let _ = inner.as_nic().transmit(f);
    }
}

/* ---- the rewriting: an IPv4 packet in place, its header first ---- */

/// A guest's packet as it goes out: from `src`:`port`, one hop on.
fn masquerade(packet: &mut [u8], ihl: usize, proto: u8, src: u32, port: u16) {
    rewrite_addr(packet, ip::SRC, src, proto, ihl);
    rewrite_port(packet, ihl, proto, udp::SRC_PORT, port);
    hop(packet, ihl);
}

/// An answer as it comes back in: to the guest's `dst`:`port`, one hop on.
fn unmasquerade(packet: &mut [u8], ihl: usize, proto: u8, dst: u32, port: u16) {
    rewrite_addr(packet, ip::DST, dst, proto, ihl);
    rewrite_port(packet, ihl, proto, udp::DST_PORT, port);
    hop(packet, ihl);
}

/// An ICMP error about a flow's packet as it comes back in: to the guest,
/// the packet it quotes made the one the guest sent -- its source, its port
/// or identifier, the checksums over them -- and the message's own checksum
/// summed again over all of it.
fn unmasquerade_error(packet: &mut [u8], ihl: usize, quoted_ihl: usize, quoted_proto: u8,
                      dst: u32, port: u16) {
    rewrite_addr(packet, ip::DST, dst, IP_PROTO_ICMP, ihl);
    let message = &mut packet[ihl..];
    {
        let quoted = &mut message[ICMP_HDR_LEN..];
        rewrite_addr(quoted, ip::SRC, dst, quoted_proto, quoted_ihl);
        rewrite_port(quoted, quoted_ihl, quoted_proto, udp::SRC_PORT, port);
        header_checksum(quoted, quoted_ihl);
    }
    wire::set_be16(message, icmp::CHECKSUM, 0);
    let sum = wire::checksum(message);
    wire::set_be16(message, icmp::CHECKSUM, sum);
    hop(packet, ihl);
}

/// A checksum updated for one 16-bit word of what it covers going from
/// `old` to `new`, rather than summed again (RFC 1624, equation 3): what
/// was wrong before is still wrong after.
fn adjust(check: u16, old: u16, new: u16) -> u16 {
    let mut sum = u32::from(!check) + u32::from(!old) + u32::from(new);
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Where the transport's checksum is when there is one to keep up: TCP's,
/// and UDP's unless it is 0 -- none -- which stays so. None when what is at
/// hand of the transport header does not reach it.
fn l4_checksum_at(packet: &[u8], ihl: usize, proto: u8) -> Option<usize> {
    let at = match proto {
        IP_PROTO_TCP => ihl + tcp::CHECKSUM,
        IP_PROTO_UDP => ihl + udp::CHECKSUM,
        _ => return None,
    };
    if packet.len() < at + 2 || (proto == IP_PROTO_UDP && wire::be16(packet, at) == 0) {
        return None;
    }
    Some(at)
}

fn set_l4_checksum(packet: &mut [u8], at: usize, proto: u8, sum: u16) {
    /* 0 in UDP's field is no checksum: the same number goes as 0xFFFF. */
    let sum = if proto == IP_PROTO_UDP && sum == 0 { 0xFFFF } else { sum };
    wire::set_be16(packet, at, sum);
}

/// The address at `at` -- the source or the destination -- made `new`, and
/// the transport's checksum, which covers it through the pseudo-header, kept
/// up. The header's own is summed again by `hop`.
fn rewrite_addr(packet: &mut [u8], at: usize, new: u32, proto: u8, ihl: usize) {
    let old = wire::be32(packet, at);
    wire::set_be32(packet, at, new);
    if let Some(c) = l4_checksum_at(packet, ihl, proto) {
        let mut sum = wire::be16(packet, c);
        sum = adjust(sum, (old >> 16) as u16, (new >> 16) as u16);
        sum = adjust(sum, old as u16, new as u16);
        set_l4_checksum(packet, c, proto, sum);
    }
}

/// The transport header's port at `which` (`udp::SRC_PORT` or `DST_PORT`)
/// -- or an echo's identifier, whichever way it goes -- made `new`, and the
/// checksum over it kept up.
fn rewrite_port(packet: &mut [u8], ihl: usize, proto: u8, which: usize, new: u16) {
    let (at, check) = match proto {
        IP_PROTO_ICMP => (ihl + icmp::ID, Some(ihl + icmp::CHECKSUM)),
        _ => (ihl + which, l4_checksum_at(packet, ihl, proto)),
    };
    let old = wire::be16(packet, at);
    wire::set_be16(packet, at, new);
    if let Some(c) = check {
        let sum = adjust(wire::be16(packet, c), old, new);
        set_l4_checksum(packet, c, proto, sum);
    }
}

fn header_checksum(packet: &mut [u8], ihl: usize) {
    wire::set_be16(packet, ip::CHECKSUM, 0);
    let sum = wire::checksum(&packet[..ihl]);
    wire::set_be16(packet, ip::CHECKSUM, sum);
}

/// One hop less to live, and the header's checksum summed again: the header
/// was checked on the way in.
fn hop(packet: &mut [u8], ihl: usize) {
    packet[ip::TTL] -= 1;
    header_checksum(packet, ihl);
}

/* ---- the C ABI a module binds (kcore::net::Nat) ---- */

/// What `kernel_net_nat_enable` answers in its `code`.
const NAT_ON: i32 = 0;
const NAT_NO_UPLINK: i32 = 1;
const NAT_DEVICES: i32 = 2;
const NAT_BUSY: i32 = 3;
const NAT_NO_MEMORY: i32 = 4;

/// NAT on, from the device `inner` names -- a `kernel_net_find` handle --
/// out through the device the default route is on. Task context: it
/// allocates the table.
#[no_mangle]
pub extern "C" fn kernel_net_nat_enable(inner: usize) -> ffi::net::NatOn {
    let refused = |code| ffi::net::NatOn { code, outer: 0 };
    let Some(dev) = DEVICES.by_handle(inner) else { return refused(NAT_DEVICES) };
    let Some(nat) = crate::abi::nat() else { return refused(NAT_NO_MEMORY) };
    match nat.enable(dev, None) {
        Ok(outer) => ffi::net::NatOn { code: NAT_ON, outer: outer.handle() },
        Err(NatError::NoUplink) => refused(NAT_NO_UPLINK),
        Err(NatError::Devices) => refused(NAT_DEVICES),
        Err(NatError::Busy) => refused(NAT_BUSY),
        Err(NatError::NoMemory) => refused(NAT_NO_MEMORY),
    }
}

/// NAT off, if it is on for the device `inner` names: 1 when it was.
#[no_mangle]
pub extern "C" fn kernel_net_nat_disable(inner: usize) -> i32 {
    match (DEVICES.by_handle(inner), crate::abi::nat()) {
        (Some(dev), Some(nat)) if nat.disable(dev) => 1,
        _ => 0,
    }
}

/// Every mapping of the guest at `ip` (host byte order) behind the device
/// `inner` names forgotten, if NAT is on for it: how many there were.
#[no_mangle]
pub extern "C" fn kernel_net_nat_forget(inner: usize, ip: u32) -> u32 {
    match (DEVICES.by_handle(inner), crate::abi::nat()) {
        (Some(dev), Some(nat)) => nat.forget(dev, ip).map_or(0, |n| n as u32),
        _ => 0,
    }
}

/* ---- the shell ---- */

/// `nat`: whether it is on and between which devices, where the guests are
/// kept from, what it has done, and the first of its mappings. `nat allow
/// <net>` lets the guests reach a network the deny list keeps them from --
/// `a.b.c.d`, or `a.b.c.d/n` -- and `nat deny <net>` takes that back.
pub fn shell(args: &str, out: &mut Output) {
    let mut words = args.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (None, _, _) => {}
        (Some(verb @ ("allow" | "deny")), Some(text), None) => {
            let Some(nat) = crate::abi::nat() else {
                let _ = writeln!(out, "nat: none");
                return;
            };
            let Some((net, mask)) = parse_net(text) else {
                let _ = writeln!(out, "nat: {} is no network -- a.b.c.d, or a.b.c.d/n", text);
                return;
            };
            let what = Net((net, mask));
            let _ = if verb == "allow" {
                if nat.allow(net, mask) {
                    writeln!(out, "nat: {} let through", what)
                } else {
                    writeln!(out, "nat: {} not let through -- {} networks are, the most there may be", what,
                             ALLOWED_MAX)
                }
            } else if nat.disallow(net, mask) {
                writeln!(out, "nat: {} kept from the guests again", what)
            } else {
                writeln!(out, "nat: {} was not let through", what)
            };
            return;
        }
        _ => {
            let _ = writeln!(out, "usage: nat [allow|deny <a.b.c.d[/n]>]");
            return;
        }
    }
    let mut shown = [NO_MAPPING; SHOWN];
    let state = crate::abi::nat().and_then(|nat| nat.state(&mut shown));
    match state {
        None => {
            let _ = writeln!(out, "nat: off");
        }
        Some((inner, outer, active, listed)) => {
            let _ = writeln!(out, "nat: on, {} ({}) out through {} ({}); {} of {} mappings, ports {}-{}",
                name(inner), Ipv4(inner.ip()), name(outer), Ipv4(outer.ip()), active, ENTRIES,
                PORT_BASE, PORT_LAST);
            for m in &shown[..listed] {
                let proto = match m.proto {
                    IP_PROTO_TCP => "tcp",
                    IP_PROTO_UDP => "udp",
                    _ => "icmp",
                };
                let _ = writeln!(out, "  {} {}:{} -> {}:{} as {}, {} s left", proto,
                    Ipv4(m.inner_ip), m.inner_port, Ipv4(m.remote_ip), m.remote_port, m.port,
                    m.left_ns / NS_PER_SEC);
            }
            if active > listed {
                let _ = writeln!(out, "  ...");
            }
        }
    }
    /* Where the guests are kept from, and what is let through of it. */
    let _ = write!(out, "kept from");
    for &net in &DENIED {
        let _ = write!(out, " {}", Net(net));
    }
    match state {
        Some((_, outer, _, _)) if outer.mask() != 0 => {
            let _ = write!(out, " and {}'s {}", name(outer), Net((outer.ip() & outer.mask(), outer.mask())));
        }
        _ => {
            let _ = write!(out, " and the uplink's subnet");
        }
    }
    let mut allowed = [(0, 0); ALLOWED_MAX];
    let n = crate::abi::nat().map_or(0, |nat| nat.allowed(&mut allowed));
    let _ = write!(out, "; let through:");
    if n == 0 {
        let _ = write!(out, " none");
    }
    for &net in &allowed[..n] {
        let _ = write!(out, " {}", Net(net));
    }
    let _ = writeln!(out);
    let c = &COUNTERS;
    let load = |counter: &AtomicUsize| counter.load(Ordering::Relaxed);
    let _ = writeln!(out, "out {}  back {}  mapped {}  taken from the most {}  no room {}  no next hop {}  \
                           no frame {}  refused {}  denied {}",
        load(&c.out), load(&c.back), load(&c.mapped), load(&c.taken), load(&c.full), load(&c.no_hop),
        load(&c.no_frame), load(&c.refused), load(&c.denied));
}

/* ---- what the boot self-test checks ---- */

/// The rewriting, against checksums summed from scratch: a packet of each
/// kind out and its answer back, an ICMP error about one, and the one sum
/// UDP cannot send as it is. And the table filled, run out, and filled
/// again.
pub fn selftest() -> bool {
    rewrites() & zero_sum() & table() & policy()
}

/// Where a guest may go: kept from a cloud's metadata server, the private
/// networks and the uplink's own subnet; let through to the world, to what
/// `nat allow` names, and to the DNS server at its port alone. And a
/// network as a person writes one.
fn policy() -> bool {
    const METADATA: u32 = 0xA9FE_A9FE; /* 169.254.169.254 */
    const PRIVATE: u32 = 0xAC1F_0001; /* 172.31.0.1 */
    const SHARED: u32 = 0x647F_FFFE; /* 100.127.255.254 */
    const UPLINK: (u32, u32) = (0x5F10_2000, 0xFFFF_FFC0); /* 95.16.32.0/26 */
    const NEIGHBOUR: u32 = 0x5F10_2011; /* 95.16.32.17 */
    const DNS: u32 = 0x0A00_0203; /* 10.0.2.3 */
    const PRIVATE_NET: (u32, u32) = (0xAC10_0000, 0xFFF0_0000); /* 172.16/12 */
    check("the metadata server kept", kept(IP_PROTO_TCP, METADATA, 80, UPLINK, &[], 0))
        & check("a private network kept", kept(IP_PROTO_UDP, PRIVATE, 1234, UPLINK, &[], 0))
        & check("the shared space kept", kept(IP_PROTO_ICMP, SHARED, 0, UPLINK, &[], 0))
        & check("the uplink's neighbour kept", kept(IP_PROTO_TCP, NEIGHBOUR, 22, UPLINK, &[], 0))
        & check("the world let through", !kept(IP_PROTO_TCP, REMOTE, 443, UPLINK, &[], 0))
        & check("an allowed network let through", !kept(IP_PROTO_ICMP, PRIVATE, 0, UPLINK, &[PRIVATE_NET], 0))
        & check("the DNS server at its port", !kept(IP_PROTO_UDP, DNS, DNS_PORT, UPLINK, &[], DNS))
        & check("the DNS server at another port kept", kept(IP_PROTO_TCP, DNS, 80, UPLINK, &[], DNS))
        & check("a network parsed", parse_net("10.0.2.2/24") == Some((0x0A00_0200, 0xFFFF_FF00))
                && parse_net("1.2.3.4") == Some((0x0102_0304, u32::MAX)) && parse_net("0.0.0.0/0") == Some((0, 0))
                && parse_net("1.2.3.4/33").is_none() && parse_net("1.2.3/8").is_none())
}

fn check(what: &str, ok: bool) -> bool {
    if !ok {
        trace!(0, "net selftest: nat: {} FAILED", what);
    }
    ok
}

const GUEST: u32 = 0x0A00_6402; /* 10.0.100.2 */
const OTHER: u32 = 0x0A00_6403; /* 10.0.100.3 */
const THIRD: u32 = 0x0A00_6404; /* 10.0.100.4 */
const OUTSIDE: u32 = 0x0A00_020F; /* 10.0.2.15 */
const REMOTE: u32 = 0x5DB8_D822; /* 93.184.216.34 */
const ROUTER: u32 = 0xC0A8_0001; /* 192.168.0.1 */
const TEST_PAYLOAD: usize = 23;

/// A packet of `proto` from `src`:`sport` to `dst`:`dport` -- an echo
/// request with `sport` its identifier -- with an odd-length payload and
/// every checksum right, but UDP's when `udp_sum` says none: its length.
fn packet(buf: &mut [u8], proto: u8, src: u32, sport: u16, dst: u32, dport: u16, udp_sum: bool) -> usize {
    let l4_len = TEST_PAYLOAD + match proto {
        IP_PROTO_TCP => tcp::HDR_LEN,
        IP_PROTO_UDP => UDP_HDR_LEN,
        _ => ICMP_HDR_LEN,
    };
    ip::write(buf, proto, src, dst, l4_len, 0x4242);
    let l4 = &mut buf[IP_HDR_LEN..IP_HDR_LEN + l4_len];
    for (i, b) in l4.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(37) ^ 0x5A;
    }
    match proto {
        IP_PROTO_TCP => {
            tcp::write(l4, sport, dport, 0x0102_0304, 0, tcp::HDR_LEN, tcp::SYN, 0xFFFF);
            let sum = tcp::checksum(src, dst, l4);
            wire::set_be16(l4, tcp::CHECKSUM, sum);
        }
        IP_PROTO_UDP => {
            udp::write(l4, sport, dport, TEST_PAYLOAD);
            if udp_sum {
                let sum = udp::checksum(src, dst, l4);
                wire::set_be16(l4, udp::CHECKSUM, sum);
            }
        }
        _ => icmp::write(l4, icmp::ECHO_REQUEST, 0, sport, 7, l4_len),
    }
    IP_HDR_LEN + l4_len
}

/// Whether every checksum in the packet holds, as a receiver checks them.
fn sums_hold(p: &[u8]) -> bool {
    let ihl = ip::header_len(p);
    let l4 = &p[ihl..usize::from(ip::total_len(p))];
    let l4_ok = match ip::protocol(p) {
        IP_PROTO_TCP => tcp::checksum(ip::src(p), ip::dst(p), l4) == 0,
        IP_PROTO_UDP => wire::be16(l4, udp::CHECKSUM) == 0
            || wire::transport_checksum(IP_PROTO_UDP, ip::src(p), ip::dst(p), l4) == 0,
        _ => wire::checksum(l4) == 0,
    };
    wire::checksum(&p[..ihl]) == 0 && l4_ok
}

fn rewrites() -> bool {
    const EXTERNAL: u16 = PORT_BASE + 5;
    let mut ok = true;
    for (proto, udp_sum, what) in [(IP_PROTO_TCP, true, "a tcp segment"),
                                   (IP_PROTO_UDP, true, "a udp datagram"),
                                   (IP_PROTO_UDP, false, "a udp datagram with no checksum"),
                                   (IP_PROTO_ICMP, true, "an echo")] {
        let mut buf = [0u8; 128];
        let len = packet(&mut buf, proto, GUEST, 40000, REMOTE, 80, udp_sum);
        let ttl = buf[ip::TTL];
        let p = &mut buf[..len];
        masquerade(p, IP_HDR_LEN, proto, OUTSIDE, EXTERNAL);
        let at = IP_HDR_LEN + if proto == IP_PROTO_ICMP { icmp::ID } else { udp::SRC_PORT };
        ok &= check(what, sums_hold(p) && ip::src(p) == OUTSIDE && ip::dst(p) == REMOTE
            && wire::be16(p, at) == EXTERNAL && p[ip::TTL] == ttl - 1
            && (udp_sum || wire::be16(p, IP_HDR_LEN + udp::CHECKSUM) == 0));

        /* Its answer, back to the guest */
        let mut back = [0u8; 128];
        let len = packet(&mut back, proto, REMOTE, 80, OUTSIDE, EXTERNAL, udp_sum);
        let b = &mut back[..len];
        unmasquerade(b, IP_HDR_LEN, proto, GUEST, 40000);
        let at = IP_HDR_LEN + if proto == IP_PROTO_ICMP { icmp::ID } else { udp::DST_PORT };
        ok &= check(what, sums_hold(b) && ip::src(b) == REMOTE && ip::dst(b) == GUEST
            && wire::be16(b, at) == 40000);
    }

    /* A router's error about a datagram that went out, quoted whole, as the
     * guest must see it: every checksum right, the quoted datagram's too. */
    let mut sent = [0u8; 128];
    let len = packet(&mut sent, IP_PROTO_UDP, GUEST, 40000, REMOTE, 53, true);
    masquerade(&mut sent[..len], IP_HDR_LEN, IP_PROTO_UDP, OUTSIDE, EXTERNAL);
    let msg_len = ICMP_HDR_LEN + len;
    let mut error = [0u8; 256];
    ip::write(&mut error, IP_PROTO_ICMP, ROUTER, OUTSIDE, msg_len, 1);
    error[IP_HDR_LEN + ICMP_HDR_LEN..IP_HDR_LEN + msg_len].copy_from_slice(&sent[..len]);
    icmp::write(&mut error[IP_HDR_LEN..], icmp::DEST_UNREACH, icmp::PORT_UNREACH, 0, 0, msg_len);
    let e = &mut error[..IP_HDR_LEN + msg_len];
    unmasquerade_error(e, IP_HDR_LEN, IP_HDR_LEN, IP_PROTO_UDP, GUEST, 40000);
    let q = &e[IP_HDR_LEN + ICMP_HDR_LEN..];
    ok &= check("an icmp error", ip::dst(e) == GUEST && sums_hold(e) && ip::src(q) == GUEST
        && wire::be16(q, IP_HDR_LEN + udp::SRC_PORT) == 40000 && sums_hold(q));
    ok
}

/// A datagram whose sum comes to 0 -- its checksum sent as 0xFFFF -- keeps
/// a checksum through a rewrite that brings it to 0 again: 0 would say it
/// has none.
fn zero_sum() -> bool {
    let mut u = [0u8; 128];
    let len = packet(&mut u, IP_PROTO_UDP, GUEST, 40000, REMOTE, 53, false);
    /* The first word of the payload made what brings the sum to 0 */
    let word = IP_HDR_LEN + UDP_HDR_LEN;
    wire::set_be16(&mut u, word, 0);
    let sum = udp::checksum(GUEST, REMOTE, &u[IP_HDR_LEN..len]);
    wire::set_be16(&mut u, word, sum);
    let zero = udp::checksum(GUEST, REMOTE, &u[IP_HDR_LEN..len]);
    wire::set_be16(&mut u, IP_HDR_LEN + udp::CHECKSUM, zero);
    let mut ok = check("a sum of 0 goes as 0xFFFF", zero == 0xFFFF && sums_hold(&u[..len]));
    rewrite_port(&mut u[..len], IP_HDR_LEN, IP_PROTO_UDP, udp::SRC_PORT, 40000);
    ok &= check("and stays a checksum through a rewrite",
        wire::be16(&u, IP_HDR_LEN + udp::CHECKSUM) == 0xFFFF && sums_hold(&u[..len]));
    ok
}

fn table() -> bool {
    filled() & shares()
}

/// A mapping of `host`'s, its port `i`: one flow of a guest's.
fn flow(host: u32, i: usize) -> Key {
    Key { proto: IP_PROTO_UDP, inner_ip: host, inner_port: i as u16, remote_ip: REMOTE, remote_port: 53 }
}

/// The table filled, run out, and filled again.
fn filled() -> bool {
    let Some(mut t) = Table::new() else {
        return check("a table to test with", false);
    };
    const BORN: u64 = 1000;
    let mut ok = true;
    for i in 0..ENTRIES {
        match t.insert(&flow(GUEST, i), [0; 6], 0) {
            Some(Made { at, taken: false }) => t.entries[at].expires = BORN + i as u64,
            _ => ok = false,
        }
    }
    ok = check("the table takes every mapping", ok && t.active == ENTRIES);
    ok &= check("and finds each one", (0..ENTRIES).all(|i| t.find(&flow(GUEST, i), 0).is_some()));

    /* The first half run out: a sweep frees those and only those, and is
     * not tried again before the rest can have. */
    let half = BORN + (ENTRIES / 2) as u64 - 1;
    ok &= check("what ran out is swept",
        matches!(t.insert(&flow(GUEST, ENTRIES), [0; 6], half), Some(Made { taken: false, .. }))
            && t.active == ENTRIES / 2 + 1 && t.sweep_at == half + 1);
    ok &= check("and what did not is kept",
        (ENTRIES / 2..ENTRIES).all(|i| t.find(&flow(GUEST, i), half).is_some())
            && (0..ENTRIES / 2).all(|i| t.find(&flow(GUEST, i), half).is_none()));

    let before = t.active;
    let i = t.find(&flow(GUEST, ENTRIES - 1), half).unwrap_or(0);
    t.release(i);
    ok &= check("one let go is gone",
        t.active == before - 1 && t.find(&flow(GUEST, ENTRIES - 1), half).is_none());
    ok &= check("and its port is the next one out",
        t.insert(&flow(GUEST, ENTRIES - 1), [0; 6], half).map(|m| m.at) == Some(i));
    ok
}

/// The table full of live flows: a guest's next flow takes its own used
/// least lately, and another guest's takes from the one that holds the most
/// -- whichever flows it opens, a guest cannot have every mapping there is.
fn shares() -> bool {
    let Some(mut t) = Table::new() else {
        return check("a table to test with", false);
    };
    const LIVE: u64 = u64::MAX;
    let count = |t: &Table, host: u32| t.host(host).map_or(0, |h| usize::from(t.hosts[h].count));
    let made = |t: &mut Table, host: u32, i: usize| match t.insert(&flow(host, i), [0; 6], 0) {
        Some(m) => {
            t.entries[m.at].expires = LIVE;
            Some(m.taken)
        }
        None => None,
    };
    let mut ok = true;
    for i in 0..ENTRIES {
        ok &= made(&mut t, GUEST, i) == Some(false);
    }
    ok = check("one guest's flows fill the table", ok && t.active == ENTRIES && count(&t, GUEST) == ENTRIES);

    /* Its first flow used again: the second is the one used least lately. */
    if let Some(i) = t.find(&flow(GUEST, 0), 0) {
        t.touch(i);
    }
    ok &= check("a full table gives a guest its own flow used least lately",
        made(&mut t, GUEST, ENTRIES) == Some(true) && t.find(&flow(GUEST, 1), 0).is_none()
            && t.find(&flow(GUEST, 0), 0).is_some() && t.active == ENTRIES);

    ok &= check("and another guest's flow takes from the guest holding the most",
        made(&mut t, OTHER, 0) == Some(true) && t.find(&flow(GUEST, 2), 0).is_none()
            && count(&t, GUEST) == ENTRIES - 1 && count(&t, OTHER) == 1);
    let mut taken = true;
    for i in 1..16 {
        taken &= made(&mut t, OTHER, i) == Some(true);
    }
    ok &= check("each of its flows, while it holds fewer",
        taken && count(&t, OTHER) == 16 && count(&t, GUEST) == ENTRIES - 16 && t.active == ENTRIES);

    ok &= check("forget lets one guest's flows go, and only them",
        t.forget(GUEST) == ENTRIES - 16 && t.active == 16 && count(&t, GUEST) == 0
            && count(&t, OTHER) == 16 && (0..16).all(|i| t.find(&flow(OTHER, i), 0).is_some()));
    ok & ties() & slots()
}

/// Two guests holding as many each: a third's flow takes from the one at
/// the lower address -- one rule, the same every time.
fn ties() -> bool {
    let Some(mut t) = Table::new() else {
        return check("a table to test with", false);
    };
    let mut ok = true;
    for i in 0..ENTRIES {
        let host = if i % 2 == 0 { GUEST } else { OTHER };
        match t.insert(&flow(host, i), [0; 6], 0) {
            Some(Made { at, taken: false }) => t.expire_at(at, u64::MAX),
            _ => ok = false,
        }
    }
    let count = |t: &Table, host: u32| t.host(host).map_or(0, |h| usize::from(t.hosts[h].count));
    let third = t.insert(&flow(THIRD, 0), [0; 6], 0).map(|m| m.taken);
    check("a tie goes against the lower address",
        ok && third == Some(true) && count(&t, GUEST) == ENTRIES / 2 - 1 && count(&t, OTHER) == ENTRIES / 2
            && count(&t, THIRD) == 1)
}

/// Every slot held by a host: one past them has no mapping made -- until a
/// host's flows run out, and its slot is swept free for the next.
fn slots() -> bool {
    let Some(mut t) = Table::new() else {
        return check("a table to test with", false);
    };
    const QUIET: u64 = 5000;
    let host = |h: usize| GUEST + 0x100 + h as u32;
    let mut ok = true;
    for h in 0..HOSTS {
        match t.insert(&flow(host(h), 0), [0; 6], 0) {
            Some(Made { at, taken: false }) => t.expire_at(at, if h == 0 { QUIET } else { u64::MAX }),
            _ => ok = false,
        }
    }
    ok = check("a slot for each host there is room for", ok && t.active == HOSTS);
    ok &= check("and none for one more", t.insert(&flow(host(HOSTS), 0), [0; 6], QUIET - 1).is_none());
    ok &= check("until a host's flows run out, which frees its slot",
        matches!(t.insert(&flow(host(HOSTS), 0), [0; 6], QUIET), Some(Made { taken: false, .. }))
            && t.active == HOSTS && t.host(host(0)).is_none() && t.host(host(HOSTS)).is_some());
    ok
}
