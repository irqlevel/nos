//! The `system` provider on the management bus: what the kernel is and how it
//! stands -- its version, its CPUs and how long each has idled, its memory,
//! its tasks, its log, its modules, its interrupts -- and the few things
//! about it the host API may change: the trace level, a task's nice, a module
//! of /lib/modules loaded or unloaded, the power. What it reads is copied out
//! of the C++'s tables (`kcore::system`), each at one moment; what it changes
//! goes through the calls the shell's commands make.
//!
//! Every count says its unit in its name, and none is a fraction: a client
//! makes a CPU's load from two `cpus` answers, the idle time between them
//! over the time between them.

use alloc::vec::Vec;
use kcore::consts::NS_PER_MS;
use kcore::system::{self, ModuleOutcome};
use kcore::trace;
use mgmt::{Call, Kind, Method, Params, Provider, Status};

struct System;

static SYSTEM: System = System;

const DESCRIPTOR: &str = include_str!("system.descriptor.json");

static METHODS: [Method; 13] = [
    Method { name: "info", kind: Kind::Read },
    Method { name: "cpus", kind: Kind::Read },
    Method { name: "memory", kind: Kind::Read },
    Method { name: "tasks", kind: Kind::Read },
    Method { name: "log", kind: Kind::Read },
    Method { name: "modules", kind: Kind::Read },
    Method { name: "interrupts", kind: Kind::Read },
    Method { name: "set_log_level", kind: Kind::Action },
    Method { name: "set_nice", kind: Kind::Action },
    Method { name: "load_module", kind: Kind::Action },
    Method { name: "unload_module", kind: Kind::Action },
    Method { name: "power_off", kind: Kind::Action },
    Method { name: "reboot", kind: Kind::Action },
];

#[cfg(target_arch = "x86_64")]
const ARCH: &str = "x86_64";
#[cfg(target_arch = "aarch64")]
const ARCH: &str = "aarch64";

/// Tasks listed at most: a thousand is more than a machine of 64 CPUs runs.
const MAX_TASKS: usize = 1024;
/// Modules listed at most.
const MAX_MODULES: usize = 64;
/// The log's bytes one answer carries, by default and at most: what the
/// records take, the text and 10 bytes a line, before JSON escapes it.
const LOG_BYTES: u64 = 32 * 1024;
const MAX_LOG_BYTES: u64 = 128 * 1024;
/// What insmod and rmmod say, kept for the answer at most.
const MODULE_OUTPUT_MAX: usize = 4096;
/// A module's name, as /lib/modules has its file: kmod's NAME_LEN less the
/// NUL.
const MODULE_NAME_MAX: usize = 31;
/// A module's parameters, as insmod takes them (ModuleTable::PARAMS_MAX).
const MODULE_PARAMS_MAX: usize = 255;
/// The nice a task may have (SchedEntity::NICE_MIN, NICE_MAX).
const NICE_MIN: i64 = -20;
const NICE_MAX: i64 = 19;
/// The command line, at most, as `info` reports it.
const CMDLINE_MAX: usize = 1024;

/// The provider registered for good: from rust_init, after the bus.
pub fn init() {
    match mgmt::register("system", &METHODS, DESCRIPTOR, &SYSTEM) {
        Ok(registration) => core::mem::forget(registration),
        Err(status) => trace!(0, "system: cannot register the provider: {}", status.name()),
    }
}

impl Provider for System {
    fn call(&self, method: &str, params: &[u8], call: &mut Call) -> Status {
        let value = match mgmt::parse_params(params) {
            Ok(v) => v,
            Err(s) => return call.refuse(s, "the parameters are not JSON as the bus reads it", None),
        };
        let p = match Params::new(&value) {
            Ok(p) => p,
            Err(e) => return call.invalid(e),
        };
        match method {
            "log" => log(p, call),
            "set_log_level" => set_log_level(p, call),
            "set_nice" => set_nice(p, call),
            "load_module" => load_module(p, call),
            "unload_module" => unload_module(p, call),
            _ => no_params(method, p, call),
        }
    }
}

/// The methods that take no parameters: refused if given any.
fn no_params(method: &str, p: Params, call: &mut Call) -> Status {
    if let Err(e) = p.finish() {
        return call.invalid(e);
    }
    match method {
        "info" => info(call),
        "cpus" => cpus(call),
        "memory" => memory(call),
        "tasks" => tasks(call),
        "modules" => modules(call),
        "interrupts" => interrupts(call),
        "power_off" => {
            let status = call.answer(|w| {
                w.begin_object();
                w.end_object();
            });
            trace!(0, "system: power-off asked for through the management bus");
            system::power_off();
            status
        }
        "reboot" => {
            let status = call.answer(|w| {
                w.begin_object();
                w.end_object();
            });
            trace!(0, "system: reboot asked for through the management bus");
            system::reboot();
            status
        }
        _ => Status::NoMethod,
    }
}

/// `p` done with: the call refused if it was handed a parameter it does not
/// take.
fn finish(p: Params, call: &mut Call) -> Result<(), Status> {
    p.finish().map_err(|e| call.invalid(e))
}

fn info(call: &mut Call) -> Status {
    let mut version = [0u8; 64];
    let version_len = system::version(&mut version);
    let mut revision = [0u8; 64];
    let revision_len = system::revision(&mut revision);
    let mut model = [0u8; 80];
    let model_len = system::cpu_model(&mut model);
    let mut cmdline = [0u8; CMDLINE_MAX];
    let cmdline_len = kcore::procinfo::cmdline(&mut cmdline);
    let cpus = kcore::cpu::online_mask().count_ones();
    let uptime_ns = kcore::time::boot_time_ns();
    let wall = kcore::time::wall_clock_secs();
    let level = system::trace_level();

    call.answer(|w| {
        w.begin_object();
        w.field_str_lossy("version", &version[..version_len]);
        w.field_str_lossy("revision", &revision[..revision_len]);
        w.field_str("arch", ARCH);
        w.field_str_lossy("cpu_model", &model[..model_len]);
        w.field_uint("cpus", u64::from(cpus));
        w.field_uint("uptime_ms", uptime_ns / NS_PER_MS);
        w.key("wall_clock_s");
        /* 0 is a clock never set: no time at all, rather than 1970 */
        if wall == 0 {
            w.null();
        } else {
            w.uint(wall);
        }
        w.field_uint("log_level", u64::from(level));
        w.field_str_lossy("cmdline", &cmdline[..cmdline_len]);
        w.end_object();
    })
}

fn cpus(call: &mut Call) -> Status {
    let mask = kcore::cpu::online_mask();
    let uptime_ns = kcore::time::boot_time_ns();
    let mut idle = [0u64; 64];
    for (cpu, ns) in idle.iter_mut().enumerate() {
        if mask & (1 << cpu) != 0 {
            *ns = system::cpu_idle_ns(cpu as u32);
        }
    }
    call.answer(|w| {
        w.begin_object();
        w.field_uint("uptime_ns", uptime_ns);
        w.key("cpus");
        w.begin_list();
        for (cpu, ns) in idle.iter().enumerate().filter(|(cpu, _)| mask & (1 << cpu) != 0) {
            w.begin_object();
            w.field_uint("cpu", cpu as u64);
            w.field_uint("idle_ns", *ns);
            w.end_object();
        }
        w.end_list();
        w.end_object();
    })
}

fn memory(call: &mut Call) -> Status {
    let m = system::memory();
    call.answer(|w| {
        w.begin_object();
        w.field_uint("page_bytes", m.page_bytes);
        w.field_uint("total_bytes", m.total_pages.saturating_mul(m.page_bytes));
        w.field_uint("free_bytes", m.free_pages.saturating_mul(m.page_bytes));
        w.field_uint("huge_block_bytes", m.huge_block_bytes);
        w.field_uint("huge_total_bytes", m.huge_blocks.saturating_mul(m.huge_block_bytes));
        w.field_uint("huge_free_bytes", m.huge_free.saturating_mul(m.huge_block_bytes));
        w.end_object();
    })
}

fn task_state(state: u32) -> &'static str {
    match state {
        system::TASK_WAITING => "waiting",
        system::TASK_RUNNING => "running",
        system::TASK_EXITED => "exited",
        _ => "unknown",
    }
}

fn tasks(call: &mut Call) -> Status {
    let Some(tasks) = system::tasks(MAX_TASKS) else {
        return call.refuse(Status::Busy, "no memory for the list of tasks", None);
    };
    call.answer(|w| {
        w.begin_object();
        w.key("tasks");
        w.begin_list();
        for t in &tasks {
            w.begin_object();
            w.field_uint("pid", t.pid);
            w.field_str_lossy("name", system::name_of(&t.name).as_bytes());
            w.field_str("state", task_state(t.state));
            w.field_bool("idle", t.flags & system::TASK_IDLE != 0);
            w.field_bool("blocked", t.flags & system::TASK_BLOCKED != 0);
            w.field_bool("stopping", t.flags & system::TASK_STOPPING != 0);
            w.field_uint("runtime_ns", t.runtime_ns);
            w.field_uint("switches", t.context_switches);
            w.end_object();
        }
        w.end_list();
        w.field_bool("complete", tasks.len() < MAX_TASKS);
        w.end_object();
    })
}

/// `{"since": N, "max_bytes": M}`: the messages numbered after N -- 0 for
/// the oldest the log has -- as many as M bytes of them hold.
fn log(mut p: Params, call: &mut Call) -> Status {
    let since = match p.opt_uint("since", 0, u64::MAX) {
        Ok(v) => v.unwrap_or(0),
        Err(e) => return call.invalid(e),
    };
    let max = match p.opt_uint("max_bytes", 1, MAX_LOG_BYTES) {
        Ok(v) => v.unwrap_or(LOG_BYTES) as usize,
        Err(e) => return call.invalid(e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }

    let mut buf = Vec::new();
    if buf.try_reserve_exact(max).is_err() {
        return call.refuse(Status::Busy, "no memory for the log", None);
    }
    buf.resize(max, 0);
    /* The number to come read first: what is logged after it is there for
     * the next ask, whatever this one leaves out */
    let next = system::log_next_seq();
    let lost = system::log_lost();
    let (len, last) = system::log_since(since, &mut buf);

    call.answer(|w| {
        w.begin_object();
        w.field_uint("next_seq", next);
        w.field_uint("last_seq", last);
        w.field_uint("lost", lost);
        w.key("lines");
        w.begin_list();
        for (seq, text) in system::records(&buf[..len]) {
            let text = text.strip_suffix(b"\n").unwrap_or(text);
            w.begin_object();
            w.field_uint("seq", seq);
            w.field_str_lossy("text", text);
            w.end_object();
        }
        w.end_list();
        w.field_bool("more", last.saturating_add(1) < next);
        w.end_object();
    })
}

fn module_state(phase: u32) -> &'static str {
    match phase {
        system::MODULE_LIVE => "live",
        system::MODULE_LOADING => "loading",
        system::MODULE_UNLOADING => "unloading",
        _ => "unknown",
    }
}

fn modules(call: &mut Call) -> Status {
    let Some(modules) = system::modules(MAX_MODULES) else {
        return call.refuse(Status::Busy, "no memory for the list of modules", None);
    };
    call.answer(|w| {
        w.begin_object();
        w.key("modules");
        w.begin_list();
        for m in &modules {
            w.begin_object();
            w.field_str_lossy("name", system::name_of(&m.name).as_bytes());
            w.field_uint("size_bytes", m.size);
            w.field_uint("imports", m.imports);
            w.field_str("state", module_state(m.phase));
            w.field_bool("permanent", m.permanent != 0);
            w.end_object();
        }
        w.end_list();
        w.end_object();
    })
}

fn interrupts(call: &mut Call) -> Status {
    let count = kcore::procinfo::interrupt_source_count();
    call.answer(|w| {
        w.begin_object();
        w.key("sources");
        w.begin_list();
        for index in 0..count {
            let mut name = [0u8; 32];
            let Some((len, n)) = kcore::procinfo::interrupt_source(index, &mut name) else { break };
            w.begin_object();
            w.field_str_lossy("name", &name[..len]);
            w.field_uint("count", n.max(0) as u64);
            w.end_object();
        }
        w.end_list();
        w.end_object();
    })
}

/// `{"level": N}`, as `loglevel N` sets it.
fn set_log_level(mut p: Params, call: &mut Call) -> Status {
    let level = match p.uint("level", 0, u64::from(u32::MAX)) {
        Ok(v) => v as u32,
        Err(e) => return call.invalid(e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    if !system::set_trace_level(level) {
        return call.refuse(Status::Invalid, "is past the highest level", Some("level"));
    }
    call.answer(|w| {
        w.begin_object();
        w.field_uint("log_level", u64::from(level));
        w.end_object();
    })
}

/// `{"pid": P, "nice": N}`, as `sched nice P N` sets it.
fn set_nice(mut p: Params, call: &mut Call) -> Status {
    let pid = match p.uint("pid", 0, u64::MAX) {
        Ok(v) => v,
        Err(e) => return call.invalid(e),
    };
    let nice = match p.int("nice", NICE_MIN, NICE_MAX) {
        Ok(v) => v,
        Err(e) => return call.invalid(e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    match system::set_nice(pid, nice) {
        Ok(()) => call.answer(|w| {
            w.begin_object();
            w.end_object();
        }),
        Err(system::NiceError::NoTask) => call.refuse(Status::NotFound, "no task has that pid", Some("pid")),
        Err(system::NiceError::OutOfRange) => call.refuse(Status::Invalid, "is out of its range", Some("nice")),
    }
}

/// A module's name as /lib/modules has its file: lowercase letters, digits,
/// `_` and `-`, up to `MODULE_NAME_MAX`.
fn module_name<'a>(p: &mut Params<'a>, call: &mut Call) -> Result<&'a str, Status> {
    let name = p.str("name", MODULE_NAME_MAX).map_err(|e| call.invalid(e))?;
    if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-') {
        return Err(call.refuse(Status::Invalid, "is lowercase letters, digits, _ and -", Some("name")));
    }
    Ok(name)
}

/// What insmod or rmmod said and how it ended, answered: done or still
/// going, or the call failed with what it said.
fn module_answer(call: &mut Call, outcome: ModuleOutcome, said: &[u8]) -> Status {
    let said = said.strip_suffix(b"\n").unwrap_or(said);
    match outcome {
        ModuleOutcome::Done | ModuleOutcome::Pending => call.answer(|w| {
            w.begin_object();
            w.field_str("outcome", if outcome == ModuleOutcome::Done { "done" } else { "pending" });
            w.field_str_lossy("output", said);
            w.end_object();
        }),
        ModuleOutcome::Failed | ModuleOutcome::NotStarted => {
            let text = match core::str::from_utf8(said) {
                Ok(t) if !t.is_empty() => t,
                _ => if outcome == ModuleOutcome::Failed { "failed" } else { "never started" },
            };
            call.refuse(Status::Failed, text, None)
        }
    }
}

/// What a module command says, kept up to `MODULE_OUTPUT_MAX` -- the C++
/// hands it over once, after the command has said all of it.
fn collect(out: &mut Vec<u8>, b: &[u8]) {
    let room = MODULE_OUTPUT_MAX.saturating_sub(out.len()).min(b.len());
    if out.try_reserve(room).is_ok() {
        out.extend_from_slice(&b[..room]);
    }
}

/// `{"name": "sshd", "params": "..."}`: /lib/modules/sshd.ko loaded, its
/// init handed the parameters -- nothing from anywhere else.
fn load_module(mut p: Params, call: &mut Call) -> Status {
    let name = match module_name(&mut p, call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let params = match p.opt_str("params", MODULE_PARAMS_MAX) {
        Ok(v) => v.unwrap_or(""),
        Err(e) => return call.invalid(e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    if !params.bytes().all(|b| (b' '..=b'~').contains(&b)) {
        return call.refuse(Status::Invalid, "is printable ASCII", Some("params"));
    }

    let mut path = [0u8; 13 + MODULE_NAME_MAX + 3];
    let path = match join(&mut path, &[b"/lib/modules/", name.as_bytes(), b".ko"]) {
        Some(p) => p,
        None => return call.refuse(Status::Invalid, "is too long", Some("name")),
    };
    let mut said = Vec::new();
    trace!(0, "system: insmod {} through the management bus", path);
    let outcome = system::load_module(path, params, &mut |b| collect(&mut said, b));
    module_answer(call, outcome, &said)
}

/// `{"name": "sshd"}`: the module unloaded, as rmmod does it.
fn unload_module(mut p: Params, call: &mut Call) -> Status {
    let name = match module_name(&mut p, call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let mut said = Vec::new();
    trace!(0, "system: rmmod {} through the management bus", name);
    let outcome = system::unload_module(name, &mut |b| collect(&mut said, b));
    module_answer(call, outcome, &said)
}

/// `parts` one after the other in `buf`, if they fit: ASCII in, so a str out.
fn join<'b>(buf: &'b mut [u8], parts: &[&[u8]]) -> Option<&'b str> {
    let mut at = 0;
    for part in parts {
        buf.get_mut(at..at + part.len())?.copy_from_slice(part);
        at += part.len();
    }
    core::str::from_utf8(&buf[..at]).ok()
}

/// Every image provider's read methods that take no parameters, called as
/// a client would at every boot: each must answer, and its answer must be
/// JSON as the bus reads it. From rust_test, once every layer has registered.
pub fn selftest() {
    let mut checked = 0;
    for provider in mgmt::providers().iter().filter(|p| !p.module) {
        for (method, kind) in &provider.methods {
            if *kind != Kind::Read {
                continue;
            }
            let mut answer = Vec::new();
            let status = mgmt::call(&provider.name, method, b"", mgmt::MAX_REPLY, &mut |b| {
                answer.try_reserve(b.len()).is_ok() && {
                    answer.extend_from_slice(b);
                    true
                }
            });
            /* A method that needs a parameter says so, as JSON too */
            if status != Status::Ok && status != Status::Invalid {
                panic!("system selftest: {}.{} answered {}", provider.name, method, status.name());
            }
            let limits = mgmtwire::Limits { bytes: mgmt::MAX_REPLY, depth: 16, values: 1 << 20, members: 1 << 16, string: 64 * 1024 };
            if let Err(e) = mgmtwire::json::parse(&answer, &limits) {
                panic!("system selftest: {}.{} answered what is not JSON: {} at {}", provider.name, method, e.what, e.at);
            }
            checked += 1;
        }
    }
    if checked == 0 {
        panic!("system selftest: no provider of the image's answered");
    }
    log_selftest();
    trace!(0, "system: selftest passed, {} reads of the image's providers answered", checked);
}

/// The log read by number: a line logged now comes back after the number
/// the log had before it, and asking after it brings nothing older.
fn log_selftest() {
    let before = system::log_next_seq();
    trace!(0, "system: selftest's own line {}", before);
    let mut buf = Vec::new();
    if buf.try_reserve_exact(4096).is_err() {
        panic!("system selftest: no memory for the log");
    }
    buf.resize(4096, 0);
    let (len, last) = system::log_since(before.saturating_sub(1), &mut buf);
    const NEEDLE: &[u8] = b"selftest's own line ";
    let found = system::records(&buf[..len]).any(|(seq, text)| {
        seq >= before && text.windows(NEEDLE.len()).any(|w| w == NEEDLE)
    });
    if !found || last < before {
        panic!("system selftest: the log did not give back the line just logged (from {}, last {})", before, last);
    }
    let (len, _) = system::log_since(last, &mut buf);
    if system::records(&buf[..len]).any(|(seq, _)| seq <= last) {
        panic!("system selftest: the log gave back a line at or before the number asked after");
    }
}
