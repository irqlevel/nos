//! vnet's state (src/crates/common/net/vnetmeta, plans/vnet.md) held to a
//! model of its own, as NBS's metadata and the registry are:
//!
//! - **Alone**: the state and the model take the same commands -- the group
//!   made, members added and removed, networks made and deleted, ports made,
//!   moved, deleted and given firewalls, public addresses added, set and
//!   removed, firewalls made, changed and deleted, bytes that are none --
//!   from pools small enough that they meet: a network made again as itself
//!   and as another, subnets that overlap and that are no network's, a port
//!   asked again for its VM and NIC, a move from a node it is not on, a /28
//!   filled, an address let go of, a public address on two ports, a
//!   firewall's revision gone by. After every command its answer and
//!   everything the state shows -- the group, members, networks, ports,
//!   public addresses, firewalls, the revision, and what an attach is
//!   answered -- must be the model's. Its snapshot must read back as itself
//!   and come out of a freeze in pieces as one; a snapshot changed must be
//!   refused or read back as a state that holds to every rule; and a replica
//!   started from a snapshot part way must end as the state does.
//!
//! The model shares nothing with what it judges: its subnets are ranges of
//! numbers, its addresses counted round from a cursor of its own.

use std::collections::BTreeMap;

use firewall::{Action, Rule};
use raft::StateMachine;
use vnetmeta::{attach, Answer, Attach, Command, Port, Refusal, State};

use crate::rng::{mix, Rng};

/* ---- the model ---- */

#[derive(Clone, Debug)]
struct MNet {
    name: String,
    vni: u32,
    net: u32,
    prefix: u8,
    mtu: u16,
    out: bool,
    next: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MPublic {
    address: u32,
    gateway: u64,
    port: u64,
}

#[derive(Clone, Debug)]
struct MFirewall {
    id: u32,
    name: String,
    revision: u64,
    inbound: Action,
    outbound: Action,
    rules: Vec<Rule>,
}

#[derive(Clone, Debug)]
struct Model {
    group: Option<([u8; 16], String)>,
    /// (host, incarnation, since), and (host, incarnation, at).
    members: Vec<(String, u64, u64)>,
    removed: Vec<(String, u64, u64)>,
    nets: Vec<MNet>,
    ports: Vec<Port>,
    public: Vec<MPublic>,
    fws: Vec<MFirewall>,
    next_vni: u32,
    next_port: u64,
    next_gen: u64,
    next_fw: u32,
    revision: u64,
}

impl Default for Model {
    fn default() -> Model {
        Model { group: None, members: Vec::new(), removed: Vec::new(), nets: Vec::new(), ports: Vec::new(),
                public: Vec::new(), fws: Vec::new(), next_vni: 1, next_port: 1, next_gen: 1, next_fw: 1, revision: 0 }
    }
}

/// A host's or a group's name: a DNS label, lowercase, up to 63.
fn m_host(s: &str) -> bool {
    (1..=63).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

/// A network's: a host's, up to 26.
fn m_net_name(s: &str) -> bool {
    s.len() <= 26 && m_host(s)
}

/// A firewall's: a letter or digit first, then letters, digits, . _ -, up to 32.
fn m_fw_name(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=32).contains(&b.len()) && b[0].is_ascii_alphanumeric()
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// A subnet's first and last address.
fn span(net: u32, prefix: u8) -> (u64, u64) {
    let size = 1u64 << (32 - u32::from(prefix));
    (u64::from(net), u64::from(net) + size - 1)
}

fn meet(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

const M_HOST_NET: (u64, u64) = (0x0A00_6400, 0x0A00_64FF);

fn m_subnet(net: u32, prefix: u8) -> bool {
    if !(16..=28).contains(&prefix) {
        return false;
    }
    let (first, last) = span(net, prefix);
    let size = last - first + 1;
    let inside = |lo: u64, hi: u64| first >= lo && last <= hi;
    first % size == 0
        && (inside(0x0A00_0000, 0x0AFF_FFFF) || inside(0xAC10_0000, 0xAC1F_FFFF) || inside(0xC0A8_0000, 0xC0A8_FFFF))
        && !meet((first, last), M_HOST_NET)
}

fn m_usable(n: &MNet, a: u32) -> bool {
    let (first, last) = span(n.net, n.prefix);
    let a64 = u64::from(a);
    a64 >= first + 2 && a64 < last && a % 256 != 0 && a % 256 != 255
}

fn m_after(n: &MNet, a: u32) -> u32 {
    let (first, last) = span(n.net, n.prefix);
    let size = last - first + 1;
    (first + (u64::from(a) - first + 1) % size) as u32
}

impl Model {
    fn net(&self, name: &str) -> Option<&MNet> {
        self.nets.iter().find(|n| n.name == name)
    }

    fn fw(&self, name: &str) -> Option<&MFirewall> {
        self.fws.iter().find(|f| f.name == name)
    }

    fn port(&self, id: u64) -> Option<&Port> {
        self.ports.iter().find(|p| p.id == id)
    }

    fn taken(&self, vni: u32, a: u32) -> bool {
        self.ports.iter().any(|p| p.vni == vni && p.address == a)
    }

    fn at_gateway(&self, g: u64) -> usize {
        self.public.iter().filter(|p| p.gateway == g).count()
    }

    fn may_be_public(&self, a: u32) -> bool {
        let x = u64::from(a);
        a != 0 && !(x >= M_HOST_NET.0 && x <= M_HOST_NET.1)
            && !self.nets.iter().any(|n| { let (f, l) = span(n.net, n.prefix); x >= f && x <= l })
    }

    fn fw_named(&self, name: &str) -> Result<u32, Refusal> {
        let name = if name.is_empty() { "default" } else { name };
        self.fw(name).map(|f| f.id).ok_or(Refusal::NoSuchFirewall)
    }

    /// What the state answers the command its bytes read as: none read is
    /// `Malformed`.
    fn apply(&mut self, index: u64, cmd: Option<&Command>) -> Answer {
        let Some(cmd) = cmd else { return Answer::Refused(Refusal::Malformed) };
        match self.step(index, cmd) {
            Ok(a) => a,
            Err(why) => Answer::Refused(why),
        }
    }

    fn step(&mut self, index: u64, cmd: &Command) -> Result<Answer, Refusal> {
        match cmd {
            Command::Init { group, name, host, member } => {
                if self.group.is_some() {
                    return Err(Refusal::Initialized);
                }
                if !m_host(name) || !m_host(host) {
                    return Err(Refusal::BadName);
                }
                if *member == 0 {
                    return Err(Refusal::BadMember);
                }
                self.group = Some((*group, name.clone()));
                self.members.push((host.clone(), *member, index));
                self.fws.push(MFirewall { id: self.next_fw, name: String::from("default"), revision: 1,
                                          inbound: Action::Deny, outbound: Action::Allow, rules: Vec::new() });
                self.next_fw += 1;
                return Ok(Answer::Done(*member));
            }
            Command::Add { host, member } => {
                if self.group.is_none() {
                    return Err(Refusal::NotInitialized);
                }
                if !m_host(host) {
                    return Err(Refusal::BadName);
                }
                if *member == 0 {
                    return Err(Refusal::BadMember);
                }
                if let Some(m) = self.members.iter().find(|m| m.0 == *host) {
                    return if m.1 == *member { Ok(Answer::Done(m.1)) } else { Err(Refusal::HostIsMember) };
                }
                if self.members.iter().any(|m| m.1 == *member) {
                    return Err(Refusal::MemberInUse);
                }
                if self.removed.iter().any(|r| r.1 == *member) {
                    return Err(Refusal::MemberRemoved);
                }
                if self.members.len() >= 16 {
                    return Err(Refusal::Full);
                }
                self.members.push((host.clone(), *member, index));
                return Ok(Answer::Done(*member));
            }
            Command::Remove { host } => {
                if self.group.is_none() {
                    return Err(Refusal::NotInitialized);
                }
                let Some(at) = self.members.iter().position(|m| m.0 == *host) else {
                    return self.removed.iter().rev().find(|r| r.0 == *host).map(|r| Answer::Done(r.1))
                        .ok_or(Refusal::NoSuchHost);
                };
                if self.members.len() == 1 {
                    return Err(Refusal::LastMember);
                }
                if self.removed.len() >= 1024 {
                    return Err(Refusal::Full);
                }
                let (h, id, _) = self.members.remove(at);
                self.removed.push((h, id, index));
                return Ok(Answer::Done(id));
            }
            Command::Digest { .. } => return Ok(Answer::Done(0)),
            _ => {}
        }
        if self.group.is_none() {
            return Err(Refusal::NotInitialized);
        }
        match cmd {
            Command::NetworkCreate { name, net, prefix, mtu, out } => {
                if !m_net_name(name) {
                    return Err(Refusal::BadName);
                }
                if let Some(n) = self.net(name) {
                    return if (n.net, n.prefix, n.mtu, n.out) == (*net, *prefix, *mtu, *out) {
                        Ok(Answer::Done(u64::from(n.vni)))
                    } else {
                        Err(Refusal::NetworkTaken)
                    };
                }
                if !m_subnet(*net, *prefix) || !(576..=1440).contains(mtu) {
                    return Err(Refusal::BadNetwork);
                }
                let here = span(*net, *prefix);
                if self.nets.iter().any(|n| meet(span(n.net, n.prefix), here))
                    || self.public.iter().any(|p| u64::from(p.address) >= here.0 && u64::from(p.address) <= here.1)
                {
                    return Err(Refusal::Overlaps);
                }
                if self.nets.len() >= 256 || self.next_vni > (1 << 24) - 1 {
                    return Err(Refusal::TooMany);
                }
                let vni = self.next_vni;
                self.next_vni += 1;
                self.nets.push(MNet { name: name.clone(), vni, net: *net, prefix: *prefix, mtu: *mtu, out: *out,
                                      next: net + 2 });
                self.nets.sort_by_key(|n| n.vni);
                self.revision += 1;
                Ok(Answer::Done(u64::from(vni)))
            }
            Command::NetworkDelete { name } => {
                let Some(vni) = self.net(name).map(|n| n.vni) else { return Ok(Answer::Done(0)) };
                if self.ports.iter().any(|p| p.vni == vni) {
                    return Err(Refusal::NetworkInUse);
                }
                self.nets.retain(|n| n.vni != vni);
                self.revision += 1;
                Ok(Answer::Done(u64::from(vni)))
            }
            Command::PortCreate { uuid, nic, network, node, address, firewall } => {
                if let Some(p) = self.ports.iter().find(|p| p.uuid == *uuid && p.nic == *nic) {
                    return Ok(Answer::Port { port: *p, made: false });
                }
                if *node == 0 || *nic >= 8 {
                    return Err(Refusal::BadPort);
                }
                let n = self.net(network).ok_or(Refusal::NoSuchNetwork)?.clone();
                let fw = self.fw_named(firewall)?;
                if self.ports.len() >= 4096 {
                    return Err(Refusal::TooMany);
                }
                let a = if *address == 0 {
                    let (first, last) = span(n.net, n.prefix);
                    let size = last - first + 1;
                    let mut found = None;
                    for k in 0..size {
                        let a = (first + (u64::from(n.next) - first + k) % size) as u32;
                        if m_usable(&n, a) && !self.taken(n.vni, a) {
                            found = Some(a);
                            break;
                        }
                    }
                    found.ok_or(Refusal::NoAddress)?
                } else if !m_usable(&n, *address) {
                    return Err(Refusal::BadAddress);
                } else if self.taken(n.vni, *address) {
                    return Err(Refusal::AddressTaken);
                } else {
                    *address
                };
                let port = Port { id: self.next_port, uuid: *uuid, nic: *nic, vni: n.vni, address: a, node: *node,
                                  generation: self.next_gen, firewall: fw };
                self.next_port += 1;
                self.next_gen += 1;
                if let Some(m) = self.nets.iter_mut().find(|m| m.vni == n.vni) {
                    m.next = m_after(&n, a);
                }
                self.ports.push(port);
                self.revision += 1;
                Ok(Answer::Port { port, made: true })
            }
            Command::PortMove { port, expect, to } => {
                let p = *self.port(*port).ok_or(Refusal::NoSuchPort)?;
                if p.node != *expect {
                    return Ok(Answer::Elsewhere(p.node));
                }
                if *to == 0 {
                    return Err(Refusal::BadPort);
                }
                if *to == p.node {
                    return Ok(Answer::Done(p.generation));
                }
                let g = self.next_gen;
                self.next_gen += 1;
                if let Some(q) = self.ports.iter_mut().find(|q| q.id == *port) {
                    q.node = *to;
                    q.generation = g;
                }
                self.revision += 1;
                Ok(Answer::Done(g))
            }
            Command::PortDelete { port } => {
                let Some(at) = self.ports.iter().position(|p| p.id == *port) else {
                    return Ok(Answer::Deleted { public: 0 });
                };
                self.ports.remove(at);
                let mut public = 0;
                if let Some(x) = self.public.iter_mut().find(|x| x.port == *port) {
                    x.port = 0;
                    public = x.address;
                }
                self.revision += 1;
                Ok(Answer::Deleted { public })
            }
            Command::PortFirewall { port, firewall } => {
                let fw = self.fw_named(firewall)?;
                let p = self.port(*port).ok_or(Refusal::NoSuchPort)?;
                if p.firewall == fw {
                    return Ok(Answer::Done(u64::from(fw)));
                }
                if let Some(q) = self.ports.iter_mut().find(|q| q.id == *port) {
                    q.firewall = fw;
                }
                self.revision += 1;
                Ok(Answer::Done(u64::from(fw)))
            }
            Command::PublicAdd { address, gateway } => {
                if *gateway == 0 || !self.may_be_public(*address) {
                    return Err(Refusal::BadPublic);
                }
                if let Some(p) = self.public.iter().find(|p| p.address == *address) {
                    return if p.gateway == *gateway { Ok(Answer::Done(u64::from(*address))) }
                           else { Err(Refusal::PublicTaken) };
                }
                if self.public.len() >= 1024 || self.at_gateway(*gateway) >= 64 {
                    return Err(Refusal::TooMany);
                }
                self.public.push(MPublic { address: *address, gateway: *gateway, port: 0 });
                self.public.sort_by_key(|p| p.address);
                self.revision += 1;
                Ok(Answer::Done(u64::from(*address)))
            }
            Command::PublicRemove { address } => {
                let Some(at) = self.public.iter().position(|p| p.address == *address) else {
                    return Ok(Answer::Done(0));
                };
                if self.public[at].port != 0 {
                    return Err(Refusal::PublicInUse);
                }
                self.public.remove(at);
                self.revision += 1;
                Ok(Answer::Done(u64::from(*address)))
            }
            Command::PublicSet { address, gateway, port, expect_gateway, expect_port } => {
                let p = *self.public.iter().find(|p| p.address == *address).ok_or(Refusal::NoSuchPublic)?;
                if (p.gateway, p.port) != (*expect_gateway, *expect_port) {
                    return Err(Refusal::Stale);
                }
                if *gateway == 0 {
                    return Err(Refusal::BadPublic);
                }
                if (p.gateway, p.port) == (*gateway, *port) {
                    return Ok(Answer::Done(u64::from(*address)));
                }
                if *port != 0 {
                    if self.port(*port).is_none() {
                        return Err(Refusal::NoSuchPort);
                    }
                    if self.public.iter().any(|x| x.port == *port && x.address != *address) {
                        return Err(Refusal::PortHasPublic);
                    }
                }
                if *gateway != p.gateway && self.at_gateway(*gateway) >= 64 {
                    return Err(Refusal::TooMany);
                }
                if let Some(x) = self.public.iter_mut().find(|x| x.address == *address) {
                    x.gateway = *gateway;
                    x.port = *port;
                }
                self.revision += 1;
                Ok(Answer::Done(u64::from(*address)))
            }
            Command::FirewallCreate { name } => {
                if !m_fw_name(name) {
                    return Err(Refusal::BadName);
                }
                if let Some(f) = self.fw(name) {
                    return Ok(Answer::Done(u64::from(f.id)));
                }
                if self.fws.len() >= 256 || self.next_fw == u32::MAX {
                    return Err(Refusal::TooMany);
                }
                let id = self.next_fw;
                self.next_fw += 1;
                self.fws.push(MFirewall { id, name: name.clone(), revision: 1, inbound: Action::Deny,
                                          outbound: Action::Allow, rules: Vec::new() });
                self.revision += 1;
                Ok(Answer::Done(u64::from(id)))
            }
            Command::FirewallSet { name, revision, inbound, outbound, rules } => {
                let f = self.fw(name).ok_or(Refusal::NoSuchFirewall)?;
                if f.revision != *revision {
                    return Err(Refusal::Stale);
                }
                if rules.len() > 64 {
                    return Err(Refusal::TooManyRules);
                }
                let id = f.id;
                let now = revision + 1;
                if let Some(f) = self.fws.iter_mut().find(|f| f.id == id) {
                    f.revision = now;
                    f.inbound = *inbound;
                    f.outbound = *outbound;
                    f.rules = rules.clone();
                }
                self.revision += 1;
                Ok(Answer::Done(now))
            }
            Command::FirewallDelete { name } => {
                if name == "default" {
                    return Err(Refusal::DefaultKept);
                }
                let Some(id) = self.fw(name).map(|f| f.id) else { return Ok(Answer::Done(0)) };
                if self.ports.iter().any(|p| p.firewall == id) {
                    return Err(Refusal::FirewallInUse);
                }
                self.fws.retain(|f| f.id != id);
                self.revision += 1;
                Ok(Answer::Done(u64::from(id)))
            }
            Command::Init { .. } | Command::Add { .. } | Command::Remove { .. } | Command::Digest { .. } => {
                Err(Refusal::Malformed)
            }
        }
    }

    /// What an attach of `uuid`'s NIC on `node` is answered, as the model
    /// has it: the port and its public address, the node it is on, or none.
    fn attach(&self, uuid: &[u8; 16], nic: u8, node: u64) -> (Option<Port>, Option<u64>, Option<u32>) {
        match self.ports.iter().find(|p| p.uuid == *uuid && p.nic == nic) {
            None => (None, None, None),
            Some(p) if p.node != node => (None, Some(p.node), None),
            Some(p) => (Some(*p), None, self.public.iter().find(|x| x.port == p.id).map(|x| x.address)),
        }
    }
}

/// Where the state differs from the model, said.
fn differs(s: &State, m: &Model) -> Option<String> {
    let group = s.group().map(|g| (g.id, g.name.clone()));
    if group != m.group {
        return Some(format!("group {:?}, the model's {:?}", group, m.group));
    }
    let members: Vec<(String, u64, u64)> = s.roll().members().iter().map(|x| (x.host.clone(), x.id, x.since)).collect();
    if members != m.members {
        return Some(format!("members {:?}, the model's {:?}", members, m.members));
    }
    let removed: Vec<(String, u64, u64)> = s.roll().removed().iter().map(|x| (x.host.clone(), x.id, x.at)).collect();
    if removed != m.removed {
        return Some(format!("removed {:?}, the model's {:?}", removed, m.removed));
    }
    let voters: Vec<u64> = m.members.iter().take(5).map(|x| x.1).collect();
    let learners: Vec<u64> = m.members.iter().skip(5).map(|x| x.1).collect();
    if s.voters() != voters || s.learners() != learners {
        return Some(format!("voters {:?}, learners {:?}, the model's {:?} and {:?}", s.voters(), s.learners(), voters,
                            learners));
    }
    let nets: Vec<(String, u32, u32, u8, u16, bool, u32)> =
        s.networks().map(|n| (n.name.clone(), n.vni, n.net, n.prefix, n.mtu, n.out, n.next)).collect();
    let mnets: Vec<(String, u32, u32, u8, u16, bool, u32)> =
        m.nets.iter().map(|n| (n.name.clone(), n.vni, n.net, n.prefix, n.mtu, n.out, n.next)).collect();
    if nets != mnets {
        return Some(format!("networks {:?}, the model's {:?}", nets, mnets));
    }
    let ports: Vec<Port> = s.ports().copied().collect();
    let mut mports = m.ports.clone();
    mports.sort_by_key(|p| p.id);
    if ports != mports {
        return Some(format!("ports {:?}, the model's {:?}", ports, mports));
    }
    let public: Vec<(u32, u64, u64)> = s.public().map(|p| (p.address, p.gateway, p.port)).collect();
    let mpublic: Vec<(u32, u64, u64)> = m.public.iter().map(|p| (p.address, p.gateway, p.port)).collect();
    if public != mpublic {
        return Some(format!("public addresses {:?}, the model's {:?}", public, mpublic));
    }
    let fws: Vec<(u32, String, u64, Action, Action, Vec<Rule>)> = s.firewalls()
        .map(|f| (f.id, f.name.clone(), f.revision, f.inbound, f.outbound, (*f.rules).clone())).collect();
    let mut mfws: Vec<(u32, String, u64, Action, Action, Vec<Rule>)> = m.fws.iter()
        .map(|f| (f.id, f.name.clone(), f.revision, f.inbound, f.outbound, f.rules.clone())).collect();
    mfws.sort_by_key(|f| f.0);
    if fws != mfws {
        return Some(format!("firewalls {:?}, the model's {:?}", fws, mfws));
    }
    if s.revision() != m.revision {
        return Some(format!("revision {}, the model's {}", s.revision(), m.revision));
    }
    for p in &m.ports {
        if s.port_of(&p.uuid, p.nic) != Some(p) || s.port(p.id) != Some(p) {
            return Some(format!("port {} not found as itself", p.id));
        }
    }
    None
}

/// What the state's attach answers, as the model's three.
fn attached(s: &State, uuid: &[u8; 16], nic: u8, node: u64) -> (Option<Port>, Option<u64>, Option<u32>) {
    match attach(s, uuid, nic, node) {
        Attach::Here { port, network, public } => {
            if network.vni != port.vni {
                return (None, Some(u64::MAX), None);
            }
            (Some(*port), None, public)
        }
        Attach::Elsewhere(node) => (None, Some(node), None),
        Attach::None => (None, None, None),
    }
}

/* ---- the commands ---- */

const HOSTS: [&str; 6] = ["ex44", "ax41", "a", "b", "c", "node-4"];
const BAD_HOSTS: [&str; 3] = ["", "Ex44", "a b"];
const NETS: [&str; 5] = ["lan", "dmz", "db", "x", "a-very-long-network-name-1"];
const BAD_NETS: [&str; 3] = ["", "Lan", "a-very-long-network-name-12"];
/// Subnets that meet: /24s and their /16, a /25 inside one, /28s, and none
/// of a network -- not private, too wide or narrow, off their boundary, hv's
/// own.
const SUBNETS: [(u32, u8); 14] = [
    (0x0A0A_0000, 24), (0x0A0A_0100, 24), (0x0A0A_0000, 16), (0x0A0A_0080, 25), (0xC0A8_0510, 28),
    (0xC0A8_0520, 28), (0xAC10_0000, 16), (0x0A0B_0000, 24),
    (0x0808_0800, 24), (0xAC10_0000, 12), (0x0A0A_0001, 24), (0x0A00_6400, 24), (0x0A00_0000, 16), (0xC0A8_0500, 29),
];
const MTUS: [u16; 5] = [1420, 1440, 576, 1500, 575];
const NODES: [u64; 4] = [1, 2, 3, 0];
const FWS: [&str; 5] = ["", "default", "edge", "web", "Edge!"];
const RULES: [&str; 6] = [
    "allow in tcp 22 from 198.51.100.0/24", "allow in from net", "deny out tcp 25", "allow in tcp 80,443",
    "allow in udp 53", "deny in icmp",
];

fn uuid(r: &mut Rng) -> [u8; 16] {
    [r.below(4) as u8; 16]
}

/// An address of a subnet the pools have -- some a port may have, some its
/// own, its gateway's, its broadcast, one ending in 0 or 255 -- or 0.
fn address(r: &mut Rng) -> u32 {
    if r.chance(40) {
        return 0;
    }
    let (net, prefix) = r.pick(&SUBNETS[..8]).unwrap_or((0x0A0A_0000, 24));
    let size = 1u64 << (32 - u32::from(prefix));
    match r.below(6) {
        0 => net,
        1 => net + 1,
        2 => (u64::from(net) + size - 1) as u32,
        3 => (u64::from(net) | 0xff) as u32 & (net | ((size - 1) as u32)),
        _ => (u64::from(net) + 2 + r.below(size.min(300) - 3)) as u32,
    }
}

fn public_address(r: &mut Rng) -> u32 {
    match r.below(5) {
        0 => 0,
        1 => address(r),
        _ => 0xCB00_7100 + r.below(4) as u32,
    }
}

fn rules(r: &mut Rng) -> Vec<Rule> {
    let n = if r.chance(3) { 65 } else { r.below(5) as usize };
    (0..n).map(|_| Rule::parse(r.pick(&RULES).unwrap_or(RULES[0])).unwrap_or_else(|_| Rule::parse(RULES[0]).unwrap()))
        .collect()
}

/// A command, from pools small enough that its parts meet -- and, as often
/// as not, of what the state has now, as a client that read it asks: a port
/// there is, a public address's gateway and port as they are, a firewall's
/// revision, an address of a network there is, near its first.
fn command(r: &mut Rng, wide: bool, m: &Model) -> Command {
    let host = |r: &mut Rng| -> String {
        String::from(if r.chance(5) { r.pick(&BAD_HOSTS).unwrap_or("") } else { r.pick(&HOSTS).unwrap_or("a") })
    };
    let net = |r: &mut Rng| -> String {
        if !m.nets.is_empty() && r.chance(60) {
            return m.nets[r.below(m.nets.len() as u64) as usize].name.clone();
        }
        String::from(if r.chance(5) { r.pick(&BAD_NETS).unwrap_or("") } else { r.pick(&NETS).unwrap_or("lan") })
    };
    let fw = |r: &mut Rng| -> String {
        if !m.fws.is_empty() && r.chance(40) {
            return m.fws[r.below(m.fws.len() as u64) as usize].name.clone();
        }
        String::from(r.pick(&FWS).unwrap_or(""))
    };
    let port = |r: &mut Rng| -> u64 {
        if !m.ports.is_empty() && r.chance(70) {
            return m.ports[r.below(m.ports.len() as u64) as usize].id;
        }
        r.range(1, if wide { 40 } else { 8 })
    };
    let addr = |r: &mut Rng| -> u32 {
        if !m.nets.is_empty() && r.chance(50) {
            let n = &m.nets[r.below(m.nets.len() as u64) as usize];
            return n.net + r.below(10) as u32;
        }
        address(r)
    };
    let public = |r: &mut Rng| -> u32 {
        if !m.public.is_empty() && r.chance(50) {
            return m.public[r.below(m.public.len() as u64) as usize].address;
        }
        if wide && r.chance(50) {
            return 0xCB00_7100 + r.below(80) as u32;
        }
        public_address(r)
    };
    let node = |r: &mut Rng| -> u64 { r.pick(&NODES).unwrap_or(1) };
    match r.below(if wide { 24 } else { 22 }) {
        0 => Command::Init { group: [r.below(3) as u8; 16], name: host(r), host: host(r), member: r.below(4) },
        1 => Command::Add { host: host(r), member: r.below(6) },
        2 => Command::Remove { host: host(r) },
        3..=4 => {
            let (n, prefix) = r.pick(&SUBNETS).unwrap_or((0x0A0A_0000, 24));
            Command::NetworkCreate { name: net(r), net: n, prefix, mtu: r.pick(&MTUS).unwrap_or(1420), out: r.chance(80) }
        }
        5 => Command::NetworkDelete { name: net(r) },
        6..=10 => Command::PortCreate { uuid: uuid(r), nic: if r.chance(5) { 9 } else { r.below(2) as u8 },
                                        network: net(r), node: node(r), address: if r.chance(40) { 0 } else { addr(r) },
                                        firewall: fw(r) },
        11..=12 => {
            let p = port(r);
            let at = m.port(p).map(|x| x.node);
            Command::PortMove { port: p, expect: at.filter(|_| r.chance(60)).unwrap_or_else(|| node(r)), to: node(r) }
        }
        13 => Command::PortDelete { port: port(r) },
        14 => Command::PortFirewall { port: port(r), firewall: fw(r) },
        15 => Command::PublicAdd { address: public(r), gateway: if wide { 1 + r.below(2) } else { node(r) } },
        16 => Command::PublicRemove { address: public(r) },
        17 => {
            let address = public(r);
            let now = m.public.iter().find(|p| p.address == address).map(|p| (p.gateway, p.port));
            let (expect_gateway, expect_port) = now.filter(|_| r.chance(70))
                .unwrap_or_else(|| (node(r), if r.chance(50) { 0 } else { port(r) }));
            Command::PublicSet { address, gateway: if r.chance(70) { expect_gateway.max(1) } else { node(r) },
                                 port: if r.chance(30) { 0 } else { port(r) }, expect_gateway, expect_port }
        }
        18 => Command::FirewallCreate { name: fw(r) },
        19 => {
            let name = fw(r);
            let now = m.fw(&name).map(|f| f.revision);
            Command::FirewallSet { name, revision: now.filter(|_| r.chance(70)).unwrap_or_else(|| r.range(0, 4)),
                                   inbound: if r.chance(50) { Action::Deny } else { Action::Allow },
                                   outbound: if r.chance(80) { Action::Allow } else { Action::Deny }, rules: rules(r) }
        }
        20 => Command::FirewallDelete { name: fw(r) },
        21 => Command::Digest { at: r.below(200), digest: [r.below(2) as u8; 32] },
        /* Wide: many ports, so that a /28 fills and the ports go round */
        _ => Command::PortCreate { uuid: [r.below(250) as u8; 16], nic: 0, network: net(r), node: 1 + r.below(3),
                                   address: 0, firewall: String::new() },
    }
}

/// Whether a command's strings and lists fit what its bytes carry: what
/// does not reads back as none.
fn fits(cmd: &Command) -> bool {
    match cmd {
        Command::Init { name, host, .. } => name.len() <= 63 && host.len() <= 63,
        Command::Add { host, .. } | Command::Remove { host } => host.len() <= 63,
        Command::NetworkCreate { name, .. } | Command::NetworkDelete { name } => name.len() <= 26,
        Command::PortCreate { network, firewall, .. } => network.len() <= 26 && firewall.len() <= 32,
        Command::PortFirewall { firewall, .. } => firewall.len() <= 32,
        Command::FirewallCreate { name } | Command::FirewallDelete { name } => name.len() <= 32,
        Command::FirewallSet { name, rules, .. } => name.len() <= 32 && rules.len() <= 64,
        _ => true,
    }
}

/// A command's bytes, now and then made into others: cut, a bit flipped,
/// noise.
fn entry(r: &mut Rng, cmd: &Command) -> Vec<u8> {
    let mut b = cmd.encode();
    if r.chance(6) {
        match r.below(3) {
            0 => b.truncate(r.below(b.len() as u64) as usize),
            1 => {
                let at = r.below(b.len() as u64) as usize;
                b[at] ^= 1 << r.below(8);
            }
            _ => b = (0..r.below(40)).map(|_| r.next() as u8).collect(),
        }
    }
    b
}

fn spoil(r: &mut Rng, mut b: Vec<u8>) -> Vec<u8> {
    match r.below(4) {
        0 if !b.is_empty() => {
            let at = r.below(b.len() as u64) as usize;
            b[at] = b[at].wrapping_add(1 + r.below(255) as u8);
        }
        1 => b.truncate(r.below(b.len() as u64 + 1) as usize),
        2 => b.extend((0..r.range(1, 16)).map(|_| r.next() as u8)),
        _ => b = (0..r.below(200)).map(|_| r.next() as u8).collect(),
    }
    b
}

fn answer_name(a: &Answer) -> String {
    match a {
        Answer::Refused(why) => format!("{:?}", why),
        Answer::Done(_) => String::from("Done"),
        Answer::Port { made, .. } => format!("Port(made {})", made),
        Answer::Elsewhere(_) => String::from("Elsewhere"),
        Answer::Deleted { .. } => String::from("Deleted"),
    }
}

/// `RAFT_VNETMETA_STATS=1`: what each run's commands were answered, and how
/// far its state got.
fn stats(seed: u64, answers: &BTreeMap<String, u64>, s: &State) {
    if std::env::var_os("RAFT_VNETMETA_STATS").is_some() {
        eprintln!("vnetmeta seed {}: networks {}, ports {}, public {}, firewalls {}, revision {}, answers {:?}", seed,
                  s.networks().count(), s.ports().count(), s.public().count(), s.firewalls().count(), s.revision(),
                  answers);
    }
}

pub fn run(seed: u64, steps: u64) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x766e_6574));
    let wide = seed % 3 == 0;
    let mut answers: BTreeMap<String, u64> = BTreeMap::new();
    let mut s = State::new();
    let mut m = Model::default();
    let mut replica: Option<State> = None;
    /* Made first in most runs, so that the rest is about the networks */
    if r.chance(90) {
        let init = Command::Init { group: [9; 16], name: String::from("vnet"), host: String::from("a"), member: 7 };
        let got = Answer::decode(&s.apply(1, &init.encode())).ok_or("the init's answer does not read back")?;
        let want = m.apply(1, Some(&init));
        if got != want {
            return Err(format!("the init answered {:?}, the model {:?}", got, want));
        }
    }
    for step in 2..=steps + 1 {
        let cmd = command(&mut r, wide, &m);
        let bytes = entry(&mut r, &cmd);
        let decoded = Command::decode(&bytes);
        if bytes == cmd.encode() && fits(&cmd) && decoded.as_ref() != Some(&cmd) {
            return Err(format!("step {}: {:?} does not read back from its bytes", step, cmd));
        }
        let got = Answer::decode(&s.apply(step, &bytes))
            .ok_or_else(|| format!("step {}: an answer that does not read back", step))?;
        let want = m.apply(step, decoded.as_ref());
        if got != want {
            return Err(format!("step {}: {:?} answered {:?}, the model {:?}", step, decoded, got, want));
        }
        *answers.entry(answer_name(&got)).or_default() += 1;
        if let Some(rep) = replica.as_mut() {
            rep.apply(step, &bytes);
        }
        if let Some(d) = differs(&s, &m) {
            return Err(format!("step {} after {:?}: {}", step, decoded, d));
        }
        /* An attach answered as the model has it */
        let (u, nic, node) = (uuid(&mut r), r.below(2) as u8, r.pick(&NODES).unwrap_or(1));
        if attached(&s, &u, nic, node) != m.attach(&u, nic, node) {
            return Err(format!("step {}: an attach of {:?}/{} on {} answered {:?}, the model {:?}", step, u[0], nic,
                               node, attached(&s, &u, nic, node), m.attach(&u, nic, node)));
        }
        /* Its snapshot read back as itself, and in pieces as one */
        if r.chance(10) {
            let bytes = s.encode();
            let back = State::decode(&bytes).map_err(|e| format!("step {}: its own snapshot refused: {:?}", step, e))?;
            if back != s {
                return Err(format!("step {}: its snapshot read back as another state", step));
            }
            let mut frozen = s.freeze();
            let mut pieces = Vec::new();
            loop {
                let before = pieces.len();
                raft::Frozen::write(&mut frozen, r.range(1, 64) as usize, &mut pieces);
                if pieces.len() == before {
                    break;
                }
            }
            if pieces != bytes {
                return Err(format!("step {}: the frozen state's pieces are not its snapshot", step));
            }
            if replica.is_none() && r.chance(30) {
                replica = Some(back);
            }
        }
        /* A snapshot changed: refused, or a state that holds to the rules
           and reads back as itself */
        if r.chance(5) {
            let spoiled = spoil(&mut r, s.encode());
            if let Ok(t) = State::decode(&spoiled) {
                let again = State::decode(&t.encode()).map_err(|e| {
                    format!("step {}: a state read from a changed snapshot refused as itself: {:?}", step, e)
                })?;
                if again != t {
                    return Err(format!("step {}: a state read from a changed snapshot does not read back", step));
                }
            }
        }
    }
    if let Some(rep) = replica {
        if rep != s {
            return Err(String::from("a replica started from a snapshot part way ends another state"));
        }
    }
    stats(seed, &answers, &s);
    Ok(())
}

/* ---- the scenarios: what each rule is there for ---- */

fn made() -> State {
    let mut s = State::new();
    s.apply_command(1, &Command::Init { group: [9; 16], name: String::from("vnet"), host: String::from("a"),
                                        member: 7 });
    s
}

fn net(s: &mut State, index: u64, name: &str, net: u32, prefix: u8) -> Answer {
    s.apply_command(index, &Command::NetworkCreate { name: String::from(name), net, prefix, mtu: 1420, out: true })
}

fn port(s: &mut State, index: u64, u: u8, network: &str, node: u64, address: u32) -> Answer {
    s.apply_command(index, &Command::PortCreate { uuid: [u; 16], nic: 0, network: String::from(network), node,
                                                  address, firewall: String::new() })
}

fn expect(what: &str, got: Answer, want: Answer) -> Result<(), String> {
    if got == want { Ok(()) } else { Err(format!("{}: {:?}, not {:?}", what, got, want)) }
}

fn port_of(a: Answer) -> Result<Port, String> {
    match a {
        Answer::Port { port, .. } => Ok(port),
        other => Err(format!("no port: {:?}", other)),
    }
}

const LAN: u32 = 0x0A0A_0000;

/// A VM's NIC has one port: asked again -- of another network, another node,
/// another address -- the one there is, as it is, never bound anew.
fn vnetmeta_port_once() -> Result<(), String> {
    let mut s = made();
    net(&mut s, 2, "lan", LAN, 24);
    net(&mut s, 3, "dmz", 0x0A0B_0000, 24);
    let p = port_of(port(&mut s, 4, 1, "lan", 1, 0))?;
    expect("asked again", port(&mut s, 5, 1, "dmz", 2, 0x0A0B_0005), Answer::Port { port: p, made: false })?;
    if s.port(p.id) != Some(&p) {
        return Err(String::from("the port changed by an ask again"));
    }
    Ok(())
}

/// A move is a compare and set of the node: from another node refused,
/// saying where the port is; done, a new generation; to where it is, none.
fn vnetmeta_move_cas() -> Result<(), String> {
    let mut s = made();
    net(&mut s, 2, "lan", LAN, 24);
    let p = port_of(port(&mut s, 3, 1, "lan", 1, 0))?;
    let mv = |s: &mut State, i, expect, to| s.apply_command(i, &Command::PortMove { port: p.id, expect, to });
    expect("a move from a node it is not on", mv(&mut s, 4, 2, 3), Answer::Elsewhere(1))?;
    let g = match mv(&mut s, 5, 1, 2) {
        Answer::Done(g) => g,
        other => return Err(format!("a move: {:?}", other)),
    };
    if g == p.generation || s.port(p.id).map(|x| (x.node, x.generation)) != Some((2, g)) {
        return Err(format!("moved: generation {} from {}, the port {:?}", g, p.generation, s.port(p.id)));
    }
    expect("a move where it is", mv(&mut s, 6, 2, 2), Answer::Done(g))?;
    expect("the old node's move again", mv(&mut s, 7, 1, 3), Answer::Elsewhere(2))?;
    Ok(())
}

/// Numbers are never given again: a network made after one deleted has a
/// new number, a port a new id and generation.
fn vnetmeta_numbers() -> Result<(), String> {
    let mut s = made();
    let a = net(&mut s, 2, "lan", LAN, 24);
    s.apply_command(3, &Command::NetworkDelete { name: String::from("lan") });
    let b = net(&mut s, 4, "lan", LAN, 24);
    if a == b {
        return Err(format!("a network made again took the number it had: {:?}", a));
    }
    let p = port_of(port(&mut s, 5, 1, "lan", 1, 0))?;
    s.apply_command(6, &Command::PortDelete { port: p.id });
    let q = port_of(port(&mut s, 7, 1, "lan", 1, 0))?;
    if q.id == p.id || q.generation == p.generation {
        return Err(format!("a port made again took an id or a generation it had: {:?} after {:?}", q, p));
    }
    Ok(())
}

/// Subnets: overlapping, hv's own, no private range's, too wide or narrow,
/// off their boundary, an MTU past its bounds -- refused.
fn vnetmeta_subnets() -> Result<(), String> {
    let mut s = made();
    expect("lan", net(&mut s, 2, "lan", LAN, 24), Answer::Done(1))?;
    expect("one overlapping", net(&mut s, 3, "wide", LAN, 16), Answer::Refused(Refusal::Overlaps))?;
    expect("one inside", net(&mut s, 4, "half", 0x0A0A_0080, 25), Answer::Refused(Refusal::Overlaps))?;
    expect("hv's own", net(&mut s, 5, "host", 0x0A00_6400, 24), Answer::Refused(Refusal::BadNetwork))?;
    expect("around hv's own", net(&mut s, 6, "big", 0x0A00_0000, 16), Answer::Refused(Refusal::BadNetwork))?;
    expect("a public range", net(&mut s, 7, "pub", 0x0808_0800, 24), Answer::Refused(Refusal::BadNetwork))?;
    expect("a /29", net(&mut s, 8, "tiny", 0xC0A8_0500, 29), Answer::Refused(Refusal::BadNetwork))?;
    expect("off its boundary", net(&mut s, 9, "off", 0x0A0B_0001, 24), Answer::Refused(Refusal::BadNetwork))?;
    let mtu = s.apply_command(10, &Command::NetworkCreate { name: String::from("jumbo"), net: 0x0A0C_0000, prefix: 24,
                                                            mtu: 1500, out: true });
    expect("an MTU past 1440", mtu, Answer::Refused(Refusal::BadNetwork))?;
    expect("lan again, as itself", net(&mut s, 11, "lan", LAN, 24), Answer::Done(1))?;
    expect("lan again, another", net(&mut s, 12, "lan", 0x0A0D_0000, 24), Answer::Refused(Refusal::NetworkTaken))?;
    port(&mut s, 13, 1, "lan", 1, 0);
    let del = s.apply_command(14, &Command::NetworkDelete { name: String::from("lan") });
    expect("a network with a port deleted", del, Answer::Refused(Refusal::NetworkInUse))?;
    Ok(())
}

/// Addresses: given round the subnet from past the last, one let go of not
/// the next; none of the network's own, its gateway's, its broadcast, or
/// ending in 0 or 255; a /28 full refused.
fn vnetmeta_addresses() -> Result<(), String> {
    let mut s = made();
    net(&mut s, 2, "lan", LAN, 24);
    let a = port_of(port(&mut s, 3, 1, "lan", 1, 0))?;
    let b = port_of(port(&mut s, 4, 2, "lan", 1, 0))?;
    if (a.address, b.address) != (LAN + 2, LAN + 3) {
        return Err(format!("the first two addresses {:x} and {:x}", a.address, b.address));
    }
    s.apply_command(5, &Command::PortDelete { port: a.id });
    let c = port_of(port(&mut s, 6, 3, "lan", 1, 0))?;
    if c.address != LAN + 4 {
        return Err(format!("after one let go of, {:x} given -- not the next round", c.address));
    }
    for (u, a) in [(4, LAN), (5, LAN + 1), (6, LAN + 255)] {
        expect("a network's own address", port(&mut s, 7, u, "lan", 1, a), Answer::Refused(Refusal::BadAddress))?;
    }
    expect("an address taken", port(&mut s, 8, 7, "lan", 1, LAN + 3), Answer::Refused(Refusal::AddressTaken))?;
    /* A /28 of 192.168.5.16: .18 to .30 are a port's, .16, .17 and .31 not */
    net(&mut s, 9, "small", 0xC0A8_0510, 28);
    let mut given = Vec::new();
    for u in 0..13u8 {
        given.push(port_of(port(&mut s, 10 + u64::from(u), 20 + u, "small", 1, 0))?.address);
    }
    let want: Vec<u32> = (0xC0A8_0512..=0xC0A8_051E).collect();
    if given != want {
        return Err(format!("a /28's addresses {:x?}", given));
    }
    expect("a /28 full", port(&mut s, 30, 40, "small", 1, 0), Answer::Refused(Refusal::NoAddress))?;
    /* A /24 crossing .255 and .0 of no /24 -- 10.10.1.0/24's are its own */
    let mut s = made();
    s.apply_command(2, &Command::NetworkCreate { name: String::from("big"), net: 0x0A0A_0000, prefix: 22, mtu: 1420,
                                                 out: true });
    for u in 0..600u16 {
        let mut id = [0u8; 16];
        id[..2].copy_from_slice(&u.to_le_bytes());
        let p = port_of(s.apply_command(3 + u64::from(u), &Command::PortCreate {
            uuid: id, nic: 0, network: String::from("big"), node: 1, address: 0, firewall: String::new() }))?;
        if p.address & 0xff == 0 || p.address & 0xff == 0xff {
            return Err(format!("an address ending in 0 or 255 given: {:x}", p.address));
        }
    }
    Ok(())
}

/// A public address: on one port at most, a port's one at most; its port
/// deleted lets it go and says so; removed only when on none; set only from
/// what was read.
fn vnetmeta_public() -> Result<(), String> {
    let mut s = made();
    net(&mut s, 2, "lan", LAN, 24);
    let p = port_of(port(&mut s, 3, 1, "lan", 1, 0))?;
    let q = port_of(port(&mut s, 4, 2, "lan", 1, 0))?;
    let (x, y) = (0xCB00_7109, 0xCB00_710A);
    let set = |s: &mut State, i, address, port, expect_port| {
        s.apply_command(i, &Command::PublicSet { address, gateway: 1, port, expect_gateway: 1, expect_port })
    };
    expect("x added", s.apply_command(5, &Command::PublicAdd { address: x, gateway: 1 }), Answer::Done(u64::from(x)))?;
    expect("y added", s.apply_command(6, &Command::PublicAdd { address: y, gateway: 1 }), Answer::Done(u64::from(y)))?;
    expect("x on p", set(&mut s, 7, x, p.id, 0), Answer::Done(u64::from(x)))?;
    expect("y on p too", set(&mut s, 8, y, p.id, 0), Answer::Refused(Refusal::PortHasPublic))?;
    expect("x on q, from what was not read", set(&mut s, 9, x, q.id, 0), Answer::Refused(Refusal::Stale))?;
    let rm = s.apply_command(10, &Command::PublicRemove { address: x });
    expect("x removed while on p", rm, Answer::Refused(Refusal::PublicInUse))?;
    let del = s.apply_command(11, &Command::PortDelete { port: p.id });
    expect("p deleted, x let go of", del, Answer::Deleted { public: x })?;
    if s.public_of(x).map(|a| a.port) != Some(0) {
        return Err(String::from("x still on the deleted port"));
    }
    let inside = s.apply_command(12, &Command::PublicAdd { address: LAN + 9, gateway: 1 });
    expect("an address of a network made public", inside, Answer::Refused(Refusal::BadPublic))?;
    /* And the other way: a network made around a public address */
    let lone = 0x0A28_0005;
    expect("10.40.0.5 made public", s.apply_command(13, &Command::PublicAdd { address: lone, gateway: 1 }),
           Answer::Done(u64::from(lone)))?;
    expect("a network around it", net(&mut s, 14, "around", 0x0A28_0000, 24), Answer::Refused(Refusal::Overlaps))?;
    Ok(())
}

/// Firewalls: `default` made with the group and never deleted; one a port
/// has not deleted; its rules changed only from the revision read.
fn vnetmeta_firewalls() -> Result<(), String> {
    let mut s = made();
    if s.firewall("default").map(|f| (f.inbound, f.outbound, f.rules.len())) != Some((Action::Deny, Action::Allow, 0)) {
        return Err(String::from("no `default` firewall, inbound denied and outbound allowed, made with the group"));
    }
    let del = |s: &mut State, i, name: &str| s.apply_command(i, &Command::FirewallDelete { name: String::from(name) });
    expect("default deleted", del(&mut s, 2, "default"), Answer::Refused(Refusal::DefaultKept))?;
    let edge = s.apply_command(3, &Command::FirewallCreate { name: String::from("edge") });
    net(&mut s, 4, "lan", LAN, 24);
    let p = port_of(port(&mut s, 5, 1, "lan", 1, 0))?;
    s.apply_command(6, &Command::PortFirewall { port: p.id, firewall: String::from("edge") });
    expect("edge deleted while p has it", del(&mut s, 7, "edge"), Answer::Refused(Refusal::FirewallInUse))?;
    let rule = Rule::parse("allow in tcp 22").map_err(String::from)?;
    let fset = |s: &mut State, i, revision| s.apply_command(i, &Command::FirewallSet {
        name: String::from("edge"), revision, inbound: Action::Deny, outbound: Action::Allow, rules: vec![rule] });
    expect("edge's rules from revision 1", fset(&mut s, 8, 1), Answer::Done(2))?;
    expect("again from revision 1", fset(&mut s, 9, 1), Answer::Refused(Refusal::Stale))?;
    s.apply_command(10, &Command::PortFirewall { port: p.id, firewall: String::new() });
    if !matches!(edge, Answer::Done(_)) || del(&mut s, 11, "edge") == Answer::Refused(Refusal::FirewallInUse) {
        return Err(String::from("edge not deleted once no port has it"));
    }
    Ok(())
}

/// An attach: the port bound here answered with its network and public
/// address; one bound elsewhere, the node it is on; none, none.
fn vnetmeta_attach() -> Result<(), String> {
    let mut s = made();
    net(&mut s, 2, "lan", LAN, 24);
    let p = port_of(port(&mut s, 3, 1, "lan", 1, 0))?;
    match attach(&s, &[1; 16], 0, 1) {
        Attach::Here { port, network, public: None } if *port == p && network.name == "lan" => {}
        other => return Err(format!("an attach of the port here: {:?}", other)),
    }
    if attach(&s, &[1; 16], 0, 2) != Attach::Elsewhere(1) {
        return Err(format!("an attach on another node: {:?}", attach(&s, &[1; 16], 0, 2)));
    }
    if attach(&s, &[2; 16], 0, 1) != Attach::None {
        return Err(String::from("an attach of a VM with no port answered with one"));
    }
    s.apply_command(4, &Command::PortMove { port: p.id, expect: 1, to: 2 });
    if attach(&s, &[1; 16], 0, 1) != Attach::Elsewhere(2) {
        return Err(String::from("an attach where the port was, after its move, answered as here"));
    }
    Ok(())
}

/// A snapshot that breaks a rule -- two ports at one address, a port of no
/// firewall -- refused, though its bytes are well formed.
fn vnetmeta_snapshot_rules() -> Result<(), String> {
    let mut s = made();
    net(&mut s, 2, "lan", LAN, 24);
    let a = port_of(port(&mut s, 3, 1, "lan", 1, 0))?;
    let b = port_of(port(&mut s, 4, 2, "lan", 1, 0))?;
    let good = s.encode();
    State::decode(&good).map_err(|e| format!("a good snapshot refused: {:?}", e))?;
    /* b's address written in for a's: two ports at one address */
    let mut bad = good.clone();
    let at = bad.windows(4).position(|w| w == a.address.to_le_bytes()).ok_or("a's address not in the snapshot")?;
    bad[at..at + 4].copy_from_slice(&b.address.to_le_bytes());
    if State::decode(&bad).is_ok() {
        return Err(String::from("a snapshot with two ports at one address taken"));
    }
    Ok(())
}

/// The ports' bound: the 4097th refused.
fn vnetmeta_bounds() -> Result<(), String> {
    let mut s = made();
    s.apply_command(2, &Command::NetworkCreate { name: String::from("big"), net: 0x0A10_0000, prefix: 16, mtu: 1420,
                                                 out: true });
    for i in 0..4096u32 {
        let mut u = [0u8; 16];
        u[..4].copy_from_slice(&i.to_le_bytes());
        let got = s.apply_command(3 + u64::from(i), &Command::PortCreate { uuid: u, nic: 0, network: String::from("big"),
                                                                            node: 1, address: 0,
                                                                            firewall: String::new() });
        if !matches!(got, Answer::Port { made: true, .. }) {
            return Err(format!("port {}: {:?}", i, got));
        }
    }
    let over = s.apply_command(9000, &Command::PortCreate { uuid: [0xff; 16], nic: 0, network: String::from("big"),
                                                             node: 1, address: 0, firewall: String::new() });
    expect("the 4097th port", over, Answer::Refused(Refusal::TooMany))?;
    /* Sixty-four public addresses a gateway, its NAT's one to one */
    let mut s = made();
    for i in 0..64u32 {
        let a = 0xCB00_7100 + i;
        expect("a public address", s.apply_command(2 + u64::from(i), &Command::PublicAdd { address: a, gateway: 1 }),
               Answer::Done(u64::from(a)))?;
    }
    let over = s.apply_command(100, &Command::PublicAdd { address: 0xCB00_7200, gateway: 1 });
    expect("a gateway's sixty-fifth", over, Answer::Refused(Refusal::TooMany))?;
    expect("another gateway's first", s.apply_command(101, &Command::PublicAdd { address: 0xCB00_7200, gateway: 2 }),
           Answer::Done(0xCB00_7200))?;
    /* 256 networks */
    let mut s = made();
    for i in 0..256u32 {
        let got = net(&mut s, 2 + u64::from(i), &format!("n{}", i), 0x0A14_0000 + (i << 8), 24);
        if got != Answer::Done(u64::from(i) + 1) {
            return Err(format!("network {}: {:?}", i, got));
        }
    }
    expect("the 257th network", net(&mut s, 300, "over", 0x0A15_0000, 24), Answer::Refused(Refusal::TooMany))?;
    Ok(())
}

pub const SCENARIOS: &[(&str, fn() -> Result<(), String>)] = &[
    ("vnetmeta_port_once", vnetmeta_port_once),
    ("vnetmeta_move_cas", vnetmeta_move_cas),
    ("vnetmeta_numbers", vnetmeta_numbers),
    ("vnetmeta_subnets", vnetmeta_subnets),
    ("vnetmeta_addresses", vnetmeta_addresses),
    ("vnetmeta_public", vnetmeta_public),
    ("vnetmeta_firewalls", vnetmeta_firewalls),
    ("vnetmeta_attach", vnetmeta_attach),
    ("vnetmeta_snapshot_rules", vnetmeta_snapshot_rules),
    ("vnetmeta_bounds", vnetmeta_bounds),
];
