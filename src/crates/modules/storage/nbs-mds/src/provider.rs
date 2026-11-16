//! The `nbs` provider on the management bus: NBS's metadata as the host
//! API, the panel and `mgmt` reach it -- through the same functions the
//! shell's commands are (`store.rs`), so that what a command does a call does.
//!
//! - reads: `status` (this host's member, and its digest), `hosts` (the
//!   members), `cs` (each host's chunk server, its devices and room),
//!   `volumes`, `volume` (one, and which hosts hold its replicas: where its
//!   VM's reads stay local);
//! - actions: `volume_create`, `volume_delete`, `volume_resize`; and
//!   `volume_snapshot` (an attached volume's asked on its writer's host),
//!   `volume_freeze` (an image), `volume_clone` (of an image or a snapshot)
//!   -- N10d;
//! - the cluster itself, as the panel makes and keeps it: `init` (an NBS
//!   cluster of this host alone), `host_add` and `host_replace` (jobs: the
//!   host's own mds offered the cluster, for as long as it takes to answer),
//!   `host_remove`; and the chunk servers' devices -- `devices` (every cs's),
//!   `device_add` (this host's: the device is here), `device_drain` (any
//!   host's, the metadata's change alone).
//!
//! Its changes can be asked again with the same result, as an automation that
//! retries needs: a volume made again by its name and size is answered as it
//! is, one deleted that is gone succeeds.

use alloc::string::String;
use alloc::sync::Arc;

use kcore::mgmt::{Call, Kind as MethodKind, Method, Provider, Status, Writer};
use mgmtwire::{Invalid, Params, Value};
use nbsmeta::{CsState, DeviceState, Kind, Refusal, Volume, REPLICAS, VOLUME_MIB_MAX, VOLUME_NAME_MAX};
use nbsmeta::repair::VolumeHealth;
use raft::Role;

use crate::store::{self, Failure};
use crate::{member_view, view, ModState};

const DESCRIPTOR: &str = include_str!("nbs.descriptor.json");

const METHODS: [Method; 19] = [
    Method { name: "status", kind: MethodKind::Read },
    Method { name: "hosts", kind: MethodKind::Read },
    Method { name: "cs", kind: MethodKind::Read },
    Method { name: "volumes", kind: MethodKind::Read },
    Method { name: "volume", kind: MethodKind::Read },
    Method { name: "volume_create", kind: MethodKind::Action },
    Method { name: "volume_delete", kind: MethodKind::Action },
    Method { name: "volume_resize", kind: MethodKind::Action },
    Method { name: "volume_snapshot", kind: MethodKind::Action },
    Method { name: "volume_freeze", kind: MethodKind::Action },
    Method { name: "volume_clone", kind: MethodKind::Action },
    Method { name: "init", kind: MethodKind::Action },
    Method { name: "host_add", kind: MethodKind::Job },
    Method { name: "host_remove", kind: MethodKind::Action },
    Method { name: "host_replace", kind: MethodKind::Job },
    Method { name: "devices", kind: MethodKind::Read },
    Method { name: "alerts", kind: MethodKind::Read },
    Method { name: "device_add", kind: MethodKind::Action },
    Method { name: "device_drain", kind: MethodKind::Action },
];

/// A device's word: its id (`part:...`, `disk:...`) or its name, at most.
const DEVICE_MAX: usize = nbsmeta::IDENT_MAX;

const HOST_MAX: usize = 63;
/// The volumes `volumes` answers at once, at most, and unless asked fewer:
/// the rest from `next` on. All of them in one answer were past what a
/// caller takes at a few hundred (the review of 2026-11).
const VOLUMES_PAGE: u64 = 256;

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
                         | Refusal::ReadersAttached | Refusal::Writing | Refusal::ReadOnly | Refusal::NotFrozen
                         | Refusal::NotTheWriter) => Status::Conflict,
        Failure::Other(why) if why.starts_with("this host is in no NBS cluster") => Status::Conflict,
        /* A snapshot of a volume attached elsewhere: asked on its writer's
           host, where it can be paused (member.rs's `take`) */
        Failure::Other(why) if why.contains(" is attached on host ") => Status::Conflict,
        _ => Status::Failed,
    };
    refuse(call, status, &f.says(), None)
}

fn answer(call: &mut Call, write: impl FnOnce(&mut Writer)) -> Status {
    if call.json(write) { Status::Ok } else { Status::TooLarge }
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
    match method {
        "status" => status(state, p, call),
        "hosts" => hosts(state, p, call),
        "cs" => cs(state, p, call),
        "volumes" => volumes(state, p, call),
        "alerts" => alerts(state, p, call),
        "volume" => volume(state, p, call),
        "volume_create" => volume_create(state, p, call),
        "volume_delete" => volume_delete(state, p, call),
        "volume_resize" => volume_resize(state, p, call),
        "volume_snapshot" => volume_snapshot(state, p, call),
        "volume_freeze" => volume_freeze(state, p, call),
        "volume_clone" => volume_clone(state, p, call),
        "init" => init(state, p, call),
        "host_add" => host_add(state, p, call),
        "host_remove" => host_remove(state, p, call),
        "host_replace" => host_replace(state, p, call),
        "devices" => devices(state, p, call),
        "device_add" => device_add(state, p, call),
        "device_drain" => device_drain(state, p, call),
        _ => Status::NoMethod,
    }
}

/// A refusal of a change to the cluster itself, said as the call's: this
/// host in it already, or in none, or the host no member -- a conflict; a
/// name not one -- invalid; anything else failed.
fn failed_text(call: &mut Call, why: String, field: Option<&str>) -> Status {
    let status = if why.starts_with("this host is a member of an NBS cluster already")
        || why.starts_with("this host is in no NBS cluster") || why.contains(" is no member")
    {
        Status::Conflict
    } else if why.contains("is not a cluster's name") || why.contains("is not a host's name") {
        Status::Invalid
    } else {
        Status::Failed
    };
    refuse(call, status, &why, field)
}

fn host_param<'a>(p: &mut Params<'a>, call: &mut Call) -> Result<&'a str, Status> {
    let host = p.str("host", HOST_MAX).map_err(|e| invalid(call, e))?;
    if !nbsmeta::valid_name(host) {
        return Err(refuse(call, Status::Invalid, "a host's name: lowercase letters, digits and -", Some("host")));
    }
    Ok(host)
}

fn init(state: &Arc<ModState>, mut p: Params<'_>, call: &mut Call) -> Status {
    let name = match p.str("name", HOST_MAX) {
        Ok(n) => n,
        Err(e) => return invalid(call, e),
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match crate::op_init(state, name) {
        Ok(m) => answer(call, |w| {
            w.begin_object();
            w.field_str("cluster", name);
            w.field_str("id", &codec::hex(&m.cluster));
            w.field_str("host", &m.host);
            w.field_uint("member", m.member);
            w.end_object();
        }),
        Err(why) => failed_text(call, why, Some("name")),
    }
}

/// A job's stop: its provider going, a cancel, the halt.
fn stopped(call: &Call) -> impl Fn() -> bool + '_ {
    move || call.cancelled()
}

fn host_add(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let host = match host_param(&mut p, call) {
        Ok(h) => h,
        Err(s) => return s,
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    call.note(&alloc::format!("nbs: {} offered the NBS cluster", host));
    let got = {
        let stop = stopped(call);
        crate::op_add(state, host, &stop)
    };
    match got {
        Ok(id) => answer(call, |w| {
            w.begin_object();
            w.field_str("host", host);
            w.field_uint("member", id);
            w.end_object();
        }),
        Err(why) => failed_text(call, why, Some("host")),
    }
}

fn host_remove(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let host = match host_param(&mut p, call) {
        Ok(h) => h,
        Err(s) => return s,
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match crate::op_remove(state, host) {
        Ok(id) => answer(call, |w| {
            w.begin_object();
            w.field_str("host", host);
            w.field_uint("member", id);
            w.end_object();
        }),
        Err(why) => failed_text(call, why, Some("host")),
    }
}

fn host_replace(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let host = match host_param(&mut p, call) {
        Ok(h) => h,
        Err(s) => return s,
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    call.note(&alloc::format!("nbs: {}'s old member removed, the host offered the cluster anew", host));
    let got = {
        let stop = stopped(call);
        crate::op_replace(state, host, &stop)
    };
    match got {
        Ok((old, new)) => answer(call, |w| {
            w.begin_object();
            w.field_str("host", host);
            w.field_uint("removed", old);
            w.field_uint("member", new);
            w.end_object();
        }),
        Err(why) => failed_text(call, why, Some("host")),
    }
}

fn devices(state: &ModState, p: Params<'_>, call: &mut Call) -> Status {
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
        w.key("devices");
        w.begin_list();
        for c in v.state.cs() {
            for d in &c.devices {
                w.begin_object();
                w.field_str("host", &c.host);
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
        }
        w.end_list();
        w.end_object();
    })
}

fn device_add(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let device = match p.str("device", DEVICE_MAX) {
        Ok(d) => d,
        Err(e) => return invalid(call, e),
    };
    let wipe = match p.opt_bool("wipe") {
        Ok(w) => w.unwrap_or(false),
        Err(e) => return invalid(call, e),
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match store::device_add(state, device, wipe) {
        Ok((ident, slots, id, wiped)) => answer(call, |w| {
            w.begin_object();
            w.field_str("id", &ident);
            w.field_uint("device", u64::from(id));
            w.field_uint("size", u64::from(slots) * (u64::from(nbsmeta::CHUNK_MIB) << 20));
            w.field_bool("wiped", wiped);
            w.end_object();
        }),
        Err(f) => failed(call, f),
    }
}

fn device_drain(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let device = match p.str("device", DEVICE_MAX) {
        Ok(d) => d,
        Err(e) => return invalid(call, e),
    };
    let host = match p.opt_str("host", HOST_MAX) {
        Ok(h) => h,
        Err(e) => return invalid(call, e),
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    match store::device_drain(state, host, device) {
        Ok(id) => answer(call, |w| {
            w.begin_object();
            w.field_str("id", device);
            w.field_uint("device", u64::from(id));
            w.end_object();
        }),
        Err(f) => failed(call, f),
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
        /* No member: and why, where this host's did not start -- its log
           gone beside its `begun` mark, a member file that cannot be read --
           not taken for a host in no NBS cluster */
        let held = state.group.runner.held();
        return answer(call, |w| {
            w.begin_object();
            w.field_bool("member", false);
            w.key("stopped");
            match &held {
                Some(why) => w.str(why),
                None => w.null(),
            }
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

/// What a person should look at in the NBS cluster: its kind -- the
/// descriptor's word for it, and its tone -- and the host, the detail and
/// the count it is of, where it is of one.
struct Alert {
    kind: &'static str,
    host: Option<String>,
    detail: Option<String>,
    count: Option<u64>,
}

/// The alerts' kinds, the worst first: the order they are said in.
const ALERTS: [&str; 16] = [
    "stopped", "no_leader", "chunks_unreachable", "unlike", "device_failed", "no_room", "chunks_at_one", "cs_down",
    "cs_out", "chunks_at_two", "copying", "cs_full", "maintenance", "device_draining", "no_member", "ok",
];

/// A chunk server whose room is under this tenth free is nearly full.
const FULL_TENTHS: u64 = 1;

/// The most of the registry's answer taken: its hosts.
const HOSTS_ANSWER_MAX: usize = 64 * 1024;

/// The hosts the cluster's registry has in maintenance, asked of its
/// `cluster` provider through the bus: none where there is no registry here,
/// or it does not answer. NBS's controller does not read the flag yet --
/// such a host goes out at `out_after` as any other -- which is what makes
/// a host in maintenance holding replicas worth saying.
fn maintenance_hosts() -> alloc::vec::Vec<String> {
    let mut text = alloc::vec::Vec::new();
    let status = kcore::mgmt::call("cluster", "hosts", b"{}", HOSTS_ANSWER_MAX, &mut |b: &[u8]| {
        text.try_reserve(b.len()).is_ok() && {
            text.extend_from_slice(b);
            true
        }
    });
    if status != Status::Ok {
        return alloc::vec::Vec::new();
    }
    let Ok(answer) = mgmtwire::json::parse(&text, &mgmtwire::Limits::PARAMS) else { return alloc::vec::Vec::new() };
    let mut hosts = alloc::vec::Vec::new();
    for h in answer.get("hosts").and_then(Value::as_list).unwrap_or(&[]) {
        if h.get("maintenance").and_then(Value::as_bool) == Some(true) {
            if let Some(name) = h.get("name").and_then(Value::as_str) {
                hosts.push(String::from(name));
            }
        }
    }
    hosts
}

/// `alerts`: what a person should look at in the NBS cluster as this host's
/// member has it, the worst first -- one saying all is well where nothing
/// is: this member stopped, or no leader (no quorum: nothing changed or
/// repaired, the volumes attached going on); chunks by how short of three
/// replicas they are, and those being copied; replicas the scrub found
/// unlike (on the leader's host); chunk servers down or out, nearly full,
/// fewer than three with room; devices failed or draining; hosts the
/// registry has in maintenance holding replicas.
fn alerts(state: &ModState, p: Params<'_>, call: &mut Call) -> Status {
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    let mut out: alloc::vec::Vec<Alert> = alloc::vec::Vec::new();
    let mut add = |kind: &'static str, host: Option<&str>, detail: Option<String>, count: Option<u64>| {
        out.push(Alert { kind, host: host.map(String::from), detail, count });
    };
    match view(state) {
        None => add("no_member", None, None, None),
        Some(v) => {
            if let Some(why) = &v.stopped {
                add("stopped", None, Some(why.clone()), None);
            } else if v.state.member_of_id(v.leader).is_none() {
                add("no_leader", None, None, None);
            }
            let h = nbsmeta::repair::health(&v.state);
            for (kind, n) in [("chunks_unreachable", h.by_active[0]), ("chunks_at_one", h.by_active[1]),
                              ("chunks_at_two", h.by_active[2]), ("copying", h.joining)] {
                if n != 0 {
                    add(kind, None, None, Some(n));
                }
            }
            let unlike = unlike_now(state, &v.state).len() as u64;
            if unlike != 0 {
                add("unlike", None, None, Some(unlike));
            }
            let mut with_room = 0;
            for c in v.state.cs() {
                match c.state {
                    CsState::Down => add("cs_down", Some(&c.host), None, None),
                    CsState::Out => add("cs_out", Some(&c.host), None, None),
                    CsState::Up => {}
                }
                for d in &c.devices {
                    match d.state {
                        DeviceState::Failed => add("device_failed", Some(&c.host), Some(d.ident.clone()), None),
                        DeviceState::Draining => add("device_draining", Some(&c.host), Some(d.ident.clone()), None),
                        DeviceState::Active => {}
                    }
                }
                let (slots, free) = (c.slots(), c.free());
                if c.state == CsState::Up && free != 0 {
                    with_room += 1;
                }
                if slots != 0 && free * 10 < slots * FULL_TENTHS {
                    add("cs_full", Some(&c.host), Some(alloc::format!("{}% used", 100 - free * 100 / slots)), None);
                }
            }
            if with_room < REPLICAS {
                add("no_room", None, None, Some(with_room as u64));
            }
            for host in maintenance_hosts() {
                if v.state.cs().iter().any(|c| c.host == host && c.reserved != 0) {
                    add("maintenance", Some(&host), None, None);
                }
            }
        }
    }
    if out.is_empty() {
        out.push(Alert { kind: "ok", host: None, detail: None, count: None });
    }
    out.sort_by_key(|a| ALERTS.iter().position(|k| *k == a.kind).unwrap_or(ALERTS.len()));
    answer(call, |w| {
        w.begin_object();
        w.key("alerts");
        w.begin_list();
        for (i, a) in out.iter().enumerate() {
            w.begin_object();
            w.field_uint("id", i as u64);
            w.field_str("kind", a.kind);
            w.key("host");
            match &a.host {
                Some(h) => w.str(h),
                None => w.null(),
            }
            w.key("detail");
            match &a.detail {
                Some(d) => w.str(d),
                None => w.null(),
            }
            w.key("count");
            match a.count {
                Some(n) => w.uint(n),
                None => w.null(),
            }
            w.end_object();
        }
        w.end_list();
        w.end_object();
    })
}

/// A volume's health in a word, the worst of what its chunks say: one with
/// no active replica, or replicas the scrub found unlike -- `failed`; one
/// with a replica alone -- `critical`; one with two, or a copy onto a
/// joiner under way -- `degraded`; else `ok`.
fn health_word(h: &VolumeHealth, unlike: u64) -> &'static str {
    if h.by_active[0] != 0 || unlike != 0 {
        "failed"
    } else if h.by_active[1] != 0 {
        "critical"
    } else if h.by_active[2] != 0 || h.joining != 0 {
        "degraded"
    } else {
        "ok"
    }
}

/// The chunks the leader's scrub found a replica of unlike the others' in a
/// set as it still is -- what `nbs replica bad` has not settled, which takes
/// the odd member out and so moves the set's version on. Known on the
/// leader's host alone, where the scrub runs; its newest findings alone.
fn unlike_now(state: &ModState, s: &nbsmeta::State) -> alloc::vec::Vec<u32> {
    let found: alloc::vec::Vec<(u32, u32, u32)> =
        state.scrubbed.with(|x| x.found.iter().map(|&(chunk, set, version, _)| (chunk, set, version)).collect());
    found.into_iter()
        .filter(|&(_, set, version)| s.sets().get(set as usize).is_some_and(|now| now.version == version))
        .map(|(chunk, _, _)| chunk)
        .collect()
}

fn volume_json(w: &mut Writer, s: &nbsmeta::State, v: &Volume, hosts: bool, unlike: &[u32]) {
    w.begin_object();
    w.field_str("name", &v.name);
    w.field_uint("volume", u64::from(v.id));
    /* How it is: its chunks by their replicas -- and those the scrub found
       unlike, where this host leads */
    let h = nbsmeta::repair::volume_health(s, v);
    let odd = if unlike.is_empty() { 0 } else { v.map.iter().filter(|c| unlike.contains(c)).count() as u64 };
    w.field_str("health", health_word(&h, odd));
    w.field_uint("chunks_at_two", h.by_active[2]);
    w.field_uint("chunks_at_one", h.by_active[1]);
    w.field_uint("chunks_unreachable", h.by_active[0]);
    w.field_uint("chunks_copying", h.joining);
    w.field_uint("chunks_unlike", odd);
    /* What it is (N10d): a volume, an image, a snapshot -- and what it was
       made of, by name while that is there */
    w.field_str("kind", match v.kind {
        Kind::Volume => "volume",
        Kind::Image => "image",
        Kind::Snapshot => "snapshot",
    });
    w.key("origin");
    match s.volumes().iter().find(|o| v.origin != 0 && o.id == v.origin) {
        Some(o) => w.str(&o.name),
        None => w.null(),
    }
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

/// The volumes from number `from` on, `limit` of them at most -- by their
/// numbers, in order -- and `next`, the number to ask from for the rest:
/// null where there are none.
fn volumes(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let from = match p.opt_uint("from", 0, u64::from(u32::MAX)) {
        Ok(f) => f.unwrap_or(0) as u32,
        Err(e) => return invalid(call, e),
    };
    let limit = match p.opt_uint("limit", 1, VOLUMES_PAGE) {
        Ok(l) => l.unwrap_or(VOLUMES_PAGE) as usize,
        Err(e) => return invalid(call, e),
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    let v = match member_view(state) {
        Ok(v) => v,
        Err(why) => return failed(call, Failure::Other(why)),
    };
    let mut vols: alloc::vec::Vec<&Volume> = v.state.volumes().iter().filter(|x| x.id >= from).collect();
    vols.sort_unstable_by_key(|x| x.id);
    let next = vols.get(limit).map(|x| x.id);
    let unlike = unlike_now(state, &v.state);
    answer(call, |w| {
        w.begin_object();
        w.key("volumes");
        w.begin_list();
        for vol in vols.iter().take(limit) {
            volume_json(w, &v.state, vol, false, &unlike);
        }
        w.end_list();
        w.key("next");
        match next {
            Some(n) => w.uint(u64::from(n)),
            None => w.null(),
        }
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
    let unlike = unlike_now(state, &v.state);
    answer(call, |w| volume_json(w, &v.state, vol, true, &unlike))
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

/// A volume's name given under `field`, held to what a name may be.
fn volume_param<'a>(p: &mut Params<'a>, call: &mut Call, field: &'static str) -> Result<&'a str, Status> {
    let name = p.str(field, VOLUME_NAME_MAX).map_err(|e| invalid(call, e))?;
    if !nbsmeta::valid_volume(name) {
        return Err(refuse(call, Status::Invalid, "lowercase letters, digits, . _ and -, up to 63", Some(field)));
    }
    Ok(name)
}

/// The volume made, answered by its number.
fn made(call: &mut Call, got: Result<u32, Failure>) -> Status {
    match got {
        Ok(id) => answer(call, |w| {
            w.begin_object();
            w.field_uint("volume", u64::from(id));
            w.end_object();
        }),
        Err(f) => failed(call, f),
    }
}

/// A snapshot `name` of volume `volume` (N10d): attached, asked on its
/// writer's host -- this call's -- which pauses it; detached, taken as it
/// is. Asked again of the same volume, answered as it is.
fn volume_snapshot(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let volume = match volume_param(&mut p, call, "volume") {
        Ok(n) => n,
        Err(s) => return s,
    };
    let name = match name_param(&mut p, call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    made(call, crate::io::snapshot(state, volume, name))
}

/// Volume `name` made an image (N10d): read-only for good -- refused while
/// attached. Asked again, answered as it is.
fn volume_freeze(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let name = match name_param(&mut p, call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    made(call, store::volume_freeze(state, name))
}

/// A volume `name` cloned from image or snapshot `from` (N10d), its own
/// host `host` -- this one where none is given. Made again of the same
/// origin, answered as it is.
fn volume_clone(state: &ModState, mut p: Params<'_>, call: &mut Call) -> Status {
    let from = match volume_param(&mut p, call, "from") {
        Ok(n) => n,
        Err(s) => return s,
    };
    let name = match name_param(&mut p, call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let host = match p.opt_str("host", HOST_MAX) {
        Ok(h) => h,
        Err(e) => return invalid(call, e),
    };
    if let Err(e) = p.finish() {
        return invalid(call, e);
    }
    made(call, store::volume_clone(state, from, name, host))
}
