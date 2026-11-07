//! What the module keeps in its folder -- /etc/cluster unless `insmod
//! cluster.ko conf=` names another:
//!
//! - `member`, which member of which cluster this host is: its raft node,
//!   the cluster's id and this host's name on its first line; for a host
//!   that joined, a line for each host its join was answered with -- its
//!   name, node, address and keys -- what it reaches the cluster by until
//!   its log has caught up. Written once, when the host makes the cluster or
//!   joins it, and never written over: the mark that this host is a member,
//!   so that a log in `raft/` with no `member` beside it is what an `init`
//!   or a `join` that did not finish left, and the next one clears it;
//! - `raft/`, the member's log (`raft::log::DiskLog`), over raftmember's
//!   `KFiles`;
//! - `.sync`, an empty file a sync of the directory syncs: nos's file
//!   system makes everything durable at any file's sync.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

use kcore::error::Error;
use raftmember::KFiles;

pub const DEFAULT_DIR: &str = "/etc/cluster";
/// The member file's most bytes: its first line, and a line of each host
/// -- a name, a node, an address and two keys -- of as many as a cluster has.
const MEMBER_MAX: usize = 128 + registry::HOSTS_MAX * 256;

/// The folder's paths.
#[derive(Clone)]
pub struct Dir {
    pub root: String,
}

impl Dir {
    pub fn new(root: &str) -> Dir {
        Dir { root: root.trim_end_matches('/').to_string() }
    }

    pub fn path(&self, name: &str) -> String {
        alloc::format!("{}/{}", self.root, name)
    }
}

/// Which member of which cluster this host is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub node: u64,
    pub cluster: [u8; 16],
    pub name: String,
    /// For a host that joined, the hosts as its join was answered -- itself
    /// among them; none for the host that made the cluster.
    pub hosts: Vec<Known>,
}

/// A host as a join was answered with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Known {
    pub name: String,
    pub node: u64,
    pub ip: u32,
    pub keys: Vec<[u8; 32]>,
}

pub use codec::{hex, unhex};

/// The member file, if there is one: None for a host in no cluster.
pub fn read_member(dir: &Dir) -> Result<Option<Member>, String> {
    let path = dir.path("member");
    let bytes = match kcore::fs::read(&path, MEMBER_MAX) {
        Ok(b) => b,
        Err(Error::NotFound) => return Ok(None),
        Err(_) => return Err(alloc::format!("cannot read {}", path)),
    };
    let text = core::str::from_utf8(&bytes).map_err(|_| alloc::format!("{} is not text", path))?;
    let mut lines = text.lines();
    let first: Vec<&str> = lines.next().unwrap_or("").split_whitespace().collect();
    let [node, cluster, name] = first.as_slice() else {
        return Err(alloc::format!("{} does not begin `<node> <cluster> <name>`", path));
    };
    let node: u64 = node.parse().map_err(|_| alloc::format!("{}: \"{}\" is no node", path, node))?;
    let cluster = unhex::<16>(cluster).ok_or_else(|| alloc::format!("{}: \"{}\" is no cluster's id", path, cluster))?;
    if node == 0 || !registry::valid_name(name) {
        return Err(alloc::format!("{}: node 0, or \"{}\" no host's name", path, name));
    }
    let mut hosts: Vec<Known> = Vec::new();
    for line in lines.filter(|l| !l.trim().is_empty()) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let bad = || alloc::format!("{}: \"{}\" is not `<host> <node> <ip> <key> [<key>]`", path, line);
        let [host, n, ip, keys @ ..] = words.as_slice() else { return Err(bad()) };
        let n: u64 = n.parse().map_err(|_| bad())?;
        let ip = crate::parse_ip(ip).ok_or_else(bad)?;
        let keys: Option<Vec<[u8; 32]>> = keys.iter().map(|k| unhex::<32>(k)).collect();
        let keys = keys.ok_or_else(bad)?;
        if n == 0 || !registry::valid_name(host) || keys.is_empty() || keys.len() > registry::KEYS_MAX
            || hosts.len() == registry::HOSTS_MAX || hosts.iter().any(|h| h.name == *host || h.node == n)
        {
            return Err(bad());
        }
        hosts.push(Known { name: String::from(*host), node: n, ip, keys });
    }
    Ok(Some(Member { node, cluster, name: String::from(*name), hosts }))
}

/// The member file written, once and synced: refused where there is one.
/// A write that failed part way leaves none.
pub fn create_member(dir: &Dir, m: &Member) -> Result<(), String> {
    let _ = kcore::fs::create_dir(&dir.root);
    let mut text = alloc::format!("{} {} {}\n", m.node, hex(&m.cluster), m.name);
    for h in &m.hosts {
        let _ = write!(text, "{} {} {}", h.name, h.node, crate::Ip(h.ip));
        for k in &h.keys {
            let _ = write!(text, " {}", hex(k));
        }
        text.push('\n');
    }
    let path = dir.path("member");
    match kcore::fs::create(&path, text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(Error::Busy) => Err(alloc::format!("{} is there already", path)),
        Err(_) => {
            let _ = kcore::fs::remove(&path);
            Err(alloc::format!("cannot write {}", path))
        }
    }
}

/// The member file taken away: an `init` undone.
pub fn remove_member(dir: &Dir) -> Result<(), String> {
    let path = dir.path("member");
    match kcore::fs::remove(&path) {
        Ok(()) | Err(Error::NotFound) => Ok(()),
        Err(_) => Err(alloc::format!("cannot remove {}", path)),
    }
}

/// Every file of `raft/` removed: what an `init` or a `join` that did not
/// finish left, which a member file does not name -- never a member's log.
pub fn clear_log(dir: &Dir) -> Result<(), String> {
    if read_member(dir)?.is_some() {
        return Err(alloc::format!("{} names a member: its log is not cleared", dir.path("member")));
    }
    KFiles::new(&dir.root, "raft")?.clear()
}
