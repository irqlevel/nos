#![no_std]

//! api: the host API, as a module -- what the panel, `nosctl` and other
//! hosts' panels manage the machine through.
//!
//!     insmod /lib/modules/api.ko [conf=/etc/api]
//!     api client add panel admin 3f2a...e1
//!     api start
//!     api
//!     api stop
//!
//! HTTP/1.1 and a WebSocket, JSON, over TLS 1.3 with a client's certificate
//! required: a client is let in by its key alone, the SHA-256 of its
//! SubjectPublicKeyInfo on the list in `clients` -- decided in the handshake,
//! so nothing a stranger sends is ever parsed. What it serves is the
//! management bus (http.rs has the routes); it runs no command of the
//! shell's, and no client can make it. Its folder's files are in files.rs;
//! where it listens, and whom it takes before any of that, is api.toml
//! (config.rs). `api panel create` makes the management panel, a VM of hv's
//! that is a client of the API like any other (panel.rs). docs/api.md has
//! the rest.

extern crate alloc;

mod config;
mod files;
mod http;
mod panel;
mod server;
mod stream;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kcore::cmd::{Command, Output};
use kcore::error::Error;
use kcore::mgmt::{Call, Kind, Method, Provider, Status};
use kcore::net::Nic;

use files::{Client, Dir, Role};
use http::Ip;
use server::{Counters, Listening, Locked, Phase, Server, Shared};

const HELP: &str = "api [start | stop | clients | client add|del | reload | cert | panel] - the host API";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const USAGE: &str = "usage: api                         what it serves, and to whom\n\
                     \x20      api start                   serve, as api.toml says\n\
                     \x20      api stop\n\
                     \x20      api clients                 the keys let in\n\
                     \x20      api client add <name> <admin|read> <fingerprint | cert=<file.pem>>\n\
                     \x20      api client del <name>\n\
                     \x20      api reload                  the clients file and api.toml's allowlists read again\n\
                     \x20      api cert                    the host's certificate, and its key's fingerprint\n\
                     \x20      api panel                   the management panel's VM, and where it is\n\
                     \x20      api panel create [image=<url>] [bundle=<url> sha256=<hex>] [port=<n>] [memory=<MiB>]\n\
                     \x20                                  the panel made, and started (at every boot: /etc/rc)";

/// The interfaces a certificate is made for, by name, when they are there.
const CERT_NICS: [&str; 2] = ["eth0", "hv0"];
/// A client's certificate, as `cert=` reads it.
const CERT_FILE_MAX: usize = 16 * 1024;

/* Dropped in this order: the command, waiting out a call of it; the
   provider, waiting out a call of it; then the state, and with its last
   reference the server, whose listeners and connections are waited for */
struct Api {
    _cmd: Command,
    _provider: Provider,
    state: Arc<State>,
}

impl kmod::Module for Api {}

impl Drop for Api {
    fn drop(&mut self) {
        /* The server's tasks told to stop before the command goes: a call
         * of it may be running, and the drop of the command waits it out */
        self.state.server.with(|s| {
            if let Some(s) = s.as_ref() {
                s.shared.stop.store(true, Ordering::Release);
            }
        });
    }
}

/// The module's: the folder, and the server while there is one.
struct State {
    dir: Dir,
    server: Locked<Option<Box<Server>>>,
    /// Held through every change of the clients file
    files: Locked<()>,
    starting: AtomicBool,
}

fn init(params: &kmod::Params, out: &mut Output) -> kcore::error::Result<Box<dyn kmod::Module>> {
    let mut root = None;
    for (key, value) in params.words() {
        match (key, value) {
            ("conf", Some(path)) if root.is_none() && path.starts_with('/') && path.len() < 200 => root = Some(path),
            _ => {
                let _ = writeln!(out, "api: \"{}\" is not a parameter of api's -- conf=<folder> is, given once", key);
                return Err(Error::InvalidValue);
            }
        }
    }
    let state = Arc::new(State {
        dir: Dir::new(root.unwrap_or(files::DEFAULT_DIR)),
        server: Locked::new(None).ok_or(Error::NoMemory)?,
        files: Locked::new(()).ok_or(Error::NoMemory)?,
        starting: AtomicBool::new(false),
    });
    let st = state.clone();
    let cmd = Command::register("api", HELP, move |args, out| {
        if let Err(problem) = run(&st, args, out) {
            let _ = writeln!(out, "api: {}", problem);
        }
    })?;
    let st = state.clone();
    let methods = [Method { name: "status", kind: Kind::Read }, Method { name: "clients", kind: Kind::Read }];
    let provider = Provider::register("api", &methods, include_str!("api.descriptor.json"),
                                      move |method, params, call| provide(&st, method, params, call))
        .map_err(|_| Error::Busy)?;
    Ok(Box::new(Api { _cmd: cmd, _provider: provider, state }))
}

/// The provider's: what `api status` says, as JSON, and the clients.
fn provide(state: &State, method: &str, params: &[u8], call: &mut Call) -> Status {
    if !params.is_empty() && params != b"null" && params != b"{}" {
        call.json(|w| Status::Invalid.write_error(w, "takes no parameters", None));
        return Status::Invalid;
    }
    match method {
        "status" => {
            let shared = state.server.with(|s| s.as_ref().map(|s| s.shared.clone()));
            let ok = call.json(|w| status_json(w, state, shared.as_deref()));
            if ok { Status::Ok } else { Status::TooLarge }
        }
        "clients" => {
            let list = match state.files.with(|_| files::read_clients(&state.dir)) {
                Ok(l) => l,
                Err(e) => {
                    call.json(|w| Status::Failed.write_error(w, &e, None));
                    return Status::Failed;
                }
            };
            let ok = call.json(|w| {
                w.begin_object();
                w.key("clients");
                w.begin_list();
                for c in &list {
                    w.begin_object();
                    w.field_str("name", &c.name);
                    w.field_str("role", c.role.name());
                    w.field_str("key", &files::hex(&c.key));
                    w.end_object();
                }
                w.end_list();
                w.end_object();
            });
            if ok { Status::Ok } else { Status::TooLarge }
        }
        _ => Status::NoMethod,
    }
}

fn status_json(w: &mut kcore::mgmt::Writer, state: &State, sh: Option<&Shared>) {
    w.begin_object();
    w.field_str("folder", &state.dir.root);
    w.field_bool("serving", sh.is_some());
    if let Some(sh) = sh {
        let c = &sh.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        w.field_uint("port", u64::from(sh.config.port));
        w.field_str("host", &sh.identity.id);
        w.field_str("key", &files::hex(&sh.identity.key_fingerprint));
        let now = kcore::time::boot_time_ns();
        w.field_uint("serving_ms", now.saturating_sub(sh.started_ns) / 1_000_000);
        w.key("listeners");
        w.begin_list();
        for (nic, l) in sh.listeners.with(|v| v.clone()) {
            w.begin_object();
            w.field_str("nic", &nic);
            w.field_str("state", match l {
                Listening::Waiting => "waiting",
                Listening::Bound => "listening",
                Listening::Failed => "failed",
            });
            w.end_object();
        }
        w.end_list();
        /* Each interface's allowlist, for those that have one: none, any
         * address */
        w.key("allow");
        w.begin_list();
        for (nic, nets) in sh.allow.with(|a| a.clone()) {
            w.begin_object();
            w.field_str("nic", &nic);
            w.key("networks");
            w.begin_list();
            for c in &nets {
                w.str(&network(c));
            }
            w.end_list();
            w.end_object();
        }
        w.end_list();
        let conns: Vec<(u64, String, u32, u16, bool, String, Option<Role>, u64)> = sh.conns.with(|v| {
            v.iter().map(|c| (c.id, c.nic.clone(), c.ip, c.port, c.phase == Phase::Open, c.name.clone(), c.role,
                              c.since_ns)).collect()
        });
        w.key("connections");
        w.begin_list();
        for (id, nic, ip, port, open, name, role, since) in &conns {
            w.begin_object();
            w.field_uint("id", *id);
            w.field_str("nic", nic);
            w.field_str("ip", &alloc::format!("{}", Ip(*ip)));
            w.field_uint("port", u64::from(*port));
            w.field_bool("open", *open);
            w.field_str("client", name);
            w.field_str("role", role.map_or("", |r| r.name()));
            w.field_uint("seconds", now.saturating_sub(*since) / 1_000_000_000);
            w.end_object();
        }
        w.end_list();
        w.field_uint("accepted", get(&c.accepted));
        w.field_uint("not_allowed", get(&c.not_allowed));
        w.field_uint("busy", get(&c.busy));
        w.field_uint("refused", get(&c.refused));
        w.field_uint("unknown_key", get(&c.unknown_key));
        w.field_uint("requests", get(&c.requests));
        w.field_uint("streams", get(&c.streams));
        let n = get(&c.handshakes);
        w.key("handshakes");
        w.begin_object();
        w.field_uint("count", n);
        w.field_uint("last_us", get(&c.handshake_us_last));
        w.field_uint("max_us", get(&c.handshake_us_max));
        w.field_uint("mean_us", get(&c.handshake_us_total).checked_div(n).unwrap_or(0));
        w.end_object();
    }
    w.end_object();
}

kmod::module!(name: "api", init_with_params: init);

fn run(state: &State, args: &str, out: &mut Output) -> Result<(), String> {
    let mut words = args.split_whitespace();
    match words.next() {
        None | Some("status") => {
            status(state, out);
            Ok(())
        }
        Some("start") => start(state, out),
        Some("stop") => stop(state, out),
        Some("clients") => clients(state, out),
        Some("client") => match words.next() {
            Some("add") => client_add(state, words, out),
            Some("del") => client_del(state, words.next(), out),
            _ => Err(String::from(USAGE)),
        },
        Some("reload") => reload(state, out),
        Some("cert") => cert(state, out),
        Some("panel") => panel::run(state, words, out),
        Some(other) => Err(alloc::format!("what is '{}'?\n{}", other, USAGE)),
    }
}

fn read_config(dir: &Dir) -> Result<config::Config, String> {
    let path = dir.path("api.toml");
    match kcore::fs::read(&path, config::MAX_BYTES) {
        Ok(bytes) => {
            let text = core::str::from_utf8(&bytes).map_err(|_| alloc::format!("{} is not UTF-8", path))?;
            config::parse(text).map_err(|e| alloc::format!("{}: {}", path, e))
        }
        Err(Error::NotFound) => Ok(config::Config::defaults()),
        Err(_) => Err(alloc::format!("cannot read {}", path)),
    }
}

/// The addresses of the interfaces the certificate is for, as they stand.
fn cert_ips() -> Vec<[u8; 4]> {
    CERT_NICS.iter().filter_map(|n| Nic::find(n)).map(|n| n.ip()).filter(|ip| *ip != 0).map(u32::to_be_bytes).collect()
}

fn start(state: &State, out: &mut Output) -> Result<(), String> {
    if state.server.with(|s| s.is_some()) {
        return Err(String::from("serving already -- api stop first"));
    }
    if state.starting.swap(true, Ordering::AcqRel) {
        return Err(String::from("being started from somewhere else"));
    }
    let result = start_locked(state, out);
    state.starting.store(false, Ordering::Release);
    result
}

fn start_locked(state: &State, out: &mut Output) -> Result<(), String> {
    let mut config = read_config(&state.dir)?;
    let allow = core::mem::take(&mut config.allow);
    let (identity, made) = state.files.with(|_| files::identity(&state.dir, &cert_ips()))?;
    if made {
        kcore::trace!(0, "api: the host's key made: {}", files::hex(&identity.key_fingerprint));
    }
    let clients = state.files.with(|_| files::read_clients(&state.dir))?;
    let tls = Arc::new(server::tls_server(&identity, &clients)?);
    let count = clients.len();
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        config,
        allow: Locked::new(allow).ok_or_else(|| String::from("no memory"))?,
        dir: Dir::new(&state.dir.root),
        identity,
        tls: Locked::new(tls).ok_or_else(|| String::from("no memory"))?,
        clients: Locked::new(clients).ok_or_else(|| String::from("no memory"))?,
        conns: Locked::new(Vec::new()).ok_or_else(|| String::from("no memory"))?,
        listeners: Locked::new(Vec::new()).ok_or_else(|| String::from("no memory"))?,
        next_id: AtomicU64::new(1),
        counters: Counters::default(),
        audit: Locked::new(()).ok_or_else(|| String::from("no memory"))?,
        started_ns: kcore::time::boot_time_ns(),
    });
    let listeners = server::start_listeners(&shared)?;
    let fingerprint = files::hex(&shared.identity.key_fingerprint);
    let port = shared.config.port;
    let names = shared.config.listen.join(", ");
    state.server.with(|s| *s = Some(Box::new(Server { shared, listeners })));
    kcore::trace!(0, "api: serving port {} on {}, host key {}", port, names, fingerprint);
    let _ = writeln!(out, "api: serving port {} on {} -- host key {} -- {} clients", port, names, fingerprint, count);
    if count == 0 {
        let _ = writeln!(out, "api: nobody is let in yet -- api client add <name> <admin|read> <fingerprint>");
    }
    Ok(())
}

fn stop(state: &State, out: &mut Output) -> Result<(), String> {
    let server = state.server.with(|s| s.take()).ok_or_else(|| String::from("not serving"))?;
    /* Out of the lock: this waits for the listeners and every connection */
    drop(server);
    let _ = writeln!(out, "api: stopped");
    Ok(())
}

fn status(state: &State, out: &mut Output) {
    let shared = state.server.with(|s| s.as_ref().map(|s| s.shared.clone()));
    let Some(sh) = shared else {
        let _ = writeln!(out, "api: not serving -- folder {}", state.dir.root);
        return;
    };
    let c = &sh.counters;
    let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let _ = writeln!(out, "api: serving port {} for {} s, folder {}, host {}", sh.config.port,
                     kcore::time::boot_time_ns().saturating_sub(sh.started_ns) / 1_000_000_000, sh.dir.root, sh.identity.id);
    let _ = writeln!(out, "api: host key {}", files::hex(&sh.identity.key_fingerprint));
    let listeners: Vec<(String, Listening)> = sh.listeners.with(|v| v.clone());
    let allow = sh.allow.with(|a| a.clone());
    for (nic, l) in &listeners {
        let what = match l {
            Listening::Waiting => "waiting for it",
            Listening::Bound => "listening",
            Listening::Failed => "cannot bind -- trying again",
        };
        let _ = write!(out, "  {}: {}", nic, what);
        match allow.iter().find(|(n, _)| n == nic) {
            Some((_, nets)) => {
                let _ = write!(out, ", from");
                for c in nets {
                    let _ = write!(out, " {}", network(c));
                }
                let _ = writeln!(out);
            }
            None => {
                let _ = writeln!(out, ", from anywhere");
            }
        }
    }
    let conns: Vec<(u64, String, u32, u16, bool, String, Option<Role>, u64)> = sh.conns.with(|v| {
        v.iter().map(|c| (c.id, c.nic.clone(), c.ip, c.port, c.phase == Phase::Open, c.name.clone(), c.role,
                          c.since_ns)).collect()
    });
    let now = kcore::time::boot_time_ns();
    for (id, nic, ip, port, open, name, role, since) in &conns {
        let _ = writeln!(out, "  [{}] {} {}:{} {} {} {} s", id, nic, Ip(*ip), port,
                         if *open { name.as_str() } else { "(handshake)" }, role.map_or("", |r| r.name()),
                         now.saturating_sub(*since) / 1_000_000_000);
    }
    let handshakes = get(&c.handshakes);
    let _ = writeln!(out, "api: {} connections taken, {} not allowed, {} past a limit, {} refused in the handshake ({} \
                           unknown keys), {} requests, {} streams",
                     get(&c.accepted), get(&c.not_allowed), get(&c.busy), get(&c.refused), get(&c.unknown_key),
                     get(&c.requests), get(&c.streams));
    let _ = writeln!(out, "api: handshakes {}, last {} us, slowest {} us, mean {} us", handshakes,
                     get(&c.handshake_us_last), get(&c.handshake_us_max),
                     get(&c.handshake_us_total).checked_div(handshakes).unwrap_or(0));
}

/// A network of an allowlist, as api.toml would write it.
fn network(c: &web::cidr::Cidr) -> String {
    let (net, bits) = c.parts();
    alloc::format!("{}/{}", Ip(net), bits)
}

fn clients(state: &State, out: &mut Output) -> Result<(), String> {
    let list = state.files.with(|_| files::read_clients(&state.dir))?;
    if list.is_empty() {
        let _ = writeln!(out, "api: no clients in {}", state.dir.path("clients"));
    }
    for c in &list {
        let _ = writeln!(out, "{} {} {}", files::hex(&c.key), c.role.name(), c.name);
    }
    Ok(())
}

/// The key `word` names: its fingerprint, or `cert=<file>` -- the
/// certificate's key, read off it.
fn key_of(word: &str) -> Result<tls::server::Fingerprint, String> {
    if let Some(path) = word.strip_prefix("cert=") {
        let bytes = kcore::fs::read(path, CERT_FILE_MAX).map_err(|_| alloc::format!("cannot read {}", path))?;
        let text = core::str::from_utf8(&bytes).map_err(|_| alloc::format!("{} is not text", path))?;
        let der = web::x509::pem_decode("CERTIFICATE", text).ok_or_else(|| alloc::format!("{} has no CERTIFICATE", path))?;
        return tls::server::fingerprint(&der).ok_or_else(|| alloc::format!("{} is not a certificate", path));
    }
    files::parse_hex(word).ok_or_else(|| String::from("a fingerprint is 64 hex digits: the SHA-256 of the key's \
                                                       SubjectPublicKeyInfo, or cert=<file.pem>"))
}

fn client_add<'a>(state: &State, mut words: impl Iterator<Item = &'a str>, out: &mut Output) -> Result<(), String> {
    let (Some(name), Some(role), Some(key), None) = (words.next(), words.next(), words.next(), words.next()) else {
        return Err(String::from(USAGE));
    };
    let role = Role::parse(role).ok_or_else(|| String::from("the role is admin or read"))?;
    let key = key_of(key)?;
    let list = state.files.with(|_| -> Result<Vec<Client>, String> {
        let mut list = files::read_clients(&state.dir)?;
        if list.iter().any(|c| c.name == name || c.key == key) {
            return Err(String::from("that name or that key is let in already -- api client del it first"));
        }
        if list.len() == files::CLIENTS_LIMIT {
            return Err(alloc::format!("{} clients is all there may be", files::CLIENTS_LIMIT));
        }
        let mut text = String::new();
        let _ = write!(text, "{} {} {}", files::hex(&key), role.name(), name);
        let parsed = files::parse_clients(&text)?;
        list.extend(parsed);
        let _ = kcore::fs::create_dir(&state.dir.root);
        files::write_clients(&state.dir, &list)?;
        Ok(list)
    })?;
    let _ = writeln!(out, "api: {} lets in {} as {}", name, files::hex(&key), role.name());
    apply(state, list, out);
    Ok(())
}

fn client_del(state: &State, name: Option<&str>, out: &mut Output) -> Result<(), String> {
    let name = name.ok_or_else(|| String::from(USAGE))?;
    let list = state.files.with(|_| -> Result<Vec<Client>, String> {
        let mut list = files::read_clients(&state.dir)?;
        let before = list.len();
        list.retain(|c| c.name != name);
        if list.len() == before {
            return Err(alloc::format!("no client {}", name));
        }
        files::write_clients(&state.dir, &list)?;
        Ok(list)
    })?;
    let _ = writeln!(out, "api: {} is let in no more", name);
    apply(state, list, out);
    Ok(())
}

/// The clients file and api.toml's allowlists read again, and given to the
/// server if it runs -- the clients from the next handshake on, the
/// allowlists from the next connection.
fn reload(state: &State, out: &mut Output) -> Result<(), String> {
    let list = state.files.with(|_| files::read_clients(&state.dir))?;
    apply(state, list, out);
    let config = read_config(&state.dir)?;
    apply_allow(state, config, out);
    Ok(())
}

/// api.toml's allowlists given to the server, if it runs; what else it says
/// waits for the next start, and is said to.
fn apply_allow(state: &State, mut config: config::Config, out: &mut Output) {
    let shared = state.server.with(|s| s.as_ref().map(|s| s.shared.clone()));
    let Some(sh) = shared else { return };
    let allow = core::mem::take(&mut config.allow);
    let lists = allow.len();
    sh.allow.with(|a| *a = allow);
    let _ = writeln!(out, "api: {} allowlists from the next connection on", lists);
    if !sh.config.same_server(&config) {
        let _ = writeln!(out, "api: api.toml's port, interfaces and limits are taken as it starts -- api stop, api \
                               start");
    }
}

/// A list of clients, given to the server if it runs: from the next
/// handshake on, and a connection of a key taken off it ends at its next
/// request.
fn apply(state: &State, list: Vec<Client>, out: &mut Output) {
    let shared = state.server.with(|s| s.as_ref().map(|s| s.shared.clone()));
    if let Some(sh) = shared {
        match sh.set_clients(list) {
            Ok(n) => {
                let _ = writeln!(out, "api: {} clients let in from the next handshake on", n);
            }
            Err(e) => {
                let _ = writeln!(out, "api: {}", e);
            }
        }
    }
}

fn cert(state: &State, out: &mut Output) -> Result<(), String> {
    let (identity, _) = state.files.with(|_| files::identity(&state.dir, &cert_ips()))?;
    out.write_bytes(identity.cert_pem.as_bytes());
    let _ = writeln!(out, "api: the host's key, as a client pins it: {}", files::hex(&identity.key_fingerprint));
    let _ = writeln!(out, "api: host {}", identity.id);
    Ok(())
}
