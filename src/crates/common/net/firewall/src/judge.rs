//! An IPv4 packet to or from a VM, judged: by the flow it is part of, by
//! the flow an ICMP error is about, or -- the first packet of a flow -- by
//! the rules, and then remembered as a flow if they let it through.

use netwire::flow::{self, Lifetimes, SEEN_ANSWER};
use netwire::{tcp, IP_PROTO_ICMP, IP_PROTO_TCP, IP_PROTO_UDP};

use crate::flows::{Flows, Key};
use crate::packet::{Icmp, Quoted, V4};
use crate::rules::{Action, By, Dir, First, Rules};

const NS_PER_SEC: u64 = 1_000_000_000;

/// How long a flow outlives its last packet: NAT's times (`flow::NAT`), but
/// for a TCP connection answered and not closing, which lives five days, as
/// Linux's connection tracking keeps one. NAT has 4096 ports to give out and
/// holds a quiet connection two hours; a VM's table holds its own flows
/// alone, and a connection two VMs keep open, quiet, for a day would be cut
/// at the third hour.
pub const LIFETIMES: Lifetimes = Lifetimes {
    tcp_established_ns: 5 * 24 * 3600 * NS_PER_SEC,
    tcp_transitory_ns: flow::NAT.tcp_transitory_ns,
    tcp_reset_ns: flow::NAT.tcp_reset_ns,
    udp_ns: flow::NAT.udp_ns,
    other_ns: flow::NAT.other_ns,
};

/// What the switch knows of a packet that the frame does not say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Context {
    /// The VM's own address.
    pub vm: u32,
    /// Whether the other end is on the VM's network.
    pub same_net: bool,
    /// Whether it is for a group -- a broadcast, a multicast: judged by the
    /// rules each time, and no flow, a flow being between two ends.
    pub group: bool,
    /// Now, on the clock the flows' times are on.
    pub now: u64,
}

/// What let a packet through, or kept it out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// The flow it is part of.
    Flow,
    /// The flow an ICMP error is about.
    Related,
    /// A rule, by its place in the list.
    Rule(u8),
    /// The policy of its direction.
    Policy,
    /// Not the VM's: to it, for an address not its own; from it, from
    /// one not its own.
    Stray,
}

/// A packet judged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Verdict {
    pub pass: bool,
    pub why: Why,
    /// A live flow given up for this one's room: the table was full.
    pub evicted: bool,
}

impl Verdict {
    fn of(action: Action, by: By) -> Verdict {
        Verdict { pass: action == Action::Allow, why: why(by), evicted: false }
    }
}

fn why(by: By) -> Why {
    match by {
        By::Rule(i) => Why::Rule(i),
        By::Policy => Why::Policy,
    }
}

/// `p`, going `dir` from the VM's side, judged by its flows and its
/// firewall's rules -- and, the first packet of a flow the rules let through,
/// the flow made.
///
/// - A packet of a flow the table has goes by, the flow seen again.
/// - An ICMP error about a flow the table has goes by: path MTU discovery is
///   nothing without it.
/// - Anything else is judged by the rules -- the first one it matches, or
///   the policy of its direction -- and a flow made for it if they let it
///   through: but for a packet to a group, an echo reply, an ICMP message
///   that is neither an echo nor an error about a flow, which are judged
///   every time.
/// - A TCP segment of no flow that starts nothing -- of a connection whose
///   flow ran out while it was quiet, or that began before the table had it
///   -- is judged as its connection's first would be, and picked up where it
///   is, as Linux's loose tracking does: the rules let its port through.
pub fn judge(flows: &mut Flows, rules: &Rules, dir: Dir, p: &V4, c: &Context) -> Verdict {
    let (peer, own) = match dir {
        Dir::In => (p.src, p.dst),
        Dir::Out => (p.dst, p.src),
    };
    if own != c.vm && !(dir == Dir::In && c.group) {
        return Verdict { pass: false, why: Why::Stray, evicted: false };
    }
    let first = |port: Option<u16>| First { dir, proto: p.proto, port, peer, same_net: c.same_net };
    match p.icmp {
        Some(Icmp::Error(q)) => {
            if related(dir, &q, c.vm).is_some_and(|k| flows.find(&k, c.now).is_some()) {
                return Verdict { pass: true, why: Why::Related, evicted: false };
            }
            let j = rules.judge(&first(None));
            return Verdict::of(j.action, j.by);
        }
        Some(Icmp::Other) => {
            let j = rules.judge(&first(None));
            return Verdict::of(j.action, j.by);
        }
        _ => {}
    }
    let service = p.ports.map(|(_, dst)| dst);
    if c.group {
        let j = rules.judge(&first(service));
        return Verdict::of(j.action, j.by);
    }
    let key = key_of(dir, peer, p);
    if let Some(at) = flows.find(&key, c.now) {
        let answer = flows.initiated(at) != dir;
        /* An echo goes as requests from the end that began it and replies
         * from the other; a request the other way is no part of it. */
        let fits = !matches!(p.icmp, Some(Icmp::Echo { request, .. }) if request == answer);
        if fits {
            let was = flows.seen(at);
            let seen = if p.proto == IP_PROTO_TCP {
                flow::tcp(was, p.tcp_flags, answer)
            } else if answer {
                was | SEEN_ANSWER
            } else {
                was
            };
            flows.renew(at, seen, c.now.saturating_add(flow::lifetime(p.proto, seen, &LIFETIMES)));
            return Verdict { pass: true, why: Why::Flow, evicted: false };
        }
        let j = rules.judge(&first(None));
        return Verdict::of(j.action, j.by);
    }
    let j = rules.judge(&first(service));
    if j.action == Action::Deny || matches!(p.icmp, Some(Icmp::Echo { request: false, .. })) {
        return Verdict::of(j.action, j.by);
    }
    let picked_up = p.proto == IP_PROTO_TCP && p.tcp_flags & (tcp::SYN | tcp::ACK_FLAG) != tcp::SYN;
    let seen = if picked_up { flow::tcp(SEEN_ANSWER, p.tcp_flags, false) } else { 0 };
    let added = flows.add(key, dir, seen, c.now.saturating_add(flow::lifetime(p.proto, seen, &LIFETIMES)), c.now);
    Verdict { pass: true, why: why(j.by), evicted: added.evicted }
}

/// The flow `p`, going `dir` with `peer` at the other end, is part of.
fn key_of(dir: Dir, peer: u32, p: &V4) -> Key {
    let (local_port, peer_port) = match (p.icmp, p.ports) {
        (Some(Icmp::Echo { id, .. }), _) => (id, 0),
        (_, Some((src, dst))) => match dir {
            Dir::Out => (src, dst),
            Dir::In => (dst, src),
        },
        _ => (0, 0),
    };
    Key { proto: p.proto, local_port, peer, peer_port }
}

/// The flow an ICMP error going `dir` is about: one coming to the VM is
/// about a packet the VM sent, which it quotes from the VM's address; one the
/// VM sends, about a packet that came to it.
fn related(dir: Dir, q: &Quoted, vm: u32) -> Option<Key> {
    let (own, peer) = match dir {
        Dir::In => (q.src, q.dst),
        Dir::Out => (q.dst, q.src),
    };
    if own != vm {
        return None;
    }
    let (local_port, peer_port) = match (q.echo, q.ports) {
        (Some(id), _) => (id, 0),
        (None, Some((src, dst))) => match dir {
            Dir::In => (src, dst),
            Dir::Out => (dst, src),
        },
        (None, None) => return None,
    };
    let proto = if q.echo.is_some() { IP_PROTO_ICMP } else { q.proto };
    Some(Key { proto, local_port, peer, peer_port })
}

/// Whether the flow `key`, begun `initiated`, its other end on the VM's
/// network or not, is one `rules` let through: what a flow is judged again
/// by when its firewall changes.
pub fn still_allowed(rules: &Rules, key: &Key, initiated: Dir, same_net: bool) -> bool {
    let port = match key.proto {
        IP_PROTO_TCP | IP_PROTO_UDP => Some(match initiated {
            Dir::Out => key.peer_port,
            Dir::In => key.local_port,
        }),
        _ => None,
    };
    rules.judge(&First { dir: initiated, proto: key.proto, port, peer: key.peer, same_net }).action == Action::Allow
}
