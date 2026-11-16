//! What the `vnet` command and the `vnet` provider both do -- each a value,
//! said by the shell in words and by the provider as JSON -- so that what a
//! command does a call does. Every change is asked of the group
//! (`raftmember::ask`) and answered once applied here too; what applying
//! cannot see -- a host's own address covered -- is looked at here first.

use alloc::string::String;
use alloc::vec::Vec;

use firewall::edit::{self, Edit};
use firewall::{Action, Dir, Rule};
use netwire::Ipv4 as Ip;
use vnetmeta::{Answer, Command, Firewall, Network, Port, Refusal};

use crate::member::View;
use crate::registry::{self, Host};
use crate::{ModState, NO_MEMBER};

/// Why a change was not made: as the bus answers it -- a parameter that is
/// none, what is not there, what is in the way -- or anything else.
#[derive(Debug)]
pub enum Refused {
    Invalid { why: String, field: String },
    NotFound(String),
    Conflict(String),
    Failed(String),
}

impl Refused {
    pub fn text(&self) -> &str {
        match self {
            Refused::Invalid { why, .. } | Refused::NotFound(why) | Refused::Conflict(why) | Refused::Failed(why) => why,
        }
    }

    pub fn invalid(why: String, field: &str) -> Refused {
        Refused::Invalid { why, field: String::from(field) }
    }

    /// The state's refusal, as what it is; `field` the parameter it is
    /// about where it is one's.
    fn of(r: Refusal, field: &'static str) -> Refused {
        let why = String::from(r.says());
        match r {
            Refusal::BadName | Refusal::BadNetwork | Refusal::BadAddress | Refusal::BadPort | Refusal::BadPublic
            | Refusal::BadMember | Refusal::TooManyRules => Refused::Invalid { why, field: String::from(field) },
            Refusal::NoSuchNetwork | Refusal::NoSuchPort | Refusal::NoSuchFirewall | Refusal::NoSuchPublic
            | Refusal::NoSuchHost => Refused::NotFound(why),
            Refusal::Malformed => Refused::Failed(why),
            _ => Refused::Conflict(why),
        }
    }
}

/// What a string error of the member's or the registry's is: no member a
/// conflict, the rest failed.
impl From<String> for Refused {
    fn from(why: String) -> Refused {
        if why.starts_with("this host is in no vnet") || why.contains(" is no host of the cluster registry") {
            Refused::Conflict(why)
        } else {
            Refused::Failed(why)
        }
    }
}

pub fn member_view(state: &ModState) -> Result<View, String> {
    state.group.member_view(NO_MEMBER)
}

/// `cmd`, as the state answers it -- asked again while the member cannot
/// take it yet.
fn ask(state: &ModState, cmd: &Command) -> Result<Answer, Refused> {
    raftmember::ask(&state.group.runner.shared, cmd, &state.group.runner.no_member(NO_MEMBER)).map_err(Refused::from)
}

/// An answer that is done: its value; a refusal as what it is.
fn done(a: Answer, field: &'static str) -> Result<u64, Refused> {
    match a {
        Answer::Done(v) => Ok(v),
        Answer::Refused(why) => Err(Refused::of(why, field)),
        other => Err(Refused::Failed(alloc::format!("answered {:?}", other))),
    }
}

/// The hosts' address a subnet covers, where one does: no network's VMs
/// and a host are one address twice.
pub fn covered(hosts: &[Host], net: u32, prefix: u8) -> Option<(String, u32)> {
    hosts.iter().find_map(|h| h.addresses.iter().find(|&&a| vnetmeta::holds(net, prefix, a)).map(|&a| (h.name.clone(), a)))
}

/// `a.b.c.d/prefix`.
pub fn parse_subnet(text: &str) -> Option<(u32, u8)> {
    let (ip, prefix) = text.split_once('/')?;
    Some((netwire::parse_ipv4(ip.as_bytes())?, prefix.parse().ok()?))
}

pub fn parse_ip(text: &str, field: &str) -> Result<u32, Refused> {
    netwire::parse_ipv4(text.as_bytes()).filter(|&a| a != 0)
        .ok_or_else(|| Refused::invalid(alloc::format!("\"{}\" is not an IPv4 address", text), field))
}

/// A VM's UUID: 32 hex digits, its dashes or none.
pub fn parse_uuid(text: &str) -> Result<[u8; 16], Refused> {
    let hex: String = text.chars().filter(|&c| c != '-').collect();
    codec::unhex::<16>(&hex).ok_or_else(|| Refused::invalid(alloc::format!("\"{}\" is not a VM's UUID", text), "uuid"))
}

pub fn uuid_text(u: &[u8; 16]) -> String {
    let h = codec::hex(u);
    alloc::format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/* ---- networks ---- */

/// A network made: its number. Refused where its subnet holds a host's own
/// address, which applying cannot see.
pub fn network_create(state: &ModState, name: &str, net: u32, prefix: u8, mtu: u16, out: bool) -> Result<u32, Refused> {
    let hosts = registry::hosts()?;
    if let Some((host, a)) = covered(&hosts, net, prefix) {
        return Err(Refused::Conflict(alloc::format!("{}/{} holds {}, {}'s own address: its VMs and that host would be \
                                                     one address twice", Ip(net), prefix, Ip(a), host)));
    }
    let cmd = Command::NetworkCreate { name: String::from(name), net, prefix, mtu, out };
    done(ask(state, &cmd)?, "subnet").map(|v| v as u32)
}

/// A network deleted: its number, 0 where it was gone already.
pub fn network_delete(state: &ModState, name: &str) -> Result<u32, Refused> {
    done(ask(state, &Command::NetworkDelete { name: String::from(name) })?, "name").map(|v| v as u32)
}

/* ---- ports ---- */

/// A port made for a VM's NIC on `host` -- this one where none is named --
/// or the one there is, and what differs from what was asked.
pub fn port_create(state: &ModState, network: &str, uuid: [u8; 16], nic: u8, address: u32, fw: &str,
                   host: Option<&str>) -> Result<(Port, bool, Vec<&'static str>), Refused> {
    let hosts = registry::hosts()?;
    let host = match host {
        Some(h) => String::from(h),
        None => raftmember::ipcnet::identity()?.name,
    };
    let node = registry::node_of(&hosts, &host)?;
    let cmd = Command::PortCreate { uuid, nic, network: String::from(network), node, address,
                                    firewall: String::from(fw) };
    match ask(state, &cmd)? {
        Answer::Port { port, made } => {
            let mut differs = Vec::new();
            if !made {
                let v = member_view(state)?;
                if v.state.network_of(port.vni).is_some_and(|n| n.name != network) {
                    differs.push("its network");
                }
                if port.node != node {
                    differs.push("its host");
                }
                if address != 0 && port.address != address {
                    differs.push("its address");
                }
                if !fw.is_empty() && v.state.firewall_of(port.firewall).is_some_and(|f| f.name != fw) {
                    differs.push("its firewall");
                }
            }
            Ok((port, made, differs))
        }
        other => Err(match done(other, "network") {
            Err(e) => e,
            Ok(_) => Refused::Failed(String::from("a port's make answered with no port")),
        }),
    }
}

/// What hv is told of a VM's NIC on a network of the cluster's as the VM
/// is made or started here (plans/vnet.md, "hv and vnet"): its port, its
/// network, its public address, its firewall.
pub struct Attached {
    pub port: Port,
    pub network: Network,
    pub public: Option<u32>,
    pub firewall: Option<Firewall>,
    /// Whether the leader vouched for the state it was answered from: with
    /// none to ask, a port this member's own state binds here is answered
    /// all the same, and hv says it is unconfirmed until it is.
    pub confirmed: bool,
    /// Whether the port was made for it now.
    pub made: bool,
}

/// How long an attach waits for the leader to vouch for the state it is
/// answered from: raft's read, asked again until a leader is known.
pub const ATTACH_WAIT_MS: u64 = 5000;

/// VM `uuid`'s NIC `nic` attached here, on `network` -- its port bound to
/// this host, read from the state the leader vouches for. A port bound to
/// another host refused, the host named; one on another network refused;
/// none there made, where `make` says to -- at the address asked for, or
/// the next free one, judged by firewall `fw` or `default` -- which a
/// member with no leader cannot do.
pub fn attach(state: &ModState, uuid: [u8; 16], nic: u8, network: &str, address: u32, fw: &str, make: bool)
    -> Result<Attached, Refused>
{
    let hosts = registry::hosts()?;
    let me = raftmember::ipcnet::identity()?.name;
    let node = registry::node_of(&hosts, &me)?;
    let confirmed = raftmember::read(&state.group.runner.shared, &state.group.runner.no_member(NO_MEMBER),
                                     ATTACH_WAIT_MS).is_ok();
    let v = member_view(state)?;
    let attached = |port: &Port, made: bool, confirmed: bool| -> Result<Attached, Refused> {
        let v = member_view(state)?;
        let n = v.state.network_of(port.vni)
            .ok_or_else(|| Refused::Failed(alloc::format!("port {}'s network is gone", port.id)))?;
        Ok(Attached {
            port: *port, network: n.clone(), public: v.state.public_on(port.id).map(|p| p.address),
            firewall: v.state.firewall_of(port.firewall).cloned(), confirmed, made,
        })
    };
    match vnetmeta::attach(&v.state, &uuid, nic, node) {
        vnetmeta::Attach::Here { port, network: n, .. } => {
            if n.name != network {
                return Err(Refused::Conflict(alloc::format!("its port {} is on {}, not on {}", port.id, n.name,
                                                            network)));
            }
            attached(port, false, confirmed)
        }
        vnetmeta::Attach::Elsewhere(other) => {
            let port = v.state.port_of(&uuid, nic).map_or(0, |p| p.id);
            Err(Refused::Conflict(alloc::format!("its port {} is {}'s -- `vnet port move {} {}` once it runs \
                                                  nowhere else", port, registry::name_of(&hosts, other), port, me)))
        }
        vnetmeta::Attach::None if !make => Err(Refused::NotFound(String::from("it has no port on a network of the \
                                                                                 cluster's"))),
        vnetmeta::Attach::None if !confirmed => Err(Refused::Failed(String::from("vnet has no leader to make its \
                                                                                   port with -- ask again once it \
                                                                                   has one"))),
        vnetmeta::Attach::None => {
            let (port, made, _) = port_create(state, network, uuid, nic, address, fw, None)?;
            attached(&port, made, true)
        }
    }
}

/// VM `uuid`'s NIC `nic`'s port deleted, wherever it is: its id, 0 where it
/// had none -- and the public address on it let go of.
pub fn detach(state: &ModState, uuid: [u8; 16], nic: u8) -> Result<u64, Refused> {
    let v = member_view(state)?;
    let Some(port) = v.state.port_of(&uuid, nic).copied() else { return Ok(0) };
    port_delete(state, port.id)?;
    Ok(port.id)
}

/// Port `id` moved to `host`, from where it is now: its new generation.
pub fn port_move(state: &ModState, id: u64, host: &str) -> Result<u64, Refused> {
    let hosts = registry::hosts()?;
    let to = registry::node_of(&hosts, host)?;
    let v = member_view(state)?;
    let p = *v.state.port(id).ok_or_else(|| Refused::NotFound(alloc::format!("no port {}", id)))?;
    match ask(state, &Command::PortMove { port: id, expect: p.node, to })? {
        Answer::Elsewhere(node) => Err(Refused::Conflict(alloc::format!("port {} is {}'s, moved meanwhile -- look again",
                                                                        id, registry::name_of(&hosts, node)))),
        other => done(other, "host"),
    }
}

/// Port `id` deleted: the public address let go of with it, 0 for none.
pub fn port_delete(state: &ModState, id: u64) -> Result<u32, Refused> {
    match ask(state, &Command::PortDelete { port: id })? {
        Answer::Deleted { public } => Ok(public),
        other => done(other, "port").map(|_| 0),
    }
}

pub fn port_firewall(state: &ModState, id: u64, fw: &str) -> Result<(), Refused> {
    done(ask(state, &Command::PortFirewall { port: id, firewall: String::from(fw) })?, "firewall").map(|_| ())
}

/* ---- public addresses ---- */

/// A public address routed to `host`: refused where it is a host's own.
pub fn public_add(state: &ModState, address: u32, host: &str) -> Result<(), Refused> {
    let hosts = registry::hosts()?;
    let gateway = registry::node_of(&hosts, host)?;
    if let Some(h) = hosts.iter().find(|h| h.addresses.contains(&address)) {
        return Err(Refused::Conflict(alloc::format!("{} is {}'s own address: no VM's public one", Ip(address),
                                                    h.name)));
    }
    done(ask(state, &Command::PublicAdd { address, gateway })?, "address").map(|_| ())
}

/// A public address put on port `port` -- 0 for none -- from where it is.
pub fn public_set(state: &ModState, address: u32, port: u64) -> Result<(), Refused> {
    let v = member_view(state)?;
    let p = *v.state.public_of(address)
        .ok_or_else(|| Refused::NotFound(alloc::format!("no public address {}", Ip(address))))?;
    let cmd = Command::PublicSet { address, gateway: p.gateway, port, expect_gateway: p.gateway, expect_port: p.port };
    done(ask(state, &cmd)?, "port").map(|_| ())
}

/// A public address routed to `host` now, on the port it is on.
pub fn public_move(state: &ModState, address: u32, host: &str) -> Result<(), Refused> {
    let hosts = registry::hosts()?;
    let gateway = registry::node_of(&hosts, host)?;
    let v = member_view(state)?;
    let p = *v.state.public_of(address)
        .ok_or_else(|| Refused::NotFound(alloc::format!("no public address {}", Ip(address))))?;
    let cmd = Command::PublicSet { address, gateway, port: p.port, expect_gateway: p.gateway, expect_port: p.port };
    done(ask(state, &cmd)?, "host").map(|_| ())
}

/// A public address removed: false where it was gone already.
pub fn public_remove(state: &ModState, address: u32) -> Result<bool, Refused> {
    done(ask(state, &Command::PublicRemove { address })?, "address").map(|v| v != 0)
}

/* ---- the cluster's firewalls ---- */

/// A firewall made, with `policies` where any is given: its id.
pub fn firewall_create(state: &ModState, name: &str, policies: Vec<(Dir, Action)>) -> Result<u64, Refused> {
    let id = done(ask(state, &Command::FirewallCreate { name: String::from(name) })?, "name")?;
    if !policies.is_empty() {
        firewall_edit(state, name, &Edit::Policy(policies))?;
    }
    Ok(id)
}

/// A firewall deleted: false where it was gone already.
pub fn firewall_delete(state: &ModState, name: &str) -> Result<bool, Refused> {
    done(ask(state, &Command::FirewallDelete { name: String::from(name) })?, "name").map(|v| v != 0)
}

/// Firewall `name` changed by `e`: its rules read, changed here and set
/// whole where its revision is still the one read -- asked again once from a
/// new read where another change came first. What was done, said.
pub fn firewall_edit(state: &ModState, name: &str, e: &Edit) -> Result<String, Refused> {
    for _ in 0..2 {
        let v = member_view(state)?;
        let f = v.state.firewall(name).ok_or_else(|| Refused::NotFound(alloc::format!("no firewall {}", name)))?;
        let mut rules: Vec<Rule> = (*f.rules).clone();
        let (mut inbound, mut outbound) = (f.inbound, f.outbound);
        let said = edit::apply_list(e, &mut rules, &mut inbound, &mut outbound)
            .map_err(|b| Refused::invalid(b.why, b.field))?;
        let cmd = Command::FirewallSet { name: String::from(name), revision: f.revision, inbound, outbound, rules };
        match ask(state, &cmd)? {
            Answer::Done(_) => return Ok(said),
            Answer::Refused(Refusal::Stale) => continue,
            other => return done(other, "name").map(|_| said),
        }
    }
    Err(Refused::Conflict(alloc::format!("firewall {} changed under this change twice -- look at it, and ask again",
                                         name)))
}

/* ---- what is wrong ---- */

/// What is wrong, as `vnet alerts` and the provider say it: its kind, and
/// the words.
pub struct Alert {
    pub kind: &'static str,
    pub says: String,
}

/// What is wrong now, the worst first: no member, a member stopped, no
/// leader; the registry not reached; ports and public addresses on hosts
/// the registry has not; a network or a public address covering a host's
/// own address.
pub fn alerts(state: &ModState) -> Vec<Alert> {
    let mut out = Vec::new();
    let mut add = |kind: &'static str, says: String| out.push(Alert { kind, says });
    let Some(v) = state.group.view() else {
        add("no_member", state.group.runner.no_member(NO_MEMBER));
        return out;
    };
    if let Some(why) = &v.stopped {
        add("stopped", alloc::format!("the member stopped: {}", why));
        return out;
    }
    if v.state.roll().member_of_id(v.leader).is_none() {
        add("no_leader", String::from("no leader: no change is made until a majority of the members is heard"));
    }
    let hosts = match registry::hosts() {
        Ok(h) => h,
        Err(why) => {
            add("no_registry", why);
            return out;
        }
    };
    for p in v.state.ports() {
        if !hosts.iter().any(|h| h.node == p.node) {
            add("port_nowhere", alloc::format!("port {} is on node {}, which the registry has not -- `vnet port \
                                                 delete {}` or `vnet port move {} <host>`", p.id, p.node, p.id, p.id));
        }
    }
    for x in v.state.public() {
        if !hosts.iter().any(|h| h.node == x.gateway) {
            add("gateway_gone", alloc::format!("public address {} is routed to node {}, which the registry has not -- \
                                                 `vnet ip move {} <host>`", Ip(x.address), x.gateway, Ip(x.address)));
        }
        if let Some(h) = hosts.iter().find(|h| h.addresses.contains(&x.address)) {
            add("covers_host", alloc::format!("public address {} is {}'s own address now", Ip(x.address), h.name));
        }
    }
    for n in v.state.networks() {
        if let Some((host, a)) = covered(&hosts, n.net, n.prefix) {
            add("covers_host", alloc::format!("network {} holds {}, {}'s own address now", n.name, Ip(a), host));
        }
    }
    out
}
