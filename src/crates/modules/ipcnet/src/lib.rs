#![no_std]

//! ipcnet: the message channel between hosts, as a module (plans/ipc.md,
//! I4) -- what makes a mailbox on another host of the cluster an address
//! here, and `name@host` a name.
//!
//!     insmod /lib/modules/ipcnet.ko [conf=/etc/ipc]
//!     ipcnet init ex44                       this host's name, a new cluster
//!     ipcnet id                              the key every peer pins
//!     ipcnet add ax41 65.109.93.213 9c1e...  a peer, pinned by its key
//!     ipcnet start
//!     ipcnet                                 the peers, and what went between them
//!
//! Each pair of hosts has two TCP connections, control and bulk, both TLS
//! 1.3 with both keys pinned, on one port (39240), on the interface
//! hosts.toml names -- eth0 unless told, and never hv0, so no guest reaches
//! it -- whose listener answers the peers' addresses alone. The host whose
//! name sorts first dials. The module is the channel's router: everything
//! sent to a peer's address comes to its mailbox, and it hands the channel
//! what peers send (`deliver_remote`), which delivers only to a `Cluster`
//! name or to a mailbox that has sent there. files.rs has its folder,
//! server.rs its tasks, link.rs a connection, router.rs the router, and
//! peer.rs a peer's session -- up once both connections have said their
//! Hello, lost the moment either ends or is silent for three seconds.

extern crate alloc;

mod control;
mod files;
mod link;
mod peer;
mod router;
mod server;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kcore::cmd::{Command, Output};
use kcore::error::Error;
use kcore::task::TaskHandle;

use files::{Dir, Ip, PeerConf};
use peer::Locked;
use server::Server;

const HELP: &str = "ipcnet [start | stop | id | init | add | remove] - this host's links to its cluster";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const USAGE: &str = "usage: ipcnet                          this host, its peers, and what went between them\n\
                     \x20      ipcnet init <name> [cluster=<32 hex digits>]\n\
                     \x20                                     hosts.toml made: this host, in a new cluster or the one given\n\
                     \x20      ipcnet id                       this host's key, which every peer pins\n\
                     \x20      ipcnet add <name> <address> <key> [<key>]\n\
                     \x20      ipcnet remove <name>\n\
                     \x20      ipcnet start                    serve, as hosts.toml says\n\
                     \x20      ipcnet stop                     every peer lost, every connection closed";

/// The module's: its folder, and the server while there is one.
struct State {
    dir: Dir,
    server: Locked<Option<Box<Server>>>,
    /// Held through every change of hosts.toml
    files: Locked<()>,
    /// The control mailbox's task stops at it (control.rs).
    ctl_stop: AtomicBool,
}

/* Dropped in this order: the control task, stopped and waited out; the
   server, whose tasks are waited for -- every peer lost on the way; then
   the command, waiting out a call of it, and the state */
struct Ipcnet {
    _cmd: Command,
    ctl: Option<TaskHandle>,
    state: Arc<State>,
}

impl kmod::Module for Ipcnet {}

impl Drop for Ipcnet {
    fn drop(&mut self) {
        self.state.ctl_stop.store(true, Ordering::Release);
        drop(self.ctl.take());
        let server = self.state.server.with(|s| s.take());
        drop(server);
    }
}

fn init(params: &kmod::Params, out: &mut Output) -> kcore::error::Result<Box<dyn kmod::Module>> {
    let mut root = None;
    for (key, value) in params.words() {
        match (key, value) {
            ("conf", Some(path)) if root.is_none() && path.starts_with('/') && path.len() < 200 => root = Some(path),
            _ => {
                let _ = writeln!(out, "ipcnet: \"{}\" is not a parameter of ipcnet's -- conf=<folder> is, given \
                                       once", key);
                return Err(Error::InvalidValue);
            }
        }
    }
    let state = Arc::new(State {
        dir: Dir::new(root.unwrap_or(files::DEFAULT_DIR)),
        server: Locked::new(None).ok_or(Error::NoMemory)?,
        files: Locked::new(()).ok_or(Error::NoMemory)?,
        ctl_stop: AtomicBool::new(false),
    });
    let st = state.clone();
    let cmd = Command::register("ipcnet", HELP, move |args, out| {
        if let Err(problem) = run(&st, args, out) {
            let _ = writeln!(out, "ipcnet: {}", problem);
        }
    })?;
    let Some(ctl) = kcore::task::spawn_with("ipcnet/ctl", state.clone(), control::main) else {
        let _ = writeln!(out, "ipcnet: could not start the control mailbox's task");
        return Err(Error::NoMemory);
    };
    Ok(Box::new(Ipcnet { _cmd: cmd, ctl: Some(ctl), state }))
}

kmod::module!(name: "ipcnet", init_with_params: init);

fn run(state: &State, args: &str, out: &mut Output) -> Result<(), String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] | ["status"] => status(state, out),
        ["start"] => start(state, out),
        ["stop"] => {
            let server = state.server.with(|s| s.take());
            match server {
                Some(s) => {
                    drop(s);
                    let _ = writeln!(out, "ipcnet: stopped");
                }
                None => {
                    let _ = writeln!(out, "ipcnet: not serving");
                }
            }
            Ok(())
        }
        ["id"] => {
            let cfg = config(state)?;
            let (id, made) = files::identity(&state.dir, &cfg.name)?;
            if made {
                let _ = writeln!(out, "ipcnet: a key made for {}", cfg.name);
            }
            let _ = writeln!(out, "{}", files::hex(&id.fingerprint));
            Ok(())
        }
        ["init", name] => init_hosts(state, name, None, out),
        ["init", name, c] => match c.strip_prefix("cluster=") {
            Some(hex) => init_hosts(state, name, Some(hex), out),
            None => Err(String::from(USAGE)),
        },
        ["add", name, ip, keys @ ..] if !keys.is_empty() => add(state, name, ip, keys, out),
        ["remove", name] => remove(state, name, out),
        _ => Err(String::from(USAGE)),
    }
}

/// hosts.toml read.
fn config(state: &State) -> Result<files::Config, String> {
    let text = files::read_hosts(&state.dir)?
        .ok_or_else(|| alloc::format!("no {} -- `ipcnet init <name>` makes one", state.dir.path("hosts.toml")))?;
    files::parse(&text)
}

fn init_hosts(state: &State, name: &str, cluster: Option<&str>, out: &mut Output) -> Result<(), String> {
    if !files::valid_host(name) {
        return Err(alloc::format!("\"{}\" is not a host's name: lowercase letters, digits and -", name));
    }
    let cluster = match cluster {
        Some(hex) => {
            let mut c = [0u8; 16];
            let b = hex.as_bytes();
            if b.len() != 32 {
                return Err(String::from("a cluster's id is 32 hex digits"));
            }
            for (i, pair) in b.chunks(2).enumerate() {
                let digit = |x: u8| (x as char).to_digit(16);
                let (Some(h), Some(l)) = (digit(pair[0]), digit(pair[1])) else {
                    return Err(String::from("a cluster's id is 32 hex digits"));
                };
                c[i] = (h * 16 + l) as u8;
            }
            c
        }
        None => {
            let mut c = [0u8; 16];
            if !kcore::random::fill_random(&mut c) {
                return Err(String::from("no randomness for a cluster's id"));
            }
            c
        }
    };
    state.files.with(|_| {
        if files::read_hosts(&state.dir)?.is_some() {
            return Err(alloc::format!("{} is there already", state.dir.path("hosts.toml")));
        }
        files::write_hosts(&state.dir, &files::render_new(name, &cluster))
    })?;
    let _ = writeln!(out, "ipcnet: {} in cluster {} -- `ipcnet id` says its key, `ipcnet add` its peers",
                     name, files::hex(&cluster));
    Ok(())
}

/// hosts.toml's peers changed by `change`, written, and the running
/// server's made the same.
fn edit_peers(state: &State, change: impl FnOnce(&files::Config, &mut Vec<PeerConf>) -> Result<(), String>)
              -> Result<(), String> {
    let peers = state.files.with(|_| {
        let text = files::read_hosts(&state.dir)?
            .ok_or_else(|| alloc::format!("no {} -- `ipcnet init <name>` makes one", state.dir.path("hosts.toml")))?;
        let cfg = files::parse(&text)?;
        let mut peers = cfg.peers.clone();
        change(&cfg, &mut peers)?;
        let text = files::with_peers(&text, &peers)?;
        files::write_hosts(&state.dir, &text)?;
        Ok::<Vec<PeerConf>, String>(peers)
    })?;
    state.server.with(|s| match s.as_ref() {
        Some(s) => s.set_peers(&peers),
        None => Ok(()),
    })
}

fn add(state: &State, name: &str, ip: &str, keys: &[&str], out: &mut Output) -> Result<(), String> {
    if !files::valid_host(name) {
        return Err(alloc::format!("\"{}\" is not a host's name: lowercase letters, digits and -", name));
    }
    let ip = files::parse_ip(ip).ok_or_else(|| alloc::format!("\"{}\" is not an IPv4 address", ip))?;
    let mut fingerprints = Vec::new();
    for k in keys {
        fingerprints.push(files::parse_key(k).ok_or_else(|| alloc::format!("\"{}\" is not a key: 64 hex digits", k))?);
    }
    let peer = PeerConf { name: String::from(name), ip, keys: fingerprints, only: None };
    edit_peers(state, |cfg, peers| {
        if cfg.name == name {
            return Err(alloc::format!("{} is this host", name));
        }
        if peers.len() >= files::PEERS_MAX {
            return Err(alloc::format!("{} peers is all there may be", files::PEERS_MAX));
        }
        match peers.iter().position(|p| p.name == name) {
            /* Given again: its address or its keys changed */
            Some(i) => peers[i] = peer.clone(),
            None => {
                if peers.iter().any(|p| p.ip == ip) {
                    return Err(alloc::format!("{} is another peer's address", Ip(ip)));
                }
                peers.push(peer.clone());
            }
        }
        Ok(())
    })?;
    let _ = writeln!(out, "ipcnet: {} ({}) pinned", name, Ip(ip));
    Ok(())
}

fn remove(state: &State, name: &str, out: &mut Output) -> Result<(), String> {
    edit_peers(state, |_, peers| {
        let before = peers.len();
        peers.retain(|p| p.name != name);
        if peers.len() == before {
            return Err(alloc::format!("{} is not a peer", name));
        }
        Ok(())
    })?;
    let _ = writeln!(out, "ipcnet: {} taken off", name);
    Ok(())
}

fn start(state: &State, out: &mut Output) -> Result<(), String> {
    let cfg = config(state)?;
    state.server.with(|s| {
        if s.is_some() {
            return Err(String::from("serving already"));
        }
        *s = Some(Server::start(&state.dir, cfg)?);
        Ok(())
    })?;
    let _ = writeln!(out, "ipcnet: serving");
    Ok(())
}

fn status(state: &State, out: &mut Output) -> Result<(), String> {
    let shared = state.server.with(|s| s.as_ref().map(|s| s.shared.clone()));
    let Some(sh) = shared else {
        let _ = writeln!(out, "ipcnet: not serving -- `ipcnet start`");
        if let Ok(cfg) = config(state) {
            let _ = writeln!(out, "ipcnet: hosts.toml: {} in cluster {}, {} peers", cfg.name, files::hex(&cfg.cluster),
                             cfg.peers.len());
        }
        return Ok(());
    };
    let now = kcore::time::boot_time_ns();
    let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let _ = writeln!(out, "ipcnet: {} in cluster {}, on {} port {}, key {} -- serving {} s", sh.cfg.name,
                     files::hex(&sh.cfg.cluster), sh.cfg.nic, sh.cfg.port, files::hex(&sh.identity.fingerprint),
                     now.saturating_sub(sh.started_ns) / 1_000_000_000);
    let peers: Vec<Arc<peer::Peer>> = sh.peers.with(|v| v.clone());
    for p in &peers {
        let (name, said, number, since) = peer::describe(p);
        let c = &p.counts;
        let mut state = if number != 0 {
            alloc::format!("up {} s as host {}", now.saturating_sub(since) / 1_000_000_000, number)
        } else {
            alloc::format!("down: {}", said)
        };
        p.only.with(|o| {
            if let Some(n) = o {
                let _ = write!(state, " (invited: {} alone)", n);
            }
        });
        let _ = writeln!(out, "  {:<12} {:<15} {} -- {}; sent {} got {}, bytes {} / {}, bounced {} / {}, refused {}, \
                               dropped {}, ups {}, losses {}", name, alloc::format!("{}", Ip(p.conf.ip)),
                         if p.dials { "dials" } else { "takes" }, state, get(&c.sent), get(&c.received),
                         get(&c.bytes_out), get(&c.bytes_in), get(&c.bounced_out), get(&c.bounced_in),
                         get(&c.refused), get(&c.dropped), get(&c.ups), get(&c.losses));
    }
    let c = &sh.counters;
    let _ = writeln!(out, "ipcnet: taken {}, busy {}, handshakes refused {} in / {} out, hellos refused {}, frames \
                           refused {}, routes bad {}, unrouted {}, strays {}", get(&c.accepted), get(&c.busy),
                     get(&c.refused_in), get(&c.refused_out), get(&c.hello_refused), get(&c.wire_refused),
                     get(&c.bad_route), get(&c.unrouted), get(&c.stray));
    Ok(())
}
