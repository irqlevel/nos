//! The rules of vnet's networks, ports, public addresses and firewalls, as
//! applying keeps them: each change moves the state's revision, a refusal
//! or one asked again that found it done does not.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use firewall::{Action, Rule, RULES_MAX};

use crate::{
    overlap, valid_network_name, valid_subnet, Answer, Command, Firewall, Network, Port, Public, Refusal, State,
    DEFAULT_FIREWALL, FIREWALLS_MAX, HOST_NET, HOST_PREFIX, MTU_MAX, MTU_MIN, NETWORKS_MAX, NICS_MAX, PORTS_MAX,
    PUBLIC_MAX, PUBLIC_PER_GATEWAY, VNI_MAX,
};

impl State {
    /// A command of the networks', the ports', the public addresses' or the
    /// firewalls', the group made.
    pub(crate) fn change(&mut self, command: &Command) -> Result<Answer, Refusal> {
        match command {
            Command::NetworkCreate { name, net, prefix, mtu, out } => self.network_create(name, *net, *prefix, *mtu, *out),
            Command::NetworkDelete { name } => self.network_delete(name),
            Command::PortCreate { uuid, nic, network, node, address, firewall } => {
                self.port_create(uuid, *nic, network, *node, *address, firewall)
            }
            Command::PortMove { port, expect, to } => self.port_move(*port, *expect, *to),
            Command::PortDelete { port } => self.port_delete(*port),
            Command::PortFirewall { port, firewall } => self.port_firewall(*port, firewall),
            Command::PublicAdd { address, gateway } => self.public_add(*address, *gateway),
            Command::PublicRemove { address } => self.public_remove(*address),
            Command::PublicSet { address, gateway, port, expect_gateway, expect_port } => {
                self.public_set(*address, *gateway, *port, *expect_gateway, *expect_port)
            }
            Command::FirewallCreate { name } => self.firewall_create(name),
            Command::FirewallSet { name, revision, inbound, outbound, rules } => {
                self.firewall_set(name, *revision, *inbound, *outbound, rules)
            }
            Command::FirewallDelete { name } => self.firewall_delete(name),
            /* The group's own: lib.rs's */
            Command::Init { .. } | Command::Add { .. } | Command::Remove { .. } | Command::Digest { .. } => {
                Err(Refusal::Malformed)
            }
        }
    }

    /// A change made: what every host's table is of moved on.
    fn moved(&mut self) {
        self.revision += 1;
    }

    /* ---- networks ---- */

    fn network_create(&mut self, name: &str, net: u32, prefix: u8, mtu: u16, out: bool) -> Result<Answer, Refusal> {
        if !valid_network_name(name) {
            return Err(Refusal::BadName);
        }
        if let Some(n) = self.network(name) {
            /* Asked again: the same network is done; another is refused */
            return if (n.net, n.prefix, n.mtu, n.out) == (net, prefix, mtu, out) {
                Ok(Answer::Done(u64::from(n.vni)))
            } else {
                Err(Refusal::NetworkTaken)
            };
        }
        if !valid_subnet(net, prefix) || !(MTU_MIN..=MTU_MAX).contains(&mtu) {
            return Err(Refusal::BadNetwork);
        }
        /* Apart from every other network, and holding no public address: a
           public address is one no network has, as its packets are the
           world's (found by the fuzzer's first run: an address made public,
           then a network around it, a state its own snapshot's check
           refused) */
        if self.networks.values().any(|n| overlap((n.net, n.prefix), (net, prefix)))
            || self.public.keys().any(|&a| crate::holds(net, prefix, a))
        {
            return Err(Refusal::Overlaps);
        }
        if self.networks.len() >= NETWORKS_MAX || self.next_vni > VNI_MAX {
            return Err(Refusal::TooMany);
        }
        let vni = self.next_vni;
        self.next_vni += 1;
        let n = Network { name: String::from(name), vni, net, prefix, mtu, out, next: net + 2 };
        Arc::make_mut(&mut self.networks).insert(vni, n);
        self.moved();
        Ok(Answer::Done(u64::from(vni)))
    }

    fn network_delete(&mut self, name: &str) -> Result<Answer, Refusal> {
        let Some(vni) = self.network(name).map(|n| n.vni) else {
            /* Asked again once done */
            return Ok(Answer::Done(0));
        };
        if self.ports.values().any(|p| p.vni == vni) {
            return Err(Refusal::NetworkInUse);
        }
        Arc::make_mut(&mut self.networks).remove(&vni);
        self.moved();
        Ok(Answer::Done(u64::from(vni)))
    }

    /* ---- ports ---- */

    /// The firewall a command names: `default` where it names none.
    fn firewall_named(&self, name: &str) -> Result<u32, Refusal> {
        let name = if name.is_empty() { DEFAULT_FIREWALL } else { name };
        self.firewall(name).map(|f| f.id).ok_or(Refusal::NoSuchFirewall)
    }

    fn port_create(&mut self, uuid: &[u8; 16], nic: u8, network: &str, node: u64, address: u32, firewall: &str)
        -> Result<Answer, Refusal>
    {
        /* Asked again for the same VM and NIC: the port there is, as it is --
           never changed or bound anew here; what differs is the asker's to
           say */
        if let Some(p) = self.port_of(uuid, nic) {
            return Ok(Answer::Port { port: *p, made: false });
        }
        if node == 0 || nic >= NICS_MAX {
            return Err(Refusal::BadPort);
        }
        let n = self.network(network).ok_or(Refusal::NoSuchNetwork)?.clone();
        let fw = self.firewall_named(firewall)?;
        if self.ports.len() >= PORTS_MAX {
            return Err(Refusal::TooMany);
        }
        let address = match address {
            0 => crate::decide::next_free(&n, &self.by_address).ok_or(Refusal::NoAddress)?,
            a if !n.may_have(a) => return Err(Refusal::BadAddress),
            a if self.by_address.contains(&(n.vni, a)) => return Err(Refusal::AddressTaken),
            a => a,
        };
        let id = self.next_port;
        self.next_port += 1;
        let generation = self.next_generation;
        self.next_generation += 1;
        let port = Port { id, uuid: *uuid, nic, vni: n.vni, address, node, generation, firewall: fw };
        /* The next address looked for past this one: one let go of is not
           the next one given */
        if let Some(net) = Arc::make_mut(&mut self.networks).get_mut(&n.vni) {
            net.next = crate::decide::after(net, address);
        }
        Arc::make_mut(&mut self.ports).insert(id, port);
        Arc::make_mut(&mut self.by_key).insert((*uuid, nic), id);
        Arc::make_mut(&mut self.by_address).insert((n.vni, address));
        self.moved();
        Ok(Answer::Port { port, made: true })
    }

    /// A compare and set: moved only from the node expected, whose refusal
    /// says where the port is. A move to where it is changes nothing.
    fn port_move(&mut self, port: u64, expect: u64, to: u64) -> Result<Answer, Refusal> {
        let p = *self.ports.get(&port).ok_or(Refusal::NoSuchPort)?;
        if p.node != expect {
            return Ok(Answer::Elsewhere(p.node));
        }
        if to == 0 {
            return Err(Refusal::BadPort);
        }
        if to == p.node {
            return Ok(Answer::Done(p.generation));
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        if let Some(q) = Arc::make_mut(&mut self.ports).get_mut(&port) {
            q.node = to;
            q.generation = generation;
        }
        self.moved();
        Ok(Answer::Done(generation))
    }

    /// The port deleted, and the public address on it let go of -- said.
    fn port_delete(&mut self, port: u64) -> Result<Answer, Refusal> {
        let Some(p) = Arc::make_mut(&mut self.ports).remove(&port) else {
            /* Asked again once done */
            return Ok(Answer::Deleted { public: 0 });
        };
        Arc::make_mut(&mut self.by_key).remove(&(p.uuid, p.nic));
        Arc::make_mut(&mut self.by_address).remove(&(p.vni, p.address));
        let mut public = 0;
        if let Some(a) = self.public.values().find(|x| x.port == port).map(|x| x.address) {
            if let Some(x) = Arc::make_mut(&mut self.public).get_mut(&a) {
                x.port = 0;
                public = a;
            }
        }
        self.moved();
        Ok(Answer::Deleted { public })
    }

    fn port_firewall(&mut self, port: u64, firewall: &str) -> Result<Answer, Refusal> {
        let fw = self.firewall_named(firewall)?;
        let p = self.ports.get(&port).ok_or(Refusal::NoSuchPort)?;
        if p.firewall == fw {
            return Ok(Answer::Done(u64::from(fw)));
        }
        if let Some(q) = Arc::make_mut(&mut self.ports).get_mut(&port) {
            q.firewall = fw;
        }
        self.moved();
        Ok(Answer::Done(u64::from(fw)))
    }

    /* ---- public addresses ---- */

    /// Whether `address` may be a public one: not 0, and inside no network of
    /// the cluster's nor hv's own.
    fn may_be_public(&self, address: u32) -> bool {
        address != 0 && !crate::holds(HOST_NET, HOST_PREFIX, address)
            && !self.networks.values().any(|n| n.holds(address))
    }

    fn at_gateway(&self, gateway: u64) -> usize {
        self.public.values().filter(|p| p.gateway == gateway).count()
    }

    fn public_add(&mut self, address: u32, gateway: u64) -> Result<Answer, Refusal> {
        if gateway == 0 || !self.may_be_public(address) {
            return Err(Refusal::BadPublic);
        }
        if let Some(p) = self.public.get(&address) {
            /* Asked again: the same is done */
            return if p.gateway == gateway { Ok(Answer::Done(u64::from(address))) } else { Err(Refusal::PublicTaken) };
        }
        if self.public.len() >= PUBLIC_MAX || self.at_gateway(gateway) >= PUBLIC_PER_GATEWAY {
            return Err(Refusal::TooMany);
        }
        Arc::make_mut(&mut self.public).insert(address, Public { address, gateway, port: 0 });
        self.moved();
        Ok(Answer::Done(u64::from(address)))
    }

    fn public_remove(&mut self, address: u32) -> Result<Answer, Refusal> {
        let Some(p) = self.public.get(&address) else {
            /* Asked again once done */
            return Ok(Answer::Done(0));
        };
        if p.port != 0 {
            return Err(Refusal::PublicInUse);
        }
        Arc::make_mut(&mut self.public).remove(&address);
        self.moved();
        Ok(Answer::Done(u64::from(address)))
    }

    /// A compare and set of the address's gateway and port: moved to another
    /// gateway, put on a port or taken off one -- only from what the asker
    /// read.
    fn public_set(&mut self, address: u32, gateway: u64, port: u64, expect_gateway: u64, expect_port: u64)
        -> Result<Answer, Refusal>
    {
        let p = *self.public.get(&address).ok_or(Refusal::NoSuchPublic)?;
        if (p.gateway, p.port) != (expect_gateway, expect_port) {
            return Err(Refusal::Stale);
        }
        if gateway == 0 {
            return Err(Refusal::BadPublic);
        }
        if (p.gateway, p.port) == (gateway, port) {
            return Ok(Answer::Done(u64::from(address)));
        }
        if port != 0 {
            if !self.ports.contains_key(&port) {
                return Err(Refusal::NoSuchPort);
            }
            if self.public.values().any(|x| x.port == port && x.address != address) {
                return Err(Refusal::PortHasPublic);
            }
        }
        if gateway != p.gateway && self.at_gateway(gateway) >= PUBLIC_PER_GATEWAY {
            return Err(Refusal::TooMany);
        }
        if let Some(x) = Arc::make_mut(&mut self.public).get_mut(&address) {
            x.gateway = gateway;
            x.port = port;
        }
        self.moved();
        Ok(Answer::Done(u64::from(address)))
    }

    /* ---- firewalls ---- */

    fn firewall_create(&mut self, name: &str) -> Result<Answer, Refusal> {
        if !firewall::valid_name(name) {
            return Err(Refusal::BadName);
        }
        if let Some(f) = self.firewall(name) {
            /* Asked again */
            return Ok(Answer::Done(u64::from(f.id)));
        }
        if self.firewalls.len() >= FIREWALLS_MAX || self.next_firewall == u32::MAX {
            return Err(Refusal::TooMany);
        }
        let id = self.next_firewall;
        self.next_firewall += 1;
        let fw = Firewall { id, name: String::from(name), revision: 1, inbound: Action::Deny, outbound: Action::Allow,
                            rules: Arc::new(Vec::new()) };
        Arc::make_mut(&mut self.firewalls).insert(id, fw);
        self.moved();
        Ok(Answer::Done(u64::from(id)))
    }

    /// The firewall's policies and rules, whole, where its revision is the
    /// one the asker read: its new revision.
    fn firewall_set(&mut self, name: &str, revision: u64, inbound: Action, outbound: Action, rules: &[Rule])
        -> Result<Answer, Refusal>
    {
        let f = self.firewall(name).ok_or(Refusal::NoSuchFirewall)?;
        if f.revision != revision {
            return Err(Refusal::Stale);
        }
        if rules.len() > RULES_MAX {
            return Err(Refusal::TooManyRules);
        }
        let id = f.id;
        let mut list = Vec::new();
        list.extend_from_slice(rules);
        let now = revision + 1;
        if let Some(f) = Arc::make_mut(&mut self.firewalls).get_mut(&id) {
            f.revision = now;
            f.inbound = inbound;
            f.outbound = outbound;
            f.rules = Arc::new(list);
        }
        self.moved();
        Ok(Answer::Done(now))
    }

    fn firewall_delete(&mut self, name: &str) -> Result<Answer, Refusal> {
        if name == DEFAULT_FIREWALL {
            return Err(Refusal::DefaultKept);
        }
        let Some(id) = self.firewall(name).map(|f| f.id) else {
            /* Asked again once done */
            return Ok(Answer::Done(0));
        };
        if self.ports.values().any(|p| p.firewall == id) {
            return Err(Refusal::FirewallInUse);
        }
        Arc::make_mut(&mut self.firewalls).remove(&id);
        self.moved();
        Ok(Answer::Done(u64::from(id)))
    }
}
