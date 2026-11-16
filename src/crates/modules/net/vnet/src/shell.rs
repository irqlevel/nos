//! The `vnet` command: the group's members, the networks, their ports, the
//! public addresses and the cluster's firewalls -- each change made by the
//! same functions the provider's calls are (ops.rs), said in words.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use firewall::edit;
use kcore::cmd::Output;
use netwire::Ipv4 as Ip;
use raft::Role;
use raftmember::offered;
use raftmember::Recovered;
use vnetmeta::{attach, Attach, Network, Port, State, MTU_DEFAULT};

use crate::member::View;
use crate::ops::{self, Refused};
use crate::registry::{self, Host};
use crate::{ModState, NO_MEMBER};

const USAGE: &str = "usage: vnet                                 this host's member: role, leader, what it applied\n\
                     \x20      vnet init <name>                    a vnet of this host alone\n\
                     \x20      vnet hosts                          every member: incarnation, vote, the leader\n\
                     \x20      vnet host add|remove|replace <host> a host of the registry made a member, out, back\n\
                     \x20      vnet recover [<name>]               on the last member standing: the one voter\n\
                     \x20      vnet create <name> <net>/<prefix> [mtu=<n>] [out=no]   a network; .1 its gateway\n\
                     \x20      vnet delete <name> | vnet list      a network deleted; every network\n\
                     \x20      vnet ports [<network>] | vnet port <id>   the ports; one, and an attach here\n\
                     \x20      vnet port create <network> <uuid> [nic=<n>] [address=<ip>] [firewall=<name>] \
                     [host=<host>]\n\
                     \x20      vnet port move <id> <host> | delete <id> | firewall <id> <firewall>\n\
                     \x20      vnet ip [add <address> <host> | set <address> <port>|none | move <address> <host> \
                     | remove <address>]\n\
                     \x20      vnet fw [create <name> [in=..] [out=..] | delete <name> | <name> [add <rule> [at=N] \
                     | del <N> | move <N> <M> | policy in=.. out=..]]\n\
                     \x20      vnet alerts                         what is wrong, the worst first";

fn member_view(state: &ModState) -> Result<View, String> {
    ops::member_view(state)
}

fn port_number(text: &str) -> Result<u64, String> {
    text.parse::<u64>().ok().filter(|&n| n != 0).ok_or_else(|| alloc::format!("\"{}\" is no port's number", text))
}

/// The addresses a port of `n` may have: its subnet's, less its own, its
/// gateway's, its broadcast and those ending in 0 or 255.
pub(crate) fn usable(n: &Network) -> u64 {
    let size = 1u64 << (32 - u32::from(n.prefix));
    (0..size).filter(|&k| n.may_have(n.net.wrapping_add(k as u32))).count() as u64
}

/// An operation's refusal, as the shell says it.
fn said<T>(r: Result<T, Refused>) -> Result<T, String> {
    r.map_err(|e| String::from(e.text()))
}

pub(crate) fn run(state: &Arc<ModState>, args: &str, out: &mut Output) -> Result<(), String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] | ["status"] => status(state, out),
        ["init", name] => {
            let made = offered::init(state, name, NO_MEMBER)?;
            let _ = writeln!(out, "vnet: {} made ({}), {} its first member ({}) -- `vnet host add` the next", name,
                             codec::hex(&made.group), made.host, made.member);
            Ok(())
        }
        ["hosts"] => hosts(state, out),
        ["host", "add", host] => {
            let id = state.group.add(host, &|| false, NO_MEMBER)?;
            let _ = writeln!(out, "vnet: {} is member {} -- a learner until it holds the log, then a voter while \
                                   fewer than five vote", host, id);
            Ok(())
        }
        ["host", "remove", host] => {
            let id = state.group.remove(host, NO_MEMBER)?;
            let _ = writeln!(out, "vnet: {}'s member {} removed, for good", host, id);
            Ok(())
        }
        ["host", "replace", host] => {
            let (old, new) = state.group.replace(host, &|| false, NO_MEMBER)?;
            let _ = writeln!(out, "vnet: {}'s member {} removed, and {} is member {} now", host, old, host, new);
            Ok(())
        }
        ["recover"] => recover(state, None, out),
        ["recover", name] => recover(state, Some(name), out),
        ["create", name, subnet, rest @ ..] => create(state, name, subnet, rest, out),
        ["delete", name] => {
            match said(ops::network_delete(state, name))? {
                0 => {
                    let _ = writeln!(out, "vnet: no network {}: gone already", name);
                }
                vni => {
                    let _ = writeln!(out, "vnet: network {} ({}) deleted", name, vni);
                }
            }
            Ok(())
        }
        ["list"] => list(state, out),
        ["ports"] => ports(state, None, out),
        ["ports", network] => ports(state, Some(network), out),
        ["port", "create", network, uuid, rest @ ..] => port_create(state, network, uuid, rest, out),
        ["port", "move", id, host] => {
            let id = port_number(id)?;
            let g = said(ops::port_move(state, id, host))?;
            let _ = writeln!(out, "vnet: port {} is {}'s now, generation {}", id, host, g);
            Ok(())
        }
        ["port", "delete", id] => {
            let id = port_number(id)?;
            match said(ops::port_delete(state, id))? {
                0 => {
                    let _ = writeln!(out, "vnet: port {} deleted", id);
                }
                public => {
                    let _ = writeln!(out, "vnet: port {} deleted, and public address {} let go of with it", id,
                                     Ip(public));
                }
            }
            Ok(())
        }
        ["port", "firewall", id, fw] => {
            let id = port_number(id)?;
            said(ops::port_firewall(state, id, fw))?;
            let _ = writeln!(out, "vnet: port {} judged by {} from now on", id, fw);
            Ok(())
        }
        ["port", id] => port(state, id, out),
        ["ip"] => public(state, out),
        ["ip", "add", address, host] => {
            let address = said(ops::parse_ip(address, "address"))?;
            said(ops::public_add(state, address, host))?;
            let _ = writeln!(out, "vnet: public address {}, routed to {}", Ip(address), host);
            Ok(())
        }
        ["ip", "set", address, to] => {
            let address = said(ops::parse_ip(address, "address"))?;
            let port = match *to {
                "none" => 0,
                id => port_number(id)?,
            };
            said(ops::public_set(state, address, port))?;
            if port == 0 {
                let _ = writeln!(out, "vnet: {} on no port", Ip(address));
            } else {
                let _ = writeln!(out, "vnet: {} on port {}", Ip(address), port);
            }
            Ok(())
        }
        ["ip", "move", address, host] => {
            let address = said(ops::parse_ip(address, "address"))?;
            said(ops::public_move(state, address, host))?;
            let _ = writeln!(out, "vnet: {} routed to {} now", Ip(address), host);
            Ok(())
        }
        ["ip", "remove", address] => {
            let address = said(ops::parse_ip(address, "address"))?;
            if said(ops::public_remove(state, address))? {
                let _ = writeln!(out, "vnet: public address {} removed", Ip(address));
            } else {
                let _ = writeln!(out, "vnet: no public address {}: gone already", Ip(address));
            }
            Ok(())
        }
        ["fw", rest @ ..] => fw(state, rest, out),
        ["alerts"] => {
            for a in ops::alerts(state) {
                let _ = writeln!(out, "vnet: {}", a.says);
            }
            Ok(())
        }
        _ => Err(String::from(USAGE)),
    }
}

pub(crate) fn role_name(r: Role) -> &'static str {
    match r {
        Role::Leader => "leader",
        Role::Follower => "follower",
        Role::Candidate | Role::PreCandidate => "candidate",
    }
}

fn status(state: &ModState, out: &mut Output) -> Result<(), String> {
    let Some(v) = state.group.view() else {
        let _ = writeln!(out, "vnet: {}", state.group.runner.no_member(NO_MEMBER));
        return Ok(());
    };
    if let Some(why) = &v.stopped {
        let _ = writeln!(out, "vnet: the member stopped: {}", why);
    }
    let roll = v.state.roll();
    if roll.group().is_some() && roll.member_of_id(v.node).is_none() {
        let _ = writeln!(out, "vnet: this host's member ({}) is removed from the vnet -- `rmmod vnet`, `rm {}` and \
                               `insmod` it, then `vnet host add` it again", v.node, state.group.dir);
    }
    let (name, id) = roll.group().map_or((String::from("(no state yet)"), String::new()),
                                          |g| (g.name.clone(), alloc::format!(" ({})", codec::hex(&g.id))));
    let leader = roll.member_of_id(v.leader).map_or("none", |m| m.host.as_str());
    let _ = writeln!(out, "vnet: {}{} -- member {}, {}, term {}, leader {}, applied {}, {} members, {} voters, {} \
                           learners, {} messages refused", name, id, v.node, role_name(v.role), v.term, leader,
                     v.applied, roll.members().len(), v.membership.voters.len(), v.membership.learners.len(),
                     v.refused);
    let _ = writeln!(out, "vnet: {} networks, {} ports, {} public addresses, {} firewalls -- revision {}",
                     v.state.networks().count(), v.state.ports().count(), v.state.public().count(),
                     v.state.firewalls().count(), v.state.revision());
    match v.state.checks().latest() {
        Some((at, d)) => {
            let _ = writeln!(out, "vnet: the state's digest at entry {}: {}... -- {} of the leader's held to it, {} too \
                                   old to", at, codec::hex(&d[..8]), v.state.checks().held, v.state.checks().unheld);
        }
        None => {
            let _ = writeln!(out, "vnet: no digest of the state yet -- one every {} entries", vnetmeta::CHECK_EVERY);
        }
    }
    let lost: Vec<(String, u64)> = state.group.lost.with(|l| l.iter().map(|(h, &(i, _))| (h.clone(), i)).collect());
    for (host, id) in lost {
        let _ = writeln!(out, "vnet: {} says its vnet member has no member {}: its state is gone -- `vnet host replace \
                               {}`", host, id, host);
    }
    Ok(())
}

fn hosts(state: &ModState, out: &mut Output) -> Result<(), String> {
    let v = member_view(state)?;
    let lost: Vec<String> = state.group.lost.with(|l| l.keys().cloned().collect());
    let roll = v.state.roll();
    for m in roll.members() {
        let vote = if v.membership.voters.contains(&m.id) {
            "voter"
        } else if v.membership.learners.contains(&m.id) {
            "learner"
        } else {
            "joining"
        };
        let mut more = String::new();
        if m.id == v.leader {
            more.push_str(" (leader)");
        }
        if m.id == v.node {
            more.push_str(" (this host)");
        }
        if lost.contains(&m.host) {
            more.push_str(" lost its state");
        }
        let _ = writeln!(out, "  {:<16} member {:<20} {:<8}{}", m.host, m.id, vote, more);
    }
    if !roll.removed().is_empty() {
        let _ = writeln!(out, "  {} members removed", roll.removed().len());
    }
    Ok(())
}

/// The last member standing made the vnet's one voter (raftmember's
/// `recover`, as every group's): the other members removed for good.
fn recover(state: &Arc<ModState>, confirm: Option<&str>, out: &mut Output) -> Result<(), String> {
    let host = raftmember::ipcnet::identity().map(|id| id.name).unwrap_or_else(|_| String::from("<this host>"));
    let remedy = crate::lost_remedy(state, &host);
    let (r, done) = offered::recover(state, confirm, NO_MEMBER, &remedy)?;
    let none_or = |list: &[&str]| if list.is_empty() { String::from("none") } else { list.join(", ") };
    match done {
        Recovered::Asked => {
            let names: Vec<&str> = r.others.iter().map(|(n, _)| n.as_str()).collect();
            let _ = writeln!(out, "vnet: recover makes {}'s member ({}) the one voter of vnet {}, and removes every \
                                   other member for good -- each back only as a new member, by `vnet host add` once \
                                   its folder is gone; whatever they had committed that {} had not is gone: a network, \
                                   a port, a public address, a firewall's change. No other voter answers. To go \
                                   ahead: vnet recover {}", r.me.0, r.me.1, r.group, r.me.0, r.group);
            let _ = writeln!(out, "vnet: the members removed: {}", none_or(&names));
        }
        Recovered::Done { removed, kept } => {
            for (host, why) in &kept {
                let _ = writeln!(out, "vnet: {}'s member not removed: {} -- `vnet host remove {}` again", host, why,
                                 host);
            }
            let removed: Vec<&str> = removed.iter().map(|n| n.as_str()).collect();
            let _ = writeln!(out, "vnet: {}'s member is vnet {}'s one voter now; removed for good: {}", r.me.0, r.group,
                             none_or(&removed));
        }
    }
    Ok(())
}

fn create(state: &ModState, name: &str, subnet: &str, rest: &[&str], out: &mut Output) -> Result<(), String> {
    let (net, prefix) = ops::parse_subnet(subnet)
        .ok_or_else(|| alloc::format!("\"{}\" is not a subnet: a.b.c.d/prefix", subnet))?;
    let mut mtu = MTU_DEFAULT;
    let mut way_out = true;
    for w in rest {
        match w.split_once('=') {
            Some(("mtu", n)) => mtu = n.parse().map_err(|_| alloc::format!("\"{}\" is not an MTU", n))?,
            Some(("out", "no")) => way_out = false,
            Some(("out", "yes")) => way_out = true,
            _ => return Err(String::from(USAGE)),
        }
    }
    let vni = said(ops::network_create(state, name, net, prefix, mtu, way_out))?;
    let _ = writeln!(out, "vnet: network {} ({}), {}/{}, gateway {} on every host, MTU {}{}", name, vni, Ip(net), prefix,
                     Ip(net + 1), mtu, if way_out { "" } else { ", no way out" });
    Ok(())
}

fn list(state: &ModState, out: &mut Output) -> Result<(), String> {
    let v = member_view(state)?;
    for n in v.state.networks() {
        let ports = v.state.ports().filter(|p| p.vni == n.vni).count() as u64;
        let _ = writeln!(out, "  {:<26} {:>5}  {:>15}/{:<2}  mtu {}  {}  ports {}  free {}", n.name, n.vni,
                         alloc::format!("{}", Ip(n.net)), n.prefix, n.mtu, if n.out { "out" } else { "no way out" },
                         ports, usable(n).saturating_sub(ports));
    }
    Ok(())
}

/// A port, said: its network, address, host, generation, firewall and
/// public address.
fn port_line(s: &State, hosts: &[Host], p: &Port) -> String {
    let net = s.network_of(p.vni).map_or("?", |n| n.name.as_str());
    let fw = s.firewall_of(p.firewall).map_or("?", |f| f.name.as_str());
    let mut line = alloc::format!("  {:>5}  {:<16} {:>15}  {:<16} gen {:<6} fw {:<12} vm {} nic {}", p.id, net,
                                  alloc::format!("{}", Ip(p.address)), registry::name_of(hosts, p.node), p.generation,
                                  fw, ops::uuid_text(&p.uuid), p.nic);
    if let Some(x) = s.public_on(p.id) {
        let _ = write!(line, "  public {}", Ip(x.address));
    }
    line
}

fn ports(state: &ModState, network: Option<&str>, out: &mut Output) -> Result<(), String> {
    let v = member_view(state)?;
    let vni = match network {
        Some(name) => Some(v.state.network(name).map(|n| n.vni).ok_or_else(|| alloc::format!("no network {}", name))?),
        None => None,
    };
    let hosts = registry::hosts().unwrap_or_default();
    for p in v.state.ports().filter(|p| vni.is_none_or(|n| p.vni == n)) {
        let _ = writeln!(out, "{}", port_line(&v.state, &hosts, p));
    }
    Ok(())
}

/// One port, and what an attach of its VM's NIC on this host is answered.
fn port(state: &ModState, id: &str, out: &mut Output) -> Result<(), String> {
    let id = port_number(id)?;
    let v = member_view(state)?;
    let p = *v.state.port(id).ok_or_else(|| alloc::format!("no port {}", id))?;
    let hosts = registry::hosts()?;
    let _ = writeln!(out, "{}", port_line(&v.state, &hosts, &p));
    let me = raftmember::ipcnet::identity()?.name;
    let node = registry::node_of(&hosts, &me)?;
    match attach(&v.state, &p.uuid, p.nic, node) {
        Attach::Here { port, network, public } => {
            let _ = write!(out, "  attached here: {}/{} gateway {} MTU {} network {} generation {}", Ip(port.address),
                           network.prefix, Ip(network.gateway()), network.mtu, network.vni, port.generation);
            if let Some(a) = public {
                let _ = write!(out, " public {}", Ip(a));
            }
            let _ = writeln!(out);
        }
        Attach::Elsewhere(n) => {
            let _ = writeln!(out, "  an attach here is refused: the port is {}'s", registry::name_of(&hosts, n));
        }
        Attach::None => {}
    }
    Ok(())
}

/// `vnet port create`: a VM's NIC's port, as hv makes one at `hv create`
/// (V2) -- for the gates and for an operator. Asked again for the same VM
/// and NIC, the port there is, and what differs from what was asked said.
fn port_create(state: &ModState, network: &str, uuid: &str, rest: &[&str], out: &mut Output) -> Result<(), String> {
    let uuid = said(ops::parse_uuid(uuid))?;
    let (mut nic, mut address, mut fw, mut host) = (0u8, 0u32, "", None);
    for w in rest {
        match w.split_once('=') {
            Some(("nic", n)) => nic = n.parse().map_err(|_| alloc::format!("\"{}\" is no NIC's number", n))?,
            Some(("address", a)) => address = said(ops::parse_ip(a, "address"))?,
            Some(("firewall", f)) => fw = f,
            Some(("host", h)) => host = Some(h),
            _ => return Err(String::from(USAGE)),
        }
    }
    let (port, made, differs) = said(ops::port_create(state, network, uuid, nic, address, fw, host))?;
    let v = member_view(state)?;
    let hosts = registry::hosts().unwrap_or_default();
    let _ = writeln!(out, "vnet: port {} {}", port.id, if made { "made" } else { "there already, as it is" });
    let _ = writeln!(out, "{}", port_line(&v.state, &hosts, &port));
    if !differs.is_empty() {
        let _ = writeln!(out, "vnet: not as asked -- {}: a VM's NIC has one port, changed by deleting it",
                         differs.join(", "));
    }
    Ok(())
}

fn public(state: &ModState, out: &mut Output) -> Result<(), String> {
    let v = member_view(state)?;
    let hosts = registry::hosts().unwrap_or_default();
    for p in v.state.public() {
        let on = if p.port == 0 { String::from("no port") } else { alloc::format!("port {}", p.port) };
        let _ = writeln!(out, "  {:>15}  routed to {:<16} on {}", alloc::format!("{}", Ip(p.address)),
                         registry::name_of(&hosts, p.gateway), on);
    }
    Ok(())
}

/* ---- the cluster's firewalls ---- */

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn fw(state: &ModState, words: &[&str], out: &mut Output) -> Result<(), String> {
    match words {
        [] => {
            let v = member_view(state)?;
            for f in v.state.firewalls() {
                let on = v.state.ports().filter(|p| p.firewall == f.id).count();
                let _ = writeln!(out, "{}: in {}, out {}, {} rule{}, revision {}, on {} port{}", f.name,
                                 f.inbound.name(), f.outbound.name(), f.rules.len(), plural(f.rules.len()), f.revision,
                                 on, plural(on));
            }
            Ok(())
        }
        ["create", name, rest @ ..] => {
            let policies = rest.iter().map(|w| edit::policy_word(w)).collect::<Result<Vec<_>, _>>()
                .map_err(|b| b.why)?;
            said(ops::firewall_create(state, name, policies))?;
            let v = member_view(state)?;
            let f = v.state.firewall(name).ok_or_else(|| alloc::format!("no firewall {}", name))?;
            let _ = writeln!(out, "vnet fw: {} made: in {}, out {}, no rule", name, f.inbound.name(), f.outbound.name());
            Ok(())
        }
        ["delete", name] => {
            if said(ops::firewall_delete(state, name))? {
                let _ = writeln!(out, "vnet fw: {} deleted", name);
            } else {
                let _ = writeln!(out, "vnet fw: no firewall {}: gone already", name);
            }
            Ok(())
        }
        [name] => {
            let v = member_view(state)?;
            let f = v.state.firewall(name).ok_or_else(|| alloc::format!("no firewall {}", name))?;
            let _ = writeln!(out, "firewall {}: in {}, out {}, revision {}", name, f.inbound.name(), f.outbound.name(),
                             f.revision);
            for (j, r) in f.rules.iter().enumerate() {
                let _ = writeln!(out, "  {:2}  {}", j + 1, r);
            }
            Ok(())
        }
        [name, rest @ ..] => match edit::parse(rest) {
            Some(Ok(e)) => {
                let done = said(ops::firewall_edit(state, name, &e))?;
                let _ = writeln!(out, "vnet fw: {}: {}", name, done);
                Ok(())
            }
            Some(Err(b)) => Err(b.why),
            None => Err(String::from(USAGE)),
        },
    }
}
