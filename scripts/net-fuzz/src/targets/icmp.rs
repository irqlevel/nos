//! `icmp`: ping both ways, and ARP under it. The world's hosts ping the
//! machine -- well, and as no host should: to a broadcast, to an address it
//! does not have, from one no host may have, with a wrong checksum, in
//! fragments -- and ask it and tell it where they are, in ARP that is well
//! made and ARP that is not, claiming addresses that are not theirs, the
//! machine's own among them. The machine's shell pings them in turn, sends
//! them datagrams, and says what its ARP cache holds.
//!
//! An echo request to the machine's address is answered with its own id,
//! sequence and payload, to where it came from; nothing else is. A ping
//! reports a reply for a round one came back for, and for no other -- and on
//! a good link, from a host that answers, for every round. An ARP request
//! for the machine's address is answered. The cache learns only from ARP
//! that is Ethernet and IPv4 through and through, only of a host that
//! addressed the machine (RFC 826's merge: any other only updates what is
//! there), never an address no host may have, a broadcast or multicast MAC,
//! or the machine's own address; and what it holds for a host is what the
//! host said last. Nothing leaves for the link's broadcast address that is
//! not a broadcast (RFC 1122 3.3.6) -- a host that does not answer ARP is
//! not sent to at all.

use netwire::{arp, eth, icmp, udp, ARP_LEN, ETH_HDR_LEN, ETH_TYPE_ARP, ETH_TYPE_IP, ICMP_HDR_LEN, IP_PROTO_ICMP,
              IP_PROTO_UDP, MAC_BROADCAST};

use crate::input::noise;
use crate::machine::{cmd, sched, ETH0_MAC};
use crate::world::frames::{self, Ip, Sum};
use crate::world::{check, App, Link, Net, Peers, World, ETH0_IP, GW_IP, GW_MAC, MASK};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

/// The hosts: one that answers everything; one that answers ARP but not a
/// ping; one deaf to ARP, which is never reached; one beyond the gateway.
const H1: u32 = 0x0A00_0264;
const H1_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x64];
const H2: u32 = 0x0A00_0265;
const H2_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x65];
const H3: u32 = 0x0A00_0266;
const H3_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x66];
const FAR: u32 = 0x5DB8_D822;
/// Nobody is at this one.
const NOBODY: u32 = 0x0A00_024D;
/// Who claims what is not theirs.
const LIAR_MAC: [u8; 6] = [0x02, 0xBA, 0xD0, 0x00, 0x00, 0x01];
const SUBNET_BROADCAST: u32 = 0x0A00_02FF;

struct Host {
    ip: u32,
    mac: [u8; 6],
    /// Answers ARP for itself; answers a ping, this long after it.
    arp: bool,
    ping: Option<u64>,
}

/// An echo request of the world's to the machine that may be answered, and
/// when it arrives -- and whether it must be.
struct Asked {
    src: u32,
    mac: [u8; 6],
    id: u16,
    seq: u16,
    payload: Vec<u8>,
    at: u64,
    must: bool,
}

/// A ping the machine sent: to whom, its id and sequence, when.
struct Pinged {
    dst: u32,
    id: u16,
    seq: u16,
    at: u64,
}

/// What an ARP packet of the world's told whoever took it: that `ip` is at
/// `mac`, and whether it was addressed to the machine.
#[derive(Clone, Copy)]
struct Claim {
    ip: u32,
    mac: [u8; 6],
    to_us: bool,
}

struct Wire {
    hosts: Vec<Host>,
    /// Echo requests to answer, and ARP requests for the machine's address
    /// to answer: who asked, and when the question arrives.
    asked: Vec<Asked>,
    arp_asked: Vec<([u8; 6], u32, u64)>,
    pinged: Vec<Pinged>,
    /// Echo replies delivered to the machine: id, sequence, arrival.
    replies: Vec<(u16, u16, u64)>,
    /// The claims, and when each arrives: in the order they arrive, those
    /// arriving at once in the order they were sent.
    claims: Vec<(Claim, u64)>,
    /// Datagrams the machine sent, and to which MAC: destination, port,
    /// payload.
    datagrams: Vec<(u32, u16, Vec<u8>, [u8; 6])>,
    /// Somebody claimed a host's address with another MAC: from then on a
    /// ping or a datagram may go to the liar.
    lied: bool,
}

/// Whether a host may have `a` (RFC 1122 3.2.1.3).
fn usable(a: u32) -> bool {
    a >> 24 != 0 && a >> 24 != 127 && a < 0xE000_0000
}

/// Whether the cache may take `c` at all.
fn acceptable(c: &Claim) -> bool {
    usable(c.ip) && c.ip != ETH0_IP && c.ip != SUBNET_BROADCAST && c.mac[0] & 1 == 0 && c.mac != [0; 6]
        && c.mac != ETH0_MAC
}

impl Wire {
    fn host(&self, a: u32) -> Option<&Host> {
        self.hosts.iter().find(|h| h.ip == a)
    }

    /// An ARP packet to the machine, taken into the model when it arrives.
    fn arp_in(&mut self, net: &mut Net, frame: Vec<u8>, at: u64) {
        let a = &frame[ETH_HDR_LEN..];
        let well_made = netwire::be16(a, arp::HW_TYPE) == 1 && netwire::be16(a, arp::PROTO_TYPE) == ETH_TYPE_IP
            && a[arp::HW_SIZE] == 6 && a[arp::PROTO_SIZE] == 4 && matches!(arp::opcode(a), 1 | 2);
        if well_made {
            let c = Claim { ip: arp::sender_ip(a), mac: arp::sender_mac(a), to_us: arp::target_ip(a) == ETH0_IP };
            let place = self.claims.iter().rposition(|k| k.1 <= at).map_or(0, |i| i + 1);
            self.claims.insert(place, (c, at));
            if acceptable(&c) && self.hosts.iter().any(|h| h.ip == c.ip && h.mac != c.mac) {
                self.lied = true;
            }
            /* A request for the machine's address is answered: a probe's
             * too, whose sender has no address yet (RFC 5227), and nobody's
             * who is not a host. */
            if arp::opcode(a) == 1 && c.to_us && c.mac[0] & 1 == 0 && c.mac != [0; 6] && c.mac != ETH0_MAC
                && (usable(c.ip) || c.ip == 0) && c.ip != ETH0_IP && c.ip != SUBNET_BROADCAST {
                self.arp_asked.push((c.mac, c.ip, at));
            }
        }
        net.inject_at(at, frame);
    }

    /// What the machine sent, from the hosts' side.
    fn from_machine(&mut self, net: &mut Net, f: &[u8]) {
        let now = sched::now();
        match eth::ether_type(f) {
            ETH_TYPE_ARP => {
                let a = &f[ETH_HDR_LEN..ETH_HDR_LEN + ARP_LEN];
                match arp::opcode(a) {
                    1 => {
                        /* A request: the host asked for answers, if it is
                         * there and hears ARP. */
                        let want = arp::target_ip(a);
                        let Some(h) = self.host(want) else { return };
                        if !h.arp {
                            return;
                        }
                        let reply = frames::arp(ETH0_MAC, h.mac, 2, h.mac, h.ip, ETH0_MAC, arp::sender_ip(a));
                        for t in net.link.fate(now, reply.len()) {
                            self.arp_in(net, reply.clone(), t);
                        }
                    }
                    _ => {
                        /* A reply: to a request for the machine's address. */
                        let mut tmac = [0u8; 6];
                        tmac.copy_from_slice(&a[arp::TARGET_MAC..arp::TARGET_MAC + 6]);
                        let tip = arp::target_ip(a);
                        let at = self.arp_asked.iter().position(|q| q.0 == tmac && q.1 == tip && q.2 <= now);
                        invariant!(at.is_some() && eth::dst(f) == tmac && arp::sender_ip(a) == ETH0_IP,
                                   "an ARP reply to {} ({:02x?}) that nobody asked for, or not to them",
                                   netwire::Ipv4(tip), tmac);
                        self.arp_asked.remove(at.unwrap_or(0));
                    }
                }
            }
            ETH_TYPE_IP => {
                let Some(p) = check::ipv4_parts(f) else { return };
                match p.proto {
                    IP_PROTO_ICMP if icmp::kind(p.l4) == icmp::ECHO_REPLY => {
                        let (id, seq) = (icmp::id(p.l4), icmp::seq(p.l4));
                        let at = self.asked.iter().position(|q| q.src == p.dst && q.id == id && q.seq == seq
                                                            && q.at <= now);
                        invariant!(at.is_some(), "an echo reply to {} id {} seq {} that nobody asked for",
                                   netwire::Ipv4(p.dst), id, seq);
                        let q = self.asked.remove(at.unwrap_or(0));
                        invariant!(eth::dst(f) == q.mac && p.src == ETH0_IP && p.l4[ICMP_HDR_LEN..] == q.payload[..],
                                   "an echo reply to {} not to where the request came from, or not its payload",
                                   netwire::Ipv4(p.dst));
                    }
                    IP_PROTO_ICMP if icmp::kind(p.l4) == icmp::ECHO_REQUEST => {
                        let (id, seq) = (icmp::id(p.l4), icmp::seq(p.l4));
                        self.pinged.push(Pinged { dst: p.dst, id, seq, at: now });
                        /* Who it reaches: the host it is for, if it went to
                         * that host's MAC -- or to the gateway's, beyond. */
                        let via = if p.dst & MASK == ETH0_IP & MASK { p.dst } else { GW_IP };
                        let Some(h) = self.host(via) else { return };
                        if eth::dst(f) != h.mac {
                            return;
                        }
                        let delay = if via == p.dst { h.ping } else { self.host(FAR).and_then(|far| far.ping) };
                        let Some(delay) = delay else { return };
                        let payload = p.l4[ICMP_HDR_LEN..].to_vec();
                        let m = frames::icmp(icmp::ECHO_REPLY, 0, id, seq, &payload, Sum::Right);
                        let reply = frames::ipv4(ETH0_MAC, h.mac, &Ip::new(p.dst, ETH0_IP, IP_PROTO_ICMP), &m);
                        for t in net.link.fate(now + delay, reply.len()) {
                            self.replies.push((id, seq, t));
                            net.inject_at(t, reply.clone());
                        }
                    }
                    IP_PROTO_UDP => {
                        if let Some(d) = udp::parse(f) {
                            self.datagrams.push((d.dst_ip, d.dst_port, d.payload.to_vec(), eth::dst(f)));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Peers for Wire {
    fn on_frame(&mut self, net: &mut Net, frame: &[u8]) {
        self.from_machine(net, frame);
    }
}

/// An echo request of the world's to the machine: well made, or as no host
/// should send one. Whether it must be answered.
fn echo_request(w: &mut World<Wire>, r: &mut Input) {
    let from = r.pick(&[GW_IP, H1, H2, FAR]);
    let mac = if from == FAR { GW_MAC } else { w.peers.host(from).map_or(GW_MAC, |h| h.mac) };
    let (id, seq) = (r.u16(), r.u16());
    let n = match r.u8() % 8 {
        0 => 0,
        1 => r.pick(&[1472usize, 1473, 1480, 1500]),
        _ => r.below(100) as usize,
    };
    let payload = noise(r.u32(), n);
    let mut ipv = Ip::new(from, ETH0_IP, IP_PROTO_ICMP);
    let (mut sum, mut code, mut dst_mac) = (Sum::Right, 0, ETH0_MAC);
    /* Whether it may be answered, and must be. */
    let (mut may, mut must) = (true, true);
    match r.u8() % 12 {
        0 => {
            ipv.dst = r.pick(&[u32::MAX, SUBNET_BROADCAST, NOBODY, 0xE000_0001]);
            dst_mac = MAC_BROADCAST;
            may = false;
        }
        1 => {
            sum = Sum::Wrong;
            may = false;
        }
        2 => {
            ipv.frag = r.pick(&[0x2000u16, 0x0001, 0x2010]);
            may = false;
        }
        3 => {
            ipv.src = r.pick(&[0, 0x7F00_0001, 0xE000_0005, u32::MAX, ETH0_IP, SUBNET_BROADCAST]);
            may = false;
        }
        /* A code no echo has (RFC 792 says 0): answered or not. */
        4 => {
            code = r.pick(&[1u8, 8, 255]);
            must = false;
        }
        5 => ipv.options = vec![7, 7, 4, 0, 0, 0, 0],
        /* A unicast to the machine's address in a frame to everyone: it may
         * answer or not (RFC 1122 3.3.6 says it should not, Linux does). */
        6 => {
            dst_mac = MAC_BROADCAST;
            must = false;
        }
        _ => {}
    }
    let m = frames::icmp(icmp::ECHO_REQUEST, code, id, seq, &payload, sum);
    let packet = ipv.packet(&m);
    /* What is too big for a frame is not a frame. */
    if ETH_HDR_LEN + packet.len() > netwire::MAX_FRAME {
        return;
    }
    let frame = frames::eth(dst_mac, mac, ETH_TYPE_IP, &packet);
    let copies = w.net.link.fate(sched::now(), frame.len());
    for &t in &copies {
        if may {
            w.peers.asked.push(Asked { src: from, mac, id, seq, payload: payload.clone(), at: t, must });
        }
        w.net.inject_at(t, frame.clone());
    }
}

/// An ARP packet of the world's: a host asking for the machine, or for
/// another; telling where it is unasked; made wrong; or lying.
fn arp_packet(w: &mut World<Wire>, r: &mut Input) {
    let host = r.pick(&[GW_IP, H1, H2, H3]);
    let (mut smac, mut sip) = w.peers.host(host).map_or((GW_MAC, GW_IP), |h| (h.mac, h.ip));
    let mut op = 1u16;
    let mut target = ETH0_IP;
    let mut dst_mac = MAC_BROADCAST;
    let mut fields: Option<(u16, u16, u8, u8)> = None;
    match r.u8() % 10 {
        /* For another host: the machine learns nothing new of the sender. */
        0 | 1 => target = r.pick(&[GW_IP, H1, NOBODY, H3]),
        /* Unasked: a reply to nobody, a gratuitous request. */
        2 => {
            op = 2;
            dst_mac = ETH0_MAC;
            target = r.pick(&[ETH0_IP, NOBODY]);
        }
        3 => target = sip,
        /* Of another protocol, another hardware, or no operation. */
        4 => {
            fields = Some(r.pick(&[(6u16, ETH_TYPE_IP, 6u8, 4u8), (1, 0x86DD, 6, 16), (1, ETH_TYPE_IP, 8, 4),
                                   (1, ETH_TYPE_IP, 6, 6), (1, 0x0806, 6, 4)]));
        }
        5 => op = r.pick(&[0u16, 3, 4, 0xFFFF]),
        /* A sender no host may be, or the machine itself. */
        6 => sip = r.pick(&[0, 0x7F00_0001, 0xE000_0005, u32::MAX, SUBNET_BROADCAST, ETH0_IP]),
        7 => smac = r.pick(&[MAC_BROADCAST, [0x01, 0x00, 0x5E, 0, 0, 1], [0; 6], ETH0_MAC]),
        /* Another's address, claimed. */
        8 => {
            sip = r.pick(&[GW_IP, H1, H2]);
            smac = LIAR_MAC;
        }
        _ => {}
    }
    let mut frame = frames::arp(dst_mac, smac, op, smac, sip, [0; 6], target);
    if let Some((hw, proto, hl, pl)) = fields {
        let a = &mut frame[ETH_HDR_LEN..];
        netwire::set_be16(a, arp::HW_TYPE, hw);
        netwire::set_be16(a, arp::PROTO_TYPE, proto);
        a[arp::HW_SIZE] = hl;
        a[arp::PROTO_SIZE] = pl;
    }
    let copies = w.net.link.fate(sched::now(), frame.len());
    for t in copies {
        w.peers.arp_in(&mut w.net, frame.clone(), t);
    }
}

/// A shell command, run to its end: what it printed.
fn shell(w: &mut World<Wire>, line: String, within: u64) -> String {
    let app = App::spawn("shell", 1, move || cmd::run(&line));
    let done = w.wait_app(&app, within);
    invariant!(done, "a shell command took more than {} s", within / SEC);
    app.take().unwrap_or_default()
}

/// `ping`, and what it said against what the replies were.
fn ping(w: &mut World<Wire>, r: &mut Input, lossless: bool) {
    let dst = r.pick(&[H1, H1, GW_IP, FAR, H2, H3, NOBODY, ETH0_IP, SUBNET_BROADCAST, u32::MAX, 0x7F00_0001, 0,
                       0xE000_0001]);
    let host = netwire::Ipv4(dst).to_string();
    let first = w.peers.pinged.len();
    let lied = w.peers.lied;
    let out = shell(w, format!("ping {}", host), 60 * SEC);
    let pinged = &w.peers.pinged[first..];
    let mut received = 0;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("reply from ") {
            received += 1;
            let seq: u16 = rest.split("seq=").nth(1).and_then(|s| s.split(' ').next()).and_then(|s| s.parse().ok())
                .unwrap_or(u16::MAX);
            let asked = pinged.iter().find(|p| p.seq == seq && p.dst == dst);
            let answered = asked.is_some_and(|p| w.peers.replies.iter().any(|&(id, s, t)| id == p.id && s == seq
                                                                             && t >= p.at));
            invariant!(answered, "ping {} said '{}', no reply having come for it", host, line);
        }
    }
    invariant!(out.contains(&format!("{}/5 received", received)), "ping {} counted other than its replies: {:?}",
               host, out);
    /* A host that answers, on a link that loses nothing, nobody lying about
     * where it is: every round. */
    let answers = match w.peers.host(dst) {
        Some(h) if h.arp => h.ping,
        _ if dst & MASK != ETH0_IP & MASK && usable(dst) => w.peers.host(FAR).filter(|_| dst == FAR)
            .and_then(|f| f.ping),
        _ => None,
    };
    if lossless && !lied && answers.is_some_and(|d| d < 2 * SEC) {
        invariant!(received == 5, "ping {} got {} of 5 from a host that answers every one: {:?}", host, received,
                   out);
    }
    /* Nothing anywhere no packet goes. */
    if dst >> 24 == 0 || dst >> 24 == 127 || (dst >> 28 == 0xF && dst != u32::MAX) {
        invariant!(pinged.is_empty(), "ping {} sent {} echo requests", host, pinged.len());
    }
}

/// `udpsend`, and where the datagram went.
fn udpsend(w: &mut World<Wire>, r: &mut Input) {
    let dst = r.pick(&[H1, GW_IP, FAR, H3, NOBODY, SUBNET_BROADCAST, u32::MAX]);
    let port = r.pick(&[9u16, 7, 65535, 1]);
    let msg = format!("m{}", r.u16());
    let first = w.peers.datagrams.len();
    let out = shell(w, format!("udpsend {} {} {}", netwire::Ipv4(dst), port, msg), 30 * SEC);
    let sent: Vec<_> = w.peers.datagrams[first..].to_vec();
    if out.starts_with("sent ") {
        let right = sent.iter().any(|d| d.0 == dst && d.1 == port && d.2 == msg.as_bytes());
        invariant!(right, "udpsend said {:?}, and sent {:?}", out.trim(), sent);
    } else {
        invariant!(sent.is_empty(), "udpsend said {:?}, and sent {:?}", out.trim(), sent);
    }
}

/// `arp`: what the cache holds, against what the world told it.
fn cache(w: &mut World<Wire>) {
    let before = sched::now();
    let out = shell(w, "arp".to_string(), 10 * SEC);
    let after = sched::now();
    let claims: Vec<Claim> = w.peers.claims.iter().filter(|k| k.1 <= after).map(|k| k.0).collect();
    let settled = w.peers.claims.iter().filter(|k| k.1 <= before).count();
    let mut n = 0;
    for line in out.lines() {
        if line == "arp table empty" {
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(a), Some(m)) = (words.next(), words.next()) else {
            invariant!(false, "arp printed {:?}", line);
            continue;
        };
        n += 1;
        let a = netwire::parse_ipv4(a.as_bytes()).unwrap_or(0);
        let mut mac = [0u8; 6];
        for (i, part) in m.split(':').enumerate().take(6) {
            mac[i] = u8::from_str_radix(part, 16).unwrap_or(0);
        }
        let c = Claim { ip: a, mac, to_us: false };
        invariant!(acceptable(&c), "the ARP cache holds {} at {}", line, "an address or MAC nobody may be at");
        invariant!(claims.iter().any(|k| k.ip == a && k.mac == mac), "the ARP cache holds {}, which nothing said",
                   line);
        invariant!(claims.iter().any(|k| k.ip == a && k.to_us), "the ARP cache holds {}, a host that never \
                   addressed the machine (RFC 826: another's request only updates what is there)", line);
        let last = claims[..settled].iter().rev().find(|k| k.ip == a && acceptable(k)).map(|k| k.mac);
        let late = claims[settled..].iter().any(|k| k.ip == a && k.mac == mac);
        invariant!(last == Some(mac) || late, "the ARP cache holds {}, the last word of it being {:02x?}", line,
                   last);
    }
    invariant!(n <= 16, "the ARP cache holds {} entries", n);
}

pub fn icmp_target(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(ETH0_IP);
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);

    let delay = |r: &mut Input| Some(r.pick(&[MS, 20 * MS, 500 * MS, 5 * SEC]));
    let hosts = vec![
        Host { ip: GW_IP, mac: GW_MAC, arp: true, ping: Some(MS) },
        Host { ip: H1, mac: H1_MAC, arp: true, ping: delay(r) },
        Host { ip: H2, mac: H2_MAC, arp: true, ping: None },
        Host { ip: H3, mac: H3_MAC, arp: false, ping: Some(MS) },
        Host { ip: FAR, mac: GW_MAC, arp: false, ping: if r.bool() { delay(r) } else { None } },
    ];
    let link = if r.u8() < 200 { Link::perfect() } else { Link::from_input(r) };
    let lossless = link.lossless();
    let mut w = World::new(link, Wire { hosts, asked: Vec::new(), arp_asked: Vec::new(), pinged: Vec::new(),
                                        replies: Vec::new(), claims: Vec::new(), datagrams: Vec::new(),
                                        lied: false });

    while let Some(op) = r.op(12) {
        match op {
            0..=3 => echo_request(&mut w, r),
            4..=6 => arp_packet(&mut w, r),
            7 if r.u8() < 64 => ping(&mut w, r, lossless),
            8 if r.u8() < 128 => udpsend(&mut w, r),
            9 => cache(&mut w),
            _ => w.run_for(r.below(3000) * MS),
        }
        w.pump();
        let now = sched::now();
        if lossless {
            let unanswered = w.peers.asked.iter().find(|q| q.must && q.at <= now);
            invariant!(unanswered.is_none(), "an echo request to the machine unanswered: from {} id {} seq {}",
                       netwire::Ipv4(unanswered.map_or(0, |q| q.src)), unanswered.map_or(0, |q| q.id),
                       unanswered.map_or(0, |q| q.seq));
            let unanswered = w.peers.arp_asked.iter().find(|q| q.2 <= now);
            invariant!(unanswered.is_none(), "an ARP request for the machine unanswered: from {:02x?}", unanswered);
        }
        w.peers.asked.retain(|q| q.at > now);
        w.peers.arp_asked.retain(|q| q.2 > now);
    }
    w.run_for(10 * SEC);
    cache(&mut w);
    super::audit();
}
