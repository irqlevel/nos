//! `dns`: the kernel's resolver asking a server on the wire, which answers
//! as the input says -- the address, several, a chain of CNAMEs first, a
//! name compressed every way a pointer can go, an error, nothing -- or not
//! as the server at all: an answer from another address or port, or with
//! another transaction id, or cut short, or of no protocol.
//!
//! The resolver takes the first A record of an answer from its server, on
//! its port, with the id it asked with, and nothing else: a forged answer
//! is never an address, and the right answer after one still is. What it
//! was told it keeps as long as the TTL says -- a day at most -- and not a
//! moment longer; and a name that cannot be a name is never asked.

use netwire::{udp, IP_PROTO_UDP};

use crate::input::noise;
use crate::machine::{sched, ETH0_MAC};
use crate::world::frames::{self, Ip, Sum};
use crate::world::lan::Lan;
use crate::world::{App, Link, Net, Peers, World, DNS_IP, ETH0_IP, GW_IP, MASK};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
const DNS_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x03];
/// The resolver's own port, and the most it keeps a record.
const CLIENT_PORT: u16 = 10053;
const MAX_TTL: u64 = 86_400;

/// How the server answers the next query.
#[derive(Clone)]
enum Reply {
    /// Well: these addresses as A records, after these CNAMEs, with this
    /// TTL -- names compressed or not.
    Answer { cnames: usize, addrs: Vec<u32>, ttl: u32, compress: bool, lead: Vec<Forged> },
    /// An error code (NXDOMAIN and the rest), or an answer with no A in it.
    Error(u16),
    Nothing,
    /// Bytes of the input's in place of a message.
    Garbage(Vec<u8>),
    /// A well-formed header and question, and then records made to break a
    /// parser: labels past the end, pointers anywhere, counts that lie.
    Broken(Vec<u8>),
}

/// An answer that is not the server's, sent ahead of the real one.
#[derive(Clone, Copy)]
enum Forged {
    WrongId,
    WrongAddress,
    WrongPort,
    NotAResponse,
}

struct Server {
    replies: Vec<Reply>,
    /// Queries asked, and their ids and names.
    queries: Vec<(u16, Vec<u8>)>,
    delay: u64,
}

struct Wire {
    lan: Lan,
    server: Server,
}

impl Peers for Wire {
    fn on_frame(&mut self, net: &mut Net, frame: &[u8]) {
        if self.lan.on_frame(net, frame) {
            return;
        }
        let Some(d) = udp::parse(frame) else { return };
        if d.dst_ip != DNS_IP {
            return;
        }
        invariant!(d.dst_port == 53 && d.src_port == CLIENT_PORT && d.src_ip == ETH0_IP,
                   "a query to {}:{} from {}:{}", netwire::Ipv4(d.dst_ip), d.dst_port, netwire::Ipv4(d.src_ip),
                   d.src_port);
        let (id, name) = check_query(d.payload);
        self.server.queries.push((id, name.clone()));
        let reply = self.server.replies.get(self.server.queries.len() - 1).cloned().unwrap_or(Reply::Nothing);
        let at = sched::now() + self.server.delay;
        for msg in answer(id, &name, &reply) {
            let (src_ip, src_port, m) = msg;
            let dg = frames::udp(src_ip, ETH0_IP, src_port, CLIENT_PORT, &m, Sum::Right);
            net.inject_at(at, frames::ipv4(ETH0_MAC, DNS_MAC, &Ip::new(src_ip, ETH0_IP, IP_PROTO_UDP), &dg));
        }
    }
}

/// A query, as RFC 1035 has one: its id, and the name asked -- one
/// question, of an A record, in class IN, recursion desired, and nothing
/// else in it.
fn check_query(q: &[u8]) -> (u16, Vec<u8>) {
    invariant!(q.len() >= 12, "a query of {} bytes", q.len());
    let id = u16::from_be_bytes([q[0], q[1]]);
    let flags = u16::from_be_bytes([q[2], q[3]]);
    invariant!(flags == 0x0100, "a query's flags {:#06x}", flags);
    invariant!(q[4..12] == [0, 1, 0, 0, 0, 0, 0, 0], "a query's counts {:02x?}", &q[4..12]);
    let mut at = 12;
    let mut name = Vec::new();
    loop {
        invariant!(at < q.len(), "a query's name runs off its end");
        let len = q[at] as usize;
        at += 1;
        if len == 0 {
            break;
        }
        invariant!(len <= 63 && at + len <= q.len(), "a query's label of {} bytes", len);
        if !name.is_empty() {
            name.push(b'.');
        }
        name.extend_from_slice(&q[at..at + len]);
        at += len;
    }
    invariant!(q.len() == at + 4 && q[at..at + 4] == [0, 1, 0, 1], "a query ends {:02x?}, not type A class IN",
               &q[at..]);
    (id, name)
}

fn put_name(m: &mut Vec<u8>, name: &[u8]) {
    for label in name.split(|&b| b == b'.') {
        m.push(label.len() as u8);
        m.extend_from_slice(label);
    }
    m.push(0);
}

fn header(id: u16, flags: u16, qd: u16, an: u16) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&flags.to_be_bytes());
    m.extend_from_slice(&qd.to_be_bytes());
    m.extend_from_slice(&an.to_be_bytes());
    m.extend_from_slice(&[0, 0, 0, 0]);
    m
}

/// The messages the server sends for one query: from where, and what.
fn answer(id: u16, name: &[u8], reply: &Reply) -> Vec<(u32, u16, Vec<u8>)> {
    match reply {
        Reply::Nothing => Vec::new(),
        Reply::Garbage(g) => {
            let mut g = g.clone();
            if g.len() >= 2 {
                g[0] = (id >> 8) as u8 ^ 0x55;
            }
            vec![(DNS_IP, 53, g)]
        }
        Reply::Broken(b) => {
            let mut m = header(id, 0x8180, 1, 3);
            put_name(&mut m, name);
            m.extend_from_slice(&[0, 1, 0, 1]);
            m.extend_from_slice(b);
            vec![(DNS_IP, 53, m)]
        }
        Reply::Error(rcode) => {
            let mut m = header(id, 0x8180 | rcode, 1, 0);
            put_name(&mut m, name);
            m.extend_from_slice(&[0, 1, 0, 1]);
            vec![(DNS_IP, 53, m)]
        }
        Reply::Answer { cnames, addrs, ttl, compress, lead } => {
            let mut out = Vec::new();
            let real = well_made(id, name, *cnames, addrs, *ttl, *compress);
            for f in lead {
                match f {
                    Forged::WrongId => out.push((DNS_IP, 53, well_made(id ^ 0x8001, name, 0, &[0x0101_0101], 999, false))),
                    Forged::WrongAddress => out.push((GW_IP, 53, well_made(id, name, 0, &[0x0202_0202], 999, false))),
                    Forged::WrongPort => out.push((DNS_IP, 5353, well_made(id, name, 0, &[0x0303_0303], 999, false))),
                    Forged::NotAResponse => {
                        let mut q = well_made(id, name, 0, &[0x0404_0404], 999, false);
                        q[2] &= 0x7F;
                        out.push((DNS_IP, 53, q));
                    }
                }
            }
            out.push((DNS_IP, 53, real));
            out
        }
    }
}

/// A well-made answer: the question, then `cnames` CNAME records leading
/// to the addresses -- each name spelt out, or pointing back into the
/// message, as a server compresses them.
fn well_made(id: u16, name: &[u8], cnames: usize, addrs: &[u32], ttl: u32, compress: bool) -> Vec<u8> {
    let mut m = header(id, 0x8180, 1, (cnames + addrs.len()) as u16);
    let qname_at = m.len();
    put_name(&mut m, name);
    m.extend_from_slice(&[0, 1, 0, 1]);
    let mut owner_at = qname_at;
    let mut owner: Vec<u8> = name.to_vec();
    for c in 0..cnames {
        if compress {
            m.extend_from_slice(&[0xC0 | (owner_at >> 8) as u8, owner_at as u8]);
        } else {
            put_name(&mut m, &owner);
        }
        m.extend_from_slice(&[0, 5, 0, 1]);
        m.extend_from_slice(&ttl.to_be_bytes());
        let target = format!("alias{}.fuzz.test", c).into_bytes();
        let rdlen_at = m.len();
        m.extend_from_slice(&[0, 0]);
        let target_at = m.len();
        put_name(&mut m, &target);
        let rdlen = (m.len() - target_at) as u16;
        m[rdlen_at..rdlen_at + 2].copy_from_slice(&rdlen.to_be_bytes());
        owner_at = target_at;
        owner = target;
    }
    for &a in addrs {
        if compress {
            m.extend_from_slice(&[0xC0 | (owner_at >> 8) as u8, owner_at as u8]);
        } else {
            put_name(&mut m, &owner);
        }
        m.extend_from_slice(&[0, 1, 0, 1]);
        m.extend_from_slice(&ttl.to_be_bytes());
        m.extend_from_slice(&[0, 4]);
        m.extend_from_slice(&a.to_be_bytes());
    }
    m
}

/// Records made to break a parser.
fn broken_records(r: &mut Input) -> Vec<u8> {
    let mut b = Vec::new();
    for _ in 0..1 + r.below(4) {
        match r.u8() % 7 {
            /* A pointer anywhere, itself included; a label run past the end. */
            0 => b.extend_from_slice(&[0xC0, r.u8()]),
            1 => b.extend_from_slice(&[63, b'a', b'b']),
            /* A label length with its top bits 01 or 10: no label at all. */
            2 => b.extend_from_slice(&[r.pick(&[0x40u8, 0x80, 0xBF]), 1, 2, 3]),
            /* A record whose length says more than there is. */
            3 => b.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0xFF, 0xFF, 1, 2, 3, 4]),
            /* An A record of the wrong length. */
            4 => b.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, r.pick(&[0u8, 3, 5, 16]), 9, 9, 9]),
            /* A record cut in its fixed part. */
            5 => b.extend_from_slice(&[0xC0, 12, 0, 1, 0]),
            _ => b.extend_from_slice(&noise(r.u32(), r.below(40) as usize)),
        }
    }
    b
}

fn reply(r: &mut Input) -> Reply {
    match r.u8() % 16 {
        0 => Reply::Error(r.pick(&[1u16, 2, 3, 5, 15])),
        1 => Reply::Nothing,
        2 => Reply::Garbage(noise(r.u32(), r.below(600) as usize)),
        3 | 4 => Reply::Broken(broken_records(r)),
        _ => {
            let n = r.pick(&[1usize, 1, 1, 2, 5]);
            let addrs = (0..n).map(|_| 0x5DB8_0000 | u32::from(r.u16())).collect();
            let ttl = match r.u8() % 6 {
                0 => 0,
                1 => r.pick(&[1u32, 59, 60, 61, 600, 3599, 3600, 86_399, 86_400, 86_401, 0x7FFF_FFFF, 0xFFFF_FFFF]),
                _ => r.below(120) as u32,
            };
            let lead = (0..r.below(3)).map(|_| r.pick(&[Forged::WrongId, Forged::WrongAddress, Forged::WrongPort,
                                                         Forged::NotAResponse])).collect();
            Reply::Answer { cnames: if r.u8() < 64 { r.below(4) as usize } else { 0 }, addrs, ttl,
                            compress: r.bool(), lead }
        }
    }
}

/// A name to ask for: one the server knows, one of every length a name
/// and a label can be and past it, one with an empty label.
fn name(r: &mut Input) -> Vec<u8> {
    match r.u8() % 12 {
        0 => format!("{}.test", "l".repeat(r.pick(&[62usize, 63, 64]))).into_bytes(),
        1 => {
            let mut n = Vec::new();
            let target = r.pick(&[252usize, 253, 254, 300]);
            while n.len() < target {
                if !n.is_empty() {
                    n.push(b'.');
                }
                n.extend_from_slice(b"abcdefghi");
            }
            n.truncate(target);
            n
        }
        2 => r.pick(&[b"a..b" as &[u8], b".", b"", b"trailing.", b".leading"]).to_vec(),
        _ => format!("host{}.fuzz.test", r.below(4)).into_bytes(),
    }
}

/// Whether the resolver may ask for `n` at all.
fn askable(n: &[u8]) -> bool {
    !n.is_empty() && n.len() <= 253 && n.split(|&b| b == b'.').all(|l| !l.is_empty() && l.len() <= 63)
}

pub fn dns(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(ETH0_IP);
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);
    let mut lan = Lan::new();
    lan.add(DNS_IP, DNS_MAC);
    let link = if r.u8() < 224 { Link::perfect() } else { Link::from_input(r) };
    let lossless = link.lossless();
    let replies: Vec<Reply> = (0..32).map(|_| reply(r)).collect();
    let delay = if r.u8() < 200 { r.below(50) * MS } else { r.below(5000) * MS };
    let mut w = World::new(link, Wire { lan, server: Server { replies, queries: Vec::new(), delay } });

    let resolver: &'static net::dns::Dns = Box::leak(Box::new(net::dns::Dns::new().expect("a resolver")));
    invariant!(resolver.start(nic, DNS_IP), "the resolver would not start");
    invariant!(resolver.server() == DNS_IP, "the resolver says its server is {}", netwire::Ipv4(resolver.server()));

    /* What the resolver was told, and when it forgets it: no sooner than
     * the first time, no later than the second -- the answer came some
     * time between the query going out and the resolve returning. */
    let mut cached: Vec<(Vec<u8>, u32, u64, u64)> = Vec::new();

    while let Some(op) = r.op(8) {
        match op {
            0..=4 => {
                let n = name(r);
                let timeout = r.pick(&[3000u64, 3000, 100, 10]);
                let asked_before = w.peers.server.queries.len();
                let now = sched::now();
                let entry = cached.iter().find(|c| c.0 == n).cloned();
                let hit = entry.as_ref().filter(|c| now < c.2).map(|c| c.1);
                let maybe = entry.as_ref().is_some_and(|c| now < c.3);
                let q = n.clone();
                let app = App::spawn("resolve", 1, move || resolver.resolve(&q, timeout));
                /* The query waits for ARP first -- three tries a second
                 * apart, on a link that loses the answer. */
                let done = w.wait_app(&app, (timeout + 5000) * MS);
                invariant!(done, "a resolve of {} ms never returned", timeout);
                let got = app.take().expect("done");
                let asked = w.peers.server.queries.len() - asked_before;
                if !askable(&n) {
                    invariant!(got.is_none() && asked == 0, "the name {:?} asked for, and answered {:?}",
                               String::from_utf8_lossy(&n), got);
                    continue;
                }
                if let Some(ip) = hit {
                    /* Answered from the cache: no query, the address it
                     * was told -- unless its time ran out while this one
                     * waited for the resolver's lock, which it does not. */
                    invariant!(asked == 0 && got == Some(ip), "a cached name answered {:?} with {} queries, the cache \
                               holding {}", got, asked, netwire::Ipv4(ip));
                    continue;
                }
                invariant!(asked <= 1, "one resolve asked {} queries", asked);
                if asked == 1 {
                    let k = w.peers.server.queries.len() - 1;
                    let (_, qname) = &w.peers.server.queries[k];
                    invariant!(*qname == n, "asked for {:?}, the query named {:?}", String::from_utf8_lossy(&n),
                               String::from_utf8_lossy(qname));
                    let reply = w.peers.server.replies.get(k).cloned().unwrap_or(Reply::Nothing);
                    judge(&reply, got, lossless, w.peers.server.delay, timeout);
                    cached.retain(|c| c.0 != n);
                    if let (Reply::Answer { addrs, ttl, .. }, Some(_)) = (&reply, got) {
                        if *ttl != 0 {
                            let keep = u64::from(*ttl).min(MAX_TTL) * SEC;
                            cached.push((n.clone(), addrs[0], now + w.peers.server.delay + keep,
                                         sched::now() + keep));
                        }
                    }
                } else {
                    /* No query: the cache answered, its entry not yet past
                     * its time -- or, on a link that loses frames, ARP never
                     * found the server, and nothing was sent at all. */
                    invariant!((maybe && got == entry.as_ref().map(|c| c.1)) || (got.is_none() && !lossless),
                               "an answer {:?} with no query asked", show(got));
                }
            }
            5 => {
                resolver.flush();
                cached.clear();
            }
            6 => {
                /* An answer nobody asked for, from the server: the pending
                 * id matches nothing now. */
                let m = well_made(r.u16(), b"host0.fuzz.test", 0, &[0x0909_0909], 600, false);
                let dg = frames::udp(DNS_IP, ETH0_IP, 53, CLIENT_PORT, &m, Sum::Right);
                w.net.inject(frames::ipv4(ETH0_MAC, DNS_MAC, &Ip::new(DNS_IP, ETH0_IP, IP_PROTO_UDP), &dg));
                w.pump();
            }
            _ => {
                /* Time, up to where a record's TTL runs out: a day at the
                 * most, which costs the fuzzer as much as a day of the
                 * machine's timers -- so seldom. */
                let step = match r.u8() % 16 {
                    0..=11 => r.pick(&[0u64, 1, 2, 30, 59, 60, 61]),
                    12..=14 => r.pick(&[599u64, 600, 601, 3599, 3600]),
                    _ if r.u8() < 64 => r.pick(&[86_399u64, 86_400, 86_401]),
                    _ => 1,
                };
                w.run_for(step * SEC);
            }
        }
    }
    w.run_for(10 * SEC);
    super::audit();
}

/// A resolve's answer, against the reply the server gave its query.
fn judge(reply: &Reply, got: Option<u32>, lossless: bool, delay: u64, timeout: u64) {
    /* On a good link an answer well inside the timeout is had. */
    let in_time = lossless && delay + 50 * MS < timeout * MS;
    match reply {
        Reply::Answer { addrs, .. } => {
            let first = addrs[0];
            if in_time {
                invariant!(got == Some(first), "a resolve answered {:?}, the server's first A record being {}",
                           show(got), netwire::Ipv4(first));
            } else {
                invariant!(got.is_none() || got == Some(first), "a resolve answered {:?}, the server saying {}",
                           show(got), netwire::Ipv4(first));
            }
        }
        /* What records made to break a parser come to is anybody's guess
         * -- a pointer into another record can make an A record of it --
         * as long as it is no panic. */
        Reply::Broken(_) | Reply::Garbage(_) => {}
        Reply::Error(_) | Reply::Nothing => {
            invariant!(got.is_none(), "an error or no answer resolved to {:?}", show(got));
        }
    }
}

/// An answer, for a report.
fn show(a: Option<u32>) -> String {
    a.map_or("nothing".to_string(), |ip| netwire::Ipv4(ip).to_string())
}
