//! The guests' network: the switch between their NICs and `hv0`
//! (`modules/hv/src/net.rs`), with its DHCP server, and NAT
//! (`net/src/nat.rs`), which takes a guest's packets out into the world and
//! the world's answers back. Frames here are the guests' own, and the
//! world's: anything at all, and every field of them a lie now and then.

use std::collections::VecDeque;
use std::sync::Arc;

use netwire::{arp, udp, ARP_LEN, ETH_HDR_LEN, ETH_TYPE_ARP, MAC_BROADCAST, MAX_FRAME};

use crate::dhcp;
use crate::network;
use crate::nic::Backend as _;
use crate::switch::{self, Switch, MAX_PORTS};
use crate::targets::Input;
use crate::vms::Shared;
use crate::vnic;

/* The switch's own constants, as its source has them: the model below says
 * from them where every frame must go. */
const HOST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0x64, 0x01];
const HOST_IP: u32 = 0x0A00_6401;
const MASK: u32 = 0xFFFF_FF00;
const PORT_MAC_PREFIX: [u8; 5] = [0x02, 0, 0, 0, 0x64];
const SWITCH_PORT_BASE: usize = 2;
const INBOX_FRAMES: usize = 256;

/// `n` bytes of payload from `seed`: varied, and not the input's to spend --
/// drawn from the input a byte at a time, a frame's payload took most of a
/// script, and left room for a few steps.
fn noise(seed: u32, n: usize) -> Vec<u8> {
    let mut x = u64::from(seed) | 1 << 40;
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(n);
    v
}

fn port_of(mac: &[u8]) -> Option<usize> {
    if mac[..5] != PORT_MAC_PREFIX {
        return None;
    }
    usize::from(mac[5]).checked_sub(SWITCH_PORT_BASE).filter(|p| *p < MAX_PORTS)
}

/// What the switch must make of a frame from port `p`, said from what port
/// security is: a guest's frame is IPv4 or ARP and its own -- from its MAC,
/// an IPv4 packet from its address and an ARP message with its MAC and
/// address as the sender's, or no address yet -- or it goes nowhere. The one
/// packet with no address yet is a DHCP client's, answered and seen by
/// nobody.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Admit {
    Own,
    Dhcp,
    Spoofed,
    Foreign,
}

fn admit(p: usize, f: &[u8]) -> Admit {
    let (mac, me) = (switch::port_mac(p), switch::port_ip(p));
    let from_mac = f[6..12] == mac;
    let own = match wire::be16(f, 12) {
        ETH_TYPE_IP if f.len() >= ETH_HDR_LEN + IP_HDR_LEN => {
            let src = wire::be32(f, ETH_HDR_LEN + ip::SRC);
            if dhcp::is_request(f) && (src == 0 || src == me) {
                return if from_mac { Admit::Dhcp } else { Admit::Spoofed };
            }
            src == me
        }
        ETH_TYPE_ARP if f.len() >= ETH_HDR_LEN + ARP_LEN => {
            let a = &f[ETH_HDR_LEN..];
            arp::sender_mac(a) == mac && (arp::sender_ip(a) == 0 || arp::sender_ip(a) == me)
        }
        _ => return Admit::Foreign,
    };
    if own && from_mac { Admit::Own } else { Admit::Spoofed }
}

/// Where the switch must put each frame, and what it must count: said from
/// what the switch is for, and held against what it does.
struct Model {
    owned: [bool; MAX_PORTS],
    inbox: Vec<VecDeque<Vec<u8>>>,
    dropped: [u64; MAX_PORTS],
    /// Frames each port's guest sent as another, since it claimed the port.
    spoofed: [u64; MAX_PORTS],
    /// Frames delivered to each port's VM, which it is woken for once each.
    woken: [u64; MAX_PORTS],
    to_host: u64,
    refused: u64,
    dhcp: u64,
    /// Guests' frames of neither IPv4 nor ARP.
    foreign: u64,
    /// What the stack must have been handed through `hv0`.
    stack: Vec<Vec<u8>>,
    dns: u32,
    refuse: bool,
}

impl Model {
    fn new() -> Model {
        Model {
            owned: [false; MAX_PORTS],
            inbox: vec![VecDeque::new(); MAX_PORTS],
            dropped: [0; MAX_PORTS],
            spoofed: [0; MAX_PORTS],
            woken: [0; MAX_PORTS],
            to_host: 0,
            refused: 0,
            dhcp: 0,
            foreign: 0,
            stack: Vec::new(),
            dns: 0,
            refuse: false,
        }
    }

    fn deliver(&mut self, p: usize, frame: &[u8]) {
        if !self.owned[p] {
            return;
        }
        if self.inbox[p].len() == INBOX_FRAMES || frame.len() > MAX_FRAME {
            self.dropped[p] += 1;
            return;
        }
        self.inbox[p].push_back(frame.to_vec());
        self.woken[p] += 1;
    }

    fn to_host(&mut self, from: Option<usize>, frame: &[u8]) {
        if from.is_none() {
            return;
        }
        if self.refuse || frame.len() > MAX_FRAME {
            self.refused += 1;
        } else {
            self.to_host += 1;
            self.stack.push(frame.to_vec());
        }
    }

    fn forward(&mut self, from: Option<usize>, frame: &[u8]) {
        if frame.len() < ETH_HDR_LEN {
            return;
        }
        if let Some(p) = from {
            match admit(p, frame) {
                Admit::Own => {}
                Admit::Dhcp => {
                    let net = dhcp::Network { server_ip: HOST_IP, server_mac: HOST_MAC, mask: MASK, dns: self.dns };
                    let mut reply = [0u8; dhcp::REPLY_MAX];
                    if let Some(len) = dhcp::answer(frame, switch::port_ip(p), &net, &mut reply) {
                        self.dhcp += 1;
                        self.deliver(p, &reply[..len]);
                    }
                    return;
                }
                Admit::Spoofed => {
                    self.spoofed[p] += 1;
                    return;
                }
                Admit::Foreign => {
                    self.foreign += 1;
                    return;
                }
            }
        }
        let dst = &frame[..6];
        if dst[0] & 1 == 0 {
            if dst == HOST_MAC {
                self.to_host(from, frame);
                return;
            }
            if let Some(q) = port_of(dst) {
                if Some(q) != from {
                    self.deliver(q, frame);
                }
                return;
            }
        }
        for q in 0..MAX_PORTS {
            if Some(q) != from {
                self.deliver(q, frame);
            }
        }
        self.to_host(from, frame);
    }
}

/// A MAC a guest on port `p` might send from: its own mostly -- or
/// another port's, `hv0`'s, anyone's.
fn guest_src_mac(r: &mut Input, p: usize) -> [u8; 6] {
    match r.u8() {
        0..=223 => switch::port_mac(p),
        224..=239 => switch::port_mac(r.below(MAX_PORTS as u64) as usize),
        240..=243 => HOST_MAC,
        _ => [r.u8(), r.u8(), r.u8(), r.u8(), r.u8(), r.u8()],
    }
}

/// An address a guest on port `p` might send from: its own mostly -- or
/// none yet, another port's, `hv0`'s, anyone's.
fn guest_src_ip(r: &mut Input, p: usize) -> u32 {
    match r.u8() {
        0..=199 => switch::port_ip(p),
        200..=215 => 0,
        216..=239 => switch::port_ip(r.below(MAX_PORTS as u64) as usize),
        240..=243 => HOST_IP,
        _ => r.u32(),
    }
}

/// A frame a guest -- or the host -- might send: IPv4 or ARP, as the guest
/// or as another, now and then another protocol; to a port, to `hv0`, to
/// everyone, to nobody in particular; a DHCP request; of any length.
fn frame(r: &mut Input, from: Option<usize>) -> Vec<u8> {
    let src = match from {
        Some(p) => guest_src_mac(r, p),
        None => HOST_MAC,
    };
    if r.u8() < 24 {
        if let Some(p) = from {
            return dhcp_request(r, p);
        }
    }
    let dst: [u8; 6] = match r.u8() % 10 {
        0..=3 => switch::port_mac(r.below(MAX_PORTS as u64) as usize),
        4 => HOST_MAC,
        5 => MAC_BROADCAST,
        6 => [0x01, 0, 0x5E, 0, 0, r.u8()],
        7 => [0x02, 0, 0, 0, 0x64, r.pick(&[0, 1, 0xFF, 18, 17])],
        _ => [r.u8() & 0xFE, r.u8(), r.u8(), r.u8(), r.u8(), r.u8()],
    };
    let len = match r.u8() % 8 {
        0 => r.below(ETH_HDR_LEN as u64 + 1) as usize,
        1 => MAX_FRAME + r.below(600) as usize,
        _ => ETH_HDR_LEN + r.below((MAX_FRAME - ETH_HDR_LEN) as u64 + 1) as usize,
    };
    let mut f = noise(r.u32(), len);
    for (i, b) in dst.iter().chain(src.iter()).enumerate() {
        if i < f.len() {
            f[i] = *b;
        }
    }
    /* What the switch looks at: the protocol, and the source it claims --
     * the guest's own, or not. */
    let kind = match r.u8() % 8 {
        0..=3 => ETH_TYPE_IP,
        4 | 5 => ETH_TYPE_ARP,
        6 => 0x86DD,
        _ => r.u16(),
    };
    if f.len() >= ETH_HDR_LEN {
        wire::set_be16(&mut f, 12, kind);
    }
    let p = from.unwrap_or(0);
    if kind == ETH_TYPE_IP && f.len() >= ETH_HDR_LEN + IP_HDR_LEN {
        let ip = guest_src_ip(r, p);
        wire::set_be32(&mut f, ETH_HDR_LEN + ip::SRC, ip);
    }
    if kind == ETH_TYPE_ARP && f.len() >= ETH_HDR_LEN + ARP_LEN {
        let (mac, ip) = (guest_src_mac(r, p), guest_src_ip(r, p));
        f[ETH_HDR_LEN + arp::SENDER_MAC..ETH_HDR_LEN + arp::SENDER_MAC + 6].copy_from_slice(&mac);
        wire::set_be32(&mut f, ETH_HDR_LEN + arp::SENDER_IP, ip);
    }
    f
}

/// A DHCP client's DISCOVER or REQUEST from port `p`, as udhcpc sends one:
/// from no address yet, or -- renewing -- its own; now and then as another.
fn dhcp_request(r: &mut Input, p: usize) -> Vec<u8> {
    let mut m = vec![0u8; 300];
    m[0] = 1;
    m[1] = 1;
    m[2] = 6;
    m[4..8].copy_from_slice(&r.u32().to_le_bytes());
    m[28..34].copy_from_slice(&switch::port_mac(p));
    m[236..240].copy_from_slice(&0x6382_5363u32.to_be_bytes());
    let kind = r.pick(&[1u8, 3, 3, 7]);
    let mut opts = vec![53, 1, kind];
    if kind == 3 {
        let asked = if r.u8() < 200 { switch::port_ip(p) } else { r.u32() };
        opts.extend_from_slice(&[50, 4]);
        opts.extend_from_slice(&asked.to_be_bytes());
    }
    opts.push(255);
    m[240..240 + opts.len()].copy_from_slice(&opts);
    let src_mac = guest_src_mac(r, p);
    let src_ip = if r.u8() < 192 { 0 } else { guest_src_ip(r, p) };
    let route = udp::Route {
        src_mac, dst_mac: MAC_BROADCAST, src_ip, dst_ip: u32::MAX,
        src_port: 68, dst_port: 67, dont_fragment: false,
    };
    let mut f = vec![0u8; udp::PAYLOAD_AT + m.len()];
    let n = udp::write_frame(&mut f, &route, m.len()).expect("a DHCP request fits a frame");
    f[udp::PAYLOAD_AT..n].copy_from_slice(&m);
    f.truncate(n);
    f
}

/// The switch: guests on ports, and `hv0`, sending frames of every kind at
/// each other; what each port's NIC takes, what the stack is handed and
/// every count, against the model.
pub fn switch_target(r: &mut Input) {
    network::reset();
    let dns = r.pick(&[0u32, 0x0808_0808, 0x0A00_0203]);
    network::dns::UPSTREAM.store(dns, std::sync::atomic::Ordering::Relaxed);
    let sw = Switch::new().expect("a switch");
    let mut m = Model::new();
    let mut vms: Vec<Option<Arc<Shared>>> = vec![None; MAX_PORTS];
    let mut backends: Vec<Option<switch::PortBackend>> = (0..MAX_PORTS).map(|_| None).collect();
    /* The most VMs this input puts on the switch at once: a few, mostly --
     * a port's first VM has its inbox's 384 KiB zeroed, which on every port
     * of every input was most of what the target spent -- and now and then
     * one more than there are ports. */
    let most = if r.u8() < 24 { MAX_PORTS + 1 } else { 1 + r.below(4) as usize };

    while let Some(op) = r.op(11) {
        match op {
            0 | 10 => {
                /* A VM on the switch, the first free port -- or, now and
                 * then, VMs until there is none. */
                for _ in 0..if op == 10 && r.u8() < 64 { MAX_PORTS + 1 } else { 1 } {
                if m.owned.iter().filter(|o| **o).count() >= most {
                    break;
                }
                let vm = Arc::new(Shared::default());
                let want = (0..MAX_PORTS).find(|&p| !m.owned[p]);
                match (sw.claim(&vm), want) {
                    (Ok(p), Some(q)) => {
                        invariant!(p == q, "a VM given port {}, port {} being the first free", p, q);
                        m.owned[p] = true;
                        m.inbox[p].clear();
                        m.woken[p] = 0;
                        m.dropped[p] = 0;
                        m.spoofed[p] = 0;
                        vms[p] = Some(vm);
                        backends[p] = Some(sw.backend(p));
                    }
                    (Err(_), None) => {}
                    (got, want) => panic!("invariant: a claim came to {:?} with port {:?} free", got, want),
                }
                }
            }
            1 => {
                let p = r.below(MAX_PORTS as u64) as usize;
                sw.release(p);
                m.owned[p] = false;
                m.inbox[p].clear();
                if let Some(vm) = vms[p].take() {
                    invariant!(vm.woken.load(std::sync::atomic::Ordering::Relaxed) == m.woken[p],
                               "port {}'s VM woken {} times for {} frames", p,
                               vm.woken.load(std::sync::atomic::Ordering::Relaxed), m.woken[p]);
                }
            }
            2..=4 => {
                /* A guest's frame, from a port that has a NIC on it. */
                let p = r.below(MAX_PORTS as u64) as usize;
                if backends[p].is_some() {
                    let f = frame(r, Some(p));
                    m.forward(Some(p), &f);
                    if let Some(b) = backends[p].as_mut() {
                        b.send(&f);
                    }
                }
            }
            5 => {
                /* The host's frame, out of `hv0` -- or a burst of them, past
                 * what an inbox holds. */
                let n = if r.u8() < 16 { INBOX_FRAMES + r.below(40) as usize } else { 1 };
                let f = frame(r, None);
                for _ in 0..n {
                    m.forward(None, &f);
                    if let Some(sink) = vnic::sink() {
                        sink.on_frame(&f);
                    }
                }
            }
            6 | 7 => {
                /* A NIC taking what waits for it, into a buffer of any size. */
                let p = r.below(MAX_PORTS as u64) as usize;
                if let Some(b) = backends[p].as_mut() {
                    let mut buf = vec![0u8; r.pick(&[MAX_FRAME, 60, 2048, 14, 0])];
                    let got = b.recv(&mut buf);
                    let want = if m.owned[p] { m.inbox[p].pop_front() } else { None };
                    match (got, want) {
                        (None, None) => {}
                        (Some(n), Some(w)) => {
                            let expect = w.len().min(buf.len());
                            invariant!(n == expect && buf[..n] == w[..n],
                                       "port {} took {} bytes, {} expected, of a frame of {}", p, n, expect, w.len());
                        }
                        (got, want) => panic!("invariant: port {} took {:?}, the model has {:?} bytes waiting",
                                              p, got, want.map(|w| w.len())),
                    }
                }
            }
            8 => {
                /* The kernel's DNS server asked for again; the stack
                 * refusing frames, or not; a way out asked for, which with
                 * no uplink is none. */
                let dns = r.pick(&[0u32, 0x0808_0808, 0x0A00_0203]);
                network::dns::UPSTREAM.store(dns, std::sync::atomic::Ordering::Relaxed);
                invariant!(sw.dns() == (dns != 0).then_some(dns), "the switch told another DNS server");
                m.dns = dns;
                m.refuse = r.u8() < 64;
                vnic::set_refuse(m.refuse);
                invariant!(sw.way_out() == Err(crate::net::NatError::NoUplink), "a way out with no uplink");
                invariant!(sw.nat_address().is_none(), "NAT on with no way out");
                /* What the module makes of a port for its guest's command line. */
                let p = r.below(MAX_PORTS as u64) as usize;
                let param = switch::ip_param(p, sw.dns());
                invariant!(param.starts_with("ip=10.0.100."), "a port's ip= of {}", param);
                let _ = (switch::nat_why(crate::net::NatError::Busy), sw.host_nic().map(|n| n.ip()));
            }
            _ => {}
        }
        /* The counts, and what the stack was handed, as the model has them. */
        for p in 0..MAX_PORTS {
            invariant!(sw.dropped(p) == (m.dropped[p], m.spoofed[p]),
                       "port {} dropped {:?} (for want of room, and sent as another), the model {:?}",
                       p, sw.dropped(p), (m.dropped[p], m.spoofed[p]));
        }
        let counts = sw.host_counts();
        let want_counts = (m.to_host, m.refused, m.dhcp, m.foreign);
        invariant!(counts == want_counts, "host counts {:?}, the model {:?}", counts, want_counts);
        let stack = network::take_stack();
        let hv0_in = network::take_hv0_in();
        let want = core::mem::take(&mut m.stack);
        invariant!(stack == want && hv0_in == want, "the stack was handed {} frames, hv0 {}, the model {}",
                   stack.len(), hv0_in.len(), want.len());
    }
    /* Every NIC takes what is still waiting for it: all of it, in order. */
    for p in 0..MAX_PORTS {
        let Some(b) = backends[p].as_mut() else { continue };
        let mut buf = vec![0u8; MAX_FRAME];
        loop {
            let got = b.recv(&mut buf);
            let want = if m.owned[p] { m.inbox[p].pop_front() } else { None };
            match (got, want) {
                (None, None) => break,
                (Some(n), Some(w)) => invariant!(buf[..n] == w[..], "port {} took another frame than was sent", p),
                (got, want) => panic!("invariant: port {} drained {:?}, the model {:?} bytes", p, got,
                                      want.map(|w| w.len())),
            }
        }
    }
}

/* ---- NAT: the guests' packets out, the world's answers back ---- */

use std::collections::BTreeMap;

use netwire::{icmp, ip, tcp, ETH_TYPE_IP, ICMP_HDR_LEN, IP_HDR_LEN, IP_PROTO_ICMP, IP_PROTO_TCP, IP_PROTO_UDP,
              UDP_HDR_LEN};

use crate::consts::NS_PER_SEC;
use crate::device::{DEVICES, ETH0, HV0};
use crate::wire;
use crate::nat::{self, COUNTERS, PORT_BASE, PORT_LAST};
use crate::time;

const SUBNET: u32 = 0x0A00_6400;
const ETH0_MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x56];
const GW_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0, 2, 2];
/// Where the guests' flows go: a few places, so that flows repeat.
const REMOTES: [u32; 5] = [0x5DB8_D822, 0x0101_0101, 0x0808_0808, 0x8EFA_0001, 0xC633_6407];
/// The shortest a mapping outlives its flow's last packet (a TCP reset's):
/// one seen within it is live, whatever else the table has done.
const LIVE_NS: u64 = 10 * NS_PER_SEC;

fn guest_mac(ip: u32) -> [u8; 6] {
    [0x02, 0, 0, 0, 0x64, ip as u8]
}

/// An IPv4 packet: a header of `ihl` bytes (options, NOPs, to its length),
/// `total` its total length -- the true one or a lie -- its checksum right
/// unless `bad_sum`; `l4` after it.
#[allow(clippy::too_many_arguments)]
fn ipv4(ihl: usize, id: u16, frag: u16, ttl: u8, proto: u8, src: u32, dst: u32, l4: &[u8], total: usize,
        bad_sum: bool) -> Vec<u8> {
    let mut p = vec![1u8; ihl];
    p[ip::VERSION_IHL] = 0x40 | (ihl / 4) as u8;
    p[ip::TOS] = 0;
    wire::set_be16(&mut p, ip::TOTAL_LEN, total as u16);
    wire::set_be16(&mut p, ip::ID, id);
    wire::set_be16(&mut p, ip::FRAG_OFF, frag);
    p[ip::TTL] = ttl;
    p[ip::PROTOCOL] = proto;
    wire::set_be16(&mut p, ip::CHECKSUM, 0);
    wire::set_be32(&mut p, ip::SRC, src);
    wire::set_be32(&mut p, ip::DST, dst);
    let sum = wire::checksum(&p);
    wire::set_be16(&mut p, ip::CHECKSUM, if bad_sum { sum ^ 0x5555 } else { sum });
    p.extend_from_slice(l4);
    p
}

/// How a transport's checksum is sent: right, none (UDP's 0), or wrong.
#[derive(Clone, Copy, PartialEq)]
enum Sum {
    Right,
    Zero,
    Wrong,
}

fn sum_kind(r: &mut Input, proto: u8) -> Sum {
    match r.u8() % 16 {
        0 => Sum::Wrong,
        1 | 2 if proto == IP_PROTO_UDP => Sum::Zero,
        _ => Sum::Right,
    }
}

/// A TCP segment, UDP datagram or ICMP message of `payload` bytes, from
/// `src`:`sport` to `dst`:`dport` (an ICMP message's identifier `sport`).
#[allow(clippy::too_many_arguments)]
fn transport(r: &mut Input, proto: u8, src: u32, dst: u32, sport: u16, dport: u16, payload: usize, sum: Sum,
             tcp_flags: u8, icmp_kind: u8) -> Vec<u8> {
    let fill = |r: &mut Input, b: &mut [u8]| {
        let n = b.len();
        b.copy_from_slice(&noise(r.u32(), n));
    };
    let mut l4 = match proto {
        IP_PROTO_TCP => {
            let hlen = tcp::HDR_LEN + 4 * r.below(4) as usize;
            let mut seg = vec![tcp::OPT_NOP; hlen + payload];
            tcp::write(&mut seg, sport, dport, r.u32(), r.u32(), hlen, tcp_flags, r.u16());
            fill(r, &mut seg[hlen..]);
            let s = tcp::checksum(src, dst, &seg);
            wire::set_be16(&mut seg, tcp::CHECKSUM, s);
            seg
        }
        IP_PROTO_UDP => {
            let mut d = vec![0u8; UDP_HDR_LEN + payload];
            udp::write(&mut d, sport, dport, payload);
            fill(r, &mut d[UDP_HDR_LEN..]);
            let s = if sum == Sum::Zero { 0 } else { udp::checksum(src, dst, &d) };
            wire::set_be16(&mut d, udp::CHECKSUM, s);
            d
        }
        IP_PROTO_ICMP => {
            let mut m = vec![0u8; ICMP_HDR_LEN + payload];
            fill(r, &mut m[ICMP_HDR_LEN..]);
            let n = m.len();
            icmp::write(&mut m, icmp_kind, 0, sport, r.u16(), n);
            m
        }
        _ => {
            let mut b = vec![0u8; 8 + payload];
            fill(r, &mut b);
            b
        }
    };
    if sum == Sum::Wrong {
        let at = match proto {
            IP_PROTO_TCP => tcp::CHECKSUM,
            IP_PROTO_UDP => udp::CHECKSUM,
            _ => icmp::CHECKSUM,
        };
        if l4.len() >= at + 2 {
            let v = wire::be16(&l4, at) ^ 0x1234;
            wire::set_be16(&mut l4, at, if v == 0 { 1 } else { v });
        }
    }
    l4
}

fn ethernet(dst: [u8; 6], src: [u8; 6], packet: &[u8]) -> Vec<u8> {
    let mut f = vec![0u8; ETH_HDR_LEN];
    f[..6].copy_from_slice(&dst);
    f[6..12].copy_from_slice(&src);
    wire::set_be16(&mut f, 12, ETH_TYPE_IP);
    f.extend_from_slice(packet);
    f
}

/// Where a transport's port -- or an echo's identifier -- and checksum are.
fn fields(proto: u8) -> (usize, usize, usize) {
    match proto {
        IP_PROTO_TCP => (tcp::SRC_PORT, tcp::DST_PORT, tcp::CHECKSUM),
        IP_PROTO_UDP => (udp::SRC_PORT, udp::DST_PORT, udp::CHECKSUM),
        _ => (icmp::ID, icmp::ID, icmp::CHECKSUM),
    }
}

/// Whether a transport's checksum holds, over its pseudo-header too; None
/// for a UDP datagram that carries none.
fn l4_sum_holds(proto: u8, src: u32, dst: u32, l4: &[u8]) -> Option<bool> {
    match proto {
        IP_PROTO_UDP if wire::be16(l4, udp::CHECKSUM) == 0 => None,
        IP_PROTO_TCP | IP_PROTO_UDP => Some(wire::transport_checksum(proto, src, dst, l4) == 0),
        _ => Some(wire::checksum(l4) == 0),
    }
}

/// A flow's key, as NAT maps it: protocol, the guest's end, the far end.
type Key = (u8, u32, u16, u32, u16);

struct Flow {
    key: Key,
    mac: [u8; 6],
    /// When a packet of it last went through, either way.
    last: u64,
}

/// What NAT has done, as far as it can be seen from outside: which port
/// each flow went out on, when each was last seen, what went out. Ordered
/// maps, whose order is the same in every process: a flow is chosen by it,
/// and an input has to choose the same one when it is replayed.
#[derive(Default)]
struct NatModel {
    by_port: BTreeMap<u16, Flow>,
    by_key: BTreeMap<Key, u16>,
    /// The ports of the flows seen last, the latest at the back: what the
    /// world's answers mostly go to.
    recent: VecDeque<u16>,
    /// Packets that went out, for the world's ICMP errors to quote.
    sent: Vec<Vec<u8>>,
    /// Flows mapped for a packet that then did not go out -- no next hop,
    /// no frame -- and the guest's MAC: the table has them, and an answer
    /// on the port is let in.
    unsent: BTreeMap<Key, [u8; 6]>,
}

/// The flow NAT's table has on `port`, live, as `nat` lists it.
fn table_flow(port: u16) -> Option<Key> {
    const NONE: nat::Mapping = nat::Mapping { proto: 0, inner_ip: 0, inner_port: 0, remote_ip: 0, remote_port: 0,
                                              port: 0, left_ns: 0 };
    thread_local! {
        /// Room for the whole table, taken once.
        static ALL: std::cell::RefCell<Vec<nat::Mapping>> = std::cell::RefCell::new(vec![NONE; nat::ENTRIES]);
    }
    let nat = crate::abi::nat()?;
    ALL.with_borrow_mut(|all| {
        let (_, _, _, n) = nat.state(all)?;
        all[..n].iter().find(|m| m.port == port)
            .map(|m| (m.proto, m.inner_ip, m.inner_port, m.remote_ip, m.remote_port))
    })
}

/// Packets the oracle judged: sent out, sent back -- for `HV_FUZZ_STATS`.
pub static OUT_CHECKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static BACK_CHECKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MUST_LET_IN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static ANSWERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static NO_NAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn report() {
    use std::sync::atomic::Ordering::Relaxed;
    if OUT_CHECKED.load(Relaxed) + BACK_CHECKED.load(Relaxed) != 0 {
        println!("  nat: {} packets checked out, {} back ({} of them answers a live flow had to let in); {} answers \
                  tried; {} inputs with no NAT", OUT_CHECKED.load(Relaxed), BACK_CHECKED.load(Relaxed),
                 MUST_LET_IN.load(Relaxed), ANSWERS.load(Relaxed), NO_NAT.load(Relaxed));
    }
}

fn counters() -> [usize; 7] {
    let c = &COUNTERS;
    let l = |a: &std::sync::atomic::AtomicUsize| a.load(std::sync::atomic::Ordering::Relaxed);
    [l(&c.out), l(&c.back), l(&c.mapped), l(&c.full), l(&c.no_hop), l(&c.no_frame), l(&c.refused)]
}

/// A packet NAT took is counted once, as what became of it -- sent out,
/// sent back, dropped for one reason -- and one it did not take not at all.
fn check_counted(taken: bool, before: [usize; 7], after: [usize; 7]) {
    let d: Vec<usize> = before.iter().zip(after.iter()).map(|(b, a)| a - b).collect();
    let fates = d[0] + d[1] + d[3] + d[4] + d[5] + d[6];
    if taken {
        invariant!(fates == 1 && d[2] <= 1, "a packet NAT took counted {:?}", d);
    } else {
        invariant!(fates == 0 && d[2] == 0, "a packet NAT left to the stack counted {:?}", d);
    }
}

/// Every byte of `out` as it was in `inp`, but those at `skip`.
fn same_but(inp: &[u8], out: &[u8], skip: &[core::ops::Range<usize>], what: &str) {
    for i in 0..inp.len().min(out.len()) {
        if !skip.iter().any(|r| r.contains(&i)) {
            invariant!(inp[i] == out[i], "{}: byte {} changed", what, i);
        }
    }
}

impl NatModel {
    fn saw_out(&mut self, key: Key, port: u16, mac: [u8; 6]) {
        let now = time::peek();

        if let Some(f) = self.by_port.get(&port) {
            invariant!(f.key == key || now >= f.last.saturating_add(LIVE_NS),
                       "port {} given to {:?} while {:?} was live on it", port, key, f.key);
        }
        if let Some(&old) = self.by_key.get(&key) {
            let live = self.by_port.get(&old).is_some_and(|f| f.key == key && now < f.last.saturating_add(LIVE_NS));
            invariant!(old == port || !live, "live flow {:?} moved from port {} to {}", key, old, port);
        }
        self.by_port.insert(port, Flow { key, mac, last: now });
        self.by_key.insert(key, port);
        self.recent.retain(|&p| p != port);
        self.recent.push_back(port);
        if self.recent.len() > 16 {
            self.recent.pop_front();
        }
    }
}

/// What NAT sent out for a guest's packet: its source the uplink's address
/// and a port of NAT's, one hop on, its checksums what they were -- right
/// where they were right -- and nothing else changed.
fn check_out(frame: &[u8], out: &[u8], m: &mut NatModel) {
    OUT_CHECKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = &frame[ETH_HDR_LEN..];
    let ihl = ip::header_len(p);
    let len = usize::from(ip::total_len(p));
    invariant!(out.len() == ETH_HDR_LEN + len, "a packet of {} sent as {} bytes", len, out.len());
    let o = &out[ETH_HDR_LEN..];
    let hop = ETH0.route_ip(ip::dst(p));
    let known = crate::abi::arp_table().and_then(|a| a.lookup(hop));
    invariant!(known == Some(out[..6].try_into().unwrap()) && out[6..12] == ETH0_MAC && wire::be16(out, 12) == ETH_TYPE_IP,
               "sent to {:02x?} from {:02x?}, the hop {:?}", &out[..6], &out[6..12], known);
    invariant!(ip::src(o) == ETH0.ip() && ip::dst(o) == ip::dst(p) && o[ip::TTL] == p[ip::TTL] - 1,
               "a header rewritten wrong: {:08x} -> {:08x}, ttl {}", ip::src(o), ip::dst(o), o[ip::TTL]);
    invariant!(wire::checksum(&o[..ihl]) == 0, "a header sent with a wrong sum");
    same_but(&p[..ihl], &o[..ihl], &[ip::TTL..ip::TTL + 1, ip::CHECKSUM..ip::CHECKSUM + 2, ip::SRC..ip::SRC + 4],
             "the header");
    let proto = ip::protocol(p);
    let (pl4, ol4) = (&p[ihl..len], &o[ihl..len]);
    let (sport, dport, sum) = fields(proto);
    let external = wire::be16(ol4, sport);
    invariant!((PORT_BASE..=PORT_LAST).contains(&external), "a flow out on port {}", external);
    same_but(pl4, ol4, &[sport..sport + 2, sum..sum + 2], "the transport header or payload");
    let (before, after) = (l4_sum_holds(proto, ip::src(p), ip::dst(p), pl4), l4_sum_holds(proto, ip::src(o), ip::dst(o), ol4));
    invariant!(before == after, "a transport sum {:?} going out, {:?} gone out", before, after);
    let remote_port = if proto == IP_PROTO_ICMP { 0 } else { wire::be16(pl4, dport) };
    let key = (proto, ip::src(p), wire::be16(pl4, sport), ip::dst(p), remote_port);
    m.saw_out(key, external, frame[6..12].try_into().unwrap());
    if m.sent.len() < 64 {
        m.sent.push(out.to_vec());
    }
}

/// A guest's packet, from `hv0`: mostly for the world -- a new flow, or one
/// that went out before going on -- and every field of it wrong now and then.
fn outbound(r: &mut Input, m: &mut NatModel) {
    let known: Vec<Key> = m.by_key.keys().copied().collect();
    let follow = !known.is_empty() && r.u8() < 110;
    let (guest, src, dst, proto, sport, dport) = if follow {
        let (proto, src, sport, dst, dport) = known[r.below(known.len() as u64) as usize];
        (src, src, dst, proto, sport, dport)
    } else {
        let guest = SUBNET | (2 + r.below(16) as u32);
        let src = match r.u8() % 16 {
            0 => HOST_IP,
            1 => r.u32(),
            2 => guest ^ 0x100,
            _ => guest,
        };
        let dst = match r.u8() % 16 {
            0..=9 => r.pick(&REMOTES),
            10 => SUBNET | r.below(256) as u32,
            11 => ETH0.ip(),
            12 => ETH0.ip() | !ETH0.mask(),
            13 => r.pick(&[0xE000_0001, 0x7F00_0001, 0x0102_0304, 0xFFFF_FFFF]),
            _ => r.u32(),
        };
        let proto = r.pick(&[IP_PROTO_TCP, IP_PROTO_TCP, IP_PROTO_TCP, IP_PROTO_UDP, IP_PROTO_UDP, IP_PROTO_UDP,
                             IP_PROTO_ICMP, IP_PROTO_ICMP, 47]);
        let sport = r.pick(&[40000u16, 40001, 50000, 1024]).wrapping_add(if r.u8() < 16 { r.u16() } else { 0 });
        let dport = r.pick(&[80u16, 443, 53, 0]).wrapping_add(if r.u8() < 16 { r.u16() } else { 0 });
        (guest, src, dst, proto, sport, dport)
    };
    let flags = r.pick(&[tcp::SYN, tcp::SYN, tcp::ACK_FLAG, tcp::SYN | tcp::ACK_FLAG, tcp::FIN | tcp::ACK_FLAG,
                         tcp::RST, tcp::PSH | tcp::ACK_FLAG]);
    let kind = r.pick(&[icmp::ECHO_REQUEST, icmp::ECHO_REQUEST, icmp::ECHO_REQUEST, icmp::ECHO_REPLY, icmp::DEST_UNREACH]);
    /* Now and then more than a frame holds, which NAT cannot copy. */
    let payload = if r.u8() < 4 { 1400 + r.below(200) as usize } else { r.below(300) as usize };
    let sum = sum_kind(r, proto);
    let mut l4 = transport(r, proto, src, dst, sport, dport, payload, sum, flags, kind);
    if r.u8() < 8 {
        let n = r.below(l4.len() as u64) as usize;
        l4.truncate(n);
    }
    let ttl = if r.u8() < 240 { 64 } else { r.pick(&[0u8, 1, 2, 255]) };
    let ihl = if r.u8() < 230 { IP_HDR_LEN } else { IP_HDR_LEN + 4 * (1 + r.below(10) as usize) };
    let frag = match r.u8() % 32 {
        0 => ip::MORE_FRAGMENTS,
        1 => 1,
        2..=9 => ip::DONT_FRAGMENT,
        _ => 0,
    };
    let honest = ihl + l4.len();
    let total = match r.u8() % 32 {
        0 => honest + 1 + r.below(50) as usize,
        1 => r.below(ihl as u64 + 1) as usize,
        2 => r.below(honest as u64 + 1) as usize,
        _ => honest,
    };
    let packet = ipv4(ihl, r.u16(), frag, ttl, proto, src, dst, &l4, total, r.u8() < 8);
    let mut frame = ethernet(HOST_MAC, guest_mac(guest & 0xFF), &packet);
    if r.u8() < 16 {
        frame.extend((0..r.below(40)).map(|i| i as u8));
    }

    let before = counters();
    let taken = nat::intercept(&HV0, &frame);
    let after = counters();
    check_counted(taken, before, after);
    let sent = ETH0.take_sent();
    invariant!(HV0.take_sent().is_empty(), "a guest's packet sent back to the guests");
    invariant!(sent.len() <= 1 && (sent.is_empty() || taken), "{} packets sent for one ({})", sent.len(), taken);
    match sent.first() {
        Some(out) => check_out(&frame, out, m),
        None if after[2] > before[2] => {
            /* Mapped, and not sent: the key as the packet had it. */
            let p = &frame[ETH_HDR_LEN..];
            let ihl = ip::header_len(p);
            let (s, d, _) = fields(ip::protocol(p));
            let l4 = &p[ihl..];
            let remote_port = if ip::protocol(p) == IP_PROTO_ICMP { 0 } else { wire::be16(l4, d) };
            let key = (ip::protocol(p), ip::src(p), wire::be16(l4, s), ip::dst(p), remote_port);
            m.unsent.insert(key, frame[6..12].try_into().unwrap());
        }
        None => {}
    }
}

/// What NAT sent back to a guest for the world's packet on port `port`:
/// for the flow on it, to the guest's address and port and MAC, one hop on,
/// sums as they were; and only from the far end the flow went to.
fn check_back(frame: &[u8], back: &[u8], m: &mut NatModel) {
    BACK_CHECKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = &frame[ETH_HDR_LEN..];
    let ihl = ip::header_len(p);
    let len = usize::from(ip::total_len(p));
    invariant!(back.len() == ETH_HDR_LEN + len, "an answer of {} sent back as {} bytes", len, back.len());
    let o = &back[ETH_HDR_LEN..];
    let proto = ip::protocol(p);
    let (pl4, ol4) = (&p[ihl..len], &o[ihl..len]);
    let error = proto == IP_PROTO_ICMP && matches!(icmp::kind(pl4), icmp::DEST_UNREACH | icmp::TIME_EXCEEDED);
    /* Whose it is: the port it came to -- or, an error, the port of the
     * packet it quotes -- and from where. */
    let (port, flow_proto, far_ip, far_port) = if error {
        let q = &pl4[ICMP_HDR_LEN..];
        let qihl = ip::header_len(q);
        let qproto = ip::protocol(q);
        let (s, d, _) = fields(qproto);
        let far_port = if qproto == IP_PROTO_ICMP { 0 } else { wire::be16(&q[qihl..], d) };
        (wire::be16(&q[qihl..], s), qproto, ip::dst(q), far_port)
    } else {
        let (_, d, _) = fields(proto);
        let far_port = if proto == IP_PROTO_ICMP { 0 } else { wire::be16(pl4, fields(proto).0) };
        (wire::be16(pl4, d), proto, ip::src(p), far_port)
    };
    /* Whose the port is: the table's word -- a mapping may be made for a
     * packet that never went out, on a port the model last saw another flow
     * on -- and never another flow's that was live on it. */
    let Some(key) = table_flow(port) else {
        panic!("invariant: an answer sent back for port {}, which NAT has no mapping on", port)
    };
    let now = time::peek();
    /* An answer keeps its flow alive; an error, NAT holds, says nothing of
     * that -- and the model must not say more than NAT does. */
    let (mac, last) = match m.by_port.get(&port) {
        Some(f) if f.key == key => (f.mac, if error { f.last } else { now }),
        Some(f) => {
            invariant!(now >= f.last.saturating_add(LIVE_NS), "port {} is {:?}'s in the table, {:?} live on it",
                       port, key, f.key);
            (m.unsent.get(&key).copied().unwrap_or(back[..6].try_into().unwrap()), if error { 0 } else { now })
        }
        None => (m.unsent.get(&key).copied().unwrap_or(back[..6].try_into().unwrap()), if error { 0 } else { now }),
    };
    m.by_port.insert(port, Flow { key, mac, last });
    m.by_key.insert(key, port);
    let flow = m.by_port.get_mut(&port).expect("the flow, known now");
    let (kproto, inner_ip, inner_port, remote_ip, remote_port) = flow.key;
    invariant!(kproto == flow_proto && remote_ip == far_ip && remote_port == far_port,
               "an answer from {:08x}:{} let in to flow {:?}", far_ip, far_port, flow.key);
    invariant!(back[..6] == flow.mac && back[6..12] == HOST_MAC && wire::be16(back, 12) == ETH_TYPE_IP,
               "an answer sent to {:02x?} from {:02x?}", &back[..6], &back[6..12]);
    invariant!(ip::dst(o) == inner_ip && ip::src(o) == ip::src(p) && o[ip::TTL] == p[ip::TTL] - 1,
               "an answer's header rewritten wrong");
    invariant!(wire::checksum(&o[..ihl]) == 0, "an answer sent back with a wrong header sum");
    same_but(&p[..ihl], &o[..ihl], &[ip::TTL..ip::TTL + 1, ip::CHECKSUM..ip::CHECKSUM + 2, ip::DST..ip::DST + 4],
             "an answer's header");
    if error {
        invariant!(wire::checksum(ol4) == 0, "an ICMP error sent back with a wrong sum");
        let (q, oq) = (&pl4[ICMP_HDR_LEN..], &ol4[ICMP_HDR_LEN..]);
        let qihl = ip::header_len(q);
        let (s, _, qsum) = fields(flow_proto);
        invariant!(ip::src(oq) == inner_ip && wire::be16(&oq[qihl..], s) == inner_port,
                   "an ICMP error's quote rewritten wrong");
        invariant!(wire::checksum(&oq[..qihl]) == 0, "an ICMP error's quote with a wrong header sum");
        same_but(q, oq, &[ip::CHECKSUM..ip::CHECKSUM + 2, ip::SRC..ip::SRC + 4, qihl + s..qihl + s + 2,
                          qihl + qsum..qihl + qsum + 2], "an ICMP error's quote");
    } else {
        let (_, d, sum) = fields(proto);
        invariant!(wire::be16(ol4, d) == inner_port, "an answer to port {} of the guest's {}", wire::be16(ol4, d),
                   inner_port);
        same_but(pl4, ol4, &[d..d + 2, sum..sum + 2], "an answer's transport header or payload");
        let before = l4_sum_holds(proto, ip::src(p), ip::dst(p), pl4);
        let after = l4_sum_holds(proto, ip::src(o), ip::dst(o), ol4);
        invariant!(before == after, "an answer's transport sum {:?} coming in, {:?} sent back", before, after);
    }
}

/// The world's packet, to the uplink: an answer to a flow that went out --
/// from where it went, mostly -- an ICMP error quoting a packet that went
/// out, or anything.
fn inbound(r: &mut Input, m: &mut NatModel) {
    /* A flow seen last, mostly; now and then any. */
    let ports: Vec<u16> = if r.u8() < 224 && !m.recent.is_empty() {
        m.recent.iter().copied().collect()
    } else {
        m.by_port.keys().copied().collect()
    };
    let flows: Vec<(u16, Key, u64)> = ports.iter().map(|p| (*p, m.by_port[p].key, m.by_port[p].last)).collect();
    let pick = |r: &mut Input| flows[r.below(flows.len() as u64) as usize];
    let ttl = if r.u8() < 240 { 64 } else { r.pick(&[0u8, 1, 2]) };
    let kind = r.u8() % 16;
    let (frame, must_let_in) = if kind < 11 && !flows.is_empty() {
        ANSWERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (port, key, last) = pick(r);
        let (proto, _, _, remote_ip, remote_port) = key;
        let src = if r.u8() < 220 { remote_ip } else { r.pick(&REMOTES) };
        let sport = if r.u8() < 220 { remote_port } else { r.u16() };
        let dport = if r.u8() < 230 { port } else { r.pick(&[port.wrapping_add(1), PORT_BASE - 1, PORT_LAST + 1]) };
        let proto2 = if r.u8() < 245 { proto } else { r.pick(&[IP_PROTO_TCP, IP_PROTO_UDP, IP_PROTO_ICMP]) };
        let flags = r.pick(&[tcp::ACK_FLAG, tcp::SYN | tcp::ACK_FLAG, tcp::FIN | tcp::ACK_FLAG, tcp::RST,
                             tcp::PSH | tcp::ACK_FLAG]);
        let icmp_kind = if r.u8() < 240 { icmp::ECHO_REPLY } else { icmp::ECHO_REQUEST };
        let sum = sum_kind(r, proto2);
        let (tsrc, tdst) = if proto2 == IP_PROTO_ICMP { (dport, 0) } else { (sport, dport) };
        let payload = r.below(200) as usize;
        let l4 = transport(r, proto2, src, ETH0.ip(), tsrc, tdst, payload, sum, flags, icmp_kind);
        let bad_sum = r.u8() < 4;
        let packet = ipv4(IP_HDR_LEN, r.u16(), 0, ttl, proto2, src, ETH0.ip(), &l4, IP_HDR_LEN + l4.len(), bad_sum);
        /* A well-made answer from the flow's far end, to its port, while it
         * is live for certain: NAT must let it in. */
        let answer = proto2 == proto && src == remote_ip && (proto == IP_PROTO_ICMP || sport == remote_port)
            && dport == port && !bad_sum && ttl > 1 && (proto != IP_PROTO_ICMP || icmp_kind == icmp::ECHO_REPLY)
            && time::peek() < last.saturating_add(LIVE_NS);
        (ethernet(ETH0_MAC, GW_MAC, &packet), answer)
    } else if kind < 14 && !m.sent.is_empty() {
        /* A router's error about a packet that went out: its header and
         * the start of its transport -- or more -- quoted. */
        let sent = &m.sent[r.below(m.sent.len() as u64) as usize];
        let q = &sent[ETH_HDR_LEN..];
        let qihl = ip::header_len(q);
        let keep = if r.u8() < 200 { qihl + 8 } else { q.len() };
        let mut quoted = q[..keep.min(q.len())].to_vec();
        if r.u8() < 8 {
            let i = r.below(quoted.len() as u64) as usize;
            quoted[i] ^= 0x40;
        }
        let mut msg = vec![0u8; ICMP_HDR_LEN];
        msg.extend_from_slice(&quoted);
        let n = msg.len();
        icmp::write(&mut msg, r.pick(&[icmp::DEST_UNREACH, icmp::TIME_EXCEEDED]), icmp::PORT_UNREACH, 0, 0, n);
        if r.u8() < 8 {
            msg[icmp::CHECKSUM] ^= 0x10;
        }
        let router = if r.bool() { ETH0.gw() } else { r.u32() };
        let packet = ipv4(IP_HDR_LEN, r.u16(), 0, ttl, IP_PROTO_ICMP, router, ETH0.ip(), &msg, IP_HDR_LEN + msg.len(), false);
        (ethernet(ETH0_MAC, GW_MAC, &packet), false)
    } else {
        let n = r.below(200) as usize;
        let mut f = noise(r.u32(), n);
        if f.len() >= ETH_HDR_LEN + IP_HDR_LEN && r.bool() {
            wire::set_be16(&mut f, 12, ETH_TYPE_IP);
            f[ETH_HDR_LEN] = 0x45;
        }
        (f, false)
    };

    let before = counters();
    let taken = nat::intercept(&ETH0, &frame);
    check_counted(taken, before, counters());
    invariant!(ETH0.take_sent().is_empty(), "the world's packet sent back out to the world");
    let back = HV0.take_sent();
    invariant!(back.len() <= 1 && (back.is_empty() || taken), "{} packets sent back for one", back.len());
    if must_let_in {
        MUST_LET_IN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    match back.first() {
        Some(b) => check_back(&frame, b, m),
        None => invariant!(!must_let_in || HV0.refuse.load(std::sync::atomic::Ordering::Relaxed),
                           "a live flow's answer not let in"),
    }
}

/// NAT between `hv0` and an uplink with a gateway: guests' packets out, the
/// world's back, time passing, the table filled, NAT off and on again.
pub fn nat_target(r: &mut Input) {
    network::reset();
    let outer_ip = r.pick(&[0xC0A8_010Au32, 0x0A00_020F, 0x5F10_2030]);
    let mask = r.pick(&[0xFFFF_FF00u32, 0xFFFF_0000, 0]);
    let gw = if r.u8() < 240 { (outer_ip & 0xFFFF_FF00) | 1 } else { r.pick(&[0u32, 0x0808_0808]) };
    HV0.set(HOST_MAC, HOST_IP, MASK, 0);
    ETH0.set(ETH0_MAC, outer_ip, mask, gw);
    if r.u8() < 32 {
        crate::device::SPARE.set([0x52, 0x54, 0, 0, 0, 9], r.u32(), MASK, 0);
    }
    network::dns::UPSTREAM.store(r.pick(&[0u32, (outer_ip & mask) | 53, 0x0808_0808]), std::sync::atomic::Ordering::Relaxed);
    if r.u8() < 230 {
        network::arp_learn(gw, GW_MAC);
    }
    let nat = crate::abi::nat().expect("NAT");
    let on = nat.enable(&HV0, None);
    let Ok(outer) = on else {
        NO_NAT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        invariant!(DEVICES.uplink(&HV0).is_none() || ETH0.ip() == 0, "NAT refused with an uplink: {:?}", on.err());
        return;
    };
    invariant!(core::ptr::eq(outer, &ETH0), "NAT out through {:?}", core::str::from_utf8(outer.name()));
    invariant!(matches!(nat.enable(&HV0, None), Err(nat::NatError::Busy)), "NAT on twice");
    let mut m = NatModel::default();

    while let Some(op) = r.op(10) {
        match op {
            0..=3 => outbound(r, &mut m),
            4..=6 => inbound(r, &mut m),
            7 if r.u8() < 128 => {}
            7 => time::advance(r.pick(&[NS_PER_SEC, 9 * NS_PER_SEC, 11 * NS_PER_SEC, 59 * NS_PER_SEC,
                                        61 * NS_PER_SEC, 239 * NS_PER_SEC, 241 * NS_PER_SEC, 299 * NS_PER_SEC,
                                        301 * NS_PER_SEC, 7439 * NS_PER_SEC, 7441 * NS_PER_SEC])),
            8 => {
                /* Many flows at once, as many as the table holds and past
                 * it: each checked as any other. */
                let n = if r.u8() < 4 { 4200 } else { r.pick(&[8u32, 64, 256]) };
                let guest = SUBNET | (2 + r.below(16) as u32);
                let remote = r.pick(&REMOTES);
                for i in 0..n {
                    let sport = 10000u16.wrapping_add(i as u16);
                    let l4 = transport(r, IP_PROTO_UDP, guest, remote, sport, 53, 4, Sum::Right, 0, 0);
                    let packet = ipv4(IP_HDR_LEN, i as u16, 0, 64, IP_PROTO_UDP, guest, remote, &l4,
                                      IP_HDR_LEN + l4.len(), false);
                    let frame = ethernet(HOST_MAC, guest_mac(guest), &packet);
                    let before = counters();
                    let taken = nat::intercept(&HV0, &frame);
                    check_counted(taken, before, counters());
                    if let Some(out) = ETH0.take_sent().first() {
                        check_out(&frame, out, &mut m);
                    }
                }
            }
            _ => match r.u8() % 6 {
                0 => {
                    /* What `nat` shows. */
                    let mut shown = [nat::Mapping { proto: 0, inner_ip: 0, inner_port: 0, remote_ip: 0,
                                                    remote_port: 0, port: 0, left_ns: 0 }; 8];
                    if let Some((_, _, active, n)) = nat.state(&mut shown) {
                        invariant!(n <= shown.len() && active <= nat::ENTRIES, "nat state {} shown of {}", n, active);
                        for s in &shown[..n] {
                            invariant!((PORT_BASE..=PORT_LAST).contains(&s.port) && s.left_ns > 0,
                                       "a mapping shown on port {} with {} ns left", s.port, s.left_ns);
                        }
                    }
                    let mut out = crate::cmd::Output::default();
                    nat::shell("", &mut out);
                    invariant!(out.0.starts_with("nat: on"), "nat said {:?}", out.0.lines().next());
                }
                1 => {
                    let on = r.bool();
                    HV0.refuse.store(on, std::sync::atomic::Ordering::Relaxed);
                    ETH0.refuse.store(r.u8() < 64, std::sync::atomic::Ordering::Relaxed);
                }
                2 => {
                    if r.bool() {
                        network::arp_learn(ETH0.gw(), GW_MAC);
                    } else if let Some(arp) = crate::abi::arp_table() {
                        arp.known.lock().unwrap().clear();
                    }
                }
                3 => {
                    /* Off and on again, through the C ABI a module has: a new
                     * table, every mapping gone. */
                    invariant!(nat::kernel_net_nat_disable(HV0.handle()) == 1, "NAT was not on");
                    invariant!(nat::kernel_net_nat_disable(HV0.handle()) == 0, "NAT off twice");
                    let on = nat::kernel_net_nat_enable(HV0.handle());
                    invariant!(on.code == 0 && on.outer == ETH0.handle(), "NAT back on: {:?}", on);
                    m = NatModel::default();
                }
                4 => {
                    /* A device NAT cannot take -- none, hv0 as its own way
                     * out -- NAT on while it is, and `nat` while it is off. */
                    invariant!(nat::kernel_net_nat_enable(0).code == 2, "NAT on for no device");
                    invariant!(nat.enable(&HV0, Some(&HV0)).is_err(), "NAT through hv0 itself");
                    invariant!(nat::kernel_net_nat_enable(HV0.handle()).code == 3, "NAT on twice");
                    if r.bool() {
                        nat::kernel_net_nat_disable(HV0.handle());
                        let mut out = crate::cmd::Output::default();
                        nat::shell("", &mut out);
                        invariant!(out.0.starts_with("nat: off"), "nat said {:?} when off", out.0.lines().next());
                        let gw = ETH0.gw();
                        ETH0.set(ETH0_MAC, ETH0.ip(), ETH0.mask(), 0);
                        invariant!(nat::kernel_net_nat_enable(HV0.handle()).code == 1, "NAT on with no uplink");
                        ETH0.set(ETH0_MAC, ETH0.ip(), ETH0.mask(), gw);
                        invariant!(nat::kernel_net_nat_enable(HV0.handle()).code == 0, "NAT back on");
                        m = NatModel::default();
                    }
                }
                _ => {}
            },
        }
    }
}

/* ---- the whole of it: guests on the switch, NAT on through it ---- */

/// Whether `f` is sent as the guest on port `q`, which is all the switch may
/// let a guest send: IPv4 from its MAC and address, ARP from its MAC with
/// its MAC and address -- or none yet -- as the sender's.
fn as_guest(q: usize, f: &[u8]) -> bool {
    let (mac, me) = (switch::port_mac(q), switch::port_ip(q));
    if f.len() < ETH_HDR_LEN || f[6..12] != mac {
        return false;
    }
    match wire::be16(f, 12) {
        ETH_TYPE_IP => f.len() >= ETH_HDR_LEN + IP_HDR_LEN && wire::be32(f, ETH_HDR_LEN + ip::SRC) == me,
        ETH_TYPE_ARP if f.len() >= ETH_HDR_LEN + ARP_LEN => {
            let a = &f[ETH_HDR_LEN..];
            arp::sender_mac(a) == mac && (arp::sender_ip(a) == 0 || arp::sender_ip(a) == me)
        }
        _ => false,
    }
}

/// What `hv0` took in while port `p` sent: nothing but its guest's own.
fn check_hv0_took(p: usize) {
    for f in network::take_hv0_in() {
        invariant!(as_guest(p, &f), "hv0 took a frame from port {} not sent as its guest: type {:04x}, {} bytes",
                   p, if f.len() >= ETH_HDR_LEN { wire::be16(&f, 12) } else { 0 }, f.len());
    }
}

/// Guests on the switch with NAT on -- turned on as the module does, by the
/// switch's way out -- their packets for the world through the switch, `hv0`
/// and NAT out of the uplink, the world's answers back in through NAT, `hv0`
/// and the switch to a port. What a guest's NIC takes from the world must
/// be for that guest: a packet another guest's flow was answered with, in
/// this guest's inbox, is one guest reading another's traffic. And what one guest
/// takes from another, or `hv0` from a guest, must be sent as that guest.
pub fn guestnet_target(r: &mut Input) {
    network::reset();
    let outer_ip = 0xC0A8_010A;
    let gw = 0xC0A8_0101;
    ETH0.set(ETH0_MAC, outer_ip, 0xFFFF_FF00, gw);
    network::arp_learn(gw, GW_MAC);
    let sw = Switch::new().expect("a switch");
    invariant!(sw.way_out() == Ok(outer_ip), "no way out through an uplink with a gateway");
    invariant!(sw.nat_address() == Some(outer_ip), "NAT not on after the way out");
    let n = 2 + r.below(3) as usize;
    let mut backends = Vec::new();
    let mut vms = Vec::new();
    for _ in 0..n {
        let vm = Arc::new(Shared::default());
        let p = sw.claim(&vm).expect("a port");
        backends.push(sw.backend(p));
        vms.push(vm);
    }
    /* What went out, for the world to answer. */
    let mut sent: Vec<Vec<u8>> = Vec::new();

    while let Some(op) = r.op(8) {
        match op {
            0 | 1 => {
                /* A guest's packet for the world: as itself -- or, now and
                 * then, with another guest's address, or another's MAC. */
                let p = r.below(n as u64) as usize;
                let other = r.below(n as u64) as usize;
                let src_ip = if r.u8() < 16 { switch::port_ip(other) } else { switch::port_ip(p) };
                let src_mac = if r.u8() < 16 { switch::port_mac(other) } else { switch::port_mac(p) };
                let proto = r.pick(&[IP_PROTO_UDP, IP_PROTO_TCP, IP_PROTO_ICMP]);
                let dst = r.pick(&REMOTES);
                let sport = r.pick(&[40000u16, 40001, 5353]);
                let dport = r.pick(&[53u16, 80]);
                let flags = if r.u8() < 200 { tcp::SYN } else { tcp::ACK_FLAG };
                let payload = r.below(64) as usize;
                let l4 = transport(r, proto, src_ip, dst, sport, dport, payload, Sum::Right, flags, icmp::ECHO_REQUEST);
                let packet = ipv4(IP_HDR_LEN, r.u16(), 0, 64, proto, src_ip, dst, &l4, IP_HDR_LEN + l4.len(), false);
                backends[p].send(&ethernet(HOST_MAC, src_mac, &packet));
                check_hv0_took(p);
                sent.extend(ETH0.take_sent());
            }
            5 => {
                /* A guest's ARP message -- a request, a reply, a probe --
                 * for hv0 or everyone: as itself, or saying another guest's
                 * address, or hv0's, is its. */
                let p = r.below(n as u64) as usize;
                let other = r.below(n as u64) as usize;
                let sender_ip = match r.u8() % 8 {
                    0 => 0,
                    1 => switch::port_ip(other),
                    2 => HOST_IP,
                    _ => switch::port_ip(p),
                };
                let sender_mac = if r.u8() < 16 { switch::port_mac(other) } else { switch::port_mac(p) };
                let src_mac = if r.u8() < 16 { switch::port_mac(other) } else { switch::port_mac(p) };
                let target = r.pick(&[HOST_IP, switch::port_ip(other), switch::port_ip(p)]);
                let mut a = [0u8; ARP_LEN];
                arp::write(&mut a, r.pick(&[1u16, 2]), &sender_mac, sender_ip, &[0; 6], target);
                let dst = if r.u8() < 128 { MAC_BROADCAST } else { HOST_MAC };
                let mut f = ethernet(dst, src_mac, &a);
                wire::set_be16(&mut f, 12, ETH_TYPE_ARP);
                backends[p].send(&f);
                check_hv0_took(p);
            }
            6 => {
                /* A guest's DHCP request -- from no address, its own or
                 * another's -- or a frame of a protocol the switch does not
                 * carry: IPv6, as every Linux sends at boot. */
                let p = r.below(n as u64) as usize;
                let f = if r.u8() < 128 {
                    dhcp_request(r, p)
                } else {
                    let mut f = ethernet(MAC_BROADCAST, switch::port_mac(p), &noise(r.u32(), 40 + r.below(64) as usize));
                    wire::set_be16(&mut f, 12, 0x86DD);
                    f
                };
                backends[p].send(&f);
                check_hv0_took(p);
            }
            2 | 3 if !sent.is_empty() => {
                /* The world answers something that went out. */
                let out = &sent[r.below(sent.len() as u64) as usize];
                let q = &out[ETH_HDR_LEN..];
                let ihl = ip::header_len(q);
                let proto = ip::protocol(q);
                let (s, d, _) = fields(proto);
                let l4 = &q[ihl..];
                let (tsrc, tdst) = if proto == IP_PROTO_ICMP { (wire::be16(l4, s), 0) } else {
                    (wire::be16(l4, d), wire::be16(l4, s))
                };
                let kind = if proto == IP_PROTO_ICMP { icmp::ECHO_REPLY } else { 0 };
                let payload = r.below(64) as usize;
                let ans = transport(r, proto, ip::dst(q), outer_ip, tsrc, tdst, payload, Sum::Right,
                                    tcp::SYN | tcp::ACK_FLAG, kind);
                let packet = ipv4(IP_HDR_LEN, r.u16(), 0, 64, proto, ip::dst(q), outer_ip, &ans,
                                  IP_HDR_LEN + ans.len(), false);
                nat::intercept(&ETH0, &ethernet(ETH0_MAC, GW_MAC, &packet));
            }
            4 => {
                /* A guest takes what came for it: from another guest, only
                 * what that guest sent as itself; from the world, only what
                 * is for its own address. */
                let p = r.below(n as u64) as usize;
                let mut buf = vec![0u8; MAX_FRAME];
                while let Some(len) = backends[p].recv(&mut buf) {
                    let f = &buf[..len];
                    invariant!(len >= ETH_HDR_LEN, "port {} took a runt of {} bytes", p, len);
                    let from = &f[6..12];
                    invariant!(from == HOST_MAC || port_of(from).is_some_and(|q| as_guest(q, f)),
                               "port {} took a frame from {:02x?} not sent as the guest there", p, from);
                    if len >= ETH_HDR_LEN + IP_HDR_LEN && wire::be16(f, 12) == ETH_TYPE_IP {
                        let ip4 = &f[ETH_HDR_LEN..];
                        let from_world = ip::src(ip4) & MASK != SUBNET;
                        invariant!(!from_world || ip::dst(ip4) == switch::port_ip(p),
                                   "port {} took the world's packet for {:08x}, another guest's", p, ip::dst(ip4));
                    }
                }
            }
            _ => time::advance(r.pick(&[0, NS_PER_SEC, 30 * NS_PER_SEC])),
        }
    }
    drop(sw);
}
