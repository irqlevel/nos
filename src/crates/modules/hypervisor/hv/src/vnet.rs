//! vnet as hv asks it (plans/vnet.md, "hv and vnet"): through the management
//! bus, as it asks NBS (nbs.rs) -- vnet.ko is a module of its own, and a
//! module reaches another's only so. Its `vnet` provider's `attach` -- a
//! VM's port on a network of the cluster's, read from the state the leader
//! vouches for, made where there is none -- `detach`, and `table`, what hv
//! keeps of the state: the networks, this host's ports and the firewalls
//! they are judged by, asked again by the module's instance and the
//! state's revision, and answered whole only when either moved.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use firewall::Action;
use kcore::events::SharedEvent;
use kcore::ipc::{Mailbox, Scope};
use kcore::mgmt::{Status, Writer};
use kcore::sync::Mutex;
use kcore::time::Duration;
use mgmtwire::{Limits, Value};

use crate::firewalls::Firewalls;
use crate::net::{Identity, Route, Switch, Wake};
use crate::tunnel::Tunnel;

/// What a network is called on a VM's `network=` when it is the cluster's.
pub use crate::config::{cluster_network, CLUSTER_PREFIX as PREFIX};
/// The most of an answer taken: a table of 256 networks, every port of the
/// cluster's -- each on another host four numbers -- and this host's
/// ports' firewalls, sixty-four rules each.
const ANSWER_MAX: usize = 1024 * 1024;
/// How an answer is read: as large as a table is.
const ANSWER_LIMITS: Limits = Limits { bytes: ANSWER_MAX, depth: 8, values: 64 * 1024, members: 64, string: 512 };

/// A firewall of the cluster's, as vnet's state has it: its rules as text,
/// which hv reads into a firewall of its own (`Firewalls::place_cluster`).
#[derive(Clone, Debug)]
pub struct ClusterFirewall {
    pub name: String,
    pub revision: u64,
    pub inbound: Action,
    pub outbound: Action,
    pub rules: Vec<String>,
}

/// What a VM's NIC on a network of the cluster's is, as vnet answers its
/// attach.
pub struct Attached {
    pub port: u64,
    pub network: String,
    pub identity: Identity,
    pub generation: u64,
    pub public: Option<u32>,
    pub firewall: ClusterFirewall,
    /// Whether the leader vouched for it: an attach with no leader to ask
    /// is answered from this host's member's own state, where that binds
    /// the port here -- hv says so until it is confirmed.
    pub confirmed: bool,
    /// Whether its port was made now.
    pub made: bool,
}

/// A network, as the table has it.
pub struct TableNetwork {
    pub name: String,
    pub vni: u32,
    pub net: u32,
    pub prefix: u8,
    pub mtu: u16,
    /// Whether it goes out to the world: a public address is served on a
    /// network that does, and on no other.
    pub out: bool,
}

/// A public address on a port, as the table has it: the address, the host
/// it is routed to, and the port's network, address and host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TablePublic {
    pub address: u32,
    pub gateway: u64,
    pub vni: u32,
    pub inner: u32,
    pub node: u64,
}

/// One of this host's ports, as the table has it.
pub struct TablePort {
    pub id: u64,
    pub uuid: [u8; 16],
    pub generation: u64,
    pub address: u32,
    /// The cluster's firewall it is judged by.
    pub firewall: String,
}

/// The table's hosts and ports elsewhere, as the tunnel takes them.
pub use crate::tunnel::{RemotePort, TableHost};

/// What hv keeps of vnet's state.
pub struct Table {
    pub instance: u64,
    pub revision: u64,
    /// The registry's hosts as one number: a host's address changed is a
    /// table changed, though vnet's revision is not.
    pub hosts_digest: u64,
    /// This host's node in the registry.
    pub node: u64,
    pub networks: Vec<TableNetwork>,
    /// This host's ports.
    pub ports: Vec<TablePort>,
    pub firewalls: Vec<ClusterFirewall>,
    pub hosts: Vec<TableHost>,
    /// Every port on another host.
    pub remote: Vec<RemotePort>,
    /// Every public address on a port.
    pub public: Vec<TablePublic>,
}

/// The provider's `method` called with the parameters `params` writes: its
/// answer, or why not -- its message where it gives one.
fn call(method: &str, params: &dyn Fn(&mut Writer)) -> Result<Value, (Status, String)> {
    let mut text = Vec::new();
    let written = {
        let mut sink = |b: &[u8]| {
            text.extend_from_slice(b);
            true
        };
        let mut w = Writer::new(&mut sink);
        params(&mut w);
        w.ok()
    };
    if !written {
        return Err((Status::Busy, String::from("out of memory")));
    }
    let mut answer = Vec::new();
    let status = kcore::mgmt::call("vnet", method, &text, ANSWER_MAX, &mut |b: &[u8]| {
        answer.extend_from_slice(b);
        true
    });
    let value = mgmtwire::json::parse(&answer, &ANSWER_LIMITS).ok();
    match status {
        Status::Ok => value.ok_or_else(|| (Status::Failed, alloc::format!("vnet {}: its answer is no JSON", method))),
        Status::NoProvider => Err((status, String::from("no vnet on this host -- `insmod /vnet.ko` first"))),
        s => {
            let said = value.as_ref().and_then(|v| v.get("error")).and_then(|e| e.get("message"))
                .and_then(|m| m.as_str()).map(String::from);
            Err((s, said.unwrap_or_else(|| alloc::format!("vnet {}: {:?}", method, s))))
        }
    }
}

/// `a.b.c.d`.
fn ip(v: &Value, key: &str) -> Option<u32> {
    netwire::parse_ipv4(v.get(key)?.as_str()?.as_bytes())
}

/// `a.b.c.d/n`.
fn subnet(v: &Value, key: &str) -> Option<(u32, u8)> {
    let (a, n) = v.get(key)?.as_str()?.split_once('/')?;
    Some((netwire::parse_ipv4(a.as_bytes())?, n.parse().ok()?))
}

fn uint(v: &Value, key: &str) -> Option<u64> {
    v.get(key)?.as_u64()
}

fn firewall_of(v: &Value) -> Option<ClusterFirewall> {
    let rules = v.get("rules")?.as_list()?.iter().map(|r| r.as_str().map(String::from)).collect::<Option<_>>()?;
    Some(ClusterFirewall {
        name: String::from(v.get("name")?.as_str()?),
        revision: uint(v, "revision")?,
        inbound: Action::parse(v.get("in")?.as_str()?)?,
        outbound: Action::parse(v.get("out")?.as_str()?)?,
        rules,
    })
}

/// A VM's UUID as vnet takes it.
fn uuid_text(uuid: &[u8; 16]) -> String {
    let h = codec::hex(uuid);
    alloc::format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// VM `uuid`'s NIC attached here on cluster network `network`: its port,
/// as vnet answers -- made, where it has none and `make` says to, at
/// `address` or the next free one, judged by `firewall` or `default`.
pub fn attach(uuid: &[u8; 16], network: &str, address: Option<u32>, firewall: Option<&str>, make: bool)
    -> Result<Attached, String>
{
    let v = call("attach", &|w| {
        w.begin_object();
        w.field_str("uuid", &uuid_text(uuid));
        w.field_str("network", network);
        if let Some(a) = address {
            w.field_str("address", &alloc::format!("{}", netwire::Ipv4(a)));
        }
        if let Some(f) = firewall {
            w.field_str("firewall", f);
        }
        w.field_bool("make", make);
        w.end_object();
    }).map_err(|(_, why)| alloc::format!("{}{}: {}", PREFIX, network, why))?;
    let read = || -> Option<Attached> {
        let (net, prefix) = subnet(&v, "subnet")?;
        let identity = Identity {
            ip: ip(&v, "address")?,
            vni: u32::try_from(uint(&v, "vni")?).ok()?,
            net,
            prefix,
            mtu: u16::try_from(uint(&v, "mtu")?).ok()?,
            out: v.get("out")?.as_bool()?,
        };
        Some(Attached {
            port: uint(&v, "port")?,
            network: String::from(v.get("network")?.as_str()?),
            identity,
            generation: uint(&v, "generation")?,
            public: ip(&v, "public"),
            firewall: firewall_of(v.get("firewall")?)?,
            confirmed: v.get("confirmed")?.as_bool()?,
            made: v.get("made")?.as_bool()?,
        })
    };
    read().ok_or_else(|| alloc::format!("{}{}: vnet's answer to an attach is not one", PREFIX, network))
}

/// A network of the cluster's, as vnet lists it: its name, its subnet, and
/// how many addresses it has free.
pub struct Network {
    pub name: String,
    pub subnet: String,
    pub free: u64,
}

/// The cluster's networks: none where there is no vnet on this host.
pub fn networks() -> Vec<Network> {
    let Ok(v) = call("networks", &|w| {
        w.begin_object();
        w.end_object();
    }) else {
        return Vec::new();
    };
    let mut all = Vec::new();
    for n in v.get("networks").and_then(|l| l.as_list()).unwrap_or(&[]) {
        let (Some(name), Some(subnet)) = (n.get("name").and_then(|x| x.as_str()), n.get("subnet").and_then(|x| x.as_str()))
        else {
            continue;
        };
        if all.try_reserve(1).is_ok() {
            all.push(Network { name: String::from(name), subnet: String::from(subnet), free: uint(n, "free").unwrap_or(0) });
        }
    }
    all
}

/// Port `id` of vnet's moved to host `to` -- from wherever it is: a
/// migration's handover (plans/vm-migration.md) -- its new generation; the
/// keepers of both hosts shut it there and attach it here as their tables
/// say.
pub fn port_move(id: u64, to: &str) -> Result<u64, String> {
    let v = call("port_move", &|w| {
        w.begin_object();
        w.field_uint("port", id);
        w.field_str("host", to);
        w.end_object();
    }).map_err(|(_, why)| why)?;
    uint(&v, "generation").ok_or_else(|| String::from("vnet's answer to a move is not one"))
}

/// VM `uuid`'s NIC's port deleted, wherever it is: its id, 0 for none.
pub fn detach(uuid: &[u8; 16]) -> Result<u64, String> {
    let v = call("detach", &|w| {
        w.begin_object();
        w.field_str("uuid", &uuid_text(uuid));
        w.end_object();
    }).map_err(|(_, why)| why)?;
    uint(&v, "port").ok_or_else(|| String::from("vnet's answer to a detach is not one"))
}

/// Why the table could not be had: no vnet on this host, or anything else.
pub enum NoTable {
    Gone,
    Failed(String),
}

/// vnet's table, where it is not the one hv has -- `known`, its instance,
/// revision and hosts' digest: None where it is.
pub fn table(known: Option<(u64, u64, u64)>) -> Result<Option<Table>, NoTable> {
    let v = call("table", &|w| {
        w.begin_object();
        if let Some((instance, revision, digest)) = known {
            w.field_uint("instance", instance);
            w.field_uint("revision", revision);
            w.field_uint("hosts_digest", digest);
        }
        w.end_object();
    }).map_err(|(s, why)| if s == Status::NoProvider { NoTable::Gone } else { NoTable::Failed(why) })?;
    if v.get("same").and_then(|s| s.as_bool()) == Some(true) {
        return Ok(None);
    }
    let read = || -> Option<Table> {
        let mut networks = Vec::new();
        for n in v.get("networks")?.as_list()? {
            let (net, prefix) = subnet(n, "subnet")?;
            networks.push(TableNetwork {
                name: String::from(n.get("name")?.as_str()?), vni: u32::try_from(uint(n, "vni")?).ok()?, net, prefix,
                mtu: u16::try_from(uint(n, "mtu")?).ok()?, out: n.get("out")?.as_bool()?,
            });
        }
        let mut ports = Vec::new();
        for p in v.get("ports")?.as_list()? {
            let hex: String = p.get("vm")?.as_str()?.chars().filter(|&c| c != '-').collect();
            ports.push(TablePort {
                id: uint(p, "id")?, uuid: codec::unhex::<16>(&hex)?, generation: uint(p, "generation")?,
                address: ip(p, "address")?, firewall: String::from(p.get("firewall")?.as_str()?),
            });
        }
        let mut firewalls = Vec::new();
        for f in v.get("firewalls")?.as_list()? {
            firewalls.push(firewall_of(f)?);
        }
        let mut hosts = Vec::new();
        for h in v.get("hosts")?.as_list()? {
            hosts.push(TableHost {
                node: uint(h, "node")?, name: String::from(h.get("name")?.as_str()?), underlay: ip(h, "underlay")?,
                port: u16::try_from(uint(h, "port")?).ok()?,
            });
        }
        let mut remote = Vec::new();
        for r in v.get("remote")?.as_list()? {
            let r = r.as_list()?;
            let n = |i: usize| r.get(i).and_then(|x| x.as_u64());
            remote.push(RemotePort {
                vni: u32::try_from(n(0)?).ok()?, address: u32::try_from(n(1)?).ok()?, node: n(2)?, generation: n(3)?,
            });
        }
        /* A vnet of V3's says no public addresses: none */
        let mut public = Vec::new();
        if let Some(list) = v.get("public") {
            for x in list.as_list()? {
                let x = x.as_list()?;
                let n = |i: usize| x.get(i).and_then(|y| y.as_u64());
                public.push(TablePublic {
                    address: u32::try_from(n(0)?).ok()?, gateway: n(1)?, vni: u32::try_from(n(2)?).ok()?,
                    inner: u32::try_from(n(3)?).ok()?, node: n(4)?,
                });
            }
        }
        Some(Table {
            instance: uint(&v, "instance")?, revision: uint(&v, "revision")?,
            hosts_digest: uint(&v, "hosts_digest")?, node: uint(&v, "node")?, networks, ports, firewalls, hosts, remote,
            public,
        })
    };
    read().map(Some).ok_or_else(|| NoTable::Failed(String::from("vnet's table is not one")))
}

/* ---- what hv keeps of vnet: the keeper's task ---- */

/// How often the keeper asks vnet for its table, at most; and an attach
/// answered with no leader again -- which waits for one up to five seconds,
/// the switch's answers to `hv0` waiting meanwhile.
const TURN_MS: u64 = 1000;
const CONFIRM_MS: u64 = 10_000;
const NS_PER_MS: u64 = 1_000_000;

/// One of this host's VMs' ports on a network of the cluster's, as hv
/// attached it.
struct Here {
    /// Its place on the switch.
    slot: usize,
    uuid: [u8; 16],
    network: String,
    /// vnet's port, and its generation as attached: a port whose generation
    /// moves is no longer this VM's here, even back on this host.
    port: u64,
    generation: u64,
    confirmed: bool,
    /// Why it was shut, once it was.
    shut: Option<String>,
}

/// What wakes the keeper: an answer the switch put on its ring for `hv0`, a
/// turn asked for now (`turn_now`), and vnet's word of a change -- its
/// mailbox made on it.
struct Bell(SharedEvent);

impl Wake for Bell {
    fn wake_up(&self) {
        self.0.signal();
    }
}

/// What hv keeps of vnet (plans/vnet.md, "hv and vnet"), and the task that
/// keeps it, `hv/vnet`: the switch's answers to `hv0` handed in as they
/// come (`Switch::hand_in`); and, while a VM here is on a network of the
/// cluster's, vnet's table asked for every second -- its networks given
/// the switch, whose guests are kept from them through their gateways; the
/// firewalls its ports are judged by put in their places again as their
/// rules change; a port whose generation moved, or that is gone, shut; an
/// attach answered with no leader read again until a leader vouches for
/// it. With vnet gone hv goes on by what it had, and says so once.
pub struct Keeper {
    switch: Arc<Switch>,
    firewalls: Arc<Firewalls>,
    /// The tunnel to the other hosts, given the table's hosts and ports
    /// elsewhere.
    tunnel: Arc<Tunnel>,
    bell: Arc<Bell>,
    here: Mutex<Vec<Here>>,
    /// The table hv has: vnet's instance and revision.
    known: Mutex<Option<(u64, u64, u64)>>,
    /// Whether vnet was found gone, and said so; why its table was last not
    /// had, said once.
    gone: AtomicBool,
    failed: Mutex<Option<String>>,
    /// When the attaches no leader vouched for are next asked again.
    confirm_due: AtomicU64,
    /// What the switch could not serve of the public addresses routed here,
    /// as it was last said: each said once, until it changes.
    serving: Mutex<Vec<String>>,
    /// Why a port here was last not moved to its firewall, said once.
    judging: Mutex<Option<String>>,
    /// A turn asked for now, not at the next second (`turn_now`).
    asked: AtomicBool,
}

impl Keeper {
    /// The keeper of `switch`, woken as it puts an answer on its ring.
    pub fn new(switch: Arc<Switch>, firewalls: Arc<Firewalls>, tunnel: Arc<Tunnel>) -> Option<Arc<Keeper>> {
        let bell = Arc::new(Bell(SharedEvent::new()?));
        let keeper = Arc::new(Keeper {
            switch, firewalls, tunnel, bell, here: Mutex::new(Vec::new())?, known: Mutex::new(None)?,
            gone: AtomicBool::new(false), failed: Mutex::new(None)?, confirm_due: AtomicU64::new(0),
            serving: Mutex::new(Vec::new())?, judging: Mutex::new(None)?, asked: AtomicBool::new(false),
        });
        let woken: Arc<dyn Wake> = keeper.bell.clone();
        drop(keeper.switch.on_answers(Some(woken)));
        Some(keeper)
    }

    /// The switch woken no more: before the keeper's task goes.
    pub fn quiet(&self) {
        drop(self.switch.on_answers(None));
        self.bell.0.signal();
    }

    /// The task's body, until it is asked to stop.
    pub fn run(keeper: Arc<Keeper>) {
        /* vnet's word of each change of its state it applies, on the bell's
         * event: the table asked for at once (vnetmeta::KEEPER). Without
         * it, at the next second */
        let told = match Mailbox::new_on(kcore::ipc::Limits { messages: 4, bytes: 256 }, &keeper.bell.0) {
            Ok(mb) => {
                if let Err(e) = mb.register(vnetmeta::KEEPER, Scope::Host) {
                    kcore::klog!(0, "hv: {} not named -- {}: vnet's changes seen here within a second, not at once",
                                 vnetmeta::KEEPER, e.name());
                }
                Some(mb)
            }
            Err(e) => {
                kcore::klog!(0, "hv: no mailbox for vnet's word of a change -- {}: its changes seen here within a \
                                 second, not at once", e.name());
                None
            }
        };
        let mut due = 0;
        while !kcore::task::stopping() {
            keeper.bell.0.wait_for(Duration::from_millis(TURN_MS));
            let mut word = false;
            if let Some(mb) = told.as_ref() {
                while mb.try_recv().is_some() {
                    word = true;
                }
            }
            keeper.switch.hand_in();
            let now = kcore::time::boot_time_ns();
            if now >= due || word || keeper.asked.swap(false, Ordering::AcqRel) {
                due = now + TURN_MS * NS_PER_MS;
                keeper.turn();
            }
        }
    }

    /// vnet's table asked for at once, not at the next turn: a port moved
    /// here or away -- a migration's -- which the table says where it is.
    pub fn turn_now(&self) {
        self.asked.store(true, Ordering::Release);
        self.bell.0.signal();
    }

    /// A VM's port on slot `slot` attached as `a`.
    pub fn attached(&self, slot: usize, uuid: &[u8; 16], a: &Attached) -> Result<(), String> {
        let mut network = String::new();
        network.try_reserve_exact(a.network.len()).map_err(|_| String::from("out of memory"))?;
        network.push_str(&a.network);
        let mut here = self.here.lock();
        here.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        here.retain(|h| h.slot != slot);
        here.push(Here { slot, uuid: *uuid, network, port: a.port, generation: a.generation,
                         confirmed: a.confirmed, shut: None });
        Ok(())
    }

    /// Slot `slot`'s VM gone.
    pub fn released(&self, slot: usize) {
        let gone: Vec<Here> = {
            let mut here = self.here.lock();
            let (gone, kept) = core::mem::take(&mut *here).into_iter().partition(|h| h.slot == slot);
            *here = kept;
            gone
        };
        drop(gone);
    }

    /// What `hv list` says of slot `slot`'s port on a cluster network: its
    /// network's name, and whether vnet vouched for it -- or why it was shut.
    pub fn said(&self, slot: usize) -> Option<(String, bool, Option<String>)> {
        self.here.lock().iter().find(|h| h.slot == slot).map(|h| (h.network.clone(), h.confirmed, h.shut.clone()))
    }

    /// One turn: the table asked for, where a VM here is on a network of
    /// the cluster's, and what it says done.
    fn turn(&self) {
        /* With no VM here on a cluster network and no table had yet, asked
         * all the same -- a host may be the way in of a public address for
         * a VM on another -- but nothing said of a vnet this host is in
         * none of */
        let known = *self.known.lock();
        let quiet = known.is_none() && self.here.lock().is_empty();
        let table = match table(known) {
            Ok(None) => {
                self.confirm();
                return;
            }
            Ok(Some(t)) => t,
            Err(_) if quiet => return,
            Err(NoTable::Gone) => {
                if !self.gone.swap(true, Ordering::AcqRel) {
                    kcore::flog!(crate::HV, "vnet is gone from this host -- the VMs on its networks go on as they are, \
                                     and none starts on one until it is back");
                }
                return;
            }
            Err(NoTable::Failed(why)) => {
                /* Said once a reason: a port vnet revoked stays open until
                 * a table comes */
                let mut said = self.failed.lock();
                if said.as_deref() != Some(why.as_str()) {
                    kcore::flog!(crate::HV, "vnet's table not had -- {}: what vnet changes is not seen here meanwhile",
                                 why);
                    *said = Some(why);
                }
                return;
            }
        };
        if self.gone.swap(false, Ordering::AcqRel) {
            kcore::flog!(crate::HV, "vnet is back on this host");
        }
        if self.failed.lock().take().is_some() {
            kcore::flog!(crate::HV, "vnet's table had again");
        }
        let mut subnets = Vec::new();
        if subnets.try_reserve_exact(table.networks.len()).is_ok() {
            subnets.extend(table.networks.iter().map(|n| (n.net, n.prefix)));
            self.switch.cluster_networks(&subnets);
        }
        let mtu = table.networks.iter().map(|n| n.mtu).max().unwrap_or(0);
        self.tunnel.set_table(table.node, &table.hosts, &table.remote, mtu);
        self.public(&table);
        for why in self.firewalls.refresh_cluster(&self.switch, &table.firewalls) {
            kcore::flog_limited!(crate::HV, "a firewall of the cluster's not changed here -- {}", why);
        }
        self.judged_by(&table);
        /* A port gone from this host's, of another generation, at another
         * address, or on a network gone: shut */
        let mut shut = Vec::new();
        {
            let mut here = self.here.lock();
            for h in here.iter_mut().filter(|h| h.shut.is_none()) {
                let mine = table.ports.iter().find(|p| p.id == h.port && p.uuid == h.uuid);
                let ip = self.switch.ip_of(h.slot);
                let network = table.networks.iter().find(|n| n.name == h.network);
                let why = match (mine, network) {
                    (_, None) => "its network is gone",
                    (Some(p), Some(n)) if p.generation == h.generation && p.address == ip
                        && self.switch.vnet_of(h.slot) == Some(n.vni) => continue,
                    (Some(p), Some(_)) if p.generation != h.generation => {
                        "its port moved since -- to another host, and back perhaps"
                    }
                    (Some(_), Some(_)) => "its port's address or network is not what it was attached at",
                    (None, Some(_)) => "its port is no longer this host's: moved, or deleted",
                };
                h.shut = Some(String::from(why));
                shut.push((h.slot, h.port));
            }
        }
        for (slot, port) in shut {
            self.switch.shut(slot);
            kcore::flog!(crate::HV, "port {} of vnet's on switch port {} shut -- revoked by vnet", port, slot);
        }
        *self.known.lock() = Some((table.instance, table.revision, table.hosts_digest));
        self.confirm();
    }

    /// Each port here judged by the firewall of the cluster's the table
    /// gives it -- `vnet port firewall` changes it under a running VM --
    /// its flows judged again by the one it moves to (`Switch::assign`).
    /// What could not be, said once.
    fn judged_by(&self, table: &Table) {
        let here: Vec<(usize, u64, [u8; 16])> = self.here.lock().iter().filter(|h| h.shut.is_none())
            .map(|h| (h.slot, h.port, h.uuid)).collect();
        for (slot, port, uuid) in here {
            let Some(p) = table.ports.iter().find(|p| p.id == port && p.uuid == uuid) else { continue };
            let Some(f) = table.firewalls.iter().find(|f| f.name == p.firewall) else { continue };
            let placed = self.firewalls.place_cluster(&self.switch, f)
                .and_then(|place| self.switch.assign(slot, place).map_err(String::from));
            if let Err(why) = placed {
                let mut said = self.judging.lock();
                if said.as_deref() != Some(why.as_str()) {
                    kcore::flog!(crate::HV, "port {} of vnet's not judged by {}{} -- {}", port, PREFIX, f.name, why);
                    *said = Some(why);
                }
            }
        }
    }

    /// The public addresses of `table`: those routed to this host served
    /// -- one to one in NAT, for ports here and on other hosts -- what could
    /// not be said once; and each port here told where its own is routed,
    /// where that is another host and its network goes out.
    fn public(&self, table: &Table) {
        let me = table.node;
        let mut routes = Vec::new();
        if routes.try_reserve_exact(table.public.len()).is_err() {
            return;
        }
        for p in table.public.iter().filter(|p| p.gateway == me) {
            if let Some(n) = table.networks.iter().find(|n| n.vni == p.vni) {
                routes.push(Route {
                    public: p.address, vni: p.vni, net: n.net, prefix: n.prefix, mtu: n.mtu, out: n.out,
                    inner: p.inner, node: p.node,
                });
            }
        }
        let said = self.switch.serve(me, &routes);
        {
            let mut before = self.serving.lock();
            for why in said.iter().filter(|w| !before.contains(w)) {
                kcore::flog!(crate::HV, "a public address of the cluster's -- {}", why);
            }
            *before = said;
        }
        let slots: Vec<usize> = self.here.lock().iter().filter(|h| h.shut.is_none()).map(|h| h.slot).collect();
        for slot in slots {
            let (addr, vni) = (self.switch.ip_of(slot), self.switch.vnet_of(slot));
            let out = table.networks.iter().any(|n| Some(n.vni) == vni && n.out);
            let gateway = table.public.iter()
                .find(|p| p.node == me && p.inner == addr && Some(p.vni) == vni)
                .map_or(0, |p| if p.gateway == me || !out { 0 } else { p.gateway });
            self.switch.public_way(slot, gateway);
        }
    }

    /// Ports attached with no leader to vouch for them, asked again: each
    /// confirmed once one does, and shut where what it vouches for is not
    /// what was attached.
    fn confirm(&self) {
        let now = kcore::time::boot_time_ns();
        if now < self.confirm_due.load(Ordering::Relaxed) {
            return;
        }
        self.confirm_due.store(now + CONFIRM_MS * NS_PER_MS, Ordering::Relaxed);
        let waiting: Vec<([u8; 16], String, u64, u64)> = self.here.lock().iter()
            .filter(|h| !h.confirmed && h.shut.is_none())
            .map(|h| (h.uuid, h.network.clone(), h.port, h.generation)).collect();
        for (uuid, network, port, generation) in waiting {
            let Ok(a) = attach(&uuid, &network, None, None, false) else { continue };
            if !a.confirmed {
                continue;
            }
            let mut here = self.here.lock();
            if let Some(h) = here.iter_mut().find(|h| h.uuid == uuid && h.port == port) {
                if a.port == port && a.generation == generation {
                    h.confirmed = true;
                } else {
                    h.shut = Some(String::from("the leader's state has another port for it"));
                    let slot = h.slot;
                    drop(here);
                    self.switch.shut(slot);
                }
            }
        }
    }
}
