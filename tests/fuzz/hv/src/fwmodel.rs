//! The `firewall` target: a VM's firewall -- its rules as a person writes
//! them, what of a frame it reads, the table of flows a port keeps, and each
//! packet judged by them -- held to a model of its own that shares nothing
//! with the `firewall` crate: the rules as plain values, matched the obvious
//! way; a frame read anew; the flows a plain list, looked up by a scan and
//! given up by when each was used last.
//!
//! Each input is a firewall -- its policies and up to a few dozen rules, made
//! as values and handed to the crate as the text a person would write, read
//! back the same -- and a VM's packets both ways through it: of TCP with its
//! flags, UDP, echoes and their replies, ICMP errors quoting what went
//! before, protocols with no ports, fragments and frames that do not hold
//! together, to and from groups and other addresses than the VM's; time
//! going by past each flow's lifetime; the rules changed under the flows
//! there are, which are judged again; and noise read as a rule. Held: every
//! verdict, what gave it and whether a flow was given up for its room; the
//! flows the table holds, each with the way it began and what it has seen,
//! after every packet; IPv6's verdict, and whether a forward may let anyone
//! in, by the rules alone.

use std::collections::BTreeSet;

use firewall::{self as fw, Action, Dir, Flows, Rule, Rules};
use netwire::{self as wire, ETH_HDR_LEN};

use crate::targets::Input;

/// The VM's address, its network's other guests', and others.
const VM: u32 = 0x0A00_6407;
const NET: [u32; 3] = [0x0A00_6402, 0x0A00_6403, 0x0A00_6409];
const PEERS: [u32; 8] = [0x0A00_6402, 0x0A00_6403, 0x0A00_6409, 0x5DB8_D822, 0x0101_0101, 0x0808_0808,
                         0xC633_6407, 0xC633_6408];
const GROUPS: [u32; 3] = [u32::MAX, 0x0A00_64FF, 0xE000_00FB];
const PORTS: [u16; 7] = [22, 53, 80, 443, 8080, 40000, 40001];
const IDS: [u16; 3] = [1, 7, 0x1234];
/// Networks a rule names: one address, a /24, a /16, everything.
const NETS: [(u32, u8); 6] = [(0x5DB8_D822, 32), (0x0101_0100, 24), (0x0A00_6400, 24), (0xC633_0000, 16),
                              (0, 0), (0x0808_0808, 32)];
const NS: u64 = 1_000_000_000;

/* ---- the rules, as values ---- */

#[derive(Clone, Debug, PartialEq, Eq)]
enum MPeer {
    Any,
    Net,
    Nets(Vec<(u32, u8)>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MRule {
    allow: bool,
    out: bool,
    /// 0 for any, else the IP protocol.
    proto: u8,
    ports: Vec<(u16, u16)>,
    peer: MPeer,
}

#[derive(Clone, Debug)]
struct MRules {
    in_allow: bool,
    out_allow: bool,
    list: Vec<MRule>,
}

fn mask(bits: u8) -> u32 {
    if bits == 0 { 0 } else { u32::MAX << (32 - u32::from(bits)) }
}

fn dotted(a: u32) -> String {
    format!("{}.{}.{}.{}", a >> 24, (a >> 16) & 0xFF, (a >> 8) & 0xFF, a & 0xFF)
}

impl MRule {
    /// As a person writes it, the way the crate writes one back.
    fn text(&self) -> String {
        let mut s = format!("{} {}", if self.allow { "allow" } else { "deny" }, if self.out { "out" } else { "in" });
        match self.proto {
            0 => {}
            6 => s.push_str(" tcp"),
            17 => s.push_str(" udp"),
            _ => s.push_str(" icmp"),
        }
        if !self.ports.is_empty() {
            let items: Vec<String> = self.ports.iter()
                .map(|&(lo, hi)| if lo == hi { format!("{}", lo) } else { format!("{}-{}", lo, hi) }).collect();
            s.push(' ');
            s.push_str(&items.join(","));
        }
        let word = if self.out { "to" } else { "from" };
        match &self.peer {
            MPeer::Any => {}
            MPeer::Net => s.push_str(&format!(" {} net", word)),
            MPeer::Nets(nets) => {
                let items: Vec<String> = nets.iter()
                    .map(|&(a, b)| if b == 32 { dotted(a) } else { format!("{}/{}", dotted(a), b) }).collect();
                s.push_str(&format!(" {} {}", word, items.join(",")));
            }
        }
        s
    }

    fn matches(&self, out: bool, proto: u8, port: Option<u16>, peer: u32, same_net: bool) -> bool {
        if self.out != out || (self.proto != 0 && self.proto != proto) {
            return false;
        }
        if !self.ports.is_empty() {
            match port {
                Some(p) if self.ports.iter().any(|&(lo, hi)| lo <= p && p <= hi) => {}
                _ => return false,
            }
        }
        match &self.peer {
            MPeer::Any => true,
            MPeer::Net => same_net,
            MPeer::Nets(nets) => nets.iter().any(|&(a, b)| peer & mask(b) == a),
        }
    }
}

/// What decided, as the model has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MWhy {
    Flow,
    Related,
    Rule(usize),
    Policy,
    Stray,
}

impl MRules {
    fn judge(&self, out: bool, proto: u8, port: Option<u16>, peer: u32, same_net: bool) -> (bool, MWhy) {
        for (i, r) in self.list.iter().enumerate() {
            if r.matches(out, proto, port, peer, same_net) {
                return (r.allow, MWhy::Rule(i));
            }
        }
        (if out { self.out_allow } else { self.in_allow }, MWhy::Policy)
    }

    fn judge6(&self, out: bool, same_net: bool) -> (bool, MWhy) {
        for (i, r) in self.list.iter().enumerate() {
            let plain = r.proto == 0 && r.ports.is_empty() && !matches!(r.peer, MPeer::Nets(_));
            if r.out == out && plain && (r.peer == MPeer::Any || same_net) {
                return (r.allow, MWhy::Rule(i));
            }
        }
        (if out { self.out_allow } else { self.in_allow }, MWhy::Policy)
    }

    fn may_let_in(&self, proto: u8, port: u16) -> bool {
        for r in &self.list {
            let takes = !r.out && (r.proto == 0 || r.proto == proto)
                && (r.ports.is_empty() || r.ports.iter().any(|&(lo, hi)| lo <= port && port <= hi));
            if !takes {
                continue;
            }
            match (&r.peer, r.allow) {
                (MPeer::Net, true) => {}
                (_, true) => return true,
                (MPeer::Any, false) => return false,
                (_, false) => {}
            }
        }
        self.in_allow
    }
}

fn rule(r: &mut Input) -> MRule {
    let proto = r.pick(&[0u8, 0, 6, 6, 17, 1]);
    let mut ports = Vec::new();
    if matches!(proto, 6 | 17) && r.u8() < 160 {
        for _ in 0..1 + r.below(3) {
            let lo = r.pick(&PORTS);
            let hi = if r.u8() < 48 { lo.saturating_add(r.pick(&[1u16, 100, 30000])) } else { lo };
            ports.push((lo, hi));
        }
    }
    let peer = match r.u8() % 4 {
        0 | 1 => MPeer::Any,
        2 => MPeer::Net,
        _ => MPeer::Nets((0..1 + r.below(3)).map(|_| {
            let (a, b) = r.pick(&NETS);
            (a & mask(b), b)
        }).collect()),
    };
    MRule { allow: r.bool(), out: r.u8() < 96, proto, ports, peer }
}

fn rules(r: &mut Input) -> MRules {
    let n = if r.u8() < 8 { 60 + r.below(8) as usize } else { r.below(10) as usize };
    MRules { in_allow: r.u8() < 64, out_allow: r.u8() < 192, list: (0..n).map(|_| rule(r)).collect() }
}

/// The model's rules made the crate's, as hv reads a firewall's file: each
/// rule from its text -- read back as the same text -- and in after the last;
/// past `RULES_MAX`, refused and left out of the model's as well.
fn engine(m: &mut MRules) -> Rules {
    let mut e = Rules::DEFAULT;
    e.inbound = if m.in_allow { Action::Allow } else { Action::Deny };
    e.outbound = if m.out_allow { Action::Allow } else { Action::Deny };
    let mut kept = Vec::new();
    for mr in &m.list {
        let text = mr.text();
        let parsed = Rule::parse(&text);
        let r = match parsed {
            Ok(r) => r,
            Err(why) => panic!("invariant: the rule \"{}\" was not read: {}", text, why),
        };
        invariant!(format!("{}", r) == text, "the rule \"{}\" written back as \"{}\"", text, r);
        let full = e.rules().len() == fw::RULES_MAX;
        match e.insert(None, r) {
            Ok(_) => {
                invariant!(!full, "a rule past {} taken", fw::RULES_MAX);
                kept.push(mr.clone());
            }
            Err(_) => invariant!(full, "a rule refused with {} in", e.rules().len()),
        }
    }
    m.list = kept;
    e
}

/* ---- a frame, read anew ---- */

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MIcmp {
    None,
    Echo(u16, bool),
    Error(MQuoted),
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MQuoted {
    src: u32,
    dst: u32,
    proto: u8,
    sport: u16,
    dport: u16,
    echo: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MRead {
    V4 { src: u32, dst: u32, proto: u8, ports: Option<(u16, u16)>, flags: u8, icmp: MIcmp },
    Fragment,
    Broken,
    NotIp,
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn m_read(f: &[u8]) -> MRead {
    if f.len() < ETH_HDR_LEN || be16(f, 12) != 0x0800 {
        return MRead::NotIp;
    }
    let p = &f[ETH_HDR_LEN..];
    if p.len() < 20 || p[0] >> 4 != 4 || p[0] & 0xF < 5 {
        return MRead::Broken;
    }
    let ihl = usize::from(p[0] & 0xF) * 4;
    let total = usize::from(be16(p, 2));
    if total < ihl || total > p.len() {
        return MRead::Broken;
    }
    if be16(p, 6) & 0x3FFF != 0 {
        return MRead::Fragment;
    }
    let l4 = &p[ihl..total];
    let proto = p[9];
    let (src, dst) = (be32(p, 12), be32(p, 16));
    let (mut ports, mut flags, mut icmp) = (None, 0, MIcmp::None);
    match proto {
        6 if l4.len() >= 20 => {
            ports = Some((be16(l4, 0), be16(l4, 2)));
            flags = l4[13];
        }
        17 if l4.len() >= 8 => ports = Some((be16(l4, 0), be16(l4, 2))),
        1 if l4.len() >= 8 => {
            icmp = match l4[0] {
                8 => MIcmp::Echo(be16(l4, 4), true),
                0 => MIcmp::Echo(be16(l4, 4), false),
                3 | 11 | 12 => {
                    let q = &l4[8..];
                    let ok = q.len() >= 20 && q[0] >> 4 == 4 && q[0] & 0xF >= 5;
                    let qihl = if ok { usize::from(q[0] & 0xF) * 4 } else { 0 };
                    if ok && q.len() >= qihl + 8 && be16(q, 6) & 0x1FFF == 0 {
                        let ql4 = &q[qihl..];
                        let (qs, qd, qp) = (be32(q, 12), be32(q, 16), q[9]);
                        match qp {
                            6 | 17 => MIcmp::Error(MQuoted { src: qs, dst: qd, proto: qp, sport: be16(ql4, 0),
                                                             dport: be16(ql4, 2), echo: None }),
                            1 if ql4[0] == 8 => MIcmp::Error(MQuoted { src: qs, dst: qd, proto: 1, sport: 0, dport: 0,
                                                                       echo: Some(be16(ql4, 4)) }),
                            _ => MIcmp::Other,
                        }
                    } else {
                        MIcmp::Other
                    }
                }
                _ => MIcmp::Other,
            };
        }
        6 | 17 | 1 => return MRead::Broken,
        _ => {}
    }
    MRead::V4 { src, dst, proto, ports, flags, icmp }
}

/* ---- the flows, as a list ---- */

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct MKey {
    proto: u8,
    local: u16,
    peer: u32,
    peer_port: u16,
}

#[derive(Clone, Copy, Debug)]
struct MFlow {
    key: MKey,
    out: bool,
    seen: u8,
    expires: u64,
    used: u64,
}

const ANSWER: u8 = 1;
const FIN: u8 = 2;
const RST: u8 = 4;

fn lifetime(proto: u8, seen: u8) -> u64 {
    match proto {
        6 if seen & RST != 0 => 10 * NS,
        6 if seen & FIN != 0 => 240 * NS,
        6 if seen & ANSWER != 0 => 5 * 24 * 3600 * NS,
        6 => 240 * NS,
        17 => 300 * NS,
        _ => 60 * NS,
    }
}

struct MFlows {
    cap: usize,
    list: Vec<MFlow>,
    clock: u64,
}

impl MFlows {
    /// The flow `k` is, live at `now`: one run out let go.
    fn find(&mut self, k: &MKey, now: u64) -> Option<usize> {
        let i = self.list.iter().position(|f| f.key == *k)?;
        if self.list[i].expires <= now {
            self.list.remove(i);
            return None;
        }
        Some(i)
    }

    fn add(&mut self, f: MFlow, now: u64) -> bool {
        let mut evicted = false;
        if self.list.len() == self.cap {
            let oldest = (0..self.list.len()).min_by_key(|&i| self.list[i].used).expect("a full list has one");
            evicted = self.list[oldest].expires > now;
            self.list.remove(oldest);
        }
        self.clock += 1;
        self.list.push(MFlow { used: self.clock, ..f });
        evicted
    }

    fn renew(&mut self, i: usize, seen: u8, expires: u64) {
        self.clock += 1;
        let f = &mut self.list[i];
        f.seen = seen;
        f.expires = expires;
        f.used = self.clock;
    }
}

/// A TCP connection's state after a segment of `flags`, from the end that
/// answers or the one that began.
fn tcp_seen(seen: u8, flags: u8, answer: bool) -> u8 {
    let starts = flags & 0x12 == 0x02;
    let mut s = if starts && !answer { 0 } else { seen };
    if answer {
        s |= ANSWER;
    }
    if flags & 0x01 != 0 {
        s |= FIN;
    }
    if flags & 0x04 != 0 {
        s |= RST;
    }
    s
}

fn is_group(a: u32) -> bool {
    GROUPS.contains(&a) || a >> 28 == 0xE
}

/// The model's verdict on a packet going `out` or in: whether it passes,
/// what decided, and whether a live flow was given up for its room.
#[allow(clippy::too_many_arguments)]
fn m_judge(flows: &mut MFlows, rules: &MRules, out: bool, src: u32, dst: u32, proto: u8, ports: Option<(u16, u16)>,
           flags: u8, icmp: MIcmp, now: u64) -> (bool, MWhy, bool) {
    let (peer, own) = if out { (dst, src) } else { (src, dst) };
    let group = is_group(dst);
    if own != VM && !(!out && group) {
        return (false, MWhy::Stray, false);
    }
    let same = NET.contains(&peer);
    match icmp {
        MIcmp::Error(q) => {
            let (qown, qpeer) = if out { (q.dst, q.src) } else { (q.src, q.dst) };
            if qown == VM {
                let k = match q.echo {
                    Some(id) => MKey { proto: 1, local: id, peer: qpeer, peer_port: 0 },
                    None if out => MKey { proto: q.proto, local: q.dport, peer: qpeer, peer_port: q.sport },
                    None => MKey { proto: q.proto, local: q.sport, peer: qpeer, peer_port: q.dport },
                };
                if flows.find(&k, now).is_some() {
                    return (true, MWhy::Related, false);
                }
            }
            let (pass, why) = rules.judge(out, proto, None, peer, same);
            return (pass, why, false);
        }
        MIcmp::Other => {
            let (pass, why) = rules.judge(out, proto, None, peer, same);
            return (pass, why, false);
        }
        _ => {}
    }
    let service = ports.map(|p| p.1);
    if group {
        let (pass, why) = rules.judge(out, proto, service, peer, same);
        return (pass, why, false);
    }
    let (local, peer_port) = match (icmp, ports) {
        (MIcmp::Echo(id, _), _) => (id, 0),
        (_, Some((s, d))) => if out { (s, d) } else { (d, s) },
        _ => (0, 0),
    };
    let k = MKey { proto, local, peer, peer_port };
    if let Some(i) = flows.find(&k, now) {
        let answer = flows.list[i].out != out;
        let fits = !matches!(icmp, MIcmp::Echo(_, request) if request == answer);
        if fits {
            let was = flows.list[i].seen;
            let seen = if proto == 6 { tcp_seen(was, flags, answer) } else if answer { was | ANSWER } else { was };
            flows.renew(i, seen, now + lifetime(proto, seen));
            return (true, MWhy::Flow, false);
        }
        let (pass, why) = rules.judge(out, proto, None, peer, same);
        return (pass, why, false);
    }
    let (pass, why) = rules.judge(out, proto, service, peer, same);
    if !pass || matches!(icmp, MIcmp::Echo(_, false)) {
        return (pass, why, false);
    }
    let seen = if proto == 6 && flags & 0x12 != 0x02 { tcp_seen(ANSWER, flags, false) } else { 0 };
    let evicted = flows.add(MFlow { key: k, out, seen, expires: now + lifetime(proto, seen), used: 0 }, now);
    (true, why, evicted)
}

/* ---- the frames a VM sends and is sent ---- */

fn frame(src: u32, dst: u32, proto: u8, l4: &[u8], frag: u16, ihl_words: u8, total_delta: i32) -> Vec<u8> {
    let ihl = usize::from(ihl_words) * 4;
    let mut f = vec![0u8; ETH_HDR_LEN + ihl.max(20) + l4.len()];
    f[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    let p = &mut f[ETH_HDR_LEN..];
    p[0] = 0x40 | ihl_words;
    let total = (ihl.max(20) + l4.len()) as i32 + total_delta;
    p[2..4].copy_from_slice(&(total.clamp(0, 0xFFFF) as u16).to_be_bytes());
    p[6..8].copy_from_slice(&frag.to_be_bytes());
    p[8] = 64;
    p[9] = proto;
    p[12..16].copy_from_slice(&src.to_be_bytes());
    p[16..20].copy_from_slice(&dst.to_be_bytes());
    let at = ihl.max(20);
    p[at..at + l4.len()].copy_from_slice(l4);
    f
}

fn transport(r: &mut Input, proto: u8, sport: u16, dport: u16, recent: &[Vec<u8>]) -> Vec<u8> {
    match proto {
        6 => {
            let mut t = vec![0u8; 20 + r.below(8) as usize];
            t[0..2].copy_from_slice(&sport.to_be_bytes());
            t[2..4].copy_from_slice(&dport.to_be_bytes());
            t[12] = 0x50;
            t[13] = r.pick(&[0x02u8, 0x12, 0x10, 0x11, 0x04, 0x18, 0x14, 0x02]);
            if r.u8() < 8 {
                t.truncate(r.below(20) as usize);
            }
            t
        }
        17 => {
            let mut u = vec![0u8; 8 + r.below(8) as usize];
            u[0..2].copy_from_slice(&sport.to_be_bytes());
            u[2..4].copy_from_slice(&dport.to_be_bytes());
            if r.u8() < 8 {
                u.truncate(r.below(8) as usize);
            }
            u
        }
        1 => {
            let kind = r.pick(&[8u8, 8, 0, 0, 3, 3, 11, 13]);
            let mut m = vec![0u8; 8];
            m[0] = kind;
            m[4..6].copy_from_slice(&r.pick(&IDS).to_be_bytes());
            if matches!(kind, 3 | 11) {
                /* The packet it is about: one that went before, mostly. */
                let q = if !recent.is_empty() && r.u8() < 224 {
                    let f = &recent[r.below(recent.len() as u64) as usize];
                    f[ETH_HDR_LEN..].to_vec()
                } else {
                    let l4 = transport_plain(r);
                    frame(r.pick(&PEERS), VM, r.pick(&[6u8, 17]), &l4, 0, 5, 0)[ETH_HDR_LEN..].to_vec()
                };
                let cut = if r.u8() < 16 { r.below(q.len() as u64 + 1) as usize } else { q.len().min(28 + r.below(40) as usize) };
                m.extend_from_slice(&q[..cut.min(q.len())]);
            }
            if r.u8() < 8 {
                m.truncate(r.below(8) as usize);
            }
            m
        }
        _ => vec![0u8; r.below(16) as usize],
    }
}

fn transport_plain(r: &mut Input) -> Vec<u8> {
    let mut t = vec![0u8; 20];
    t[0..2].copy_from_slice(&r.pick(&PORTS).to_be_bytes());
    t[2..4].copy_from_slice(&r.pick(&PORTS).to_be_bytes());
    t[12] = 0x50;
    t[13] = 0x02;
    t
}

/// A packet to or from the VM: the way it goes, and its frame.
fn packet(r: &mut Input, recent: &[Vec<u8>]) -> (bool, Vec<u8>) {
    let out = r.bool();
    let proto = r.pick(&[6u8, 6, 17, 17, 1, 1, 47]);
    let peer = r.pick(&PEERS);
    let (mut src, mut dst) = if out { (VM, peer) } else { (peer, VM) };
    match r.u8() % 16 {
        0 => dst = r.pick(&GROUPS),
        1 if !out => dst = r.pick(&PEERS),
        2 if out => src = r.pick(&PEERS),
        _ => {}
    }
    let (sport, dport) = (r.pick(&PORTS), r.pick(&PORTS));
    let l4 = transport(r, proto, sport, dport, recent);
    let frag = match r.u8() % 32 {
        0 => 0x2000,
        1 => r.u16() & 0x1FFF,
        2 => 0x4000,
        _ => 0,
    };
    let ihl = if r.u8() < 6 { r.pick(&[0u8, 4, 6, 15]) } else { 5 };
    let delta = if r.u8() < 6 { r.pick(&[-30i32, -1, 1, 300]) } else { 0 };
    let mut f = frame(src, dst, proto, &l4, frag, ihl, delta);
    if r.u8() < 4 {
        f[ETH_HDR_LEN] = (f[ETH_HDR_LEN] & 0x0F) | 0x60;
    }
    if r.u8() < 4 {
        let keep = r.below(f.len() as u64 + 1) as usize;
        f.truncate(keep);
    }
    if r.u8() < 4 && f.len() >= 14 {
        f[12..14].copy_from_slice(&r.pick(&[0x0806u16, 0x86DD, 0x1234]).to_be_bytes());
    }
    (out, f)
}

/* ---- the target ---- */

fn why(w: fw::Why) -> MWhy {
    match w {
        fw::Why::Flow => MWhy::Flow,
        fw::Why::Related => MWhy::Related,
        fw::Why::Rule(i) => MWhy::Rule(usize::from(i)),
        fw::Why::Policy => MWhy::Policy,
        fw::Why::Stray => MWhy::Stray,
    }
}

/// The flows the table holds, live or not yet let go, as the model keys
/// them: each with the way it began and what it has seen.
fn held(flows: &Flows) -> BTreeSet<(MKey, bool, u8)> {
    let mut s = BTreeSet::new();
    flows.each(0, &mut |k, initiated, seen, _| {
        s.insert((MKey { proto: k.proto, local: k.local_port, peer: k.peer, peer_port: k.peer_port },
                  initiated == Dir::Out, seen));
    });
    s
}

pub fn firewall_target(r: &mut Input) {
    let mut m = rules(r);
    let mut e = engine(&mut m);
    let cap = r.pick(&[4usize, 8, 16, 64]);
    let mut flows = Flows::new(cap, r.u64()).expect("a table of flows");
    let mut mf = MFlows { cap, list: Vec::new(), clock: 0 };
    let mut now: u64 = NS + u64::from(r.u16());
    let mut recent: Vec<Vec<u8>> = Vec::new();

    while let Some(op) = r.op(8) {
        match op {
            0..=3 => {
                let (out, f) = packet(r, &recent);
                let dir = if out { Dir::Out } else { Dir::In };
                let read = fw::read(&f);
                let want = m_read(&f);
                match (read, want) {
                    (fw::Read::NotIp, MRead::NotIp) | (fw::Read::Fragment, MRead::Fragment)
                    | (fw::Read::Broken, MRead::Broken) => {}
                    (fw::Read::V6 { .. }, MRead::NotIp) => {}
                    (fw::Read::V4(v4), MRead::V4 { src, dst, proto, ports, flags, icmp }) => {
                        let micmp = match v4.icmp {
                            None => MIcmp::None,
                            Some(fw::Icmp::Echo { id, request }) => MIcmp::Echo(id, request),
                            Some(fw::Icmp::Other) => MIcmp::Other,
                            Some(fw::Icmp::Error(q)) => MIcmp::Error(MQuoted {
                                src: q.src, dst: q.dst, proto: q.proto,
                                sport: q.ports.map_or(0, |p| p.0), dport: q.ports.map_or(0, |p| p.1), echo: q.echo,
                            }),
                        };
                        invariant!((v4.src, v4.dst, v4.proto, v4.ports, v4.tcp_flags, micmp)
                                   == (src, dst, proto, ports, if proto == 6 { flags } else { 0 }, icmp),
                                   "a packet read as {:?}, the model {:?}", v4, want);
                        let peer = if out { v4.dst } else { v4.src };
                        let ctx = fw::Context { vm: VM, same_net: NET.contains(&peer), group: is_group(v4.dst), now };
                        let v = fw::judge(&mut flows, &e, dir, &v4, &ctx);
                        let (pass, w, evicted) = m_judge(&mut mf, &m, out, src, dst, proto, ports, flags, icmp, now);
                        invariant!((v.pass, why(v.why), v.evicted) == (pass, w, evicted),
                                   "a packet {} {:?} judged {:?}, the model {:?}", if out { "out" } else { "in" },
                                   want, v, (pass, w, evicted));
                        if recent.len() == 8 {
                            recent.remove(0);
                        }
                        recent.push(f);
                    }
                    (read, want) => panic!("invariant: a frame read as {:?}, the model {:?}", read, want),
                }
                let want: BTreeSet<(MKey, bool, u8)> = mf.list.iter().map(|f| (f.key, f.out, f.seen)).collect();
                invariant!(held(&flows) == want, "the table holds {:?}, the model {:?}", held(&flows), want);
                invariant!(flows.len() == mf.list.len(), "the table counts {} flows, the model {}", flows.len(),
                           mf.list.len());
            }
            4 => {
                now += r.pick(&[NS / 10, NS, 11 * NS, 61 * NS, 241 * NS, 301 * NS, 7441 * NS, 5 * 24 * 3600 * NS + 1]);
            }
            5 => {
                /* The rules changed under the flows there are: each judged
                 * again by the new, as the switch does a part at a time; the
                 * run out let go with them. */
                m = rules(r);
                e = engine(&mut m);
                let part = r.pick(&[1usize, 3, 256]);
                let mut from = 0;
                while from < flows.capacity() {
                    let (next, _) = flows.sweep(from, part, now, &mut |k, initiated| {
                        fw::still_allowed(&e, k, initiated, NET.contains(&k.peer))
                    });
                    from = next;
                }
                mf.list.retain(|f| {
                    let port = if matches!(f.key.proto, 6 | 17) {
                        Some(if f.out { f.key.peer_port } else { f.key.local })
                    } else {
                        None
                    };
                    f.expires > now && m.judge(f.out, f.key.proto, port, f.key.peer, NET.contains(&f.key.peer)).0
                });
                let want: BTreeSet<(MKey, bool, u8)> = mf.list.iter().map(|f| (f.key, f.out, f.seen)).collect();
                invariant!(held(&flows) == want, "after the rules changed the table holds {:?}, the model {:?}",
                           held(&flows), want);
            }
            6 => {
                /* IPv6, and a forward, by the rules alone. */
                let out = r.bool();
                let same = r.bool();
                let j = e.judge6(if out { Dir::Out } else { Dir::In }, same);
                let (pass, w) = m.judge6(out, same);
                let mine = (j.action == Action::Allow, match j.by {
                    fw::By::Rule(i) => MWhy::Rule(usize::from(i)),
                    fw::By::Policy => MWhy::Policy,
                });
                invariant!(mine == (pass, w), "IPv6 {} judged {:?}, the model {:?}", if out { "out" } else { "in" },
                           mine, (pass, w));
                let (proto, port) = (r.pick(&[6u8, 17]), r.pick(&PORTS));
                invariant!(e.may_let_in(proto, port) == m.may_let_in(proto, port),
                           "may let in at {}/{} said {}, the model {}", proto, port, e.may_let_in(proto, port),
                           m.may_let_in(proto, port));
            }
            _ => {
                /* Noise read as a rule: refused, or read back as itself. */
                let n = r.below(48) as usize;
                let words = ["allow", "deny", "in", "out", "tcp", "udp", "icmp", "any", "from", "to", "net", "22",
                             "80,443", "1-65535", "0", "65536", "10.0.0.0/8", "1.2.3.4", "1.2.3.4/24", "/33",
                             "8000-80", ",", "-", "", "x"];
                let mut text = String::new();
                for _ in 0..n.min(8) {
                    text.push_str(r.pick(&words));
                    text.push(if r.u8() < 220 { ' ' } else { ',' });
                }
                if let Ok(rule) = Rule::parse(&text) {
                    let again = format!("{}", rule);
                    let back = Rule::parse(&again);
                    invariant!(back == Ok(rule), "\"{}\" read as \"{}\", which reads back as {:?}", text, again, back);
                }
                let _ = wire::checksum(text.as_bytes());
            }
        }
    }
}

/* ---- the firewall at the switch's ports ---- */

use std::sync::Arc;

use crate::nic::Backend as _;
use crate::switch::{self as sw, Switch, MAX_PORTS};
use crate::vms::Shared;

/// The places of the two firewalls the ports here are on: one open both
/// ways, and one that keeps every peer out but what answers a flow its VM
/// began.
const OPEN: usize = 0;
const CLOSED: usize = 1;
const HOST_IP: u32 = 0x0A00_6401;

/// The flows a port's VM began since it claimed the port, as what an
/// answer to each would be keyed by: its protocol, the VM's port -- an
/// echo's identifier -- the peer and the peer's port.
type Began = BTreeSet<(u8, u16, u32, u16)>;

/// A frame from port `p`'s guest to port `q`'s, as itself: TCP, UDP or an
/// echo, its ports `sport` and `dport` -- an echo's identifier `sport`.
fn guest_frame(p: usize, q: usize, proto: u8, sport: u16, dport: u16, flags: u8, request: bool) -> Vec<u8> {
    let l4 = match proto {
        6 => {
            let mut t = vec![0u8; 20];
            t[0..2].copy_from_slice(&sport.to_be_bytes());
            t[2..4].copy_from_slice(&dport.to_be_bytes());
            t[12] = 0x50;
            t[13] = flags;
            t
        }
        17 => {
            let mut u = vec![0u8; 12];
            u[0..2].copy_from_slice(&sport.to_be_bytes());
            u[2..4].copy_from_slice(&dport.to_be_bytes());
            u[4..6].copy_from_slice(&12u16.to_be_bytes());
            u
        }
        _ => {
            let mut m = vec![0u8; 16];
            m[0] = if request { 8 } else { 0 };
            m[4..6].copy_from_slice(&sport.to_be_bytes());
            m
        }
    };
    let mut f = frame(sw::port_ip(p), sw::port_ip(q), proto, &l4, 0, 5, 0);
    f[0..6].copy_from_slice(&sw::port_mac(q));
    f[6..12].copy_from_slice(&sw::port_mac(p));
    f
}

/// What a frame a port's NIC took is, as what keys a flow its VM began:
/// None for one from the host -- which no firewall judges -- or not IPv4.
fn answer_key(f: &[u8]) -> Option<(u8, u16, u32, u16)> {
    match m_read(f) {
        MRead::V4 { src, .. } if src == HOST_IP => None,
        MRead::V4 { src, proto, ports: Some((s, d)), .. } => Some((proto, d, src, s)),
        MRead::V4 { src, proto: 1, icmp: MIcmp::Echo(id, _), .. } => Some((1, id, src, 0)),
        MRead::V4 { src, proto, .. } => Some((proto, 0, src, 0)),
        _ => None,
    }
}

/// What port `q`'s NIC takes, all of it: on the closed firewall, nothing
/// from a peer but what answers a flow its VM began since it claimed the
/// port.
fn check(sw: &Switch, q: usize, closed: bool, began: &Began, backend: &mut sw::PortBackend) -> usize {
    let mut buf = vec![0u8; crate::offload::MAX_SEND];
    let mut took = 0;
    while let Some((n, _)) = backend.recv_with(&mut buf) {
        took += 1;
        if !closed {
            continue;
        }
        if let Some(key) = answer_key(&buf[..n]) {
            invariant!(began.contains(&key), "port {} on the closed firewall took {:?}, which answers no flow its VM \
                       began ({:?}): {:?}", q, key, began, sw.counts(q));
        }
    }
    took
}

/// Port `p`'s backend, its NIC taking sends whole and checksums partial.
fn taking(sw: &Switch, p: usize) -> sw::PortBackend {
    let mut b = sw.backend(p);
    b.takes(crate::offload::Takes { partial: true, whole: true });
    b
}

/// Ports on an open firewall and a closed one, their guests sending to each
/// other -- flows begun, and answers to them -- and a port changing hands, to
/// a VM on either firewall, at any of the locks a send takes: a VM on the
/// closed firewall takes nothing from a peer but what answers a flow it
/// began itself, and nothing a flow of the VM before it on the port let in.
pub fn fwswitch_target(r: &mut Input) {
    crate::network::reset();
    let sw = Arc::new(Switch::new().expect("a switch"));
    let mut open = Rules::DEFAULT;
    open.inbound = Action::Allow;
    sw.install(OPEN, &open).expect("a place for a firewall");
    sw.install(CLOSED, &Rules::DEFAULT).expect("a place for a firewall");
    let n = 2 + r.below(3) as usize;
    let mut closed = vec![false; MAX_PORTS];
    let mut began: Vec<Began> = vec![Began::new(); MAX_PORTS];
    let mut vms: Vec<Option<Arc<Shared>>> = vec![None; MAX_PORTS];
    let mut backends: Vec<Option<sw::PortBackend>> = (0..MAX_PORTS).map(|_| None).collect();
    for p in 0..n {
        let vm = Arc::new(Shared::default());
        closed[p] = r.u8() < 160;
        let place = if closed[p] { CLOSED } else { OPEN };
        sw.claim_at(p, &vm, Some("lab"), place).expect("a port");
        vms[p] = Some(vm);
        backends[p] = Some(taking(&sw, p));
    }
    while let Some(op) = r.op(7) {
        let p = r.below(n as u64) as usize;
        let q = r.below(n as u64) as usize;
        if p == q {
            continue;
        }
        /* An answer to a flow `q`'s VM began towards `p`, mostly -- or a
         * flow of its own. */
        let theirs: Vec<(u8, u16, u32, u16)> = began[q].iter().filter(|k| k.2 == sw::port_ip(p)).copied().collect();
        let (proto, sport, dport, request) = if !theirs.is_empty() && r.u8() < 192 {
            let (proto, local, _, peer_port) = theirs[r.below(theirs.len() as u64) as usize];
            if proto == 1 { (1, local, 0, false) } else { (proto, peer_port, local, false) }
        } else {
            (r.pick(&[6u8, 17, 1]), r.pick(&PORTS), r.pick(&PORTS), true)
        };
        let flags = if request { 0x02 } else { 0x12 };
        let f = guest_frame(p, q, proto, sport, dport, flags, request);
        match op {
            0..=2 => {
                if let Some(b) = backends[p].as_mut() {
                    b.send(&f);
                }
            }
            3 => {
                /* `q` changing hands just before one of the locks `p`'s send
                 * takes, to a VM on either firewall. */
                let place = if r.u8() < 192 { CLOSED } else { OPEN };
                let k = r.below(8) as u32;
                let changed: Arc<std::sync::Mutex<Option<Arc<Shared>>>> = Arc::default();
                let change: Box<dyn FnOnce()> = {
                    let (sw, changed) = (sw.clone(), changed.clone());
                    Box::new(move || {
                        sw.release(q);
                        let vm = Arc::new(Shared::default());
                        let got = sw.claim_at(q, &vm, Some("lab"), place);
                        invariant!(got == Ok(q), "port {} not taken again: {:?}", q, got);
                        /* Its NIC taking sends whole at once, so that one
                         * judged for the VM going may be put whole in the
                         * inbox of the VM coming. */
                        sw.backend(q).takes(crate::offload::Takes { partial: true, whole: true });
                        *changed.lock().expect("the change's slot") = Some(vm);
                    })
                };
                /* What waited for the VM going is its own. */
                if let Some(b) = backends[q].as_mut() {
                    check(&sw, q, closed[q], &began[q], b);
                }
                crate::sync::before_lock(k, change);
                let mut w = f.clone();
                w.extend_from_slice(&[0u8; 3000]);
                let total = (w.len() - ETH_HDR_LEN) as u16;
                w[ETH_HDR_LEN + 2..ETH_HDR_LEN + 4].copy_from_slice(&total.to_be_bytes());
                let whole = (proto == 6 && r.bool()).then(|| crate::offload::check_tcp4(&w, 1448).ok()).flatten();
                if let Some(b) = backends[p].as_mut() {
                    match whole {
                        Some(layout) => b.send_tcp4(&w, layout),
                        None => b.send(&f),
                    }
                }
                if let Some(change) = crate::sync::take_before_lock() {
                    change();
                }
                vms[q] = changed.lock().expect("the change's slot").take();
                closed[q] = place == CLOSED;
                began[q].clear();
                backends[q] = Some(taking(&sw, q));
            }
            4 => {
                /* A client of a forward to `q`'s VM (`hv forward`'s relay,
                 * which reaches the VM as the host): let in by the VM's
                 * firewall alone -- the open one lets anyone in, the closed
                 * one nobody, from its network or off it -- and a refusal
                 * counted against the port; and none let in at a port no VM
                 * has. */
                let peer = if r.bool() { sw::port_ip(p) } else { 0xCB00_7100 | u32::from(r.u8()) };
                let service = r.pick(&PORTS);
                let before = sw.counts(q).denied_in;
                let allowed = sw.lets_in(q, service, peer);
                invariant!(allowed == !closed[q], "port {} on the {} firewall {} a client from {:x} at {}", q,
                           if closed[q] { "closed" } else { "open" }, if allowed { "let in" } else { "kept out" }, peer,
                           service);
                let after = sw.counts(q).denied_in;
                invariant!(after == before + u64::from(!allowed), "port {}'s refusals went {} -> {} for one client", q,
                           before, after);
                invariant!(!sw.lets_in(n, service, peer), "a client let in at port {}, which no VM has", n);
                continue;
            }
            5 => {
                /* The host's own, from `hv0`: no firewall's to judge, so its
                 * VM takes it, open or closed. */
                let mut h = f.clone();
                h[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0x64, 0x01]);
                h[ETH_HDR_LEN + 12..ETH_HDR_LEN + 16].copy_from_slice(&HOST_IP.to_be_bytes());
                let sum = {
                    h[ETH_HDR_LEN + 10..ETH_HDR_LEN + 12].copy_from_slice(&[0, 0]);
                    wire::checksum(&h[ETH_HDR_LEN..ETH_HDR_LEN + 20])
                };
                h[ETH_HDR_LEN + 10..ETH_HDR_LEN + 12].copy_from_slice(&sum.to_be_bytes());
                if let Some(b) = backends[q].as_mut() {
                    check(&sw, q, closed[q], &began[q], b);
                }
                if let Some(sink) = crate::vnic::sink() {
                    sink.on_frame(&h);
                }
                let took = backends[q].as_mut().map_or(0, |b| check(&sw, q, closed[q], &began[q], b));
                invariant!(took == 1, "port {} took {} of the host's frames, sent one", q, took);
                continue;
            }
            6 if proto == 6 => {
                /* A send of TCP whole, taken whole by `q`'s NIC. */
                let mut w = f.clone();
                let payload = vec![0u8; 3000];
                w.extend_from_slice(&payload);
                let total = (w.len() - ETH_HDR_LEN) as u16;
                w[ETH_HDR_LEN + 2..ETH_HDR_LEN + 4].copy_from_slice(&total.to_be_bytes());
                if let Ok(layout) = crate::offload::check_tcp4(&w, 1448) {
                    if let Some(b) = backends[p].as_mut() {
                        b.send_tcp4(&w, layout);
                    }
                }
            }
            _ => {
                crate::time::advance(r.pick(&[NS / 100, NS, 30 * NS]));
                continue;
            }
        }
        /* What `p` sent began a flow of its VM's -- or kept one going --
         * its firewall letting anything out: what answers it may come in. */
        let key = match proto {
            1 => (1, sport, sw::port_ip(q), 0),
            _ => (proto, sport, sw::port_ip(q), dport),
        };
        began[p].insert(key);
        if let Some(b) = backends[q].as_mut() {
            check(&sw, q, closed[q], &began[q], b);
        }
    }
}
