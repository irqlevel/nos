//! The `cluster` provider on the management bus: the registry as the host
//! API, the panel and `mgmt` reach it -- through the same functions the
//! commands are, so that what a command does a call does.
//!
//! - reads: `status` (this host's member), `hosts` (every host, the
//!   invitations open, how many were removed);
//! - actions: `invite` (the token answered, once, and kept nowhere),
//!   `revoke`, `remove`, `maintenance`, `address`, `rekey` (this host's).
//!
//! `join` and `recover` are a host's own business, typed at its shell: a
//! join's token comes from another host, and a recover is unsafe by
//! design.

use alloc::sync::Arc;

use kcore::mgmt::{Call, Kind, Method, Provider, Status, Writer};
use mgmtwire::{Invalid, Params, Value};
use raft::Role;
use registry::Role as AddrRole;

use crate::{files, op_address, op_invite, op_maintenance, op_rekey, op_remove, op_revoke, parse_ip, view,
            ModState, INVITE_SECS};

const DESCRIPTOR: &str = include_str!("cluster.descriptor.json");

const METHODS: [Method; 8] = [
    Method { name: "status", kind: Kind::Read },
    Method { name: "hosts", kind: Kind::Read },
    Method { name: "invite", kind: Kind::Action },
    Method { name: "revoke", kind: Kind::Action },
    Method { name: "remove", kind: Kind::Action },
    Method { name: "maintenance", kind: Kind::Action },
    Method { name: "address", kind: Kind::Action },
    Method { name: "rekey", kind: Kind::Action },
];

/// A host's name, an address and a key in text, at most.
const HOST_MAX: usize = 63;
const IP_MAX: usize = 15;
const KEY_MAX: usize = 64;
const ROLE_MAX: usize = 8;

/// The provider registered, its calls answered from `state` -- until it is
/// dropped, which waits out every call of it.
pub fn register(state: Arc<ModState>) -> Result<Provider, Status> {
    Provider::register("cluster", &METHODS, DESCRIPTOR, move |method, params, call| provide(&state, method, params, call))
}

fn refuse(call: &mut Call, status: Status, message: &str, field: Option<&str>) -> Status {
    call.json(|w| status.write_error(w, message, field));
    status
}

fn invalid(call: &mut Call, e: Invalid) -> Status {
    refuse(call, Status::Invalid, e.why, Some(e.field))
}

/// What a command refused, said as the call's: no member, a host the
/// registry has not, anything else the registry or ipcnet said.
fn failed(call: &mut Call, why: &str) -> Status {
    let status = if why.starts_with("this host is no member") {
        Status::Conflict
    } else if why.starts_with("no host of that name") || why.starts_with("no open invitation") {
        Status::NotFound
    } else {
        Status::Failed
    };
    refuse(call, status, why, None)
}

fn answer(call: &mut Call, write: impl FnOnce(&mut Writer)) -> Status {
    if call.json(write) { Status::Ok } else { Status::TooLarge }
}

fn provide(state: &ModState, method: &str, params: &[u8], call: &mut Call) -> Status {
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
    match method {
        "status" => status(state, p, call),
        "hosts" => hosts(state, p, call),
        "invite" => invite(state, p, call),
        "revoke" => revoke(state, p, call),
        "remove" => remove(state, p, call),
        "maintenance" => maintenance(state, p, call),
        "address" => address(state, p, call),
        "rekey" => rekey(state, p, call),
        _ => Status::NoMethod,
    }
}

/// A host's name, as the registry has names.
fn host_param<'a>(p: &mut Params<'a>, call: &mut Call) -> Result<&'a str, Status> {
    let host = p.str("host", HOST_MAX).map_err(|e| invalid(call, e))?;
    if !registry::valid_name(host) {
        return Err(refuse(call, Status::Invalid, "lowercase letters, digits and -, up to 63", Some("host")));
    }
    Ok(host)
}

fn ip_param(p: &mut Params<'_>, key: &'static str, call: &mut Call) -> Result<Option<u32>, Status> {
    match p.opt_str(key, IP_MAX).map_err(|e| invalid(call, e))? {
        None => Ok(None),
        Some(text) => parse_ip(text).map(Some)
            .ok_or_else(|| refuse(call, Status::Invalid, "not an IPv4 address", Some(key))),
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Leader => "leader",
        Role::Follower => "follower",
        Role::Candidate | Role::PreCandidate => "candidate",
    }
}

/// A port given (#99): None where none is, refused where it is not one.
fn port_param(p: &mut Params<'_>, call: &mut Call) -> Result<u16, Status> {
    match p.opt_uint("port", 1, u64::from(u16::MAX)).map_err(|e| invalid(call, e))? {
        None => Ok(0),
        Some(n) => Ok(n as u16),
    }
}

/// A port, null where none is said: ipcnet's own.
fn port_or_null(w: &mut Writer, port: u16) {
    if port == 0 {
        w.null();
    } else {
        w.uint(u64::from(port));
    }
}

fn ip_or_null(w: &mut Writer, ip: Option<u32>) {
    match ip {
        Some(ip) => w.str(&alloc::format!("{}", crate::Ip(ip))),
        None => w.null(),
    }
}

fn status(state: &ModState, p: Params<'_>, call: &mut Call) -> Status {
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    let Some(v) = view(state) else {
        return answer(call, |w| {
            w.begin_object();
            w.field_bool("member", false);
            w.end_object();
        });
    };
    answer(call, |w| {
        w.begin_object();
        w.field_bool("member", true);
        w.key("cluster");
        match v.state.cluster() {
            Some(c) => w.str(&c.name),
            None => w.null(),
        }
        w.field_uint("node", v.node);
        w.field_str("role", role_name(v.role));
        w.field_uint("term", v.term);
        w.key("leader");
        match v.state.host_of_node(v.leader) {
            Some(h) => w.str(&h.name),
            None => w.null(),
        }
        w.field_uint("applied", v.applied);
        w.field_uint("hosts", v.state.hosts().len() as u64);
        w.field_uint("voters", v.membership.voters.len() as u64);
        w.field_uint("learners", v.membership.learners.len() as u64);
        w.field_uint("refused", v.refused);
        w.key("stopped");
        match &v.stopped {
            Some(why) => w.str(why),
            None => w.null(),
        }
        w.end_object();
    })
}

fn hosts(state: &ModState, p: Params<'_>, call: &mut Call) -> Status {
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    let Some(v) = view(state) else {
        return failed(call, "this host is no member -- `cluster init`, or a join");
    };
    answer(call, |w| {
        w.begin_object();
        w.key("hosts");
        w.begin_list();
        for h in v.state.hosts() {
            let vote = if v.membership.voters.contains(&h.node) {
                "voter"
            } else if v.membership.learners.contains(&h.node) {
                "learner"
            } else {
                "joining"
            };
            let of_role = |role: AddrRole| h.addrs.iter().find(|a| a.role == role).map(|a| a.ip);
            w.begin_object();
            w.field_str("name", &h.name);
            w.field_uint("node", h.node);
            w.field_str("vote", vote);
            w.field_bool("leader", h.node == v.leader);
            w.key("ip");
            ip_or_null(w, Some(h.ipc()));
            w.key("port");
            port_or_null(w, h.ipc_port());
            w.key("underlay");
            ip_or_null(w, of_role(AddrRole::Underlay));
            w.key("public");
            ip_or_null(w, of_role(AddrRole::Public));
            w.field_bool("maintenance", h.maintenance);
            w.field_str("key", &h.keys.first().map(|k| files::hex(k)).unwrap_or_default());
            w.key("moving_to");
            match h.keys.get(1) {
                Some(k) => w.str(&files::hex(k)),
                None => w.null(),
            }
            w.end_object();
        }
        w.end_list();
        w.key("invitations");
        w.begin_list();
        for i in v.state.invitations() {
            w.begin_object();
            w.field_str("host", &i.host);
            w.key("ip");
            ip_or_null(w, Some(i.ip));
            w.key("port");
            port_or_null(w, i.port);
            w.field_uint("expires_s", i.expires);
            w.end_object();
        }
        w.end_list();
        w.field_uint("removed", v.state.removed().len() as u64);
        w.end_object();
    })
}

fn invite(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let host = match host_param(&mut p, call) {
        Ok(h) => h,
        Err(s) => return s,
    };
    let ip = match ip_param(&mut p, "ip", call) {
        Ok(Some(ip)) => ip,
        Ok(None) => return refuse(call, Status::Invalid, "the address the host joins from", Some("ip")),
        Err(s) => return s,
    };
    let port = match port_param(&mut p, call) {
        Ok(port) => port,
        Err(s) => return s,
    };
    let key = match p.str("key", KEY_MAX).map(files::unhex::<32>) {
        Ok(Some(k)) => k,
        Ok(None) => return refuse(call, Status::Invalid, "64 hex digits, as `ipcnet id` says them", Some("key")),
        Err(e) => return invalid(call, e),
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match op_invite(state, host, ip, port, key) {
        Ok(token) => answer(call, |w| {
            w.begin_object();
            w.field_str("host", host);
            w.field_str("token", &token);
            w.field_uint("expires_s", kcore::time::wall_clock_secs() + INVITE_SECS);
            w.end_object();
        }),
        Err(why) => failed(call, &why),
    }
}

fn revoke(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let host = match host_param(&mut p, call) {
        Ok(h) => h,
        Err(s) => return s,
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match op_revoke(state, host) {
        Ok(()) => answer(call, |w| {
            w.begin_object();
            w.field_str("host", host);
            w.end_object();
        }),
        Err(why) => failed(call, &why),
    }
}

fn remove(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let host = match host_param(&mut p, call) {
        Ok(h) => h,
        Err(s) => return s,
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match op_remove(state, host) {
        Ok(node) => answer(call, |w| {
            w.begin_object();
            w.field_str("host", host);
            w.field_uint("node", node);
            w.end_object();
        }),
        Err(why) => failed(call, &why),
    }
}

fn maintenance(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let host = match host_param(&mut p, call) {
        Ok(h) => h,
        Err(s) => return s,
    };
    let on = match p.bool("on") {
        Ok(on) => on,
        Err(e) => return invalid(call, e),
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match op_maintenance(state, host, on) {
        Ok(_) => answer(call, |w| {
            w.begin_object();
            w.field_str("host", host);
            w.field_bool("maintenance", on);
            w.end_object();
        }),
        Err(why) => failed(call, &why),
    }
}

fn address(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let host = match host_param(&mut p, call) {
        Ok(h) => h,
        Err(s) => return s,
    };
    let role = match p.str("role", ROLE_MAX).map(AddrRole::from_name) {
        Ok(Some(r)) => r,
        Ok(None) => return refuse(call, Status::Invalid, "ipc, underlay or public", Some("role")),
        Err(e) => return invalid(call, e),
    };
    let ip = match ip_param(&mut p, "ip", call) {
        Ok(ip) => ip,
        Err(s) => return s,
    };
    let port = match port_param(&mut p, call) {
        Ok(port) => port,
        Err(s) => return s,
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match op_address(state, host, role, ip.unwrap_or(0), port) {
        Ok(_) => answer(call, |w| {
            w.begin_object();
            w.field_str("host", host);
            w.field_str("role", role.name());
            w.key("ip");
            ip_or_null(w, ip);
            w.key("port");
            port_or_null(w, port);
            w.end_object();
        }),
        Err(why) => failed(call, &why),
    }
}

fn rekey(state: &ModState, p: Params<'_>, call: &mut Call) -> Status {
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match op_rekey(state) {
        Ok((old, new)) => answer(call, |w| {
            w.begin_object();
            w.field_str("old", &files::hex(&old));
            w.field_str("new", &files::hex(&new));
            w.end_object();
        }),
        Err(why) => failed(call, &why),
    }
}
