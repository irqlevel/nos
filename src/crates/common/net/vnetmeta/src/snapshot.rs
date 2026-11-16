//! vnet's state as a snapshot holds it, written a record at a time and read
//! back off a stream: never the whole of it in one buffer, where the
//! kernel's heap gives no block past 512 KiB -- a firewall's rules are a
//! record of a few KiB. Its bytes are the state's in a fixed order, the same
//! on every member with the same state, so their SHA-256 is the digest the
//! members hold each other's to.
//!
//! Every table read back is held to the rules applying keeps (`check`): a
//! snapshot that says anything applying could not have made is refused.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use codec::{Source, Stream, Writer};
use firewall::RULES_MAX;
use raft::rsm::{Frozen, RestoreError};

use crate::command::{action_code, action_of, read_rule, write_port, write_rule, RULE_TEXT_MAX};
use crate::{
    overlap, valid_network_name, valid_subnet, Firewall, Group, Member, Network, Port, Public, Removed, State,
    CHECK_EVERY, DEFAULT_FIREWALL, FIREWALLS_MAX, HOST_NET, HOST_PREFIX, MTU_MAX, MTU_MIN, NETWORKS_MAX,
    NETWORK_NAME_MAX, NICS_MAX, PORTS_MAX, PUBLIC_MAX, PUBLIC_PER_GATEWAY, VNI_MAX,
};

const MAGIC: [u8; 4] = *b"VNM1";
const BAD: RestoreError = RestoreError("not vnet's state");

/// Where the stream is.
#[derive(Clone, Copy)]
enum At {
    Header,
    Members(usize),
    Removed(usize),
    Networks,
    Ports,
    Public,
    Firewalls,
    Done,
}

/// A state frozen -- its tables held as they were -- and written out a
/// record at a time.
pub struct FrozenState {
    state: State,
    at: At,
    /// The tables as lists, taken at the freeze.
    networks: Vec<Network>,
    ports: Vec<Port>,
    public: Vec<Public>,
    firewalls: Vec<Firewall>,
    /// Where each list is.
    i: usize,
    /// The record under way, and how much of it is out.
    pending: Vec<u8>,
    sent: usize,
}

impl FrozenState {
    pub(crate) fn new(state: State) -> FrozenState {
        let networks = state.networks.values().cloned().collect();
        let ports = state.ports.values().copied().collect();
        let public = state.public.values().copied().collect();
        let firewalls = state.firewalls.values().cloned().collect();
        FrozenState { state, at: At::Header, networks, ports, public, firewalls, i: 0, pending: Vec::new(), sent: 0 }
    }

    /// The next record's bytes into `pending`: false once there are none.
    fn next(&mut self) -> bool {
        self.sent = 0;
        let s = &self.state;
        let mut w = Writer { buf: core::mem::take(&mut self.pending) };
        w.buf.clear();
        self.at = match self.at {
            At::Header => {
                w.fixed(&MAGIC);
                match s.members.group() {
                    Some(g) => {
                        w.u8(1);
                        w.fixed(&g.id);
                        w.str(&g.name);
                    }
                    None => w.u8(0),
                }
                w.u32(s.next_vni);
                w.u64(s.next_port);
                w.u64(s.next_generation);
                w.u32(s.next_firewall);
                w.u64(s.revision);
                w.u64(s.next_check);
                w.u8(s.members.members().len() as u8);
                At::Members(0)
            }
            At::Members(i) => match s.members.members().get(i) {
                Some(m) => {
                    w.str(&m.host);
                    w.u64(m.id);
                    w.u64(m.since);
                    At::Members(i + 1)
                }
                None => {
                    w.u32(s.members.removed().len() as u32);
                    At::Removed(0)
                }
            },
            At::Removed(i) => match s.members.removed().get(i) {
                Some(r) => {
                    w.str(&r.host);
                    w.u64(r.id);
                    w.u64(r.at);
                    At::Removed(i + 1)
                }
                None => {
                    w.u32(self.networks.len() as u32);
                    self.i = 0;
                    At::Networks
                }
            },
            At::Networks => match self.networks.get(self.i) {
                Some(n) => {
                    w.str(&n.name);
                    w.u32(n.vni);
                    w.u32(n.net);
                    w.u8(n.prefix);
                    w.u16(n.mtu);
                    w.u8(u8::from(n.out));
                    w.u32(n.next);
                    self.i += 1;
                    At::Networks
                }
                None => {
                    w.u32(self.ports.len() as u32);
                    self.i = 0;
                    At::Ports
                }
            },
            At::Ports => match self.ports.get(self.i) {
                Some(p) => {
                    write_port(&mut w, p);
                    self.i += 1;
                    At::Ports
                }
                None => {
                    w.u32(self.public.len() as u32);
                    self.i = 0;
                    At::Public
                }
            },
            At::Public => match self.public.get(self.i) {
                Some(p) => {
                    w.u32(p.address);
                    w.u64(p.gateway);
                    w.u64(p.port);
                    self.i += 1;
                    At::Public
                }
                None => {
                    w.u32(self.firewalls.len() as u32);
                    self.i = 0;
                    At::Firewalls
                }
            },
            At::Firewalls => match self.firewalls.get(self.i) {
                Some(f) => {
                    w.u32(f.id);
                    w.str(&f.name);
                    w.u64(f.revision);
                    w.u8(action_code(f.inbound));
                    w.u8(action_code(f.outbound));
                    w.u8(f.rules.len() as u8);
                    for r in f.rules.iter() {
                        write_rule(&mut w, r);
                    }
                    self.i += 1;
                    At::Firewalls
                }
                None => At::Done,
            },
            At::Done => {
                self.pending = w.buf;
                return false;
            }
        };
        self.pending = w.buf;
        true
    }
}

impl Frozen for FrozenState {
    fn write(&mut self, max: usize, out: &mut Vec<u8>) {
        let mut room = max;
        while room > 0 {
            if self.sent == self.pending.len() && !self.next() {
                return;
            }
            let n = room.min(self.pending.len() - self.sent);
            out.extend_from_slice(&self.pending[self.sent..self.sent + n]);
            self.sent += n;
            room -= n;
        }
    }
}

/// A state read off `r`, as `FrozenState` writes it, every count bounded
/// before anything is made of it, and held to every rule applying keeps.
pub(crate) fn read<S: Source>(r: &mut Stream<S>) -> Result<State, RestoreError> {
    if r.fixed::<4>().ok_or(BAD)? != MAGIC {
        return Err(BAD);
    }
    let mut s = State::new();
    let group = match r.u8().ok_or(BAD)? {
        0 => None,
        1 => {
            let id = r.fixed().ok_or(BAD)?;
            Some(Group { id, name: r.str(members::NAME_MAX).ok_or(BAD)? })
        }
        _ => return Err(BAD),
    };
    s.next_vni = r.u32().ok_or(BAD)?;
    s.next_port = r.u64().ok_or(BAD)?;
    s.next_generation = r.u64().ok_or(BAD)?;
    s.next_firewall = r.u32().ok_or(BAD)?;
    s.revision = r.u64().ok_or(BAD)?;
    s.next_check = r.u64().ok_or(BAD)?;
    let n = usize::from(r.u8().ok_or(BAD)?);
    if n > members::MEMBERS_MAX {
        return Err(RestoreError("more members than a vnet takes"));
    }
    let mut list = Vec::with_capacity(n);
    for _ in 0..n {
        let host = r.str(members::NAME_MAX).ok_or(BAD)?;
        list.push(Member { host, id: r.u64().ok_or(BAD)?, since: r.u64().ok_or(BAD)? });
    }
    let n = r.u32().ok_or(BAD)? as usize;
    if n > members::REMOVED_MAX {
        return Err(RestoreError("more members removed than are kept"));
    }
    let mut removed = Vec::with_capacity(n);
    for _ in 0..n {
        let host = r.str(members::NAME_MAX).ok_or(BAD)?;
        removed.push(Removed { host, id: r.u64().ok_or(BAD)?, at: r.u64().ok_or(BAD)? });
    }
    s.members = members::Members::from_parts(group, list, removed);
    let n = r.u32().ok_or(BAD)? as usize;
    if n > NETWORKS_MAX {
        return Err(RestoreError("more networks than are kept"));
    }
    let mut networks = BTreeMap::new();
    for _ in 0..n {
        let name = r.str(NETWORK_NAME_MAX).ok_or(BAD)?;
        let (vni, net, prefix, mtu) = (r.u32().ok_or(BAD)?, r.u32().ok_or(BAD)?, r.u8().ok_or(BAD)?, r.u16().ok_or(BAD)?);
        let out = match r.u8().ok_or(BAD)? {
            0 => false,
            1 => true,
            _ => return Err(BAD),
        };
        let next = r.u32().ok_or(BAD)?;
        if networks.insert(vni, Network { name, vni, net, prefix, mtu, out, next }).is_some() {
            return Err(RestoreError("a network's number twice"));
        }
    }
    s.networks = Arc::new(networks);
    let n = r.u32().ok_or(BAD)? as usize;
    if n > PORTS_MAX {
        return Err(RestoreError("more ports than are kept"));
    }
    let mut ports = BTreeMap::new();
    for _ in 0..n {
        let p = Port {
            id: r.u64().ok_or(BAD)?,
            uuid: r.fixed().ok_or(BAD)?,
            nic: r.u8().ok_or(BAD)?,
            vni: r.u32().ok_or(BAD)?,
            address: r.u32().ok_or(BAD)?,
            node: r.u64().ok_or(BAD)?,
            generation: r.u64().ok_or(BAD)?,
            firewall: r.u32().ok_or(BAD)?,
        };
        if ports.insert(p.id, p).is_some() {
            return Err(RestoreError("a port's id twice"));
        }
    }
    s.ports = Arc::new(ports);
    let n = r.u32().ok_or(BAD)? as usize;
    if n > PUBLIC_MAX {
        return Err(RestoreError("more public addresses than are kept"));
    }
    let mut public = BTreeMap::new();
    for _ in 0..n {
        let p = Public { address: r.u32().ok_or(BAD)?, gateway: r.u64().ok_or(BAD)?, port: r.u64().ok_or(BAD)? };
        if public.insert(p.address, p).is_some() {
            return Err(RestoreError("a public address twice"));
        }
    }
    s.public = Arc::new(public);
    let n = r.u32().ok_or(BAD)? as usize;
    if n > FIREWALLS_MAX {
        return Err(RestoreError("more firewalls than are kept"));
    }
    let mut firewalls = BTreeMap::new();
    for _ in 0..n {
        let id = r.u32().ok_or(BAD)?;
        let name = r.str(firewall::NAME_MAX).ok_or(BAD)?;
        let revision = r.u64().ok_or(BAD)?;
        let inbound = action_of(r.u8().ok_or(BAD)?).ok_or(BAD)?;
        let outbound = action_of(r.u8().ok_or(BAD)?).ok_or(BAD)?;
        let k = usize::from(r.u8().ok_or(BAD)?);
        if k > RULES_MAX {
            return Err(RestoreError("more rules than a firewall holds"));
        }
        let mut rules = Vec::with_capacity(k);
        for _ in 0..k {
            let text: String = r.str(RULE_TEXT_MAX).ok_or(BAD)?;
            rules.push(read_rule(&text).ok_or(RestoreError("a firewall's rule that is none"))?);
        }
        let f = Firewall { id, name, revision, inbound, outbound, rules: Arc::new(rules) };
        if firewalls.insert(id, f).is_some() {
            return Err(RestoreError("a firewall's id twice"));
        }
    }
    s.firewalls = Arc::new(firewalls);
    s.index();
    s.check()?;
    Ok(s)
}

impl State {
    /// The tables made again from the ports: a port by its VM and NIC, and
    /// by its network and address.
    fn index(&mut self) {
        let mut by_key = BTreeMap::new();
        let mut by_address = BTreeSet::new();
        for p in self.ports.values() {
            by_key.insert((p.uuid, p.nic), p.id);
            by_address.insert((p.vni, p.address));
        }
        self.by_key = Arc::new(by_key);
        self.by_address = Arc::new(by_address);
    }

    /// The rules a state read back is held to: what applying never makes,
    /// refused.
    pub(crate) fn check(&self) -> Result<(), RestoreError> {
        self.members.check().map_err(RestoreError)?;
        let none = self.networks.is_empty() && self.ports.is_empty() && self.public.is_empty()
            && self.firewalls.is_empty();
        if self.members.group().is_none() && !none {
            return Err(RestoreError("networks, ports, addresses or firewalls of no vnet"));
        }
        if self.next_vni == 0 || self.next_vni > VNI_MAX + 1 || self.next_port == 0 || self.next_generation == 0
            || self.next_firewall == 0 || self.next_check < CHECK_EVERY
        {
            return Err(RestoreError("a counter of the state's at a value applying never leaves it at"));
        }
        if self.members.group().is_some() && self.firewall(DEFAULT_FIREWALL).is_none() {
            return Err(RestoreError("a vnet with no `default` firewall"));
        }
        /* Networks: named, numbered below the next, of a subnet and an MTU
           a network may have, apart from one another, their next address
           inside */
        let nets: Vec<&Network> = self.networks.values().collect();
        for (i, n) in nets.iter().enumerate() {
            if !valid_network_name(&n.name) || n.vni == 0 || n.vni >= self.next_vni || !valid_subnet(n.net, n.prefix)
                || !(MTU_MIN..=MTU_MAX).contains(&n.mtu) || !n.holds(n.next)
            {
                return Err(RestoreError("a network of no name, number, subnet or MTU a network has"));
            }
            if nets[..i].iter().any(|o| o.name == n.name || overlap((o.net, o.prefix), (n.net, n.prefix))) {
                return Err(RestoreError("a network's name twice, or two networks that overlap"));
            }
        }
        /* Firewalls: named, numbered below the next, each name once */
        let fws: Vec<&Firewall> = self.firewalls.values().collect();
        for (i, f) in fws.iter().enumerate() {
            if !firewall::valid_name(&f.name) || f.id == 0 || f.id >= self.next_firewall || f.revision == 0 {
                return Err(RestoreError("a firewall of no name or number"));
            }
            if fws[..i].iter().any(|o| o.name == f.name) {
                return Err(RestoreError("a firewall's name twice"));
            }
        }
        /* Ports: numbered below the next, each VM's NIC once, on a network at
           an address a port may have there and no other port has, on a node,
           of a firewall there is */
        let mut keys = BTreeSet::new();
        let mut addresses = BTreeSet::new();
        for p in self.ports.values() {
            let Some(n) = self.networks.get(&p.vni) else {
                return Err(RestoreError("a port of no network"));
            };
            if p.id == 0 || p.id >= self.next_port || p.generation == 0 || p.generation >= self.next_generation
                || p.node == 0 || p.nic >= NICS_MAX || !n.may_have(p.address)
                || !self.firewalls.contains_key(&p.firewall)
            {
                return Err(RestoreError("a port of no number, node, address or firewall a port has"));
            }
            if !keys.insert((p.uuid, p.nic)) || !addresses.insert((p.vni, p.address)) {
                return Err(RestoreError("a VM's NIC, or an address, two ports'"));
            }
        }
        /* Public addresses: of a gateway, inside no network, on a port there
           is or none, a port's one at most, a gateway's sixty-four */
        let mut on = BTreeSet::new();
        let mut at: BTreeMap<u64, usize> = BTreeMap::new();
        for p in self.public.values() {
            if p.address == 0 || p.gateway == 0 || crate::holds(HOST_NET, HOST_PREFIX, p.address)
                || self.networks.values().any(|n| n.holds(p.address))
            {
                return Err(RestoreError("a public address that is none, of no gateway, or inside a network"));
            }
            if p.port != 0 && (!self.ports.contains_key(&p.port) || !on.insert(p.port)) {
                return Err(RestoreError("a public address on a port there is not, or two on one port"));
            }
            let c = at.entry(p.gateway).or_insert(0);
            *c += 1;
            if *c > PUBLIC_PER_GATEWAY {
                return Err(RestoreError("more public addresses on a gateway than its NAT maps"));
            }
        }
        Ok(())
    }
}
