//! The `nbs` provider on the management bus: NBS's metadata as the host
//! API, the panel and `mgmt` reach it -- through the same functions the
//! shell's commands are (`store.rs`), so that what a command does a call does.
//!
//! - reads: `status` (this host's member, and its digest), `hosts` (the
//!   members), `cs` (each host's chunk server, its devices and room),
//!   `volumes`, `volume` (one, and which hosts hold its replicas: where its
//!   VM's reads stay local);
//! - actions: `volume_create`, `volume_delete`, `volume_resize`.
//!
//! Its changes can be asked again with the same result, as an automation that
//! retries needs: a volume made again by its name and size is answered as it
//! is, one deleted that is gone succeeds. Devices are added at a host's own
//! shell (`nbs devices add`), where the device is.

use alloc::sync::Arc;

use kcore::mgmt::{Call, Kind, Method, Provider, Status, Writer};
use mgmtwire::{Invalid, Params, Value};
use nbsmeta::{Refusal, Volume, VOLUME_MIB_MAX, VOLUME_NAME_MAX};
use raft::Role;

use crate::store::{self, Failure};
use crate::{member_view, view, ModState};

const DESCRIPTOR: &str = include_str!("nbs.descriptor.json");

const METHODS: [Method; 8] = [
    Method { name: "status", kind: Kind::Read },
    Method { name: "hosts", kind: Kind::Read },
    Method { name: "cs", kind: Kind::Read },
    Method { name: "volumes", kind: Kind::Read },
    Method { name: "volume", kind: Kind::Read },
    Method { name: "volume_create", kind: Kind::Action },
    Method { name: "volume_delete", kind: Kind::Action },
    Method { name: "volume_resize", kind: Kind::Action },
];

const HOST_MAX: usize = 63;

/// The provider registered, its calls answered from `state` -- until it is
/// dropped, which waits out every call of it.
pub fn register(state: Arc<ModState>) -> Result<Provider, Status> {
    Provider::register("nbs", &METHODS, DESCRIPTOR, move |method, params, call| provide(&state, method, params, call))
}

fn refuse(call: &mut Call, status: Status, message: &str, field: Option<&str>) -> Status {
    call.json(|w| status.write_error(w, message, field));
    status
}

fn invalid(call: &mut Call, e: Invalid) -> Status {
    refuse(call, Status::Invalid, e.why, Some(e.field))
}

/// A change not done, said as the call's: the metadata's refusal by what it
/// is, anything else as failed.
fn failed(call: &mut Call, f: Failure) -> Status {
    let status = match &f {
        Failure::Refused(Refusal::NoSuchVolume | Refusal::NoSuchDevice | Refusal::NotAMember) => Status::NotFound,
        Failure::Refused(Refusal::BadVolume | Refusal::Shrink | Refusal::BadDevice) => Status::Invalid,
        Failure::Refused(Refusal::NameTaken | Refusal::Attached | Refusal::AttachedElsewhere
                         | Refusal::ReadersAttached | Refusal::Writing | Refusal::ReadOnly) => Status::Conflict,
        Failure::Other(why) if why.starts_with("this host is in no NBS cluster") => Status::Conflict,
        _ => Status::Failed,
    };
    refuse(call, status, &f.says(), None)
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
        "cs" => cs(state, p, call),
        "volumes" => volumes(state, p, call),
        "volume" => volume(state, p, call),
        "volume_create" => volume_create(state, p, call),
        "volume_delete" => volume_delete(state, p, call),
        "volume_resize" => volume_resize(state, p, call),
        _ => Status::NoMethod,
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Leader => "leader",
        Role::Follower => "follower",
        Role::Candidate | Role::PreCandidate => "candidate",
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
        match v.state.member_of_id(v.leader) {
            Some(m) => w.str(&m.host),
            None => w.null(),
        }
        w.field_uint("applied", v.applied);
        w.field_uint("members", v.state.members().len() as u64);
        w.field_uint("volumes", v.state.volumes().len() as u64);
        w.field_uint("chunks", v.state.live_chunks());
        w.key("digest_at");
        match v.state.checks().latest() {
            Some((at, _)) => w.uint(at),
            None => w.null(),
        }
        w.key("digest");
        match v.state.checks().latest() {
            Some((_, d)) => w.str(&codec::hex(&d)),
            None => w.null(),
        }
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
    let v = match member_view(state) {
        Ok(v) => v,
        Err(why) => return failed(call, Failure::Other(why)),
    };
    answer(call, |w| {
        w.begin_object();
        w.key("hosts");
        w.begin_list();
        for m in v.state.members() {
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
            w.end_object();
        }
        w.end_list();
        w.end_object();
    })
}

fn cs(state: &ModState, p: Params<'_>, call: &mut Call) -> Status {
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    let v = match member_view(state) {
        Ok(v) => v,
        Err(why) => return failed(call, Failure::Other(why)),
    };
    let slot = u64::from(nbsmeta::CHUNK_MIB) << 20;
    answer(call, |w| {
        w.begin_object();
        w.key("cs");
        w.begin_list();
        for c in v.state.cs() {
            w.begin_object();
            w.field_str("host", &c.host);
            w.field_bool("member", v.state.member(&c.host).is_some());
            w.field_uint("size", c.slots() * slot);
            w.field_uint("reserved", c.reserved * slot);
            w.field_uint("free", c.free() * slot);
            w.key("devices");
            w.begin_list();
            for d in &c.devices {
                w.begin_object();
                w.field_uint("device", u64::from(d.id));
                w.field_str("id", &d.ident);
                w.field_uint("size", u64::from(d.slots) * slot);
                w.field_str("state", match d.state {
                    nbsmeta::DeviceState::Active => "active",
                    nbsmeta::DeviceState::Draining => "draining",
                    nbsmeta::DeviceState::Failed => "failed",
                });
                w.end_object();
            }
            w.end_list();
            w.end_object();
        }
        w.end_list();
        w.end_object();
    })
}

fn volume_json(w: &mut Writer, s: &nbsmeta::State, v: &Volume, hosts: bool) {
    w.begin_object();
    w.field_str("name", &v.name);
    w.field_uint("volume", u64::from(v.id));
    w.field_uint("size", v.size_mib << 20);
    w.field_uint("chunks", v.map.len() as u64);
    w.field_str("host", &v.host);
    w.key("writer");
    match &v.writer {
        Some(a) => w.str(&a.host),
        None => w.null(),
    }
    w.field_uint("generation", v.generation);
    w.key("readers");
    w.begin_list();
    for r in &v.readers {
        w.str(r);
    }
    w.end_list();
    if hosts {
        w.key("replicas");
        w.begin_list();
        for (h, n) in store::replica_hosts(s, v) {
            w.begin_object();
            w.field_str("host", &h);
            w.field_uint("chunks", n);
            w.end_object();
        }
        w.end_list();
    }
    w.end_object();
}

fn volumes(state: &ModState, p: Params<'_>, call: &mut Call) -> Status {
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    let v = match member_view(state) {
        Ok(v) => v,
        Err(why) => return failed(call, Failure::Other(why)),
    };
    answer(call, |w| {
        w.begin_object();
        w.key("volumes");
        w.begin_list();
        for vol in v.state.volumes().iter() {
            volume_json(w, &v.state, vol, false);
        }
        w.end_list();
        w.end_object();
    })
}

fn name_param<'a>(p: &mut Params<'a>, call: &mut Call) -> Result<&'a str, Status> {
    let name = p.str("name", VOLUME_NAME_MAX).map_err(|e| invalid(call, e))?;
    if !nbsmeta::valid_volume(name) {
        return Err(refuse(call, Status::Invalid, "lowercase letters, digits, . _ and -, up to 63", Some("name")));
    }
    Ok(name)
}

fn volume(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let name = match name_param(&mut p, call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    let v = match member_view(state) {
        Ok(v) => v,
        Err(why) => return failed(call, Failure::Other(why)),
    };
    let Some(vol) = v.state.volume(name) else {
        return failed(call, Failure::Refused(Refusal::NoSuchVolume));
    };
    answer(call, |w| volume_json(w, &v.state, vol, true))
}

fn volume_create(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let name = match name_param(&mut p, call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let size_mib = match p.uint("size_mib", 1, VOLUME_MIB_MAX) {
        Ok(n) => n,
        Err(e) => return invalid(call, e),
    };
    let host = match p.opt_str("host", HOST_MAX) {
        Ok(h) => h,
        Err(e) => return invalid(call, e),
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match store::volume_create(state, name, size_mib, host) {
        Ok(id) => answer(call, |w| {
            w.begin_object();
            w.field_uint("volume", u64::from(id));
            w.end_object();
        }),
        Err(f) => failed(call, f),
    }
}

fn volume_delete(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let name = match name_param(&mut p, call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match store::volume_delete(state, name) {
        Ok(id) => answer(call, |w| {
            w.begin_object();
            w.field_uint("volume", u64::from(id));
            w.end_object();
        }),
        Err(f) => failed(call, f),
    }
}

fn volume_resize(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let name = match name_param(&mut p, call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let size_mib = match p.uint("size_mib", 1, VOLUME_MIB_MAX) {
        Ok(n) => n,
        Err(e) => return invalid(call, e),
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match store::volume_resize(state, name, size_mib) {
        Ok(id) => answer(call, |w| {
            w.begin_object();
            w.field_uint("volume", u64::from(id));
            w.end_object();
        }),
        Err(f) => failed(call, f),
    }
}
