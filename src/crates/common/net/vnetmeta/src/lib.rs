//! vnet's state (plans/vnet.md, "The state"): what every host's member of
//! the vnet group keeps through raft's replicated state machine, and the
//! commands that change it.
//!
//! - **The group and its members**: as every group whose hosts are offered
//!   it keeps them (`members`) -- each a host of the cluster registry and
//!   the incarnation its member made; the first five vote.
//! - **Networks**, 256 at most: a name, a number (the VNI, 24 bits, from a
//!   counter, never given again), a private range's subnet of /16 to /28 --
//!   apart from every other network's and from hv's own 10.0.100.0/24 -- its
//!   MTU, 576 to 1440, whether its VMs go out to the world, and where its
//!   next address is looked for from: the next free one round the subnet,
//!   so that an address let go is not the next one given. Its gateway is
//!   its first address, on every host.
//! - **Ports**, 4096 at most: a VM's NIC's -- its UUID and the NIC -- on a
//!   network, at an address, on the registry's node of the host it runs on,
//!   with a generation from one counter for the cluster, which a move takes
//!   anew, and a firewall of the cluster's.
//! - **Public addresses**: each routed by its provider to a host -- its
//!   gateway, sixty-four a gateway at most -- and on a port or none.
//! - **Firewalls**, 256 at most, `default` made with the group and never
//!   deleted: a policy each way and rules in order (`firewall`'s), changed
//!   whole by a compare and set of their revision.
//!
//! Applying is the same on every member: it reads the entry's index and
//! nothing else of the world -- no clock, no member's id, no registry. What
//! it cannot see -- a subnet or a public address that covers a host's own
//! address -- the proposing member looks at. Every byte string it is handed
//! is read whole and every count bounded: an entry that is no command is
//! answered `Malformed`, a snapshot that is no state's refused.
//!
//! The state is cheap to copy, as the member copies it into its view at
//! every turn: each table behind an `Arc`, copied whole only by a change
//! made while a copy holds it -- its tables are small, the ports' 4096
//! records at most. A determinism bug is a report, not a silent fork: every
//! `CHECK_EVERY` entries a member takes its state's digest (`rsmkit`), and
//! the leader proposes its own, which every member holds its own to.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod apply;
mod command;
mod decide;
mod snapshot;

pub use command::{Answer, Command, Refusal, REFUSALS};
pub use decide::{attach, Attach};
pub use members::{Group, Member, Removed};
pub use rsmkit::{Checks, Diverged};
pub use snapshot::FrozenState;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use firewall::{Action, Rule};
use raft::rsm::{RestoreError, SnapshotReader, StateMachine};

/// Networks a cluster has at most, and a network's name at most.
pub const NETWORKS_MAX: usize = 256;
pub const NETWORK_NAME_MAX: usize = 26;
/// The largest network number: 24 bits.
pub const VNI_MAX: u32 = (1 << 24) - 1;
/// A network's MTU, and its subnet's prefix.
pub const MTU_MIN: u16 = 576;
pub const MTU_MAX: u16 = 1440;
pub const MTU_DEFAULT: u16 = 1420;
pub const PREFIX_MIN: u8 = 16;
pub const PREFIX_MAX: u8 = 28;
/// Ports a cluster has at most, and the NICs a VM has.
pub const PORTS_MAX: usize = 4096;
pub const NICS_MAX: u8 = 8;
/// Public addresses a gateway takes at most -- its NAT's one to one -- and a
/// cluster.
pub const PUBLIC_PER_GATEWAY: usize = 64;
pub const PUBLIC_MAX: usize = 1024;
/// Firewalls a cluster has at most, and the one every VM has where it is
/// given none.
pub const FIREWALLS_MAX: usize = 256;
pub const DEFAULT_FIREWALL: &str = "default";
/// hv's own network, every host's: no network of the cluster's covers it.
pub const HOST_NET: u32 = 0x0A00_6400;
pub const HOST_PREFIX: u8 = 24;
/// How many entries apart a member takes its state's digest.
pub const CHECK_EVERY: u64 = 64;

/// The private ranges a network's subnet is inside: 10/8, 172.16/12,
/// 192.168/16.
const PRIVATE: [(u32, u8); 3] = [(0x0A00_0000, 8), (0xAC10_0000, 12), (0xC0A8_0000, 16)];

/// The mask of a prefix.
pub fn mask(prefix: u8) -> u32 {
    if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) }
}

/// Whether subnet `net`/`prefix` holds `address`.
pub fn holds(net: u32, prefix: u8, address: u32) -> bool {
    address & mask(prefix) == net
}

/// Whether two subnets on their own boundaries overlap: one holds the
/// other's first address.
pub fn overlap(a: (u32, u8), b: (u32, u8)) -> bool {
    holds(a.0, a.1, b.0) || holds(b.0, b.1, a.0)
}

/// Whether `name` may be a network's: a host's name's letters, up to 26.
pub fn valid_network_name(name: &str) -> bool {
    name.len() <= NETWORK_NAME_MAX && members::valid_name(name)
}

/// Whether `net`/`prefix` may be a network's subnet: a private range's, of
/// /16 to /28, on its own boundary, apart from hv's own.
pub fn valid_subnet(net: u32, prefix: u8) -> bool {
    (PREFIX_MIN..=PREFIX_MAX).contains(&prefix) && net & !mask(prefix) == 0
        && PRIVATE.iter().any(|&(p, l)| holds(p, l, net) && prefix >= l)
        && !overlap((net, prefix), (HOST_NET, HOST_PREFIX))
}

/// A network of the cluster's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub name: String,
    /// Its number, the VNI.
    pub vni: u32,
    pub net: u32,
    pub prefix: u8,
    pub mtu: u16,
    /// Whether its VMs go out to the world, masqueraded by their own host.
    pub out: bool,
    /// Where the next address is looked for from.
    pub next: u32,
}

impl Network {
    pub fn mask(&self) -> u32 {
        mask(self.prefix)
    }

    /// Its gateway: its first address, on every host.
    pub fn gateway(&self) -> u32 {
        self.net + 1
    }

    pub fn broadcast(&self) -> u32 {
        self.net | !self.mask()
    }

    pub fn holds(&self, address: u32) -> bool {
        holds(self.net, self.prefix, address)
    }

    /// Whether a port may have `address` here: inside the subnet, and none
    /// of its own, its gateway's, its broadcast, or one ending in 0 or 255 --
    /// the stack's ARP on `hv0` judges a sender by `hv0`'s own /24.
    pub fn may_have(&self, address: u32) -> bool {
        self.holds(address) && address != self.net && address != self.gateway() && address != self.broadcast()
            && !matches!(address & 0xff, 0 | 0xff)
    }
}

/// A VM's NIC's port on a network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Port {
    pub id: u64,
    pub uuid: [u8; 16],
    pub nic: u8,
    /// Its network's number.
    pub vni: u32,
    pub address: u32,
    /// The registry's node of the host it is on.
    pub node: u64,
    pub generation: u64,
    /// Its firewall's id.
    pub firewall: u32,
}

/// A public address, routed to its gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Public {
    pub address: u32,
    /// The registry's node of the host its provider routes it to.
    pub gateway: u64,
    /// The port it is on, 0 for none.
    pub port: u64,
}

/// A firewall of the cluster's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Firewall {
    pub id: u32,
    pub name: String,
    /// Moved by every change of its rules: what a change of them is asked
    /// against.
    pub revision: u64,
    pub inbound: Action,
    pub outbound: Action,
    pub rules: Arc<Vec<Rule>>,
}

/// vnet's state: what its log's entries make.
#[derive(Clone, Debug)]
pub struct State {
    members: members::Members,
    /// By number.
    networks: Arc<BTreeMap<u32, Network>>,
    /// By id.
    ports: Arc<BTreeMap<u64, Port>>,
    /// By address.
    public: Arc<BTreeMap<u32, Public>>,
    /// By id.
    firewalls: Arc<BTreeMap<u32, Firewall>>,
    /// The next network's number, port's id, generation and firewall's id:
    /// never given again.
    next_vni: u32,
    next_port: u64,
    next_generation: u64,
    next_firewall: u32,
    /// Moved by every command that changes the networks, the ports, the
    /// public addresses or the firewalls: what a host's table is of.
    revision: u64,
    /// The first entry from which the next digest is taken.
    next_check: u64,
    /* Of the tables above, made again from them: never in a snapshot */
    /// A port by its VM and NIC, and by its network and address.
    by_key: Arc<BTreeMap<([u8; 16], u8), u64>>,
    by_address: Arc<BTreeSet<(u32, u32)>>,
    /* This member's own: never in a snapshot, never compared */
    checks: Checks,
}

impl Default for State {
    fn default() -> State {
        State {
            members: members::Members::new(),
            networks: Arc::default(),
            ports: Arc::default(),
            public: Arc::default(),
            firewalls: Arc::default(),
            next_vni: 1,
            next_port: 1,
            next_generation: 1,
            next_firewall: 1,
            revision: 0,
            next_check: CHECK_EVERY,
            by_key: Arc::default(),
            by_address: Arc::default(),
            checks: Checks::default(),
        }
    }
}

/// Two states are the same where every table a snapshot holds is: what a
/// member keeps of its own -- its digests -- is not compared.
impl PartialEq for State {
    fn eq(&self, o: &State) -> bool {
        self.members == o.members && self.networks == o.networks && self.ports == o.ports
            && self.public == o.public && self.firewalls == o.firewalls && self.next_vni == o.next_vni
            && self.next_port == o.next_port && self.next_generation == o.next_generation
            && self.next_firewall == o.next_firewall && self.revision == o.revision
            && self.next_check == o.next_check
    }
}

impl Eq for State {}

impl State {
    pub fn new() -> State {
        State::default()
    }

    /* ---- what the state shows ---- */

    /// The group, its members and those removed (`members`).
    pub fn roll(&self) -> &members::Members {
        &self.members
    }

    pub fn group(&self) -> Option<&Group> {
        self.members.group()
    }

    pub fn networks(&self) -> impl Iterator<Item = &Network> + '_ {
        self.networks.values()
    }

    pub fn network(&self, name: &str) -> Option<&Network> {
        self.networks.values().find(|n| n.name == name)
    }

    pub fn network_of(&self, vni: u32) -> Option<&Network> {
        self.networks.get(&vni)
    }

    pub fn ports(&self) -> impl Iterator<Item = &Port> + '_ {
        self.ports.values()
    }

    pub fn port(&self, id: u64) -> Option<&Port> {
        self.ports.get(&id)
    }

    /// The port of a VM's NIC.
    pub fn port_of(&self, uuid: &[u8; 16], nic: u8) -> Option<&Port> {
        self.by_key.get(&(*uuid, nic)).and_then(|id| self.ports.get(id))
    }

    pub fn public(&self) -> impl Iterator<Item = &Public> + '_ {
        self.public.values()
    }

    pub fn public_of(&self, address: u32) -> Option<&Public> {
        self.public.get(&address)
    }

    /// The public address on port `port`, if one is.
    pub fn public_on(&self, port: u64) -> Option<&Public> {
        if port == 0 {
            return None;
        }
        self.public.values().find(|p| p.port == port)
    }

    pub fn firewalls(&self) -> impl Iterator<Item = &Firewall> + '_ {
        self.firewalls.values()
    }

    pub fn firewall(&self, name: &str) -> Option<&Firewall> {
        self.firewalls.values().find(|f| f.name == name)
    }

    pub fn firewall_of(&self, id: u32) -> Option<&Firewall> {
        self.firewalls.get(&id)
    }

    /// What every host's table is of: moved by every change of it.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// This member's digests.
    pub fn checks(&self) -> &Checks {
        &self.checks
    }

    /// The members that vote, and those that learn.
    pub fn voters(&self) -> Vec<u64> {
        self.members.voters()
    }

    pub fn learners(&self) -> Vec<u64> {
        self.members.learners()
    }

    /* ---- applying ---- */

    /// `command`, applied at entry `index`: what its proposer is answered.
    pub fn apply_command(&mut self, index: u64, command: &Command) -> Answer {
        let done = match command {
            Command::Init { group, name, host, member } => self.init(index, *group, name, host, *member),
            Command::Add { host, member } => self.members.add(index, host, *member).map_err(Refusal::from)
                .map(Answer::Done),
            Command::Remove { host } => self.members.remove(index, host).map_err(Refusal::from).map(Answer::Done),
            Command::Digest { at, digest } => {
                self.checks.compare(*at, digest, index);
                Ok(Answer::Done(0))
            }
            other if self.members.group().is_none() => {
                let _ = other;
                Err(Refusal::NotInitialized)
            }
            /* The networks, ports, public addresses and firewalls: apply.rs's,
               each change moving the revision -- a refusal, or one asked
               again that found it done, not */
            other => self.change(other),
        };
        done.unwrap_or_else(Answer::Refused)
    }

    /// The state's digest: the SHA-256 of its snapshot's bytes.
    pub fn digest(&self) -> [u8; 32] {
        rsmkit::digest(FrozenState::new(self.clone()))
    }

    /// The state as a snapshot holds it, in one buffer: for a host's tests --
    /// the member writes it a piece at a time (`freeze`).
    pub fn encode(&self) -> Vec<u8> {
        let mut f = FrozenState::new(self.clone());
        let mut out = Vec::new();
        loop {
            let before = out.len();
            raft::rsm::Frozen::write(&mut f, 64 * 1024, &mut out);
            if out.len() == before {
                return out;
            }
        }
    }

    /// The state `bytes` are, read whole and held to every rule: an error
    /// where they are no state's.
    pub fn decode(bytes: &[u8]) -> Result<State, RestoreError> {
        let mut r = codec::Stream::new(codec::Reader::new(bytes));
        let s = snapshot::read(&mut r)?;
        if !r.into_inner().done() {
            return Err(RestoreError("not vnet's state"));
        }
        Ok(s)
    }

    fn init(&mut self, index: u64, group: [u8; 16], name: &str, host: &str, member: u64) -> Result<Answer, Refusal> {
        let id = self.members.init(index, group, name, host, member)?;
        let fw = Firewall { id: self.next_firewall, name: String::from(DEFAULT_FIREWALL), revision: 1,
                            inbound: Action::Deny, outbound: Action::Allow, rules: Arc::new(Vec::new()) };
        self.next_firewall += 1;
        Arc::make_mut(&mut self.firewalls).insert(fw.id, fw);
        Ok(Answer::Done(id))
    }
}

impl StateMachine for State {
    type Frozen = FrozenState;

    /// A command applied -- and at every `CHECK_EVERY` entries, the state's
    /// digest taken, for the leader's to be held to.
    fn apply(&mut self, index: u64, command: &[u8]) -> Vec<u8> {
        let answer = match Command::decode(command) {
            Some(c) => self.apply_command(index, &c),
            None => Answer::Refused(Refusal::Malformed),
        };
        if index >= self.next_check {
            self.next_check = index.saturating_add(CHECK_EVERY);
            let digest = self.digest();
            self.checks.push(index, digest);
        }
        answer.encode()
    }

    /// No reads go through raft: a member's shell and provider read its own
    /// view, at the index it has applied.
    fn query(&self, _query: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    /// About its snapshot's bytes: each record at its most.
    fn size(&self) -> u64 {
        let rules: u64 = self.firewalls.values().map(|f| 64 + f.rules.len() as u64 * 128).sum();
        128 + self.members.size() + self.networks.len() as u64 * 48 + self.ports.len() as u64 * 56
            + self.public.len() as u64 * 20 + rules
    }

    fn freeze(&self) -> FrozenState {
        FrozenState::new(self.clone())
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>) -> Result<(), RestoreError> {
        let mut stream = codec::Stream::new(rsmkit::FromSnapshot(r));
        let s = snapshot::read(&mut stream)?;
        if stream.into_inner().0.remaining() != 0 {
            return Err(RestoreError("a snapshot longer than its state"));
        }
        *self = s;
        Ok(())
    }
}
