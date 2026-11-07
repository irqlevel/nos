//! What the module keeps in its folder -- /nbs/mds unless `insmod
//! nbs-mds.ko dir=` names another:
//!
//! - `member`, which member of which NBS cluster this host is: its
//!   incarnation, the cluster's id, this host's name and the cluster's name
//!   on its first line; then, for a host the cluster was offered to, a line
//!   for each member it was offered by -- a host and its incarnation -- what
//!   it knows the members by until its log has caught up. Written once, when
//!   the host makes the cluster or takes an offer, and never written over:
//!   the mark that this host is a member, so that a log in `raft/` with no
//!   `member` beside it is what an `init` or an offer that did not finish
//!   left, and the next one clears it;
//! - `raft/`, the member's log (`raft::log::DiskLog`), over raftmember's
//!   `KFiles`;
//! - `.sync`, an empty file a sync of the folder syncs.
//!
//! A host whose folder is lost comes back with none, and is offered the
//! cluster again as a new member: never the one it was.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use kcore::error::Error;
use raftmember::KFiles;

pub const DEFAULT_DIR: &str = "/nbs/mds";
/// The member file's most bytes: its first line, and a line of each member
/// of as many as a cluster has.
const MEMBER_MAX: usize = 256 + nbsmeta::MEMBERS_MAX * 96;

/// A member as an offer named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Known {
    pub host: String,
    pub id: u64,
}

/// This host's member: what its member file says.
#[derive(Clone, Debug)]
pub struct Member {
    /// Its incarnation: its raft node.
    pub id: u64,
    pub cluster: [u8; 16],
    /// This host's ipc name.
    pub host: String,
    /// The cluster's name.
    pub name: String,
    /// The members the cluster was offered by, for a host that took an
    /// offer: none for the host that made it.
    pub known: Vec<Known>,
}

fn path(dir: &str) -> String {
    alloc::format!("{}/member", dir)
}

/// The member file, if there is one: None for a host in no NBS cluster.
pub fn read_member(dir: &str) -> Result<Option<Member>, String> {
    let path = path(dir);
    let bytes = match kcore::fs::read(&path, MEMBER_MAX) {
        Ok(b) => b,
        Err(Error::NotFound) => return Ok(None),
        Err(_) => return Err(alloc::format!("cannot read {}", path)),
    };
    let text = core::str::from_utf8(&bytes).map_err(|_| alloc::format!("{} is not text", path))?;
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    let words: Vec<&str> = first.split_whitespace().collect();
    let bad = || alloc::format!("{}: \"{}\" is not `<incarnation> <cluster> <host> <name>`", path, first);
    let [id, cluster, host, name] = words.as_slice() else { return Err(bad()) };
    let id: u64 = id.parse().map_err(|_| bad())?;
    let cluster = codec::unhex::<16>(cluster).ok_or_else(bad)?;
    if id == 0 || !nbsmeta::valid_name(host) || !nbsmeta::valid_name(name) {
        return Err(bad());
    }
    let mut known = Vec::new();
    for line in lines.filter(|l| !l.trim().is_empty()) {
        let bad = || alloc::format!("{}: \"{}\" is not `<host> <incarnation>`", path, line);
        let words: Vec<&str> = line.split_whitespace().collect();
        let [h, n] = words.as_slice() else { return Err(bad()) };
        let n: u64 = n.parse().map_err(|_| bad())?;
        if n == 0 || !nbsmeta::valid_name(h) || known.len() >= nbsmeta::MEMBERS_MAX {
            return Err(bad());
        }
        known.push(Known { host: String::from(*h), id: n });
    }
    Ok(Some(Member { id, cluster, host: String::from(*host), name: String::from(*name), known }))
}

/// The member file written, once and synced: refused where there is one. A
/// write that failed part way leaves none.
pub fn create_member(dir: &str, m: &Member) -> Result<(), String> {
    raftmember::create_dirs(dir);
    let mut text = alloc::format!("{} {} {} {}\n", m.id, codec::hex(&m.cluster), m.host, m.name);
    for k in &m.known {
        let _ = writeln!(text, "{} {}", k.host, k.id);
    }
    let path = path(dir);
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
pub fn remove_member(dir: &str) -> Result<(), String> {
    let path = path(dir);
    match kcore::fs::remove(&path) {
        Ok(()) | Err(Error::NotFound) => Ok(()),
        Err(_) => Err(alloc::format!("cannot remove {}", path)),
    }
}

/// Every file of `raft/` removed: what an `init` or an offer that did not
/// finish left, which a member file does not name -- never a member's log.
pub fn clear_log(dir: &str) -> Result<(), String> {
    if read_member(dir)?.is_some() {
        return Err(alloc::format!("{} names a member: its log is not cleared", path(dir)));
    }
    KFiles::new(dir, "raft")?.clear()
}
