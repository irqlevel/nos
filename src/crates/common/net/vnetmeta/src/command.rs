//! vnet's commands and their answers, as bytes: what its log's entries
//! are and what a member is answered. Every byte string is read whole,
//! every count and string bounded: anything else is no command.

use alloc::string::String;
use alloc::vec::Vec;

use codec::{Reader, Writer};
use firewall::{Action, Rule, RULES_MAX};

use crate::{Port, NETWORK_NAME_MAX};

/// A change of the state: the group's members (`members`' rules), a
/// network, a port, a public address or a firewall of the cluster's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// The group made, `host` its first member as incarnation `member`, and
    /// the `default` firewall with it.
    Init { group: [u8; 16], name: String, host: String, member: u64 },
    Add { host: String, member: u64 },
    Remove { host: String },
    /// The leader's digest of checkpoint `at`, which every member holds its
    /// own to.
    Digest { at: u64, digest: [u8; 32] },
    /// A network: its name, its subnet -- `net`/`prefix` -- its MTU, and
    /// whether its VMs go out to the world.
    NetworkCreate { name: String, net: u32, prefix: u8, mtu: u16, out: bool },
    NetworkDelete { name: String },
    /// A VM's NIC's port on a network, on the registry's node `node`: at
    /// `address`, or the next one free where it is 0; its firewall by name,
    /// `default` where none is named. Asked again for the same VM and NIC,
    /// the port there is, never changed.
    PortCreate { uuid: [u8; 16], nic: u8, network: String, node: u64, address: u32, firewall: String },
    /// A port moved to node `to`, where it is on `expect` still.
    PortMove { port: u64, expect: u64, to: u64 },
    PortDelete { port: u64 },
    PortFirewall { port: u64, firewall: String },
    /// A public address, routed by its provider to node `gateway`.
    PublicAdd { address: u32, gateway: u64 },
    PublicRemove { address: u32 },
    /// A public address's gateway and port -- 0 for none -- where they are
    /// `expect_gateway` and `expect_port` still.
    PublicSet { address: u32, gateway: u64, port: u64, expect_gateway: u64, expect_port: u64 },
    FirewallCreate { name: String },
    /// A firewall's policies and rules, whole, where its revision is
    /// `revision` still.
    FirewallSet { name: String, revision: u64, inbound: Action, outbound: Action, rules: Vec<Rule> },
    FirewallDelete { name: String },
}

/// What a command is answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Done: a member's incarnation, a network's number, a firewall's id or
    /// its revision, a port's generation, a public address -- as the command
    /// has it; 0 where it was gone already.
    Done(u64),
    /// A port: made now, or there already for that VM and NIC -- as it is.
    Port { port: Port, made: bool },
    /// A move refused: the port is on this node, not the one expected.
    Elsewhere(u64),
    /// A port deleted, and the public address let go of with it -- 0 for
    /// none.
    Deleted { public: u32 },
    Refused(Refusal),
}

/// Why a command was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Malformed,
    NotInitialized,
    Initialized,
    BadName,
    BadMember,
    HostIsMember,
    MemberInUse,
    MemberRemoved,
    Full,
    NoSuchHost,
    LastMember,
    /// A subnet that is not a private range's, not of /16 to /28, not on its
    /// own boundary, or hv's own 10.0.100.0/24; an MTU past 576 to 1440.
    BadNetwork,
    /// A network of that name there already, another.
    NetworkTaken,
    /// The subnet overlaps another network's, or holds a public address.
    Overlaps,
    NoSuchNetwork,
    /// Ports are on it.
    NetworkInUse,
    /// As many as the state keeps -- networks, ports, public addresses, a
    /// gateway's, firewalls -- or numbers used up.
    TooMany,
    NoSuchPort,
    /// Node 0, or a NIC past the most.
    BadPort,
    /// Not an address of the network a port may have: off its subnet, its
    /// own, its gateway's, its broadcast, or one ending in 0 or 255.
    BadAddress,
    AddressTaken,
    /// The network has no address free.
    NoAddress,
    NoSuchFirewall,
    /// Ports have it.
    FirewallInUse,
    /// `default` is never deleted.
    DefaultKept,
    /// What a compare and set expected is not what the state has: a
    /// firewall's revision, a public address's gateway and port.
    Stale,
    /// Address 0, gateway 0, or an address inside a network of the
    /// cluster's or hv's own.
    BadPublic,
    /// The address is there already, routed to another gateway.
    PublicTaken,
    NoSuchPublic,
    /// It is on a port.
    PublicInUse,
    /// The port has a public address already.
    PortHasPublic,
    /// More rules than a firewall holds.
    TooManyRules,
}

/// Every refusal, by its code: its place here. A new one goes last.
pub const REFUSALS: [Refusal; 32] = [
    Refusal::Malformed, Refusal::NotInitialized, Refusal::Initialized, Refusal::BadName, Refusal::BadMember,
    Refusal::HostIsMember, Refusal::MemberInUse, Refusal::MemberRemoved, Refusal::Full, Refusal::NoSuchHost,
    Refusal::LastMember, Refusal::BadNetwork, Refusal::NetworkTaken, Refusal::Overlaps, Refusal::NoSuchNetwork,
    Refusal::NetworkInUse, Refusal::TooMany, Refusal::NoSuchPort, Refusal::BadPort, Refusal::BadAddress,
    Refusal::AddressTaken, Refusal::NoAddress, Refusal::NoSuchFirewall, Refusal::FirewallInUse,
    Refusal::DefaultKept, Refusal::Stale, Refusal::BadPublic, Refusal::PublicTaken, Refusal::NoSuchPublic,
    Refusal::PublicInUse, Refusal::PortHasPublic, Refusal::TooManyRules,
];

impl Refusal {
    /// Its place in `REFUSALS`: every refusal is there once, which the
    /// fuzzer's answers' round trip holds.
    fn code(self) -> u8 {
        REFUSALS.iter().position(|r| *r == self).unwrap_or(0) as u8
    }

    fn from_code(code: u8) -> Option<Refusal> {
        REFUSALS.get(usize::from(code)).copied()
    }

    /// What a person is told.
    pub fn says(self) -> &'static str {
        match self {
            Refusal::Malformed => "bytes that are no command",
            Refusal::NotInitialized => "no vnet yet -- `vnet init` makes one",
            Refusal::Initialized => "the vnet is made already",
            Refusal::BadName => "not a name: lowercase letters, digits and -, a network's up to 26",
            Refusal::BadMember => "member 0 is none",
            Refusal::HostIsMember => "the host is a member already, as another incarnation -- if its state was lost, \
                                      `vnet host replace` takes it back",
            Refusal::MemberInUse => "another host's member is that incarnation",
            Refusal::MemberRemoved => "that incarnation was removed: never taken again",
            Refusal::Full => "as many members as a vnet takes, or as many removed as it keeps",
            Refusal::NoSuchHost => "no member of that host",
            Refusal::LastMember => "the vnet's last member",
            Refusal::BadNetwork => "not a network: a private range's subnet of /16 to /28 on its own boundary, not \
                                    10.0.100.0/24, with an MTU of 576 to 1440",
            Refusal::NetworkTaken => "a network of that name is there already, another",
            Refusal::Overlaps => "the subnet overlaps another network's, or holds a public address",
            Refusal::NoSuchNetwork => "no network of that name",
            Refusal::NetworkInUse => "ports are on the network -- their VMs deleted first, or their ports",
            Refusal::TooMany => "as many as the state keeps: networks, ports, public addresses -- a gateway's \
                                 too -- or firewalls",
            Refusal::NoSuchPort => "no such port",
            Refusal::BadPort => "a port of no host, or of a NIC past the most",
            Refusal::BadAddress => "not an address of the network's a port may have: off its subnet, its own, its \
                                    gateway's, its broadcast, or one ending in 0 or 255",
            Refusal::AddressTaken => "the address is another port's",
            Refusal::NoAddress => "the network has no address free",
            Refusal::NoSuchFirewall => "no firewall of that name",
            Refusal::FirewallInUse => "ports have the firewall -- each given another first",
            Refusal::DefaultKept => "`default` is never deleted",
            Refusal::Stale => "asked of what has changed since: read it again",
            Refusal::BadPublic => "not a public address: 0, of no gateway, or inside a network of the cluster's or \
                                   hv's own",
            Refusal::PublicTaken => "the address is there already, routed to another host",
            Refusal::NoSuchPublic => "no such public address",
            Refusal::PublicInUse => "the address is on a port -- `vnet ip set <address> none` first",
            Refusal::PortHasPublic => "the port has a public address already",
            Refusal::TooManyRules => "a firewall has sixty-four rules at most",
        }
    }
}

impl From<members::Refusal> for Refusal {
    fn from(r: members::Refusal) -> Refusal {
        match r {
            members::Refusal::Initialized => Refusal::Initialized,
            members::Refusal::NotInitialized => Refusal::NotInitialized,
            members::Refusal::BadName => Refusal::BadName,
            members::Refusal::BadMember => Refusal::BadMember,
            members::Refusal::HostIsMember => Refusal::HostIsMember,
            members::Refusal::MemberInUse => Refusal::MemberInUse,
            members::Refusal::MemberRemoved => Refusal::MemberRemoved,
            members::Refusal::Full => Refusal::Full,
            members::Refusal::NoSuchHost => Refusal::NoSuchHost,
            members::Refusal::LastMember => Refusal::LastMember,
        }
    }
}

const INIT: u8 = 1;
const ADD: u8 = 2;
const REMOVE: u8 = 3;
const DIGEST: u8 = 4;
const NETWORK_CREATE: u8 = 10;
const NETWORK_DELETE: u8 = 11;
const PORT_CREATE: u8 = 20;
const PORT_MOVE: u8 = 21;
const PORT_DELETE: u8 = 22;
const PORT_FIREWALL: u8 = 23;
const PUBLIC_ADD: u8 = 30;
const PUBLIC_REMOVE: u8 = 31;
const PUBLIC_SET: u8 = 32;
const FIREWALL_CREATE: u8 = 40;
const FIREWALL_SET: u8 = 41;
const FIREWALL_DELETE: u8 = 42;

const DONE: u8 = 1;
const PORT: u8 = 2;
const ELSEWHERE: u8 = 3;
const DELETED: u8 = 4;
const REFUSED: u8 = 5;

/// The longest a rule's text is: eight ports or ranges and eight networks.
pub const RULE_TEXT_MAX: usize = 256;
/// A host's name at most.
const HOST_MAX: usize = members::NAME_MAX;

pub(crate) fn action_code(a: Action) -> u8 {
    match a {
        Action::Allow => 1,
        Action::Deny => 2,
    }
}

pub(crate) fn action_of(code: u8) -> Option<Action> {
    match code {
        1 => Some(Action::Allow),
        2 => Some(Action::Deny),
        _ => None,
    }
}

/// A rule as it goes in a command and a snapshot: its text, as a person
/// writes it -- read back by the same parser as the shell's.
pub(crate) fn write_rule(w: &mut Writer, r: &Rule) {
    let mut text = String::new();
    let _ = core::fmt::write(&mut text, format_args!("{}", r));
    w.str(&text);
}

pub(crate) fn read_rule(text: &str) -> Option<Rule> {
    Rule::parse(text).ok()
}

pub(crate) fn write_port(w: &mut Writer, p: &Port) {
    w.u64(p.id);
    w.fixed(&p.uuid);
    w.u8(p.nic);
    w.u32(p.vni);
    w.u32(p.address);
    w.u64(p.node);
    w.u64(p.generation);
    w.u32(p.firewall);
}

pub(crate) fn read_port(r: &mut Reader) -> Option<Port> {
    Some(Port {
        id: r.u64()?,
        uuid: r.fixed()?,
        nic: r.u8()?,
        vni: r.u32()?,
        address: r.u32()?,
        node: r.u64()?,
        generation: r.u64()?,
        firewall: r.u32()?,
    })
}

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Command::Init { group, name, host, member } => {
                w.u8(INIT);
                w.fixed(group);
                w.str(name);
                w.str(host);
                w.u64(*member);
            }
            Command::Add { host, member } => {
                w.u8(ADD);
                w.str(host);
                w.u64(*member);
            }
            Command::Remove { host } => {
                w.u8(REMOVE);
                w.str(host);
            }
            Command::Digest { at, digest } => {
                w.u8(DIGEST);
                w.u64(*at);
                w.fixed(digest);
            }
            Command::NetworkCreate { name, net, prefix, mtu, out } => {
                w.u8(NETWORK_CREATE);
                w.str(name);
                w.u32(*net);
                w.u8(*prefix);
                w.u16(*mtu);
                w.u8(u8::from(*out));
            }
            Command::NetworkDelete { name } => {
                w.u8(NETWORK_DELETE);
                w.str(name);
            }
            Command::PortCreate { uuid, nic, network, node, address, firewall } => {
                w.u8(PORT_CREATE);
                w.fixed(uuid);
                w.u8(*nic);
                w.str(network);
                w.u64(*node);
                w.u32(*address);
                w.str(firewall);
            }
            Command::PortMove { port, expect, to } => {
                w.u8(PORT_MOVE);
                w.u64(*port);
                w.u64(*expect);
                w.u64(*to);
            }
            Command::PortDelete { port } => {
                w.u8(PORT_DELETE);
                w.u64(*port);
            }
            Command::PortFirewall { port, firewall } => {
                w.u8(PORT_FIREWALL);
                w.u64(*port);
                w.str(firewall);
            }
            Command::PublicAdd { address, gateway } => {
                w.u8(PUBLIC_ADD);
                w.u32(*address);
                w.u64(*gateway);
            }
            Command::PublicRemove { address } => {
                w.u8(PUBLIC_REMOVE);
                w.u32(*address);
            }
            Command::PublicSet { address, gateway, port, expect_gateway, expect_port } => {
                w.u8(PUBLIC_SET);
                w.u32(*address);
                w.u64(*gateway);
                w.u64(*port);
                w.u64(*expect_gateway);
                w.u64(*expect_port);
            }
            Command::FirewallCreate { name } => {
                w.u8(FIREWALL_CREATE);
                w.str(name);
            }
            Command::FirewallSet { name, revision, inbound, outbound, rules } => {
                w.u8(FIREWALL_SET);
                w.str(name);
                w.u64(*revision);
                w.u8(action_code(*inbound));
                w.u8(action_code(*outbound));
                w.u8(rules.len().min(usize::from(u8::MAX)) as u8);
                for r in rules.iter().take(usize::from(u8::MAX)) {
                    write_rule(&mut w, r);
                }
            }
            Command::FirewallDelete { name } => {
                w.u8(FIREWALL_DELETE);
                w.str(name);
            }
        }
        w.buf
    }

    /// The command `bytes` are, read whole: None for anything else.
    pub fn decode(bytes: &[u8]) -> Option<Command> {
        let mut r = Reader::new(bytes);
        let name = |r: &mut Reader, max: usize| r.str(max).map(String::from);
        let cmd = match r.u8()? {
            INIT => Command::Init { group: r.fixed()?, name: name(&mut r, members::NAME_MAX)?,
                                    host: name(&mut r, HOST_MAX)?, member: r.u64()? },
            ADD => Command::Add { host: name(&mut r, HOST_MAX)?, member: r.u64()? },
            REMOVE => Command::Remove { host: name(&mut r, HOST_MAX)? },
            DIGEST => Command::Digest { at: r.u64()?, digest: r.fixed()? },
            NETWORK_CREATE => {
                let name = name(&mut r, NETWORK_NAME_MAX)?;
                let (net, prefix, mtu) = (r.u32()?, r.u8()?, r.u16()?);
                let out = match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                Command::NetworkCreate { name, net, prefix, mtu, out }
            }
            NETWORK_DELETE => Command::NetworkDelete { name: name(&mut r, NETWORK_NAME_MAX)? },
            PORT_CREATE => Command::PortCreate { uuid: r.fixed()?, nic: r.u8()?, network: name(&mut r, NETWORK_NAME_MAX)?,
                                                 node: r.u64()?, address: r.u32()?,
                                                 firewall: name(&mut r, firewall::NAME_MAX)? },
            PORT_MOVE => Command::PortMove { port: r.u64()?, expect: r.u64()?, to: r.u64()? },
            PORT_DELETE => Command::PortDelete { port: r.u64()? },
            PORT_FIREWALL => Command::PortFirewall { port: r.u64()?, firewall: name(&mut r, firewall::NAME_MAX)? },
            PUBLIC_ADD => Command::PublicAdd { address: r.u32()?, gateway: r.u64()? },
            PUBLIC_REMOVE => Command::PublicRemove { address: r.u32()? },
            PUBLIC_SET => Command::PublicSet { address: r.u32()?, gateway: r.u64()?, port: r.u64()?,
                                               expect_gateway: r.u64()?, expect_port: r.u64()? },
            FIREWALL_CREATE => Command::FirewallCreate { name: name(&mut r, firewall::NAME_MAX)? },
            FIREWALL_SET => {
                let name = name(&mut r, firewall::NAME_MAX)?;
                let revision = r.u64()?;
                let inbound = action_of(r.u8()?)?;
                let outbound = action_of(r.u8()?)?;
                let n = usize::from(r.u8()?);
                if n > RULES_MAX {
                    return None;
                }
                let mut rules = Vec::with_capacity(n);
                for _ in 0..n {
                    rules.push(read_rule(r.str(RULE_TEXT_MAX)?)?);
                }
                Command::FirewallSet { name, revision, inbound, outbound, rules }
            }
            FIREWALL_DELETE => Command::FirewallDelete { name: name(&mut r, firewall::NAME_MAX)? },
            _ => return None,
        };
        r.done().then_some(cmd)
    }
}

impl Answer {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Answer::Done(v) => {
                w.u8(DONE);
                w.u64(*v);
            }
            Answer::Port { port, made } => {
                w.u8(PORT);
                write_port(&mut w, port);
                w.u8(u8::from(*made));
            }
            Answer::Elsewhere(node) => {
                w.u8(ELSEWHERE);
                w.u64(*node);
            }
            Answer::Deleted { public } => {
                w.u8(DELETED);
                w.u32(*public);
            }
            Answer::Refused(why) => {
                w.u8(REFUSED);
                w.u8(why.code());
            }
        }
        w.buf
    }

    pub fn decode(bytes: &[u8]) -> Option<Answer> {
        let mut r = Reader::new(bytes);
        let a = match r.u8()? {
            DONE => Answer::Done(r.u64()?),
            PORT => {
                let port = read_port(&mut r)?;
                let made = match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                Answer::Port { port, made }
            }
            ELSEWHERE => Answer::Elsewhere(r.u64()?),
            DELETED => Answer::Deleted { public: r.u32()? },
            REFUSED => Answer::Refused(Refusal::from_code(r.u8()?)?),
            _ => return None,
        };
        r.done().then_some(a)
    }
}
