//! The cluster registry as vnet reads it: through the management bus, the
//! `cluster` provider's `hosts` -- a module reaches another's only so. A host
//! is the registry's node in vnet's state (plans/vnet.md, "Decisions": a
//! removed host's name may be a new host's, its node never), and its
//! addresses are what no network or public address of vnet's may cover.
//!
//! An ask that fails is said, never taken for a registry of no hosts.

use alloc::string::String;
use alloc::vec::Vec;

use kcore::mgmt::Status;

/// The most of an answer taken: sixteen hosts, each a few hundred bytes.
const ANSWER_MAX: usize = 16 * 1024;

/// A host of the registry, as vnet needs it.
#[derive(Clone, Debug)]
pub struct Host {
    pub name: String,
    pub node: u64,
    /// Every address it has -- ipc, underlay, public -- none of which a
    /// network or a public address of vnet's may cover.
    pub addresses: Vec<u32>,
    /// The address the tunnel reaches it at (plans/vnet.md, "The tunnel"):
    /// its underlay's, or its ipc's where it has none of its own; 0 for
    /// none.
    pub underlay: u32,
    /// And the port, UDP: that address's where it has one of its own --
    /// one of two hosts behind a NAT's address -- else 0, the tunnel's own.
    pub port: u16,
}

/// The registry's hosts, as this host's member of it has them now.
pub fn hosts() -> Result<Vec<Host>, String> {
    let mut answer = Vec::new();
    let status = kcore::mgmt::call("cluster", "hosts", b"{}", ANSWER_MAX, &mut |b: &[u8]| {
        answer.extend_from_slice(b);
        true
    });
    let value = mgmtwire::json::parse(&answer, &mgmtwire::Limits::PARAMS).ok();
    match status {
        Status::Ok => {}
        Status::NoProvider => {
            return Err(String::from("no cluster registry on this host -- `insmod /cluster.ko` first"));
        }
        s => {
            let said = value.as_ref().and_then(|v| v.get("error")).and_then(|e| e.get("message"))
                .and_then(|m| m.as_str()).map(String::from);
            return Err(alloc::format!("the cluster registry could not be asked: {}",
                                      said.unwrap_or_else(|| alloc::format!("{:?}", s))));
        }
    }
    let list = value.as_ref().and_then(|v| v.get("hosts")).and_then(|h| h.as_list())
        .ok_or_else(|| String::from("the cluster registry answered with no hosts' list"))?;
    let mut hosts = Vec::new();
    for h in list {
        let (Some(name), Some(node)) = (h.get("name").and_then(|n| n.as_str()), h.get("node").and_then(|n| n.as_u64()))
        else {
            return Err(String::from("the cluster registry answered a host of no name or node"));
        };
        let ip = |key: &str| h.get(key).and_then(|v| v.as_str()).and_then(|t| netwire::parse_ipv4(t.as_bytes()));
        let addresses: Vec<u32> = ["ip", "underlay", "public"].iter().filter_map(|k| ip(k)).filter(|&a| a != 0).collect();
        let port = |key: &str| h.get(key).and_then(|v| v.as_u64()).and_then(|p| u16::try_from(p).ok()).unwrap_or(0);
        let (underlay, port) = match ip("underlay").filter(|&a| a != 0) {
            Some(a) => (a, port("underlay_port")),
            None => (ip("ip").unwrap_or(0), port("port")),
        };
        hosts.push(Host { name: String::from(name), node, addresses, underlay, port });
    }
    Ok(hosts)
}

/// The node host `name` is in the registry.
pub fn node_of(hosts: &[Host], name: &str) -> Result<u64, String> {
    hosts.iter().find(|h| h.name == name).map(|h| h.node)
        .ok_or_else(|| alloc::format!("{} is no host of the cluster registry", name))
}

/// The name of the host node `node` is, as the registry has it -- or the
/// node, said as one the registry has not.
pub fn name_of(hosts: &[Host], node: u64) -> String {
    match hosts.iter().find(|h| h.node == node) {
        Some(h) => h.name.clone(),
        None => alloc::format!("node {} (no host of the registry's)", node),
    }
}
