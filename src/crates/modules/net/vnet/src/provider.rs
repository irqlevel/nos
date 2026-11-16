//! The `vnet` provider on the management bus: the cluster's networks for
//! VMs as the host API, the panel and `mgmt` reach them -- through the same
//! functions the shell's commands are (ops.rs), each refusal the kind of no
//! it is.
//!
//! - reads: `status` (this host's member, and why it did not start where it
//!   did not), `hosts` (the members), `networks`, `ports`, `public`,
//!   `firewalls`, `firewall` (one, its rules among it), `alerts`;
//! - actions: `init`, `host_remove`, `network_create`, `network_delete`,
//!   `port_move`, `port_delete`, `port_firewall`, `public_add`, `public_set`,
//!   `public_move`, `public_remove`, `firewall_create`, `firewall_delete`,
//!   `firewall_policy`, `firewall_add`, `firewall_remove`, `firewall_move`;
//! - jobs: `host_add` and `host_replace` -- the host's own member offered
//!   the vnet, for as long as it takes to answer;
//! - hv's, as a VM on a network of the cluster's is made, started and
//!   deleted on this host (plans/vnet.md, "hv and vnet"): `attach` -- its
//!   port, read from the state the leader vouches for, made where there is
//!   none -- `detach`, and `table`, what hv keeps of the state: the
//!   networks, this host's ports and their firewalls, polled by the
//!   module's instance and the state's revision.
//!
//! A port is made by hv as a VM is (`attach`): the shell's `vnet port
//! create` is for the gates and an operator.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use firewall::edit::Edit;
use firewall::{Action, Dir, Rule, RULES_MAX};
use kcore::mgmt::{Call, Kind, Method, Provider, Status, Writer};
use mgmtwire::{Invalid, Params, Value};
use netwire::Ipv4 as Ip;
use raftmember::offered;
use vnetmeta::{MTU_DEFAULT, MTU_MAX, MTU_MIN};

use crate::ops::{self, Refused};
use crate::registry;
use crate::shell::{role_name, usable};
use crate::{ModState, NO_MEMBER};

const DESCRIPTOR: &str = include_str!("vnet.descriptor.json");

const METHODS: [Method; 30] = [
    Method { name: "status", kind: Kind::Read },
    Method { name: "hosts", kind: Kind::Read },
    Method { name: "networks", kind: Kind::Read },
    Method { name: "ports", kind: Kind::Read },
    Method { name: "public", kind: Kind::Read },
    Method { name: "firewalls", kind: Kind::Read },
    Method { name: "firewall", kind: Kind::Read },
    Method { name: "alerts", kind: Kind::Read },
    Method { name: "init", kind: Kind::Action },
    Method { name: "host_add", kind: Kind::Job },
    Method { name: "host_remove", kind: Kind::Action },
    Method { name: "host_replace", kind: Kind::Job },
    Method { name: "network_create", kind: Kind::Action },
    Method { name: "network_delete", kind: Kind::Action },
    Method { name: "port_move", kind: Kind::Action },
    Method { name: "port_delete", kind: Kind::Action },
    Method { name: "port_firewall", kind: Kind::Action },
    Method { name: "public_add", kind: Kind::Action },
    Method { name: "public_set", kind: Kind::Action },
    Method { name: "public_move", kind: Kind::Action },
    Method { name: "public_remove", kind: Kind::Action },
    Method { name: "firewall_create", kind: Kind::Action },
    Method { name: "firewall_delete", kind: Kind::Action },
    Method { name: "firewall_policy", kind: Kind::Action },
    Method { name: "firewall_add", kind: Kind::Action },
    Method { name: "firewall_remove", kind: Kind::Action },
    Method { name: "firewall_move", kind: Kind::Action },
    Method { name: "attach", kind: Kind::Action },
    Method { name: "detach", kind: Kind::Action },
    Method { name: "table", kind: Kind::Read },
];

/// A host's, a vnet's and a network's name at most; a firewall's; a rule's
/// text; an address's; a subnet's.
const NAME_MAX: usize = 63;
const RULE_MAX: usize = 256;
const IP_MAX: usize = 15;
const SUBNET_MAX: usize = 18;
/// A VM's UUID, its dashes and all; and the NICs a VM has, at most.
const UUID_MAX: usize = 36;
const NIC_MAX: u64 = 7;

/// The provider registered, its calls answered from `state` -- until it is
/// dropped, which waits out every call of it.
pub fn register(state: Arc<ModState>) -> Result<Provider, Status> {
    Provider::register("vnet", &METHODS, DESCRIPTOR, move |method, params, call| provide(&state, method, params, call))
}

fn refuse(call: &mut Call, status: Status, message: &str, field: Option<&str>) -> Status {
    call.json(|w| status.write_error(w, message, field));
    status
}

fn invalid(call: &mut Call, e: Invalid) -> Status {
    refuse(call, Status::Invalid, e.why, Some(e.field))
}

/// A refusal as the bus answers it: its kind's status, and the field it is
/// about where it is one's.
fn refused(call: &mut Call, r: Refused) -> Status {
    match r {
        Refused::Invalid { why, field } => refuse(call, Status::Invalid, &why, Some(&field)),
        Refused::NotFound(why) => refuse(call, Status::NotFound, &why, None),
        Refused::Conflict(why) => refuse(call, Status::Conflict, &why, None),
        Refused::Failed(why) => refuse(call, Status::Failed, &why, None),
    }
}

fn answer(call: &mut Call, write: impl FnOnce(&mut Writer)) -> Status {
    if call.json(write) { Status::Ok } else { Status::TooLarge }
}

/// An object of one field, `key`, a string.
fn said(call: &mut Call, key: &str, value: &str) -> Status {
    answer(call, |w| {
        w.begin_object();
        w.field_str(key, value);
        w.end_object();
    })
}

fn provide(state: &Arc<ModState>, method: &str, params: &[u8], call: &mut Call) -> Status {
    let value = if params.is_empty() {
        Value::Null
    } else {
        match mgmtwire::json::parse(params, &mgmtwire::Limits::PARAMS) {
            Ok(v) => v,
            Err(_) => return refuse(call, Status::Invalid, "the parameters are not JSON as the bus reads it", None),
        }
    };
    let p = match Params::new(&value) {
        Ok(p) => p,
        Err(e) => return invalid(call, e),
    };
    let done = match method {
        "status" => status(state, p, call),
        "hosts" => hosts(state, p, call),
        "networks" => networks(state, p, call),
        "ports" => ports(state, p, call),
        "public" => public(state, p, call),
        "firewalls" => firewalls(state, p, call),
        "firewall" => firewall(state, p, call),
        "alerts" => alerts(state, p, call),
        "init" => init(state, p, call),
        "host_add" => host_job(state, p, call, false),
        "host_replace" => host_job(state, p, call, true),
        "host_remove" => host_remove(state, p, call),
        "network_create" => network_create(state, p, call),
        "network_delete" => network_delete(state, p, call),
        "port_move" => port_move(state, p, call),
        "port_delete" => port_delete(state, p, call),
        "port_firewall" => port_firewall(state, p, call),
        "public_add" => public_add(state, p, call),
        "public_set" => public_set(state, p, call),
        "public_move" => public_move(state, p, call),
        "public_remove" => public_remove(state, p, call),
        "firewall_create" => firewall_create(state, p, call),
        "firewall_delete" => firewall_delete(state, p, call),
        "firewall_policy" => firewall_policy(state, p, call),
        "firewall_add" => firewall_add(state, p, call),
        "firewall_remove" => firewall_remove(state, p, call),
        "firewall_move" => firewall_move(state, p, call),
        "attach" => attach(state, p, call),
        "detach" => detach(state, p, call),
        "table" => table(state, p, call),
        _ => return Status::NoMethod,
    };
    match done {
        Ok(s) => s,
        Err(r) => refused(call, r),
    }
}

/// Parameters all read: the rest refused.
fn finish(p: Params<'_>) -> Result<(), Refused> {
    p.finish().map_err(|e| Refused::invalid(String::from(e.why), e.field))
}

fn text<'a>(p: &mut Params<'a>, key: &'a str, max: usize) -> Result<&'a str, Refused> {
    p.str(key, max).map_err(|e| Refused::invalid(String::from(e.why), e.field))
}

fn opt_text<'a>(p: &mut Params<'a>, key: &'a str, max: usize) -> Result<Option<&'a str>, Refused> {
    p.opt_str(key, max).map_err(|e| Refused::invalid(String::from(e.why), e.field))
}

fn number<'a>(p: &mut Params<'a>, key: &'a str, min: u64, max: u64) -> Result<u64, Refused> {
    p.uint(key, min, max).map_err(|e| Refused::invalid(String::from(e.why), e.field))
}

fn address<'a>(p: &mut Params<'a>, key: &'a str) -> Result<u32, Refused> {
    let t = text(p, key, IP_MAX)?;
    ops::parse_ip(t, key)
}

/// The policies a call sets: `in` and `out`, each allow or deny, where
/// given.
fn policies(p: &mut Params) -> Result<Vec<(Dir, Action)>, Refused> {
    let mut set = Vec::new();
    for (key, dir) in [("in", Dir::In), ("out", Dir::Out)] {
        if let Some(word) = opt_text(p, key, 8)? {
            let action = Action::parse(word).ok_or_else(|| Refused::invalid(String::from("is allow or deny"), key))?;
            set.push((dir, action));
        }
    }
    Ok(set)
}

/* ---- reads ---- */

fn status(state: &ModState, p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    finish(p)?;
    let Some(v) = state.group.view() else {
        /* No member: and why, where this host's did not start */
        let held = state.group.runner.held();
        return Ok(answer(call, |w| {
            w.begin_object();
            w.field_bool("member", false);
            w.key("stopped");
            match &held {
                Some(why) => w.str(why),
                None => w.null(),
            }
            w.end_object();
        }));
    };
    let roll = v.state.roll();
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_bool("member", true);
        w.key("vnet");
        match roll.group() {
            Some(g) => w.str(&g.name),
            None => w.null(),
        }
        w.field_uint("node", v.node);
        w.field_str("role", role_name(v.role));
        w.field_uint("term", v.term);
        w.key("leader");
        match roll.member_of_id(v.leader) {
            Some(m) => w.str(&m.host),
            None => w.null(),
        }
        w.field_uint("applied", v.applied);
        w.field_uint("members", roll.members().len() as u64);
        w.field_uint("voters", v.membership.voters.len() as u64);
        w.field_uint("learners", v.membership.learners.len() as u64);
        w.field_uint("networks", v.state.networks().count() as u64);
        w.field_uint("ports", v.state.ports().count() as u64);
        w.field_uint("public", v.state.public().count() as u64);
        w.field_uint("firewalls", v.state.firewalls().count() as u64);
        w.field_uint("revision", v.state.revision());
        w.field_uint("refused", v.refused);
        w.key("stopped");
        match &v.stopped {
            Some(why) => w.str(why),
            None => w.null(),
        }
        w.end_object();
    }))
}

fn hosts(state: &ModState, p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    finish(p)?;
    let v = ops::member_view(state)?;
    let lost: Vec<String> = state.group.lost.with(|l| l.keys().cloned().collect());
    Ok(answer(call, |w| {
        w.begin_object();
        w.key("hosts");
        w.begin_list();
        for m in v.state.roll().members() {
            let vote = if v.membership.voters.contains(&m.id) {
                "voter"
            } else if v.membership.learners.contains(&m.id) {
                "learner"
            } else {
                "joining"
            };
            w.begin_object();
            w.field_str("host", &m.host);
            w.field_uint("member", m.id);
            w.field_str("vote", vote);
            w.field_bool("leader", m.id == v.leader);
            w.field_bool("lost", lost.contains(&m.host));
            w.end_object();
        }
        w.end_list();
        w.end_object();
    }))
}

fn networks(state: &ModState, p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    finish(p)?;
    let v = ops::member_view(state)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.key("networks");
        w.begin_list();
        for n in v.state.networks() {
            let ports = v.state.ports().filter(|p| p.vni == n.vni).count() as u64;
            w.begin_object();
            w.field_str("name", &n.name);
            w.field_uint("vni", u64::from(n.vni));
            w.field_str("subnet", &alloc::format!("{}/{}", Ip(n.net), n.prefix));
            w.field_str("gateway", &alloc::format!("{}", Ip(n.gateway())));
            w.field_uint("mtu", u64::from(n.mtu));
            w.field_bool("out", n.out);
            w.field_uint("ports", ports);
            w.field_uint("free", usable(n).saturating_sub(ports));
            w.end_object();
        }
        w.end_list();
        w.end_object();
    }))
}

fn ports(state: &ModState, p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    finish(p)?;
    let v = ops::member_view(state)?;
    let hosts = registry::hosts().unwrap_or_default();
    Ok(answer(call, |w| {
        w.begin_object();
        w.key("ports");
        w.begin_list();
        for x in v.state.ports() {
            w.begin_object();
            w.field_uint("id", x.id);
            w.field_str("network", v.state.network_of(x.vni).map_or("", |n| n.name.as_str()));
            w.field_str("address", &alloc::format!("{}", Ip(x.address)));
            w.field_str("host", &registry::name_of(&hosts, x.node));
            w.field_uint("generation", x.generation);
            w.field_str("firewall", v.state.firewall_of(x.firewall).map_or("", |f| f.name.as_str()));
            w.field_str("vm", &ops::uuid_text(&x.uuid));
            w.field_uint("nic", u64::from(x.nic));
            w.key("public");
            match v.state.public_on(x.id) {
                Some(a) => w.str(&alloc::format!("{}", Ip(a.address))),
                None => w.null(),
            }
            w.end_object();
        }
        w.end_list();
        w.end_object();
    }))
}

fn public(state: &ModState, p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    finish(p)?;
    let v = ops::member_view(state)?;
    let hosts = registry::hosts().unwrap_or_default();
    Ok(answer(call, |w| {
        w.begin_object();
        w.key("public");
        w.begin_list();
        for x in v.state.public() {
            w.begin_object();
            w.field_str("address", &alloc::format!("{}", Ip(x.address)));
            w.field_str("host", &registry::name_of(&hosts, x.gateway));
            w.key("port");
            if x.port == 0 {
                w.null();
            } else {
                w.uint(x.port);
            }
            w.end_object();
        }
        w.end_list();
        w.end_object();
    }))
}

fn firewalls(state: &ModState, p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    finish(p)?;
    let v = ops::member_view(state)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.key("firewalls");
        w.begin_list();
        for f in v.state.firewalls() {
            w.begin_object();
            w.field_str("name", &f.name);
            w.field_str("in", f.inbound.name());
            w.field_str("out", f.outbound.name());
            w.field_uint("rules", f.rules.len() as u64);
            w.field_uint("revision", f.revision);
            w.field_uint("ports", v.state.ports().filter(|x| x.firewall == f.id).count() as u64);
            w.end_object();
        }
        w.end_list();
        w.end_object();
    }))
}

fn firewall(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let name = text(&mut p, "name", NAME_MAX)?;
    finish(p)?;
    let v = ops::member_view(state)?;
    let f = v.state.firewall(name).ok_or_else(|| Refused::NotFound(alloc::format!("no firewall {}", name)))?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_str("name", &f.name);
        w.field_str("in", f.inbound.name());
        w.field_str("out", f.outbound.name());
        w.field_uint("revision", f.revision);
        w.key("rules");
        w.begin_list();
        for (j, r) in f.rules.iter().enumerate() {
            w.begin_object();
            w.field_uint("n", j as u64 + 1);
            w.field_str("rule", &alloc::format!("{}", r));
            w.end_object();
        }
        w.end_list();
        w.end_object();
    }))
}

fn alerts(state: &ModState, p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    finish(p)?;
    let list = ops::alerts(state);
    Ok(answer(call, |w| {
        w.begin_object();
        w.key("alerts");
        w.begin_list();
        for a in &list {
            w.begin_object();
            w.field_str("kind", a.kind);
            w.field_str("says", &a.says);
            w.end_object();
        }
        w.end_list();
        w.end_object();
    }))
}

/* ---- the members ---- */

fn host<'a>(p: &mut Params<'a>) -> Result<&'a str, Refused> {
    let h = text(p, "host", NAME_MAX)?;
    if !members::valid_name(h) {
        return Err(Refused::invalid(String::from("a host's name: lowercase letters, digits and -"), "host"));
    }
    Ok(h)
}

fn init(state: &Arc<ModState>, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let name = text(&mut p, "name", NAME_MAX)?;
    finish(p)?;
    let m = offered::init(state, name, NO_MEMBER).map_err(|why| {
        if why.contains("is not a vnet's name") {
            Refused::invalid(why, "name")
        } else if why.starts_with("this host is a member") {
            Refused::Conflict(why)
        } else {
            Refused::Failed(why)
        }
    })?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_str("vnet", name);
        w.field_str("id", &codec::hex(&m.group));
        w.field_str("host", &m.host);
        w.field_uint("member", m.member);
        w.end_object();
    }))
}

/// `host_add` or `host_replace`: the host offered the vnet for as long as
/// it takes to answer, or the job is told to stop.
fn host_job(state: &ModState, mut p: Params<'_>, call: &mut Call, replace: bool) -> Result<Status, Refused> {
    let host = host(&mut p)?;
    finish(p)?;
    call.note(&alloc::format!("vnet: {} offered the vnet", host));
    let got = {
        let stop = || call.cancelled();
        if replace {
            state.group.replace(host, &stop, NO_MEMBER).map(|(old, new)| (Some(old), new))
        } else {
            state.group.add(host, &stop, NO_MEMBER).map(|id| (None, id))
        }
    };
    let (old, id) = got.map_err(Refused::from)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_str("host", host);
        if let Some(old) = old {
            w.field_uint("removed", old);
        }
        w.field_uint("member", id);
        w.end_object();
    }))
}

fn host_remove(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let host = host(&mut p)?;
    finish(p)?;
    let id = state.group.remove(host, NO_MEMBER).map_err(Refused::from)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_str("host", host);
        w.field_uint("member", id);
        w.end_object();
    }))
}

/* ---- networks, ports, public addresses ---- */

fn network_create(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let name = text(&mut p, "name", NAME_MAX)?;
    let subnet = text(&mut p, "subnet", SUBNET_MAX)?;
    let mtu = p.opt_uint("mtu", u64::from(MTU_MIN), u64::from(MTU_MAX))
        .map_err(|e| Refused::invalid(String::from(e.why), e.field))?.unwrap_or(u64::from(MTU_DEFAULT));
    let out = p.opt_bool("out").map_err(|e| Refused::invalid(String::from(e.why), e.field))?.unwrap_or(true);
    finish(p)?;
    let (net, prefix) = ops::parse_subnet(subnet)
        .ok_or_else(|| Refused::invalid(String::from("is a subnet: a.b.c.d/prefix"), "subnet"))?;
    let vni = ops::network_create(state, name, net, prefix, mtu as u16, out)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_str("name", name);
        w.field_uint("vni", u64::from(vni));
        w.end_object();
    }))
}

fn network_delete(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let name = text(&mut p, "name", NAME_MAX)?;
    finish(p)?;
    let vni = ops::network_delete(state, name)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_str("name", name);
        w.field_bool("deleted", vni != 0);
        w.end_object();
    }))
}

fn port_move(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let id = number(&mut p, "port", 1, u64::MAX)?;
    let to = host(&mut p)?;
    finish(p)?;
    let g = ops::port_move(state, id, to)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_uint("port", id);
        w.field_str("host", to);
        w.field_uint("generation", g);
        w.end_object();
    }))
}

fn port_delete(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let id = number(&mut p, "port", 1, u64::MAX)?;
    finish(p)?;
    let public = ops::port_delete(state, id)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_uint("port", id);
        w.key("public");
        if public == 0 {
            w.null();
        } else {
            w.str(&alloc::format!("{}", Ip(public)));
        }
        w.end_object();
    }))
}

fn port_firewall(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let id = number(&mut p, "port", 1, u64::MAX)?;
    let fw = text(&mut p, "firewall", firewall::NAME_MAX)?;
    finish(p)?;
    ops::port_firewall(state, id, fw)?;
    Ok(said(call, "firewall", fw))
}

fn public_add(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let a = address(&mut p, "address")?;
    let to = host(&mut p)?;
    finish(p)?;
    ops::public_add(state, a, to)?;
    Ok(said(call, "address", &alloc::format!("{}", Ip(a))))
}

fn public_set(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let a = address(&mut p, "address")?;
    let port = p.opt_uint("port", 1, u64::MAX).map_err(|e| Refused::invalid(String::from(e.why), e.field))?
        .unwrap_or(0);
    finish(p)?;
    ops::public_set(state, a, port)?;
    Ok(said(call, "address", &alloc::format!("{}", Ip(a))))
}

fn public_move(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let a = address(&mut p, "address")?;
    let to = host(&mut p)?;
    finish(p)?;
    ops::public_move(state, a, to)?;
    Ok(said(call, "address", &alloc::format!("{}", Ip(a))))
}

fn public_remove(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let a = address(&mut p, "address")?;
    finish(p)?;
    let removed = ops::public_remove(state, a)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_str("address", &alloc::format!("{}", Ip(a)));
        w.field_bool("removed", removed);
        w.end_object();
    }))
}

/* ---- the cluster's firewalls ---- */

fn fw_name<'a>(p: &mut Params<'a>) -> Result<&'a str, Refused> {
    let name = text(p, "name", firewall::NAME_MAX)?;
    if !firewall::valid_name(name) {
        return Err(Refused::invalid(String::from("is a firewall's name: a letter or a digit, then letters, digits, \
                                                  . _ and -"), "name"));
    }
    Ok(name)
}

fn firewall_create(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let name = fw_name(&mut p)?;
    let set = policies(&mut p)?;
    finish(p)?;
    ops::firewall_create(state, name, set)?;
    Ok(said(call, "name", name))
}

fn firewall_delete(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let name = fw_name(&mut p)?;
    finish(p)?;
    let deleted = ops::firewall_delete(state, name)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_str("name", name);
        w.field_bool("deleted", deleted);
        w.end_object();
    }))
}

/// A change of a firewall's rules: made, and what was done said.
fn firewall_change(state: &ModState, call: &mut Call, name: &str, e: Edit) -> Result<Status, Refused> {
    let done = ops::firewall_edit(state, name, &e)?;
    Ok(said(call, "done", &done))
}

fn firewall_policy(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let name = fw_name(&mut p)?;
    let set = policies(&mut p)?;
    finish(p)?;
    if set.is_empty() {
        return Err(Refused::invalid(String::from("in or out, or both, is given"), "in"));
    }
    firewall_change(state, call, name, Edit::Policy(set))
}

fn firewall_add(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let name = fw_name(&mut p)?;
    let text_of = text(&mut p, "rule", RULE_MAX)?;
    let at = p.opt_uint("at", 1, RULES_MAX as u64).map_err(|e| Refused::invalid(String::from(e.why), e.field))?;
    finish(p)?;
    let rule = Rule::parse(text_of).map_err(|why| Refused::invalid(alloc::format!("\"{}\": {}", text_of, why), "rule"))?;
    firewall_change(state, call, name, Edit::Add { rule, at: at.map(|n| n as usize - 1) })
}

fn firewall_remove(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let name = fw_name(&mut p)?;
    let n = number(&mut p, "n", 1, RULES_MAX as u64)?;
    finish(p)?;
    firewall_change(state, call, name, Edit::Del(n as usize))
}

fn firewall_move(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let name = fw_name(&mut p)?;
    let from = number(&mut p, "n", 1, RULES_MAX as u64)?;
    let to = number(&mut p, "to", 1, RULES_MAX as u64)?;
    finish(p)?;
    firewall_change(state, call, name, Edit::Move { from: from as usize, to: to as usize })
}

/* ---- hv's ---- */

/// A firewall of the cluster's as hv puts it in its switch: its policies,
/// the revision of its rules, and each rule's text.
fn firewall_value(w: &mut Writer, f: &vnetmeta::Firewall) {
    w.begin_object();
    w.field_str("name", &f.name);
    w.field_uint("revision", f.revision);
    w.field_str("in", f.inbound.name());
    w.field_str("out", f.outbound.name());
    w.key("rules");
    w.begin_list();
    for r in f.rules.iter() {
        w.str(&alloc::format!("{}", r));
    }
    w.end_list();
    w.end_object();
}

fn attach(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let uuid = ops::parse_uuid(text(&mut p, "uuid", UUID_MAX)?)?;
    let nic = p.opt_uint("nic", 0, NIC_MAX).map_err(|e| Refused::invalid(String::from(e.why), e.field))?.unwrap_or(0);
    let network = text(&mut p, "network", NAME_MAX)?;
    let at = match opt_text(&mut p, "address", IP_MAX)? {
        Some(t) => ops::parse_ip(t, "address")?,
        None => 0,
    };
    let fw = opt_text(&mut p, "firewall", NAME_MAX)?.unwrap_or("");
    let make = p.opt_bool("make").map_err(|e| Refused::invalid(String::from(e.why), e.field))?.unwrap_or(true);
    finish(p)?;
    let a = ops::attach(state, uuid, nic as u8, network, at, fw, make)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_uint("port", a.port.id);
        w.field_str("network", &a.network.name);
        w.field_uint("vni", u64::from(a.network.vni));
        w.field_str("address", &alloc::format!("{}", Ip(a.port.address)));
        w.field_str("subnet", &alloc::format!("{}/{}", Ip(a.network.net), a.network.prefix));
        w.field_str("gateway", &alloc::format!("{}", Ip(a.network.gateway())));
        w.field_uint("mtu", u64::from(a.network.mtu));
        w.field_bool("out", a.network.out);
        w.field_uint("generation", a.port.generation);
        w.key("public");
        match a.public {
            Some(x) => w.str(&alloc::format!("{}", Ip(x))),
            None => w.null(),
        }
        w.key("firewall");
        match &a.firewall {
            Some(f) => firewall_value(w, f),
            None => w.null(),
        }
        w.field_bool("confirmed", a.confirmed);
        w.field_bool("made", a.made);
        w.end_object();
    }))
}

fn detach(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let uuid = ops::parse_uuid(text(&mut p, "uuid", UUID_MAX)?)?;
    let nic = p.opt_uint("nic", 0, NIC_MAX).map_err(|e| Refused::invalid(String::from(e.why), e.field))?.unwrap_or(0);
    finish(p)?;
    let port = ops::detach(state, uuid, nic as u8)?;
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_uint("port", port);
        w.end_object();
    }))
}

/// What the tunnel takes of the registry's hosts, as one number: their
/// nodes, names, addresses and ports -- what a table's `hosts_digest` says,
/// and hv gives back, so that a host's address changed in the registry is a
/// table changed though vnet's own state is not.
fn hosts_digest(hosts: &[registry::Host]) -> u64 {
    let mut crc = 0u32;
    for h in hosts {
        crc = crc::crc32c::update(crc, &h.node.to_le_bytes());
        crc = crc::crc32c::update(crc, h.name.as_bytes());
        crc = crc::crc32c::update(crc, &[0]);
        crc = crc::crc32c::update(crc, &h.underlay.to_le_bytes());
        crc = crc::crc32c::update(crc, &h.port.to_le_bytes());
    }
    (hosts.len() as u64) << 32 | u64::from(crc)
}

/// What hv keeps of the state: the networks, this host's ports and the
/// firewalls they are judged by, and the hosts the tunnel reaches -- or,
/// where the caller has this instance's table at this revision already, of
/// these hosts, that it is the same.
fn table(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Result<Status, Refused> {
    let instance = p.opt_uint("instance", 0, u64::MAX).map_err(|e| Refused::invalid(String::from(e.why), e.field))?;
    let revision = p.opt_uint("revision", 0, u64::MAX).map_err(|e| Refused::invalid(String::from(e.why), e.field))?;
    let digest = p.opt_uint("hosts_digest", 0, u64::MAX).map_err(|e| Refused::invalid(String::from(e.why), e.field))?;
    finish(p)?;
    let v = ops::member_view(state)?;
    let rev = v.state.revision();
    let hosts = registry::hosts()?;
    let hosts_now = hosts_digest(&hosts);
    if instance == Some(state.instance) && revision == Some(rev) && digest == Some(hosts_now) {
        return Ok(answer(call, |w| {
            w.begin_object();
            w.field_uint("instance", state.instance);
            w.field_uint("revision", rev);
            w.field_uint("hosts_digest", hosts_now);
            w.field_bool("same", true);
            w.end_object();
        }));
    }
    let me = raftmember::ipcnet::identity()?.name;
    let node = registry::node_of(&hosts, &me)?;
    let here: Vec<&vnetmeta::Port> = v.state.ports().filter(|x| x.node == node).collect();
    let mut used: Vec<u32> = here.iter().map(|x| x.firewall).collect();
    used.sort_unstable();
    used.dedup();
    Ok(answer(call, |w| {
        w.begin_object();
        w.field_uint("instance", state.instance);
        w.field_uint("revision", rev);
        w.field_uint("hosts_digest", hosts_now);
        w.field_bool("same", false);
        w.field_uint("node", node);
        /* The hosts, by node, and where the tunnel reaches each: an
         * address, and a port of its own or 0 for the tunnel's */
        w.key("hosts");
        w.begin_list();
        for h in hosts.iter() {
            w.begin_object();
            w.field_uint("node", h.node);
            w.field_str("name", &h.name);
            w.field_str("underlay", &alloc::format!("{}", Ip(h.underlay)));
            w.field_uint("port", u64::from(h.port));
            w.end_object();
        }
        w.end_list();
        /* Every port on another host, as the tunnel needs it: its network,
         * its address, its host -- four numbers each */
        w.key("remote");
        w.begin_list();
        for x in v.state.ports().filter(|x| x.node != node) {
            w.begin_list();
            w.uint(u64::from(x.vni));
            w.uint(u64::from(x.address));
            w.uint(x.node);
            w.uint(x.generation);
            w.end_list();
        }
        w.end_list();
        w.key("networks");
        w.begin_list();
        for n in v.state.networks() {
            w.begin_object();
            w.field_str("name", &n.name);
            w.field_uint("vni", u64::from(n.vni));
            w.field_str("subnet", &alloc::format!("{}/{}", Ip(n.net), n.prefix));
            w.field_uint("mtu", u64::from(n.mtu));
            w.field_bool("out", n.out);
            w.end_object();
        }
        w.end_list();
        /* The public addresses on ports, wherever the port is and wherever
         * the address is routed: the address, the host it is routed to, and
         * the port's network, address and host -- five numbers each */
        w.key("public");
        w.begin_list();
        for x in v.state.public().filter(|x| x.port != 0) {
            let Some(port) = v.state.port(x.port) else { continue };
            w.begin_list();
            w.uint(u64::from(x.address));
            w.uint(x.gateway);
            w.uint(u64::from(port.vni));
            w.uint(u64::from(port.address));
            w.uint(port.node);
            w.end_list();
        }
        w.end_list();
        w.key("ports");
        w.begin_list();
        for x in here.iter() {
            w.begin_object();
            w.field_uint("id", x.id);
            w.field_str("vm", &ops::uuid_text(&x.uuid));
            w.field_uint("nic", u64::from(x.nic));
            w.field_uint("vni", u64::from(x.vni));
            w.field_str("address", &alloc::format!("{}", Ip(x.address)));
            w.field_uint("generation", x.generation);
            w.field_str("firewall", v.state.firewall_of(x.firewall).map_or("", |f| f.name.as_str()));
            w.end_object();
        }
        w.end_list();
        w.key("firewalls");
        w.begin_list();
        for f in used.iter().filter_map(|&id| v.state.firewall_of(id)) {
            firewall_value(w, f);
        }
        w.end_list();
        w.end_object();
    }))
}
