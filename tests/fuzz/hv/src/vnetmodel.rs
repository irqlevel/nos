//! The switch's cluster networks (`modules/hypervisor/hv/src/net.rs`,
//! plans/vnet.md, "The switch"): ports claimed as who a network of the
//! cluster's made them -- an address, a subnet, an MTU, whether it goes out
//! -- beside ports of the host's own networks, and `hv0`. What each port's
//! inbox is given, what `hv0` is handed and every count, held to a model
//! read from the plan and the RFCs, sharing nothing with the switch: ARP and
//! the gateway's echo answered by the switch, `hv0`'s ARP after a port's
//! address answered through the ring a task hands in, isolation, the MTU and
//! its "fragmentation needed", DHCP from the gateway with the mask and the
//! MTU, the firewall at both ends, deliveries held to the port's owner.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::dhcp;
use crate::network;
use crate::nic::Backend as _;
use crate::offload;
use crate::switch::{self, Identity, Switch, MAX_PORTS};
use crate::targets::Input;
use crate::vms::Shared;
use crate::vnic;

/* The switch's constants, as its documents and the plan say them */
const HOST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0x64, 0x01];
const HOST_IP: u32 = 0x0A00_6401;
const HOST_SUBNET: u32 = 0x0A00_6400;
const HOST_MASK: u32 = 0xFFFF_FF00;
const BROADCAST: [u8; 6] = [0xFF; 6];
const INBOX: usize = 256;
const WHOLE: usize = 8;
const BACKLOG: usize = 256;
const RING: usize = 2 * MAX_PORTS;
/// What a port is told at once, of what its firewall kept in and of what
/// was past its MTU: the clock never earns another in an input.
const TOLD: u32 = 16;
const ETH: usize = 14;
const IP: usize = 20;
const ARP_BYTES: usize = 28;
const SEND_MAX: usize = 65_536;

/// What the inputs came to, for `HV_FUZZ_STATS`: that each way the model
/// knows was taken, and how often.
#[derive(Clone, Copy)]
enum Seen {
    Claimed,
    Refused,
    Delivered,
    DeliveredPartial,
    DeliveredWhole,
    ArpAnswered,
    HostArpQueued,
    Echoed,
    ToHost,
    TooBigTold,
    TooBig,
    Isolated,
    Misdirected,
    Spoofed,
    Foreign,
    Dhcp,
    DeniedIn,
    DeniedOut,
    Refusal,
    Full,
    OverShare,
    HostPortFrame,
    Carried,
    FromTunnel,
    CarriedOn,
}

const SEEN: [&str; 25] = ["claims", "claims refused", "frames delivered", "with a checksum left to finish",
                          "sends whole", "ARP answered", "hv0's ARP answered", "echoes", "to hv0", "too big, told",
                          "too big", "isolated", "misdirected", "spoofed", "foreign", "DHCP answered",
                          "denied in", "denied out", "told of a denial", "an inbox full", "past a share",
                          "to a host network's port", "carried to another host", "out of the tunnel",
                          "carried on for a port moved away"];

static COUNTS: [std::sync::atomic::AtomicU64; 25] = [const { std::sync::atomic::AtomicU64::new(0) }; 25];

fn seen(what: Seen) {
    COUNTS[what as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn report() {
    let all: Vec<String> = SEEN.iter().zip(COUNTS.iter())
        .map(|(name, n)| format!("{} {}", n.load(std::sync::atomic::Ordering::Relaxed), name)).collect();
    println!("  vnetswitch: {}", all.join(", "));
}

/// The firewalls in their places: every packet let through, and none.
const ALLOW_ALL: usize = 0;
const DENY_ALL: usize = 1;

/// The cluster networks a port may be given, as vnet would give them -- and
/// two it would not give beside the first: its number with another subnet,
/// and a subnet that meets it.
#[derive(Clone, Copy)]
struct Net {
    vni: u32,
    net: u32,
    prefix: u8,
    mtu: u16,
    out: bool,
}

const NETS: [Net; 5] = [
    Net { vni: 1, net: 0x0A0A_0000, prefix: 24, mtu: 1400, out: true },
    Net { vni: 2, net: 0x0A14_0000, prefix: 24, mtu: 576, out: false },
    Net { vni: 3, net: 0xC0A8_0500, prefix: 28, mtu: 1440, out: true },
    Net { vni: 1, net: 0x0A0B_0000, prefix: 24, mtu: 1400, out: true },
    Net { vni: 4, net: 0x0A0A_0000, prefix: 16, mtu: 1400, out: true },
];

/// Subnets vnet's table may have, a port here or not: the first three's,
/// and one of no port's.
const TABLE: [(u32, u8); 4] = [(0x0A0A_0000, 24), (0x0A14_0000, 24), (0xC0A8_0500, 28), (0xAC10_0000, 16)];

/// Addresses past every network here: the world's.
const WORLD: [u32; 3] = [0x0808_0808, 0x0101_0101, 0x5DB8_D822];

/// Ports on other hosts, as the tunnel would have them: (network, address).
const REMOTE: [(u32, u32); 4] = [(1, 0x0A0A_0064), (1, 0x0A0A_0065), (3, 0xC0A8_050D), (2, 0x0A14_0064)];

/// The tunnel as the switch sees it (`switch::Carrier`): the ports on other
/// hosts, and what it was handed to carry.
struct Tunnel {
    carried: std::sync::Mutex<Vec<(u32, u32, Vec<u8>)>>,
    /// What it was handed for a port moved away: the host, the network, the
    /// packet.
    carried_on: std::sync::Mutex<Vec<(u64, u32, Vec<u8>)>>,
}

impl switch::Carrier for Tunnel {
    fn elsewhere(&self, vni: u32, addr: u32) -> bool {
        REMOTE.contains(&(vni, addr))
    }

    fn carry(&self, vni: u32, to: u32, packet: &[u8]) -> bool {
        self.carried.lock().unwrap_or_else(|e| e.into_inner()).push((vni, to, packet.to_vec()));
        true
    }

    /* No public address is served here or set on a port: the switch never
     * asks these -- the `tunnel` target's real tunnels do */
    fn carry_out(&self, _node: u64, _vni: u32, _packet: &[u8]) -> bool {
        panic!("invariant: a packet carried out to a public address's host, with none set");
    }

    fn carry_in(&self, _node: u64, _vni: u32, _packet: &[u8]) -> bool {
        panic!("invariant: a packet carried in for a public address, with none served");
    }

    fn carry_to(&self, node: u64, vni: u32, packet: &[u8]) -> bool {
        self.carried_on.lock().unwrap_or_else(|e| e.into_inner()).push((node, vni, packet.to_vec()));
        true
    }
}

fn mask(prefix: u8) -> u32 {
    u32::MAX << (32 - u32::from(prefix))
}

pub(crate) fn mac_for(ip: u32) -> [u8; 6] {
    let b = ip.to_be_bytes();
    [0x02, 0x00, b[0], b[1], b[2], b[3]]
}

fn host_mac(p: usize) -> [u8; 6] {
    [0x02, 0, 0, 0, 0x64, (p + 2) as u8]
}

fn host_ip(p: usize) -> u32 {
    HOST_SUBNET | (p as u32 + 2)
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// RFC 1071's sum, the model's own.
fn sum(bytes: &[u8]) -> u16 {
    let mut s: u32 = 0;
    for pair in bytes.chunks(2) {
        let word = if pair.len() == 2 { u32::from(be16(pair, 0)) } else { u32::from(pair[0]) << 8 };
        s += word;
    }
    while s >> 16 != 0 {
        s = (s & 0xFFFF) + (s >> 16);
    }
    !(s as u16)
}

pub(crate) fn eth(dst: [u8; 6], src: [u8; 6], ethertype: u16, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(ETH + body.len());
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(body);
    f
}

/// A 20-byte IPv4 header and `l4` behind it: TTL 64, its id and flags as
/// given.
pub(crate) fn ipv4(src: u32, dst: u32, proto: u8, id: u16, flags: u16, l4: &[u8]) -> Vec<u8> {
    let mut p = vec![0u8; IP];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&((IP + l4.len()) as u16).to_be_bytes());
    p[4..6].copy_from_slice(&id.to_be_bytes());
    p[6..8].copy_from_slice(&flags.to_be_bytes());
    p[8] = 64;
    p[9] = proto;
    p[12..16].copy_from_slice(&src.to_be_bytes());
    p[16..20].copy_from_slice(&dst.to_be_bytes());
    let s = sum(&p);
    p[10..12].copy_from_slice(&s.to_be_bytes());
    p.extend_from_slice(l4);
    p
}

/// An ICMP message: its type, code, the word after the checksum, and data.
fn icmp(kind: u8, code: u8, word: [u8; 4], data: &[u8]) -> Vec<u8> {
    let mut m = vec![kind, code, 0, 0];
    m.extend_from_slice(&word);
    m.extend_from_slice(data);
    let s = sum(&m);
    m[2..4].copy_from_slice(&s.to_be_bytes());
    m
}

fn arp(op: u16, sha: [u8; 6], spa: u32, tha: [u8; 6], tpa: u32) -> Vec<u8> {
    let mut a = vec![0, 1, 0x08, 0x00, 6, 4];
    a.extend_from_slice(&op.to_be_bytes());
    a.extend_from_slice(&sha);
    a.extend_from_slice(&spa.to_be_bytes());
    a.extend_from_slice(&tha);
    a.extend_from_slice(&tpa.to_be_bytes());
    a
}

/// A DHCP client's message, well formed: BOOTREQUEST, its type, the
/// address it asks for when it asks one, from `ciaddr`.
fn dhcp_message(kind: u8, xid: u32, chaddr: [u8; 6], ciaddr: u32, requested: Option<u32>) -> Vec<u8> {
    let mut m = vec![0u8; 240];
    m[0] = 1;
    m[1] = 1;
    m[2] = 6;
    m[4..8].copy_from_slice(&xid.to_be_bytes());
    m[12..16].copy_from_slice(&ciaddr.to_be_bytes());
    m[28..34].copy_from_slice(&chaddr);
    m[236..240].copy_from_slice(&0x6382_5363u32.to_be_bytes());
    m.extend_from_slice(&[53, 1, kind]);
    if let Some(r) = requested {
        m.push(50);
        m.push(4);
        m.extend_from_slice(&r.to_be_bytes());
    }
    m.push(255);
    m
}

pub(crate) fn udp(sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let mut u = Vec::with_capacity(8 + payload.len());
    u.extend_from_slice(&sport.to_be_bytes());
    u.extend_from_slice(&dport.to_be_bytes());
    u.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    u.extend_from_slice(&[0, 0]);
    u.extend_from_slice(payload);
    u
}

/// Who holds a port, as the model has it.
#[derive(Clone, Copy, PartialEq)]
enum Owner {
    Free,
    /// A host network's VM, alone.
    Host,
    Vnet(Identity),
}

/// How an entry waits in an inbox.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Done,
    Partial(usize, usize),
    Whole,
}

struct Port {
    owner: Owner,
    /// Shut by hv (`Switch::shut`): nothing from it or to it.
    shut: bool,
    place: usize,
    queue: VecDeque<(Vec<u8>, Kind)>,
    woken: u64,
    takes_partial: bool,
    takes_whole: bool,
    counts: switch::PortCounts,
    refused: u32,
    told_big: u32,
    on_backlog: usize,
    drain: u64,
}

struct Model {
    ports: Vec<Port>,
    /// `hv0`'s: everything handed in, in order -- a guest's frames and the
    /// ring's answers -- and the ring.
    hv0_in: Vec<Vec<u8>>,
    ring: VecDeque<Vec<u8>>,
    to_host: u64,
    refused: u64,
    dhcp: u64,
    foreign: u64,
    arps: u64,
    echoes: u64,
    /// The cluster networks with ports here: number, subnet, mask, out.
    here: Vec<(u32, u32, u32, bool, usize)>,
    table: Vec<(u32, u32)>,
    /// What went into the tunnel: network, to, the packet -- while the
    /// switch has one (`carrying`).
    carried: Vec<(u32, u32, Vec<u8>)>,
    carrying: bool,
    /// The ports moved away -- network, address, the host they went to --
    /// the oldest first, at most `MOVED_MAX`; and what was carried on to
    /// them, as `carried`.
    moved: Vec<(u32, u32, u64)>,
    carried_on: Vec<(u64, u32, Vec<u8>)>,
}

/// The switch's marks of ports moved away (`net::MOVED_MAX`).
const MOVED_MAX: usize = 8;

impl Model {
    fn new() -> Model {
        let ports = (0..MAX_PORTS).map(|_| Port {
            owner: Owner::Free, shut: false, place: ALLOW_ALL, queue: VecDeque::new(), woken: 0, takes_partial: false,
            takes_whole: false, counts: switch::PortCounts::default(), refused: 0, told_big: 0, on_backlog: 0,
            drain: 0,
        }).collect();
        Model { ports, hv0_in: Vec::new(), ring: VecDeque::new(), to_host: 0, refused: 0, dhcp: 0, foreign: 0,
                arps: 0, echoes: 0, here: Vec::new(), table: Vec::new(), carried: Vec::new(),
                carrying: false, moved: Vec::new(), carried_on: Vec::new() }
    }

    /// The host port `addr` of network `vni` moved to, while marked.
    fn moved_to(&self, vni: u32, addr: u32) -> Option<u64> {
        self.moved.iter().find(|m| m.0 == vni && m.1 == addr).map(|m| m.2)
    }

    /// Port `addr` of network `vni` moved to host `node`: its mark anew, the
    /// oldest let go past the switch's room for them. No mark runs out in
    /// an input: ten seconds of the clock are more reads than one makes.
    fn mark(&mut self, vni: u32, addr: u32, node: u64) {
        self.moved.retain(|m| !(m.0 == vni && m.1 == addr));
        self.moved.push((vni, addr, node));
        if self.moved.len() > MOVED_MAX {
            self.moved.remove(0);
        }
    }

    fn in_use(&self) -> usize {
        self.ports.iter().filter(|p| p.owner != Owner::Free).count()
    }

    fn vnet(&self, p: usize) -> Option<Identity> {
        match self.ports[p].owner {
            Owner::Vnet(i) => Some(i),
            _ => None,
        }
    }

    /// Whether `addr` of network `vni` is a port on another host the
    /// switch's tunnel knows -- while it has one.
    fn elsewhere(&self, vni: u32, addr: u32) -> bool {
        self.carrying && REMOTE.contains(&(vni, addr))
    }

    /// The cluster network's port here whose address `ip` is.
    fn port_at(&self, ip: u32) -> Option<usize> {
        (0..MAX_PORTS).find(|&q| self.vnet(q).is_some_and(|i| i.ip == ip))
    }

    fn mac_of(&self, p: usize) -> [u8; 6] {
        self.vnet(p).map_or(host_mac(p), |i| mac_for(i.ip))
    }

    /// A claim on a cluster network as `who`: the port it has, or None
    /// where the switch must refuse it.
    fn claim_vnet(&mut self, who: &Identity, place: usize) -> Option<usize> {
        let m = mask(who.prefix);
        let gw = who.net + 1;
        let valid = (16..=28).contains(&who.prefix) && who.net & !m == 0 && who.ip & m == who.net
            && who.ip != who.net && who.ip != gw && who.ip != who.net | !m && (576..=1440).contains(&who.mtu)
            && who.vni != 0 && who.vni < 1 << 24;
        if !valid || self.port_at(who.ip).is_some() {
            return None;
        }
        match self.here.iter().position(|h| h.0 == who.vni) {
            Some(at) => {
                let h = self.here[at];
                if h.1 != who.net || h.2 != m || h.3 != who.out {
                    return None;
                }
            }
            None => {
                if self.here.iter().any(|h| h.1 & h.2 & m == who.net & h.2 & m) {
                    return None;
                }
            }
        }
        let p = (0..MAX_PORTS).find(|&p| self.ports[p].owner == Owner::Free)?;
        match self.here.iter_mut().find(|h| h.0 == who.vni) {
            Some(h) => h.4 += 1,
            None => self.here.push((who.vni, who.net, m, who.out, 1)),
        }
        self.take(p, Owner::Vnet(*who), place);
        Some(p)
    }

    fn take(&mut self, p: usize, owner: Owner, place: usize) {
        let port = &mut self.ports[p];
        port.owner = owner;
        port.shut = false;
        port.place = place;
        port.queue.clear();
        port.woken = 0;
        port.takes_partial = false;
        port.takes_whole = false;
        port.counts = switch::PortCounts::default();
        port.refused = 0;
        port.told_big = 0;
        port.on_backlog = 0;
    }

    /// Port `p` given up: how many times its VM was woken.
    fn release(&mut self, p: usize) -> u64 {
        if let Owner::Vnet(i) = self.ports[p].owner {
            if let Some(at) = self.here.iter().position(|h| h.0 == i.vni) {
                self.here[at].4 -= 1;
                if self.here[at].4 == 0 {
                    self.here.swap_remove(at);
                }
            }
        }
        let port = &mut self.ports[p];
        let woken = port.woken;
        port.owner = Owner::Free;
        port.queue.clear();
        woken
    }

    /// Into port `q`'s inbox, if a VM has it, it is open, and there is
    /// room.
    fn deliver(&mut self, q: usize, frame: Vec<u8>, kind: Kind) {
        let port = &mut self.ports[q];
        if port.owner == Owner::Free || port.shut {
            return;
        }
        let wholes = port.queue.iter().filter(|(_, k)| *k == Kind::Whole).count();
        if port.queue.len() == INBOX || (kind == Kind::Whole && wholes == WHOLE) {
            port.counts.dropped += 1;
            seen(Seen::Full);
            return;
        }
        port.queue.push_back((frame, kind));
        port.woken += 1;
        seen(match kind {
            Kind::Done => Seen::Delivered,
            Kind::Partial(..) => Seen::DeliveredPartial,
            Kind::Whole => Seen::DeliveredWhole,
        });
    }

    /// Whether port `q`'s firewall lets `packet`, an IPv4 packet, go `out`
    /// of it or into it: every one but the host's own is judged, and the
    /// one in every place lets all through or none. One kept from going out
    /// is answered "administratively prohibited" from its gateway, its
    /// first `TOLD` -- never one to a group.
    fn judged(&mut self, q: usize, out: bool, frame: &[u8]) -> bool {
        let packet = &frame[ETH..];
        let peer = if out { be32(packet, 16) } else { be32(packet, 12) };
        if peer == HOST_IP || self.ports[q].place == ALLOW_ALL {
            return true;
        }
        let port = &mut self.ports[q];
        if out {
            port.counts.denied_out += 1;
            seen(Seen::DeniedOut);
            let to = be32(packet, 16);
            let group = to == u32::MAX || to == HOST_SUBNET | 0xFF || to >> 28 == 0xE;
            if !group && port.refused < TOLD {
                port.refused += 1;
                seen(Seen::Refusal);
                let reply = self.unreachable(q, frame, 13, 0);
                self.deliver(q, reply, Kind::Done);
            }
        } else {
            port.counts.denied_in += 1;
            seen(Seen::DeniedIn);
        }
        false
    }

    /// "Destination unreachable" of `code` to port `q`'s VM about `frame`,
    /// from its gateway: the packet's header and eight bytes quoted, and
    /// `mtu` where it says one.
    fn unreachable(&self, q: usize, frame: &[u8], code: u8, mtu: u16) -> Vec<u8> {
        let i = self.vnet(q).expect("a cluster network's port");
        let packet = &frame[ETH..];
        let ihl = usize::from(packet[0] & 0xF) * 4;
        let quoted = &packet[..(ihl + 8).min(packet.len())];
        let mut word = [0u8; 4];
        word[2..].copy_from_slice(&mtu.to_be_bytes());
        let m = icmp(3, code, word, quoted);
        eth(mac_for(i.ip), HOST_MAC, 0x0800, &ipv4(i.net + 1, i.ip, 1, 0, 0, &m))
    }

    /// Onto `hv0`'s backlog from port `p`, within its share.
    fn to_host(&mut self, p: usize, frame: &[u8]) {
        let drains = vnic::DRAINS.load(std::sync::atomic::Ordering::Relaxed);
        let share = BACKLOG / self.in_use().max(1);
        let port = &mut self.ports[p];
        if port.drain != drains {
            port.drain = drains;
            port.on_backlog = 0;
        }
        if port.on_backlog >= share {
            port.counts.over_share += 1;
            seen(Seen::OverShare);
            return;
        }
        port.on_backlog += 1;
        self.to_host += 1;
        seen(Seen::ToHost);
        self.hv0_in.push(frame.to_vec());
    }

    /// Past the MTU: counted, and told where it said not to cut it.
    fn too_big(&mut self, p: usize, frame: &[u8]) {
        let i = self.vnet(p).expect("a cluster network's port");
        self.ports[p].counts.oversize += 1;
        seen(Seen::TooBig);
        let df = be16(&frame[ETH..], 6) & 0x4000 != 0;
        if df && self.ports[p].told_big < TOLD {
            self.ports[p].told_big += 1;
            seen(Seen::TooBigTold);
            let reply = self.unreachable(p, frame, 4, i.mtu);
            self.deliver(p, reply, Kind::Done);
        }
    }

    fn in_cluster(&self, ip: u32) -> bool {
        self.table.iter().any(|&(n, m)| ip & m == n) || self.here.iter().any(|h| ip & h.2 == h.1)
    }

    /// What the guest on cluster port `p` sending `frame` comes to.
    fn forward_vnet(&mut self, p: usize, frame: &[u8]) {
        let i = self.vnet(p).expect("a cluster network's port");
        if frame.len() < ETH {
            return;
        }
        if self.ports[p].shut {
            self.ports[p].counts.isolated += 1;
            seen(Seen::Isolated);
            return;
        }
        let me = mac_for(i.ip);
        let ethertype = be16(frame, 12);
        let body = &frame[ETH..];
        /* Its own, or not -- and DHCP's, the switch's to answer */
        let own = match ethertype {
            0x0800 if body.len() >= IP => {
                let src = be32(body, 12);
                let dhcp = body.len() >= IP + 8 && body[9] == 17 && be16(body, IP) == 68 && be16(body, IP + 2) == 67;
                if dhcp && (src == 0 || src == i.ip) {
                    if frame[6..12] != me {
                        self.ports[p].counts.spoofed += 1;
            seen(Seen::Spoofed);
                        return;
                    }
                    self.answer_dhcp(p, &i, frame);
                    return;
                }
                src == i.ip
            }
            0x0806 if body.len() >= ARP_BYTES => {
                let spa = be32(body, 14);
                body[8..14] == me && (spa == 0 || spa == i.ip)
            }
            _ => {
                self.foreign += 1;
                seen(Seen::Foreign);
                return;
            }
        };
        if !own || frame[6..12] != me {
            self.ports[p].counts.spoofed += 1;
            seen(Seen::Spoofed);
            return;
        }
        if ethertype == 0x0806 {
            /* Asked of the switch, which answers for the gateway and a port
             * of the network here, and for nothing else. */
            if be16(body, 6) != 1 {
                return;
            }
            let tpa = be32(body, 24);
            let gw = i.net + 1;
            let mac = if tpa == gw {
                HOST_MAC
            } else if tpa != i.ip && (self.port_at(tpa).is_some_and(|q| self.vnet(q).is_some_and(|o| o.vni == i.vni))
                || self.elsewhere(i.vni, tpa))
            {
                mac_for(tpa)
            } else {
                return;
            };
            let reply = eth(me, mac, 0x0806, &arp(2, mac, tpa, me, be32(body, 14)));
            self.arps += 1;
            seen(Seen::ArpAnswered);
            self.deliver(p, reply, Kind::Done);
            return;
        }
        /* IPv4 within the MTU */
        let total = usize::from(be16(body, 2));
        if body.len() > usize::from(i.mtu) || total > usize::from(i.mtu) {
            self.too_big(p, frame);
            return;
        }
        let to = be32(body, 16);
        if frame[0] & 1 != 0 {
            self.ports[p].counts.isolated += 1;
            seen(Seen::Isolated);
            return;
        }
        let gw = i.net + 1;
        if frame[..6] == HOST_MAC {
            if to == gw {
                if !self.echo(p, &i, frame) {
                    self.ports[p].counts.isolated += 1;
            seen(Seen::Isolated);
                }
                return;
            }
            let group = to == u32::MAX || to == HOST_SUBNET | 0xFF || to >> 28 == 0xE;
            if to != HOST_IP && (to & HOST_MASK == HOST_SUBNET || group || self.in_cluster(to) || !i.out) {
                self.ports[p].counts.isolated += 1;
            seen(Seen::Isolated);
                return;
            }
            if self.judged(p, true, frame) {
                self.to_host(p, frame);
            }
            return;
        }
        /* A port moved away, at its MAC: carried on to its host, judged
         * going out -- with no tunnel, dropped */
        if frame[0] == 0x02 && frame[1] == 0x00 && be32(frame, 2) == to {
            if let Some(node) = self.moved_to(i.vni, to) {
                if self.carrying && self.judged(p, true, frame) {
                    let end = (ETH + total).min(frame.len());
                    self.carried_on.push((node, i.vni, frame[ETH..end].to_vec()));
                    seen(Seen::CarriedOn);
                }
                return;
            }
        }
        let q = if frame[0] == 0x02 && frame[1] == 0x00 { self.port_at(be32(frame, 2)) } else { None };
        let Some(q) = q else {
            /* A port of its network on another host, at its MAC: carried,
             * judged going out */
            if frame[0] == 0x02 && frame[1] == 0x00 && be32(frame, 2) == to && self.elsewhere(i.vni, to) {
                if self.judged(p, true, frame) {
                    let end = (ETH + total).min(frame.len());
                    self.carried.push((i.vni, to, frame[ETH..end].to_vec()));
                    seen(Seen::Carried);
                }
                return;
            }
            self.ports[p].counts.misdirected += 1;
            seen(Seen::Misdirected);
            return;
        };
        if q == p {
            return;
        }
        let theirs = self.vnet(q).expect("a cluster network's port");
        if theirs.vni != i.vni {
            self.ports[p].counts.isolated += 1;
            seen(Seen::Isolated);
            return;
        }
        if theirs.ip != to {
            self.ports[p].counts.misdirected += 1;
            seen(Seen::Misdirected);
            return;
        }
        if self.judged(p, true, frame) && self.judged(q, false, frame) {
            self.deliver(q, frame.to_vec(), Kind::Done);
        }
    }

    /// The echo the gateway answers: whether `frame` was a request.
    fn echo(&mut self, p: usize, i: &Identity, frame: &[u8]) -> bool {
        let body = &frame[ETH..];
        let ihl = usize::from(body[0] & 0xF) * 4;
        let total = usize::from(be16(body, 2));
        let fragment = be16(body, 6) & 0x3FFF != 0;
        if ihl < IP || body[9] != 1 || fragment || total < ihl + 8 || total > body.len() {
            return false;
        }
        let msg = &body[ihl..total];
        if msg[0] != 8 || msg[1] != 0 {
            return false;
        }
        let reply = icmp(0, 0, [msg[4], msg[5], msg[6], msg[7]], &msg[8..]);
        let frame = eth(mac_for(i.ip), HOST_MAC, 0x0800, &ipv4(i.net + 1, i.ip, 1, 0, 0, &reply));
        self.echoes += 1;
        seen(Seen::Echoed);
        self.deliver(p, frame, Kind::Done);
        true
    }

    fn answer_dhcp(&mut self, p: usize, i: &Identity, frame: &[u8]) {
        let net = dhcp::Network { server_ip: i.net + 1, server_mac: HOST_MAC, mask: mask(i.prefix),
                                  dns: network::dns::upstream(), mtu: i.mtu };
        let mut reply = [0u8; dhcp::REPLY_MAX];
        match dhcp::answer(frame, i.ip, &net, &mut reply) {
            Some(len) => {
                self.dhcp += 1;
                seen(Seen::Dhcp);
                self.deliver(p, reply[..len].to_vec(), Kind::Done);
            }
            None => self.foreign += 1,
        }
    }

    /// What `hv0` sending `frame` comes to: for a cluster network's port
    /// here, carried as the plan says; anything else the host networks'
    /// way, which here is the ports alone's -- an ARP for one, a frame to
    /// one's MAC, a broadcast to every one.
    fn from_host(&mut self, frame: &[u8]) {
        let dst: [u8; 6] = frame[..6].try_into().expect("a header");
        let host_port = dst[..5] == [0x02, 0, 0, 0, 0x64] && (2..2 + MAX_PORTS as u8).contains(&dst[5]);
        let vnet_port = if !host_port && dst != HOST_MAC && dst[0] == 0x02 && dst[1] == 0 {
            self.port_at(be32(&dst, 2))
        } else {
            None
        };
        let ethertype = be16(frame, 12);
        if ethertype == 0x0806 && frame.len() >= ETH + ARP_BYTES {
            let a = &frame[ETH..];
            let tpa = be32(a, 24);
            if self.port_at(tpa).is_some() {
                if be16(a, 6) == 1 {
                    let mac = mac_for(tpa);
                    let sha: [u8; 6] = a[8..14].try_into().expect("a MAC");
                    let reply = eth(sha, mac, 0x0806, &arp(2, mac, tpa, sha, be32(a, 14)));
                    if self.ring.len() == RING {
                        self.refused += 1;
                    } else {
                        self.ring.push_back(reply);
                        self.arps += 1;
                        seen(Seen::HostArpQueued);
                    }
                }
                return;
            }
            if vnet_port.is_some() {
                return;
            }
        } else if ethertype == 0x0800 && frame.len() >= ETH + IP {
            if let Some(q) = vnet_port {
                let ip = self.vnet(q).expect("a cluster network's port").ip;
                if be32(frame, ETH + 16) == ip && self.judged(q, false, frame) {
                    self.deliver(q, frame.to_vec(), Kind::Done);
                }
                return;
            }
        } else if vnet_port.is_some() {
            return;
        }
        /* The host networks' way: to the port whose MAC it is -- a free
         * one's to nobody, a cluster network's to every one -- or to every
         * one. Each judged by its firewall, which lets in what is not IP,
         * the host's own, and to its address alone or a group's. */
        let to: Vec<usize> = if dst == HOST_MAC {
            vec![]
        } else if dst[0] & 1 == 0 && host_port {
            let k = usize::from(dst[5]) - 2;
            match self.ports[k].owner {
                Owner::Free => vec![],
                Owner::Host => vec![k],
                Owner::Vnet(_) => (0..MAX_PORTS).collect(),
            }
        } else {
            (0..MAX_PORTS).collect()
        };
        for q in to {
            if self.ports[q].owner != Owner::Host {
                continue;
            }
            if ethertype == 0x0800 && frame.len() >= ETH + IP {
                let (s, d) = (be32(frame, ETH + 12), be32(frame, ETH + 16));
                let group = d == u32::MAX || d == HOST_SUBNET | 0xFF || d >> 28 == 0xE;
                if s != HOST_IP && !group && d != host_ip(q) {
                    self.ports[q].counts.denied_in += 1;
                    continue;
                }
            }
            seen(Seen::HostPortFrame);
            self.deliver(q, frame.to_vec(), Kind::Done);
        }
    }
}

/// A frame from cluster port `p`'s guest: of every kind the plan names a
/// way for, and its own mostly.
fn frame(r: &mut Input, m: &Model, p: usize) -> Vec<u8> {
    let i = m.vnet(p).expect("a cluster network's port");
    let me = mac_for(i.ip);
    let gw = i.net + 1;
    let others: Vec<u32> = (0..MAX_PORTS).filter_map(|q| m.vnet(q)).map(|o| o.ip).collect();
    let other = if others.is_empty() { gw } else { r.pick(&others) };
    let src_mac = if r.u8() < 16 { r.pick(&[mac_for(other), HOST_MAC, host_mac(0)]) } else { me };
    match r.u8() % 8 {
        0 | 1 => {
            let near = i.net + 3 + r.below(4) as u32;
            let tpa = r.pick(&[gw, other, i.ip, near, HOST_IP, i.net, WORLD[0], REMOTE[0].1, REMOTE[2].1]);
            let spa = if r.u8() < 32 { r.pick(&[0, other]) } else { i.ip };
            let sha = if r.u8() < 16 { mac_for(other) } else { me };
            let op = if r.u8() < 32 { 2 } else { 1 };
            let dst = r.pick(&[BROADCAST, BROADCAST, HOST_MAC, mac_for(other)]);
            let mut a = arp(op, sha, spa, [0; 6], tpa);
            if r.u8() < 8 {
                a.truncate(r.below(ARP_BYTES as u64) as usize);
            }
            eth(dst, src_mac, 0x0806, &a)
        }
        2 => {
            let (kind, ciaddr, requested) = match r.u8() % 4 {
                0 => (1, 0, None),
                1 => (3, 0, Some(r.pick(&[i.ip, other]))),
                2 => (3, i.ip, None),
                _ => (8, r.pick(&[i.ip, other]), None),
            };
            let m = dhcp_message(kind, r.u32(), me, ciaddr, requested);
            let src = if kind == 3 && ciaddr != 0 || kind == 8 { ciaddr } else { 0 };
            eth(BROADCAST, src_mac, 0x0800, &ipv4(src, u32::MAX, 17, 0, 0, &udp(68, 67, &m)))
        }
        3..=5 => {
            let to = match r.u8() % 10 {
                0 => gw,
                1 => HOST_IP,
                2 => r.pick(&WORLD),
                3 | 4 => other,
                5 => i.net | !mask(i.prefix),
                6 => r.pick(&[0x0A00_6407, 0xAC10_0304, 0xE000_00FB, u32::MAX]),
                7 => i.net + 3 + r.below(8) as u32,
                8 => i.ip,
                9 if r.bool() => match m.moved.iter().filter(|x| x.0 == i.vni).map(|x| x.1).collect::<Vec<_>>() {
                    away if !away.is_empty() => r.pick(&away),
                    _ => other,
                },
                _ => match REMOTE.iter().filter(|(v, _)| *v == i.vni).map(|(_, a)| *a).collect::<Vec<_>>() {
                    mine if !mine.is_empty() && r.bool() => r.pick(&mine),
                    _ => r.pick(&[0x0A0A_0005, 0x0A14_0006, 0xC0A8_0507, REMOTE[0].1, REMOTE[2].1, REMOTE[3].1]),
                },
            };
            let dst = match r.u8() % 6 {
                0 | 1 => HOST_MAC,
                2 | 3 => mac_for(to),
                4 => mac_for(other),
                _ => r.pick(&[BROADCAST, host_mac(1), [0x01, 0x00, 0x5E, 0, 0, 0xFB]]),
            };
            let src = if r.u8() < 16 { r.pick(&[other, 0, gw]) } else { i.ip };
            /* Totals at each MTU, one past it, and further */
            let size = if r.u8() < 48 {
                r.pick(&[548usize, 549, 556, 1372, 1373, 1380, 1412, 1413, 1420, 1480])
            } else {
                r.below(64) as usize
            };
            let data: Vec<u8> = (0..size).map(|k| k as u8).collect();
            let df = if r.bool() { 0x4000 } else { 0 };
            let (proto, l4) = if r.bool() {
                (1, icmp(8, 0, [0, 7, 0, r.u8()], &data))
            } else {
                (17, udp(r.pick(&[53, 4000]), 9, &data))
            };
            let mut f = eth(dst, src_mac, 0x0800, &ipv4(src, to, proto, r.u16(), df, &l4));
            /* Bytes past the packet, as a frame padded to Ethernet's least */
            if r.u8() < 32 {
                f.extend((0..r.below(24)).map(|_| 0xA5));
            }
            f
        }
        6 => eth(r.pick(&[HOST_MAC, mac_for(other)]), src_mac, 0x86DD, &[0x60; 40]),
        _ => {
            let n = r.below(40) as usize;
            let mut f = eth(HOST_MAC, src_mac, r.pick(&[0x0800, 0x0806, 0x88B5]), &vec![0x45; n]);
            f.truncate(r.below(f.len() as u64 + 1) as usize);
            f
        }
    }
}

/// `hv0`'s frame: an ARP after anyone's address, a packet to a cluster
/// port's MAC -- for its address or not, from the host or the world -- or
/// to a MAC that is no port's, IPv6 to one.
fn host_frame(r: &mut Input, m: &Model) -> Vec<u8> {
    let ips: Vec<u32> = (0..MAX_PORTS).filter_map(|q| m.vnet(q)).map(|o| o.ip).collect();
    let ip = if ips.is_empty() { 0x0A0A_0002 } else { r.pick(&ips) };
    let slot = r.below(MAX_PORTS as u64) as usize;
    let mac = r.pick(&[mac_for(ip), mac_for(ip), mac_for(0x0A0A_00EE), host_mac(slot)]);
    match r.u8() % 4 {
        0 => {
            let host = host_ip(r.below(4) as usize);
            let tpa = r.pick(&[ip, ip, host, 0x0A0A_00EE]);
            let dst = if r.u8() < 200 { BROADCAST } else { mac };
            eth(dst, HOST_MAC, 0x0806, &arp(if r.u8() < 16 { 2 } else { 1 }, HOST_MAC, HOST_IP, [0; 6], tpa))
        }
        1 | 2 => {
            let to = if r.u8() < 200 { ip } else { r.pick(&[0x0A0A_00EE, host_ip(1)]) };
            let from = r.pick(&[HOST_IP, WORLD[1]]);
            let data: Vec<u8> = (0..r.below(200)).map(|k| k as u8).collect();
            eth(mac, HOST_MAC, 0x0800, &ipv4(from, to, 17, 0, 0, &udp(53, 4000, &data)))
        }
        /* IPv6 to a cluster port's MAC, which goes nowhere: a host
         * network's port's firewall would read it, which is another
         * target's to model. */
        _ if !ips.is_empty() => eth(mac_for(ip), HOST_MAC, 0x86DD, &[0x60; 40]),
        _ => eth(BROADCAST, HOST_MAC, 0x0806, &arp(1, HOST_MAC, HOST_IP, [0; 6], ip)),
    }
}

/// An identity of `NETS[n]`'s, at `host` of its subnet -- now and then one
/// the switch must refuse.
fn identity(r: &mut Input, n: usize, host: u32) -> Identity {
    let net = NETS[n];
    let mut who = Identity { ip: net.net + host, vni: net.vni, net: net.net, prefix: net.prefix, mtu: net.mtu,
                             out: net.out };
    match r.u8() {
        0..=7 => who.ip = net.net + 1,
        8..=11 => who.ip = net.net,
        12..=15 => who.mtu = 500,
        16..=19 => who.ip = 0x0A00_6409,
        20..=23 => who.prefix = 30,
        _ => {}
    }
    who
}

/// The switch's cluster networks against the model.
pub fn vnetswitch_target(r: &mut Input) {
    use std::sync::atomic::Ordering::Relaxed;
    network::reset();
    network::dns::UPSTREAM.store(r.pick(&[0u32, 0x0808_0808]), Relaxed);
    let sw = Arc::new(Switch::new().expect("a switch"));
    let mut rules = firewall::Rules::DEFAULT;
    rules.inbound = firewall::Action::Allow;
    rules.outbound = firewall::Action::Allow;
    sw.install(ALLOW_ALL, &rules).expect("a place");
    let mut none = firewall::Rules::DEFAULT;
    none.inbound = firewall::Action::Deny;
    none.outbound = firewall::Action::Deny;
    sw.install(DENY_ALL, &none).expect("a place");
    let mut m = Model::new();
    let mut vms: Vec<Option<Arc<Shared>>> = vec![None; MAX_PORTS];
    let mut backends: Vec<Option<switch::PortBackend>> = (0..MAX_PORTS).map(|_| None).collect();
    let tunnel = Arc::new(Tunnel { carried: std::sync::Mutex::new(Vec::new()),
                                   carried_on: std::sync::Mutex::new(Vec::new()) });
    let carrier: Arc<dyn switch::Carrier> = tunnel.clone();
    let carrying = r.u8() < 200;
    if carrying {
        invariant!(sw.carrier(Some(carrier)).is_none(), "a carrier there already");
    }
    m.carrying = carrying;
    let mut relayed = 0u64;

    while let Some(op) = r.op(15) {
        if !carrying && op == 13 {
            continue;
        }
        match op {
            0 | 1 => {
                /* A VM on a cluster network, as vnet made it */
                let n = r.below(NETS.len() as u64) as usize;
                let at = 2 + r.below(9) as u32;
                let who = identity(r, n, at);
                let place = if r.u8() < 48 { DENY_ALL } else { ALLOW_ALL };
                let vm = Arc::new(Shared::default());
                let got = sw.claim_vnet(&vm, &who, place).ok();
                let want = m.claim_vnet(&who, place);
                if want.is_some() {
                    /* Its address here again: carried on no more */
                    m.moved.retain(|x| !(x.0 == who.vni && x.1 == who.ip));
                }
                seen(if want.is_some() { Seen::Claimed } else { Seen::Refused });
                invariant!(got == want, "a claim as {:?} came to port {:?}, the model's {:?}", who, got, want);
                if let Some(p) = got {
                    invariant!(sw.ip_of(p) == who.ip && sw.mac_of(p) == mac_for(who.ip)
                               && sw.vnet_of(p) == Some(who.vni),
                               "port {} is {:#x} at {:?} on {:?}", p, sw.ip_of(p), sw.mac_of(p), sw.vnet_of(p));
                    let mut b = sw.backend(p);
                    let (partial, whole) = (r.bool(), r.bool());
                    if partial || whole {
                        b.takes(offload::Takes { partial, whole });
                        m.ports[p].takes_partial = partial;
                        m.ports[p].takes_whole = whole;
                    }
                    vms[p] = Some(vm);
                    backends[p] = Some(b);
                }
            }
            2 => {
                /* A host network's VM, alone */
                let vm = Arc::new(Shared::default());
                let got = sw.claim(&vm, None, ALLOW_ALL).ok();
                let want = (0..MAX_PORTS).find(|&p| m.ports[p].owner == Owner::Free);
                invariant!(got == want, "a host network's claim came to {:?}, the model's {:?}", got, want);
                if let Some(p) = got {
                    invariant!(sw.ip_of(p) == host_ip(p) && sw.mac_of(p) == host_mac(p) && sw.vnet_of(p).is_none(),
                               "host port {} is {:#x}", p, sw.ip_of(p));
                    m.take(p, Owner::Host, ALLOW_ALL);
                    vms[p] = Some(vm);
                    backends[p] = Some(sw.backend(p));
                }
            }
            3 => {
                let p = r.below(MAX_PORTS as u64) as usize;
                sw.release(p);
                backends[p] = None;
                let woken = m.release(p);
                if let Some(vm) = vms[p].take() {
                    invariant!(vm.woken.load(Relaxed) == woken, "port {}'s VM woken {} times, the model {}", p,
                               vm.woken.load(Relaxed), woken);
                }
                invariant!(sw.ip_of(p) == host_ip(p) && sw.vnet_of(p).is_none(), "free port {} is {:#x}", p,
                           sw.ip_of(p));
            }
            4..=6 => {
                /* A cluster port's guest sends -- a frame, mostly, or a
                 * burst of one */
                let p = r.below(MAX_PORTS as u64) as usize;
                if m.vnet(p).is_none() {
                    continue;
                }
                let f = frame(r, &m, p);
                let n = if r.u8() < 8 { 1 + r.below(40) } else { 1 };
                for _ in 0..n {
                    m.forward_vnet(p, &f);
                    if let Some(b) = backends[p].as_mut() {
                        b.send(&f);
                    }
                }
            }
            7 => {
                /* A send left to the switch to finish or to cut, to another
                 * port of the network here */
                let p = r.below(MAX_PORTS as u64) as usize;
                let Some(i) = m.vnet(p) else { continue };
                let peers: Vec<usize> = (0..MAX_PORTS).filter(|&q| q != p && m.vnet(q).is_some_and(|o| o.vni == i.vni))
                    .collect();
                if peers.is_empty() {
                    continue;
                }
                let q = r.pick(&peers);
                let to = m.vnet(q).expect("a peer").ip;
                send_offloaded(r, &mut m, &mut backends, p, &i, q, to);
            }
            8 => {
                let f = host_frame(r, &m);
                let n = if r.u8() < 8 { 1 + r.below(300) } else { 1 };
                for _ in 0..n {
                    m.from_host(&f);
                    if let Some(sink) = vnic::sink() {
                        sink.on_frame(&f);
                    }
                }
            }
            9 => {
                /* A host network's guest sending to a cluster port: isolated */
                let p = r.below(MAX_PORTS as u64) as usize;
                if m.ports[p].owner != Owner::Host {
                    continue;
                }
                let Some(q) = (0..MAX_PORTS).find(|&q| m.vnet(q).is_some()) else { continue };
                let ip = m.vnet(q).expect("a port").ip;
                let f = eth(mac_for(ip), host_mac(p), 0x0800, &ipv4(host_ip(p), ip, 17, 0, 0, &udp(1, 2, &[0; 8])));
                m.ports[p].counts.isolated += 1;
                if let Some(b) = backends[p].as_mut() {
                    b.send(&f);
                }
            }
            13 => {
                /* A packet out of the tunnel: from a port on another host,
                 * for a port here -- of its network, or another's, or none */
                let Some(q) = (0..MAX_PORTS).find(|&q| m.vnet(q).is_some()) else { continue };
                let i = m.vnet(q).expect("a port");
                let (vni, from) = *REMOTE.iter().find(|(v, _)| *v == i.vni).unwrap_or(&REMOTE[0]);
                let vni = if r.u8() < 32 { r.pick(&[1u32, 2, 3, 9]) } else { vni };
                let away: Vec<u32> = m.moved.iter().filter(|x| x.0 == vni).map(|x| x.1).collect();
                let to = if !away.is_empty() && r.u8() < 64 {
                    r.pick(&away)
                } else if r.u8() < 32 {
                    r.pick(&[0x0A0A_00EE, i.ip ^ 1])
                } else {
                    i.ip
                };
                let data: Vec<u8> = (0..r.below(80)).map(|k| k as u8).collect();
                let packet = ipv4(from, to, 17, r.u16(), 0, &udp(5000, 7, &data));
                /* A port moved away first -- carried on to its host -- then
                 * one here */
                let moved = m.moved_to(vni, to);
                let here = m.port_at(to).filter(|&k| m.vnet(k).is_some_and(|o| o.vni == vni));
                let got = sw.from_tunnel(vni, &packet);
                invariant!(got == (moved.is_some() || here.is_some()), "a packet out of the tunnel for {:#x} of \
                           network {} taken {}, the model's {:?} here, {:?} moved", to, vni, got, here, moved);
                if let Some(node) = moved {
                    m.carried_on.push((node, vni, packet.clone()));
                    seen(Seen::CarriedOn);
                } else if let Some(k) = here {
                    let frame = eth(mac_for(to), mac_for(from), 0x0800, &packet);
                    if m.judged(k, false, &frame) {
                        seen(Seen::FromTunnel);
                        m.deliver(k, frame, Kind::Done);
                    }
                }
            }
            14 => {
                /* A cluster port's VM moved to another host: its port given
                 * back here, and what comes for its address carried on to
                 * that host a while (`moved_away`) */
                let p = r.below(MAX_PORTS as u64) as usize;
                let Some(i) = m.vnet(p) else { continue };
                sw.release(p);
                backends[p] = None;
                let woken = m.release(p);
                if let Some(vm) = vms[p].take() {
                    invariant!(vm.woken.load(Relaxed) == woken, "port {}'s VM woken {} times, the model {}", p,
                               vm.woken.load(Relaxed), woken);
                }
                let node = r.pick(&[7u64, 8, 9]);
                sw.moved_away(i.vni, i.ip, node);
                m.mark(i.vni, i.ip, node);
            }
            12 => {
                /* A port hv shuts: its cluster network's port revoked */
                let p = r.below(MAX_PORTS as u64) as usize;
                if m.vnet(p).is_none() {
                    continue;
                }
                sw.shut(p);
                m.ports[p].shut = true;
                invariant!(sw.is_shut(p), "port {} not shut", p);
            }
            10 => {
                /* vnet's table of the cluster's networks */
                let mut table = Vec::new();
                for &(net, prefix) in TABLE.iter() {
                    if r.bool() {
                        table.push((net, prefix));
                    }
                }
                invariant!(sw.cluster_networks(&table), "a table not taken");
                m.table = table.iter().map(|&(n, pfx)| (n, mask(pfx))).collect();
            }
            _ => {
                /* What the inboxes hold, `hv0`'s ring handed in, what `hv0`
                 * was handed */
                for p in 0..MAX_PORTS {
                    let Some(b) = backends[p].as_mut() else { continue };
                    loop {
                        let mut buf = vec![0u8; SEND_MAX];
                        let got = b.recv_with(&mut buf);
                        let want = m.ports[p].queue.pop_front();
                        match (got, want) {
                            (None, None) => break,
                            (Some((n, rx)), Some((w, kind))) => {
                                invariant!(buf[..n] == w[..], "port {} took {} bytes {:02x?}, the model's {} {:02x?}",
                                           p, n, &buf[..n.min(64)], w.len(), &w[..w.len().min(64)]);
                                let same = match (rx, kind) {
                                    (offload::Rx::Done, Kind::Done) | (offload::Rx::Whole(_), Kind::Whole) => true,
                                    (offload::Rx::Partial(q), Kind::Partial(start, offset)) => {
                                        let h = q.header();
                                        usize::from(h.csum_start) == start && usize::from(h.csum_offset) == offset
                                    }
                                    _ => false,
                                };
                                invariant!(same, "port {} took a frame as {:?}, the model {:?}", p, rx, kind);
                            }
                            (got, want) => panic!("invariant: port {} took {:?}, the model has {:?} waiting", p,
                                                  got.map(|(n, _)| n), want.map(|(w, _)| w.len())),
                        }
                    }
                }
                let handed = sw.hand_in();
                invariant!(handed == m.ring.len(), "{} answers handed in, the model's {}", handed, m.ring.len());
                m.hv0_in.extend(m.ring.drain(..));
                let got = network::take_hv0_in();
                invariant!(got == m.hv0_in, "hv0 was handed {} frames, the model {}: {:02x?} against {:02x?}",
                           got.len(), m.hv0_in.len(), got.iter().map(|f| f.len()).collect::<Vec<_>>(),
                           m.hv0_in.iter().map(|f| f.len()).collect::<Vec<_>>());
                m.hv0_in.clear();
            }
        }
        for p in 0..MAX_PORTS {
            if m.ports[p].owner == Owner::Free {
                continue;
            }
            let c = sw.counts(p);
            let c = switch::PortCounts { evicted: 0, flows: 0, ..c };
            invariant!(c == m.ports[p].counts, "port {} counts {:?}, the model {:?}", p, c, m.ports[p].counts);
        }
        let (to_host, refused, dhcp, foreign) = sw.host_counts();
        invariant!((to_host, refused, dhcp, foreign) == (m.to_host, m.refused, m.dhcp, m.foreign),
                   "hv0 took {}, refused {}, DHCP {}, foreign {}; the model {}, {}, {}, {}", to_host, refused, dhcp,
                   foreign, m.to_host, m.refused, m.dhcp, m.foreign);
        let carried = std::mem::take(&mut *tunnel.carried.lock().unwrap_or_else(|e| e.into_inner()));
        invariant!(carried == m.carried, "carried {:?}, the model {:?}",
                   carried.iter().map(|(v, t, p)| (v, t, p.len())).collect::<Vec<_>>(),
                   m.carried.iter().map(|(v, t, p)| (v, t, p.len())).collect::<Vec<_>>());
        m.carried.clear();
        let on = std::mem::take(&mut *tunnel.carried_on.lock().unwrap_or_else(|e| e.into_inner()));
        invariant!(on == m.carried_on, "carried on {:?}, the model {:?}",
                   on.iter().map(|(n, v, p)| (n, v, p.len())).collect::<Vec<_>>(),
                   m.carried_on.iter().map(|(n, v, p)| (n, v, p.len())).collect::<Vec<_>>());
        relayed += on.len() as u64;
        invariant!(sw.relayed() == relayed, "{} carried on, the switch says {}", relayed, sw.relayed());
        m.carried_on.clear();
        let (arps, echoes) = sw.vnet_counts();
        invariant!((arps, echoes) == (m.arps, m.echoes), "ARPs {} and echoes {} answered, the model {} and {}", arps,
                   echoes, m.arps, m.echoes);
    }
}

/// A send of port `p`'s guest that it left the switch to finish -- a
/// checksum, or a send of TCP whole -- to port `q`'s address on its network.
fn send_offloaded(r: &mut Input, m: &mut Model, backends: &mut [Option<switch::PortBackend>], p: usize,
                  i: &Identity, q: usize, to: u32) {
    let me = mac_for(i.ip);
    let spoofed = r.u8() < 16;
    let src_mac = if spoofed { HOST_MAC } else { me };
    if r.bool() {
        /* A datagram whose checksum is left to finish */
        let payload = r.pick(&[0usize, 100, 1372, 1500]);
        let mut f = crate::offloadmodel::build(r, 17, 20, 20, payload, 0x0800, 0x4000, 0x18, true);
        f[..6].copy_from_slice(&mac_for(to));
        f[6..12].copy_from_slice(&src_mac);
        f[ETH + 12..ETH + 16].copy_from_slice(&i.ip.to_be_bytes());
        f[ETH + 16..ETH + 20].copy_from_slice(&to.to_be_bytes());
        let (start, offset) = (ETH + IP, 6);
        let Ok(partial) = offload::Partial::check(f.len(), start as u16, offset as u16) else { return };
        let mut done = f.clone();
        crate::offloadmodel::finish(&mut done, start, offset);
        /* As it is to the port, where its own and within the MTU -- past
         * it, told of as it is; not its own, finished and gone the way
         * every frame goes. */
        let fits = f.len() - ETH <= usize::from(i.mtu);
        if spoofed || m.ports[p].shut {
            m.forward_vnet(p, &done);
        } else if !fits {
            m.too_big(p, &f);
        } else if m.judged(p, true, &f) && m.judged(q, false, &f) {
            if m.ports[q].takes_partial {
                m.deliver(q, f.clone(), Kind::Partial(start, offset));
            } else {
                m.deliver(q, done.clone(), Kind::Done);
            }
        }
        if let Some(b) = backends[p].as_mut() {
            b.send_partial(&mut f, partial);
        }
        return;
    }
    /* A send of TCP whole, its frames cut to an MSS within the MTU or past */
    let payload = r.pick(&[300usize, 4000, 20_000]);
    let mss = r.pick(&[usize::from(i.mtu) - 40, usize::from(i.mtu) - 40, usize::from(i.mtu)]);
    let mut f = crate::offloadmodel::build(r, 6, 20, 20, payload, 0x0800, 0x4000, 0x18, true);
    f[..6].copy_from_slice(&mac_for(to));
    f[6..12].copy_from_slice(&src_mac);
    f[ETH + 12..ETH + 16].copy_from_slice(&i.ip.to_be_bytes());
    f[ETH + 16..ETH + 20].copy_from_slice(&to.to_be_bytes());
    let Ok(layout) = offload::check_tcp4(&f, mss) else { return };
    if m.ports[p].shut {
        /* Shut: each frame cut from it goes nowhere alike */
        m.ports[p].counts.isolated += layout.frames(payload) as u64;
    } else if spoofed {
        /* Not its own: each frame cut from it refused alike */
        m.ports[p].counts.spoofed += layout.frames(payload) as u64;
    } else if IP + 20 + mss > usize::from(i.mtu) {
        m.too_big(p, &f);
    } else if m.judged(p, true, &f) && m.judged(q, false, &f) {
        /* Whole to a NIC that takes it so, with room; else cut, which the
         * model leaves to the targets that cut: only room for it here. */
        let port = &m.ports[q];
        let wholes = port.queue.iter().filter(|(_, k)| *k == Kind::Whole).count();
        if !(port.takes_whole && port.queue.len() < INBOX && wholes < WHOLE) {
            return;
        }
        m.deliver(q, f.clone(), Kind::Whole);
    }
    if let Some(b) = backends[p].as_mut() {
        b.send_tcp4(&f, layout);
    }
}

/// A TCP segment with no options: ports, flags, and a few bytes.
fn tcp(sport: u16, dport: u16, flags: u8, data: &[u8]) -> Vec<u8> {
    let mut t = vec![0u8; 20];
    t[0..2].copy_from_slice(&sport.to_be_bytes());
    t[2..4].copy_from_slice(&dport.to_be_bytes());
    t[4..8].copy_from_slice(&1000u32.to_be_bytes());
    t[8..12].copy_from_slice(&2000u32.to_be_bytes());
    t[12] = 5 << 4;
    t[13] = flags;
    t[14..16].copy_from_slice(&8192u16.to_be_bytes());
    t.extend_from_slice(data);
    t
}

/// The flows a migration carries (plans/vm-migration.md, M3): a port's
/// stateful firewall's flows taken as its VM pauses (`flows_of`), made the
/// flows of its port on another host's switch (`adopt_flows`). From then on
/// the guest's packets go to both -- the old port the oracle -- and every
/// packet that comes is judged by both: the new port refuses nothing the
/// old one lets in. Its flows run out no sooner than they did there.
pub fn flowmove_target(r: &mut Input) {
    use std::sync::atomic::Ordering::Relaxed;
    network::reset();
    let old = Arc::new(Switch::new().expect("a switch"));
    /* One `hv0` stands for both hosts' and takes one sink: the old host's
     * let go of before the new host's switch is made */
    vnic::reset();
    let new = Arc::new(Switch::new().expect("a switch"));
    vnic::reset();
    /* What comes in denied but for flows -- or, now and then, let in from
     * the network too, so that flows begun coming in are carried as well */
    let mut rules = firewall::Rules::DEFAULT;
    if r.u8() < 64 {
        let rule = firewall::Rule::parse(r.pick(&["allow in udp from net", "allow in tcp 22 from net",
                                                   "allow in icmp from net"])).expect("a rule");
        rules.insert(None, rule).expect("room for a rule");
    }
    for sw in [&old, &new] {
        sw.install(ALLOW_ALL, &rules).expect("a place");
        let tunnel = Arc::new(Tunnel { carried: std::sync::Mutex::new(Vec::new()),
                                       carried_on: std::sync::Mutex::new(Vec::new()) });
        let carrier: Arc<dyn switch::Carrier> = tunnel;
        invariant!(sw.carrier(Some(carrier)).is_none(), "a carrier there already");
    }
    let who = Identity { ip: 0x0A0A_0005, vni: 1, net: 0x0A0A_0000, prefix: 24, mtu: 1400, out: true };
    let me = mac_for(who.ip);
    let vm = Arc::new(Shared::default());
    let p = old.claim_vnet(&vm, &who, ALLOW_ALL).expect("a port");
    let mut on_old = old.backend(p);
    let mut moved: Option<(usize, switch::PortBackend, Arc<Shared>)> = None;
    /* The other ends: ports elsewhere of its network, a few ports each --
     * few enough that no table is ever full, and its least used let go */
    let peers: Vec<u32> = REMOTE.iter().filter(|(v, _)| *v == who.vni).map(|(_, a)| *a).collect();
    const LOCAL: [u16; 4] = [22, 4000, 4001, 50000];
    const REMOTE_PORTS: [u16; 4] = [22, 53, 443, 6000];
    let mut carried = 0;
    while let Some(op) = r.op(5) {
        match op {
            0 | 1 => {
                /* The guest sends to a port elsewhere: a segment, a datagram
                 * or an echo -- to both once it has moved */
                let peer = r.pick(&peers);
                let (lp, pp) = (r.pick(&LOCAL), r.pick(&REMOTE_PORTS));
                let (proto, l4) = match r.u8() % 3 {
                    0 => (6, tcp(lp, pp, r.pick(&[0x02u8, 0x10, 0x18, 0x11, 0x04, 0x12]), &[])),
                    1 => (17, udp(lp, pp, &[1, 2, 3])),
                    _ => (1, icmp(8, 0, [(lp >> 8) as u8, lp as u8, 0, 1], &[0; 8])),
                };
                let f = eth(mac_for(peer), me, 0x0800, &ipv4(who.ip, peer, proto, r.u16(), 0, &l4));
                on_old.send(&f);
                if let Some((_, b, _)) = moved.as_mut() {
                    b.send(&f);
                }
            }
            2 => {
                /* What comes for it out of the tunnel: answers to its
                 * flows, mostly, and what begins one */
                let peer = r.pick(&peers);
                let (lp, pp) = (r.pick(&LOCAL), r.pick(&REMOTE_PORTS));
                let (proto, l4) = match r.u8() % 3 {
                    0 => (6, tcp(pp, lp, r.pick(&[0x12u8, 0x10, 0x18, 0x11, 0x04, 0x02]), &[])),
                    1 => (17, udp(pp, lp, &[4, 5, 6])),
                    _ => (1, icmp(r.pick(&[0u8, 8]), 0, [(lp >> 8) as u8, lp as u8, 0, 1], &[0; 8])),
                };
                let packet = ipv4(peer, who.ip, proto, r.u16(), 0, &l4);
                let before = old.counts(p).denied_in;
                invariant!(old.from_tunnel(who.vni, &packet), "a packet for its port not taken by the old host");
                let refused_old = old.counts(p).denied_in > before;
                if let Some((q, _, _)) = moved.as_ref() {
                    let before = new.counts(*q).denied_in;
                    invariant!(new.from_tunnel(who.vni, &packet), "a packet for its port not taken by the new host");
                    let refused_new = new.counts(*q).denied_in > before;
                    invariant!(!refused_new || refused_old, "the new host refused what the old one let in -- a \
                               flow not carried: {:02x?}", &packet[..packet.len().min(48)]);
                }
            }
            3 => {
                /* The VM paused and moved: its flows taken and made the new
                 * port's, as migrate.rs does -- once */
                if moved.is_some() {
                    continue;
                }
                let mut flows = Vec::with_capacity(switch::FLOWS);
                old.flows_of(p, &mut flows);
                let live = old.counts(p).flows;
                invariant!(flows.len() <= live && flows.iter().all(|c| c.left_ns > 0),
                           "{} flows taken of {} live, some with none left: {:?}", flows.len(), live, flows);
                flows.sort_unstable_by_key(|c| c.left_ns);
                let vm2 = Arc::new(Shared::default());
                let q = new.claim_vnet(&vm2, &who, ALLOW_ALL).expect("its port on the new host");
                let n = new.adopt_flows(q, &flows);
                invariant!(n == flows.len() && new.counts(q).flows == n, "{} of {} flows had, the port says {}", n,
                           flows.len(), new.counts(q).flows);
                /* Taken again, each has no more left than it had -- and no
                 * less than it had less the time between */
                let mut again = Vec::with_capacity(switch::FLOWS);
                new.flows_of(q, &mut again);
                invariant!(again.len() == flows.len(), "{} flows read back of {}", again.len(), flows.len());
                for c in &again {
                    let had = flows.iter().find(|f| f.key == c.key).expect("a flow it had");
                    invariant!(c.inbound == had.inbound && c.seen == had.seen && c.left_ns <= had.left_ns,
                               "flow {:?} read back as {:?}", had, c);
                }
                carried += n;
                let b = new.backend(q);
                moved = Some((q, b, vm2));
            }
            _ => {
                /* Time: a flow's life, or less */
                crate::time::advance(r.pick(&[1u64, 100, 1_000, 30_000, 120_000, 600_000]) * 1_000_000);
                crate::time::progressed();
            }
        }
        /* What the inboxes hold, taken */
        let mut buf = vec![0u8; SEND_MAX];
        while on_old.recv_with(&mut buf).is_some() {}
        if let Some((_, b, _)) = moved.as_mut() {
            while b.recv_with(&mut buf).is_some() {}
        }
    }
    FLOWS_CARRIED.fetch_add(carried as u64, Relaxed);
    drop(vm);
}

/// The flows the `flowmove` target carried, for `HV_FUZZ_STATS`.
pub static FLOWS_CARRIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
