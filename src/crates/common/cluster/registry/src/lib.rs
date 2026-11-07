//! The registry of a nos cluster's hosts (plans/cluster.md): one place for
//! the hosts' keys, addresses and labels, kept by every host of the cluster
//! through raft's replicated state machine, so that a host is let in once
//! and every other host learns of it -- where ipcnet's peers used to be
//! pinned by hand on every host.
//!
//! [`State`] is what the cluster module keeps replicated, and is raft's
//! [`StateMachine`]: its log's entries are [`Command`]s, which applying
//! answers with an [`Answer`]. What it holds:
//!
//! - the cluster's id and name, made by its first host's `Init`;
//! - its hosts, in the order they joined, each with its node -- the raft
//!   member it is, never given to another host -- its key's fingerprint (two
//!   while a key changes), its addresses by what each is for (the one
//!   ipcnet calls first), its labels and whether it is being worked on;
//! - the hosts removed, kept so that a removed key and a removed node are
//!   never taken again, whatever an old copy of the list says; and the keys
//!   hosts have dropped as they moved to new ones, retired as for good;
//! - the invitations open: a host's name, the address it will come from,
//!   the key it will come with -- which every member's ipcnet pins from the
//!   invitation on, so that a host joining never meets a member that takes
//!   a key it does not know -- the hash of the secret it must prove, and
//!   when that runs out.
//!
//! Applying is the same on every member: a command carries the proposer's
//! clock (`now`), and every invitation past its time by that clock is let
//! go of before the command is looked at. No clock, no member's id and no
//! randomness of its own is read here. Every byte string it is handed --
//! an entry, a snapshot -- is read whole, every count bounded, and refused
//! or answered, never trusted: an entry that is no command is answered
//! `Malformed`, a snapshot that is no state's refused.
//!
//! The policy the cluster module reconciles raft's configuration with
//! (`voters`): the first five hosts to join vote, the rest are learners.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod command;

pub use command::{Answer, Command, Refusal};

use alloc::string::String;
use alloc::vec::Vec;

use codec::{Reader, Writer};
use raft::rsm::{Frozen, RestoreError, SnapshotReader, StateMachine};

/// A key's fingerprint: the SHA-256 of its public half, as ipcnet pins it.
pub type Fingerprint = [u8; 32];

/// Hosts and open invitations a cluster takes at once: each host is a peer
/// of every other's ipcnet, which takes 16.
pub const HOSTS_MAX: usize = 16;
/// Hosts removed the registry keeps: past this, a remove is refused rather
/// than a removed key forgotten.
pub const REMOVED_MAX: usize = 1024;
/// Keys a host may be pinned by: its own, and the one it moves to.
pub const KEYS_MAX: usize = 2;
/// Keys dropped the registry keeps: past this, a drop is refused rather
/// than a retired key forgotten.
pub const RETIRED_MAX: usize = 1024;
/// Labels a host has, and the longest.
pub const LABELS_MAX: usize = 8;
pub const LABEL_MAX: usize = 32;
/// The cluster's name: a host's name's rule.
pub const CLUSTER_NAME_MAX: usize = ipcwire::HOST_MAX;
/// Hosts that vote in the registry's group: the rest are learners.
pub const VOTERS_MAX: usize = 5;
/// The most a snapshot of the state may be: a state at its limits is a
/// fraction of it.
pub const SNAPSHOT_MAX: u64 = 256 * 1024;

/// What a snapshot of the state begins with.
const MAGIC: &[u8; 4] = b"REG2";

/// The cluster: made once, by its first host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cluster {
    pub id: [u8; 16],
    pub name: String,
    /// When it was made, by its first host's clock.
    pub created: u64,
}

/// What an address of a host is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// The one the other hosts' ipcnet calls.
    Ipc,
    /// Where a network of VMs across hosts tunnels its packets.
    Underlay,
    /// An address the world reaches the host at.
    Public,
}

const ROLES: [Role; 3] = [Role::Ipc, Role::Underlay, Role::Public];

impl Role {
    pub(crate) fn code(self) -> u8 {
        match self {
            Role::Ipc => 1,
            Role::Underlay => 2,
            Role::Public => 3,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Role> {
        ROLES.iter().copied().find(|r| r.code() == code)
    }

    /// The role a word names, as the shell and the bus have them.
    pub fn from_name(name: &str) -> Option<Role> {
        ROLES.iter().copied().find(|r| r.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Ipc => "ipc",
            Role::Underlay => "underlay",
            Role::Public => "public",
        }
    }
}

/// An address of a host's: IPv4, host order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addr {
    pub role: Role,
    pub ip: u32,
}

/// A host of the cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub name: String,
    /// The raft member it is: never another host's, ever.
    pub node: u64,
    /// One, or two while it changes its key.
    pub keys: Vec<Fingerprint>,
    /// Its `Ipc` address first, and the rest each of a role of its own.
    pub addrs: Vec<Addr>,
    pub labels: Vec<String>,
    /// Being worked on: every service of the cluster holds off counting it
    /// out, and places nothing new on it.
    pub maintenance: bool,
    /// When it joined, by the proposer's clock.
    pub joined: u64,
}

impl Host {
    /// The address the other hosts' ipcnet calls.
    pub fn ipc(&self) -> u32 {
        self.addrs.first().map_or(0, |a| a.ip)
    }
}

/// A host removed: its name may be a new host's, its keys and its node are
/// never anybody's again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removed {
    pub name: String,
    pub node: u64,
    pub keys: Vec<Fingerprint>,
    pub removed: u64,
}

/// A key a host dropped as it moved to another: never taken again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retired {
    pub key: Fingerprint,
    /// The node of the host it was.
    pub node: u64,
    pub retired: u64,
}

/// An invitation open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invitation {
    pub id: u64,
    pub host: String,
    pub ip: u32,
    /// The key the host will come with: its ipcnet's.
    pub key: Fingerprint,
    pub secret_hash: [u8; 32],
    pub expires: u64,
}

/// A peer of a host's ipcnet, as the registry has it: every other host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub ip: u32,
    pub keys: Vec<Fingerprint>,
}

/// The registry: what its log's entries make.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct State {
    cluster: Option<Cluster>,
    /// In the order they joined: by node.
    hosts: Vec<Host>,
    /// In the order they were removed.
    removed: Vec<Removed>,
    /// In the order they were made.
    invitations: Vec<Invitation>,
    /// In the order they were dropped.
    retired: Vec<Retired>,
    /// The node the next host joining gets: 0 before the cluster is made.
    next_node: u64,
}

/// Whether `name` is a cluster's or a host's name.
pub fn valid_name(name: &str) -> bool {
    ipcwire::valid_host(name)
}

/// Whether `label` is a label: a name's letters, up to `LABEL_MAX`.
pub fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= LABEL_MAX
        && label.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

impl State {
    pub fn new() -> State {
        State::default()
    }

    pub fn cluster(&self) -> Option<&Cluster> {
        self.cluster.as_ref()
    }

    pub fn hosts(&self) -> &[Host] {
        &self.hosts
    }

    pub fn host(&self, name: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.name == name)
    }

    pub fn host_of_node(&self, node: u64) -> Option<&Host> {
        self.hosts.iter().find(|h| h.node == node)
    }

    /// The host one of whose keys `key` is.
    pub fn host_of_key(&self, key: &Fingerprint) -> Option<&Host> {
        self.hosts.iter().find(|h| h.keys.contains(key))
    }

    pub fn removed(&self) -> &[Removed] {
        &self.removed
    }

    /// Whether `key` is a removed host's, or one a host dropped: never
    /// taken again.
    pub fn key_removed(&self, key: &Fingerprint) -> bool {
        self.removed.iter().any(|r| r.keys.contains(key)) || self.retired.iter().any(|r| r.key == *key)
    }

    pub fn retired(&self) -> &[Retired] {
        &self.retired
    }

    pub fn invitations(&self) -> &[Invitation] {
        &self.invitations
    }

    pub fn invitation(&self, id: u64) -> Option<&Invitation> {
        self.invitations.iter().find(|i| i.id == id)
    }

    /// The nodes that vote in the registry's group: the first `VOTERS_MAX`
    /// hosts to join.
    pub fn voters(&self) -> Vec<u64> {
        self.hosts.iter().take(VOTERS_MAX).map(|h| h.node).collect()
    }

    /// The nodes that learn: every host past the voters.
    pub fn learners(&self) -> Vec<u64> {
        self.hosts.iter().skip(VOTERS_MAX).map(|h| h.node).collect()
    }

    /// Host `me`'s peers: every other host, by its `Ipc` address and keys,
    /// and every host invited, by the address and the key it will come
    /// with -- pinned before it comes, so that its join meets a member that
    /// knows it.
    pub fn peers_of(&self, me: &str) -> Vec<Peer> {
        let hosts = self.hosts.iter().filter(|h| h.name != me)
            .map(|h| Peer { name: h.name.clone(), ip: h.ipc(), keys: h.keys.clone() });
        let invited = self.invitations.iter().filter(|i| i.host != me)
            .map(|i| Peer { name: i.host.clone(), ip: i.ip, keys: alloc::vec![i.key] });
        hosts.chain(invited).collect()
    }

    /// Whether `key` is a host's or an open invitation's.
    fn key_in_use(&self, key: &Fingerprint) -> bool {
        self.host_of_key(key).is_some() || self.invitations.iter().any(|i| i.key == *key)
    }

    /// `command`, applied: what its proposer is answered.
    pub fn apply_command(&mut self, command: &Command) -> Answer {
        let now = command.now();
        /* Every invitation past its time by this entry's clock let go of
           first, on every member alike */
        self.invitations.retain(|i| i.expires > now);
        match self.change(command) {
            Ok(node) => Answer::Done { node },
            Err(why) => Answer::Refused(why),
        }
    }

    fn change(&mut self, command: &Command) -> Result<u64, Refusal> {
        if let Command::Init { now, cluster, name, host, key, ip } = command {
            if self.cluster.is_some() {
                return Err(Refusal::Initialized);
            }
            if !valid_name(name) || !valid_name(host) {
                return Err(Refusal::BadName);
            }
            if *ip == 0 {
                return Err(Refusal::BadAddress);
            }
            self.cluster = Some(Cluster { id: *cluster, name: name.clone(), created: *now });
            self.hosts.push(Host {
                name: host.clone(),
                node: 1,
                keys: alloc::vec![*key],
                addrs: alloc::vec![Addr { role: Role::Ipc, ip: *ip }],
                labels: Vec::new(),
                maintenance: false,
                joined: *now,
            });
            self.next_node = 2;
            return Ok(1);
        }
        if self.cluster.is_none() {
            return Err(Refusal::NotInitialized);
        }
        match command {
            Command::Init { .. } => Err(Refusal::Initialized),
            Command::Invite { now, id, host, ip, key, secret_hash, expires } => {
                if !valid_name(host) {
                    return Err(Refusal::BadName);
                }
                if *ip == 0 {
                    return Err(Refusal::BadAddress);
                }
                if self.host(host).is_some() || self.invitations.iter().any(|i| i.host == *host) {
                    return Err(Refusal::HostExists);
                }
                if self.invitation(*id).is_some() {
                    return Err(Refusal::InvitationExists);
                }
                if self.hosts.iter().any(|h| h.addrs.iter().any(|a| a.ip == *ip))
                    || self.invitations.iter().any(|i| i.ip == *ip)
                {
                    return Err(Refusal::AddressInUse);
                }
                if self.key_in_use(key) {
                    return Err(Refusal::KeyInUse);
                }
                if self.key_removed(key) {
                    return Err(Refusal::KeyRemoved);
                }
                if *expires <= *now {
                    return Err(Refusal::Expired);
                }
                if self.hosts.len() + self.invitations.len() >= HOSTS_MAX {
                    return Err(Refusal::Full);
                }
                self.invitations.push(Invitation {
                    id: *id,
                    host: host.clone(),
                    ip: *ip,
                    key: *key,
                    secret_hash: *secret_hash,
                    expires: *expires,
                });
                Ok(0)
            }
            Command::Join { now, invitation } => {
                let at = self.invitations.iter().position(|i| i.id == *invitation)
                    .ok_or(Refusal::InvitationGone)?;
                /* Its key was checked as it was invited, and kept from any
                   other invitation since: looked at again all the same */
                let key = self.invitations[at].key;
                if self.host_of_key(&key).is_some() {
                    return Err(Refusal::KeyInUse);
                }
                if self.key_removed(&key) {
                    return Err(Refusal::KeyRemoved);
                }
                let inv = self.invitations.remove(at);
                let node = self.next_node;
                self.next_node += 1;
                self.hosts.push(Host {
                    name: inv.host,
                    node,
                    keys: alloc::vec![inv.key],
                    addrs: alloc::vec![Addr { role: Role::Ipc, ip: inv.ip }],
                    labels: Vec::new(),
                    maintenance: false,
                    joined: *now,
                });
                Ok(node)
            }
            Command::Remove { now, host } => {
                let at = self.hosts.iter().position(|h| h.name == *host).ok_or(Refusal::NoSuchHost)?;
                if self.hosts.len() == 1 {
                    return Err(Refusal::LastHost);
                }
                if self.removed.len() >= REMOVED_MAX {
                    return Err(Refusal::Full);
                }
                let h = self.hosts.remove(at);
                let node = h.node;
                self.removed.push(Removed { name: h.name, node, keys: h.keys, removed: *now });
                Ok(node)
            }
            Command::Revoke { invitation, .. } => {
                let at = self.invitations.iter().position(|i| i.id == *invitation)
                    .ok_or(Refusal::InvitationGone)?;
                self.invitations.remove(at);
                Ok(0)
            }
            /* Each of these asked again answers as it did: what it asks for
               is already so */
            Command::Maintenance { host, on, .. } => {
                let h = self.hosts.iter_mut().find(|h| h.name == *host).ok_or(Refusal::NoSuchHost)?;
                h.maintenance = *on;
                Ok(h.node)
            }
            Command::Address { host, role, ip, .. } => {
                let at = self.hosts.iter().position(|h| h.name == *host).ok_or(Refusal::NoSuchHost)?;
                if *ip == 0 && *role == Role::Ipc {
                    return Err(Refusal::BadAddress);
                }
                let taken = self.hosts.iter().enumerate().any(|(i, h)| i != at && h.addrs.iter().any(|a| a.ip == *ip))
                    || self.invitations.iter().any(|i| i.ip == *ip);
                if *ip != 0 && taken {
                    return Err(Refusal::AddressInUse);
                }
                let h = &mut self.hosts[at];
                h.addrs.retain(|a| a.role != *role);
                if *ip != 0 {
                    h.addrs.push(Addr { role: *role, ip: *ip });
                }
                /* The ipc address first, the rest by role */
                h.addrs.sort_by_key(|a| a.role);
                Ok(h.node)
            }
            Command::AddKey { host, key, .. } => {
                let at = self.hosts.iter().position(|h| h.name == *host).ok_or(Refusal::NoSuchHost)?;
                if self.hosts[at].keys.contains(key) {
                    return Ok(self.hosts[at].node);
                }
                if self.key_in_use(key) {
                    return Err(Refusal::KeyInUse);
                }
                if self.key_removed(key) {
                    return Err(Refusal::KeyRemoved);
                }
                let h = &mut self.hosts[at];
                if h.keys.len() >= KEYS_MAX {
                    return Err(Refusal::KeysFull);
                }
                h.keys.push(*key);
                Ok(h.node)
            }
            Command::DropKey { now, host, key } => {
                let at = self.hosts.iter().position(|h| h.name == *host).ok_or(Refusal::NoSuchHost)?;
                let node = self.hosts[at].node;
                if !self.hosts[at].keys.contains(key) {
                    return if self.retired.iter().any(|r| r.key == *key && r.node == node) {
                        Ok(node)
                    } else {
                        Err(Refusal::NotItsKey)
                    };
                }
                if self.hosts[at].keys.len() == 1 {
                    return Err(Refusal::LastKey);
                }
                if self.retired.len() >= RETIRED_MAX {
                    return Err(Refusal::Full);
                }
                self.hosts[at].keys.retain(|k| k != key);
                self.retired.push(Retired { key: *key, node, retired: *now });
                Ok(node)
            }
        }
    }

    /* ---- the state as bytes: a snapshot's, and a read's answer ---- */

    /// The state, as a snapshot holds it.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.fixed(MAGIC);
        match &self.cluster {
            Some(c) => {
                w.u8(1);
                w.fixed(&c.id);
                w.str(&c.name);
                w.u64(c.created);
            }
            None => w.u8(0),
        }
        w.u64(self.next_node);
        w.u32(self.hosts.len() as u32);
        for h in &self.hosts {
            w.str(&h.name);
            w.u64(h.node);
            w.u8(h.keys.len() as u8);
            for k in &h.keys {
                w.fixed(k);
            }
            w.u8(h.addrs.len() as u8);
            for a in &h.addrs {
                w.u8(a.role.code());
                w.u32(a.ip);
            }
            w.u8(h.labels.len() as u8);
            for l in &h.labels {
                w.str(l);
            }
            w.u8(u8::from(h.maintenance));
            w.u64(h.joined);
        }
        w.u32(self.removed.len() as u32);
        for r in &self.removed {
            w.str(&r.name);
            w.u64(r.node);
            w.u8(r.keys.len() as u8);
            for k in &r.keys {
                w.fixed(k);
            }
            w.u64(r.removed);
        }
        w.u32(self.invitations.len() as u32);
        for i in &self.invitations {
            w.u64(i.id);
            w.str(&i.host);
            w.u32(i.ip);
            w.fixed(&i.key);
            w.fixed(&i.secret_hash);
            w.u64(i.expires);
        }
        w.u32(self.retired.len() as u32);
        for r in &self.retired {
            w.fixed(&r.key);
            w.u64(r.node);
            w.u64(r.retired);
        }
        w.buf
    }

    /// The state `bytes` are -- as `encode` wrote one, and holding to every
    /// rule applying keeps -- or why not.
    pub fn decode(bytes: &[u8]) -> Result<State, RestoreError> {
        let bad = RestoreError("not a registry's state");
        let mut r = Reader::new(bytes);
        if &r.fixed::<4>().ok_or(bad)? != MAGIC {
            return Err(RestoreError("not a registry's snapshot"));
        }
        let mut s = State::new();
        s.cluster = match r.u8().ok_or(bad)? {
            0 => None,
            1 => {
                let id = r.fixed().ok_or(bad)?;
                let name = r.str(CLUSTER_NAME_MAX).ok_or(bad)?;
                if !valid_name(name) {
                    return Err(RestoreError("the cluster's name is none"));
                }
                Some(Cluster { id, name: String::from(name), created: r.u64().ok_or(bad)? })
            }
            _ => return Err(bad),
        };
        s.next_node = r.u64().ok_or(bad)?;
        let hosts = r.u32().ok_or(bad)? as usize;
        if hosts > HOSTS_MAX {
            return Err(RestoreError("more hosts than a cluster takes"));
        }
        for _ in 0..hosts {
            s.hosts.push(read_host(&mut r)?);
        }
        let removed = r.u32().ok_or(bad)? as usize;
        if removed > REMOVED_MAX {
            return Err(RestoreError("more hosts removed than are kept"));
        }
        for _ in 0..removed {
            let name = r.str(ipcwire::HOST_MAX).ok_or(bad)?;
            let node = r.u64().ok_or(bad)?;
            let keys = read_keys(&mut r)?;
            let at = r.u64().ok_or(bad)?;
            if !valid_name(name) {
                return Err(RestoreError("a removed host's name is none"));
            }
            s.removed.push(Removed { name: String::from(name), node, keys, removed: at });
        }
        let invitations = r.u32().ok_or(bad)? as usize;
        if invitations > HOSTS_MAX {
            return Err(RestoreError("more invitations than a cluster takes"));
        }
        for _ in 0..invitations {
            let id = r.u64().ok_or(bad)?;
            let host = r.str(ipcwire::HOST_MAX).ok_or(bad)?;
            let ip = r.u32().ok_or(bad)?;
            let key = r.fixed().ok_or(bad)?;
            let secret_hash = r.fixed().ok_or(bad)?;
            let expires = r.u64().ok_or(bad)?;
            if !valid_name(host) || ip == 0 {
                return Err(RestoreError("an invitation of no host or no address"));
            }
            s.invitations.push(Invitation { id, host: String::from(host), ip, key, secret_hash, expires });
        }
        let retired = r.u32().ok_or(bad)? as usize;
        if retired > RETIRED_MAX {
            return Err(RestoreError("more keys retired than are kept"));
        }
        for _ in 0..retired {
            let key = r.fixed().ok_or(bad)?;
            let node = r.u64().ok_or(bad)?;
            let at = r.u64().ok_or(bad)?;
            s.retired.push(Retired { key, node, retired: at });
        }
        if !r.done() {
            return Err(RestoreError("bytes past the registry's state"));
        }
        s.check()?;
        Ok(s)
    }

    /// Whether the state holds to every rule applying keeps -- what a
    /// snapshot read back is held to.
    fn check(&self) -> Result<(), RestoreError> {
        if self.cluster.is_none() {
            return if self.hosts.is_empty() && self.removed.is_empty() && self.invitations.is_empty()
                && self.retired.is_empty() && self.next_node == 0
            {
                Ok(())
            } else {
                Err(RestoreError("hosts in a registry with no cluster"))
            };
        }
        if self.hosts.is_empty() {
            return Err(RestoreError("a cluster with no host"));
        }
        if self.hosts.len() + self.invitations.len() > HOSTS_MAX {
            return Err(RestoreError("more hosts and invitations than a cluster takes"));
        }
        /* Nodes: the hosts' in the order they joined, every one -- removed
           too -- below the next, none twice */
        let mut nodes: Vec<u64> = self.hosts.iter().map(|h| h.node).chain(self.removed.iter().map(|r| r.node))
            .collect();
        if self.hosts.windows(2).any(|w| w[0].node >= w[1].node) {
            return Err(RestoreError("hosts out of the order they joined"));
        }
        if nodes.iter().any(|&n| n == 0 || n >= self.next_node) {
            return Err(RestoreError("a node past the next"));
        }
        nodes.sort_unstable();
        if nodes.windows(2).any(|w| w[0] == w[1]) {
            return Err(RestoreError("a node twice"));
        }
        /* Names and addresses: a host's or an invitation's, each once */
        for (i, h) in self.hosts.iter().enumerate() {
            if self.hosts[..i].iter().any(|o| o.name == h.name) || self.invitations.iter().any(|v| v.host == h.name) {
                return Err(RestoreError("a host's name twice"));
            }
            for a in &h.addrs {
                let taken = self.hosts[..i].iter().any(|o| o.addrs.iter().any(|b| b.ip == a.ip))
                    || self.invitations.iter().any(|v| v.ip == a.ip);
                if taken {
                    return Err(RestoreError("an address twice"));
                }
            }
        }
        for (i, v) in self.invitations.iter().enumerate() {
            let twice = self.invitations[..i].iter()
                .any(|o| o.id == v.id || o.host == v.host || o.ip == v.ip || o.key == v.key);
            if twice || self.host_of_key(&v.key).is_some() || self.key_removed(&v.key) {
                return Err(RestoreError("an invitation twice, or of a key in use or removed"));
            }
        }
        /* Retired keys: each once, a node's that was, never a removed
           host's -- what a host still has, or an invitation, the loop below
           refuses */
        for (i, r) in self.retired.iter().enumerate() {
            if self.retired[..i].iter().any(|o| o.key == r.key) || r.node == 0 || r.node >= self.next_node
                || self.removed.iter().any(|x| x.keys.contains(&r.key))
            {
                return Err(RestoreError("a key retired twice, of no node, or a removed host's"));
            }
        }
        /* Keys: each host's its own, none a removed host's nor retired */
        for (i, h) in self.hosts.iter().enumerate() {
            for k in &h.keys {
                let twice = self.hosts[..i].iter().any(|o| o.keys.contains(k))
                    || h.keys.iter().filter(|x| *x == k).count() > 1;
                if twice || self.key_removed(k) {
                    return Err(RestoreError("a key twice, or a removed host's"));
                }
            }
        }
        Ok(())
    }
}

fn read_keys(r: &mut Reader<'_>) -> Result<Vec<Fingerprint>, RestoreError> {
    let bad = RestoreError("not a registry's state");
    let n = usize::from(r.u8().ok_or(bad)?);
    if n == 0 || n > KEYS_MAX {
        return Err(RestoreError("a host of no key, or of more than it may have"));
    }
    let mut keys = Vec::with_capacity(n);
    for _ in 0..n {
        keys.push(r.fixed().ok_or(bad)?);
    }
    Ok(keys)
}

fn read_host(r: &mut Reader<'_>) -> Result<Host, RestoreError> {
    let bad = RestoreError("not a registry's state");
    let name = r.str(ipcwire::HOST_MAX).ok_or(bad)?;
    if !valid_name(name) {
        return Err(RestoreError("a host's name is none"));
    }
    let node = r.u64().ok_or(bad)?;
    let keys = read_keys(r)?;
    let n = usize::from(r.u8().ok_or(bad)?);
    if n == 0 || n > ROLES.len() {
        return Err(RestoreError("a host of no address, or of more than it may have"));
    }
    let mut addrs: Vec<Addr> = Vec::with_capacity(n);
    for k in 0..n {
        let role = Role::from_code(r.u8().ok_or(bad)?).ok_or(RestoreError("an address of no role"))?;
        let ip = r.u32().ok_or(bad)?;
        /* The ipc address first, each role once, none 0 */
        if (k == 0) != (role == Role::Ipc) || addrs.iter().any(|a| a.role == role) || ip == 0 {
            return Err(RestoreError("a host's addresses out of rule"));
        }
        addrs.push(Addr { role, ip });
    }
    let n = usize::from(r.u8().ok_or(bad)?);
    if n > LABELS_MAX {
        return Err(RestoreError("more labels than a host may have"));
    }
    let mut labels = Vec::with_capacity(n);
    for _ in 0..n {
        let l = r.str(LABEL_MAX).ok_or(bad)?;
        if !valid_label(l) || labels.iter().any(|x: &String| x == l) {
            return Err(RestoreError("a label that is none, or twice"));
        }
        labels.push(String::from(l));
    }
    let maintenance = match r.u8().ok_or(bad)? {
        0 => false,
        1 => true,
        _ => return Err(bad),
    };
    let joined = r.u64().ok_or(bad)?;
    Ok(Host { name: String::from(name), node, keys, addrs, labels, maintenance, joined })
}

/// A state frozen: its bytes, made at the freeze, handed out a piece at a
/// time.
pub struct FrozenState {
    bytes: Vec<u8>,
    at: usize,
}

impl Frozen for FrozenState {
    fn write(&mut self, max: usize, out: &mut Vec<u8>) {
        let n = max.min(self.bytes.len() - self.at);
        out.extend_from_slice(&self.bytes[self.at..self.at + n]);
        self.at += n;
    }
}

impl StateMachine for State {
    type Frozen = FrozenState;

    fn apply(&mut self, _index: u64, command: &[u8]) -> Vec<u8> {
        let answer = match Command::decode(command) {
            Some(c) => self.apply_command(&c),
            None => Answer::Refused(Refusal::Malformed),
        };
        answer.encode()
    }

    /// Any query: the state as it is, as a snapshot holds it.
    fn query(&self, _query: &[u8]) -> Vec<u8> {
        self.encode()
    }

    fn size(&self) -> u64 {
        /* The state is small: its bytes counted roughly -- a host's, a
           removed host's and an invitation's at their most */
        (self.hosts.len() * 512 + self.removed.len() * 160 + self.invitations.len() * 128 + self.retired.len() * 48
         + 128) as u64
    }

    fn freeze(&self) -> FrozenState {
        FrozenState { bytes: self.encode(), at: 0 }
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>) -> Result<(), RestoreError> {
        let len = r.remaining();
        if len > SNAPSHOT_MAX {
            return Err(RestoreError("a registry's snapshot larger than any state"));
        }
        let mut bytes = alloc::vec![0u8; len as usize];
        if !r.read_exact(&mut bytes) {
            return Err(RestoreError("a registry's snapshot cut short"));
        }
        *self = State::decode(&bytes)?;
        Ok(())
    }
}
