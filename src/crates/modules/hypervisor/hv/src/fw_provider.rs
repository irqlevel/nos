//! The `fw` provider on the management bus: the host's firewalls
//! (`firewalls.rs`) as the host API, the management panel and `mgmt` reach
//! them -- what `hv fw` does, through the same functions, each refusal the
//! kind of no it is (`Refused`).
//!
//! - reads: `list` (each firewall: its policies, how many rules, the VMs
//!   running on it, what its policies judged; `default` before its first
//!   use, as it will be made), `rules` (every firewall's rules in their
//!   order, each with the flows it judged since its firewall last changed),
//!   `get` (one firewall, its rules among it);
//! - actions: `create`, `delete`, `policy`, `add`, `remove`, `move` -- each
//!   change written to its file, then what its VMs are judged by from the
//!   next frame on, the open flows it denies cut and counted -- and `assign`,
//!   a VM onto a firewall: its vm.toml, and at once where it runs.
//!
//! Its own provider rather than the `vm` provider's methods: the host's
//! firewalls are a page of the panel's of their own, made from this
//! descriptor alone, and the vm descriptor keeps its room (plans/vnet.md).
//! x86-64 alone, where the guests are.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use firewall::{Action, Dir, Rule, NAME_MAX, RULES_MAX};
use kcore::mgmt::{Call, Kind, Method, Provider, Status, Writer};
use mgmtwire::Params;

use crate::firewalls::{Refused, Row};
use crate::net::{Hits, Switch};
use crate::provider::{answer, finish, invalid, refuse, value_of};
use crate::store;
use crate::State;

const DESCRIPTOR: &str = include_str!("fw.descriptor.json");

const METHODS: [Method; 10] = [
    Method { name: "list", kind: Kind::Read },
    Method { name: "rules", kind: Kind::Read },
    Method { name: "get", kind: Kind::Read },
    Method { name: "create", kind: Kind::Action },
    Method { name: "delete", kind: Kind::Action },
    Method { name: "policy", kind: Kind::Action },
    Method { name: "add", kind: Kind::Action },
    Method { name: "remove", kind: Kind::Action },
    Method { name: "move", kind: Kind::Action },
    Method { name: "assign", kind: Kind::Action },
];

/// The longest rule as a person writes it: what a firewall's file holds of
/// one is no longer.
const RULE_MAX: usize = 512;

/// The provider registered, its calls answered from `state` -- until it is
/// dropped, which waits out every call of it.
pub fn register(state: Arc<State>) -> Result<Provider, Status> {
    Provider::register("fw", &METHODS, DESCRIPTOR, move |method, params, call| provide(&state, method, params, call))
}

fn provide(state: &State, method: &str, params: &[u8], call: &mut Call) -> Status {
    let value = match value_of(params, call) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let p = match Params::new(&value) {
        Ok(p) => p,
        Err(e) => return invalid(call, e),
    };
    match method {
        "list" => list(state, p, call),
        "rules" => rules(state, p, call),
        "get" => get(state, p, call),
        "create" => create(state, p, call),
        "delete" => delete(state, p, call),
        "policy" => policy(state, p, call),
        "add" => add(state, p, call),
        "remove" => remove(state, p, call),
        "move" => reorder(state, p, call),
        "assign" => assign(state, p, call),
        _ => Status::NoMethod,
    }
}

/// A refusal as the bus answers it: its kind's status, and the field it is
/// about where the call gave it.
fn refused(call: &mut Call, r: Refused) -> Status {
    match r {
        Refused::Invalid { why, field } => refuse(call, Status::Invalid, &why, Some(field)),
        Refused::NotFound(why) => refuse(call, Status::NotFound, &why, None),
        Refused::Conflict(why) => refuse(call, Status::Conflict, &why, None),
        Refused::Failed(why) => refuse(call, Status::Failed, &why, None),
    }
}

/// The `name` parameter, a firewall's -- or the parameter `key`.
fn name_param<'a>(p: &mut Params<'a>, key: &'a str, call: &mut Call) -> Result<&'a str, Status> {
    let name = p.str(key, NAME_MAX).map_err(|e| invalid(call, e))?;
    if !firewall::valid_name(name) {
        return Err(refuse(call, Status::Invalid, "is a firewall's name: a letter or a digit, then letters, digits, \
                                                  . _ and -", Some(key)));
    }
    Ok(name)
}

/// The policies a call sets: `in` and `out`, each allow or deny, where
/// given.
fn policies_param(p: &mut Params, call: &mut Call) -> Result<Vec<(Dir, Action)>, Status> {
    let mut set = Vec::new();
    for (key, dir) in [("in", Dir::In), ("out", Dir::Out)] {
        match p.opt_str(key, 8) {
            Ok(None) => {}
            Ok(Some(word)) => match Action::parse(word) {
                Some(action) => {
                    set.try_reserve(1).map_err(|_| refuse(call, Status::Busy, "out of memory", None))?;
                    set.push((dir, action));
                }
                None => return Err(refuse(call, Status::Invalid, "is allow or deny", Some(key))),
            },
            Err(e) => return Err(invalid(call, e)),
        }
    }
    Ok(set)
}

/// A rule's number, from 1.
fn number_param<'a>(p: &mut Params<'a>, key: &'a str, call: &mut Call) -> Result<usize, Status> {
    p.uint(key, 1, RULES_MAX as u64).map(|n| n as usize).map_err(|e| invalid(call, e))
}

/// The switch, where it has been made: changes reach the VMs on it.
fn switch(state: &State) -> Option<Arc<Switch>> {
    state.vms.switch_now()
}

/// What a firewall in a row has judged, and the VMs running on it by the
/// names a listing gives them.
fn on_it(state: &State, switch: Option<&Switch>, row: &Row) -> (Hits, Vec<String>) {
    let (Some(place), Some(switch)) = (row.place, switch) else { return (Hits::NONE, Vec::new()) };
    let hits = switch.hits(place).unwrap_or(Hits::NONE);
    let mut names = Vec::new();
    for port in switch.ports_on(place) {
        if let Some(n) = state.vms.named_on_port(port) {
            if names.try_reserve(1).is_err() {
                break;
            }
            names.push(n);
        }
    }
    (hits, names)
}

/// A firewall as `list` says it, and `get` among the rest.
fn firewall_json(w: &mut Writer, row: &Row, hits: &Hits, vms: &[String]) {
    w.begin_object();
    w.field_str("name", &row.name);
    match &row.read {
        Ok(s) => {
            w.field_str("in", s.inbound.name());
            w.field_str("out", s.outbound.name());
            w.field_uint("rules", s.rules.len() as u64);
        }
        Err(e) => {
            w.key("in");
            w.null();
            w.key("out");
            w.null();
            w.key("rules");
            w.null();
            w.field_str("error", e);
        }
    }
    w.key("vms");
    w.begin_list();
    for n in vms {
        w.str(n);
    }
    w.end_list();
    w.field_uint("policy_in_flows", hits.policy_in);
    w.field_uint("policy_out_flows", hits.policy_out);
    w.field_bool("made", row.made);
    w.end_object();
}

fn list(state: &State, p: Params, call: &mut Call) -> Status {
    if let Err(s) = finish(p, call) {
        return s;
    }
    let rows = match state.vms.firewalls.rows(None) {
        Ok(r) => r,
        Err(e) => return refused(call, e),
    };
    let switch = switch(state);
    let on: Vec<(Hits, Vec<String>)> = rows.iter().map(|r| on_it(state, switch.as_deref(), r)).collect();
    answer(call, |w| {
        w.begin_object();
        w.key("firewalls");
        w.begin_list();
        for (row, (hits, vms)) in rows.iter().zip(on.iter()) {
            firewall_json(w, row, hits, vms);
        }
        w.end_list();
        w.end_object();
    })
}

/// A firewall's rules, each with what it judged: `firewall` its name where
/// the rows are of many.
fn rules_json(w: &mut Writer, row: &Row, hits: &Hits, firewall: bool) {
    let Ok(s) = &row.read else { return };
    for (j, text) in s.rules.iter().enumerate() {
        w.begin_object();
        if firewall {
            w.field_str("id", &alloc::format!("{}/{}", row.name, j + 1));
            w.field_str("firewall", &row.name);
        }
        w.field_uint("n", j as u64 + 1);
        w.field_str("rule", text);
        w.field_uint("flows", hits.rules.get(j).copied().unwrap_or(0));
        w.end_object();
    }
}

fn rules(state: &State, p: Params, call: &mut Call) -> Status {
    if let Err(s) = finish(p, call) {
        return s;
    }
    let rows = match state.vms.firewalls.rows(None) {
        Ok(r) => r,
        Err(e) => return refused(call, e),
    };
    let switch = switch(state);
    let hits: Vec<Hits> = rows.iter().map(|r| match (r.place, switch.as_deref()) {
        (Some(place), Some(s)) => s.hits(place).unwrap_or(Hits::NONE),
        _ => Hits::NONE,
    }).collect();
    answer(call, |w| {
        w.begin_object();
        w.key("rules");
        w.begin_list();
        for (row, h) in rows.iter().zip(hits.iter()) {
            rules_json(w, row, h, true);
        }
        w.end_list();
        w.end_object();
    })
}

fn get(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let rows = match state.vms.firewalls.rows(Some(name)) {
        Ok(r) => r,
        Err(e) => return refused(call, e),
    };
    let Some(row) = rows.first() else { return refuse(call, Status::NotFound, "no such firewall", Some("name")) };
    let switch = switch(state);
    let (hits, vms) = on_it(state, switch.as_deref(), row);
    answer(call, |w| {
        w.begin_object();
        w.key("firewall");
        firewall_json(w, row, &hits, &vms);
        w.key("rules");
        w.begin_list();
        rules_json(w, row, &hits, false);
        w.end_list();
        w.end_object();
    })
}

fn create(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match p.str("name", NAME_MAX) {
        Ok(n) => n,
        Err(e) => return invalid(call, e),
    };
    let policies = match policies_param(&mut p, call) {
        Ok(set) => set,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    match state.vms.firewalls.create(name, &policies) {
        Ok((inbound, outbound)) => answer(call, |w| {
            w.begin_object();
            w.field_str("name", name);
            w.field_str("in", inbound.name());
            w.field_str("out", outbound.name());
            w.end_object();
        }),
        Err(e) => refused(call, e),
    }
}

fn delete(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let switch = switch(state);
    match state.vms.firewalls.delete(&state.vms_root, switch.as_deref(), name) {
        Ok(()) => answer(call, |w| {
            w.begin_object();
            w.field_str("deleted", name);
            w.end_object();
        }),
        Err(e) => refused(call, e),
    }
}

/// What a change answers: what it says of itself, and the open flows of the
/// firewall's VMs it cut.
fn changed(call: &mut Call, said: impl FnOnce(&mut Writer), cut: usize) -> Status {
    answer(call, |w| {
        w.begin_object();
        said(w);
        w.field_uint("cut", cut as u64);
        w.end_object();
    })
}

fn policy(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let policies = match policies_param(&mut p, call) {
        Ok(set) => set,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    if policies.is_empty() {
        return refuse(call, Status::Invalid, "is allow or deny: in, out or both are given", Some("in"));
    }
    let switch = switch(state);
    match state.vms.firewalls.policy(switch.as_deref(), name, &policies) {
        Ok(((inbound, outbound), cut)) => changed(call, |w| {
            w.field_str("in", inbound.name());
            w.field_str("out", outbound.name());
        }, cut),
        Err(e) => refused(call, e),
    }
}

fn add(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let (text, at) = match (p.str("rule", RULE_MAX), p.opt_uint("at", 1, RULES_MAX as u64)) {
        (Ok(t), Ok(a)) => (t, a),
        (Err(e), _) | (_, Err(e)) => return invalid(call, e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let rule = match Rule::parse(text) {
        Ok(r) => r,
        Err(why) => return refuse(call, Status::Invalid, why, Some("rule")),
    };
    let switch = switch(state);
    match state.vms.firewalls.add(switch.as_deref(), name, rule, at.map(|n| n as usize - 1)) {
        Ok((place, cut)) => changed(call, |w| {
            w.field_uint("n", place as u64 + 1);
            w.field_str("rule", &alloc::format!("{}", rule));
        }, cut),
        Err(e) => refused(call, e),
    }
}

fn remove(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let n = match number_param(&mut p, "n", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let switch = switch(state);
    match state.vms.firewalls.remove(switch.as_deref(), name, n) {
        Ok((rule, cut)) => changed(call, |w| w.field_str("rule", &alloc::format!("{}", rule)), cut),
        Err(e) => refused(call, e),
    }
}

fn reorder(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let (n, to) = match (number_param(&mut p, "n", call), number_param(&mut p, "to", call)) {
        (Ok(n), Ok(to)) => (n, to),
        (Err(s), _) | (_, Err(s)) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let switch = switch(state);
    match state.vms.firewalls.reorder(switch.as_deref(), name, n, to) {
        Ok(cut) => changed(call, |w| w.field_uint("n", to as u64), cut),
        Err(e) => refused(call, e),
    }
}

fn assign(state: &State, mut p: Params, call: &mut Call) -> Status {
    let vm = match p.str("vm", store::NAME_MAX) {
        Ok(v) if store::valid_name(v) => v,
        Ok(_) => {
            return refuse(call, Status::Invalid, "is a VM's name: a letter or a digit, then letters, digits, . _ and -",
                          Some("vm"));
        }
        Err(e) => return invalid(call, e),
    };
    let name = match name_param(&mut p, "firewall", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    /* Each parameter's own refusal said by its field: the firewall there,
     * the VM running or of a folder. */
    match state.vms.firewalls.rows(Some(name)) {
        Err(Refused::NotFound(why) | Refused::Invalid { why, .. }) => {
            return refuse(call, Status::NotFound, &why, Some("firewall"));
        }
        Err(e) => return refused(call, e),
        Ok(_) => {}
    }
    if state.vms.id_of(vm).is_err() && store::folder_of(&state.vms_root, vm).is_err() {
        return refuse(call, Status::NotFound, &alloc::format!("no vm {}, running or in the VMs' folder", vm),
                      Some("vm"));
    }
    match state.vms.firewalls.assign(&state.vms_root, &state.vms, vm, name) {
        Ok(a) => changed(call, |w| {
            w.field_str("vm", vm);
            w.field_str("firewall", name);
            w.key("id");
            match a.running {
                Some(id) => w.uint(u64::from(id)),
                None => w.null(),
            }
            w.field_bool("live", a.live);
            w.field_bool("kept", a.kept.is_some());
        }, a.cut),
        Err(e) => refused(call, e),
    }
}
