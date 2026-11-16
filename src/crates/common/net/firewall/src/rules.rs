//! A firewall's rules: what one is, how a person writes it, and how a
//! flow's first packet is judged by them.

use core::fmt;

use netwire::{parse_ipv4, Ipv4, IP_PROTO_ICMP, IP_PROTO_TCP, IP_PROTO_UDP};

/// The most rules a firewall holds.
pub const RULES_MAX: usize = 64;
/// The most ports or ranges of them one rule names.
pub const PORTS_MAX: usize = 8;
/// The most networks one rule names.
pub const NETS_MAX: usize = 8;
/// The longest a firewall's name is: a VM's longest, so that a VM's own
/// firewall may have its name.
pub const NAME_MAX: usize = 32;

/// Whether `name` may be a firewall's: a letter or a digit, then letters,
/// digits, `.`, `_` and `-`, at most `NAME_MAX` in all -- as a network's
/// and a VM's are.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= NAME_MAX && b[0].is_ascii_alphanumeric()
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// What a rule, or a policy, does with a flow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Allow,
    Deny,
}

impl Action {
    pub fn parse(word: &str) -> Option<Action> {
        match word {
            "allow" => Some(Action::Allow),
            "deny" => Some(Action::Deny),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Deny => "deny",
        }
    }
}

/// Which way, from the VM: what comes to it, what it sends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    In,
    Out,
}

impl Dir {
    pub fn name(self) -> &'static str {
        match self {
            Dir::In => "in",
            Dir::Out => "out",
        }
    }

    /// The other way.
    pub fn back(self) -> Dir {
        match self {
            Dir::In => Dir::Out,
            Dir::Out => Dir::In,
        }
    }
}

/// The protocol a rule names; `Any` takes every one, those with no name
/// here among them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Proto {
    Any,
    Tcp,
    Udp,
    Icmp,
}

impl Proto {
    fn parse(word: &str) -> Option<Proto> {
        match word {
            "any" => Some(Proto::Any),
            "tcp" => Some(Proto::Tcp),
            "udp" => Some(Proto::Udp),
            "icmp" => Some(Proto::Icmp),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Proto::Any => "any",
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
            Proto::Icmp => "icmp",
        }
    }

    /// Whether it takes IP protocol `ip`.
    fn takes(self, ip: u8) -> bool {
        match self {
            Proto::Any => true,
            Proto::Tcp => ip == IP_PROTO_TCP,
            Proto::Udp => ip == IP_PROTO_UDP,
            Proto::Icmp => ip == IP_PROTO_ICMP,
        }
    }
}

/// The ports a rule names, each a range: none is every port.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ports {
    ranges: [(u16, u16); PORTS_MAX],
    len: u8,
}

impl Ports {
    pub const ANY: Ports = Ports { ranges: [(0, 0); PORTS_MAX], len: 0 };

    pub fn is_any(&self) -> bool {
        self.len == 0
    }

    pub fn ranges(&self) -> &[(u16, u16)] {
        &self.ranges[..usize::from(self.len)]
    }

    fn takes(&self, port: u16) -> bool {
        self.ranges().iter().any(|&(lo, hi)| lo <= port && port <= hi)
    }

    /// `22`, `80,443`, `8000-8100`, `22,8000-8100`: every port between 1
    /// and 65535, a range's low end no higher than its high.
    fn parse(word: &str) -> Result<Ports, &'static str> {
        let mut p = Ports::ANY;
        for item in word.split(',') {
            if usize::from(p.len) == PORTS_MAX {
                return Err("a rule names eight ports or ranges at most");
            }
            let (lo, hi) = match item.split_once('-') {
                Some((lo, hi)) => (port(lo)?, port(hi)?),
                None => {
                    let one = port(item)?;
                    (one, one)
                }
            };
            if lo > hi {
                return Err("a range of ports runs from its low end to its high");
            }
            p.ranges[usize::from(p.len)] = (lo, hi);
            p.len += 1;
        }
        Ok(p)
    }
}

/// A port as a person writes it: digits alone, 1 to 65535.
fn port(word: &str) -> Result<u16, &'static str> {
    const WRONG: &str = "a port is a number from 1 to 65535";
    if word.is_empty() || word.len() > 5 || !word.bytes().all(|b| b.is_ascii_digit()) {
        return Err(WRONG);
    }
    word.parse::<u16>().ok().filter(|&p| p != 0).ok_or(WRONG)
}

impl fmt::Display for Ports {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, &(lo, hi)) in self.ranges().iter().enumerate() {
            if i != 0 {
                f.write_str(",")?;
            }
            if lo == hi {
                write!(f, "{}", lo)?;
            } else {
                write!(f, "{}-{}", lo, hi)?;
            }
        }
        Ok(())
    }
}

/// Networks a rule names, each its address and its mask.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Nets {
    nets: [(u32, u32); NETS_MAX],
    len: u8,
}

impl Nets {
    pub fn list(&self) -> &[(u32, u32)] {
        &self.nets[..usize::from(self.len)]
    }

    fn takes(&self, addr: u32) -> bool {
        self.list().iter().any(|&(net, mask)| addr & mask == net)
    }

    /// `a.b.c.d` -- that address alone -- or `a.b.c.d/n`, its bits past the
    /// mask none: written with them, a network is a guess at which of two
    /// things was meant.
    fn parse(word: &str) -> Result<Nets, &'static str> {
        let mut n = Nets { nets: [(0, 0); NETS_MAX], len: 0 };
        for item in word.split(',') {
            if usize::from(n.len) == NETS_MAX {
                return Err("a rule names eight networks at most");
            }
            let (addr, bits) = match item.split_once('/') {
                Some((a, b)) => {
                    let bits = (!b.is_empty() && b.len() <= 2 && b.bytes().all(|c| c.is_ascii_digit()))
                        .then(|| b.parse::<u32>().ok()).flatten().filter(|&b| b <= 32)
                        .ok_or("a network's mask is /0 to /32")?;
                    (a, bits)
                }
                None => (item, 32),
            };
            let addr = parse_ipv4(addr.as_bytes()).ok_or("a network is an address, a.b.c.d, and a mask, /n")?;
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            if addr & !mask != 0 {
                return Err("a network has no bits set past its mask: a.b.c.0/24, or a.b.c.d alone");
            }
            n.nets[usize::from(n.len)] = (addr, mask);
            n.len += 1;
        }
        Ok(n)
    }
}

impl fmt::Display for Nets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, &(net, mask)) in self.list().iter().enumerate() {
            if i != 0 {
                f.write_str(",")?;
            }
            if mask == u32::MAX {
                write!(f, "{}", Ipv4(net))?;
            } else {
                write!(f, "{}/{}", Ipv4(net), mask.count_ones())?;
            }
        }
        Ok(())
    }
}

/// The other end a rule names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Peer {
    Any,
    /// The VM's own network: the other ports of its network.
    Net,
    Nets(Nets),
}

/// One rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rule {
    pub action: Action,
    pub dir: Dir,
    pub proto: Proto,
    pub ports: Ports,
    pub peer: Peer,
}

/// What a rule judges of a flow: which way its first packet goes, its
/// protocol, the port it is for -- its destination's, which is the
/// service's either way; none for a protocol with no ports -- the address of
/// the other end, and whether that end is on the VM's own network.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct First {
    pub dir: Dir,
    pub proto: u8,
    pub port: Option<u16>,
    pub peer: u32,
    pub same_net: bool,
}

impl Rule {
    /// A rule no firewall has: what the array is filled with past the end.
    const NONE: Rule = Rule { action: Action::Deny, dir: Dir::In, proto: Proto::Any, ports: Ports::ANY, peer: Peer::Any };

    /// A rule as a person writes it:
    /// `allow|deny in|out [tcp|udp|icmp|any] [ports] [from|to any|net|<networks>]`,
    /// `from` with `in` and `to` with `out`, ports only with `tcp` or `udp`.
    pub fn parse(text: &str) -> Result<Rule, &'static str> {
        let mut words = text.split_whitespace().peekable();
        let action = words.next().and_then(Action::parse).ok_or("a rule begins with allow or deny")?;
        let dir = match words.next() {
            Some("in") => Dir::In,
            Some("out") => Dir::Out,
            _ => return Err("a rule says in or out after allow or deny"),
        };
        let mut rule = Rule { action, dir, proto: Proto::Any, ports: Ports::ANY, peer: Peer::Any };
        if let Some(proto) = words.peek().and_then(|w| Proto::parse(w)) {
            rule.proto = proto;
            words.next();
        }
        if let Some(w) = words.peek().copied().filter(|w| w.starts_with(|c: char| c.is_ascii_digit())) {
            if !matches!(rule.proto, Proto::Tcp | Proto::Udp) {
                return Err("ports go with tcp or udp, named before them");
            }
            rule.ports = Ports::parse(w)?;
            words.next();
        }
        if let Some(w) = words.next() {
            match (w, dir) {
                ("from", Dir::In) | ("to", Dir::Out) => {}
                ("from", Dir::Out) => return Err("a rule out names the other end with to, not from"),
                ("to", Dir::In) => return Err("a rule in names the other end with from, not to"),
                _ => return Err("past the protocol and the ports a rule has only from or to"),
            }
            rule.peer = match words.next() {
                Some("any") => Peer::Any,
                Some("net") => Peer::Net,
                Some(nets) => Peer::Nets(Nets::parse(nets)?),
                None => return Err("from or to names any, net, or networks"),
            };
        }
        if words.next().is_some() {
            return Err("a rule has nothing past the other end it names");
        }
        Ok(rule)
    }

    /// Whether the flow whose first packet is `f` is this rule's.
    pub fn matches(&self, f: &First) -> bool {
        self.dir == f.dir && self.proto.takes(f.proto)
            && (self.ports.is_any() || f.port.is_some_and(|p| self.ports.takes(p)))
            && match &self.peer {
                Peer::Any => true,
                Peer::Net => f.same_net,
                Peer::Nets(nets) => nets.takes(f.peer),
            }
    }

    /// Whether it is a rule an IPv6 packet is judged by: one that names no
    /// protocol, no port and no address, those being IPv4's here.
    fn takes_ipv6(&self) -> bool {
        self.proto == Proto::Any && self.ports.is_any() && !matches!(self.peer, Peer::Nets(_))
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.action.name(), self.dir.name())?;
        if self.proto != Proto::Any {
            write!(f, " {}", self.proto.name())?;
        }
        if !self.ports.is_any() {
            write!(f, " {}", self.ports)?;
        }
        let word = if self.dir == Dir::In { "from" } else { "to" };
        match &self.peer {
            Peer::Any => Ok(()),
            Peer::Net => write!(f, " {} net", word),
            Peer::Nets(nets) => write!(f, " {} {}", word, nets),
        }
    }
}

/// What decided a flow: a rule -- its place in the list -- or the policy of
/// its direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum By {
    Rule(u8),
    Policy,
}

/// A verdict, and what gave it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Judged {
    pub action: Action,
    pub by: By,
}

/// A firewall's policies and rules.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rules {
    pub inbound: Action,
    pub outbound: Action,
    list: [Rule; RULES_MAX],
    len: u8,
}

const _: () = assert!(RULES_MAX <= u8::MAX as usize);

impl Rules {
    /// `default`'s: what comes in denied, what goes out allowed, no rule.
    pub const DEFAULT: Rules =
        Rules { inbound: Action::Deny, outbound: Action::Allow, list: [Rule::NONE; RULES_MAX], len: 0 };

    pub fn rules(&self) -> &[Rule] {
        &self.list[..usize::from(self.len)]
    }

    /// `self` made what `other` is, copied in place, field by field: a
    /// `Rules` is seven kilobytes, and an assignment of a clone may make it
    /// on the stack first -- of a deep caller's, under a spin lock.
    pub fn copy_from(&mut self, other: &Rules) {
        self.inbound = other.inbound;
        self.outbound = other.outbound;
        self.list = other.list;
        self.len = other.len;
    }

    /// The policy of `dir`.
    pub fn policy(&self, dir: Dir) -> Action {
        match dir {
            Dir::In => self.inbound,
            Dir::Out => self.outbound,
        }
    }

    pub fn set_policy(&mut self, dir: Dir, action: Action) {
        match dir {
            Dir::In => self.inbound = action,
            Dir::Out => self.outbound = action,
        }
    }

    /// `rule` in at place `at` -- the rules from there on one further down --
    /// or last: where it went, or why it did not.
    pub fn insert(&mut self, at: Option<usize>, rule: Rule) -> Result<usize, &'static str> {
        let len = usize::from(self.len);
        if len == RULES_MAX {
            return Err("a firewall has sixty-four rules at most");
        }
        let at = at.unwrap_or(len);
        if at > len {
            return Err("there is no such place among the rules");
        }
        self.list.copy_within(at..len, at + 1);
        self.list[at] = rule;
        self.len += 1;
        Ok(at)
    }

    /// The rule at `at` taken out, if there is one.
    pub fn remove(&mut self, at: usize) -> Option<Rule> {
        let len = usize::from(self.len);
        if at >= len {
            return None;
        }
        let rule = self.list[at];
        self.list.copy_within(at + 1..len, at);
        self.list[len - 1] = Rule::NONE;
        self.len -= 1;
        Some(rule)
    }

    /// The rule at `from` moved to place `to`, the rest keeping their order:
    /// whether both places are there.
    pub fn reorder(&mut self, from: usize, to: usize) -> bool {
        let len = usize::from(self.len);
        if from >= len || to >= len {
            return false;
        }
        let rule = self.list[from];
        if from < to {
            self.list.copy_within(from + 1..=to, from);
        } else {
            self.list.copy_within(to..from, to + 1);
        }
        self.list[to] = rule;
        true
    }

    /// The flow whose first packet is `f`, judged: the first rule it matches,
    /// or, none matching, the policy of its direction.
    pub fn judge(&self, f: &First) -> Judged {
        match self.rules().iter().position(|r| r.matches(f)) {
            Some(i) => Judged { action: self.list[i].action, by: By::Rule(i as u8) },
            None => Judged { action: self.policy(f.dir), by: By::Policy },
        }
    }

    /// Whether a flow of `proto` to the VM's port `port` may be let in from
    /// some address off the VM's network: a rule letting it in from such an
    /// address says yes, unless a rule before it denies it from every
    /// address; none, the policy. What `hv forward` says a forward is for
    /// nothing by, its clients being off the VM's network.
    pub fn may_let_in(&self, proto: u8, port: u16) -> bool {
        for r in self.rules() {
            if r.dir != Dir::In || !r.proto.takes(proto) || !(r.ports.is_any() || r.ports.takes(port)) {
                continue;
            }
            match (r.action, &r.peer) {
                (Action::Allow, Peer::Net) => {}
                (Action::Allow, _) => return true,
                (Action::Deny, Peer::Any) => return false,
                (Action::Deny, _) => {}
            }
        }
        self.inbound == Action::Allow
    }

    /// An IPv6 packet going `dir`, its other end on the VM's network or not
    /// (`same_net`), judged by the rules that name no protocol, no port and
    /// no address -- the rest are IPv4's -- or the policy.
    pub fn judge6(&self, dir: Dir, same_net: bool) -> Judged {
        let applies = |r: &Rule| r.dir == dir && r.takes_ipv6() && (r.peer == Peer::Any || same_net);
        match self.rules().iter().position(applies) {
            Some(i) => Judged { action: self.list[i].action, by: By::Rule(i as u8) },
            None => Judged { action: self.policy(dir), by: By::Policy },
        }
    }
}
