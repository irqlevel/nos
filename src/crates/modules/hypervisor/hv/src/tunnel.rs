//! vnet's tunnel in hv (plans/vnet.md, "The tunnel"): a cluster network's
//! packet from a port here to a port of the same network on another host,
//! sealed into a UDP datagram on the underlay (`vnetwire`), and opened on
//! the other side into that port's inbox.
//!
//! - **What it knows** comes from vnet's table, by the keeper
//!   (`vnet::Keeper`): this host's node, every other host's underlay
//!   address and port, and every port on another host -- its network,
//!   address, host and generation. A packet goes into the tunnel only for a
//!   port the table has elsewhere, and comes out of it only from a port the
//!   table has on the host that sealed it, to a port of the same network
//!   here.
//! - **Where**: each host's tunnel listens on its own port -- the one its
//!   registry address has, where it has one of its own, else `PORT` -- and
//!   sends from it, out of the uplink's own address: a host behind a NAT
//!   is reached at the NAT's address and the port the NAT forwards to it,
//!   two hosts behind one address each at a port of its own. What arrives
//!   is known by its key's number, never by where it came from -- a NAT
//!   may give it any address and port -- each number this host gives
//!   unique among every key it has given.
//! - **Keys**: each host gives every other the key to seal *to it* with, in
//!   a message over ipc (`hv.tunnel`, a `Cluster` name): made anew for
//!   every message from the kernel's pool, its number random, installed to
//!   open by before it is sent; carrying the instance of this hv that made
//!   it and the instance of the other's it is for, taken only by that
//!   instance, a number had before not taken again. A host's new instance
//!   has every key to it and from it dropped; a key is given anew every
//!   `rekey` seconds, the one before it kept to open by for `keylife`.
//! - **Sending** is in the context of what sends -- a guest's CPU: the key
//!   and a counter taken under the hosts' lock, the frame built and sealed
//!   outside it, from the pool, only while the uplink keeps room for the
//!   stack (`Nic::tx_room`); the next hop's MAC had beforehand, by the
//!   tunnel's task, which nothing on the way waits for.
//! - **Receiving** is a UDP listener on the uplink, in the receive soft IRQ:
//!   under a key this host gave another that is known, with a counter its
//!   window has not seen; then the frame is kept and
//!   opened where it lies, outside the lock, the tag checked before a byte
//!   is deciphered, and the window moved after -- once, should two CPUs
//!   have opened it.
//! - **A public address** routed to one host and on a port of another's
//!   (plans/vnet.md V4): the world's packet NAT made the port's comes out
//!   of `hv0` here, in its sink -- where nothing may wait -- and is queued
//!   for the task to carry (`carry_in`, kind 3); the port's for the world
//!   goes to the address's host from the guest's CPU (`carry_out`, kind 2).
//!   Each end holds what it takes to its table (`Switch::from_tunnel_out`,
//!   `from_tunnel_in`).
//! - **An echo** through the tunnel to every host with a key, every
//!   `ECHO_S` seconds: a host whose answers stop for `SILENT_S` is said
//!   once in the log, and its key is no longer trusted -- dropped and asked
//!   for anew, so a key's message lost on the way, or a key the host has
//!   let go of, costs that long and not until the next `rekey`. Every
//!   `BIG_EVERY`th is as large as the largest packet a cluster network
//!   carries, and answered as large: a way that carries the small ones and
//!   not those for `SILENT_S` is an underlay short of the networks' MTU,
//!   said once.
//!
//! The locks -- the hosts', the remote ports' -- are each held for a lookup
//! or a change, never with an inbox's lock, never across a seal, an open, a
//! transmit or an ipc send; nothing is allocated, freed or logged under one.
//!
//! tests/fuzz/hv compiles this file as it is (its `tunnel` target): hosts of
//! their own, each a switch and a tunnel, over a wire that loses, repeats,
//! damages, reorders and forges, their keys' messages lost and their tunnels
//! started anew.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};

use kcore::events::SharedEvent;
use kcore::ipc::{Limits, Mailbox, Scope};
use kcore::net::{Lent, Nic, RxContext, UdpHandler, UdpListener};
use kcore::sync::{IrqSpinLock, Mutex};
use kcore::time::Duration;
use kcore::trace::{self, TunnelDrop, TunnelKey, TunnelKeyWhat, TunnelKind, TunnelWhy};
use netwire::{eth, ip, ETH_HDR_LEN, ETH_TYPE_IP, IP_HDR_LEN, IP_PROTO_UDP, UDP_HDR_LEN};
use vnetwire::{Header, Kind, Message, Window, HEADER, KEY, MESSAGE_MAX, PORT, TAG};

use crate::net::Switch;

/// The mailbox the keys go in, on every host.
pub const MAILBOX: &str = "hv.tunnel";
/// The tag its messages carry.
const TAG_TUNNEL: u32 = 1;
/// The most hosts a cluster has: the registry's bound.
const HOSTS: usize = 16;
/// Keys given a host kept to open by: the newest and the one before.
const KEPT: usize = 2;
/// Numbers taken from a host's instance, remembered, none taken twice.
const TAKEN: usize = 32;
/// What the uplink keeps for the stack: a send into the tunnel goes only
/// while the device has more room than this -- ipc, raft and NBS live in the
/// same queue.
const STACK_ROOM: usize = 32;
/// The task's turn, a key asked of a host that gave none, and an echo.
const TURN_MS: u64 = 500;
const HELLO_MS: u64 = 2000;
pub const ECHO_S: u64 = 5;
/// A host answering no echo for this many is said in the log, and its key
/// asked for anew.
pub const SILENT_S: u64 = 20;
/// A key given anew, and the one before kept to open by for: the module's
/// parameters (`rekey=`, `keylife=`), or no gate could wait for them.
pub const REKEY_S_DEFAULT: u64 = 3600;
pub const KEYLIFE_S_DEFAULT: u64 = 120;
const NS_PER_SEC: u64 = 1_000_000_000;
const NS_PER_MS: u64 = 1_000_000;
/// An echo's payload: when it was sent, whether it is a full one, and as
/// many bytes as it is sent with -- `ECHO_BYTES`, or every `BIG_EVERY`th the
/// largest MTU of the cluster's networks.
const ECHO_BYTES: usize = 64;
const BIG_EVERY: u32 = 3;
/// The world's packets for ports elsewhere waiting for the task (`Inbound`),
/// and the most a packet is: Ethernet's MTU.
const INBOUND: usize = 64;
const PACKET_MAX: usize = 1500;
const ECHO_SMALL: u8 = 0;
const ECHO_BIG: u8 = 1;

/// A host of the registry, as the tunnel reaches it: vnet's table's -- its
/// address, and its port: one of its own, or 0 for the tunnel's own
/// (`PORT`).
pub struct TableHost {
    pub node: u64,
    pub name: String,
    pub underlay: u32,
    pub port: u16,
}

/// The port the tunnel reaches host `h` at: its own, or the tunnel's.
fn port_of(h: &TableHost) -> u16 {
    if h.port == 0 { PORT } else { h.port }
}

/// A port on another host: its network, address, host and generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemotePort {
    pub vni: u32,
    pub address: u32,
    pub node: u64,
    pub generation: u64,
}

/// The task's times: its next turn, and its next echo.
#[derive(Default)]
pub struct Clock {
    next_turn: u64,
    next_echo: u64,
}

/// A key given a host, to open by.
#[derive(Clone)]
struct Given {
    number: u32,
    key: [u8; KEY],
    window: Window,
    /// When it was given.
    at: u64,
}

/// Another host of the cluster's, as the tunnel has it. Its name and its
/// mailbox's address are made once, outside the locks, and shared out from
/// under them: nothing is allocated or freed while one is held.
struct Peer {
    node: u64,
    name: Arc<str>,
    /// `hv.tunnel@<name>`.
    to: Arc<str>,
    /// Its address and port, as the table has them: where it is sent to.
    addr: u32,
    port: u16,
    /// The next hop's MAC toward it, had by the task.
    mac: Option<[u8; 6]>,
    /// Its hv's instance, as its messages say.
    instance: Option<u64>,
    /// The key it gave this host to seal to it with: its number, the key and
    /// the next counter -- and when it was taken.
    send: Option<(u32, [u8; KEY], u64)>,
    send_at: u64,
    /// The keys this host gave it, the newest first.
    given: [Option<Given>; KEPT],
    /// The numbers of the keys it gave that were taken, the newest
    /// `TAKEN`: kept when its keys are dropped, so that no message seen
    /// twice has a key sealed with from its first counter again.
    taken: [u32; TAKEN],
    taken_n: usize,
    /// When it was last asked for a key, and last answered an echo.
    asked_at: u64,
    heard_at: u64,
    silent_said: bool,
    /// When a full echo went to it that no full answer has come back for
    /// since (0: none), when one last came, and whether the way's being
    /// too narrow was said.
    big_since: u64,
    big_heard_at: u64,
    narrow_said: bool,
}

impl Peer {
    fn new(h: &TableHost) -> Peer {
        Peer {
            node: h.node, name: Arc::from(h.name.as_str()),
            to: Arc::from(alloc::format!("{}@{}", MAILBOX, h.name).as_str()), addr: h.underlay, port: port_of(h),
            mac: None,
            instance: None, send: None, send_at: 0,
            given: [None, None], taken: [0; TAKEN], taken_n: 0, asked_at: 0, heard_at: 0, silent_said: false,
            big_since: 0, big_heard_at: 0, narrow_said: false,
        }
    }

    /// Every key to it and from it dropped: a new instance of its hv's.
    /// The numbers taken stay; a full echo outstanding is let go -- the
    /// next is the way's word, not one sent to what was there before.
    fn forget(&mut self) {
        self.send = None;
        self.given = [None, None];
        self.big_since = 0;
    }
}

/// The world's packets for ports on other hosts, from `hv0`'s sink --
/// where nothing may wait or take the heap -- for the task to carry: room
/// taken with the tunnel, a full ring dropping (`no_room`).
struct Inbound {
    packets: Vec<[u8; PACKET_MAX]>,
    lens: [u16; INBOUND],
    to: [(u64, u32); INBOUND],
    head: usize,
    count: usize,
}

/// What the tunnel has counted.
#[derive(Default)]
struct Counts {
    sent: AtomicU64,
    received: AtomicU64,
    /// Sends with no key from the host yet, no room on the uplink, no next
    /// hop; datagrams with no key known, replayed, forged, or carrying what
    /// the table does not let through (`stale`).
    no_key: AtomicU64,
    no_room: AtomicU64,
    no_hop: AtomicU64,
    unknown: AtomicU64,
    replayed: AtomicU64,
    forged: AtomicU64,
    stale: AtomicU64,
    echoes: AtomicU64,
}

/// The tunnel: the hosts and their keys, the ports elsewhere, the uplink.
pub struct Tunnel {
    /// This load of hv's: what another host's keys are held to.
    instance: u64,
    peers: IrqSpinLock<Vec<Peer>>,
    /// Every port on another host, sorted by network and address.
    remote: IrqSpinLock<Vec<RemotePort>>,
    /// This host's node, and the address and port the registry has for
    /// it: the port listened on and sent from.
    node: AtomicU64,
    me: AtomicU32,
    port: AtomicU16,
    /// The device the tunnel goes out of, and is listened on.
    nic: IrqSpinLock<Option<Nic>>,
    switch: Arc<Switch>,
    counts: Counts,
    rekey_ns: u64,
    keylife_ns: u64,
    /// The task's: the mailbox, and the listener on the uplink -- and the
    /// event the mailbox is made on, which `carry_in` signals too.
    mailbox: Mutex<Option<Mailbox>>,
    wake: IrqSpinLock<Option<SharedEvent>>,
    inbound: IrqSpinLock<Inbound>,
    /// The device listened on, and the port.
    listener: Mutex<Option<(Nic, u16, UdpListener)>>,
    /// Whether the address the registry has for this host is the uplink's
    /// -- said once where it is not: reached through a NAT, if at all.
    misaddressed: AtomicBool,
    /// Whether two hosts the table has at one address and port were said.
    clash_said: AtomicBool,
    /// The largest MTU of the cluster's networks: a full echo's size.
    mtu: AtomicU32,
    /// Echo rounds so far: every `BIG_EVERY`th a full one.
    rounds: AtomicU32,
}

/// `tunnel_drop`'s record: a datagram from or to the host at `peer` and
/// `peer_port` not taken or not sent, of `header` where it had one, and
/// why. Off, a byte's load and a branch.
fn dropped(peer: u32, peer_port: u16, header: Option<&Header>, why: TunnelWhy) {
    if !trace::on::<TunnelDrop>() {
        return;
    }
    let (network, key, counter, kind) = match header {
        Some(h) => (h.network, h.key, h.counter as u32, h.kind as u8),
        None => (0, 0, 0, TunnelKind::None as u8),
    };
    trace::emit(&TunnelDrop { peer, network, key, counter, peer_port, kind, why: why as u8 });
}

/// `tunnel_key`'s record: what became of the keys with the host at `peer`
/// and `peer_port`, the key's number where one is meant.
fn keyed(peer: u32, peer_port: u16, number: u32, what: TunnelKeyWhat) {
    if trace::on::<TunnelKey>() {
        trace::emit(&TunnelKey { peer, number, peer_port, what: what as u8, _pad: 0 });
    }
}

/// What a packet sent into the tunnel came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    Sent,
    /// No such port elsewhere: the switch's to count.
    NotElsewhere,
    Lost,
}

impl Tunnel {
    /// The tunnel of `switch`, with no host yet: keys given anew every
    /// `rekey_s` seconds, the one before kept for `keylife_s`.
    pub fn new(switch: Arc<Switch>, rekey_s: u64, keylife_s: u64) -> Option<Arc<Tunnel>> {
        let mut peers = Vec::new();
        peers.try_reserve_exact(HOSTS).ok()?;
        let mut packets = Vec::new();
        packets.try_reserve_exact(INBOUND).ok()?;
        packets.resize(INBOUND, [0u8; PACKET_MAX]);
        let instance = kcore::random::random_u64()?;
        Some(Arc::new(Tunnel {
            instance, peers: IrqSpinLock::new(peers), remote: IrqSpinLock::new(Vec::new()),
            node: AtomicU64::new(0), me: AtomicU32::new(0), port: AtomicU16::new(PORT), nic: IrqSpinLock::new(None),
            switch,
            counts: Counts::default(), rekey_ns: rekey_s.saturating_mul(NS_PER_SEC),
            keylife_ns: keylife_s.saturating_mul(NS_PER_SEC), mailbox: Mutex::new(None)?, listener: Mutex::new(None)?,
            wake: IrqSpinLock::new(None),
            inbound: IrqSpinLock::new(Inbound { packets, lens: [0; INBOUND], to: [(0, 0); INBOUND], head: 0, count: 0 }),
            misaddressed: AtomicBool::new(false), clash_said: AtomicBool::new(false), mtu: AtomicU32::new(0),
            rounds: AtomicU32::new(0),
        }))
    }

    /* ---- what the table says ---- */

    /// vnet's table's hosts and ports elsewhere taken -- a host gone dropped
    /// with its keys, a new one added, one whose address or port moved
    /// asked again -- and the largest MTU of its networks, `mtu`. Two hosts
    /// at one address and port are said: what is sent to one reaches
    /// whichever the address's NAT forwards it to. Task context: allocates
    /// and logs outside the locks.
    pub fn set_table(&self, node: u64, hosts: &[TableHost], remote: &[RemotePort], mtu: u16) {
        self.node.store(node, Ordering::Relaxed);
        self.mtu.store(u32::from(mtu), Ordering::Relaxed);
        if let Some(me) = hosts.iter().find(|h| h.node == node) {
            self.me.store(me.underlay, Ordering::Relaxed);
            self.port.store(port_of(me), Ordering::Relaxed);
        }
        let mut clash = None;
        for (i, a) in hosts.iter().enumerate().filter(|(_, h)| h.underlay != 0) {
            if let Some(b) = hosts[i + 1..].iter().find(|b| b.underlay == a.underlay && port_of(b) == port_of(a)) {
                clash = Some((a, b));
                break;
            }
        }
        match clash {
            Some((a, b)) => {
                if !self.clash_said.swap(true, Ordering::AcqRel) {
                    kcore::klog!(0, "hv: tunnel: {} and {} are both at {}:{} -- two hosts behind one address need ports \
                                     of their own: cluster address <host> underlay <ip:port>, the NAT forwarding \
                                     each", a.name, b.name, netwire::Ipv4(a.underlay), port_of(a));
                }
            }
            None => self.clash_said.store(false, Ordering::Release),
        }
        let mut sorted = Vec::new();
        if sorted.try_reserve_exact(remote.len()).is_ok() {
            sorted.extend_from_slice(remote);
            sorted.sort_unstable_by_key(|r| (r.vni, r.address));
            let old = core::mem::replace(&mut *self.remote.lock(), sorted);
            drop(old);
        }
        /* The hosts: those the table has but this one, at most HOSTS --
         * the room taken when the tunnel was made */
        let mut fresh: Vec<Peer> = Vec::new();
        let _ = fresh.try_reserve_exact(HOSTS);
        for h in hosts.iter().filter(|h| h.node != node && h.underlay != 0).take(HOSTS) {
            if fresh.len() < fresh.capacity() {
                fresh.push(Peer::new(h));
            }
        }
        let gone = {
            let mut peers = self.peers.lock();
            /* Those there already are kept -- their keys, and the numbers
             * they took above all: a host whose address, port or name moved
             * is the same host, by its node, and a key's message it sent
             * before, come again, must not be taken anew from its first
             * counter. Sent to where it is now, its next hop looked up
             * anew; the rest are new */
            for f in fresh.iter_mut() {
                if let Some(p) = peers.iter_mut().find(|p| p.node == f.node) {
                    core::mem::swap(f, p);
                    core::mem::swap(&mut f.name, &mut p.name);
                    core::mem::swap(&mut f.to, &mut p.to);
                    if f.addr != p.addr || f.port != p.port {
                        f.addr = p.addr;
                        f.port = p.port;
                        f.mac = None;
                        f.big_since = 0;
                        f.narrow_said = false;
                    }
                }
            }
            core::mem::replace(&mut *peers, fresh)
        };
        drop(gone);
    }

    /// The host port `addr` of network `vni` is on, if it is on another.
    pub fn elsewhere(&self, vni: u32, addr: u32) -> Option<u64> {
        let remote = self.remote.lock();
        remote.binary_search_by_key(&(vni, addr), |r| (r.vni, r.address)).ok().map(|i| remote[i].node)
    }

    /* ---- sending ---- */

    /// `packet`, an IPv4 packet of network `vni` from a port here, to the
    /// port at its destination on another host -- or what it came to.
    /// A guest's CPU's context: nothing waits, nothing is held across the
    /// seal or the transmit.
    pub fn send_packet(&self, kind: Kind, vni: u32, to: u32, packet: &[u8]) -> Sent {
        let Some(node) = self.elsewhere(vni, to) else { return Sent::NotElsewhere };
        if self.send_to(node, kind, vni, packet) { Sent::Sent } else { Sent::Lost }
    }

    /// `packet` sealed to host `node` and sent: whether it went.
    fn send_to(&self, node: u64, kind: Kind, vni: u32, packet: &[u8]) -> bool {
        let Some(nic) = *self.nic.lock() else {
            self.counts.no_hop.fetch_add(1, Ordering::Relaxed);
            dropped(0, 0, None, TunnelWhy::NoHop);
            return false;
        };
        /* The key and a counter of its own, under the lock */
        let taken = {
            let mut peers = self.peers.lock();
            let Some(p) = peers.iter_mut().find(|p| p.node == node) else {
                drop(peers);
                self.counts.no_hop.fetch_add(1, Ordering::Relaxed);
                dropped(0, 0, None, TunnelWhy::NoHop);
                return false;
            };
            match (p.mac, p.send.as_mut()) {
                (None, _) => Err((&self.counts.no_hop, p.addr, p.port, TunnelWhy::NoHop)),
                (_, None) => Err((&self.counts.no_key, p.addr, p.port, TunnelWhy::NoKey)),
                (Some(mac), Some((number, key, next))) if *next < u64::MAX => {
                    let counter = *next;
                    *next += 1;
                    Ok((mac, p.addr, p.port, *number, *key, counter))
                }
                (Some(_), Some(_)) => Err((&self.counts.no_key, p.addr, p.port, TunnelWhy::NoKey)),
            }
        };
        let (mac, addr, port, number, key, counter) = match taken {
            Ok(t) => t,
            Err((count, addr, port, why)) => {
                count.fetch_add(1, Ordering::Relaxed);
                dropped(addr, port, None, why);
                return false;
            }
        };
        if nic.tx_room() <= STACK_ROOM {
            self.counts.no_room.fetch_add(1, Ordering::Relaxed);
            dropped(addr, port, None, TunnelWhy::NoRoom);
            return false;
        }
        let at = ETH_HDR_LEN + IP_HDR_LEN + UDP_HDR_LEN;
        let len = at + HEADER + packet.len() + TAG;
        let Some(mut frame) = kcore::net::NetFrame::alloc_tx(len) else {
            self.counts.no_room.fetch_add(1, Ordering::Relaxed);
            dropped(addr, port, None, TunnelWhy::NoRoom);
            return false;
        };
        let buf = frame.data_raw_mut(len);
        if buf.len() < len {
            self.counts.no_room.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let header = Header { kind, network: vni, key: number, counter };
        if vnetwire::seal(&key, &header, packet, &mut buf[at..len]).is_err() {
            self.counts.no_room.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        /* From the uplink's own address -- a NAT's makes it the address
         * the others know -- and this host's port */
        eth::write(buf, &mac, &nic.mac(), ETH_TYPE_IP);
        let udp_len = UDP_HDR_LEN + HEADER + packet.len() + TAG;
        ip::write_flags(&mut buf[ETH_HDR_LEN..], IP_PROTO_UDP, nic.ip(), addr, udp_len, 0, ip::DONT_FRAGMENT);
        let u = ETH_HDR_LEN + IP_HDR_LEN;
        buf[u..u + 2].copy_from_slice(&self.port.load(Ordering::Relaxed).to_be_bytes());
        buf[u + 2..u + 4].copy_from_slice(&port.to_be_bytes());
        buf[u + 4..u + 6].copy_from_slice(&(udp_len as u16).to_be_bytes());
        buf[u + 6..u + 8].copy_from_slice(&[0, 0]);
        frame.set_len(len);
        if nic.transmit(frame) {
            self.counts.sent.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            self.counts.no_room.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /* ---- receiving ---- */

    /// A datagram to the tunnel's port, `frame` -- its Ethernet header
    /// first -- held to what the table lets through and opened: the
    /// receive soft IRQ's.
    fn receive(&self, frame: Lent<'_>) {
        /* What the frame says, read before it is kept */
        let (from, from_port, payload_at, len, header) = {
            let Some(udp) = netwire::udp::parse(frame.bytes()) else { return };
            if udp.dst_port != self.port.load(Ordering::Relaxed) {
                return;
            }
            let Some(header) = Header::read(udp.payload) else {
                self.counts.stale.fetch_add(1, Ordering::Relaxed);
                dropped(udp.src_ip, udp.src_port, None, TunnelWhy::Malformed);
                return;
            };
            (udp.src_ip, udp.src_port, udp.payload_at, udp.payload.len(), header)
        };
        /* The host it is from: the one this host gave the key of that
         * number -- whatever address and port a NAT on the way gave it --
         * and the key, where its window has not seen the counter, copied
         * out. A number no key has: from a host whose keys are gone, where
         * the address is a host's, else from nobody known */
        let found = {
            let peers = self.peers.lock();
            match peers.iter().find_map(|p| p.given.iter().flatten().find(|g| g.number == header.key).map(|g| (p, g))) {
                Some((p, g)) => Ok((p.node, p.addr, p.port, g.window.fresh(header.counter).then_some(g.key))),
                None => Err(peers.iter().any(|p| p.addr == from)),
            }
        };
        let (node, addr, port, key) = match found {
            Err(false) => {
                self.counts.unknown.fetch_add(1, Ordering::Relaxed);
                dropped(from, from_port, Some(&header), TunnelWhy::Unknown);
                return;
            }
            Err(true) => {
                self.counts.no_key.fetch_add(1, Ordering::Relaxed);
                dropped(from, from_port, Some(&header), TunnelWhy::NoKey);
                return;
            }
            Ok((_, addr, port, None)) => {
                self.counts.replayed.fetch_add(1, Ordering::Relaxed);
                dropped(addr, port, Some(&header), TunnelWhy::Replayed);
                return;
            }
            Ok((node, addr, port, Some(key))) => (node, addr, port, key),
        };
        /* Kept, and opened where it lies */
        let mut kept = frame.retain();
        let data = kept.data_mut();
        let Some(datagram) = data.get_mut(payload_at..payload_at + len) else { return };
        let Ok((header, packet)) = vnetwire::open(&key, datagram) else {
            self.counts.forged.fetch_add(1, Ordering::Relaxed);
            dropped(addr, port, Some(&header), TunnelWhy::Forged);
            return;
        };
        /* The window moved -- once: another CPU may have opened it too */
        let first = {
            let mut peers = self.peers.lock();
            match peers.iter_mut().find(|p| p.node == node)
                .and_then(|p| p.given.iter_mut().flatten().find(|g| g.number == header.key))
            {
                Some(g) if g.window.fresh(header.counter) => {
                    g.window.take(header.counter);
                    true
                }
                _ => false,
            }
        };
        if !first {
            self.counts.replayed.fetch_add(1, Ordering::Relaxed);
            dropped(addr, port, Some(&header), TunnelWhy::Replayed);
            return;
        }
        self.counts.received.fetch_add(1, Ordering::Relaxed);
        match header.kind {
            Kind::PortToPort => self.arrived(node, (addr, port), &header, packet),
            Kind::Echo => {
                /* Answered as it asked: as large, so a full one tries the
                 * way back too */
                self.send_to(node, Kind::EchoAnswer, 0, packet);
            }
            Kind::EchoAnswer => {
                self.counts.echoes.fetch_add(1, Ordering::Relaxed);
                let now = kcore::time::boot_time_ns();
                let big = packet.get(8) == Some(&ECHO_BIG);
                if let Some(p) = self.peers.lock().iter_mut().find(|p| p.node == node) {
                    p.heard_at = now;
                    p.silent_said = false;
                    if big {
                        p.big_heard_at = now;
                        p.big_since = 0;
                        p.narrow_said = false;
                    }
                }
            }
            /* A public address's kinds are V4's */
            /* A port's packet for the world, to the host its public
             * address is routed to; the world's, to the port */
            Kind::PortToGateway => {
                if !self.switch.from_tunnel_out(node, header.network, packet) {
                    self.counts.stale.fetch_add(1, Ordering::Relaxed);
                    dropped(addr, port, Some(&header), TunnelWhy::Stale);
                }
            }
            Kind::GatewayToPort => {
                if !self.switch.from_tunnel_in(node, header.network, packet) {
                    self.counts.stale.fetch_add(1, Ordering::Relaxed);
                    dropped(addr, port, Some(&header), TunnelWhy::Stale);
                }
            }
        }
    }

    /// An IPv4 packet of the network `header` names from host `node`'s port
    /// -- the host at `at`, its address and port -- for a port here: from a
    /// port the table has on that host, of that network -- or counted as
    /// `stale`, as what the switch will not take.
    fn arrived(&self, node: u64, at: (u32, u16), header: &Header, packet: &[u8]) {
        let vni = header.network;
        if packet.len() < IP_HDR_LEN || ip::version(packet) != 4 {
            self.counts.stale.fetch_add(1, Ordering::Relaxed);
            dropped(at.0, at.1, Some(header), TunnelWhy::Stale);
            return;
        }
        if self.elsewhere(vni, ip::src(packet)) != Some(node) || !self.switch.from_tunnel(vni, packet) {
            self.counts.stale.fetch_add(1, Ordering::Relaxed);
            dropped(at.0, at.1, Some(header), TunnelWhy::Stale);
        }
    }

    /* ---- the keys, and the task ---- */

    /// A message of the tunnel's from host `name` taken.
    pub(crate) fn on_message(&self, name: &str, m: &Message, now: u64) {
        let mut give = None;
        /* What became of the keys with it, recorded after the lock */
        let mut record: [Option<(u32, u16, u32, TunnelKeyWhat)>; 2] = [None, None];
        {
            let mut peers = self.peers.lock();
            let Some(p) = peers.iter_mut().find(|p| &*p.name == name) else { return };
            match *m {
                Message::Hello { instance } => {
                    if p.instance != Some(instance) {
                        p.forget();
                        p.instance = Some(instance);
                        record[0] = Some((p.addr, p.port, 0, TunnelKeyWhat::Dropped));
                    }
                    give = Some(p.node);
                }
                Message::Key { made_by, made_for, number, key } => {
                    let had = p.taken[..p.taken_n.min(TAKEN)].contains(&number);
                    if made_for != self.instance || had {
                        return;
                    }
                    if p.instance != Some(made_by) {
                        p.forget();
                        p.instance = Some(made_by);
                        give = Some(p.node);
                        record[0] = Some((p.addr, p.port, 0, TunnelKeyWhat::Dropped));
                    }
                    /* A full echo outstanding stays: a key given anew --
                     * every `rekey` -- says nothing of the way */
                    p.send = Some((number, key, 0));
                    p.send_at = now;
                    p.taken[p.taken_n % TAKEN] = number;
                    p.taken_n += 1;
                    record[1] = Some((p.addr, p.port, number, TunnelKeyWhat::Taken));
                }
                Message::Gone { instance } => {
                    if p.instance == Some(instance) {
                        p.forget();
                        p.instance = None;
                        record[0] = Some((p.addr, p.port, 0, TunnelKeyWhat::Dropped));
                    }
                }
            }
        }
        for (peer, port, number, what) in record.into_iter().flatten() {
            keyed(peer, port, number, what);
        }
        if let Some(node) = give {
            self.give(node, now);
        }
    }

    /// A key made anew for host `node`, installed to open by, and given it:
    /// its number not 0 and none other a key this host has given has -- what
    /// arrives is known by it. Refused while the kernel's pool has no seed:
    /// a key is never made of anything less.
    fn give(&self, node: u64, now: u64) {
        let mut key = [0u8; KEY];
        let mut n = [0u8; 8];
        if !kcore::random::fill_random(&mut key) || !kcore::random::fill_random(&mut n) {
            return;
        }
        /* Two numbers drawn, the first not had taken: both had -- one in
         * two billion squared -- and the next turn draws again */
        let drawn = [u32::from_be_bytes([n[0], n[1], n[2], n[3]]), u32::from_be_bytes([n[4], n[5], n[6], n[7]])];
        let (to, addr, port, number, message) = {
            let mut peers = self.peers.lock();
            let had = |x: u32| x == 0 || peers.iter().any(|q| q.given.iter().flatten().any(|g| g.number == x));
            let Some(number) = drawn.into_iter().find(|&x| !had(x)) else { return };
            let Some(p) = peers.iter_mut().find(|p| p.node == node) else { return };
            let Some(made_for) = p.instance else { return };
            p.given[1] = p.given[0].take();
            p.given[0] = Some(Given { number, key, window: Window::new(), at: now });
            (p.to.clone(), p.addr, p.port, number, Message::Key { made_by: self.instance, made_for, number, key })
        };
        keyed(addr, port, number, TunnelKeyWhat::Given);
        self.say(&to, &message);
    }

    /// `message` sent to the mailbox `to`; what fails is the next turn's.
    fn say(&self, to: &str, message: &Message) {
        let mut buf = [0u8; MESSAGE_MAX];
        let n = message.encode(&mut buf);
        if let Some(mb) = self.mailbox.lock().as_ref() {
            let _ = mb.send_to(to, TAG_TUNNEL, 0, &buf[..n]);
        }
    }

    /// The task's turn: a host with no key from it asked, a key given anew
    /// where its time is up and the one before it let go after its overlap,
    /// a next hop had where there is none, an echo, a silent host said. Task
    /// context: a next hop's look may wait for ARP.
    pub(crate) fn turn(&self, now: u64, echo: bool) {
        /* Room for every host taken first: nothing is allocated under the
         * lock, and a list full drops what is past it to the next turn */
        let mut ask: Vec<(Arc<str>, u32, u16)> = Vec::new();
        let mut rekey = Vec::new();
        let mut hops = Vec::new();
        let mut echoes = Vec::new();
        let mut silent: Vec<(Arc<str>, u32, u16)> = Vec::new();
        let mut narrow: Vec<(Arc<str>, u32, u16)> = Vec::new();
        if ask.try_reserve_exact(HOSTS).is_err() || rekey.try_reserve_exact(HOSTS).is_err()
            || hops.try_reserve_exact(HOSTS).is_err() || echoes.try_reserve_exact(HOSTS).is_err()
            || silent.try_reserve_exact(HOSTS).is_err() || narrow.try_reserve_exact(HOSTS).is_err()
        {
            return;
        }
        /* This round's echo a full one, every BIG_EVERYth: as large as the
         * largest packet a network carries */
        let mtu = self.mtu.load(Ordering::Relaxed) as usize;
        let big = echo && mtu > ECHO_BYTES && self.rounds.fetch_add(1, Ordering::Relaxed) % BIG_EVERY == BIG_EVERY - 1;
        let size = if big { mtu } else { ECHO_BYTES };
        let mut payload = Vec::new();
        if payload.try_reserve_exact(size).is_err() {
            return;
        }
        payload.resize(size, 0);
        payload[..8].copy_from_slice(&now.to_be_bytes());
        payload[8] = if big { ECHO_BIG } else { ECHO_SMALL };
        {
            let mut peers = self.peers.lock();
            for p in peers.iter_mut() {
                if p.send.is_none() && now.saturating_sub(p.asked_at) >= HELLO_MS * NS_PER_MS
                    && ask.len() < ask.capacity()
                {
                    p.asked_at = now;
                    ask.push((p.to.clone(), p.addr, p.port));
                }
                if p.instance.is_some()
                    && p.given[0].as_ref().is_none_or(|g| now.saturating_sub(g.at) >= self.rekey_ns)
                    && rekey.len() < rekey.capacity()
                {
                    rekey.push(p.node);
                }
                if let Some(old) = &p.given[1] {
                    let newest = p.given[0].as_ref().map_or(0, |g| g.at);
                    if now.saturating_sub(newest) >= self.keylife_ns && old.at <= newest {
                        p.given[1] = None;
                    }
                }
                if p.mac.is_none() && hops.len() < hops.capacity() {
                    hops.push((p.node, p.addr));
                }
                if echo && p.send.is_some() && echoes.len() < echoes.capacity() {
                    echoes.push(p.node);
                    if big && p.big_since == 0 {
                        p.big_since = now;
                    }
                }
                /* Full echoes unanswered for SILENT_S while the small ones
                 * are: the way does not carry the networks' MTU */
                if p.big_since != 0 && p.heard_at > p.big_since && !p.narrow_said
                    && now.saturating_sub(p.big_since) >= SILENT_S * NS_PER_SEC && narrow.len() < narrow.capacity()
                {
                    p.narrow_said = true;
                    narrow.push((p.name.clone(), p.addr, p.port));
                }
                /* No echo answered since the key was taken, or for
                 * SILENT_S: the key no longer trusted, asked for anew at
                 * the next turn -- said once, where it had answered */
                if p.send.is_some() && now.saturating_sub(p.heard_at.max(p.send_at)) >= SILENT_S * NS_PER_SEC {
                    if p.heard_at != 0 && !p.silent_said && silent.len() < silent.capacity() {
                        p.silent_said = true;
                        silent.push((p.name.clone(), p.addr, p.port));
                    }
                    p.send = None;
                    p.asked_at = 0;
                    p.big_since = 0;
                }
            }
        }
        for (name, addr, port) in silent.iter() {
            kcore::klog!(0, "hv: tunnel: {} answered no echo for {} s", name, SILENT_S);
            keyed(*addr, *port, 0, TunnelKeyWhat::Silent);
        }
        for (name, addr, port) in narrow.iter() {
            kcore::klog!(0, "hv: tunnel: the way to {} does not carry a packet of {} bytes -- a cluster network's MTU \
                             is past what the underlay takes", name, mtu);
            keyed(*addr, *port, 0, TunnelKeyWhat::Narrow);
        }
        let hello = Message::Hello { instance: self.instance };
        for (to, addr, port) in ask.iter() {
            keyed(*addr, *port, 0, TunnelKeyWhat::Asked);
            self.say(to, &hello);
        }
        for node in rekey {
            self.give(node, now);
        }
        /* The device copied out: its look may wait for ARP */
        let nic = *self.nic.lock();
        if let Some(nic) = nic {
            for (node, addr) in hops {
                if let Some(mac) = nic.resolve(addr) {
                    if let Some(p) = self.peers.lock().iter_mut().find(|p| p.node == node && p.addr == addr) {
                        p.mac = Some(mac);
                    }
                }
            }
        }
        for node in echoes {
            let _ = self.send_to(node, Kind::Echo, 0, &payload);
        }
    }

    /// The uplink the tunnel goes out of and listens on: `nic`, the device
    /// NAT goes out of, while there is one -- listened on anew where it, or
    /// this host's port, changed. Said once where the registry's address for
    /// this host is not that device's: the other hosts reach it only through
    /// a NAT that forwards its port to it.
    pub(crate) fn uplink(self: &Arc<Self>, nic: Option<Nic>) {
        let me = self.me.load(Ordering::Relaxed);
        let port = self.port.load(Ordering::Relaxed);
        if let Some(n) = nic {
            if me != 0 && n.ip() != me && !self.misaddressed.swap(true, Ordering::AcqRel) {
                kcore::klog!(0, "hv: tunnel: the registry has {}:{} for this host and the uplink is at {} -- the other \
                                 hosts reach it only through a NAT that forwards UDP port {} here", netwire::Ipv4(me),
                             port, netwire::Ipv4(n.ip()), port);
            }
        }
        let same = {
            let held = self.listener.lock();
            match (held.as_ref(), nic) {
                (Some((had, at, _)), Some(n)) => had.ip() == n.ip() && had.mac() == n.mac() && *at == port,
                (None, None) => true,
                _ => false,
            }
        };
        if same {
            return;
        }
        *self.nic.lock() = nic;
        /* The old listener let go of with no lock held: its drop waits */
        let old = self.listener.lock().take();
        drop(old);
        if let Some(n) = nic {
            match n.listen(port, Arc::new(Receiver(self.clone()))) {
                Ok(l) => *self.listener.lock() = Some((n, port, l)),
                Err(e) => kcore::klog!(0, "hv: tunnel: not listening on port {}: {:?}", port, e),
            }
        }
    }

    /// The task's body, until it is asked to stop: its mailbox made and
    /// named, then a message taken or a turn every `TURN_MS`; going, every
    /// host told so, and the listener and the mailbox let go of.
    pub fn run(tunnel: Arc<Tunnel>) {
        tunnel.open_mailbox();
        let mut clock = Clock::default();
        while !kcore::task::stopping() {
            /* The mailbox is the task's alone: held as it waits */
            let message = tunnel.mailbox.lock().as_ref().and_then(|mb| mb.recv(Duration::from_millis(TURN_MS)));
            if tunnel.mailbox.lock().is_none() {
                kcore::task::sleep_ms(TURN_MS);
            }
            let now = kcore::time::boot_time_ns();
            if let Some(m) = message {
                if !m.is_system() && m.tag == TAG_TUNNEL {
                    if let (Some(name), Some(msg)) = (kcore::ipc::peer_name(m.from), Message::decode(&m.data)) {
                        tunnel.on_message(&name, &msg, now);
                    }
                }
            }
            tunnel.drain_in();
            tunnel.tick(now, &mut clock);
        }
        tunnel.close();
    }

    /// The mailbox the keys come in by, made and named: the task's first
    /// step.
    pub(crate) fn open_mailbox(&self) {
        let Some(event) = SharedEvent::new() else {
            kcore::klog!(0, "hv: tunnel: no event to wait on: no mailbox, and the world's packets for ports here \
                             are not carried");
            return;
        };
        match Mailbox::new_on(Limits { messages: 64, bytes: 64 * 1024 }, &event) {
            Ok(mb) => {
                if let Err(e) = mb.register(MAILBOX, Scope::Cluster) {
                    kcore::klog!(0, "hv: tunnel: {} not named: {}", MAILBOX, e.name());
                }
                *self.mailbox.lock() = Some(mb);
            }
            Err(e) => kcore::klog!(0, "hv: tunnel: no mailbox: {}", e.name()),
        }
        /* The one before let go of with the lock down */
        let old = core::mem::replace(&mut *self.wake.lock(), Some(event));
        drop(old);
    }

    /// The world's packets `carry_in` queued, each carried to its port's
    /// host: the task's -- at most a ring's worth a call, so that what keeps
    /// coming does not keep it from its turn.
    pub(crate) fn drain_in(&self) {
        let mut packet = [0u8; PACKET_MAX];
        for _ in 0..INBOUND {
            let (len, node, vni) = {
                let mut ring = self.inbound.lock();
                if ring.count == 0 {
                    return;
                }
                let at = ring.head;
                let len = usize::from(ring.lens[at]).min(PACKET_MAX);
                packet[..len].copy_from_slice(&ring.packets[at][..len]);
                ring.head = (at + 1) % INBOUND;
                ring.count -= 1;
                (len, ring.to[at].0, ring.to[at].1)
            };
            self.send_to(node, Kind::GatewayToPort, vni, &packet[..len]);
        }
    }

    /// The task's turn, where `clock` has it due: the uplink looked at, and
    /// the hosts (`turn`), an echo among them where that is due too. The
    /// uplink is NAT's -- the device the default route is on -- and NAT is
    /// put on for it once there are other hosts to reach, where no guest
    /// has put it on yet: a host with no guest of its own still carries
    /// keys and echoes, and is reached as soon as one starts.
    pub(crate) fn tick(self: &Arc<Self>, now: u64, clock: &mut Clock) {
        if now < clock.next_turn {
            return;
        }
        clock.next_turn = now + TURN_MS * NS_PER_MS;
        let mut nic = self.switch.outer_nic();
        let hosts = !self.peers.lock().is_empty();
        if nic.is_none() && hosts && self.switch.way_out().is_ok() {
            nic = self.switch.outer_nic();
        }
        self.uplink(nic);
        let echo = now >= clock.next_echo;
        if echo {
            clock.next_echo = now + ECHO_S * NS_PER_SEC;
        }
        self.turn(now, echo);
    }

    /// Going: every host told, to drop its keys to and from this one; the
    /// listener and the mailbox let go of -- the listener first, its drop
    /// waiting out a receive still running. Task context.
    pub(crate) fn close(&self) {
        let gone = Message::Gone { instance: self.instance };
        let mut names: Vec<Arc<str>> = Vec::new();
        if names.try_reserve_exact(HOSTS).is_ok() {
            for p in self.peers.lock().iter() {
                if names.len() < names.capacity() {
                    names.push(p.to.clone());
                }
            }
        }
        for to in names.iter() {
            self.say(to, &gone);
        }
        let listener = self.listener.lock().take();
        drop(listener);
        let mailbox = self.mailbox.lock().take();
        drop(mailbox);
        let wake = self.wake.lock().take();
        drop(wake);
        *self.nic.lock() = None;
    }

    /// `hv tunnel`: the hosts, their keys and echoes, and what was counted.
    pub fn report(&self, out: &mut dyn Write) {
        let now = kcore::time::boot_time_ns();
        let _ = writeln!(out, "hv tunnel: instance {:016x}, node {}, at {}:{}", self.instance,
                         self.node.load(Ordering::Relaxed), netwire::Ipv4(self.me.load(Ordering::Relaxed)),
                         self.port.load(Ordering::Relaxed));
        /* Each host copied out under the lock, written after */
        let mut rows: Vec<(Arc<str>, u32, u16, bool, bool, usize, u64, u64)> = Vec::new();
        if rows.try_reserve_exact(HOSTS).is_ok() {
            for p in self.peers.lock().iter() {
                if rows.len() < rows.capacity() {
                    rows.push((p.name.clone(), p.addr, p.port, p.mac.is_some(), p.send.is_some(),
                               p.given.iter().flatten().count(), p.heard_at, p.big_heard_at));
                }
            }
        }
        let ago = |at: u64| if at == 0 { String::from("never") }
                            else { alloc::format!("{} s ago", now.saturating_sub(at) / NS_PER_SEC) };
        for (name, addr, port, hop, key, given, heard_at, big_at) in rows.iter() {
            let _ = writeln!(out, "  {:<16} {:<21} hop {}  key to it {}  keys given it {}  echo answered {}, a full \
                                   one {}", name, alloc::format!("{}:{}", netwire::Ipv4(*addr), port),
                             if *hop { "known" } else { "none" }, if *key { "had" } else { "none" }, given,
                             ago(*heard_at), ago(*big_at));
        }
        for (public, inner, node) in self.switch.served() {
            let _ = writeln!(out, "  serves {} for {} on node {}", netwire::Ipv4(public), netwire::Ipv4(inner), node);
        }
        let (served_in, served_out, over) = self.switch.served_counts();
        if served_in + served_out + over != 0 {
            let _ = writeln!(out, "  the world's packets carried to ports elsewhere {}; their packets into hv0 {}, past \
                                   their share {}", served_in, served_out, over);
        }
        let c = &self.counts;
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let _ = writeln!(out, "  sent {}, received {}; not sent: no key {}, no room {}, no hop {}; not taken: unknown {}, \
                               replayed {}, forged {}, stale {}; echoes answered {}",
                         l(&c.sent), l(&c.received), l(&c.no_key), l(&c.no_room), l(&c.no_hop), l(&c.unknown),
                         l(&c.replayed), l(&c.forged), l(&c.stale), l(&c.echoes));
    }
}

/// The tunnel's listener on the uplink.
struct Receiver(Arc<Tunnel>);

impl UdpHandler for Receiver {
    fn on_frame(&self, frame: Lent<'_>, _rx: &mut RxContext) {
        self.0.receive(frame);
    }
}

/// What the switch carries into the tunnel by: a cluster network's packet to
/// a port on another host.
impl crate::net::Carrier for Tunnel {
    fn elsewhere(&self, vni: u32, addr: u32) -> bool {
        Tunnel::elsewhere(self, vni, addr).is_some()
    }

    fn carry(&self, vni: u32, to: u32, packet: &[u8]) -> bool {
        self.send_packet(Kind::PortToPort, vni, to, packet) == Sent::Sent
    }

    fn carry_out(&self, node: u64, vni: u32, packet: &[u8]) -> bool {
        self.send_to(node, Kind::PortToGateway, vni, packet)
    }

    fn carry_in(&self, node: u64, vni: u32, packet: &[u8]) -> bool {
        if packet.len() > PACKET_MAX {
            self.counts.no_room.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        {
            let mut ring = self.inbound.lock();
            if ring.count == INBOUND {
                drop(ring);
                self.counts.no_room.fetch_add(1, Ordering::Relaxed);
                dropped(0, 0, None, TunnelWhy::NoRoom);
                return false;
            }
            let at = (ring.head + ring.count) % INBOUND;
            ring.packets[at][..packet.len()].copy_from_slice(packet);
            ring.lens[at] = packet.len() as u16;
            ring.to[at] = (node, vni);
            ring.count += 1;
        }
        if let Some(e) = self.wake.lock().as_ref() {
            e.signal();
        }
        true
    }
}
