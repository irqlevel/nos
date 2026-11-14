//! `dhcp`: the kernel's DHCP client getting an address from the servers on
//! the wire, and keeping it -- renewing it before its time is up, giving it
//! up when a server says no or its time runs out. The servers answer as the
//! input says: an offer and an ACK, a NAK, silence, an answer for another
//! transaction or another client, an address no host may have, options made
//! to break a parser, bytes of no protocol; a second server offers another
//! address, a rogue one answers first; and a server's lease changes under
//! the client -- a new mask, a new router, another address altogether.
//!
//! What the client sends must be the message its state calls for: a
//! DISCOVER from nowhere; a REQUEST for what an offer said, naming the
//! server that made it, in the offer's transaction; a renewal from the
//! address it holds, only while it holds it, to the server that granted it
//! or to everyone. What it binds to must be what an ACK -- for this client,
//! in its transaction -- said, all of it; what the device has, what the
//! client says it holds; an address no host may have, never; and an address
//! past its lease, or after a server refused to renew it, not for longer
//! than the client takes to notice.

use netwire::{eth, udp, IP_PROTO_UDP, MAC_BROADCAST};

use crate::input::noise;
use crate::machine::{sched, ETH0_MAC};
use crate::world::frames::{self, Ip, Sum};
use crate::world::lan::Lan;
use crate::world::{App, Link, Net, Peers, World, DNS_IP, GW_IP, GW_MAC};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
const COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
/// The second server.
const OTHER_IP: u32 = 0x0A00_02FE;
const OTHER_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0xFE];
/// A server nobody set up, answering the client's transaction first.
const ROGUE_IP: u32 = 0x0A00_0299;
const ROGUE_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x99];
/// Another client on the LAN.
const STRANGER_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x07];
/// How long the client may take to act on what it was told: to let an
/// address go once its lease is up or its renewal refused, to put a renewed
/// lease on the device.
const SLACK: u64 = 2 * SEC;
/// The shortest lease the client takes: a shorter one is taken as this, as
/// dhcpcd does, so that a server that says 0 does not have it asking again
/// and again.
const MIN_LEASE: u64 = 20;
/// A lease with no end (RFC 2131 3.3).
const INFINITE: u32 = 0xFFFF_FFFF;
/// A NAK is the client's to hear if it comes this soon after the request:
/// one that comes after the client has stopped waiting is nobody's.
const HEARD: u64 = SEC;

/* Message types */
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;

/// What a server does with the client's next message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mood {
    Serve,
    /// Serves, and the second server offers its own lease too.
    Both,
    Nak,
    Silent,
    /// Answers another transaction.
    OtherXid,
    /// Answers another client.
    OtherClient,
    /// Offers, or grants, an address no host may have.
    BadAddress,
    /// Answers with options made to break a parser -- none of which leave a
    /// message type to be read.
    Broken,
    /// Answers with bytes of no protocol.
    Garbage,
}

/// A lease as a server states it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Terms {
    ip: u32,
    mask: u32,
    router: u32,
    dns: u32,
    secs: u32,
}

/// An ACK the client may have taken: from whom, what it said, when it
/// first arrived.
#[derive(Clone, Copy, Debug)]
struct Ack {
    server: u32,
    terms: Terms,
    arrival: u64,
    /// Granted before the client was last stopped: nobody keeps it since.
    abandoned: bool,
}

struct Server {
    ip: u32,
    mac: [u8; 6],
    terms: Terms,
}

/// A client's message, as a server reads it.
#[derive(Clone, Copy, Debug)]
struct Msg {
    kind: u8,
    xid: u32,
    ciaddr: u32,
    /// Options 50 and 54: the address asked for, the server asked.
    requested: u32,
    server: u32,
    src: u32,
    dst: u32,
    dst_mac: [u8; 6],
}

/// What a server's answer is, to the model, once it arrives.
enum Note {
    None,
    Offer(u32, u32),
    Ack(Terms, u32),
    /// A NAK of a renewal of this address.
    Refused(u32),
    /// An address no host may have, offered by a server, or granted.
    BadOffer(u32, u32),
    BadAck(u32),
}

/// Whether a host may have `ip` on a subnet of `mask` (RFC 1122 3.2.1.3):
/// not "this network", not loopback, not multicast or reserved, not the
/// subnet's broadcast.
fn usable(ip: u32, mask: u32) -> bool {
    ip >> 24 != 0 && ip >> 24 != 127 && ip < 0xE000_0000 && !(mask.count_zeros() >= 2 && ip | mask == u32::MAX)
}

/// When a lease stated in `secs`, from `from`, ends.
fn lease_end(from: u64, secs: u32) -> u64 {
    if secs == INFINITE {
        u64::MAX
    } else {
        from.saturating_add(u64::from(secs).max(MIN_LEASE) * SEC)
    }
}

fn be32(v: &[u8]) -> u32 {
    u32::from_be_bytes([v[0], v[1], v[2], v[3]])
}

fn ip(a: u32) -> netwire::Ipv4 {
    netwire::Ipv4(a)
}

/// A time, as seconds into the input.
fn at(t: u64) -> String {
    if t == u64::MAX {
        return "never".to_string();
    }
    format!("{:.3} s", t.saturating_sub(sched::START) as f64 / 1e9)
}

/// The DHCP message in a frame the machine sent, checked as a server
/// reads one: the fixed part a client fills in, options that end.
fn client_message(f: &[u8]) -> Option<Msg> {
    let d = udp::parse(f)?;
    if d.dst_port != 67 {
        return None;
    }
    invariant!(d.src_port == 68, "a DHCP message from port {}", d.src_port);
    let m = d.payload;
    invariant!(m.len() >= 240 && m[0] == 1 && m[1] == 1 && m[2] == 6, "a client message of {} bytes starting {:02x?}",
               m.len(), &m[..m.len().min(4)]);
    invariant!(m[28..34] == ETH0_MAC, "a client message with another's hardware address {:02x?}", &m[28..34]);
    invariant!(m[236..240] == COOKIE, "a client message with no magic cookie");
    let (mut kind, mut requested, mut server, mut ended) = (0, 0, 0, false);
    let mut i = 240;
    while i < m.len() {
        let code = m[i];
        if code == 255 {
            ended = true;
            break;
        }
        if code == 0 {
            i += 1;
            continue;
        }
        invariant!(i + 1 < m.len() && i + 2 + usize::from(m[i + 1]) <= m.len(), "a client option {} past the \
                   message's end", code);
        let v = &m[i + 2..i + 2 + usize::from(m[i + 1])];
        match (code, v.len()) {
            (53, 1) => kind = v[0],
            (50, 4) => requested = be32(v),
            (54, 4) => server = be32(v),
            (53 | 50 | 54, n) => invariant!(false, "a client option {} of {} bytes", code, n),
            _ => {}
        }
        i += 2 + v.len();
    }
    invariant!(ended, "a client message with no end option");
    invariant!(kind == DISCOVER || kind == REQUEST, "a client message of type {}", kind);
    Some(Msg { kind, xid: be32(&m[4..8]), ciaddr: be32(&m[12..16]), requested, server, src: d.src_ip, dst: d.dst_ip,
               dst_mac: eth::dst(f) })
}

/// A server's message: `kind` in transaction `xid` to `chaddr`, giving
/// `yiaddr` on `terms` -- or, `broken`, those options in place of any.
fn reply(xid: u32, kind: u8, yiaddr: u32, terms: &Terms, server: u32, chaddr: [u8; 6], broken: Option<Vec<u8>>)
         -> Vec<u8> {
    let mut m = vec![0u8; 240];
    m[0] = 2;
    m[1] = 1;
    m[2] = 6;
    m[4..8].copy_from_slice(&xid.to_be_bytes());
    if kind != NAK {
        m[16..20].copy_from_slice(&yiaddr.to_be_bytes());
    }
    m[28..34].copy_from_slice(&chaddr);
    m[236..240].copy_from_slice(&COOKIE);
    if let Some(b) = broken {
        m.extend_from_slice(&b);
        return m;
    }
    m.extend_from_slice(&[53, 1, kind, 54, 4]);
    m.extend_from_slice(&server.to_be_bytes());
    if kind != NAK {
        /* What the server has none of, it leaves out. */
        for (code, v) in [(1u8, terms.mask), (3, terms.router), (6, terms.dns)] {
            if v != 0 {
                m.extend_from_slice(&[code, 4]);
                m.extend_from_slice(&v.to_be_bytes());
            }
        }
        m.extend_from_slice(&[51, 4]);
        m.extend_from_slice(&terms.secs.to_be_bytes());
    }
    m.push(255);
    m
}

/// Options made to break a parser: lengths past the end, no end, every
/// option the wrong length -- and in none of them a message type a parser
/// that keeps to the lengths can find, so that whatever the client makes
/// of one, a lease is not it.
fn broken_options(seed: u32) -> Vec<u8> {
    match seed % 7 {
        /* A mask of 200 bytes, running past the end, the type after it. */
        0 => vec![1, 200, 255, 255, 255, 0, 53, 1, 5, 255],
        /* A type of no bytes. */
        1 => vec![53, 0, 255],
        /* Padding, and no end. */
        2 => vec![0; 40],
        /* A type cut short. */
        3 => vec![53],
        /* The end before the type. */
        4 => vec![255, 53, 1, 5],
        /* A lease time of two bytes, a router of nine, no type. */
        5 => vec![51, 2, 0, 1, 3, 9, 1, 2, 3, 4, 5, 6, 7, 8, 9, 255],
        _ => {
            /* Options of every code but the type's and the end's, of every
             * length, the last maybe past the end. */
            let n = noise(seed, 256);
            let mut b = Vec::new();
            let mut i = 0;
            while i + 2 < n.len() && b.len() < 120 {
                let code = match n[i] {
                    53 => 52,
                    255 => 254,
                    c => c,
                };
                b.push(code);
                if code == 0 {
                    i += 1;
                    continue;
                }
                let len = if n[i + 1] < 16 { 200 } else { n[i + 1] % 12 };
                b.push(len);
                let take = usize::from(len).min(n.len() - i - 2).min(12);
                b.extend_from_slice(&n[i + 2..i + 2 + take]);
                i += 2 + take;
            }
            b
        }
    }
}

struct Wire {
    lan: Lan,
    nic: net::Nic,
    client: &'static net::dhcp::Dhcp,
    /// The first server (at the gateway, as QEMU's is), and a second.
    servers: [Server; 2],
    moods: Vec<Mood>,
    served: usize,
    /// How long a server takes to answer.
    delay: u64,
    /// Every message the client sent.
    seen: Vec<Msg>,
    /// Every offer made to this client: transaction, address, server, and
    /// whether a host may have the address.
    offers: Vec<(u32, u32, u32, bool)>,
    /// Every usable ACK to this client that arrives.
    acks: Vec<Ack>,
    /// Renewals refused in time to be heard: the address, when the NAK came.
    naks: Vec<(u32, u64)>,
    /// When the model looks at the device next, whatever else happens.
    checks: Vec<u64>,
    /// Since when the device has had other than the lease the client holds,
    /// and what.
    mismatch: Option<(u64, (u32, u32, u32))>,
}

impl Wire {
    /// Whether the machine may have `a` at `now`: an ACK gave it, and
    /// neither its time nor a refusal to renew it has ended it since.
    fn holds_rightly(&self, a: u32, now: u64) -> bool {
        self.acks.iter().any(|k| k.terms.ip == a && k.arrival <= now
            && (k.abandoned || now <= self.end_of(k).saturating_add(SLACK)))
    }

    /// When the lease an ACK gave ends: its time, or a refusal to renew it.
    fn end_of(&self, k: &Ack) -> u64 {
        let refused = self.naks.iter().filter(|n| n.0 == k.terms.ip && n.1 > k.arrival).map(|n| n.1).min();
        lease_end(k.arrival, k.terms.secs).min(refused.unwrap_or(u64::MAX))
    }

    /// Everything the servers said of `a`, for a report.
    fn story(&self, a: u32) -> String {
        let mut s = String::new();
        for k in self.acks.iter().filter(|k| k.terms.ip == a) {
            s += &format!("[ACK from {} at {} for {} s, ends {}{}] ", ip(k.server), at(k.arrival), k.terms.secs,
                          at(self.end_of(k)), if k.abandoned { ", abandoned" } else { "" });
        }
        for n in self.naks.iter().filter(|n| n.0 == a) {
            s += &format!("[its renewal refused at {}] ", at(n.1));
        }
        if s.is_empty() {
            s = "no ACK gave it".to_string();
        }
        s
    }

    /// What the device has, and what the client says it holds, against what
    /// the servers said.
    fn check_device(&mut self, now: u64) {
        let (a, mask, gw) = (self.nic.ip(), self.nic.mask(), self.nic.gw());
        if a != 0 {
            invariant!(usable(a, mask), "the device has {} mask {}, an address no host may have", ip(a), ip(mask));
            invariant!(self.holds_rightly(a, now), "the device has {} at {}: {}", ip(a), at(now), self.story(a));
        }
        if !self.client.is_ready() {
            self.mismatch = None;
            return;
        }
        let l = self.client.lease();
        let t = Terms { ip: l.ip, mask: l.mask, router: l.router, dns: l.dns, secs: l.lease_secs };
        invariant!(usable(t.ip, t.mask), "bound to {} mask {}, an address no host may have", ip(t.ip), ip(t.mask));
        invariant!(self.acks.iter().any(|k| k.terms == t && k.server == l.server_ip),
                   "bound to {} mask {} router {} dns {} for {} s from {}, which no ACK for this client said",
                   ip(t.ip), ip(t.mask), ip(t.router), ip(t.dns), t.secs, ip(l.server_ip));
        let device = (a, mask, gw);
        if device == (t.ip, t.mask, t.router) {
            self.mismatch = None;
            return;
        }
        match self.mismatch {
            Some((since, d)) if d == device => {
                invariant!(now <= since + SLACK, "the device has had {} mask {} gateway {} since {}, the lease the \
                           client holds being {} mask {} router {}", ip(a), ip(mask), ip(gw), at(since), ip(t.ip),
                           ip(t.mask), ip(t.router));
            }
            _ => {
                self.mismatch = Some((now, device));
                self.checks.push(now + SLACK + 1);
            }
        }
    }

    /// A message of the client's, against what its state calls for.
    fn check_message(&self, m: &Msg, now: u64) {
        match (m.kind, m.ciaddr) {
            (DISCOVER, c) => invariant!(m.src == 0 && m.dst == u32::MAX && c == 0,
                                        "a DISCOVER from {} to {} with ciaddr {}", ip(m.src), ip(m.dst), ip(c)),
            (_, 0) => {
                invariant!(m.src == 0 && m.dst == u32::MAX, "a selecting REQUEST from {} to {}", ip(m.src),
                           ip(m.dst));
                let offer = self.offers.iter().find(|o| o.0 == m.xid && o.1 == m.requested && o.2 == m.server);
                invariant!(offer.is_some(), "a REQUEST for {} of {} in transaction {:08x}, which no offer made",
                           ip(m.requested), ip(m.server), m.xid);
                invariant!(offer.is_some_and(|o| o.3), "a REQUEST for {}, offered by {} on terms that make it an \
                           address no host may have", ip(m.requested), ip(m.server));
            }
            (_, c) => {
                /* RFC 2131 4.3.2, table 5: from the address, which is in
                 * ciaddr, and neither option 50 nor 54. */
                invariant!(m.src == c && m.requested == 0 && m.server == 0,
                           "a renewal from {} of {}, asking {} of {}", ip(m.src), ip(c), ip(m.requested),
                           ip(m.server));
                invariant!(self.holds_rightly(c, now), "a renewal of {} at {}: {}", ip(c), at(now), self.story(c));
                if m.dst == u32::MAX {
                    invariant!(m.dst_mac == MAC_BROADCAST, "a renewal to the broadcast address sent to {:02x?}",
                               m.dst_mac);
                } else {
                    invariant!(self.acks.iter().any(|k| k.server == m.dst && k.terms.ip == c)
                               && self.lan.hosts.iter().any(|h| h.1 == m.dst_mac),
                               "a renewal of {} to {} ({:02x?}), which never granted it", ip(c), ip(m.dst),
                               m.dst_mac);
                }
            }
        }
    }

    /// The servers' answer to a message of the client's, as `mood` has it.
    fn answer(&mut self, net: &mut Net, m: &Msg, mood: Mood, now: u64) {
        /* Who answers: the first server a DISCOVER; the server a selecting
         * REQUEST names; the one a renewal is sent to, or -- broadcast, which
         * is rebinding -- the one whose address it is, or the first, which
         * refuses what is nobody's. */
        let who = match (m.kind, m.ciaddr) {
            (DISCOVER, _) => Some(0),
            (_, 0) => self.servers.iter().position(|s| s.ip == m.server),
            (_, _) if m.dst != u32::MAX => self.servers.iter().position(|s| s.ip == m.dst),
            (_, c) => Some(self.servers.iter().position(|s| s.terms.ip == c).unwrap_or(0)),
        };
        let Some(who) = who else { return };
        let (sip, terms) = (self.servers[who].ip, self.servers[who].terms);
        let asked = if m.ciaddr != 0 { m.ciaddr } else { m.requested };
        let kind = match m.kind {
            DISCOVER if mood == Mood::Nak => NAK,
            DISCOVER => OFFER,
            _ if mood != Mood::Nak && asked == terms.ip => ACK,
            _ => NAK,
        };
        let right = match kind {
            OFFER => Note::Offer(terms.ip, sip),
            ACK => Note::Ack(terms, sip),
            _ if m.ciaddr != 0 => Note::Refused(m.ciaddr),
            _ => Note::None,
        };
        let mut out: Vec<(usize, Vec<u8>, Note)> = Vec::new();
        match mood {
            Mood::Silent => {}
            Mood::Garbage => out.push((who, noise(m.xid ^ 0x5A5A, 300), Note::None)),
            Mood::Broken => out.push((who, reply(m.xid, kind, terms.ip, &terms, sip, ETH0_MAC,
                                                 Some(broken_options(m.xid))), Note::None)),
            Mood::OtherXid | Mood::OtherClient => {
                let t = Terms { ip: terms.ip ^ 0x80, secs: 7777, ..terms };
                let (xid, chaddr) = if mood == Mood::OtherXid { (m.xid ^ 0x0100_0000, ETH0_MAC) }
                                    else { (m.xid, STRANGER_MAC) };
                out.push((who, reply(xid, kind, t.ip, &t, sip, chaddr, None), Note::None));
            }
            Mood::BadAddress if kind != NAK => {
                let mut bad = [0, 0x7F00_0001, 0xE000_0005, 0xF000_0001, u32::MAX, 5, terms.ip | !terms.mask]
                    [(m.xid % 7) as usize];
                if usable(bad, terms.mask) {
                    /* A subnet of one or two addresses has no broadcast of
                     * its own. */
                    bad = u32::MAX;
                }
                let t = Terms { ip: bad, ..terms };
                let note = if kind == OFFER { Note::BadOffer(bad, sip) } else { Note::BadAck(bad) };
                out.push((who, reply(m.xid, kind, bad, &t, sip, ETH0_MAC, None), note));
            }
            _ => {
                out.push((who, reply(m.xid, kind, terms.ip, &terms, sip, ETH0_MAC, None), right));
                if mood == Mood::Both && m.kind == DISCOVER {
                    let o = &self.servers[1];
                    out.push((1, reply(m.xid, OFFER, o.terms.ip, &o.terms, o.ip, ETH0_MAC, None),
                              Note::Offer(o.terms.ip, o.ip)));
                }
            }
        }
        /* To the address a renewal came from; otherwise, the client having
         * none, to everyone (RFC 2131 4.1). */
        let to = if m.ciaddr != 0 { m.ciaddr } else { u32::MAX };
        for (s, msg, note) in out {
            let (sip, smac) = (self.servers[s].ip, self.servers[s].mac);
            let dg = frames::udp(sip, to, 67, 68, &msg, Sum::Right);
            let dst_mac = if to == u32::MAX { MAC_BROADCAST } else { ETH0_MAC };
            let frame = frames::ipv4(dst_mac, smac, &Ip::new(sip, to, IP_PROTO_UDP), &dg);
            let arrivals: Vec<u64> = net.link.fate(now, frame.len()).into_iter().map(|t| t + self.delay).collect();
            for &t in &arrivals {
                net.inject_at(t, frame.clone());
            }
            let Some(&first) = arrivals.iter().min() else { continue };
            self.note(net, m.xid, note, first, now);
        }
    }

    /// What arrives for the client, into the model.
    fn note(&mut self, net: &mut Net, xid: u32, note: Note, arrival: u64, now: u64) {
        match note {
            Note::None => {}
            Note::Offer(a, server) => self.offers.push((xid, a, server, true)),
            Note::BadOffer(a, server) => self.offers.push((xid, a, server, false)),
            Note::Ack(terms, server) => {
                self.acks.push(Ack { server, terms, arrival, abandoned: false });
                if !net.expect.ips.contains(&terms.ip) {
                    net.expect.ips.push(terms.ip);
                }
                self.checks.push(lease_end(arrival, terms.secs).saturating_add(SLACK + 1));
            }
            Note::Refused(a) => {
                if arrival <= now + HEARD {
                    self.naks.push((a, arrival));
                    self.checks.push(arrival + SLACK + 1);
                }
            }
            Note::BadAck(a) => {
                /* So that a frame from it is judged here, by what it is. */
                if !net.expect.ips.contains(&a) {
                    net.expect.ips.push(a);
                }
            }
        }
    }
}

impl Peers for Wire {
    fn on_frame(&mut self, net: &mut Net, frame: &[u8]) {
        let now = sched::now();
        self.check_device(now);
        if self.lan.on_frame(net, frame) {
            return;
        }
        let Some(m) = client_message(frame) else { return };
        self.check_message(&m, now);
        self.seen.push(m);
        let mood = self.moods.get(self.served).copied().unwrap_or(Mood::Serve);
        self.served += 1;
        self.answer(net, &m, mood, now);
    }

    fn next_timer(&self) -> u64 {
        self.checks.iter().copied().min().unwrap_or(u64::MAX)
    }

    fn on_timer(&mut self, _net: &mut Net) {
        let now = sched::now();
        self.checks.retain(|&t| t > now);
        self.check_device(now);
    }
}

/// A lease a server gives: an address a host may have on its subnet.
fn terms(r: &mut Input) -> Terms {
    let mut t = Terms {
        ip: r.pick(&[0x0A00_020Fu32, 0x0A00_0263, 0xC0A8_0164]),
        mask: r.pick(&[0xFFFF_FF00u32, 0xFFFF_0000, 0xFFFF_FFFC, 0xFFFF_FFFF, 0]),
        router: r.pick(&[GW_IP, 0, 0x0808_0808]),
        dns: r.pick(&[DNS_IP, 0]),
        secs: match r.u8() % 4 {
            0 => r.pick(&[0u32, 1, 19, 20, 21, INFINITE, INFINITE - 1, 0x8000_0000]),
            _ => r.pick(&[60u32, 120, 600, 3600]),
        },
    };
    if !usable(t.ip, t.mask) {
        t.ip ^= 1;
    }
    t
}

/// An answer nobody asked for, or the right transaction's from a server
/// nobody set up.
fn stray(w: &mut World<Wire>, r: &mut Input) {
    let now = sched::now();
    let xid = w.peers.seen.last().map_or(0, |m| m.xid);
    let rogue = Terms { ip: 0x0A00_0277, mask: 0xFFFF_FF00, router: ROGUE_IP, dns: ROGUE_IP, secs: 60 };
    let (src, mac, msg, note) = match r.u8() % 4 {
        0 => (ROGUE_IP, ROGUE_MAC, reply(xid, OFFER, rogue.ip, &rogue, ROGUE_IP, ETH0_MAC, None),
              Note::Offer(rogue.ip, ROGUE_IP)),
        1 => (ROGUE_IP, ROGUE_MAC, reply(xid, ACK, rogue.ip, &rogue, ROGUE_IP, ETH0_MAC, None),
              Note::Ack(rogue, ROGUE_IP)),
        2 => {
            let t = w.peers.servers[0].terms;
            (GW_IP, GW_MAC, reply(r.u32(), ACK, t.ip, &t, GW_IP, ETH0_MAC, None), Note::None)
        }
        _ => (GW_IP, GW_MAC, noise(r.u32(), r.below(600) as usize), Note::None),
    };
    let dg = frames::udp(src, u32::MAX, 67, 68, &msg, Sum::Right);
    w.net.inject(frames::ipv4(MAC_BROADCAST, mac, &Ip::new(src, u32::MAX, IP_PROTO_UDP), &dg));
    w.peers.note(&mut w.net, xid, note, now, now);
    w.pump();
}

/// The client stopped, from a task as the way down stops it: waits for its
/// task, which may be sleeping out a retry.
fn stop(w: &mut World<Wire>, client: &'static net::dhcp::Dhcp) {
    let app = App::spawn("dhcp-stop", 1, move || client.stop());
    let stopped = w.wait_app(&app, 60 * SEC);
    invariant!(stopped, "the DHCP client took a minute to stop");
    invariant!(!client.is_ready(), "a stopped DHCP client says it holds a lease, which nobody keeps now");
    for k in w.peers.acks.iter_mut() {
        k.abandoned = true;
    }
}

struct Nobody;

impl net::nic::UdpHandler for Nobody {
    fn on_frame(&'static self, _frame: net::nic::Lent<'_>, _rx: &mut net::nic::RxContext) {}
}

static NOBODY: Nobody = Nobody;

pub fn dhcp(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(0);
    nic.set_mask(0);
    nic.set_gw(0);

    let first = terms(r);
    let mut second = Terms { ip: first.ip ^ 0x40, ..first };
    if !usable(second.ip, second.mask) {
        second.ip ^= 1;
    }
    let moods = (0..32).map(|_| match r.u8() % 20 {
        0 => Mood::Nak,
        1 | 2 => Mood::Silent,
        3 => Mood::OtherXid,
        4 => Mood::OtherClient,
        5 => Mood::BadAddress,
        6 => Mood::Broken,
        7 => Mood::Garbage,
        8 | 9 => Mood::Both,
        _ => Mood::Serve,
    }).collect();
    let link = if r.u8() < 200 { Link::perfect() } else { Link::from_input(r) };
    let delay = if r.u8() < 220 { r.below(20) * MS } else { r.below(4000) * MS };
    let mut lan = Lan::new();
    lan.add(OTHER_IP, OTHER_MAC);
    let client: &'static net::dhcp::Dhcp = Box::leak(Box::new(net::dhcp::Dhcp::new().expect("a client")));
    let mut w = World::new(link, Wire {
        lan,
        nic,
        client,
        servers: [Server { ip: GW_IP, mac: GW_MAC, terms: first }, Server { ip: OTHER_IP, mac: OTHER_MAC,
                                                                              terms: second }],
        moods,
        served: 0,
        delay,
        seen: Vec::new(),
        offers: Vec::new(),
        acks: Vec::new(),
        naks: Vec::new(),
        checks: Vec::new(),
        mismatch: None,
    });
    w.net.expect.ips = Vec::new();
    w.net.expect.dhcp_from_nothing = true;

    invariant!(client.start(nic), "the DHCP client would not start");
    invariant!(!client.start(nic), "the DHCP client started twice");
    let mut running = true;

    while let Some(op) = r.op(12) {
        match op {
            0..=5 => {
                let step = match r.u8() % 16 {
                    0..=9 => r.below(3000) * MS,
                    10..=14 => r.below(60) * SEC,
                    _ => r.pick(&[300u64, 1800, 3600]) * SEC,
                };
                w.run_for(step);
            }
            6 => {
                /* The first server's lease changes but for its address: a
                 * renewal is told the new mask, router, server, time. */
                let t = terms(r);
                let s = &mut w.peers.servers[0].terms;
                *s = Terms { ip: s.ip, ..t };
                if !usable(s.ip, s.mask) {
                    s.mask = 0xFFFF_FF00;
                }
            }
            7 => {
                /* Its address changes: a renewal of the old one is refused. */
                let s = &mut w.peers.servers[0].terms;
                s.ip = r.pick(&[0x0A00_0264u32, 0x0A00_0265, 0x0A00_020F]);
                if !usable(s.ip, s.mask) {
                    s.ip ^= 2;
                }
            }
            8 if running && r.u8() < 48 => {
                stop(&mut w, client);
                running = false;
            }
            9 if !running => {
                invariant!(client.start(nic), "the DHCP client would not start again");
                running = true;
            }
            10 | 11 => stray(&mut w, r),
            _ => {}
        }
        w.peers.check_device(sched::now());
    }

    /* The servers answer every message now, at once, over a good link: a
     * client that is running has a lease within a minute or two. */
    w.peers.moods.clear();
    w.peers.delay = 0;
    w.net.link = Link::perfect();
    if !running {
        invariant!(client.start(nic), "the DHCP client would not start again");
    }
    let end = sched::now() + 120 * SEC;
    let bound = w.run_until(end, || client.is_ready());
    invariant!(bound, "no lease in two minutes from servers that answer every message at once");
    w.peers.check_device(sched::now());
    stop(&mut w, client);

    /* Its port given back. */
    let app = App::spawn("port-68", 1, move || nic.listen(net::dhcp::CLIENT_PORT, &NOBODY).is_ok());
    let done = w.wait_app(&app, 10 * SEC);
    invariant!(done && app.take() == Some(true), "port 68 still taken after the DHCP client stopped");
    w.run_for(10 * SEC);
    super::audit();
}
