//! vnet's tunnel (`modules/hypervisor/hv/src/tunnel.rs`, plans/vnet.md, "The
//! tunnel") between three hosts of their own -- each a switch and a tunnel
//! as hv makes them, ports of the cluster's networks on its switch -- over a
//! wire that loses, repeats, damages, reorders, misdelivers and forges
//! datagrams; the keys' messages carried in order or lost; tunnels started
//! anew, their goodbyes said or not; the table each host has of where the
//! ports are left behind, or wrong, as ports move; a host that seals what
//! its guests did not send; uplinks with no room, a pool gone dry, next
//! hops ARP does not find. Held, against what the target saw go by:
//!
//! - no key and counter sealed twice, by any host, across its restarts too:
//!   every datagram put on the wire opens under a key its receiver gave its
//!   sender, and that key's counter is remembered;
//! - a packet out of the tunnel reaches a port only as a guest sent it --
//!   or a host sealed it -- at most once, of the kind a port's packet is,
//!   sealed by the host the receiver's table has its sender on, for a port
//!   of the same network at its address: nothing forged, replayed, damaged
//!   or stale gets in;
//! - nothing from the tunnel goes back into it: what a host sends as it
//!   takes a datagram is an echo's answer, if anything;
//! - once the wire is kind and every host has the table, within two minutes
//!   of the tunnels' own turns every port reaches every other of its
//!   network on another host (`settle`);
//! - and then, on a quarter of the inputs, a way between two hosts that
//!   loses every datagram past 1000 bytes for a hundred seconds is said
//!   narrow by both its ends, and no other way is (`narrow`).
//!
//! And public addresses (plans/vnet.md V4): two routed to host a, one on a
//! port of a's own and one on a port elsewhere -- served by a's switch
//! (`Switch::serve`) through NAT one to one, the real one, its uplink the
//! world's way in and out. The world's packet for a public address goes in
//! at the uplink (`nat::intercept`), out of `hv0` to a's switch, and to its
//! port -- through the tunnel, queued by the sink and carried by the task,
//! where the port is elsewhere; the port's for the world goes to a and out
//! of the uplink. Held: a packet of the world's reaches only the port its
//! address is on, by its public address's host alone; a port's goes out
//! from its public address, or masqueraded where it has none, at most once
//! and as it was sent; and in `settle`, every public address both ways.
//!
//! Most inputs begin with the tunnels up, their keys given round (`warm`):
//! what the wire then does, it does to tunnels that carry. The keys'
//! messages are lost now and then, and now and then one comes again later
//! -- a link's resend after a reconnect would look so.
//!
//! A third of the inputs have b and c behind a NAT (`Layout`): the table has
//! both at the NAT's address, each at a port of its own; every datagram of
//! theirs leaves through the NAT with its address -- or another of the
//! NAT's own, on a quarter of those -- and whatever port it gives, the one
//! it came from mostly, and each reaches them at the port the NAT forwards
//! it. A host's port moves now and then, every host told. What arrives is
//! held to the same rules: known by its key, never by where it came from.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use vnetwire::{Header, Kind, Message, HEADER, KEY, PORT, TAG};

use crate::aead::RustCrypto;
use crate::device::{ETH0, HV0};
use crate::net::{Lent, Nic, RxContext};
use crate::vnic::VnicSink;
use crate::nic::Backend as _;
use crate::switch::{Identity, PortBackend, Route, Switch};
use crate::targets::Input;
use crate::tunnel::{RemotePort, TableHost, Tunnel, SILENT_S};
use crate::underlay::{self, ipc};
use crate::vms::Shared;
use crate::vnetmodel::{eth, ipv4, mac_for, udp};

const HOSTS: usize = 3;
const NAMES: [&str; HOSTS] = ["a", "b", "c"];
/// The hosts' underlay addresses: 192.168.100.1, .2, .3.
const UNDERLAY: [u32; HOSTS] = [0xC0A8_6401, 0xC0A8_6402, 0xC0A8_6403];
/// Behind the NAT: b's and c's uplinks at 10.0.0.2 and .3, the NAT at
/// 192.168.100.9 and its MAC, and the ports it forwards to each -- 0 for a,
/// which is not behind it, at the tunnel's own.
const INSIDE: [u32; HOSTS] = [0xC0A8_6401, 0x0A00_0002, 0x0A00_0003];
const NAT_IP: u32 = 0xC0A8_6409;
/// The NAT's other address, what leaves through it may be given.
const NAT_OUT: u32 = 0xC0A8_640A;
/// The ports a host's moves to, from here on.
const MOVED_PORTS: u16 = 39250;
const NAT_MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 0x99];
const NAT_PORTS: [u16; HOSTS] = [0, 39242, 39243];
const MAILBOX: &str = "hv.tunnel";
/// The cluster's networks: their number and subnet, /24 each.
const NETS: [(u32, u32); 2] = [(1, 0x0A0A_0000), (2, 0x0A14_0000)];
const MTU: u16 = 1400;
/// The ports: network, address, and the host each is on first.
const PORTS: [(u32, u32, usize); 6] = [
    (1, 0x0A0A_0002, 0), (1, 0x0A0A_0003, 1), (1, 0x0A0A_0004, 2),
    (2, 0x0A14_0002, 0), (2, 0x0A14_0003, 1), (2, 0x0A14_0004, 2),
];
/// The firewall's place on each switch: everything let through.
const PLACE: usize = 0;
/// The world's side of host a: its uplink -- NAT's way out -- the uplink's
/// gateway, and a host of the world's; and the switch's own `hv0`.
const ETH0_IP: u32 = 0xC633_640A;
const ETH0_GW: u32 = 0xC633_6401;
const ETH0_MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x56];
const GW_MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 0x01];
const WORLD_IP: u32 = 0x0808_0808;
const HOST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0x64, 0x01];
/// The public addresses, each on a port of lan's -- one elsewhere, one on
/// the host they are routed to -- every one routed to `GATEWAY`.
const PUBLICS: [(u32, usize); 2] = [(0xCB00_710A, 1), (0xCB00_710B, 0)];
const GATEWAY: usize = 0;
const WORLD_PORT: u16 = 5000;
const ETH: usize = 14;
const IP: usize = 20;
const UDP: usize = 8;
const NS_PER_MS: u64 = 1_000_000;
const NS_PER_SEC: u64 = 1_000_000_000;
/// The guests' datagrams' port, and the bytes ahead of each one's number.
const GUEST_PORT: u16 = 4000;
const MARK: [u8; 4] = *b"TUNL";

fn node(h: usize) -> u64 {
    h as u64 + 1
}

fn uplink_mac(h: usize) -> [u8; 6] {
    [0x52, 0x54, 0, 0, 0, 0x10 + h as u8]
}

/// Where the hosts are. Plain: each at its uplink's address, at the
/// tunnel's own port. Behind a NAT: b and c at addresses of their own on
/// their uplinks, the table having both at the NAT's, each at a port of its
/// own; a as it was -- every frame of b's and c's to the NAT's MAC, and
/// a's to them.
#[derive(Clone, Copy)]
struct Layout {
    nat: bool,
    /// The address the NAT gives what leaves through it: the one it is
    /// reached at, or another of its own.
    out_ip: u32,
    /// The port the table has for each host: 0 for the tunnel's own.
    ports: [u16; HOSTS],
}

impl Layout {
    /// Host `h`'s uplink's own address: what it sends from.
    fn uplink_ip(self, h: usize) -> u32 {
        if self.nat { INSIDE[h] } else { UNDERLAY[h] }
    }

    /// Whether host `h` is behind the NAT.
    fn behind(self, h: usize) -> bool {
        self.nat && h != 0
    }

    /// Where the table has host `h`: its address, and its port -- 0 for
    /// the tunnel's own.
    fn addr(self, h: usize) -> u32 {
        if self.behind(h) { NAT_IP } else { UNDERLAY[h] }
    }

    fn table_port(self, h: usize) -> u16 {
        self.ports[h]
    }

    /// The port host `h` listens on, and sends from.
    fn port(self, h: usize) -> u16 {
        match self.table_port(h) {
            0 => PORT,
            p => p,
        }
    }

    /// The MAC a frame to host `to` goes to.
    fn hop_mac(self, to: usize) -> [u8; 6] {
        if self.nat { NAT_MAC } else { uplink_mac(to) }
    }

    /// The host at an address and port, as the wire's NAT forwards it.
    fn host_at(self, ip: u32, port: u16) -> Option<usize> {
        (0..HOSTS).find(|&h| self.addr(h) == ip && self.port(h) == port)
    }

    fn hosts(self) -> Vec<TableHost> {
        (0..HOSTS).map(|h| TableHost { node: node(h), name: String::from(NAMES[h]), underlay: self.addr(h),
                                       port: self.table_port(h) }).collect()
    }
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

/// A frame's destination made host `to`'s, as the table has it: its
/// address, its port and the MAC on the way.
fn retarget(layout: Layout, bytes: &mut [u8], to: usize) {
    if bytes.len() >= ETH + IP + UDP {
        bytes[..6].copy_from_slice(&layout.hop_mac(to));
        bytes[ETH + 16..ETH + 20].copy_from_slice(&layout.addr(to).to_be_bytes());
        bytes[ETH + IP + 2..ETH + IP + 4].copy_from_slice(&layout.port(to).to_be_bytes());
    }
}

/// What the inputs came to, for `HV_FUZZ_STATS`.
#[derive(Clone, Copy)]
enum Seen {
    Sealed,
    Delivered,
    Local,
    Replayed,
    Forged,
    Damaged,
    Lost,
    Moved,
    Restarted,
    Settled,
    Spoofed,
    Again,
    Narrow,
    WorldIn,
    WorldOut,
    Nat,
    NatOut,
    PortMoved,
    Migrated,
    CarriedOn,
}

const SEEN: [&str; 20] = ["datagrams sealed", "packets out of the tunnel", "delivered on their host", "replays",
                          "forgeries", "damaged", "lost", "ports moved", "tunnels started anew", "inputs settled",
                          "sealed by a host for another's port", "keys' messages again", "narrow ways said",
                          "the world's packets in through the tunnel", "guests' packets out of the uplink",
                          "inputs behind a NAT", "out of the NAT's other address", "hosts' tunnel ports moved",
                          "ports migrated", "packets carried on by a port's old host"];

static COUNTS: [std::sync::atomic::AtomicU64; 20] = [const { std::sync::atomic::AtomicU64::new(0) }; 20];

fn seen(what: Seen) {
    COUNTS[what as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn report() {
    let all: Vec<String> = SEEN.iter().zip(COUNTS.iter())
        .map(|(name, n)| format!("{} {}", n.load(std::sync::atomic::Ordering::Relaxed), name)).collect();
    println!("  tunnel: {}", all.join(", "));
}

/// A host: its uplink, its switch and tunnel, and the table it was given.
struct Host {
    nic: usize,
    sw: Arc<Switch>,
    /// Its switch's sink on `hv0`: what NAT's packets for its guests are
    /// handed to.
    sink: Arc<dyn VnicSink>,
    tunnel: Arc<Tunnel>,
    /// The ports elsewhere, as it was last told.
    table: Vec<RemotePort>,
    /// Its ports' public addresses' hosts, as it was told them
    /// (`Switch::public_way`), by `PORTS`' place.
    ways: BTreeMap<usize, u64>,
}

/// A packet a guest -- or a host -- sent: by which host, of what kind, of
/// what network, from and to, the packet.
struct Sent {
    host: usize,
    kind: Kind,
    vni: u32,
    to: u32,
    packet: Vec<u8>,
}

struct World {
    layout: Layout,
    /// The next port a host's moves to.
    next_port: u16,
    hosts: Vec<Host>,
    rekey_s: u64,
    keylife_s: u64,
    /// Where each of `PORTS` is, truly, and its port there.
    at: Vec<usize>,
    slots: Vec<Option<(usize, Arc<Shared>, PortBackend)>>,
    generation: Vec<u64>,
    /// The wire: what is on its way, from which host -- and all that ever
    /// was.
    wire: VecDeque<(usize, Vec<u8>)>,
    history: Vec<(usize, Vec<u8>)>,
    /// The keys' messages on their way, in order, from a host to a host.
    mail: BTreeMap<(usize, usize), VecDeque<Vec<u8>>>,
    /// Every key given: by whom, to whom, its number, the key.
    keys: Vec<(usize, usize, u32, [u8; KEY])>,
    /// Every key and counter sealed.
    sealed: BTreeSet<([u8; KEY], u64)>,
    sent: BTreeMap<u64, Sent>,
    delivered: BTreeSet<u64>,
    next_id: u64,
    /// The two hosts whose way loses what is past `NARROW` bytes, while it
    /// does; and whether this input looks for that to be said.
    narrowed: Option<(usize, usize)>,
    narrow_check: bool,
    /// The packets of guests' that went out of the uplink.
    out_seen: BTreeSet<u64>,
    /// Each port's last migration, while its marks may last: the host it
    /// went to, the host it came from, and the clock then
    /// (`Switch::came_here`); and every host's marks of ports moved away
    /// from it: host, network, address, the clock then (`moved_away`).
    came: Vec<Option<(usize, usize, u64)>>,
    away: Vec<(usize, u32, u32, u64)>,
    /// Every host that ever had a port marked moved away, with the port:
    /// what it may have carried on -- a datagram the wire holds arriving
    /// long after the mark is gone.
    carried_on_by: BTreeSet<(usize, u32, u32)>,
}

/// How long a migration's marks last, and what the clock may have read past
/// the moment the model took for one (`switch::MOVED_NS`).
const MARK_NS: u64 = crate::switch::MOVED_NS + NS_PER_SEC;

/// What a narrow way carries at most: an echo of the networks' MTU does
/// not get through it, a small one does.
const NARROW: usize = 1000;

/// Port `i`'s identity: lan goes out, dev does not.
fn identity(i: usize) -> Identity {
    let (vni, ip, _) = PORTS[i];
    let net = NETS.iter().find(|n| n.0 == vni).expect("a network").1;
    Identity { ip, vni, net, prefix: 24, mtu: MTU, out: vni == 1 }
}

impl World {
    fn new(r: &mut Input) -> World {
        let rekey_s = r.pick(&[2u64, 5, 30, 3600]);
        let keylife_s = r.pick(&[1u64, 3, 10]);
        let nat = r.u8() < 85;
        let layout = Layout {
            nat, out_ip: if nat && r.u8() < 64 { NAT_OUT } else { NAT_IP },
            ports: if nat { NAT_PORTS } else { [0; HOSTS] },
        };
        if nat {
            seen(if layout.out_ip == NAT_IP { Seen::Nat } else { Seen::NatOut });
        }
        let mut w = World {
            layout, next_port: MOVED_PORTS, hosts: Vec::new(), rekey_s, keylife_s, at: PORTS.iter().map(|p| p.2).collect(),
            slots: (0..PORTS.len()).map(|_| None).collect(), generation: vec![1; PORTS.len()],
            wire: VecDeque::new(), history: Vec::new(), mail: BTreeMap::new(), keys: Vec::new(),
            sealed: BTreeSet::new(), sent: BTreeMap::new(), delivered: BTreeSet::new(), next_id: 1,
            narrowed: None, narrow_check: r.u8() < 64, out_seen: BTreeSet::new(),
            came: vec![None; PORTS.len()], away: Vec::new(), carried_on_by: BTreeSet::new(),
        };
        /* The uplink NAT goes out by, and its gateway's MAC; and who
         * hands what in to `hv0` */
        crate::network::keep_handed(true);
        ETH0.set(ETH0_MAC, ETH0_IP, 0xFFFF_FF00, ETH0_GW);
        crate::network::arp_learn(ETH0_GW, GW_MAC);
        for h in 0..HOSTS {
            let nic = underlay::add(layout.uplink_ip(h), uplink_mac(h));
            for o in (0..HOSTS).filter(|&o| o != h) {
                underlay::set_hop(layout.addr(o), layout.hop_mac(o), true);
            }
            let sw = Arc::new(Switch::new().expect("a switch"));
            /* One `hv0` stands here for every host's, and takes one sink:
             * each host's kept and let go of before the next host's switch
             * is made -- NAT's packets for the guests then kept by `hv0`,
             * for the target to hand the gateway's switch */
            let sink = crate::vnic::sink().expect("hv0's sink");
            crate::vnic::reset();
            let mut all = firewall::Rules::DEFAULT;
            all.inbound = firewall::Action::Allow;
            all.outbound = firewall::Action::Allow;
            sw.install(PLACE, &all).expect("a place");
            /* The cluster's networks, as each keeper gives its switch them
             * from vnet's table: what is not the world's */
            let subnets: Vec<(u32, u8)> = NETS.iter().map(|n| (n.1, 24)).collect();
            invariant!(sw.cluster_networks(&subnets), "the cluster's networks not taken");
            let table = w.truth(h);
            let tunnel = start(h, nic, &sw, rekey_s, keylife_s, &layout.hosts(), &table);
            w.hosts.push(Host { nic, sw, sink, tunnel, table, ways: BTreeMap::new() });
        }
        for i in 0..PORTS.len() {
            w.claim(i, w.at[i]);
        }
        w.publics();
        w.collect_all();
        if r.u8() < 192 {
            w.warm();
        }
        w
    }

    /// The tunnels' turns, the wire kind, until every port reaches the
    /// others of its network -- or twenty seconds have gone.
    fn warm(&mut self) {
        for round in 0..40 {
            crate::time::advance(500 * NS_PER_MS);
            crate::time::progressed();
            for h in 0..HOSTS {
                self.turn(h, round % 10 == 0);
            }
            self.flush();
            if round % 4 == 3 && self.all_reach() {
                return;
            }
        }
    }

    /// The ports elsewhere, as vnet has them, for host `h`.
    fn truth(&self, h: usize) -> Vec<RemotePort> {
        (0..PORTS.len()).filter(|&i| self.at[i] != h)
            .map(|i| RemotePort { vni: PORTS[i].0, address: PORTS[i].1, node: node(self.at[i]),
                                  generation: self.generation[i] })
            .collect()
    }

    /// The public addresses served on `GATEWAY` as they are now, and each
    /// port's host told where its port's is routed -- as each keeper does
    /// from vnet's table.
    fn publics(&mut self) {
        let routes: Vec<Route> = PUBLICS.iter().map(|&(public, i)| {
            let (vni, inner, _) = PORTS[i];
            let net = NETS.iter().find(|n| n.0 == vni).expect("a network").1;
            Route { public, vni, net, prefix: 24, mtu: MTU, out: vni == 1, inner, node: node(self.at[i]) }
        }).collect();
        ipc::set_current(GATEWAY);
        let said = self.hosts[GATEWAY].sw.serve(node(GATEWAY), &routes);
        invariant!(said.is_empty(), "a's switch did not serve the public addresses: {:?}", said);
        let served = self.hosts[GATEWAY].sw.served();
        invariant!(served.len() == PUBLICS.len(), "a serves {:?}", served);
        for h in 0..HOSTS {
            for i in 0..PORTS.len() {
                if self.at[i] != h {
                    continue;
                }
                let Some((slot, _, _)) = self.slots[i].as_ref() else { continue };
                let way = if PUBLICS.iter().any(|&(_, p)| p == i) && h != GATEWAY { node(GATEWAY) } else { 0 };
                self.hosts[h].sw.public_way(*slot, way);
                self.hosts[h].ways.insert(i, way);
            }
        }
    }

    /// The world's packet for public address `k`'s, in at the uplink: what
    /// NAT made of it out of `hv0`, handed to the gateway's switch, carried
    /// by its tunnel's task where the port is elsewhere. Its number.
    fn world_in(&mut self, k: usize, size: usize) -> u64 {
        let (public, i) = PUBLICS[k];
        let id = self.next_id;
        self.next_id += 1;
        let mut payload = MARK.to_vec();
        payload.extend_from_slice(&id.to_be_bytes());
        payload.extend((0..size).map(|n| n as u8));
        let packet = ipv4(WORLD_IP, public, 17, id as u16, 0, &udp(WORLD_PORT, 7, &payload));
        ipc::set_current(GATEWAY);
        crate::nat::intercept(&ETH0, &eth(ETH0_MAC, GW_MAC, 0x0800, &packet));
        for f in HV0.take_sent() {
            /* NAT's packet for the port, as it made it: what must arrive */
            if f.len() >= ETH + IP + UDP + 12 && f[ETH + IP + UDP..ETH + IP + UDP + 4] == MARK {
                let got = u64::from_be_bytes(f[ETH + IP + UDP + 4..ETH + IP + UDP + 12].try_into().expect("eight"));
                invariant!(got == id && be32(&f, ETH + 16) == PORTS[i].1,
                           "NAT handed hv0 packet {} for {:#x}, the world's {} for {:#x}", got, be32(&f, ETH + 16), id,
                           public);
                self.sent.insert(id, Sent { host: GATEWAY, kind: Kind::GatewayToPort, vni: PORTS[i].0,
                                            to: PORTS[i].1, packet: f[ETH..].to_vec() });
            }
            let sink = self.hosts[GATEWAY].sink.clone();
            sink.on_frame(&f);
        }
        self.hosts[GATEWAY].tunnel.drain_in();
        let frames = underlay::take_sent(self.hosts[GATEWAY].nic);
        self.put_on_wire(GATEWAY, frames);
        self.drain(GATEWAY, None);
        self.world_seen();
        id
    }

    /// Port `i`'s guest's packet for the world, by its gateway: through its
    /// switch -- and the tunnel, where its public address is elsewhere -- to
    /// NAT and out of the uplink. Its number.
    fn world_out(&mut self, i: usize, size: usize) -> u64 {
        let (vni, src, _) = PORTS[i];
        let h = self.at[i];
        let id = self.next_id;
        self.next_id += 1;
        let mut payload = MARK.to_vec();
        payload.extend_from_slice(&id.to_be_bytes());
        payload.extend((0..size).map(|n| n as u8));
        let packet = ipv4(src, WORLD_IP, 17, id as u16, 0, &udp(GUEST_PORT, 7, &payload));
        self.sent.insert(id, Sent { host: h, kind: Kind::PortToGateway, vni, to: WORLD_IP, packet: packet.clone() });
        ipc::set_current(h);
        if let Some((_, _, b)) = self.slots[i].as_mut() {
            b.send(&eth(HOST_MAC, mac_for(src), 0x0800, &packet));
        }
        let frames = underlay::take_sent(self.hosts[h].nic);
        self.put_on_wire(h, frames);
        self.world_seen();
        id
    }

    /// What went out of the uplink: each of a guest's packets once, as it
    /// was sent, from its port's public address -- where it has one -- or
    /// masqueraded where it has none.
    fn world_seen(&mut self) {
        /* Into `hv0` by the host whose way out it is: a port with a public
         * address by the host its address is routed to -- the port being on
         * the host that sent it -- and one with none by its own */
        for (hander, f) in crate::network::take_handed() {
            let at = ETH + IP + UDP;
            if f.len() < at + 12 || f[12..14] != [0x08, 0x00] || f[ETH + 9] != 17 || f[at..at + 4] != MARK {
                continue;
            }
            let id = u64::from_be_bytes(f[at + 4..at + 12].try_into().expect("eight bytes"));
            let Some(s) = self.sent.get(&id) else { continue };
            let from = be32(&f, ETH + 12);
            let port = (0..PORTS.len()).find(|&i| PORTS[i].1 == from);
            let routed = PUBLICS.iter().any(|&(_, i)| Some(i) == port);
            let way = if routed { GATEWAY } else { s.host };
            invariant!(hander == way, "packet {} of {:#x}'s, sent by {}, went into {}'s hv0, not {}'s", id, from,
                       NAMES[s.host], NAMES[hander], NAMES[way]);
            invariant!(port.is_some_and(|i| self.at[i] == s.host), "packet {} of {:#x}'s went out for {}, which has \
                       no port at that address", id, from, NAMES[s.host]);
            invariant!(be32(&f, ETH + 16) == WORLD_IP, "packet {} of {:#x}'s into {}'s hv0 for {:#x}, no address of \
                       the world's", id, from, NAMES[hander], be32(&f, ETH + 16));
        }
        for f in ETH0.take_sent() {
            let at = ETH + IP + UDP;
            if f.len() < at + 12 || f[12..14] != [0x08, 0x00] || f[ETH + 9] != 17 || f[at..at + 4] != MARK {
                continue;
            }
            let id = u64::from_be_bytes(f[at + 4..at + 12].try_into().expect("eight bytes"));
            let Some(s) = self.sent.get(&id) else {
                panic!("invariant: packet {} out of the uplink, which no guest sent", id);
            };
            invariant!(s.kind == Kind::PortToGateway && s.to == WORLD_IP && be32(&f, ETH + 16) == WORLD_IP,
                       "packet {} for {:#x} out of the uplink to {:#x}", id, s.to, be32(&f, ETH + 16));
            invariant!(self.out_seen.insert(id), "packet {} out of the uplink twice", id);
            let from = be32(&s.packet, 12);
            let public = PUBLICS.iter().find(|&&(_, i)| PORTS[i].1 == from).map(|&(p, _)| p);
            let src = be32(&f, ETH + 12);
            invariant!(src == public.unwrap_or(ETH0_IP), "packet {} of {:#x}'s out from {:#x}, not {:#x}", id, from, src,
                       public.unwrap_or(ETH0_IP));
            invariant!(f[at..] == s.packet[IP + UDP..], "packet {} out of the uplink not as it was sent", id);
            seen(Seen::WorldOut);
        }
    }

    /// Port `i` taken on host `h`'s switch, a VM behind it.
    fn claim(&mut self, i: usize, h: usize) {
        /* Its address here again: the host's marks of it gone (`claim_vnet`) */
        let (vni, addr, _) = PORTS[i];
        self.away.retain(|a| !(a.0 == h && a.1 == vni && a.2 == addr));
        if self.came[i].is_some_and(|c| c.0 == h) {
            self.came[i] = None;
        }
        let vm = Arc::new(Shared::default());
        let p = self.hosts[h].sw.claim_vnet(&vm, &identity(i), PLACE).expect("a port of the cluster's");
        let b = self.hosts[h].sw.backend(p);
        self.slots[i] = Some((p, vm, b));
        self.at[i] = h;
    }

    fn collect_all(&mut self) {
        for h in 0..HOSTS {
            let frames = underlay::take_sent(self.hosts[h].nic);
            self.put_on_wire(h, frames);
        }
        self.post();
    }

    /// The datagrams host `h` sent, each held to the keys its receiver gave
    /// it and its counter to every counter sealed, and put on the wire.
    fn put_on_wire(&mut self, h: usize, frames: Vec<Vec<u8>>) {
        self.post();
        let l = self.layout;
        for f in frames {
            /* From its uplink's own address and its own port: a NAT on the
             * way makes the first the NAT's */
            invariant!(f.len() >= ETH + IP + UDP + HEADER + TAG && f[6..12] == uplink_mac(h)
                       && be32(&f, ETH + 12) == l.uplink_ip(h) && be16(&f, ETH + IP) == l.port(h),
                       "host {} sent a frame not its own: {:02x?}", NAMES[h], &f[..f.len().min(48)]);
            let to = l.host_at(be32(&f, ETH + 16), be16(&f, ETH + IP + 2));
            invariant!(to.is_some_and(|t| t != h) && f[..6] == l.hop_mac(to.unwrap_or(0)),
                       "host {} sent a datagram to {:#x}:{} at {:02x?}: no host of the cluster's", NAMES[h],
                       be32(&f, ETH + 16), be16(&f, ETH + IP + 2), &f[..6]);
            let to = to.unwrap_or(0);
            let at = ETH + IP + UDP;
            let header = Header::read(&f[at..]).expect("a header");
            /* Which key: the one `to` gave `h` by that number that opens it */
            let mut found = None;
            for &(giver, taker, number, key) in self.keys.iter() {
                if giver == to && taker == h && number == header.key {
                    let mut copy = f[at..].to_vec();
                    if vnetwire::open(&RustCrypto, &key, &mut copy).is_ok() {
                        found = Some(key);
                        break;
                    }
                }
            }
            let Some(key) = found else {
                panic!("invariant: {} sealed a datagram to {} under key {} -- none {} gave it", NAMES[h], NAMES[to],
                       header.key, NAMES[to]);
            };
            invariant!(self.sealed.insert((key, header.counter)), "{} sealed counter {} under key {} of {}'s twice",
                       NAMES[h], header.counter, header.key, NAMES[to]);
            seen(Seen::Sealed);
            self.wire.push_back((h, f.clone()));
            self.history.push((h, f));
        }
    }

    /// The keys' messages sent, put in their queues; the keys among them
    /// remembered.
    fn post(&mut self) {
        for (from, name, _tag, data) in ipc::take_sent() {
            let Some(to) = name.strip_prefix(MAILBOX).and_then(|n| n.strip_prefix('@'))
                .and_then(|n| NAMES.iter().position(|x| *x == n)) else {
                panic!("invariant: {} sent to {}: no host's tunnel", NAMES[from], name);
            };
            let m = Message::decode(&data);
            invariant!(m.is_some(), "{} sent {} bytes that are no message", NAMES[from], data.len());
            if let Some(Message::Key { number, key, .. }) = m {
                self.keys.push((from, to, number, key));
            }
            self.mail.entry((from, to)).or_default().push_back(data);
        }
    }

    /// `bytes` handed to host `to`'s listener on the port they are for, as
    /// sent by host `origin` -- through the NAT where either is behind it,
    /// at `nat_port` where it gives one of its own -- and sealed by host
    /// `sealer`, `origin` itself unless an old datagram is sent again from
    /// another's address: what `to` sends as it takes them an echo's answer
    /// at most, and what reached its ports held to what was sent -- by
    /// `sealer`, the host its key is of, wherever it came from.
    fn deliver(&mut self, to: usize, origin: usize, sealer: usize, bytes: &[u8], nat_port: Option<u16>) {
        let before = underlay::take_sent(self.hosts[to].nic);
        self.put_on_wire(to, before);
        let mut bytes = bytes.to_vec();
        let l = self.layout;
        if bytes.len() >= ETH + IP + UDP {
            if l.behind(origin) {
                bytes[ETH + 12..ETH + 16].copy_from_slice(&l.out_ip.to_be_bytes());
                if let Some(p) = nat_port {
                    bytes[ETH + IP..ETH + IP + 2].copy_from_slice(&p.to_be_bytes());
                }
            }
            if l.behind(to) && be32(&bytes, ETH + 16) == NAT_IP {
                bytes[ETH + 16..ETH + 20].copy_from_slice(&INSIDE[to].to_be_bytes());
            }
        }
        let port = if bytes.len() >= ETH + IP + UDP { be16(&bytes, ETH + IP + 2) } else { 0 };
        let Some(listener) = underlay::listener(self.hosts[to].nic, port) else { return };
        ipc::set_current(to);
        listener.on_frame(Lent::lend(&bytes), &mut RxContext::for_the_target());
        drop(listener);
        let after = underlay::take_sent(self.hosts[to].nic);
        /* A port's packet carried on, by a host it moved away from a moment
         * ago, is the one other datagram a host sends as it takes one */
        let now = crate::time::peek();
        let carrying_on = self.away.iter().any(|a| a.0 == to && now <= a.3 + MARK_NS);
        for f in after.iter() {
            let kind = f.get(ETH + IP + UDP).copied().and_then(Kind::of);
            invariant!(kind == Some(Kind::EchoAnswer) || carrying_on && kind == Some(Kind::PortToPort),
                       "{} sent a datagram of kind {:?} as it took one: from the tunnel into it", NAMES[to], kind);
        }
        self.put_on_wire(to, after);
        self.drain(to, Some(sealer));
        self.world_seen();
    }

    /// What reached host `h`'s ports, each packet held to what was sent: if
    /// it came out of the tunnel, from `origin`.
    fn drain(&mut self, h: usize, origin: Option<usize>) {
        for i in 0..PORTS.len() {
            if self.at[i] != h {
                continue;
            }
            loop {
                let mut buf = vec![0u8; 2048];
                let Some((_, _, b)) = self.slots[i].as_mut() else { break };
                let Some((n, _)) = b.recv_with(&mut buf) else { break };
                buf.truncate(n);
                self.check(h, i, &buf, origin);
            }
        }
    }

    fn check(&mut self, h: usize, i: usize, frame: &[u8], origin: Option<usize>) {
        let at = ETH + IP + UDP;
        if frame.len() < at + 12 || frame[12..14] != [0x08, 0x00] || frame[ETH + 9] != 17 || frame[at..at + 4] != MARK {
            panic!("invariant: port {} of {} took a frame no guest sent: {:02x?}", i, NAMES[h],
                   &frame[..frame.len().min(64)]);
        }
        let id = u64::from_be_bytes(frame[at + 4..at + 12].try_into().expect("eight bytes"));
        let (vni, addr, _) = PORTS[i];
        let Some(s) = self.sent.get(&id) else {
            panic!("invariant: port {} of {} took packet {}, which nobody sent", i, NAMES[h], id);
        };
        invariant!(self.delivered.insert(id), "packet {} reached {}'s port {} twice", id, NAMES[h], i);
        invariant!(s.vni == vni && s.to == addr && frame[ETH..] == s.packet[..],
                   "packet {} reached {}'s port {} ({:#x} of {}) not as it was sent: for {:#x} of {}", id, NAMES[h],
                   i, addr, vni, s.to, s.vni);
        if s.host == h {
            seen(Seen::Local);
            return;
        }
        /* Out of the tunnel: sealed by the host whose key opened it --
         * wherever it came from -- a port's kind, from a port the
         * receiver's table has on that host */
        let Some(o) = origin else {
            panic!("invariant: packet {} from {} reached {} with nothing out of the tunnel", id, NAMES[s.host],
                   NAMES[h]);
        };
        let src = be32(frame, ETH + 12);
        /* Or carried on by a host the port moved away from -- while its
         * mark lasted, though the wire may hand it on long after -- and
         * taken here as the port came from that host a moment ago, or by
         * the table: its sender's port on that host now */
        let now = crate::time::peek();
        let came = self.came[i].is_some_and(|(to, from, at)| to == h && from == o && now <= at + MARK_NS);
        let its = self.hosts[h].table.iter().any(|r| r.vni == vni && r.address == src && r.node == node(o));
        let carried_on = s.kind == Kind::PortToPort && self.carried_on_by.contains(&(o, vni, addr));
        invariant!(s.host == o || carried_on && (came || its),
                   "packet {} sealed by {} reached {} out of a datagram of {}'s", id, NAMES[s.host], NAMES[h], NAMES[o]);
        match s.kind {
            Kind::PortToPort => {
                let elsewhere = self.hosts[h].table.iter().any(|r| r.vni == vni && r.address == src);
                invariant!(its || came && elsewhere,
                           "packet {} from {:#x} reached {}, whose table has no port at {:#x} of {} on {}", id, src,
                           NAMES[h], src, vni, NAMES[o]);
                seen(if its && s.host == o { Seen::Delivered } else { Seen::CarriedOn });
            }
            Kind::GatewayToPort => {
                invariant!(self.hosts[h].ways.get(&i) == Some(&node(o)),
                           "the world's packet {} reached {}'s port {} from {}, which its public address is not \
                           routed to", id, NAMES[h], i, NAMES[o]);
                let gateway = NETS.iter().find(|n| n.0 == vni).map_or(0, |n| n.1 + 1);
                invariant!(src == gateway || !NETS.iter().any(|n| src & 0xFFFF_FF00 == n.1),
                           "the world's packet {} reached {}'s port {} from {:#x}, an address of the cluster's", id,
                           NAMES[h], i, src);
                seen(Seen::WorldIn);
            }
            k => panic!("invariant: packet {} sealed as {:?} reached {}'s port {}", id, k, NAMES[h], i),
        }
    }

    /// A guest's packet from port `i` to address `to`: through its switch,
    /// into the tunnel or to a port on its host.
    fn guest_send(&mut self, i: usize, to: u32, size: usize) -> u64 {
        let (vni, src, _) = PORTS[i];
        let h = self.at[i];
        let id = self.next_id;
        self.next_id += 1;
        let mut payload = MARK.to_vec();
        payload.extend_from_slice(&id.to_be_bytes());
        payload.extend((0..size).map(|k| k as u8));
        let packet = ipv4(src, to, 17, id as u16, 0, &udp(GUEST_PORT, 7, &payload));
        let frame = eth(mac_for(to), mac_for(src), 0x0800, &packet);
        self.sent.insert(id, Sent { host: h, kind: Kind::PortToPort, vni, to, packet });
        ipc::set_current(h);
        if let Some((_, _, b)) = self.slots[i].as_mut() {
            b.send(&frame);
        }
        let frames = underlay::take_sent(self.hosts[h].nic);
        self.put_on_wire(h, frames);
        self.drain(h, None);
        id
    }

    /// The wire's datagram `k` handed on: delivered, lost, repeated,
    /// damaged or given to another host.
    fn wire_step(&mut self, r: &mut Input) {
        if self.wire.is_empty() {
            return;
        }
        let k = if r.u8() < 200 { 0 } else { r.below(self.wire.len() as u64) as usize };
        let (origin, mut bytes) = if r.u8() < 24 {
            self.wire[k].clone()
        } else {
            self.wire.remove(k).expect("a datagram")
        };
        let l = self.layout;
        let mut to = l.host_at(be32(&bytes, ETH + 16), be16(&bytes, ETH + IP + 2)).unwrap_or(0);
        let nat_port = if r.u8() < 64 { Some(r.u16()) } else { None };
        match r.u8() % 16 {
            0 => {
                seen(Seen::Lost);
                return;
            }
            15 if bytes.len() > 1000 => {
                /* A narrow way: what is longer than it lost */
                seen(Seen::Lost);
                return;
            }
            1 | 2 => {
                let at = r.below(bytes.len() as u64) as usize;
                bytes[at] ^= 1 << r.below(8);
                seen(Seen::Damaged);
            }
            3 => {
                /* To another host -- at its port: a NAT that forwards one
                 * host's port to another */
                to = (to + 1 + r.below(HOSTS as u64 - 1) as usize) % HOSTS;
                retarget(l, &mut bytes, to);
            }
            _ => {}
        }
        self.deliver(to, origin, origin, &bytes, nat_port);
    }

    /// A datagram of the past given again, to whom it was for or another.
    fn replay(&mut self, r: &mut Input) {
        if self.history.is_empty() {
            return;
        }
        let (origin, mut bytes) = self.history[r.below(self.history.len() as u64) as usize].clone();
        let l = self.layout;
        let mut to = l.host_at(be32(&bytes, ETH + 16), be16(&bytes, ETH + IP + 2)).unwrap_or(0);
        if r.u8() < 32 {
            to = r.below(HOSTS as u64) as usize;
            retarget(l, &mut bytes, to);
        }
        let nat_port = if r.u8() < 64 { Some(r.u16()) } else { None };
        seen(Seen::Replayed);
        self.deliver(to, origin, origin, &bytes, nat_port);
    }

    /// A datagram nobody sealed: noise, a header of a key given with a key
    /// nobody gave -- or one of the past from another address, its sealer's
    /// still: known by its key, it is taken as its sealer's if its counter
    /// is new to its receiver.
    fn forge(&mut self, r: &mut Input) {
        let from = r.below(HOSTS as u64) as usize;
        let to = (from + 1 + r.below(HOSTS as u64 - 1) as usize) % HOSTS;
        let mut sealer = from;
        let payload = match r.u8() % 3 {
            0 => (0..r.below(120)).map(|_| r.u8()).collect::<Vec<u8>>(),
            1 => {
                /* A key's number the receiver gave, and a key it did not */
                let number = self.keys.iter().rev().find(|k| k.0 == to && k.1 == from).map_or(r.u32(), |k| k.2);
                let mut key = [0u8; KEY];
                for b in key.iter_mut() {
                    *b = r.u8();
                }
                let (vni, src, _) = PORTS[r.below(PORTS.len() as u64) as usize];
                let dst = PORTS.iter().find(|p| p.0 == vni && p.2 != from).map_or(src, |p| p.1);
                let id = self.next_id;
                self.next_id += 1;
                let mut data = MARK.to_vec();
                data.extend_from_slice(&id.to_be_bytes());
                let packet = ipv4(src, dst, 17, 0, 0, &udp(GUEST_PORT, 7, &data));
                let header = Header { kind: Kind::PortToPort, network: vni, key: number, counter: r.u64() };
                let mut out = vec![0u8; HEADER + packet.len() + TAG];
                vnetwire::seal(&RustCrypto, &key, &header, &packet, &mut out).expect("room for it");
                out
            }
            _ => {
                let Some((was, old)) = self.history.last().cloned() else { return };
                sealer = was;
                old[ETH + IP + UDP..].to_vec()
            }
        };
        let l = self.layout;
        let frame = eth(l.hop_mac(to), uplink_mac(from), 0x0800,
                        &ipv4(l.uplink_ip(from), l.addr(to), 17, 0, 0x4000, &udp(l.port(from), l.port(to), &payload)));
        seen(Seen::Forged);
        self.deliver(to, from, sealer, &frame, None);
    }

    /// Host `h`'s messages from host `from`: the next one taken, or lost.
    fn mail_step(&mut self, r: &mut Input) {
        let queued: Vec<(usize, usize)> = self.mail.iter().filter(|(_, q)| !q.is_empty()).map(|(k, _)| *k).collect();
        if queued.is_empty() {
            return;
        }
        let (from, to) = r.pick(&queued);
        let data = self.mail.get_mut(&(from, to)).and_then(|q| q.pop_front()).expect("a message");
        match r.u8() {
            0..=23 => {
                seen(Seen::Lost);
                return;
            }
            24..=39 => {
                /* Again, later: behind what was sent after it */
                self.mail.entry((from, to)).or_default().push_back(data.clone());
                seen(Seen::Again);
            }
            _ => {}
        }
        self.hand(from, to, &data);
    }

    fn hand(&mut self, from: usize, to: usize, data: &[u8]) {
        let msg = Message::decode(data).expect("a message");

        ipc::set_current(to);
        let now = crate::time::boot_time_ns();
        self.hosts[to].tunnel.on_message(NAMES[from], &msg, now);
        let frames = underlay::take_sent(self.hosts[to].nic);
        self.put_on_wire(to, frames);
    }

    /// Host `h`'s tunnel's turn.
    fn turn(&mut self, h: usize, echo: bool) {
        ipc::set_current(h);
        let now = crate::time::boot_time_ns();
        self.hosts[h].tunnel.turn(now, echo);
        let frames = underlay::take_sent(self.hosts[h].nic);
        self.put_on_wire(h, frames);
    }

    /// Host `h`'s tunnel started anew: its goodbyes said, or its host gone
    /// without a word.
    fn restart(&mut self, h: usize, clean: bool) {
        ipc::set_current(h);
        let host = &self.hosts[h];
        if clean {
            host.tunnel.close();
        } else {
            underlay::kill_listener(host.nic);
        }
        drop(host.sw.carrier(None));
        let frames = underlay::take_sent(host.nic);
        self.put_on_wire(h, frames);
        let table = self.hosts[h].table.clone();
        let fresh = start(h, self.hosts[h].nic, &self.hosts[h].sw, self.rekey_s, self.keylife_s, &self.layout.hosts(),
                          &table);
        let old = core::mem::replace(&mut self.hosts[h].tunnel, fresh);
        drop(old);
        seen(Seen::Restarted);
        self.post();
    }

    /// A host's port moved -- where it is not behind the NAT, to the
    /// tunnel's own now and then -- every host told at once, as each
    /// keeper's next table tells it, and the host listening anew, as its
    /// task's next turn has it.
    fn port_moved(&mut self, r: &mut Input) {
        let h = r.below(HOSTS as u64) as usize;
        let port = if !self.layout.behind(h) && r.bool() {
            0
        } else {
            let p = self.next_port;
            self.next_port = self.next_port.wrapping_add(1).max(MOVED_PORTS);
            p
        };
        self.layout.ports[h] = port;
        let hosts = self.layout.hosts();
        for x in 0..HOSTS {
            ipc::set_current(x);
            let table = self.hosts[x].table.clone();
            self.hosts[x].tunnel.set_table(node(x), &hosts, &table, MTU);
        }
        ipc::set_current(h);
        self.hosts[h].tunnel.uplink(Nic::from_handle(self.hosts[h].nic));
        self.collect_all();
        seen(Seen::PortMoved);
    }

    /// Host `h` told where the ports are: as they are, or with one wrong.
    fn tell(&mut self, h: usize, r: &mut Input) {
        let mut table = self.truth(h);
        if r.u8() < 48 && !table.is_empty() {
            let k = r.below(table.len() as u64) as usize;
            if r.bool() {
                table.remove(k);
            } else {
                let wrong = (0..HOSTS).find(|&o| o != h && node(o) != table[k].node).unwrap_or(h);
                table[k].node = node(wrong);
            }
        }
        ipc::set_current(h);
        let hosts = self.layout.hosts();
        self.hosts[h].tunnel.set_table(node(h), &hosts, &table, MTU);
        self.hosts[h].table = table;
    }

    /// Port `i` moved to another host: given up where it was, taken there.
    fn moved(&mut self, i: usize, to: usize) {
        let from = self.at[i];
        if from == to {
            return;
        }

        if let Some((p, _, b)) = self.slots[i].take() {
            drop(b);
            self.hosts[from].sw.release(p);
        }
        self.drain(from, None);
        self.claim(i, to);
        self.generation[i] += 1;
        self.publics();
        seen(Seen::Moved);
    }

    /// Port `i`'s VM migrated to host `to`: its old host marks it moved
    /// away before it gives its port up, the new one claims it and marks
    /// where it came from (migrate.rs) -- and no host's table told, as
    /// none is until vnet's keepers turn.
    fn migrated(&mut self, i: usize, to: usize) {
        let from = self.at[i];
        if from == to {
            return;
        }
        let (vni, addr, _) = PORTS[i];
        self.hosts[from].sw.moved_away(vni, addr, node(to));
        if let Some((p, _, b)) = self.slots[i].take() {
            drop(b);
            self.hosts[from].sw.release(p);
        }
        self.drain(from, None);
        self.claim(i, to);
        self.hosts[to].sw.came_here(vni, addr, node(from));
        let now = crate::time::peek();
        self.away.retain(|a| !(a.0 == from && a.1 == vni && a.2 == addr));
        self.away.push((from, vni, addr, now));
        self.carried_on_by.insert((from, vni, addr));
        self.came[i] = Some((to, from, now));
        self.generation[i] += 1;
        self.publics();
        seen(Seen::Migrated);
    }

    /// Host `h` seals a packet its guests did not send: from a port that is
    /// not its own, of a kind no port's is, to a port elsewhere.
    fn spoof(&mut self, h: usize, r: &mut Input) {
        let i = r.below(PORTS.len() as u64) as usize;
        let (vni, src, _) = PORTS[i];
        let Some(j) = (0..PORTS.len()).find(|&j| PORTS[j].0 == vni && j != i && self.at[j] != h) else { return };
        let to = PORTS[j].1;
        let id = self.next_id;
        self.next_id += 1;
        let mut data = MARK.to_vec();
        data.extend_from_slice(&id.to_be_bytes());
        ipc::set_current(h);
        match r.u8() % 4 {
            0 => {
                /* A packet for the world as a port's whose public address
                 * is another's, to the gateway */
                let packet = ipv4(src, WORLD_IP, 17, 0, 0, &udp(GUEST_PORT, 7, &data));
                self.sent.insert(id, Sent { host: h, kind: Kind::PortToGateway, vni, to: WORLD_IP,
                                            packet: packet.clone() });
                let _ = crate::switch::Carrier::carry_out(&*self.hosts[h].tunnel, node(GATEWAY), vni, &packet);
            }
            1 => {
                /* The world's, as if this host were the port's gateway */
                let packet = ipv4(r.pick(&[WORLD_IP, ETH0_GW, NETS[0].1 + 1, src]), to, 17, 0, 0,
                                  &udp(WORLD_PORT, 7, &data));
                self.sent.insert(id, Sent { host: h, kind: Kind::GatewayToPort, vni, to, packet: packet.clone() });
                let _ = self.hosts[h].tunnel.send_packet(Kind::GatewayToPort, vni, to, &packet);
            }
            _ => {
                let kind = if r.u8() < 200 { Kind::PortToPort } else { Kind::PortToGateway };
                let packet = ipv4(src, to, 17, 0, 0, &udp(GUEST_PORT, 7, &data));
                self.sent.insert(id, Sent { host: h, kind, vni, to, packet: packet.clone() });
                let _ = self.hosts[h].tunnel.send_packet(kind, vni, to, &packet);
            }
        }
        let frames = underlay::take_sent(self.hosts[h].nic);
        self.put_on_wire(h, frames);
        seen(Seen::Spoofed);
    }

    /// Everything the wire and the mailboxes hold handed on, none lost,
    /// until nothing is left: a turn of a kind world.
    fn flush(&mut self) {
        for _ in 0..64 {
            let queued: Vec<(usize, usize)> =
                self.mail.iter().filter(|(_, q)| !q.is_empty()).map(|(k, _)| *k).collect();
            for (from, to) in queued {
                while let Some(data) = self.mail.get_mut(&(from, to)).and_then(|q| q.pop_front()) {
                    self.hand(from, to, &data);
                }
            }
            if self.wire.is_empty() && self.mail.values().all(|q| q.is_empty()) {
                return;
            }
            while let Some((origin, bytes)) = self.wire.pop_front() {
                let to = self.layout.host_at(be32(&bytes, ETH + 16), be16(&bytes, ETH + IP + 2)).unwrap_or(0);
                if self.narrowed.is_some_and(|(x, y)| (origin, to) == (x, y) || (origin, to) == (y, x))
                    && bytes.len() > NARROW
                {
                    continue;
                }
                self.deliver(to, origin, origin, &bytes, None);
            }
        }
        panic!("invariant: the wire and the mailboxes never ran dry -- hosts that answer each other for ever");
    }

    /// The wire kind, every host told the truth, the uplinks with room, the
    /// next hops known: within two minutes of turns every port reaches every
    /// other of its network on another host.
    fn settle(&mut self) {
        underlay::set_dry(false);
        ipc::set_refuse(false);
        for h in 0..HOSTS {
            if let Some(u) = underlay::get(self.hosts[h].nic) {
                u.room.store(256, std::sync::atomic::Ordering::Relaxed);
            }
            for o in (0..HOSTS).filter(|&o| o != h) {
                underlay::set_hop(self.layout.addr(o), self.layout.hop_mac(o), true);
            }
            let table = self.truth(h);
            let hosts = self.layout.hosts();
            ipc::set_current(h);
            self.hosts[h].tunnel.set_table(node(h), &hosts, &table, MTU);
            self.hosts[h].table = table;
        }
        /* A key lost on the way costs a host SILENT_S at most, and an
         * instance unknown a hello's two seconds: two minutes is plenty */
        let rounds = (SILENT_S * 6).max(240);
        for round in 0..rounds {
            crate::time::advance(500 * NS_PER_MS);
            crate::time::progressed();
            for h in 0..HOSTS {
                self.turn(h, round % 10 == 0);
            }
            self.flush();
            if round % 10 == 9 && self.all_reach() && self.all_public() {
                seen(Seen::Settled);
                if self.narrow_check {
                    self.narrow();
                }
                return;
            }
        }
        invariant!(self.all_reach(), "a kind wire for {} s, and not every port reaches its network's others",
                   rounds / 2);
        invariant!(self.all_public(), "a kind wire for {} s, and not every public address reaches its port both ways",
                   rounds / 2);
    }

    /// The way between a and b loses what is past `NARROW` bytes for a
    /// hundred seconds of turns, echoes every five: both its ends say so --
    /// a `tunnel_key` of `narrow` about the other -- and no host says it of
    /// a way that is not narrow.
    fn narrow(&mut self) {
        /* Twenty seconds of a kind wire first: a full echo answered on
         * every way, so that what was said of the wire before -- once, until
         * a full one comes back -- is said again of this one */
        for round in 0..8 {
            crate::time::advance(2_500 * NS_PER_MS);
            crate::time::progressed();
            for h in 0..HOSTS {
                self.turn(h, round % 2 == 0);
            }
            self.flush();
        }
        crate::trace::set_on(true);
        crate::trace::keep(true);
        self.narrowed = Some((0, 1));
        for round in 0..40 {
            crate::time::advance(2_500 * NS_PER_MS);
            crate::time::progressed();
            for h in 0..HOSTS {
                self.turn(h, round % 2 == 0);
            }
            self.flush();
        }
        self.narrowed = None;
        let kept = crate::trace::take_kept();
        crate::trace::keep(false);
        /* A host behind the NAT is known by its port too: b and c share an
         * address there */
        let l = self.layout;
        let said = |host: usize, of: usize| kept.iter().any(|(h, name, b)| {
            *h == host && *name == "tunnel_key" && b.len() >= 11 && b[10] == crate::trace::TunnelKeyWhat::Narrow as u8
                && u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) == l.addr(of)
                && u16::from_ne_bytes([b[8], b[9]]) == l.port(of)
        });
        invariant!(said(0, 1) && said(1, 0), "a way that lost every datagram past {} bytes for 100 s, said narrow by \
                   a: {}, by b: {}", NARROW, said(0, 1), said(1, 0));
        for (h, o) in [(0, 2), (2, 0), (1, 2), (2, 1)] {
            invariant!(!said(h, o), "{} said the way to {} narrow, which is not", NAMES[h], NAMES[o]);
        }
        seen(Seen::Narrow);
    }

    /// Whether every public address reaches its port from the world, and
    /// its port the world from it, now: the wire flushed.
    fn all_public(&mut self) -> bool {
        let mut ins = Vec::new();
        let mut outs = Vec::new();
        for k in 0..PUBLICS.len() {
            ins.push(self.world_in(k, 16));
            outs.push(self.world_out(PUBLICS[k].1, 16));
        }
        self.flush();
        ins.iter().all(|id| self.delivered.contains(id)) && outs.iter().all(|id| self.out_seen.contains(id))
    }

    /// Whether every port reaches every other of its network on another
    /// host now: one packet each way, the wire flushed.
    fn all_reach(&mut self) -> bool {
        let mut ids = Vec::new();
        for i in 0..PORTS.len() {
            for j in 0..PORTS.len() {
                if i != j && PORTS[i].0 == PORTS[j].0 && self.at[i] != self.at[j] {
                    ids.push(self.guest_send(i, PORTS[j].1, 16));
                }
            }
        }
        self.flush();
        ids.iter().all(|id| self.delivered.contains(id))
    }
}

/// Host `h`'s tunnel, as hv makes it: its mailbox, its table -- `hosts`
/// and the ports elsewhere -- its uplink, carried into by its switch.
fn start(h: usize, nic: usize, sw: &Arc<Switch>, rekey_s: u64, keylife_s: u64, hosts: &[TableHost],
         table: &[RemotePort]) -> Arc<Tunnel> {
    ipc::set_current(h);
    let t = Tunnel::new(sw.clone(), &RustCrypto, rekey_s, keylife_s).expect("a tunnel");
    t.open_mailbox();
    t.set_table(node(h), hosts, table, MTU);
    t.uplink(Nic::from_handle(nic));
    let carrier: Arc<dyn crate::switch::Carrier> = t.clone();
    drop(sw.carrier(Some(carrier)));
    t
}

pub fn tunnel_target(r: &mut Input) {
    crate::network::reset();
    underlay::reset();
    let mut w = World::new(r);
    while let Some(op) = r.op(20) {
        match op {
            19 => {
                let i = r.below(PORTS.len() as u64) as usize;
                let to = r.below(HOSTS as u64) as usize;
                w.migrated(i, to);
            }
            18 => w.port_moved(r),
            16 => {
                let k = r.below(PUBLICS.len() as u64) as usize;
                w.world_in(k, r.below(200) as usize);
            }
            17 => {
                /* A port's guest for the world: one with a public address,
                 * mostly, or any */
                let i = if r.u8() < 160 { PUBLICS[r.below(PUBLICS.len() as u64) as usize].1 }
                        else { r.below(PORTS.len() as u64) as usize };
                w.world_out(i, r.below(200) as usize);
            }
            0..=2 => {
                /* A guest sends: to a port of its network on another host,
                 * mostly; on its own host, of another network, or to no one */
                let i = r.below(PORTS.len() as u64) as usize;
                let (vni, _, _) = PORTS[i];
                let others: Vec<usize> = (0..PORTS.len()).filter(|&j| j != i && PORTS[j].0 == vni).collect();
                let to = match r.u8() % 8 {
                    0 => r.pick(&[0x0A0A_0063u32, 0x0A14_0063]),
                    1 => PORTS[r.below(PORTS.len() as u64) as usize].1,
                    _ => PORTS[r.pick(&others)].1,
                };
                let size = if r.u8() < 16 { r.pick(&[1300usize, 1340, 1347, 1348]) } else { r.below(200) as usize };
                w.guest_send(i, to, size);
            }
            3..=5 => w.wire_step(r),
            6 | 7 => w.mail_step(r),
            8 => {
                let h = r.below(HOSTS as u64) as usize;
                let ms = r.pick(&[1u64, 500, 2_100, 5_000, 21_000, w.rekey_s * 1000 + 1, w.keylife_s * 1000 + 1]);
                crate::time::advance(ms * NS_PER_MS);
                crate::time::progressed();
                w.turn(h, r.bool());
            }
            9 => w.replay(r),
            10 => w.forge(r),
            11 => {
                let h = r.below(HOSTS as u64) as usize;
                w.restart(h, r.bool());
            }
            12 => {
                let h = r.below(HOSTS as u64) as usize;
                w.tell(h, r);
            }
            13 => {
                let i = r.below(PORTS.len() as u64) as usize;
                let to = r.below(HOSTS as u64) as usize;
                w.moved(i, to);
            }
            14 => {
                let h = r.below(HOSTS as u64) as usize;
                w.spoof(h, r);
            }
            _ => {
                /* The uplinks, the pool, the next hops, the mailboxes */
                let h = r.below(HOSTS as u64) as usize;
                match r.u8() % 5 {
                    0 => {
                        if let Some(u) = underlay::get(w.hosts[h].nic) {
                            u.room.store(r.pick(&[0usize, 10, 32, 33, 256]), std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    1 => underlay::set_dry(r.u8() < 64),
                    2 => {
                        let o = (h + 1 + r.below(HOSTS as u64 - 1) as usize) % HOSTS;
                        underlay::set_hop(w.layout.addr(o), w.layout.hop_mac(o), r.bool());
                    }
                    3 => ipc::set_refuse(r.u8() < 64),
                    _ => {
                        let mut out = String::new();
                        w.hosts[h].tunnel.report(&mut out);
                        invariant!(out.starts_with("hv tunnel: instance"), "hv tunnel said {:?}", out);
                    }
                }
            }
        }
    }
    w.settle();
    /* The tunnels gone, as hv's unload has them go */
    for h in 0..HOSTS {
        ipc::set_current(h);
        w.hosts[h].tunnel.close();
        drop(w.hosts[h].sw.carrier(None));
    }
}
