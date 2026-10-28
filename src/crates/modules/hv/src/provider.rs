//! The `vm` provider on the management bus: hv's VMs as the host API, the
//! management panel and `mgmt` reach them -- each a folder of the VMs'
//! folder, known by its name -- and the switch they are on. What it does is
//! what the commands do, through the same functions: a start is
//! `Vms::start_from`, a clone `clone::clone`, a create `cloud::create`; what
//! a command prints, a job keeps as its log, and what a command's caller
//! reads in its lines, a caller here reads in the answer.
//!
//! - reads: `list` (every VM: its folder's, and those on the list without
//!   one), `get`, `host`, `networks`, `config_get`, `console_read` (a page of
//!   the console, its control sequences taken out), `disks`, `forwards`;
//! - actions: `config_set` (a vm.toml checked as a start checks it, the old
//!   one kept as vm.toml.prev), `forward_add` and `forward_del` (kept in its
//!   vm.toml, and made or taken away now if it runs), `console_write`,
//!   `disk_add`, `disk_grow`;
//! - jobs: `start`, `stop`, `restart`, `create` (a cloud's image by its URL:
//!   the user-data of the caller's own if given -- a job, which only an
//!   admin starts -- a port of the switch of its own, nos's ports forwarded,
//!   `protected`, and started once made, as the call asks), `clone`,
//!   `delete`. Each says as it goes what the command
//!   would, its progress and whether it is stopped; a cancel -- or the
//!   module's going -- stops a download, a conversion and a copy where they
//!   are, what they made taken away.
//!
//! A VM whose vm.toml says `protected` is stopped and deleted only when the
//! call says `force`: the management panel's own VM has it. x86-64 alone,
//! where the guests are.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use kcore::mgmt::{Call, Kind, Method, Provider, Status, Writer};
use mgmtwire::{Invalid, Params, Value};

use crate::cloud;
use crate::config::{self, Options};
use crate::forward;
use crate::net;
use crate::store::{self, Folder};
use crate::vms::Listed;
use crate::work::{self, Work};
use crate::State;

const DESCRIPTOR: &str = include_str!("vm.descriptor.json");

const METHODS: [Method; 20] = [
    Method { name: "list", kind: Kind::Read },
    Method { name: "get", kind: Kind::Read },
    Method { name: "host", kind: Kind::Read },
    Method { name: "networks", kind: Kind::Read },
    Method { name: "config_get", kind: Kind::Read },
    Method { name: "console_read", kind: Kind::Read },
    Method { name: "disks", kind: Kind::Read },
    Method { name: "forwards", kind: Kind::Read },
    Method { name: "config_set", kind: Kind::Action },
    Method { name: "forward_add", kind: Kind::Action },
    Method { name: "forward_del", kind: Kind::Action },
    Method { name: "console_write", kind: Kind::Action },
    Method { name: "disk_add", kind: Kind::Action },
    Method { name: "disk_grow", kind: Kind::Action },
    Method { name: "start", kind: Kind::Job },
    Method { name: "stop", kind: Kind::Job },
    Method { name: "restart", kind: Kind::Job },
    Method { name: "create", kind: Kind::Job },
    Method { name: "clone", kind: Kind::Job },
    Method { name: "delete", kind: Kind::Job },
];

/// What a page of the console is, by default and at most.
const CONSOLE_PAGE: u64 = 16 * 1024;
const CONSOLE_PAGE_MAX: u64 = 64 * 1024;
/// What is typed at a console at once, at most: what a VM queues.
const TYPE_MAX: usize = 4096;
/// The longest URL an image is fetched by, and a disk's file's name.
const URL_MAX: usize = 2047;
const FILE_MAX: usize = 64;
/// The longest line of a job's log: what the bus keeps of one.
const LINE_MAX: usize = 240;
const MIB: u64 = 1024 * 1024;

/// The provider registered, its calls answered from `state` -- until it is
/// dropped, which waits out every call and job of it.
pub fn register(state: alloc::sync::Arc<State>) -> Result<Provider, Status> {
    Provider::register("vm", &METHODS, DESCRIPTOR, move |method, params, call| provide(&state, method, params, call))
}

/* ---- answering ---- */

fn refuse(call: &mut Call, status: Status, message: &str, field: Option<&str>) -> Status {
    call.json(|w| status.write_error(w, message, field));
    status
}

fn invalid(call: &mut Call, e: Invalid) -> Status {
    refuse(call, Status::Invalid, e.why, Some(e.field))
}

/// The answer `write` makes: Ok, or TooLarge where the caller took less.
fn answer(call: &mut Call, write: impl FnOnce(&mut Writer)) -> Status {
    if call.json(write) { Status::Ok } else { Status::TooLarge }
}

/// What a job that failed ended with: Cancelled where it was stopped, and
/// Failed with why otherwise.
fn failed(call: &mut Call, why: &str) -> Status {
    if why == work::STOPPED {
        refuse(call, Status::Cancelled, why, None)
    } else {
        refuse(call, Status::Failed, why, None)
    }
}

/// An action's lines, which nobody reads: what it did is its answer.
struct Quiet;

impl Write for Quiet {
    fn write_str(&mut self, _: &str) -> fmt::Result {
        Ok(())
    }
}

/// A job's work: its lines its log on the bus, without the `hv: ` the
/// shell's begin with; its progress the bus's; and a stop asked for by its
/// cancel or by the module's going.
struct Job<'a> {
    call: &'a mut Call,
    unloading: &'a AtomicBool,
    line: String,
}

impl<'a> Job<'a> {
    fn new(call: &'a mut Call, unloading: &'a AtomicBool) -> Job<'a> {
        let mut line = String::new();
        let _ = line.try_reserve_exact(LINE_MAX);
        Job { call, unloading, line }
    }

    /// The line so far, to the log.
    fn flush(&mut self) {
        if !self.line.is_empty() {
            let text = self.line.strip_prefix("hv: ").unwrap_or(&self.line);
            self.call.note(text);
            self.line.clear();
        }
    }

    /// `s` added to the line: what does not fit in its room left out.
    fn push(&mut self, s: &str) {
        let room = LINE_MAX.saturating_sub(self.line.len());
        let mut end = s.len().min(room);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.line.push_str(&s[..end]);
    }
}

impl Write for Job<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for piece in s.split_inclusive('\n') {
            match piece.strip_suffix('\n') {
                Some(text) => {
                    self.push(text);
                    self.flush();
                }
                None => self.push(piece),
            }
        }
        Ok(())
    }
}

impl Work for Job<'_> {
    fn progress(&mut self, done: u64, total: u64) {
        self.call.progress(done, total);
    }

    fn cancelled(&self) -> bool {
        self.call.cancelled() || self.unloading.load(Ordering::Acquire)
    }
}

impl Drop for Job<'_> {
    fn drop(&mut self) {
        self.flush();
    }
}

/* ---- the calls ---- */

fn provide(state: &State, method: &str, params: &[u8], call: &mut Call) -> Status {
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
        "list" => list(state, p, call),
        "get" => get(state, p, call),
        "host" => host(state, p, call),
        "networks" => networks(state, p, call),
        "config_get" => config_get(state, p, call),
        "console_read" => console_read(state, p, call),
        "disks" => disks(state, p, call),
        "forwards" => forwards(state, p, call),
        "config_set" => config_set(state, p, call),
        "forward_add" => forward_add(state, p, call),
        "forward_del" => forward_del(state, p, call),
        "console_write" => console_write(state, p, call),
        "disk_add" => disk_add(state, p, call),
        "disk_grow" => disk_grow(state, p, call),
        "start" => start(state, p, call),
        "stop" => stop(state, p, call),
        "restart" => restart(state, p, call),
        "create" => create(state, p, call),
        "clone" => clone(state, p, call),
        "delete" => delete(state, p, call),
        _ => Status::NoMethod,
    }
}

/// `p` done with: refused if it was handed a parameter it does not take.
fn finish(p: Params, call: &mut Call) -> Result<(), Status> {
    p.finish().map_err(|e| invalid(call, e))
}

/// The `name` parameter: a VM's name, as its folder has it.
fn name_param<'a>(p: &mut Params<'a>, key: &'a str, call: &mut Call) -> Result<&'a str, Status> {
    let name = p.str(key, store::NAME_MAX).map_err(|e| invalid(call, e))?;
    if !store::valid_name(name) {
        return Err(refuse(call, Status::Invalid, "is a letter or a digit, then letters, digits, . _ and -", Some(key)));
    }
    Ok(name)
}

/// The folder of the VM `name`: NotFound where it has none.
fn folder(state: &State, name: &str, call: &mut Call) -> Result<Folder, Status> {
    store::folder_of(&state.vms_root, name).map_err(|e| refuse(call, Status::NotFound, &e, Some("name")))
}

/// The VM on the list `listed` has of the name, if any.
fn on_list<'a>(listed: &'a [Listed], name: &str) -> Option<&'a Listed> {
    listed.iter().find(|l| l.name.as_deref() == Some(name))
}

fn ipv4(w: &mut Writer, key: &str, ip: u32) {
    w.field_str(key, &net::dotted(ip));
}

/// A VM as `list` and `get` say it: its folder's configuration, when it has
/// one, and what of it runs, when it is on the list.
fn vm_json(w: &mut Writer, name: Option<&str>, c: Option<&Result<config::Config, String>>, l: Option<&Listed>,
           doing: Option<&str>) {
    w.begin_object();
    w.key("name");
    match name.or(l.and_then(|l| l.name.as_deref())) {
        Some(n) => w.str(n),
        None => w.null(),
    }
    w.key("id");
    match l {
        Some(l) => w.uint(u64::from(l.id)),
        None => w.null(),
    }
    let state = match (l, doing) {
        (Some(l), _) => l.state,
        (None, Some(_)) => "busy",
        (None, None) => "off",
    };
    w.field_str("state", state);
    if let Some(doing) = doing {
        w.field_str("being", doing);
    }
    let options = c.and_then(|c| c.as_ref().ok());
    w.field_bool("folder", c.is_some());
    w.field_bool("protected", options.is_some_and(|c| c.protected));
    if let Some(Err(e)) = c {
        w.field_str("error", e);
    }
    let o = options.map(|c| &c.options);
    let cpus = match (l, o) {
        (Some(l), _) => l.cpus.len() as u64,
        (None, Some(o)) => u64::from(o.cpus.unwrap_or(1)),
        (None, None) => 0,
    };
    w.field_uint("cpus", cpus);
    let mem = match (l, o) {
        (Some(l), _) => l.mem_mib,
        (None, Some(o)) => o.mem_mib.unwrap_or(if o.kernel == cloud::HVBOOT { 1024 } else { crate::guest::DEFAULT_MEM_MIB }),
        (None, None) => 0,
    };
    w.field_uint("memory_mib", mem);
    w.field_bool("net", l.is_some_and(|l| l.port.is_some()) || o.is_some_and(|o| o.net));
    let port = l.and_then(|l| l.port).or_else(|| o.and_then(|o| o.port).map(|p| p as usize));
    w.key("port");
    match port {
        Some(p) => w.uint(p as u64),
        None => w.null(),
    }
    w.key("ip");
    match port.filter(|&p| p < net::MAX_PORTS) {
        Some(p) => w.str(&net::dotted(net::port_ip(p))),
        None => w.null(),
    }
    w.key("network");
    match o.and_then(|o| o.network.as_deref()) {
        Some(n) => w.str(n),
        None => w.null(),
    }
    w.key("kernel");
    match (o, l) {
        (Some(o), _) => w.str(&o.kernel),
        (None, Some(l)) => w.str(&l.kernel),
        (None, None) => w.null(),
    }
    if let Some(l) = l {
        w.field_uint("uptime_ms", l.uptime_ms);
        w.field_uint("restarts", u64::from(l.restarts));
        w.field_uint("exits", l.exits);
        w.field_uint("console_bytes", l.console_bytes);
        if !l.reason.is_empty() {
            w.field_str("reason", &l.reason);
        }
    }
    w.end_object();
}

/// The VMs' folders under the root, by name, and what each one's vm.toml
/// says -- or why it cannot be read.
fn folders(state: &State) -> Vec<(String, Result<config::Config, String>)> {
    let mut all = Vec::new();
    let Ok(entries) = kcore::fs::list(&state.vms_root) else { return all };
    for entry in entries.iter().filter(|e| e.dir) {
        let Ok(name) = core::str::from_utf8(&entry.name) else { continue };
        let Ok(folder) = store::folder_of(&state.vms_root, name) else { continue };
        if all.try_reserve(1).is_err() {
            break;
        }
        all.push((folder.name.clone(), store::load(&folder)));
    }
    all
}

fn list(state: &State, p: Params, call: &mut Call) -> Status {
    if let Err(s) = finish(p, call) {
        return s;
    }
    let Some(listed) = state.vms.listed() else {
        return refuse(call, Status::Busy, "no memory for the list of vms", None);
    };
    let folders = folders(state);
    answer(call, |w| {
        w.begin_object();
        w.key("vms");
        w.begin_list();
        for (name, c) in &folders {
            vm_json(w, Some(name), Some(c), on_list(&listed, name), state.vms.doing(name));
        }
        /* Being made, with no vm.toml yet; and on the list with no folder,
         * or with one gone meanwhile. */
        for l in &listed {
            if l.name.as_deref().is_none_or(|n| !folders.iter().any(|(f, _)| f == n)) {
                vm_json(w, None, None, Some(l), None);
            }
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
    let folder = match folder(state, name, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    let Some(listed) = state.vms.listed() else {
        return refuse(call, Status::Busy, "no memory for the list of vms", None);
    };
    let c = store::load(&folder);
    let options = c.as_ref().ok().map(|c| store::options(&folder, c));
    let doing = state.vms.doing(name);
    let rows = state.forwards.rows();
    let id = on_list(&listed, name).map(|l| l.id);
    answer(call, |w| {
        w.begin_object();
        w.key("vm");
        vm_json(w, Some(name), Some(&c), on_list(&listed, name), doing);
        if let Ok(conf) = &c {
            w.key("config");
            config_json(w, conf);
        }
        if let Some(Ok(o)) = &options {
            w.key("disks");
            disks_json(w, name, &folder, o);
        }
        w.key("forwards");
        w.begin_list();
        for r in rows.iter().filter(|r| Some(r.vm) == id) {
            forward_json(w, r, Some(name));
        }
        w.end_list();
        w.end_object();
    })
}

/// What a vm.toml says, as the host API shows it.
fn config_json(w: &mut Writer, c: &config::Config) {
    let o = &c.options;
    w.begin_object();
    w.field_str("kernel", &o.kernel);
    w.key("initrd");
    match &o.initrd {
        Some(i) => w.str(i),
        None => w.null(),
    }
    w.key("cmdline");
    match &o.cmdline {
        Some(l) => w.str(l),
        None => w.null(),
    }
    w.key("forward");
    w.begin_list();
    for (host, guest) in &o.forwards {
        w.begin_object();
        w.field_uint("port", u64::from(*host));
        w.field_uint("guest_port", u64::from(*guest));
        w.end_object();
    }
    w.end_list();
    w.field_bool("restart", o.restart);
    w.field_bool("console_log", c.console_log);
    w.key("uuid");
    match &o.uuid {
        Some(u) => w.str(&config::uuid_text(u)),
        None => w.null(),
    }
    w.end_object();
}

/// A VM's disks as its vm.toml names them, and how big each file is now.
fn disks_json(w: &mut Writer, name: &str, folder: &Folder, o: &Options) {
    w.begin_list();
    for (path, ro) in &o.disks {
        w.begin_object();
        let file = path.strip_prefix(folder.path.as_str()).and_then(|r| r.strip_prefix('/')).unwrap_or(path);
        w.field_str("file", file);
        w.field_str("path", path);
        w.key("size_bytes");
        match kcore::fs::size(path) {
            Ok(n) => w.uint(n),
            Err(_) => w.null(),
        }
        w.field_bool("read_only", *ro);
        w.field_str("vm", name);
        w.end_object();
    }
    w.end_list();
}

fn forward_json(w: &mut Writer, r: &forward::Row, name: Option<&str>) {
    w.begin_object();
    w.field_uint("port", u64::from(r.port));
    w.key("vm");
    match name {
        Some(n) => w.str(n),
        None => w.null(),
    }
    w.field_uint("id", u64::from(r.vm));
    ipv4(w, "guest_ip", r.guest_ip);
    w.field_uint("guest_port", u64::from(r.guest_port));
    w.field_uint("active", r.active as u64);
    w.field_uint("accepted", r.accepted);
    w.field_uint("refused", r.refused);
    w.field_uint("failed", r.failed);
    w.field_uint("stalled", r.stalled);
    w.end_object();
}

fn host(state: &State, p: Params, call: &mut Call) -> Status {
    if let Err(s) = finish(p, call) {
        return s;
    }
    let machine = &state.machine;
    let switch = state.vms.switch_now();
    let listed = state.vms.listed().map_or(0, |l| l.len());
    let pool = kcore::frame::huge_pool();
    answer(call, |w| {
        w.begin_object();
        w.key("extension");
        match machine.ext() {
            Ok(ext) => w.str(ext.vendor().name()),
            Err(_) => w.null(),
        }
        w.field_uint("on_cpu_mask", machine.hardware_mask());
        w.field_str("vms_root", &state.vms_root);
        w.field_uint("stop_secs", state.stop_secs);
        w.field_str("cores", if state.cores.own() { "own" } else { "shared" });
        w.field_uint("vms_listed", listed as u64);
        w.field_uint("vms_max", crate::vms::MAX_VMS as u64);
        w.field_uint("huge_pool_bytes", pool.blocks * kcore::frame::HUGE_SIZE);
        w.field_uint("huge_pool_free_bytes", pool.free * kcore::frame::HUGE_SIZE);
        w.key("switch");
        match &switch {
            Some(s) => {
                let (to_host, refused, dhcp, foreign) = s.host_counts();
                w.begin_object();
                ipv4(w, "ip", net::HOST_IP);
                w.key("nat");
                match s.nat_address() {
                    Some(ip) => w.str(&net::dotted(ip)),
                    None => w.null(),
                }
                w.field_uint("frames_to_host", to_host);
                w.field_uint("frames_refused", refused);
                w.field_uint("dhcp_answers", dhcp);
                w.field_uint("frames_foreign", foreign);
                w.end_object();
            }
            None => w.null(),
        }
        /* The host's own address on the switch, what its guests reach nos
         * at -- the host API on hv0 among it -- and the address each of its
         * ports gives its VM, by number: a VM with a port of its own
         * (`port`) has it at every start. The same before the switch is
         * made, as it is at the first VM's start. */
        ipv4(w, "host_ip", net::HOST_IP);
        w.key("port_ips");
        w.begin_list();
        for p in 0..net::MAX_PORTS {
            w.str(&net::dotted(net::port_ip(p)));
        }
        w.end_list();
        w.end_object();
    })
}

fn networks(state: &State, p: Params, call: &mut Call) -> Status {
    if let Err(s) = finish(p, call) {
        return s;
    }
    let Some(listed) = state.vms.listed() else {
        return refuse(call, Status::Busy, "no memory for the list of vms", None);
    };
    let switch = state.vms.switch_now();
    /* Each VM with a port, and the network it is on: None alone. */
    let mut on: Vec<(String, Option<String>)> = Vec::new();
    for l in &listed {
        let Some(port) = l.port else { continue };
        let name = match &l.name {
            Some(n) => n.clone(),
            None => alloc::format!("vm{}", l.id),
        };
        if on.try_reserve(1).is_ok() {
            on.push((name, switch.as_ref().and_then(|s| s.network_of(port))));
        }
    }
    let mut names: Vec<&str> = on.iter().filter_map(|(_, n)| n.as_deref()).collect();
    names.sort_unstable();
    names.dedup();
    answer(call, |w| {
        w.begin_object();
        w.key("networks");
        w.begin_list();
        for n in &names {
            w.begin_object();
            w.field_str("name", n);
            w.key("vms");
            w.begin_list();
            for (vm, _) in on.iter().filter(|(_, net)| net.as_deref() == Some(*n)) {
                w.str(vm);
            }
            w.end_list();
            w.end_object();
        }
        w.end_list();
        w.key("alone");
        w.begin_list();
        for (vm, _) in on.iter().filter(|(_, net)| net.is_none()) {
            w.str(vm);
        }
        w.end_list();
        w.end_object();
    })
}

fn config_get(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let folder = match folder(state, name, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    let file = store::join(&folder.path, store::CONFIG).unwrap_or_default();
    let prev = store::join(&folder.path, store::CONFIG_PREV).is_some_and(|f| kcore::fs::size(&f).is_ok());
    match kcore::fs::read(&file, config::MAX_BYTES) {
        Ok(bytes) => answer(call, |w| {
            w.begin_object();
            w.field_str_lossy("text", &bytes);
            w.field_bool("prev", prev);
            w.end_object();
        }),
        Err(e) => refuse(call, Status::Failed, &alloc::format!("{}: {}", file, e), None),
    }
}

fn console_read(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let from = match p.opt_uint("from", 0, u64::MAX) {
        Ok(f) => f.unwrap_or(0),
        Err(e) => return invalid(call, e),
    };
    let max = match p.opt_uint("max_bytes", 1, CONSOLE_PAGE_MAX) {
        Ok(m) => m.unwrap_or(CONSOLE_PAGE),
        Err(e) => return invalid(call, e),
    };
    let terminal = match p.opt_bool("terminal") {
        Ok(t) => t.unwrap_or(false),
        Err(e) => return invalid(call, e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    match state.vms.console_page(name, from, max as usize, terminal) {
        Ok(page) => answer(call, |w| {
            w.begin_object();
            w.field_uint("from", page.start);
            w.field_uint("next", page.end);
            w.field_uint("total", page.total);
            w.field_str_lossy("text", &page.text);
            w.end_object();
        }),
        Err(e) => refuse(call, Status::NotFound, &e, Some("name")),
    }
}

fn disks(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let folder = match folder(state, name, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    let options = store::load(&folder).and_then(|c| store::options(&folder, &c));
    match options {
        Ok(o) => answer(call, |w| {
            w.begin_object();
            w.key("disks");
            disks_json(w, name, &folder, &o);
            w.end_object();
        }),
        Err(e) => refuse(call, Status::Failed, &e, None),
    }
}

fn forwards(state: &State, p: Params, call: &mut Call) -> Status {
    if let Err(s) = finish(p, call) {
        return s;
    }
    let rows = state.forwards.rows();
    let names: Vec<Option<String>> = rows.iter().map(|r| state.vms.name_of(r.vm)).collect();
    answer(call, |w| {
        w.begin_object();
        w.key("forwards");
        w.begin_list();
        for (r, n) in rows.iter().zip(names.iter()) {
            forward_json(w, r, n.as_deref());
        }
        w.end_list();
        w.end_object();
    })
}

/* ---- actions ---- */

fn config_set(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let text = match p.text("text", config::MAX_BYTES) {
        Ok(t) => t,
        Err(e) => return invalid(call, e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let folder = match folder(state, name, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    /* Checked as a start would check it: a vm.toml that cannot boot is
     * refused, saying why, and the one there kept. */
    let checked = config::parse(text).and_then(|c| store::options_checked(&folder, &c).map(|_| ()));
    if let Err(e) = checked {
        return refuse(call, Status::Invalid, &e, Some("text"));
    }
    let (was, _) = match store::load_text(&folder) {
        Ok(t) => t,
        Err(e) => return refuse(call, Status::Failed, &e, None),
    };
    let changed = was != text;
    if changed {
        if let Err(e) = store::write_config(&folder, &was, text) {
            return refuse(call, Status::Failed, &e, None);
        }
    }
    answer(call, |w| {
        w.begin_object();
        w.field_bool("changed", changed);
        w.end_object();
    })
}

/// A port parameter: 1 to 65535.
fn port_param<'a>(p: &mut Params<'a>, key: &'a str, call: &mut Call) -> Result<u16, Status> {
    p.uint(key, 1, u64::from(u16::MAX)).map(|n| n as u16).map_err(|e| invalid(call, e))
}

fn forward_add(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let (port, guest_port) = match (port_param(&mut p, "port", call), port_param(&mut p, "guest_port", call)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(s), _) | (_, Err(s)) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let folder = match folder(state, name, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    /* nos's port relayed to another VM now is that VM's. */
    let id = state.vms.status_of(name).map(|(id, _)| id);
    if let Some(other) = state.forwards.vm_of(port).filter(|&vm| Some(vm) != id) {
        return refuse(call, Status::Conflict, &alloc::format!("port {} is forwarded to vm {}", port, other), Some("port"));
    }
    let kept = store::update(&folder, |c| {
        c.options.forwards.retain(|&(host, _)| host != port);
        c.options.forwards.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        c.options.forwards.push((port, guest_port));
        Ok(())
    });
    let kept = match kept {
        Ok(k) => k,
        Err(e) => return refuse(call, Status::Invalid, &e, None),
    };
    /* Running: relayed from now on too -- a forward to it on that port
     * taken away first, for its guest port may be another. */
    let mut live = false;
    if let (Some(id), Some(host)) = (id, state.vms.host_nic()) {
        if let Some(ip) = state.vms.address_of(id) {
            if state.forwards.vm_of(port) == Some(id) {
                let _ = state.forwards.del(port);
            }
            match state.forwards.add(port, id, guest_port, ip, host, &mut Quiet) {
                Ok(()) => live = true,
                Err(e) => return refuse(call, Status::Failed, &alloc::format!("kept in its vm.toml, but not relayed now \
                                                                               -- {}", e), None),
            }
        }
    }
    answer(call, |w| {
        w.begin_object();
        w.field_bool("kept", kept);
        w.field_bool("live", live);
        w.end_object();
    })
}

fn forward_del(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let port = match port_param(&mut p, "port", call) {
        Ok(port) => port,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let folder = match folder(state, name, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    let kept = match store::update(&folder, |c| {
        c.options.forwards.retain(|&(host, _)| host != port);
        Ok(())
    }) {
        Ok(k) => k,
        Err(e) => return refuse(call, Status::Failed, &e, None),
    };
    let id = state.vms.status_of(name).map(|(id, _)| id);
    let live = id.is_some() && state.forwards.vm_of(port) == id && state.forwards.del(port).is_ok();
    if !kept && !live {
        return refuse(call, Status::NotFound, &alloc::format!("port {} is not forwarded to {}", port, name),
                      Some("port"));
    }
    answer(call, |w| {
        w.begin_object();
        w.field_bool("kept", kept);
        w.field_bool("live", live);
        w.end_object();
    })
}

fn console_write(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    /* Keys as a terminal sends them: control keys and escape sequences
     * too, which the guest's line editor reads. */
    let text = match p.any("text", TYPE_MAX) {
        Ok(t) => t,
        Err(e) => return invalid(call, e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    match state.vms.type_at(name, text.as_bytes()) {
        Ok(n) => answer(call, |w| {
            w.begin_object();
            w.field_uint("queued", n as u64);
            w.end_object();
        }),
        Err(e) => refuse(call, Status::Conflict, &e, None),
    }
}

/// A disk's file in a VM's folder: a name of letters, digits, `.`, `_` and
/// `-`, none of the folder's own files'.
fn file_param<'a>(p: &mut Params<'a>, call: &mut Call) -> Result<&'a str, Status> {
    let file = p.str("file", FILE_MAX).map_err(|e| invalid(call, e))?;
    let ok = !file.starts_with('.') && file.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        && ![store::CONFIG, store::CONFIG_PREV, cloud::USER_DATA, "console.log", "console.log.1"].contains(&file);
    if !ok {
        return Err(refuse(call, Status::Invalid, "is a file's name in the folder: letters, digits, . _ and -",
                          Some("file")));
    }
    Ok(file)
}

fn disk_add(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let file = match file_param(&mut p, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    let size = match p.uint("size_bytes", MIB, u64::MAX) {
        Ok(s) => s,
        Err(e) => return invalid(call, e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let folder = match folder(state, name, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    let Some(path) = store::join(&folder.path, file) else {
        return refuse(call, Status::Busy, "out of memory", None);
    };
    /* Made new, its size a hole, before its vm.toml names it -- and taken
     * away again if its vm.toml will not. */
    match kcore::fs::create(&path, &[]) {
        Ok(()) => {}
        Err(kcore::error::Error::Busy) => {
            return refuse(call, Status::Conflict, &alloc::format!("{} is there already", path), Some("file"));
        }
        Err(e) => return refuse(call, Status::Failed, &alloc::format!("{} could not be made -- {}", path, e), None),
    }
    let grown = kcore::fs::File::open(&path, true).and_then(|f| f.set_len(size));
    let named = grown.map_err(|e| alloc::format!("{}: {} bytes do not fit there -- {}", path, size, e))
        .and_then(|()| store::update(&folder, |c| {
            c.options.disks.try_reserve(1).map_err(|_| String::from("out of memory"))?;
            c.options.disks.push((String::from(file), false));
            Ok(())
        }));
    if let Err(e) = named {
        let _ = kcore::fs::remove(&path);
        return refuse(call, Status::Failed, &e, None);
    }
    answer(call, |w| {
        w.begin_object();
        w.field_str("path", &path);
        w.field_uint("size_bytes", size);
        w.end_object();
    })
}

fn disk_grow(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let file = match file_param(&mut p, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    let size = match p.uint("size_bytes", 1, u64::MAX) {
        Ok(s) => s,
        Err(e) => return invalid(call, e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    let folder = match folder(state, name, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    if let Some((id, _)) = state.vms.status_of(name) {
        return refuse(call, Status::Conflict, &alloc::format!("vm {} is {} -- its disks are held: stop it first", id,
                                                              name), Some("name"));
    }
    let o = match store::load(&folder).and_then(|c| store::options(&folder, &c)) {
        Ok(o) => o,
        Err(e) => return refuse(call, Status::Failed, &e, None),
    };
    let Some(path) = store::join(&folder.path, file) else {
        return refuse(call, Status::Busy, "out of memory", None);
    };
    match o.disks.iter().find(|(d, _)| *d == path) {
        Some((_, true)) => return refuse(call, Status::Conflict, "is a disk it reads only", Some("file")),
        Some((_, false)) => {}
        None => return refuse(call, Status::NotFound, "is none of its disks", Some("file")),
    }
    match cloud::grow(&path, Some(size)) {
        Ok(n) => answer(call, |w| {
            w.begin_object();
            w.field_uint("size_bytes", n);
            w.end_object();
        }),
        Err(e) => refuse(call, Status::Failed, &e, Some("size_bytes")),
    }
}

/* ---- jobs ---- */

/// Refused when the folder of `name` says it is protected and the call does
/// not say `force`.
fn guarded(state: &State, name: &str, force: bool, call: &mut Call) -> Result<(), Status> {
    let protected = store::folder_of(&state.vms_root, name).ok()
        .and_then(|f| store::load(&f).ok())
        .is_some_and(|c| c.protected);
    if protected && !force {
        return Err(refuse(call, Status::Conflict, &alloc::format!("{} is protected -- force does it all the same",
                                                                  name), Some("force")));
    }
    Ok(())
}

fn start(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    if let Err(e) = hv::run::ensure_runnable(&state.machine) {
        return refuse(call, Status::Unsupported, &alloc::format!("no guest can run here -- {}", e), None);
    }
    let folder = match folder(state, name, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    if let Some((id, _)) = state.vms.status_of(name) {
        return refuse(call, Status::Conflict, &alloc::format!("vm {} is {} already", id, name), Some("name"));
    }
    let started = {
        let mut job = Job::new(call, &state.unloading);
        state.vms.start_from(&state.machine, &state.cores, &state.forwards, folder, &mut job)
    };
    match started {
        Ok(id) => answer(call, |w| {
            w.begin_object();
            w.field_uint("id", u64::from(id));
            w.end_object();
        }),
        Err(e) => failed(call, &e),
    }
}

fn stop(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let secs = match p.opt_uint("secs", 0, crate::vms::WAIT_MAX_S) {
        Ok(s) => s.unwrap_or(crate::vms::STOP_DEFAULT_S),
        Err(e) => return invalid(call, e),
    };
    let force = match p.opt_bool("force") {
        Ok(f) => f.unwrap_or(false),
        Err(e) => return invalid(call, e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    if let Err(s) = guarded(state, name, force, call) {
        return s;
    }
    /* By its name, or a VM with no folder by its number. */
    if state.vms.id_of(name).is_err() {
        return refuse(call, Status::NotFound, &alloc::format!("{} is not on the list -- not started", name),
                      Some("name"));
    }
    let stopped = {
        let job = Job::new(call, &state.unloading);
        state.vms.stop_vm(name, secs, &state.forwards, &|| job.cancelled())
    };
    match stopped {
        Ok(s) => answer(call, |w| {
            w.begin_object();
            w.field_uint("id", u64::from(s.id));
            w.field_str("reason", &s.reason);
            w.field_str("asked", s.asked.name());
            w.field_uint("forwards_taken", s.forwarded as u64);
            w.end_object();
        }),
        Err(e) => failed(call, &e),
    }
}

fn restart(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    if state.vms.id_of(name).is_err() {
        return refuse(call, Status::NotFound, &alloc::format!("{} is not on the list -- start it", name),
                      Some("name"));
    }
    let restarted = {
        let job = Job::new(call, &state.unloading);
        state.vms.restart_vm(name, &|| job.cancelled())
    };
    match restarted {
        Ok(id) => answer(call, |w| {
            w.begin_object();
            w.field_uint("id", u64::from(id));
            w.end_object();
        }),
        Err(e) => failed(call, &e),
    }
}

fn create(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let url = match p.str("url", URL_MAX) {
        Ok(u) if cloud::is_url(u) => u,
        Ok(_) => return refuse(call, Status::Invalid, "is an http:// or https:// URL", Some("url")),
        Err(e) => return invalid(call, e),
    };
    let fields = (|| -> Result<_, Invalid> {
        let size = p.opt_uint("size_bytes", 1, u64::MAX)?;
        let mem = p.opt_uint("memory_mib", 64, u64::MAX)?;
        let cpus = p.opt_uint("cpus", 1, u64::from(u32::MAX))?;
        let network = p.opt_str("network", config::NETWORK_NAME_MAX)?;
        let password = p.opt_str("password", cloud::MAX_PASSWORD)?;
        let user_data = p.opt_text("user_data", cloud::MAX_USER_DATA)?;
        let port = p.opt_uint("port", 0, net::MAX_PORTS as u64 - 1)?;
        let protected = p.opt_bool("protected")?.unwrap_or(false);
        let start = p.opt_bool("start")?.unwrap_or(false);
        Ok((size, mem, cpus, network, password, user_data, port, protected, start))
    })();
    let (size, mem, cpus, network, password, user_data, port, protected, start) = match fields {
        Ok(f) => f,
        Err(e) => return invalid(call, e),
    };
    let forwards = match forwards_param(&mut p, call) {
        Ok(f) => f,
        Err(s) => return s,
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    if network.is_some_and(|n| !config::valid_network(n)) {
        return refuse(call, Status::Invalid, "is a network's name: a letter or a digit, then letters, digits, . _ and -",
                      Some("network"));
    }
    if store::folder_of(&state.vms_root, name).is_ok() {
        return refuse(call, Status::Conflict, &alloc::format!("{} is there already", name), Some("name"));
    }
    /* What would keep it from starting is said before an image is fetched
     * for it: a relay of nos's port another VM has now. */
    if let Some(&(host, _)) = forwards.iter().find(|(host, _)| state.forwards.vm_of(*host).is_some()) {
        return refuse(call, Status::Conflict, &alloc::format!("nos's port {} is forwarded to another vm now", host),
                      Some("forwards"));
    }
    if start {
        if let Err(e) = hv::run::ensure_runnable(&state.machine) {
            return refuse(call, Status::Unsupported, &alloc::format!("no guest can run here -- {}", e), Some("start"));
        }
    }
    let o = Options {
        mem_mib: mem,
        cpus: cpus.map(|n| n as u32),
        network: network.map(String::from),
        port: port.map(|n| n as u32),
        forwards,
        ..Options::default()
    };
    let recipe = cloud::Recipe { url, size, password, hvboot: cloud::HVBOOT, user_data, protected };
    let mut job = Job::new(call, &state.unloading);
    let created = cloud::create(&state.vms_root, &state.vms, name, &recipe, o, &mut job);
    /* Started once it is made, when asked: by the same job, its reservation
     * of the name given back by now. */
    let started = match &created {
        Ok(_) if start && !job.cancelled() => Some(store::folder_of(&state.vms_root, name).and_then(|folder| {
            state.vms.start_from(&state.machine, &state.cores, &state.forwards, folder, &mut job)
        })),
        _ => None,
    };
    drop(job);
    match (created, started) {
        (Ok(_), Some(Err(e))) => failed(call, &alloc::format!("{} was made, but did not start -- {}", name, e)),
        (Ok(c), started) => answer(call, |w| {
            w.begin_object();
            w.field_str("name", name);
            w.field_str("file", &c.file);
            w.field_uint("disk_bytes", c.disk_bytes);
            w.field_uint("keys", c.keys as u64);
            w.key("port");
            match port {
                Some(p) => w.uint(p),
                None => w.null(),
            }
            w.key("id");
            match started {
                Some(Ok(id)) => w.uint(u64::from(id)),
                _ => w.null(),
            }
            w.end_object();
        }),
        (Err(e), _) => failed(call, &e),
    }
}

/// `forwards`: nos's ports relayed to the guest's, each `"<nos's>:<the
/// guest's>"` as vm.toml's `forward` has them.
fn forwards_param(p: &mut Params, call: &mut Call) -> Result<Vec<(u16, u16)>, Status> {
    let items = p.list("forwards", forward::MAX_FORWARDS).map_err(|e| invalid(call, e))?;
    let mut forwards = Vec::new();
    if forwards.try_reserve_exact(items.len()).is_err() {
        return Err(refuse(call, Status::Busy, "out of memory", None));
    }
    for item in items {
        let pair = item.as_str().and_then(config::forward);
        match pair {
            Some(pair) if !forwards.iter().any(|&(host, _)| host == pair.0) => forwards.push(pair),
            _ => return Err(refuse(call, Status::Invalid, "is a list of \"<nos's port>:<the guest's>\", each 1..65535 \
                                                          and each of nos's ports once", Some("forwards"))),
        }
    }
    Ok(forwards)
}

fn clone(state: &State, mut p: Params, call: &mut Call) -> Status {
    let (from, to) = match (name_param(&mut p, "name", call), name_param(&mut p, "to", call)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(s), _) | (_, Err(s)) => return s,
    };
    let port = match p.opt_uint("port", 0, net::MAX_PORTS as u64 - 1) {
        Ok(port) => port.map(|n| n as u32),
        Err(e) => return invalid(call, e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    if let Err(s) = folder(state, from, call) {
        return s;
    }
    if state.vms.status_of(from).is_some() {
        return refuse(call, Status::Conflict, &alloc::format!("{} is on the list -- stop it first", from),
                      Some("name"));
    }
    if store::folder_of(&state.vms_root, to).is_ok() {
        return refuse(call, Status::Conflict, &alloc::format!("{} is there already", to), Some("to"));
    }
    let cloned = {
        let mut job = Job::new(call, &state.unloading);
        crate::clone::clone(&state.vms_root, &state.vms, from, to, port, &mut job)
    };
    match cloned {
        Ok(c) => answer(call, |w| {
            w.begin_object();
            w.field_str("name", to);
            w.field_str("file", &c.file);
            w.key("port");
            match c.port {
                Some(p) => w.uint(u64::from(p)),
                None => w.null(),
            }
            w.field_bool("seed", c.seed);
            w.key("copied");
            w.begin_list();
            for (path, size, written) in &c.copied {
                w.begin_object();
                w.field_str("path", path);
                w.field_uint("size_bytes", *size);
                w.field_uint("written_bytes", *written);
                w.end_object();
            }
            w.end_list();
            w.end_object();
        }),
        Err(e) => failed(call, &e),
    }
}

fn delete(state: &State, mut p: Params, call: &mut Call) -> Status {
    let name = match name_param(&mut p, "name", call) {
        Ok(n) => n,
        Err(s) => return s,
    };
    let (keep, force) = match (p.opt_bool("keep_disks"), p.opt_bool("force")) {
        (Ok(k), Ok(f)) => (k.unwrap_or(false), f.unwrap_or(false)),
        (Err(e), _) | (_, Err(e)) => return invalid(call, e),
    };
    if let Err(s) = finish(p, call) {
        return s;
    }
    if let Err(s) = folder(state, name, call) {
        return s;
    }
    if let Err(s) = guarded(state, name, force, call) {
        return s;
    }
    if state.vms.status_of(name).is_some() {
        return refuse(call, Status::Conflict, &alloc::format!("{} is on the list -- stop it first", name),
                      Some("name"));
    }
    let deleted = {
        let mut job = Job::new(call, &state.unloading);
        let d = store::delete(&state.vms_root, &state.vms, name, keep, force);
        if let Ok(d) = &d {
            let _ = writeln!(job, "{} deleted -- {} files and folders taken away", name, d.removed);
        }
        d
    };
    match deleted {
        Ok(d) => answer(call, |w| {
            w.begin_object();
            w.field_uint("removed", d.removed as u64);
            w.key("kept");
            w.begin_list();
            for k in &d.kept {
                w.str(k);
            }
            w.end_list();
            w.end_object();
        }),
        Err(e) => failed(call, &e),
    }
}
