//! ipcnet's control mailbox, `ipcnet.ctl` (`ipcwire::control`,
//! plans/cluster.md): this host's modules ask it who this host is in its
//! cluster, and tell it the peers it is to have -- the cluster's registry
//! does, on every change it commits. hosts.toml is written here as `ipcnet
//! add` and `remove` write it (`edit_peers`), and the running server is
//! given the peers: one writer of the file, one way to the peers.
//!
//! It runs while the module is loaded, serving or not: a registry that
//! starts first finds this host's name and key, and the list it sends is in
//! the file for `ipcnet start` to read.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use ipcwire::control::{self, Identity, Peers};
use kcore::ipc::{Limits, Mailbox, Scope};
use kcore::klog;
use kcore::net::Nic;
use kcore::time::Duration;

use super::files::{self, Ip, PeerConf};
use super::server::Server;
use super::{config, edit_peers, State};

/// Requests at once, and their bytes: a list of sixteen peers is under a
/// kilobyte and a half.
const LIMITS: Limits = Limits { messages: 8, bytes: 32 * 1024 };
/// How long a receive waits at a time: how soon the module's unload is
/// seen.
const WAIT_MS: u64 = 250;

/// The task: requests answered until the module goes.
pub fn main(state: Arc<State>) {
    let mb = match Mailbox::new(LIMITS) {
        Ok(mb) => mb,
        Err(e) => {
            klog!(0, "ipcnet: no control mailbox: {}", e.name());
            return;
        }
    };
    if let Err(e) = mb.register(control::NAME, Scope::Host) {
        klog!(0, "ipcnet: {} not named: {}", control::NAME, e.name());
        return;
    }
    while !state.ctl_stop.load(Ordering::Acquire) {
        let Some(m) = mb.recv(Duration::from_millis(WAIT_MS)) else { continue };
        /* A request of this host's, as a `Host` name has them alone */
        if m.is_system() || m.from.host() != 0 {
            continue;
        }
        let answer = match m.tag {
            control::TAG_ID => identity(&state).map(|id| (control::TAG_ID, id.encode())),
            control::TAG_PEERS => match Peers::decode(&m.data) {
                Some(p) => set_peers(&state, &p).map(|()| (control::TAG_OK, Vec::new())),
                None => Err(String::from("not a list of peers")),
            },
            control::TAG_JOIN => match <[u8; 16]>::try_from(&m.data[..]) {
                Ok(cluster) => join(&state, &cluster).map(|()| (control::TAG_OK, Vec::new())),
                Err(_) => Err(String::from("not a cluster's id")),
            },
            _ => Err(String::from("not a request of ipcnet's")),
        };
        let _ = match answer {
            Ok((tag, data)) => mb.reply(&m, tag, &data),
            Err(why) => mb.reply(&m, control::TAG_REFUSED, why.as_bytes()),
        };
    }
}

/// This host as hosts.toml and its key say -- the key made first, where
/// there is none, as `ipcnet id` makes it.
fn identity(state: &State) -> Result<Identity, String> {
    let cfg = config(state)?;
    let (id, made) = files::identity(&state.dir, &cfg.name)?;
    if made {
        klog!(0, "ipcnet: a key made for {}: {}", cfg.name, files::hex(&id.fingerprint));
    }
    let serving = state.server.with(|s| s.is_some());
    let ip = Nic::find(&cfg.nic).map_or(0, |n| n.ip());
    Ok(Identity { name: cfg.name, cluster: cfg.cluster, key: id.fingerprint, nic: cfg.nic, ip, port: cfg.port, serving })
}

/// This host's cluster made `cluster`: the one it joins -- refused for a
/// host with peers in another, as hosts.toml says. A server running is
/// started again under it, its Hello saying the cluster's id: with no peers
/// there is nothing it loses.
fn join(state: &State, cluster: &[u8; 16]) -> Result<(), String> {
    let changed = state.files.with(|_| {
        let text = files::read_hosts(&state.dir)?
            .ok_or_else(|| alloc::format!("no {} -- `ipcnet init <name>` makes one", state.dir.path("hosts.toml")))?;
        let cfg = files::parse(&text)?;
        if cfg.cluster == *cluster {
            return Ok(false);
        }
        if !cfg.peers.is_empty() {
            return Err(alloc::format!("this host has peers in cluster {} already", files::hex(&cfg.cluster)));
        }
        files::write_hosts(&state.dir, &files::with_cluster(&text, cluster)?)?;
        Ok(true)
    })?;
    if !changed {
        return Ok(());
    }
    /* Out of the lock before it is dropped: the drop waits for its tasks */
    let Some(old) = state.server.with(|s| s.take()) else { return Ok(()) };
    drop(old);
    let cfg = config(state)?;
    state.server.with(|s| {
        if s.is_none() {
            *s = Some(Server::start(&state.dir, cfg)?);
        }
        Ok(())
    })
}

/// The peers made `p`'s -- the cluster's own, this host not among them, no
/// address twice -- in hosts.toml and in the running server.
fn set_peers(state: &State, p: &Peers) -> Result<(), String> {
    let mut list: Vec<PeerConf> = Vec::with_capacity(p.peers.len());
    for peer in &p.peers {
        if list.iter().any(|q| q.ip == peer.ip) {
            return Err(alloc::format!("{} is two peers' address", Ip(peer.ip)));
        }
        list.push(PeerConf { name: peer.name.clone(), ip: peer.ip, keys: peer.keys.clone(), only: peer.only.clone() });
    }
    edit_peers(state, |cfg, peers| {
        if p.cluster != cfg.cluster {
            return Err(String::from("the peers of another cluster"));
        }
        if list.iter().any(|q| q.name == cfg.name) {
            return Err(alloc::format!("{} is this host", cfg.name));
        }
        *peers = list;
        Ok(())
    })
}
