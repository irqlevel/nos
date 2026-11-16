//! The guests' network: a switch between the virtio NICs of the guests that
//! have one and the host's end of it, `hv0` -- a virtual NIC in the kernel's
//! stack (`kcore::vnic`), 10.0.100.1/24, which is how nos reaches the guests
//! and they reach it.
//!
//! A guest's NIC is a port here, and a port's number is its address: MAC
//! 02:00:00:00:64:NN and 10.0.100.NN, NN the port plus two, handed to the
//! guest's kernel on its command line (`ip=`). So the switch forwards by the
//! destination MAC without learning anything: to the port whose MAC it is,
//! to `hv0` for `hv0`'s, and to every port -- and `hv0`, when the frame did
//! not come from there -- for a broadcast, a multicast or a MAC that is no
//! port's.
//!
//! A port is on a network, or alone (`claim`'s `network`): the guests on a
//! network reach each other as on one LAN, and a guest alone -- what a VM is
//! unless its `network=` says otherwise -- reaches no other guest at all,
//! only `hv0` and, through NAT, the world. A frame for another network's
//! guest goes nowhere, counted against the port that sent it (`isolated`);
//! one for every port goes to the ports of its sender's network, and `hv0`;
//! and IPv6's first-come bindings are each network's own, an address one
//! network's guest took free for another's. `hv0` -- nos -- reaches every
//! guest, and nothing goes from one guest to another through it: NAT takes
//! nothing for the guests' own subnet (`layers/net/src/nat.rs`), and the stack
//! forwards nothing.
//!
//! What a guest sends is checked where it comes in (`ingress`): it has to
//! be IPv4, ARP or IPv6, sent as the guest on that port -- from the port's
//! MAC, an IPv4 packet from the port's address, an ARP message with the
//! port's MAC and address as its sender, or no address yet, as an ARP probe
//! has. The one IPv4 packet with no address yet is a DHCP client's, which
//! the switch answers and nothing past it sees. Anything else is dropped,
//! counted. A guest sending as another could take that guest's flows
//! through NAT, which answers a flow at the MAC its last packet came from,
//! and tell `hv0` another's address was its own.
//!
//! IPv6 goes between the guests and no further: nothing of nos's speaks it,
//! so `hv0` is given none, and there is no way out. Its addresses are the
//! guests' own to choose -- link-local, and in the prefix the switch's
//! router advertisement gives them (`PREFIX6`, unique-local, with no route)
//! -- so they are not checked against the port as an IPv4 address is but
//! bound to the port that first uses one (`Bindings`, RFC 6620's first come,
//! first served): as its source, or as the target of its neighbour
//! advertisement. From then on the address is that port's alone, until its
//! VM goes; another sending from it, or advertising it, is spoofing -- and a
//! guest looking for duplicates of an address it would have (DAD) is bound
//! to it then, if it is nobody's. A guest is no router here and no DHCPv6
//! server: its router advertisements, redirects and DHCPv6 replies go
//! nowhere, and a neighbour discovery message of its must give its own MAC
//! wherever it gives one, and come whole, not in fragments (RFC 6980). A
//! packet whose chain of extension headers hides what it carries -- past
//! its length, or a first fragment that stops short of the end of the upper
//! header it names, a UDP header's 8 bytes among them (`netwire::ipv6::upper`)
//! -- goes nowhere either: a router advertisement or a DHCPv6 server's reply
//! could be one (RFC 7112). The packet looked at is the one its receiver
//! takes, to the length its header says.
//!
//! The switch is also the guests' way out: while it exists, NAT is on from
//! `hv0` through the device nos's default route is on, so a guest reaches
//! whatever nos can, from nos's address (`layers/net/src/nat.rs`). And it answers
//! DHCP itself (`dhcp.rs`): a guest that asks is told its port's address,
//! nos as its router and the DNS server nos has as its own as it asks --
//! which `ip=` also carries, as its `dns0`, as it was at the guest's start;
//! one whose address is set by hand is told the rest (DHCPINFORM).
//!
//! A port's inbox holds what waits for the guest's NIC: as many frames as the
//! guest posts buffers for, so that a burst its ring would take is not
//! dropped on the way to it. It is filled from any CPU, the host's transmit
//! path among them with interrupts off, so it is a spin lock with interrupts
//! off over storage taken when the port is first claimed and kept for the
//! next VM on it: nothing on the way allocates, and a full inbox drops,
//! counted. A frame put in wakes the VM it belongs to -- a halted guest's
//! vCPU waits on the VM's event, a running one is kicked out of its guest
//! (`hv::Kick`) -- by a signal made under the inbox's lock, which is also
//! what keeps the VM there while it is made: the port gives the VM up in
//! task context, the lock down.
//!
//! What the guests share is shared out. `hv0`'s backlog -- every guest's
//! traffic to nos and through NAT goes on it -- takes from a port, between
//! two of the stack's passes over it, no more than its share: the backlog
//! over the ports in use, so that a guest alone has all of it and one of
//! several sending as fast as it can leaves the rest theirs. And what a
//! guest sends to every port -- a broadcast, a multicast, a MAC no port has
//! -- is a copy into every inbox and a wake or a kick of every VM: as many
//! at once as `FLOOD_BURST`, and `FLOOD_PER_SEC` after. What either keeps
//! out is dropped, counted against the port.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use firewall::{self as fw, Dir, Flows, Rules, Why, RULES_MAX};
use hv::offload::{self, Partial, Rx, Takes, Tcp4};

use kcore::consts::NS_PER_SEC;
use kcore::net::{Inside, Nat, NatError, OneToOne};
use kcore::sync::{IrqSpinLock, Mutex};
use kcore::trace::{self, FwDeny, FwDir, FwWhy};
use kcore::vnic::{Attached, Vnic, VnicSink, BACKLOG};

use netwire::{icmpv6, ipv6};

use crate::dhcp;

/// The most ports: a VM each.
pub const MAX_PORTS: usize = 16;
/// A port alone on the switch: its group (`Port::group`) is this and its
/// number, apart from every network's, which are 1 to `MAX_PORTS`.
const ALONE: u32 = 1 << 31;
/// A port on a network of the cluster's (`claim_vnet`): its group is this
/// and the network's number, its VNI -- 24 bits, never 0 -- apart from a
/// host's networks' and a port alone's.
const VNET: u32 = 1 << 30;
const VNI_MASK: u32 = (1 << 24) - 1;
/// A cluster network's port's MAC: these two bytes and its address -- the
/// same on whichever host its VM runs, and no host network's port's, whose
/// addresses (`SUBNET`) no cluster network has.
const VNET_MAC_PREFIX: [u8; 2] = [0x02, 0x00];
/// A cluster network's MTU, at least and at most: what vnet makes one with.
pub const VNET_MTU_MIN: u16 = 576;
pub const VNET_MTU_MAX: u16 = 1440;
/// What the switch answers `hv0` with and cannot hand in where it makes the
/// answer (`ToHost`): ARP replies, for as many ports as there are, twice.
const TO_HOST: usize = 2 * MAX_PORTS;
const ARP_FRAME: usize = netwire::ETH_HDR_LEN + netwire::ARP_LEN;
/// What a port's VM is told of a packet past its network's MTU -- ICMP's
/// "fragmentation needed" -- as many at once as this, and so many a second
/// after.
pub const TOO_BIG_BURST: u64 = 16;
pub const TOO_BIG_PER_SEC: u64 = 16;
/// Frames an inbox holds: the guest's receive ring's worth (a virtio NIC's
/// queue has 256 entries). At 64, a guest fetching over several connections
/// at once lost frames on every burst, kicked or not.
const INBOX_FRAMES: usize = 256;
const FRAME: usize = hv::nic::MAX_FRAME;
/// Sends of TCP up to 64 KiB an inbox holds whole at once, for a guest
/// whose NIC takes them so (`Backend::takes_whole`), in room of their own
/// taken with the inbox's: one more waits as frames, cut.
const WHOLE_SENDS: usize = 8;
const SEND: usize = offload::MAX_SEND;
/// An inbox entry that is a frame and not a whole send.
const NOT_WHOLE: u8 = u8::MAX;
/// Every room for a whole send, a bit each.
const ALL_SENDS: u8 = ((1u16 << WHOLE_SENDS) - 1) as u8;
/// What a port may send to every port at once, and how many a second after:
/// ARP and DHCP's few, with room to spare.
pub const FLOOD_BURST: u64 = 64;
pub const FLOOD_PER_SEC: u64 = 256;
/// The flows a port keeps for its VM's firewall (`firewall::Flows`): room
/// taken when the port is first claimed, as its inbox's is, and kept for the
/// next VM on it.
pub const FLOWS: usize = 2048;
/// The most firewalls the switch holds at once, a place each
/// (`Switch::install`).
pub const FIREWALLS_MAX: usize = 64;
/// What a VM is told of what its firewall keeps it from sending -- an ICMP
/// error, so that its connect fails at once rather than waiting out its
/// retries: as many at once as this, and so many a second after.
pub const REFUSALS_BURST: u64 = 16;
pub const REFUSALS_PER_SEC: u64 = 16;

/// What a VM on a host's network takes of a packet at most: what a public
/// address mapped to one lets in (`map_public`).
const PUBLIC_MTU: u16 = 1500;

/// The host's end: its name, MAC and address, and the subnet's mask.
const HOST_IF: &str = "hv0";
pub const HOST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0x64, 0x01];
const SUBNET: u32 = 0x0A00_6400;
pub const HOST_IP: u32 = SUBNET | 1;
const MASK: u32 = 0xFFFF_FF00;
/// A port's MAC is this and its number plus two; so is its address's last
/// byte.
const PORT_MAC_PREFIX: [u8; 5] = [0x02, 0, 0, 0, 0x64];
const PORT_BASE: usize = 2;

/// The guests' IPv6 prefix, a /64 the switch's router advertisement gives
/// them to make addresses in: unique-local (RFC 4193), its global ID "nos"
/// and the subnet's third byte -- fd6e:6f73:64::/64. Advertised with no
/// route: nos speaks no IPv6, and is no way out.
pub const PREFIX6: [u8; 8] = [0xFD, 0x6E, 0x6F, 0x73, 0x00, 0x64, 0x00, 0x00];
/// The IPv6 addresses one port has bound at once: its link-local one, the
/// prefix's and temporary ones beside them. A new one past them takes the
/// place of the port's least recently used.
const ADDRS_PER_PORT: usize = 8;
const BINDINGS: usize = MAX_PORTS * ADDRS_PER_PORT;
/// DHCPv6's server's port, which a guest's packet is never from (RFC 8415).
const DHCP6_SERVER_PORT: u16 = 547;
/// The router advertisement the switch answers a solicitation with: its
/// link-layer address, the MTU, and the prefix -- on the link, and to make
/// addresses in -- for good; no route (router lifetime 0).
const RA_BYTES: usize = netwire::ETH_HDR_LEN + netwire::IPV6_HDR_LEN + RA_MSG;
const RA_MSG: usize = icmpv6::RA_OPTIONS + 8 + 8 + 32;
const RA_HOP_LIMIT: u8 = 64;
const ND_HOP_LIMIT: u8 = 255;
const MTU: u32 = 1500;
const PREFIX_LEN: u8 = 64;
/// The prefix option's flags: on the link, and for making addresses in.
const PREFIX_ON_LINK: u8 = 0x80;
const PREFIX_AUTONOMOUS: u8 = 0x40;
const FOREVER: u32 = u32::MAX;
/// All the link's nodes: where an advertisement answering a solicitation
/// from no address yet goes.
const ALL_NODES: ipv6::Addr = [0xFF, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

pub fn port_ip(port: usize) -> u32 {
    SUBNET | (port + PORT_BASE) as u32
}

pub fn port_mac(port: usize) -> [u8; 6] {
    let mut m = [0u8; 6];
    m[..5].copy_from_slice(&PORT_MAC_PREFIX);
    m[5] = (port + PORT_BASE) as u8;
    m
}

/// The port a MAC is, if it is one.
fn port_of(mac: &[u8]) -> Option<usize> {
    if mac[..5] != PORT_MAC_PREFIX {
        return None;
    }
    usize::from(mac[5]).checked_sub(PORT_BASE).filter(|p| *p < MAX_PORTS)
}

/// The port whose address `addr` is, if it is a port's.
pub fn port_at(addr: u32) -> Option<usize> {
    if addr & MASK != SUBNET {
        return None;
    }
    usize::from((addr & !MASK) as u8).checked_sub(PORT_BASE).filter(|p| *p < MAX_PORTS)
}

/// Whether `addr` is a group's: everyone's, the subnet's everyone, a
/// multicast group.
fn to_group(addr: u32) -> bool {
    const MULTICAST: u32 = 0xE;
    addr == u32::MAX || addr == SUBNET | !MASK || addr >> 28 == MULTICAST
}

/// A cluster network's port's MAC: its address behind `VNET_MAC_PREFIX`.
pub fn vnet_mac(ip: u32) -> [u8; 6] {
    let mut m = [0u8; 6];
    m[..2].copy_from_slice(&VNET_MAC_PREFIX);
    m[2..].copy_from_slice(&ip.to_be_bytes());
    m
}

/// The address a cluster network's port's MAC says, if it is one.
fn vnet_ip(mac: &[u8]) -> Option<u32> {
    (mac.len() >= 6 && mac[..2] == VNET_MAC_PREFIX).then(|| netwire::be32(mac, 2))
}

/// Whether group `group` (`Port::group`) is a cluster network's.
fn is_vnet(group: u32) -> bool {
    group & (VNET | ALONE) == VNET
}

/// The mask of a prefix of `prefix` bits, 1 to 32.
fn prefix_mask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix.clamp(1, 32))).unwrap_or(0)
}

/// Whether `addr` is a private one (RFC 1918), as every address of a
/// cluster network's is.
fn private(addr: u32) -> bool {
    addr >> 24 == 10 || addr >> 20 == 0xAC1 || addr >> 16 == 0xC0A8
}

/// Who a VM is on a network of the cluster's (`Switch::claim_vnet`): its
/// address; the network's number (VNI), subnet and MTU; and whether the
/// network goes out to the world -- masqueraded by NAT, from this host.
/// What vnet's state says of its port, and of the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub ip: u32,
    pub vni: u32,
    pub net: u32,
    pub prefix: u8,
    pub mtu: u16,
    pub out: bool,
}

impl Identity {
    pub fn mask(&self) -> u32 {
        prefix_mask(self.prefix)
    }

    /// The network's gateway, the same on every host: its first address.
    pub fn gateway(&self) -> u32 {
        self.net.wrapping_add(1)
    }

    /// Why the switch will not have it, if it will not: what vnet's state
    /// holds to already, held to here again -- an address no port of a host
    /// network's may have, and no other port of its network's is given.
    fn check(&self) -> Result<(), &'static str> {
        let mask = self.mask();
        if self.vni == 0 || self.vni > VNI_MASK {
            return Err("a cluster network's number is 1 to 16777215");
        }
        if !(16..=28).contains(&self.prefix) || self.net & !mask != 0 {
            return Err("a cluster network is a subnet of /16 to /28");
        }
        if !private(self.net) || !private(self.net | !mask) {
            return Err("a cluster network's addresses are private ones");
        }
        /* The host networks' subnet -- their ports' addresses, and hv0's --
         * met by the shorter of the two prefixes */
        let wider = MASK & mask;
        if SUBNET & wider == self.net & wider {
            return Err("a cluster network is apart from the host's networks, 10.0.100.0/24");
        }
        if self.ip & mask != self.net || self.ip == self.net || self.ip == self.gateway()
            || self.ip == self.net | !mask
        {
            return Err("the port's address is no host's of its network");
        }
        if !(VNET_MTU_MIN..=VNET_MTU_MAX).contains(&self.mtu) {
            return Err("a cluster network's MTU is 576 to 1440");
        }
        Ok(())
    }
}

/// A seed for a port's table of flows (`firewall::Flows::new`), from the
/// kernel's pool -- the clock's, should the pool have nothing yet: what the
/// far end of a flow does not know, so cannot make collide.
fn flow_seed() -> u64 {
    let mut b = [0u8; 8];
    if !kcore::random::fill_random(&mut b) {
        return kcore::time::boot_time_ns().wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    u64::from_le_bytes(b)
}

/// An address, dotted, for a command line.
pub fn dotted(ip: u32) -> String {
    alloc::format!("{}.{}.{}.{}", ip >> 24, (ip >> 16) & 0xFF, (ip >> 8) & 0xFF, ip & 0xFF)
}

/// The kernel command line's `ip=` for the guest on `port`: its address, the
/// host's as the gateway, the mask, and no autoconfiguration -- and no
/// device, which is the first one there is: a kernel told `eth0` waits
/// twelve seconds for it to appear, and one whose NIC driver is a module its
/// initramfs loads (a distribution's) finds none there before it gives up,
/// and leaves `ip=` to the initramfs. Then the DNS server, when there is
/// one to give: `dns0`, which the kernel shows in /proc/net/pnp and an
/// initramfs (Alpine's) writes to resolv.conf.
pub fn ip_param(port: usize, dns: Option<u32>) -> String {
    ip_param_at(port_ip(port), HOST_IP, MASK, dns)
}

/// `ip_param`'s, for a guest at `ip` behind `gateway` on a subnet of
/// `mask`: a cluster network's port's (`Identity`).
pub fn ip_param_at(ip: u32, gateway: u32, mask: u32, dns: Option<u32>) -> String {
    let ip = alloc::format!("ip={}::{}:{}:::off", dotted(ip), dotted(gateway), dotted(mask));
    match dns {
        Some(dns) => alloc::format!("{}:{}", ip, dotted(dns)),
        None => ip,
    }
}

/// Why the guests have no way out, as `hv` says it.
pub fn nat_why(why: NatError) -> &'static str {
    match why {
        NatError::NoUplink => "no device of nos's has a gateway to send them through",
        NatError::Device => "hv0 has no address",
        NatError::Busy => "NAT is on already, for something else",
        NatError::NoMemory => "out of memory for NAT's table",
    }
}

/// IPv6 addresses, each bound to the port that first used one -- as the
/// source of a packet of its, or as the target of its neighbour
/// advertisement -- and that port's alone from then on, until its VM goes
/// (RFC 6620's first come, first served). Fixed room, on the heap and
/// taken when the switch is made: `ADDRS_PER_PORT` a port, a new one past
/// them in the place of the port's least recently used. Read and changed
/// with interrupts off: a guest's frames come in on its CPUs' paths.
struct Bindings {
    entries: Vec<Option<Binding>>,
    /// Each use's stamp: what says which of a port's is least recently used.
    clock: u64,
}

#[derive(Clone, Copy)]
struct Binding {
    addr: ipv6::Addr,
    port: u8,
    /// The group of the port it is bound to (`Port::group`): an address is
    /// one port's within a network, and another network's to take too.
    group: u32,
    used: u64,
}

impl Bindings {
    fn new() -> Option<Bindings> {
        let mut entries = Vec::new();
        entries.try_reserve_exact(BINDINGS).ok()?;
        entries.resize(BINDINGS, None);
        Some(Bindings { entries, clock: 0 })
    }

    /// Whether `addr` is `port`'s to send from or advertise, `port` being of
    /// `group`: bound to it -- now, if it is nobody's on its network -- and
    /// not, if it is another port's there.
    fn claim(&mut self, port: usize, group: u32, addr: &ipv6::Addr) -> bool {
        self.clock += 1;
        if let Some(b) = self.entries.iter_mut().flatten().find(|b| b.addr == *addr && b.group == group) {
            if usize::from(b.port) != port {
                return false;
            }
            b.used = self.clock;
            return true;
        }
        let mine = self.entries.iter().flatten().filter(|b| usize::from(b.port) == port).count();
        let at = if mine < ADDRS_PER_PORT {
            self.entries.iter().position(Option::is_none)
        } else {
            self.entries.iter().enumerate()
                .filter_map(|(i, e)| e.filter(|b| usize::from(b.port) == port).map(|b| (i, b.used)))
                .min_by_key(|&(_, used)| used)
                .map(|(i, _)| i)
        };
        /* Room for every port's fill: a port under its own has a free
         * entry, one at it has its least recently used. */
        let Some(at) = at else { return false };
        self.entries[at] = Some(Binding { addr: *addr, port: port as u8, group, used: self.clock });
        true
    }

    /// Every address of `port`'s let go of: its VM has gone.
    fn release(&mut self, port: usize) {
        for e in self.entries.iter_mut() {
            if e.is_some_and(|b| usize::from(b.port) == port) {
                *e = None;
            }
        }
    }

    fn count(&self) -> usize {
        self.entries.iter().flatten().count()
    }
}

/// What carries a cluster network's packet to a port on another host: the
/// tunnel (`crate::tunnel`), which the switch is given (`Switch::carrier`).
pub trait Carrier: Send + Sync {
    /// Whether `addr`, of network `vni`, is a port on another host.
    fn elsewhere(&self, vni: u32, addr: u32) -> bool;
    /// The IPv4 `packet` of network `vni` carried to the port at `to` on its
    /// host: whether it went. A guest's CPU's context: nothing may wait.
    fn carry(&self, vni: u32, to: u32, packet: &[u8]) -> bool;
    /// The IPv4 `packet` a port here of network `vni` sent the world,
    /// carried to host `node` -- its public address's -- to go out there.
    /// A guest's CPU's context.
    fn carry_out(&self, node: u64, vni: u32, packet: &[u8]) -> bool;
    /// The world's IPv4 `packet` for the port of network `vni` on host
    /// `node`, whose public address this host serves: queued for the
    /// tunnel's task to carry. `hv0`'s sink's context: nothing may wait or
    /// take the heap.
    fn carry_in(&self, node: u64, vni: u32, packet: &[u8]) -> bool;
}

/// Whether two routes are the one mapping in NAT: the public address, to the
/// same address of the same network, at the same MTU -- wherever its port is.
fn same_mapping(a: &Route, b: &Route) -> bool {
    a.public == b.public && a.vni == b.vni && a.inner == b.inner && a.mtu == b.mtu && a.out == b.out
        && a.net == b.net && a.prefix == b.prefix
}

/// A public address routed to this host, on a port of a cluster network --
/// here or on another host -- as vnet's table has it (`Switch::serve`): the
/// address; the port's network, its subnet and MTU and whether it goes out;
/// the port's address, and its host's node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub public: u32,
    pub vni: u32,
    pub net: u32,
    pub prefix: u8,
    pub mtu: u16,
    pub out: bool,
    pub inner: u32,
    pub node: u64,
}

/// What a port's frames wake as they are put in its inbox: the VM on it --
/// its first CPU's halted task, or the CPU kicked out of its guest -- or a
/// probe's task (`crate::probe`).
pub trait Wake: Send + Sync {
    /// Called under the inbox's spin lock, interrupts off, on any CPU: it
    /// may not sleep, allocate, or take a lock that is not IRQ-safe.
    fn wake_up(&self);
}

/// What waits for one guest's NIC.
struct Inbox {
    /// The VM on the port, or a probe; None while it is free.
    owner: Option<Arc<dyn Wake>>,
    /// Which claim of the port this is, one more at each (`Switch::take`):
    /// what a firewall judged for one owner -- by its flows, and its rules --
    /// is held to it as it is delivered, and never put in the next one's
    /// inbox (`deliver_rx`).
    claim: u64,
    /// Room for `INBOX_FRAMES` while a VM owns the port -- taken when the
    /// port is first claimed, and kept for the next -- and none before.
    frames: Vec<[u8; FRAME]>,
    lens: [u16; INBOX_FRAMES],
    head: usize,
    count: usize,
    /// Which entries are whole sends -- their room, or `NOT_WHOLE` -- and
    /// the room for them: `WHOLE_SENDS` of `SEND` bytes, taken with the
    /// frames' and apart, the heap having no half megabyte in one piece to
    /// give; their lengths, how each is to be cut, and which are free, a bit
    /// each.
    whole: [u8; INBOX_FRAMES],
    /// A frame's checksum left to finish, where its guest's NIC takes them
    /// so.
    partial: [Option<Partial>; INBOX_FRAMES],
    sends: Vec<Vec<u8>>,
    send_lens: [usize; WHOLE_SENDS],
    layouts: [Option<Tcp4>; WHOLE_SENDS],
    free_sends: u8,
}

impl Inbox {
    /// Empty, every room for a whole send free.
    fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
        self.whole = [NOT_WHOLE; INBOX_FRAMES];
        self.partial = [None; INBOX_FRAMES];
        self.layouts = [None; WHOLE_SENDS];
        self.free_sends = ALL_SENDS;
    }

    /// The entry at the head taken off, its room for a whole send freed.
    fn pop(&mut self) {
        let w = usize::from(self.whole[self.head]);
        if w < WHOLE_SENDS {
            self.free_sends |= 1 << w;
            self.layouts[w] = None;
        }
        self.whole[self.head] = NOT_WHOLE;
        self.partial[self.head] = None;
        self.head = (self.head + 1) % INBOX_FRAMES;
        self.count -= 1;
    }
}

/// What a port may do of something held to a rate: how many it may do now,
/// and from when the part of one being earned counts -- a token bucket of
/// `BURST`, filled at `PER_SEC`.
struct Bucket<const BURST: u64, const PER_SEC: u64> {
    tokens: u64,
    at: u64,
}

/// A port's sends to every port.
type Flood = Bucket<FLOOD_BURST, FLOOD_PER_SEC>;
/// What a port's VM is told its firewall kept from going out.
type Refusals = Bucket<REFUSALS_BURST, REFUSALS_PER_SEC>;
/// What a port's VM is told of packets past its cluster network's MTU.
type TooBig = Bucket<TOO_BIG_BURST, TOO_BIG_PER_SEC>;

impl<const BURST: u64, const PER_SEC: u64> Bucket<BURST, PER_SEC> {
    const FULL: Self = Bucket { tokens: BURST, at: 0 };

    /// Whether one more may go at `now`: `BURST` at once, and `PER_SEC`
    /// after. The whole tokens earned since `at` are added and `at` moved on
    /// by the time they stand for, the part of the next one kept: moved to
    /// `now` at each one, the part was thrown away, and a sender just over
    /// the rate was held to half of it. A full bucket earns nothing while it
    /// is full: `at` is then `now`.
    fn take(&mut self, now: u64) -> bool {
        let earned = now.saturating_sub(self.at).saturating_mul(PER_SEC) / NS_PER_SEC;
        if self.tokens.saturating_add(earned) >= BURST {
            self.tokens = BURST;
            self.at = now;
        } else if earned != 0 {
            /* Under BURST tokens' time: no overflow, and never past `now` */
            self.tokens += earned;
            self.at += earned * NS_PER_SEC / PER_SEC;
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

struct Port {
    inbox: IrqSpinLock<Inbox>,
    /// `count`, readable without the lock: the vCPU looks every time round
    /// its loop, and takes the lock only when there is something.
    waiting: AtomicUsize,
    /// Frames for the VM on the port that its inbox had no room for, and
    /// frames it sent as another guest -- since it claimed the port.
    dropped: AtomicU64,
    spoofed: AtomicU64,
    /// Its frames on `hv0`'s backlog since the stack's pass `drain` -- its
    /// share spent -- and those its share kept out. Only its guest's sends
    /// touch them, which its NIC makes one at a time.
    drain: AtomicU64,
    on_backlog: AtomicUsize,
    over_share: AtomicU64,
    /// Its sends to every port, and those their rate kept out.
    flood: IrqSpinLock<Flood>,
    flooded: AtomicU64,
    /// Until when, on the boot clock, its VM's NIC looks for frames on its
    /// own, its receive interrupts held till then (`Backend::quiet_until`):
    /// a frame put in before it wakes nobody.
    quiet_until: AtomicU64,
    /// Whether its VM's NIC takes checksums left to finish, and sends of TCP
    /// whole (`Backend::takes`). Written under `inbox`'s lock, so that what
    /// is put there is put as the NIC takes it then (`deliver_rx`,
    /// `deliver_whole`); read without, they are what it took a moment ago.
    takes_partial: AtomicBool,
    takes_whole: AtomicBool,
    /// Which guests it reaches, while a VM has it: a network's number -- the
    /// ports on one share it -- or `ALONE` and its own, or `VNET` and a
    /// cluster network's VNI; 0 while it is free. Written under `inbox`'s
    /// lock with its `owner`, so that read there it is the owner's
    /// (`deliver_rx`); read without, it was a moment ago.
    group: AtomicU32,
    /// Who its VM is: its address -- on a host's network the port's own
    /// (`port_ip`), on a cluster network the one vnet gave it -- and on a
    /// cluster network the subnet, the MTU and whether the network goes out
    /// to the world. Written under `inbox`'s lock before `group`, which is
    /// stored after them with release: read after an acquiring load of
    /// `group`, they are its owner's or a later one's -- which a delivery
    /// holds to its owner under the lock (`deliver_rx`). A free port's
    /// address is its own again.
    ip: AtomicU32,
    net: AtomicU32,
    mask: AtomicU32,
    mtu: AtomicU32,
    out: AtomicBool,
    /// Shut by hv (`Switch::shut`): its cluster network's port revoked --
    /// moved to another host, or deleted, its generation gone by -- while
    /// its VM runs. Nothing from it or to it goes on, counted against it
    /// as `isolated`; its next claim opens it again.
    shut: AtomicBool,
    /// The host -- its node -- its public address is routed to, where that
    /// is another host (`Switch::public_way`): what it sends the world goes
    /// there through the tunnel, and only from there does the world's come
    /// in for it. 0: none, or this host's own NAT. Its claim and its
    /// release clear it.
    gateway: AtomicU64,
    /// Frames its guest sent where its network does not go -- to another
    /// network's guest; on a cluster network, also a broadcast, the host's
    /// networks, another cluster network, an address its gateway does not
    /// answer, the world from a network that does not go out -- dropped.
    isolated: AtomicU64,
    /// On a cluster network: frames its guest sent to a MAC that is no
    /// port's of its network's here, or to one port's MAC and another's
    /// address; and packets past its network's MTU, and what it is told of
    /// them, at a rate (`too_big`).
    misdirected: AtomicU64,
    oversize: AtomicU64,
    too_big: IrqSpinLock<TooBig>,
    /// Its VM's firewall: the flows it keeps, the firewall it is judged by,
    /// and the claim they are for. A spin lock with interrupts off, as the
    /// inbox's: a frame is judged on any CPU's path, `hv0`'s sink's among
    /// them. Taken before the firewalls' (`Ports::firewalls`) and never with
    /// the inbox's.
    fw: IrqSpinLock<PortFw>,
    /// Packets its firewall kept from its VM, and from going out of it; and
    /// live flows given up for new ones' room, its table being full.
    denied_in: AtomicU64,
    denied_out: AtomicU64,
    evicted: AtomicU64,
}

/// A port's firewall (`Port::fw`).
struct PortFw {
    /// Whether a VM has the port: a free one lets nothing through.
    owned: bool,
    /// The claim it is kept for (`Inbox::claim`).
    claim: u64,
    /// The firewall it is judged by: its place in `Ports::firewalls`.
    place: usize,
    /// Its flows: none until the port is first claimed, and kept, emptied,
    /// for the next VM on it.
    flows: Option<Flows>,
    /// What its VM is told of what is kept from going out.
    refusals: Refusals,
}

/// What a port's guest lost, since it claimed the port: frames for it its
/// inbox had no room for, frames it sent as another guest, frames for `hv0`
/// past its share of the backlog, sends to every port past their rate,
/// frames for where its network does not go; on a cluster network, frames
/// for a MAC or an address that is no port's there, and packets past its
/// MTU; packets its firewall kept from it and from going out of it, and
/// flows given up for new ones' room -- and the flows its firewall holds
/// now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PortCounts {
    pub dropped: u64,
    pub spoofed: u64,
    pub over_share: u64,
    pub flooded: u64,
    pub isolated: u64,
    pub misdirected: u64,
    pub oversize: u64,
    pub denied_in: u64,
    pub denied_out: u64,
    pub evicted: u64,
    pub flows: usize,
}

/// The firewalls ports are judged by, each in a place of its own: its
/// rules, and what each rule and each policy has let through and kept out.
/// The places are hv's to fill (`Switch::install`), and a port names its own
/// by its place, so that nothing is looked up by name on the way. A place's
/// room is taken the first time a firewall is put in it, with no lock held,
/// and kept: a change copies the rules in under the lock, which allocates
/// and frees nothing.
struct Firewalls {
    places: Vec<Option<Box<[Firewall]>>>,
}

/// A place for a firewall: its rules, and what they have judged, while it
/// is in use.
struct Firewall {
    used: bool,
    rules: Rules,
    hits: Hits,
}

impl Firewalls {
    /// The places, all of them empty and none with room yet.
    fn new() -> Option<Firewalls> {
        let mut places = Vec::new();
        places.try_reserve_exact(FIREWALLS_MAX).ok()?;
        places.resize_with(FIREWALLS_MAX, || None);
        Some(Firewalls { places })
    }

    fn get(&self, place: usize) -> Option<&Firewall> {
        self.places.get(place)?.as_ref()?.first().filter(|f| f.used)
    }

    fn get_mut(&mut self, place: usize) -> Option<&mut Firewall> {
        self.places.get_mut(place)?.as_mut()?.first_mut().filter(|f| f.used)
    }
}

/// Room for a place's firewall, one, or None out of memory: the heap's
/// room for a firewall's rules asked for as a vector's, which says when there
/// is none, rather than a box's, which would not.
fn firewall_room() -> Option<Box<[Firewall]>> {
    let mut room = Vec::new();
    room.try_reserve_exact(1).ok()?;
    room.push(Firewall { used: false, rules: Rules::DEFAULT, hits: Hits::NONE });
    Some(room.into_boxed_slice())
}

/// `fw_deny`'s record of a packet port `port`'s VM's firewall, in place
/// `place`, kept out going `dir`: what kept it out -- `rule` its number
/// from 1, 0 for none -- and the other end, the VM's port and the other's,
/// and the protocol, as the packet has them. Its caller has asked
/// `trace::on`; nothing here can fail on what a guest sent.
#[allow(clippy::too_many_arguments)]
fn denied_record(port: usize, place: usize, dir: Dir, why: FwWhy, rule: u8, peer: u32, ports: Option<(u16, u16)>,
                 proto: u8) {
    let (lport, rport) = ports.unwrap_or((0, 0));
    trace::emit(&FwDeny {
        peer,
        lport,
        rport,
        port: u8::try_from(port).unwrap_or(u8::MAX),
        place: u8::try_from(place).unwrap_or(u8::MAX),
        dir: if dir == Dir::In { FwDir::In } else { FwDir::Out } as u8,
        why: why as u8,
        rule,
        proto,
        _pad: [0; 2],
    });
}

/// What a firewall has judged since its rules were last set: the flows --
/// first packets -- each rule decided, and each policy, either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hits {
    pub rules: [u64; RULES_MAX],
    pub policy_in: u64,
    pub policy_out: u64,
}

impl Hits {
    pub const NONE: Hits = Hits { rules: [0; RULES_MAX], policy_in: 0, policy_out: 0 };

    fn count(&mut self, dir: Dir, why: Why) {
        match why {
            Why::Rule(i) => {
                if let Some(n) = self.rules.get_mut(usize::from(i)) {
                    *n += 1;
                }
            }
            Why::Policy if dir == Dir::In => self.policy_in += 1,
            Why::Policy => self.policy_out += 1,
            Why::Flow | Why::Related | Why::Stray => {}
        }
    }
}

/// The ports, shared by the switch, every guest's NIC and `hv0`'s sink.
pub struct Ports {
    ports: Vec<Port>,
    /// The ports a VM has: how many `hv0`'s backlog is shared among.
    in_use: AtomicUsize,
    vnic: Vnic,
    to_host: AtomicU64,
    refused: AtomicU64,
    /// DHCP messages answered.
    dhcp: AtomicU64,
    /// Frames from the guests the switch does not carry: neither IPv4, ARP
    /// nor IPv6, too short to be, IPv6 for nos -- which speaks none -- or
    /// IPv6 hiding what it carries; and DHCP messages it takes and does not
    /// answer -- a release, a decline, one not well-formed -- which went
    /// nowhere, counted nowhere.
    foreign: AtomicU64,
    /// The IPv6 addresses the guests have bound (`Bindings`).
    bindings: IrqSpinLock<Bindings>,
    /// Router solicitations answered.
    advertised: AtomicU64,
    /// The firewalls the ports are judged by (`Firewalls`): taken, when a
    /// frame is judged, with its port's firewall lock held (`Port::fw`).
    firewalls: IrqSpinLock<Firewalls>,
    /// ARP requests the switch answered for the cluster networks' guests and
    /// for `hv0`, and echoes it answered from a cluster network's gateway.
    arps: AtomicU64,
    echoes: AtomicU64,
    /// The subnets of the cluster's networks, address and mask, as vnet's
    /// table has them (`Switch::cluster_networks`): where a cluster
    /// network's guest is not let go through its gateway. Sorted by address,
    /// and apart; replaced whole, the old one freed with the lock down.
    clusters: IrqSpinLock<Vec<(u32, u32)>>,
    /// The switch's answers to `hv0` waiting to be handed in (`ToHost`).
    answers: IrqSpinLock<ToHost>,
    /// What carries a cluster network's packets to its ports on other hosts
    /// (`Carrier`): copied out under the lock, used outside it.
    carrier: IrqSpinLock<Option<Arc<dyn Carrier>>>,
    /// The public addresses this host serves for ports on other hosts, by
    /// the port's address: its network and its host (`Switch::serve`) --
    /// what `hv0`'s frames for such a port are carried by, and what such a
    /// port's packet for the world is held to. Replaced whole, the old freed
    /// with the lock down.
    served: IrqSpinLock<Vec<(u32, u32, u64)>>,
    /// The other hosts' ports' packets for the world, as one more port's
    /// share of `hv0`'s backlog (`to_host_served`): spent since the pass
    /// `served_drain`, and those it kept out; and the world's packets for
    /// them carried and handed to `hv0`.
    served_drain: AtomicU64,
    served_on_backlog: AtomicUsize,
    served_over_share: AtomicU64,
    served_in: AtomicU64,
    served_out: AtomicU64,
}

/// The switch's answers to `hv0` that cannot be handed in where they are
/// made: `hv0`'s sink is called under the device's transmit lock, interrupts
/// off, and a frame handed in may take the heap. ARP replies for a cluster
/// network's ports here -- what the stack asks as it reaches one, as `hv
/// forward`'s relay does -- in room taken with the switch, handed in by a
/// task (`Switch::hand_in`) woken as one is put in (`Switch::on_answers`).
/// A full ring drops, counted with what `hv0` would not take.
struct ToHost {
    frames: [[u8; ARP_FRAME]; TO_HOST],
    head: usize,
    count: usize,
    /// What is woken as one is put in: called under this lock, interrupts
    /// off (`Wake`).
    wake: Option<Arc<dyn Wake>>,
}

/// Whom a frame put in a port's inbox is for, asked again under the inbox's
/// lock (`deliver_rx`), where the port is its owner's: it may have changed
/// hands since the switch looked, and what was for the guest on it then is
/// for nobody now.
#[derive(Clone, Copy)]
struct For {
    /// From a guest of this group: for a port of it alone. None from `hv0`,
    /// and for the switch's own answer.
    group: Option<u32>,
    /// The address the port was found by, where it was by one: a cluster
    /// network's port's goes with its owner.
    ip: Option<u32>,
    /// The claim of the port its firewall judged it for: what the old
    /// owner's flows let in is not the new one's. None for the switch's own
    /// answer.
    claim: Option<u64>,
}

impl For {
    /// The switch's own answer, to the guest that asked: it is sending, so
    /// the port is its.
    const ASKER: For = For { group: None, ip: None, claim: None };

    /// From a guest of `group`, judged for `claim` of the port.
    fn guest(group: u32, claim: u64) -> For {
        For { group: Some(group), ip: None, claim: Some(claim) }
    }

    /// Whether port `p`, whose inbox is `inbox`, is still that owner's --
    /// and open.
    fn holds(&self, p: &Port, inbox: &Inbox) -> bool {
        !p.shut.load(Ordering::Relaxed)
            && self.group.is_none_or(|g| p.group.load(Ordering::Relaxed) == g)
            && self.ip.is_none_or(|ip| p.ip.load(Ordering::Relaxed) == ip)
            && self.claim.is_none_or(|c| c == inbox.claim)
    }
}

/// What a frame a guest sent is, where it comes into the switch.
enum Ingress {
    /// IPv4 or ARP, sent as the guest on its port: on its way.
    Own,
    /// IPv6, sent as the guest on its port: on its way to the other guests,
    /// and not to nos, which speaks none.
    Own6,
    /// A router solicitation, sent as the guest on its port: the switch's to
    /// answer with its advertisement, and nobody else's to see.
    Solicit,
    /// A DHCP client's request, from the port's address or none yet: the
    /// switch's to answer, and nobody else's to see.
    Dhcp,
    /// Sent as another: another's MAC, IPv4 or ARP from another's address,
    /// IPv6 from or advertising an address bound to another port -- or as a
    /// router, or a DHCPv6 server.
    Spoofed,
    /// From a port hv shut (`Switch::shut`): nothing of it goes on.
    Shut,
    /// Neither IPv4, ARP nor IPv6, too short to be any, or IPv6 hiding what
    /// it carries.
    Foreign,
}

/// The router advertisement the switch sends to `mac`, at `to` (RFC 4861,
/// 4.2), into `frame`: from `hv0`'s MAC and the link-local address it
/// makes, with that MAC, the link's MTU and `PREFIX6` -- on the link, to
/// make addresses in, for good -- and a router lifetime of 0: no route.
fn router_advert(mac: &netwire::Mac, to: &ipv6::Addr, frame: &mut [u8; RA_BYTES]) {
    use netwire::{ETH_HDR_LEN, ETH_TYPE_IPV6, IPV6_HDR_LEN, IP_PROTO_ICMPV6};
    let from = ipv6::with_eui64(&[0xFE, 0x80, 0, 0, 0, 0, 0, 0], &HOST_MAC);
    netwire::eth::write(frame, mac, &HOST_MAC, ETH_TYPE_IPV6);
    let (head, msg) = frame[ETH_HDR_LEN..].split_at_mut(IPV6_HDR_LEN);
    ipv6::write(head, RA_MSG as u16, IP_PROTO_ICMPV6, ND_HOP_LIMIT, &from, to);

    msg[0] = icmpv6::ROUTER_ADVERT;
    msg[4] = RA_HOP_LIMIT;
    /* Flags, the router lifetime, reachable time and retransmit timer: 0 */
    let mut at = icmpv6::RA_OPTIONS;
    msg[at..at + 2].copy_from_slice(&[icmpv6::OPT_SOURCE_LLA, 1]);
    msg[at + 2..at + 8].copy_from_slice(&HOST_MAC);
    at += 8;
    msg[at..at + 2].copy_from_slice(&[icmpv6::OPT_MTU, 1]);
    msg[at + 4..at + 8].copy_from_slice(&MTU.to_be_bytes());
    at += 8;
    msg[at..at + 4].copy_from_slice(&[icmpv6::OPT_PREFIX, 4, PREFIX_LEN, PREFIX_ON_LINK | PREFIX_AUTONOMOUS]);
    msg[at + 4..at + 8].copy_from_slice(&FOREVER.to_be_bytes());
    msg[at + 8..at + 12].copy_from_slice(&FOREVER.to_be_bytes());
    msg[at + 16..at + 24].copy_from_slice(&PREFIX6);

    let sum = ipv6::checksum(IP_PROTO_ICMPV6, &from, to, msg);
    msg[2..4].copy_from_slice(&sum.to_be_bytes());
}

/// What `frame`, from a guest whose MAC is `mac` and address `addr`, is:
/// IPv4 or ARP. The ARP fields are read where the kernel's ARP table reads
/// them, whatever the header says of their sizes.
fn ingress(mac: [u8; 6], addr: u32, frame: &[u8]) -> Ingress {
    use netwire::{arp, eth, ip, ARP_LEN, ETH_HDR_LEN, ETH_TYPE_ARP, ETH_TYPE_IP, IP_HDR_LEN};
    let Some(body) = frame.get(ETH_HDR_LEN..) else { return Ingress::Foreign };
    let own = match eth::ether_type(frame) {
        ETH_TYPE_IP if body.len() >= IP_HDR_LEN => {
            let src = ip::src(body);
            if dhcp::is_request(frame) && (src == 0 || src == addr) {
                return if eth::src(frame) == mac { Ingress::Dhcp } else { Ingress::Spoofed };
            }
            src == addr
        }
        ETH_TYPE_ARP if body.len() >= ARP_LEN => {
            let sender = arp::sender_ip(body);
            arp::sender_mac(body) == mac && (sender == 0 || sender == addr)
        }
        _ => return Ingress::Foreign,
    };
    if own && eth::src(frame) == mac { Ingress::Own } else { Ingress::Spoofed }
}

impl Ports {
    /// What `frame`, from the guest on `port`, is (`ingress`, and for IPv6
    /// `ingress6`): on a cluster network, IPv4 and ARP alone are carried.
    fn ingress(&self, port: usize, frame: &[u8]) -> Ingress {
        if self.ports.get(port).is_some_and(|p| p.shut.load(Ordering::Acquire)) {
            return Ingress::Shut;
        }
        let v6 = frame.len() >= netwire::ETH_HDR_LEN && netwire::eth::ether_type(frame) == netwire::ETH_TYPE_IPV6;
        if is_vnet(self.group(port)) {
            if v6 {
                return Ingress::Foreign;
            }
            return ingress(self.mac_of(port), self.ip_of(port), frame);
        }
        if v6 {
            return self.ingress6(port, frame);
        }
        ingress(port_mac(port), port_ip(port), frame)
    }

    /// Port `port`'s address (`Port::ip`): its VM's while one has it, the
    /// port's own while it is free.
    fn ip_of(&self, port: usize) -> u32 {
        self.ports.get(port).map_or(0, |p| p.ip.load(Ordering::Acquire))
    }

    /// Port `port`'s MAC: on a cluster network its address's
    /// (`vnet_mac`), otherwise the port's own (`port_mac`).
    fn mac_of(&self, port: usize) -> [u8; 6] {
        if is_vnet(self.group(port)) { vnet_mac(self.ip_of(port)) } else { port_mac(port) }
    }

    /// The gateway port `port`'s guest is told and answered from: its
    /// cluster network's first address, or `hv0`'s.
    fn gateway_of(&self, port: usize) -> u32 {
        match self.ports.get(port) {
            Some(p) if is_vnet(p.group.load(Ordering::Acquire)) => p.net.load(Ordering::Relaxed).wrapping_add(1),
            _ => HOST_IP,
        }
    }

    /// The port whose MAC `mac` is, if it is one's: a host network's port
    /// -- or a free one -- by its number, a cluster network's by its
    /// address. A host network's MAC whose port is a cluster network's now
    /// is nobody's.
    fn port_by_mac(&self, mac: &[u8]) -> Option<usize> {
        if let Some(p) = port_of(mac) {
            return (!is_vnet(self.group(p))).then_some(p);
        }
        self.port_by_ip(vnet_ip(mac)?)
    }

    /// The cluster network's port here whose address `addr` is: one of a
    /// cluster network's, every one of which has an address of its own.
    fn port_by_ip(&self, addr: u32) -> Option<usize> {
        self.ports.iter().position(|p| is_vnet(p.group.load(Ordering::Acquire)) && p.ip.load(Ordering::Relaxed) == addr)
    }

    /// Whether `addr` is on one of the cluster's networks -- by vnet's
    /// table, or of a port here.
    fn in_cluster(&self, addr: u32) -> bool {
        {
            let clusters = self.clusters.lock();
            /* The last subnet at or below the address: the one it can be in,
             * the subnets being apart. */
            let at = clusters.partition_point(|&(net, _)| net <= addr);
            if at > 0 && addr & clusters[at - 1].1 == clusters[at - 1].0 {
                return true;
            }
        }
        self.ports.iter().any(|p| {
            is_vnet(p.group.load(Ordering::Acquire))
                && addr & p.mask.load(Ordering::Relaxed) == p.net.load(Ordering::Relaxed)
        })
    }

    /// What an IPv6 frame from the guest on `port` is: its own -- from its
    /// MAC, from no address yet only while it finds one, and otherwise from
    /// an address that is its (`Bindings`), a neighbour advertisement's
    /// target its too, and wherever a neighbour discovery message gives a
    /// MAC its own -- or sent as another, or as a router or a DHCPv6
    /// server, or hiding what it carries.
    fn ingress6(&self, port: usize, frame: &[u8]) -> Ingress {
        use netwire::{be16, ETH_HDR_LEN, IPV6_HDR_LEN, IP_PROTO_ICMPV6, IP_PROTO_UDP};
        let pkt = &frame[ETH_HDR_LEN..];
        if pkt.len() < IPV6_HDR_LEN || ipv6::version(pkt) != ipv6::VERSION {
            return Ingress::Foreign;
        }
        /* The packet its receiver takes: to the length its header says,
         * the frame's bytes past it none of it. */
        let Some(pkt) = pkt.get(..IPV6_HDR_LEN + usize::from(be16(pkt, ipv6::PAYLOAD_LEN))) else {
            return Ingress::Foreign;
        };
        let mac = port_mac(port);
        if netwire::eth::src(frame) != mac {
            return Ingress::Spoofed;
        }
        let (protocol, at, fragment) = match ipv6::upper(pkt) {
            ipv6::Upper::Found { protocol, at, fragment } => (Some(protocol), at, fragment),
            ipv6::Upper::LaterFragment => (None, 0, true),
            ipv6::Upper::Hidden => return Ingress::Foreign,
        };

        /* What only a router, or a DHCPv6 server, sends; and the MACs and the
         * target neighbour discovery gives -- whole, for every option of it
         * to be looked at here (RFC 6980), and a target that is one
         * address. */
        let mut kind = None;
        let mut target = None;
        if protocol == Some(IP_PROTO_ICMPV6) {
            let msg = &pkt[at..];
            let Some(&k) = msg.first() else { return Ingress::Foreign };
            kind = Some(k);
            let options = match k {
                icmpv6::ROUTER_ADVERT | icmpv6::REDIRECT => return Ingress::Spoofed,
                icmpv6::ROUTER_SOLICIT => Some(icmpv6::RS_OPTIONS),
                icmpv6::NEIGHBOR_SOLICIT => Some(icmpv6::NS_OPTIONS),
                icmpv6::NEIGHBOR_ADVERT => Some(icmpv6::NA_OPTIONS),
                _ => None,
            };
            if let Some(options) = options {
                if fragment || msg.len() < options || !icmpv6::link_addresses_are(msg, options, &mac) {
                    return Ingress::Spoofed;
                }
                if k != icmpv6::ROUTER_SOLICIT {
                    let mut t = [0u8; 16];
                    t.copy_from_slice(&msg[icmpv6::TARGET..icmpv6::TARGET + 16]);
                    if ipv6::is_multicast(&t) {
                        return Ingress::Spoofed;
                    }
                    target = Some(t);
                }
            }
        }
        if protocol == Some(IP_PROTO_UDP) && pkt.len() >= at + 2 && be16(pkt, at) == DHCP6_SERVER_PORT {
            return Ingress::Spoofed;
        }

        /* The source: none yet only while finding one -- a solicitation of
         * an address it would have, or a router's, a report of what it
         * listens to -- and otherwise its own; a neighbour advertisement's
         * target its own too. */
        let src = ipv6::src(pkt);
        if src == ipv6::UNSPECIFIED {
            match kind {
                /* Looking for a duplicate of an address it would have: the
                 * address its, if it is nobody's (RFC 6620's tentative
                 * binding), and the solicitation let through either way, for
                 * an owner to answer. */
                Some(icmpv6::NEIGHBOR_SOLICIT) => {
                    if let Some(t) = target {
                        let group = self.group(port);
                        let _ = self.bindings.lock().claim(port, group, &t);
                    }
                }
                Some(icmpv6::ROUTER_SOLICIT | icmpv6::MLD_REPORT | icmpv6::MLD2_REPORT) => {}
                _ => return Ingress::Spoofed,
            }
        } else if ipv6::is_multicast(&src) {
            return Ingress::Spoofed;
        } else {
            let advertised = target.filter(|_| kind == Some(icmpv6::NEIGHBOR_ADVERT));
            let group = self.group(port);
            let mut bindings = self.bindings.lock();
            if !bindings.claim(port, group, &src) || advertised.is_some_and(|t| !bindings.claim(port, group, &t)) {
                return Ingress::Spoofed;
            }
        }
        if kind == Some(icmpv6::ROUTER_SOLICIT) { Ingress::Solicit } else { Ingress::Own6 }
    }

    /// Port `port`'s group (`Port::group`): 0 for a free port, or none.
    fn group(&self, port: usize) -> u32 {
        self.ports.get(port).map_or(0, |p| p.group.load(Ordering::Acquire))
    }

    /// Whether a frame from port `from` goes on to port `to`'s guest: the
    /// two on one network -- or `to` free, where nothing takes it -- and if
    /// not, it is dropped, counted against `from`. The delivery asks again,
    /// under the lock the port changes hands under.
    fn may_reach(&self, from: usize, to: usize) -> bool {
        let g = self.group(to);
        if g == 0 || g == self.group(from) {
            return true;
        }
        if let Some(p) = self.ports.get(from) {
            p.isolated.fetch_add(1, Ordering::Relaxed);
        }
        false
    }

    /// Whether the guest at `addr` is on port `q`'s network: a port of its
    /// group, which a port alone has none of -- or, on a cluster network,
    /// any host's address of its subnet but its own and its gateway's,
    /// wherever that host's port is.
    fn same_net(&self, q: usize, addr: u32) -> bool {
        let g = self.group(q);
        if is_vnet(g) {
            let Some(p) = self.ports.get(q) else { return false };
            let (net, mask) = (p.net.load(Ordering::Relaxed), p.mask.load(Ordering::Relaxed));
            return addr & mask == net && addr != net && addr != net | !mask && addr != net.wrapping_add(1)
                && addr != p.ip.load(Ordering::Relaxed);
        }
        g != 0 && g & ALONE == 0 && port_at(addr).is_some_and(|p| p != q && self.group(p) == g)
    }

    /// What port `q`'s firewall says of `frame`, going `dir` from its VM's
    /// side -- its headers read as a whole send's (`firewall::read_send`)
    /// where `whole`: the claim of the port it was judged for, to be held to
    /// as it is delivered (`deliver_rx`), or None for a frame that goes no
    /// further, counted against the port. A packet the firewall keeps from
    /// going out is answered to the VM itself, at a rate (`refuse`).
    ///
    /// Not judged: what is not IP -- ARP -- and neighbour discovery, which
    /// find the way and carry nothing; and the host's own traffic, to or
    /// from its address on `hv0`, the host being the VM's hypervisor. A free
    /// port lets nothing through.
    fn judged(&self, q: usize, dir: Dir, frame: &[u8], whole: bool) -> Option<u64> {
        let p = self.ports.get(q)?;
        let seen = if whole { fw::read_send(frame) } else { fw::read(frame) };
        let now = kcore::time::boot_time_ns();
        let mut refuse = false;
        /* What kept it out, where something did: for `fw_deny`. */
        let mut why = FwWhy::Unready;
        let mut rule = 0u8;
        let at;
        let claim = {
            let mut guard = p.fw.lock();
            let PortFw { owned, claim, place, flows, refusals } = &mut *guard;
            /* A free port's: nobody's to keep anything from, nothing counted
             * -- what was for the VM gone, or sent by its NIC as it went. */
            if !*owned {
                return None;
            }
            at = *place;
            let pass = match seen {
                fw::Read::NotIp | fw::Read::V6 { neighbour: true } => true,
                /* Only between a network's guests: the switch carries IPv6
                 * nowhere else. */
                fw::Read::V6 { neighbour: false } => {
                    why = FwWhy::V6;
                    self.firewalls.lock().get(*place)
                        .is_some_and(|f| f.rules.judge6(dir, true).action == fw::Action::Allow)
                }
                fw::Read::Fragment => {
                    why = FwWhy::Fragment;
                    false
                }
                fw::Read::Broken => {
                    why = FwWhy::Broken;
                    false
                }
                fw::Read::V4(v4) => {
                    let peer = if dir == Dir::In { v4.src } else { v4.dst };
                    if peer == HOST_IP {
                        true
                    } else {
                        let ctx = fw::Context {
                            vm: self.ip_of(q), same_net: self.same_net(q, peer), group: to_group(v4.dst), now,
                        };
                        let mut firewalls = self.firewalls.lock();
                        match (flows.as_mut(), firewalls.get_mut(*place)) {
                            (Some(flows), Some(f)) => {
                                let v = fw::judge(flows, &f.rules, dir, &v4, &ctx);
                                f.hits.count(dir, v.why);
                                (why, rule) = match v.why {
                                    Why::Rule(i) => (FwWhy::Rule, i.saturating_add(1)),
                                    Why::Policy => (FwWhy::Policy, 0),
                                    Why::Stray => (FwWhy::Stray, 0),
                                    Why::Flow | Why::Related => (FwWhy::Unready, 0),
                                };
                                if v.evicted {
                                    p.evicted.fetch_add(1, Ordering::Relaxed);
                                }
                                /* Told, where what it sent was kept in by a
                                 * rule or the policy: never of an error, nor
                                 * of a packet to a group. */
                                refuse = !v.pass && dir == Dir::Out && matches!(v.why, Why::Rule(_) | Why::Policy)
                                    && !ctx.group && !matches!(v4.icmp, Some(fw::Icmp::Error(_)))
                                    && refusals.take(now);
                                v.pass
                            }
                            _ => false,
                        }
                    }
                }
            };
            pass.then_some(*claim)
        };
        if claim.is_none() {
            let denied = if dir == Dir::In { &p.denied_in } else { &p.denied_out };
            denied.fetch_add(1, Ordering::Relaxed);
            if trace::on::<FwDeny>() {
                let (peer, ports, proto) = match seen {
                    fw::Read::V4(v4) if dir == Dir::In => (v4.src, v4.ports.map(|(src, dst)| (dst, src)), v4.proto),
                    fw::Read::V4(v4) => (v4.dst, v4.ports, v4.proto),
                    _ => (0, None, 0),
                };
                denied_record(q, at, dir, why, rule, peer, ports, proto);
            }
            if refuse {
                self.refuse(q, frame);
            }
        }
        claim
    }

    /// The VM on port `q` told that `frame`, which it sent, was kept in by
    /// its firewall: destination unreachable, communication administratively
    /// prohibited (RFC 1812 5.2.7.1), from its gateway, quoting the packet's
    /// header and the eight bytes after it (RFC 792) -- its connect fails at
    /// once, "no route to host", where it would wait out its retries.
    fn refuse(&self, q: usize, frame: &[u8]) {
        const ADMIN_PROHIBITED: u8 = 13;
        self.unreachable(q, frame, ADMIN_PROHIBITED, 0);
    }

    /// The VM on port `q` told that `frame`, which it sent, went nowhere:
    /// destination unreachable, of `code`, from its gateway, quoting the
    /// packet's header and the eight bytes after it (RFC 792) -- and for
    /// "fragmentation needed", the MTU it can have, `next_mtu` (RFC 1191).
    fn unreachable(&self, q: usize, frame: &[u8], code: u8, next_mtu: u16) {
        use netwire::{eth, icmp, ip, ETH_HDR_LEN, ETH_TYPE_IP, ICMP_HDR_LEN, IP_HDR_LEN, IP_PROTO_ICMP};
        const MAX_IHL: usize = 60;
        const QUOTED_L4: usize = 8;
        let Some(packet) = frame.get(ETH_HDR_LEN..).filter(|p| p.len() >= IP_HDR_LEN) else { return };
        let ihl = ip::header_len(packet);
        if ihl == 0 {
            return;
        }
        let quoted = (ihl + QUOTED_L4).min(packet.len());
        let icmp_len = ICMP_HDR_LEN + quoted;
        let total = ETH_HDR_LEN + IP_HDR_LEN + icmp_len;
        let mut buf = [0u8; ETH_HDR_LEN + IP_HDR_LEN + ICMP_HDR_LEN + MAX_IHL + QUOTED_L4];
        eth::write(&mut buf, &self.mac_of(q), &HOST_MAC, ETH_TYPE_IP);
        ip::write(&mut buf[ETH_HDR_LEN..], IP_PROTO_ICMP, self.gateway_of(q), self.ip_of(q), icmp_len, 0);
        let at = ETH_HDR_LEN + IP_HDR_LEN;
        buf[at + ICMP_HDR_LEN..total].copy_from_slice(&packet[..quoted]);
        /* "Unused" but for the next hop's MTU, in its second half */
        icmp::write(&mut buf[at..], icmp::DEST_UNREACH, code, 0, next_mtu, icmp_len);
        self.deliver(q, &buf[..total], For::ASKER);
    }

    /// Whether `frame`, an IPv4 packet from port `from`'s guest on a cluster
    /// network, is within its network's MTU -- its header's length and its
    /// own both. One past it goes nowhere, counted, its sender told so --
    /// for a packet that says not to cut it, at a rate (`too_big`).
    fn within_mtu(&self, from: usize, frame: &[u8]) -> bool {
        let Some(p) = self.ports.get(from) else { return false };
        let packet = &frame[netwire::ETH_HDR_LEN..];
        let mtu = p.mtu.load(Ordering::Relaxed) as usize;
        if packet.len() <= mtu && usize::from(netwire::ip::total_len(packet)) <= mtu {
            return true;
        }
        self.too_big(from, frame);
        false
    }

    /// A packet from port `from`'s guest past its cluster network's MTU, or
    /// a send of TCP whole whose frames would be, kept in: counted, and its
    /// sender told "fragmentation needed" from its gateway, with the MTU,
    /// where the packet says not to cut it -- as many at once as
    /// `TOO_BIG_BURST`, and `TOO_BIG_PER_SEC` after.
    fn too_big(&self, from: usize, frame: &[u8]) {
        use netwire::{be16, ip, ETH_HDR_LEN, IP_HDR_LEN};
        const FRAGMENTATION_NEEDED: u8 = 4;
        let Some(p) = self.ports.get(from) else { return };
        p.oversize.fetch_add(1, Ordering::Relaxed);
        let Some(packet) = frame.get(ETH_HDR_LEN..).filter(|p| p.len() >= IP_HDR_LEN) else { return };
        if be16(packet, ip::FRAG_OFF) & ip::DONT_FRAGMENT == 0
            || !p.too_big.lock().take(kcore::time::boot_time_ns())
        {
            return;
        }
        let mtu = u16::try_from(p.mtu.load(Ordering::Relaxed)).unwrap_or(VNET_MTU_MIN);
        self.unreachable(from, frame, FRAGMENTATION_NEEDED, mtu);
    }

    /// An echo request in `frame`, from port `from`'s guest to its gateway,
    /// answered from there: whether it was one. The echo's data comes back
    /// as it went, and the IP header's options do not.
    fn echo(&self, from: usize, frame: &[u8]) -> bool {
        use netwire::{eth, icmp, ip, ETH_HDR_LEN, ETH_TYPE_IP, ICMP_HDR_LEN, IP_HDR_LEN, IP_PROTO_ICMP};
        let packet = &frame[ETH_HDR_LEN..];
        let ihl = ip::header_len(packet);
        let total = usize::from(ip::total_len(packet));
        if ihl == 0 || ip::protocol(packet) != IP_PROTO_ICMP || ip::is_fragment(packet)
            || total < ihl + ICMP_HDR_LEN || total > packet.len()
        {
            return false;
        }
        let msg = &packet[ihl..total];
        if icmp::kind(msg) != icmp::ECHO_REQUEST || icmp::code(msg) != 0 {
            return false;
        }
        let at = ETH_HDR_LEN + IP_HDR_LEN;
        let mut reply = [0u8; FRAME];
        let Some(room) = reply.get_mut(at..at + msg.len()) else { return false };
        room.copy_from_slice(msg);
        eth::write(&mut reply, &self.mac_of(from), &HOST_MAC, ETH_TYPE_IP);
        ip::write(&mut reply[ETH_HDR_LEN..], IP_PROTO_ICMP, self.gateway_of(from), self.ip_of(from), msg.len(), 0);
        icmp::write(&mut reply[at..], icmp::ECHO_REPLY, 0, icmp::id(msg), icmp::seq(msg), msg.len());
        self.echoes.fetch_add(1, Ordering::Relaxed);
        self.deliver(from, &reply[..at + msg.len()], For::ASKER);
        true
    }

    /// An ARP request in `frame`, from port `from`'s guest on cluster
    /// network `group`, answered here: for its gateway, `HOST_MAC`; for a
    /// port of its network here, that port's MAC; for anything else -- its
    /// own address, which it asks after to find a duplicate, among them --
    /// nothing. ARP goes no further on a cluster network.
    fn answer_arp(&self, from: usize, group: u32, frame: &[u8]) {
        use netwire::{arp, eth, ARP_OP_REPLY, ARP_OP_REQUEST, ETH_HDR_LEN, ETH_TYPE_ARP};
        let request = &frame[ETH_HDR_LEN..];
        if arp::opcode(request) != ARP_OP_REQUEST {
            return;
        }
        let target = arp::target_ip(request);
        let mac = if target == self.gateway_of(from) {
            HOST_MAC
        } else if target != self.ip_of(from)
            && (self.port_by_ip(target).is_some_and(|q| self.group(q) == group)
                || self.carrier_now().is_some_and(|c| c.elsewhere(group & VNI_MASK, target)))
        {
            vnet_mac(target)
        } else {
            return;
        };
        let asker = self.mac_of(from);
        let mut reply = [0u8; ARP_FRAME];
        eth::write(&mut reply, &asker, &mac, ETH_TYPE_ARP);
        arp::write(&mut reply[ETH_HDR_LEN..], ARP_OP_REPLY, &mac, target, &asker, arp::sender_ip(request));
        self.arps.fetch_add(1, Ordering::Relaxed);
        self.deliver(from, &reply, For::ASKER);
    }

    /// The port of cluster network `group` here that `frame`, from port
    /// `from`'s guest, is for -- with the address it is for -- where it is a
    /// packet of its own, IPv4, to that port's MAC and address: what may go
    /// to it as it is, a send of TCP whole or a checksum left to finish.
    /// None for anything else, which goes the way every frame goes
    /// (`forward_vnet`) to be carried, answered or counted there.
    fn vnet_unicast(&self, from: usize, group: u32, frame: &[u8]) -> Option<(usize, u32)> {
        if frame.len() < netwire::ETH_HDR_LEN + netwire::IP_HDR_LEN
            || netwire::eth::ether_type(frame) != netwire::ETH_TYPE_IP
            || !matches!(self.ingress(from, frame), Ingress::Own)
        {
            return None;
        }
        let q = self.port_by_ip(vnet_ip(&frame[..6])?)?;
        let to = netwire::ip::dst(&frame[netwire::ETH_HDR_LEN..]);
        (q != from && self.group(q) == group && self.ip_of(q) == to).then_some((q, to))
    }

    /// A frame from port `from`'s guest on cluster network `group`, which
    /// carries IPv4 and ARP alone: its own (`ingress`); its DHCP and its ARP
    /// answered here; within its network's MTU (`within_mtu`); then by where
    /// it is for -- its gateway's MAC by the address (`via_gateway`), a
    /// port's MAC to that port, if it is of its network and has that
    /// address -- and nowhere else: no broadcast or multicast is carried.
    fn forward_vnet(&self, from: usize, group: u32, frame: &[u8]) {
        use netwire::{eth, ip, ETH_HDR_LEN, ETH_TYPE_ARP};
        let Some(p) = self.ports.get(from) else { return };
        match self.ingress(from, frame) {
            Ingress::Own => {}
            Ingress::Dhcp => {
                self.answer_dhcp(from, frame);
                return;
            }
            Ingress::Spoofed => {
                p.spoofed.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Ingress::Shut => {
                p.isolated.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Ingress::Own6 | Ingress::Solicit | Ingress::Foreign => {
                self.foreign.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        if eth::ether_type(frame) == ETH_TYPE_ARP {
            self.answer_arp(from, group, frame);
            return;
        }
        /* IPv4, its header there (`ingress`) */
        if !self.within_mtu(from, frame) {
            return;
        }
        let to = ip::dst(&frame[ETH_HDR_LEN..]);
        let dst = &frame[..6];
        if dst[0] & 1 != 0 {
            p.isolated.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if dst == HOST_MAC {
            self.via_gateway(from, frame, to);
            return;
        }
        let Some(q) = vnet_ip(dst).and_then(|ip| self.port_by_ip(ip)) else {
            /* A port of its network on another host -- its MAC its
             * address's: into the tunnel, judged going out as any packet
             * of its */
            let vni = group & VNI_MASK;
            match self.carrier_now() {
                Some(c) if vnet_ip(dst) == Some(to) && c.elsewhere(vni, to) => {
                    if self.judged(from, Dir::Out, frame, false).is_some() {
                        let end = (ETH_HDR_LEN + usize::from(ip::total_len(&frame[ETH_HDR_LEN..]))).min(frame.len());
                        c.carry(vni, to, &frame[ETH_HDR_LEN..end]);
                    }
                }
                _ => {
                    p.misdirected.fetch_add(1, Ordering::Relaxed);
                }
            }
            return;
        };
        if q == from {
            return;
        }
        if self.group(q) != group {
            p.isolated.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.ip_of(q) != to {
            p.misdirected.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.judged(from, Dir::Out, frame, false).is_none() {
            return;
        }
        if let Some(claim) = self.judged(q, Dir::In, frame, false) {
            self.deliver(q, frame, For { group: Some(group), ip: Some(to), claim: Some(claim) });
        }
    }

    /// What carries to other hosts, while there is one.
    fn carrier_now(&self) -> Option<Arc<dyn Carrier>> {
        self.carrier.lock().clone()
    }

    /// An IPv4 packet from port `from`'s guest on a cluster network to its
    /// gateway's MAC, by the address it is for: the gateway's own, whose
    /// echo is answered here and nothing else; `hv0`'s, the host's; then
    /// nowhere for the host's networks, the cluster's, or a group -- and the
    /// world through NAT, for a network that goes out, judged by its
    /// firewall on the way.
    fn via_gateway(&self, from: usize, frame: &[u8], to: u32) {
        let Some(p) = self.ports.get(from) else { return };
        if to == self.gateway_of(from) {
            if !self.echo(from, frame) {
                p.isolated.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        if to != HOST_IP
            && (to & MASK == SUBNET || to_group(to) || self.in_cluster(to) || !p.out.load(Ordering::Relaxed))
        {
            p.isolated.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.judged(from, Dir::Out, frame, false).is_none() {
            return;
        }
        /* The world, by the host its public address is routed to, where
         * that is another: through the tunnel, to go out there */
        let gateway = p.gateway.load(Ordering::Acquire);
        if gateway != 0 && to != HOST_IP {
            use netwire::{ip, ETH_HDR_LEN};
            let end = (ETH_HDR_LEN + usize::from(ip::total_len(&frame[ETH_HDR_LEN..]))).min(frame.len());
            let vni = self.group(from) & VNI_MASK;
            if let Some(c) = self.carrier_now() {
                c.carry_out(gateway, vni, &frame[ETH_HDR_LEN..end]);
            }
            return;
        }
        self.to_host(Some(from), frame);
    }

    /// A frame from `hv0` for a cluster network's port here, carried here --
    /// whether it was one of those: an ARP request after one's address,
    /// answered by the switch (`answer_host`) and sent to no port; an IPv4
    /// packet to one's MAC, into its inbox if it is for its address, judged
    /// by its firewall; anything else to one's MAC, nowhere. What is for no
    /// such port goes the host networks' way -- which a broadcast does, to
    /// their ports alone (`forward`): no IPv6 and no broadcast of `hv0`'s
    /// reaches a cluster network.
    fn from_host_vnet(&self, frame: &[u8]) -> bool {
        use netwire::{arp, eth, ip, ARP_LEN, ARP_OP_REQUEST, ETH_HDR_LEN, ETH_TYPE_ARP, ETH_TYPE_IP, IP_HDR_LEN};
        let dst = &frame[..6];
        /* A cluster network's port's MAC, of a port here: a host network's
         * port's, and `hv0`'s own, share its first two bytes. */
        let port = if port_of(dst).is_none() && dst != HOST_MAC {
            vnet_ip(dst).and_then(|addr| Some((self.port_by_ip(addr)?, addr)))
        } else {
            None
        };
        match eth::ether_type(frame) {
            ETH_TYPE_ARP if frame.len() >= ETH_HDR_LEN + ARP_LEN => {
                let request = &frame[ETH_HDR_LEN..];
                let target = arp::target_ip(request);
                if self.port_by_ip(target).is_none() {
                    return port.is_some();
                }
                if arp::opcode(request) == ARP_OP_REQUEST {
                    self.answer_host(request, target);
                }
                true
            }
            ETH_TYPE_IP if frame.len() >= ETH_HDR_LEN + IP_HDR_LEN => {
                let Some((q, addr)) = port else { return self.to_served(dst, frame) };
                if ip::dst(&frame[ETH_HDR_LEN..]) != addr {
                    return true;
                }
                if let Some(claim) = self.judged(q, Dir::In, frame, false) {
                    self.deliver(q, frame, For { group: None, ip: Some(addr), claim: Some(claim) });
                }
                true
            }
            _ => port.is_some(),
        }
    }

    /// An IPv4 frame from `hv0` to the MAC of a port on another host whose
    /// public address this host serves -- the world's packet NAT made the
    /// port's -- carried there by the tunnel's task: whether it was for one
    /// (`from_host_vnet`). `hv0`'s sink's context.
    fn to_served(&self, dst: &[u8], frame: &[u8]) -> bool {
        use netwire::{ip, ETH_HDR_LEN};
        let Some(addr) = vnet_ip(dst).filter(|_| port_of(dst).is_none() && dst != HOST_MAC) else { return false };
        let found = self.served.lock().iter().find(|s| s.0 == addr).map(|s| (s.1, s.2));
        let Some((vni, node)) = found else { return false };
        let packet = &frame[ETH_HDR_LEN..];
        if ip::dst(packet) != addr {
            return true;
        }
        let end = (ETH_HDR_LEN + usize::from(ip::total_len(packet))).min(frame.len());
        match self.carrier_now() {
            Some(c) if c.carry_in(node, vni, &frame[ETH_HDR_LEN..end]) => {
                self.served_in.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                self.refused.fetch_add(1, Ordering::Relaxed);
            }
        }
        true
    }

    /// A frame of the other hosts' ports' for the world into `hv0`, as one
    /// more port's share of its backlog (`to_host`): whether it went.
    fn to_host_served(&self, frame: &[u8]) -> bool {
        let drains = self.vnic.drains();
        if self.served_drain.swap(drains, Ordering::Relaxed) != drains {
            self.served_on_backlog.store(0, Ordering::Relaxed);
        }
        if self.served_on_backlog.load(Ordering::Relaxed) >= BACKLOG / (self.in_use.load(Ordering::Relaxed) + 1) {
            self.served_over_share.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if self.vnic.receive(frame) {
            self.served_on_backlog.fetch_add(1, Ordering::Relaxed);
            self.served_out.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            self.refused.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// `hv0`'s ARP request `request` after the address of a cluster
    /// network's port here, `target`, answered with that port's MAC: put on
    /// `answers` for a task to hand in -- `hv0`'s sink, where this is, may
    /// not -- and the task woken.
    fn answer_host(&self, request: &[u8], target: u32) {
        use netwire::{arp, eth, ARP_OP_REPLY, ETH_HDR_LEN, ETH_TYPE_ARP};
        let mac = vnet_mac(target);
        let (asker_mac, asker) = (arp::sender_mac(request), arp::sender_ip(request));
        let mut reply = [0u8; ARP_FRAME];
        eth::write(&mut reply, &asker_mac, &mac, ETH_TYPE_ARP);
        arp::write(&mut reply[ETH_HDR_LEN..], ARP_OP_REPLY, &mac, target, &asker_mac, asker);
        let mut answers = self.answers.lock();
        if answers.count == TO_HOST {
            drop(answers);
            self.refused.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let at = (answers.head + answers.count) % TO_HOST;
        answers.frames[at] = reply;
        answers.count += 1;
        self.arps.fetch_add(1, Ordering::Relaxed);
        if let Some(wake) = &answers.wake {
            wake.wake_up();
        }
    }

    /// A frame into a port's inbox, and its VM woken, if it is still for
    /// the port's owner (`For`).
    fn deliver(&self, port: usize, frame: &[u8], to: For) {
        self.deliver_rx(port, frame, None, to);
    }

    /// A frame into a port's inbox -- its checksum left to finish, for a NIC
    /// that takes them so -- and its VM woken, if it is still for the port's
    /// owner (`For`).
    fn deliver_rx(&self, port: usize, frame: &[u8], partial: Option<Partial>, to: For) {
        let Some(p) = self.ports.get(port) else { return };
        let mut inbox = p.inbox.lock();
        if inbox.owner.is_none() || !to.holds(p, &inbox) {
            return;
        }
        if inbox.count == INBOX_FRAMES || frame.len() > FRAME {
            p.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let slot = (inbox.head + inbox.count) % INBOX_FRAMES;
        inbox.frames[slot][..frame.len()].copy_from_slice(frame);
        /* Its checksum left to finish only for a NIC that takes it so now:
         * the NIC, or the VM on the port, may have changed since the switch
         * looked. */
        let mut partial = partial;
        if let Some(left) = partial.filter(|_| !p.takes_partial.load(Ordering::Relaxed)) {
            left.complete(&mut inbox.frames[slot][..frame.len()]);
            partial = None;
        }
        inbox.lens[slot] = frame.len() as u16;
        inbox.whole[slot] = NOT_WHOLE;
        inbox.partial[slot] = partial;
        inbox.count += 1;
        p.waiting.store(inbox.count, Ordering::Release);
        Self::wake(p, &inbox);
    }

    /// A send of TCP up to 64 KiB from a guest into a port's inbox whole,
    /// for the port's owner it is still for (`For`) whose NIC takes it so,
    /// and its VM woken: false where it does not, or there is no room for it
    /// whole -- the caller cuts it then.
    fn deliver_whole(&self, port: usize, frame: &[u8], layout: Tcp4, to: For) -> bool {
        let Some(p) = self.ports.get(port) else { return true };
        if frame.len() > SEND {
            return false;
        }
        let mut inbox = p.inbox.lock();
        /* As `deliver_rx` asks, and whether its NIC takes the send whole --
         * here, for the same reason. */
        if inbox.owner.is_none() || !to.holds(p, &inbox) {
            return true;
        }
        if !p.takes_whole.load(Ordering::Relaxed) || inbox.count == INBOX_FRAMES || inbox.free_sends == 0
            || inbox.sends.len() < WHOLE_SENDS
        {
            return false;
        }
        let w = inbox.free_sends.trailing_zeros() as usize;
        inbox.free_sends &= !(1 << w);
        inbox.sends[w][..frame.len()].copy_from_slice(frame);
        inbox.send_lens[w] = frame.len();
        inbox.layouts[w] = Some(layout);
        let slot = (inbox.head + inbox.count) % INBOX_FRAMES;
        inbox.whole[slot] = w as u8;
        inbox.partial[slot] = None;
        inbox.count += 1;
        p.waiting.store(inbox.count, Ordering::Release);
        Self::wake(p, &inbox);
        true
    }

    /// A port's VM woken for what was just put in its inbox -- under the
    /// lock, which keeps the VM there while it is.
    fn wake(p: &Port, inbox: &Inbox) {
        /* Not while its NIC holds its receive interrupts: its round comes
         * for what is here by then on its own, and a kick per frame would
         * hand each over alone -- unless the inbox is filling, which a
         * fast sender can do within the wait. */
        if inbox.count < INBOX_FRAMES / 2
            && p.quiet_until.load(Ordering::Acquire) > kcore::time::boot_time_ns()
        {
            return;
        }
        /* Under the lock: what keeps the VM from going meanwhile. A halted
         * guest's task is woken; one in its guest is kicked out to take the
         * frame now -- at the host's next tick, a busy guest's inbox would
         * have overflowed many times over at line rate. */
        if let Some(owner) = &inbox.owner {
            owner.wake_up();
        }
    }

    /// A frame from port `from`'s guest whose checksum it left to finish: so
    /// to the guest on its network it is for, if that guest's NIC takes it
    /// so (`deliver_rx`), and finished for anyone else -- `hv0` and NAT,
    /// everyone -- going on as a frame sent would.
    fn forward_partial(&self, from: usize, frame: &mut [u8], partial: Partial) {
        let group = self.group(from);
        if is_vnet(group) {
            /* As it is only to a port of its network here, within the MTU */
            if let Some((q, to)) = self.vnet_unicast(from, group, frame) {
                if self.within_mtu(from, frame) && self.judged(from, Dir::Out, frame, false).is_some() {
                    if let Some(claim) = self.judged(q, Dir::In, frame, false) {
                        self.deliver_rx(q, frame, Some(partial), For { group: Some(group), ip: Some(to), claim: Some(claim) });
                    }
                }
                return;
            }
            partial.complete(frame);
            self.forward(Some(from), frame);
            return;
        }
        if frame.len() >= netwire::ETH_HDR_LEN && matches!(self.ingress(from, frame), Ingress::Own | Ingress::Own6)
            && frame[0] & 1 == 0
        {
            if let Some(q) = port_of(&frame[..6]) {
                if q != from && self.group(q) == group {
                    /* Judged by the sender's firewall and the receiver's, as
                     * any frame between two guests is (`forward`). */
                    if self.judged(from, Dir::Out, frame, false).is_some() {
                        if let Some(claim) = self.judged(q, Dir::In, frame, false) {
                            self.deliver_rx(q, frame, Some(partial), For::guest(group, claim));
                        }
                    }
                    return;
                }
            }
        }
        partial.complete(frame);
        self.forward(Some(from), frame);
    }

    /// A send of TCP up to 64 KiB from port `from`'s guest, held up by its
    /// NIC (`offload::check_tcp4`): whole to the port it is for, if that
    /// port's NIC takes it so and has room, and cut into frames for anyone
    /// else -- another guest, `hv0` and NAT, everyone -- each going on as a
    /// frame sent would.
    fn forward_tcp4(&self, from: usize, frame: &[u8], layout: Tcp4) {
        if frame.len() < netwire::ETH_HDR_LEN {
            return;
        }
        if !matches!(self.ingress(from, frame), Ingress::Own) {
            /* Not its own, or not what it says: each frame cut from it would
             * be refused and counted the same, one by one. */
            let mut seg = [0u8; FRAME];
            offload::segment_tcp4(frame, layout, &mut seg, &mut |f| self.forward(Some(from), f));
            return;
        }
        let group = self.group(from);
        if is_vnet(group) {
            /* Its frames past its network's MTU: kept in whole, and told
             * once for all of them. */
            let mtu = self.ports.get(from).map_or(0, |p| p.mtu.load(Ordering::Relaxed) as usize);
            if layout.headers() - netwire::ETH_HDR_LEN + layout.mss() > mtu {
                self.too_big(from, frame);
                return;
            }
            if let Some((q, to)) = self.vnet_unicast(from, group, frame) {
                if self.judged(from, Dir::Out, frame, true).is_none() {
                    return;
                }
                let Some(claim) = self.judged(q, Dir::In, frame, true) else { return };
                if self.deliver_whole(q, frame, layout, For { group: Some(group), ip: Some(to), claim: Some(claim) }) {
                    return;
                }
            }
            let mut seg = [0u8; FRAME];
            offload::segment_tcp4(frame, layout, &mut seg, &mut |f| self.forward(Some(from), f));
            return;
        }
        let dst = &frame[..6];
        if dst[0] & 1 == 0 {
            if let Some(p) = port_of(dst) {
                if p != from && self.group(p) == group {
                    /* Judged once for the whole of it, by its headers: each
                     * frame cut from it, should it be, is of the flow this
                     * makes or finds. */
                    if self.judged(from, Dir::Out, frame, true).is_none() {
                        return;
                    }
                    let Some(claim) = self.judged(p, Dir::In, frame, true) else { return };
                    if self.deliver_whole(p, frame, layout, For::guest(group, claim)) {
                        return;
                    }
                }
            }
        }
        let mut seg = [0u8; FRAME];
        offload::segment_tcp4(frame, layout, &mut seg, &mut |f| self.forward(Some(from), f));
    }

    /// A frame to wherever its destination says, from port `from` -- or
    /// from `hv0`, which a frame from there never goes back to. What a guest
    /// sends goes on only if it is its own; its DHCP message and its router
    /// solicitation are answered here, and go nowhere. IPv6 goes to the
    /// guests and never to `hv0`: nos speaks none.
    fn forward(&self, from: Option<usize>, frame: &[u8]) {
        if frame.len() < netwire::ETH_HDR_LEN {
            return;
        }
        match from {
            Some(port) => {
                let group = self.group(port);
                if is_vnet(group) {
                    self.forward_vnet(port, group, frame);
                    return;
                }
            }
            None => {
                if self.from_host_vnet(frame) {
                    return;
                }
            }
        }
        if let Some(port) = from {
            match self.ingress(port, frame) {
                Ingress::Own | Ingress::Own6 => {}
                Ingress::Solicit => {
                    self.advertise(port, frame);
                    return;
                }
                Ingress::Dhcp => {
                    self.answer_dhcp(port, frame);
                    return;
                }
                Ingress::Spoofed => {
                    if let Some(p) = self.ports.get(port) {
                        p.spoofed.fetch_add(1, Ordering::Relaxed);
                    }
                    return;
                }
                Ingress::Shut => {
                    if let Some(p) = self.ports.get(port) {
                        p.isolated.fetch_add(1, Ordering::Relaxed);
                    }
                    return;
                }
                Ingress::Foreign => {
                    self.foreign.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
            /* What the guest's firewall keeps in goes nowhere: to `hv0` and
             * NAT as to another guest. */
            if self.judged(port, Dir::Out, frame, false).is_none() {
                return;
            }
        }
        let dst = &frame[..6];
        let broadcast = dst[0] & 1 != 0;
        let v6 = netwire::eth::ether_type(frame) == netwire::ETH_TYPE_IPV6;
        if !broadcast {
            if dst == HOST_MAC {
                if !v6 {
                    self.to_host(from, frame);
                } else if from.is_some() {
                    self.foreign.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
            if let Some(p) = self.port_by_mac(dst) {
                if Some(p) != from && self.group(p) != 0 && from.map_or(true, |f| self.may_reach(f, p)) {
                    if let Some(claim) = self.judged(p, Dir::In, frame, false) {
                        self.deliver(p, frame, For { group: from.map(|f| self.group(f)), ip: None, claim: Some(claim) });
                    }
                }
                return;
            }
        }
        /* Everyone: a broadcast, a multicast, a MAC that is no port's --
         * from a guest, at its rate. */
        if let Some(port) = from {
            let Some(p) = self.ports.get(port) else { return };
            if !p.flood.lock().take(kcore::time::boot_time_ns()) {
                p.flooded.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        /* To its own network's ports -- which `deliver` asks again under the
         * inbox's lock -- or, from `hv0`, every host network's port; each
         * judged by its own firewall. A cluster network's has none of it. */
        let group = from.map(|f| self.group(f));
        for p in 0..self.ports.len() {
            let theirs = self.group(p);
            if Some(p) == from || theirs == 0 || is_vnet(theirs) || group.is_some_and(|g| g != theirs) {
                continue;
            }
            if let Some(claim) = self.judged(p, Dir::In, frame, false) {
                self.deliver(p, frame, For { group, ip: None, claim: Some(claim) });
            }
        }
        if !v6 {
            self.to_host(from, frame);
        }
    }

    /// The switch's router advertisement, to the guest on `port` that
    /// solicited one with `solicit`: the prefix to make addresses in, and
    /// no route.
    fn advertise(&self, port: usize, solicit: &[u8]) {
        let asked = ipv6::src(&solicit[netwire::ETH_HDR_LEN..]);
        let to = if asked == ipv6::UNSPECIFIED { ALL_NODES } else { asked };
        let mut frame = [0u8; RA_BYTES];
        router_advert(&port_mac(port), &to, &mut frame);
        self.advertised.fetch_add(1, Ordering::Relaxed);
        self.deliver(port, &frame, For::ASKER);
    }

    /// A guest's DHCP message answered into its port -- with the DNS server
    /// nos has now, asked of the kernel as each is answered: the one it had
    /// when a guest was last started was handed out until the next start,
    /// whatever nos's own lease said since. On a cluster network, from its
    /// gateway, with its mask and MTU. One it does not answer is counted
    /// with what the switch does not carry.
    fn answer_dhcp(&self, port: usize, frame: &[u8]) {
        let (server_ip, mask, mtu) = match self.ports.get(port) {
            Some(p) if is_vnet(p.group.load(Ordering::Acquire)) => (
                p.net.load(Ordering::Relaxed).wrapping_add(1), p.mask.load(Ordering::Relaxed),
                u16::try_from(p.mtu.load(Ordering::Relaxed)).unwrap_or(VNET_MTU_MIN),
            ),
            _ => (HOST_IP, MASK, 0),
        };
        let network = dhcp::Network {
            server_ip, server_mac: HOST_MAC, mask, dns: kcore::net::dns_server().unwrap_or(0), mtu,
        };
        let mut reply = [0u8; dhcp::REPLY_MAX];
        match dhcp::answer(frame, self.ip_of(port), &network, &mut reply) {
            Some(len) => {
                self.dhcp.fetch_add(1, Ordering::Relaxed);
                self.deliver(port, &reply[..len], For::ASKER);
            }
            None => {
                self.foreign.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// A guest's frame onto `hv0`'s backlog, within the port's share of it.
    fn to_host(&self, from: Option<usize>, frame: &[u8]) {
        let Some(p) = from.and_then(|port| self.ports.get(port)) else { return };
        /* Its share counted afresh from each pass of the stack's, which
         * empties the backlog. */
        let drains = self.vnic.drains();
        if p.drain.swap(drains, Ordering::Relaxed) != drains {
            p.on_backlog.store(0, Ordering::Relaxed);
        }
        if p.on_backlog.load(Ordering::Relaxed) >= self.share() {
            p.over_share.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.vnic.receive(frame) {
            p.on_backlog.fetch_add(1, Ordering::Relaxed);
            self.to_host.fetch_add(1, Ordering::Relaxed);
        } else {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A port's share of `hv0`'s backlog: all of it over the ports in use.
    fn share(&self) -> usize {
        BACKLOG / self.in_use.load(Ordering::Relaxed).max(1)
    }

    /// The next frame in a port's inbox, into `buf` -- or a send carried
    /// whole, and how it is to be cut, if `buf` has the room; one it has
    /// not is dropped, counted. A checksum left to finish is finished first,
    /// over the whole frame, for a caller that takes none partial (`finish`).
    fn take(&self, port: usize, buf: &mut [u8], finish: bool) -> Option<(usize, Rx)> {
        let p = self.ports.get(port)?;
        if p.waiting.load(Ordering::Acquire) == 0 {
            return None;
        }
        let mut inbox = p.inbox.lock();
        while inbox.count != 0 {
            let slot = inbox.head;
            let w = usize::from(inbox.whole[slot]);
            let got = if w < WHOLE_SENDS {
                let len = inbox.send_lens[w];
                let layout = inbox.layouts[w];
                if len <= buf.len() {
                    buf[..len].copy_from_slice(&inbox.sends[w][..len]);
                    layout.map(|t| (len, Rx::Whole(t)))
                } else {
                    p.dropped.fetch_add(1, Ordering::Relaxed);
                    None
                }
            } else {
                let whole_len = usize::from(inbox.lens[slot]);
                let mut rx = inbox.partial[slot].map_or(Rx::Done, Rx::Partial);
                if let (Rx::Partial(partial), true) = (rx, finish) {
                    partial.complete(&mut inbox.frames[slot][..whole_len]);
                    rx = Rx::Done;
                }
                let len = whole_len.min(buf.len());
                buf[..len].copy_from_slice(&inbox.frames[slot][..len]);
                Some((len, rx))
            };
            inbox.pop();
            p.waiting.store(inbox.count, Ordering::Release);
            if got.is_some() {
                return got;
            }
        }
        None
    }
}

/// `hv0`'s sink: what the host sends to the guests.
struct Uplink(Arc<Ports>);

impl VnicSink for Uplink {
    fn on_frame(&self, frame: &[u8]) {
        self.0.forward(None, frame);
    }
}

/// What a guest's NIC is given: its port.
pub struct PortBackend {
    port: usize,
    ports: Arc<Ports>,
}

impl hv::nic::Backend for PortBackend {
    fn send(&mut self, frame: &[u8]) {
        self.ports.forward(Some(self.port), frame);
    }

    fn send_partial(&mut self, frame: &mut [u8], partial: Partial) {
        self.ports.forward_partial(self.port, frame, partial);
    }

    fn send_tcp4(&mut self, frame: &[u8], layout: Tcp4) {
        self.ports.forward_tcp4(self.port, frame, layout);
    }

    /// A frame, its checksum finished if it was left to -- a whole send has
    /// no room in `buf` and is dropped, which none is put in for a NIC that
    /// has not said it takes them.
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.ports.take(self.port, buf, true).map(|(len, _)| len)
    }

    fn recv_with(&mut self, buf: &mut [u8]) -> Option<(usize, Rx)> {
        self.ports.take(self.port, buf, false)
    }

    fn takes(&mut self, takes: Takes) {
        if let Some(p) = self.ports.ports.get(self.port) {
            let _inbox = p.inbox.lock();
            p.takes_partial.store(takes.partial, Ordering::Release);
            p.takes_whole.store(takes.whole, Ordering::Release);
        }
    }

    fn quiet_until(&mut self, until_ns: u64) {
        if let Some(p) = self.ports.ports.get(self.port) {
            p.quiet_until.store(until_ns, Ordering::Release);
        }
    }
}

/// The switch: the ports, `hv0`'s sink attached for as long as it lives, and
/// NAT for the guests while there is a way out.
pub struct Switch {
    /// Fields go in the order they are declared. The public addresses mapped
    /// one to one to its ports' VMs first (`map_public`), by port, while NAT
    /// is still on to take them back; then NAT: no guest's packet is sent on
    /// once the switch starts to go.
    ones: Mutex<Vec<(usize, OneToOne)>>,
    /// The public addresses this host serves for the cluster's ports, here
    /// and on other hosts, one to one in NAT (`serve`): before NAT too.
    /// Taken before `vnets`.
    served: Mutex<Vec<(Route, OneToOne)>>,
    /// The cluster networks with ports here, or public addresses this host
    /// serves for ports on them elsewhere, and NAT's networks behind `hv0`
    /// for those that go out: before NAT too. Taken before `nat`.
    vnets: Mutex<Vec<VnetHere>>,
    nat: Mutex<Option<Nat>>,
    /// Then `hv0`'s sink detached, and any call of it waited out, before
    /// the ports go.
    _uplink: Attached,
    ports: Arc<Ports>,
    /// The networks the ports are on, a network's number less one its
    /// place: its name and how many ports are on it -- one with none free
    /// for the next name. A claim holds it throughout, so that a name has
    /// one number for as long as a port is on it.
    networks: Mutex<Vec<Network>>,
}

/// A cluster network with ports here (`Switch::vnets`): its number, its
/// subnet and how it goes out, as its ports were given them, and how many
/// there are; and while it goes out and NAT is on, its place behind `hv0`.
struct VnetHere {
    vni: u32,
    net: u32,
    mask: u32,
    out: bool,
    ports: usize,
    /// The public addresses this host serves for ports on it, here or
    /// elsewhere (`Switch::serve`): it stays while it has either.
    served: usize,
    inside: Option<Inside>,
}

/// A network ports are on (`Switch::networks`).
struct Network {
    name: String,
    ports: usize,
}

/// The place of network `name` in `networks` -- found, or given the first
/// place no port is on, the name's from then on -- or None for no network,
/// a port alone; why not, when every place has ports on it. A name is what
/// `config::valid_network` let through.
fn find_network(networks: &mut Vec<Network>, name: Option<&str>) -> Result<Option<usize>, &'static str> {
    let Some(name) = name else { return Ok(None) };
    if name.is_empty() {
        return Err("a network has a name");
    }
    if let Some(at) = networks.iter().position(|n| n.name == name) {
        return Ok(Some(at));
    }
    let mut owned = String::new();
    owned.try_reserve_exact(name.len()).map_err(|_| "out of memory for the network's name")?;
    owned.push_str(name);
    if let Some(at) = networks.iter().position(|n| n.ports == 0) {
        networks[at] = Network { name: owned, ports: 0 };
        return Ok(Some(at));
    }
    if networks.len() >= MAX_PORTS {
        return Err("as many networks as there are ports are in use");
    }
    networks.try_reserve(1).map_err(|_| "out of memory for the network's name")?;
    networks.push(Network { name: owned, ports: 0 });
    Ok(Some(networks.len() - 1))
}

/// The group of port `port` (`Port::group`): its network's number, the
/// place `at` plus one -- or, alone, `ALONE` and its own number.
fn group(at: Option<usize>, port: usize) -> u32 {
    match at {
        Some(at) => at as u32 + 1,
        None => ALONE | port as u32,
    }
}

impl Switch {
    /// `hv0` opened -- made the first time any load of this module asks --
    /// and the ports made, their storage all taken now.
    pub fn new() -> Result<Switch, String> {
        let vnic = Vnic::open(HOST_IF, HOST_MAC, HOST_IP, MASK)
            .ok_or_else(|| String::from("no hv0: the kernel's virtual NICs are all taken, or one is being made"))?;
        let mut ports = Vec::new();
        ports.try_reserve_exact(MAX_PORTS).map_err(|_| String::from("out of memory for the switch"))?;
        let bindings = Bindings::new().ok_or_else(|| String::from("out of memory for the switch"))?;
        let firewalls = Firewalls::new().ok_or_else(|| String::from("out of memory for the switch"))?;
        for _ in 0..MAX_PORTS {
            ports.push(Port {
                inbox: IrqSpinLock::new(Inbox {
                    owner: None, claim: 0, frames: Vec::new(), lens: [0; INBOX_FRAMES], head: 0, count: 0,
                    whole: [NOT_WHOLE; INBOX_FRAMES], partial: [None; INBOX_FRAMES], sends: Vec::new(),
                    send_lens: [0; WHOLE_SENDS],
                    layouts: [None; WHOLE_SENDS], free_sends: ALL_SENDS,
                }),
                waiting: AtomicUsize::new(0),
                dropped: AtomicU64::new(0),
                spoofed: AtomicU64::new(0),
                drain: AtomicU64::new(0),
                on_backlog: AtomicUsize::new(0),
                over_share: AtomicU64::new(0),
                flood: IrqSpinLock::new(Flood::FULL),
                flooded: AtomicU64::new(0),
                quiet_until: AtomicU64::new(0),
                takes_partial: AtomicBool::new(false),
                takes_whole: AtomicBool::new(false),
                group: AtomicU32::new(0),
                ip: AtomicU32::new(port_ip(ports.len())),
                net: AtomicU32::new(0),
                mask: AtomicU32::new(0),
                mtu: AtomicU32::new(0),
                out: AtomicBool::new(false),
                shut: AtomicBool::new(false),
                gateway: AtomicU64::new(0),
                isolated: AtomicU64::new(0),
                misdirected: AtomicU64::new(0),
                oversize: AtomicU64::new(0),
                too_big: IrqSpinLock::new(TooBig::FULL),
                fw: IrqSpinLock::new(PortFw { owned: false, claim: 0, place: 0, flows: None, refusals: Refusals::FULL }),
                denied_in: AtomicU64::new(0),
                denied_out: AtomicU64::new(0),
                evicted: AtomicU64::new(0),
            });
        }
        let ports = Arc::new(Ports {
            ports, in_use: AtomicUsize::new(0), vnic, to_host: AtomicU64::new(0), refused: AtomicU64::new(0),
            dhcp: AtomicU64::new(0), foreign: AtomicU64::new(0),
            bindings: IrqSpinLock::new(bindings), advertised: AtomicU64::new(0),
            firewalls: IrqSpinLock::new(firewalls),
            arps: AtomicU64::new(0), echoes: AtomicU64::new(0),
            clusters: IrqSpinLock::new(Vec::new()),
            answers: IrqSpinLock::new(ToHost { frames: [[0; ARP_FRAME]; TO_HOST], head: 0, count: 0, wake: None }),
            carrier: IrqSpinLock::new(None),
            served: IrqSpinLock::new(Vec::new()),
            served_drain: AtomicU64::new(0),
            served_on_backlog: AtomicUsize::new(0),
            served_over_share: AtomicU64::new(0),
            served_in: AtomicU64::new(0),
            served_out: AtomicU64::new(0),
        });
        let nat = Mutex::new(None).ok_or_else(|| String::from("out of memory for the switch"))?;
        let ones = Mutex::new(Vec::new()).ok_or_else(|| String::from("out of memory for the switch"))?;
        let mut here = Vec::new();
        here.try_reserve_exact(MAX_PORTS).map_err(|_| String::from("out of memory for the switch"))?;
        let vnets = Mutex::new(here).ok_or_else(|| String::from("out of memory for the switch"))?;
        let mut table = Vec::new();
        table.try_reserve_exact(MAX_PORTS).map_err(|_| String::from("out of memory for the switch"))?;
        let networks = Mutex::new(table).ok_or_else(|| String::from("out of memory for the switch"))?;
        let uplink = vnic.attach(Arc::new(Uplink(ports.clone())))
            .ok_or_else(|| String::from("hv0 has a sink attached already -- another load of hv?"))?;
        let served = Mutex::new(Vec::new()).ok_or_else(|| String::from("out of memory for the switch"))?;
        Ok(Switch { ones, served, vnets, nat, _uplink: uplink, ports, networks })
    }

    /// The guests' way out: NAT on, if it is not yet, through the device
    /// nos's default route is on -- the address they go out from, or why
    /// there is none. Asked at every start of a guest with a NIC, so one
    /// that finds no way out does not keep the next from having one once
    /// nos has a lease. Task context.
    pub fn way_out(&self) -> Result<u32, NatError> {
        let ip = {
            let mut nat = self.nat.lock();
            match nat.as_ref() {
                Some(n) => n.outer().ip(),
                None => {
                    let n = self.ports.vnic.nic().ok_or(NatError::Device)?.nat()?;
                    let ip = n.outer().ip();
                    *nat = Some(n);
                    ip
                }
            }
        };
        /* The cluster networks here that go out, behind `hv0` once NAT is
         * on: those whose ports came before it, and any NAT refused then. */
        let mut vnets = self.vnets.lock();
        for v in vnets.iter_mut() {
            if let Err(why) = self.behind_hv0(v) {
                kcore::klog!(0, "hv: cluster network {} is not behind hv0 for NAT: {}", v.vni, why);
            }
        }
        Ok(ip)
    }

    /// Cluster network `v` put behind `hv0` for NAT, where it goes out, NAT
    /// is on and it is not there yet: why not, where NAT refuses it.
    fn behind_hv0(&self, v: &mut VnetHere) -> Result<(), &'static str> {
        if !v.out || v.inside.is_some() {
            return Ok(());
        }
        let nat = self.nat.lock();
        let Some(n) = nat.as_ref() else { return Ok(()) };
        v.inside = Some(n.inside(v.net, v.mask, v.net.wrapping_add(1)).map_err(|e| e.name())?);
        Ok(())
    }

    /// Public address `public` mapped one to one to the VM on `port` (NAT's
    /// `map`): the world reaches the VM there -- its firewall judging what
    /// comes, the source as it was -- and the VM goes out from it, until its
    /// port is given up (`release`). Why not: no way out, NAT being off, or
    /// NAT's refusal -- an address mapped already, or one that will not do.
    pub fn map_public(&self, port: usize, public: u32) -> Result<(), String> {
        let mut ones = self.ones.lock();
        ones.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        let one = {
            let nat = self.nat.lock();
            let n = nat.as_ref().ok_or_else(|| String::from("the guests have no way out to map it on"))?;
            /* What a public address lets in is held to what the VM takes:
             * Ethernet's on a host's network, its own network's MTU on a
             * cluster's. */
            let mtu = match self.ports.ports.get(port) {
                Some(p) if is_vnet(p.group.load(Ordering::Acquire)) => {
                    u16::try_from(p.mtu.load(Ordering::Relaxed)).unwrap_or(VNET_MTU_MIN)
                }
                _ => PUBLIC_MTU,
            };
            n.map(public, self.ports.ip_of(port), self.ports.mac_of(port), mtu).map_err(|e| String::from(e.name()))?
        };
        ones.push((port, one));
        Ok(())
    }

    /// The public address mapped one to one to the VM on `port`, if one is.
    pub fn public_of(&self, port: usize) -> Option<u32> {
        self.ones.lock().iter().find(|(p, _)| *p == port).map(|(_, o)| o.public())
    }

    /// The address the guests go out from, while NAT is on.
    pub fn nat_address(&self) -> Option<u32> {
        self.nat.lock().as_ref().map(|n| n.outer().ip())
    }

    /// The device the guests go out through, while NAT is on: the one the
    /// world reaches them by, and so the one `hv forward` listens on.
    pub fn outer_nic(&self) -> Option<kcore::net::Nic> {
        self.nat.lock().as_ref().map(|n| n.outer())
    }

    /// The DNS server the guests are given, asked of the kernel afresh: for
    /// a guest's `ip=` as it starts. DHCP asks as it answers.
    pub fn dns(&self) -> Option<u32> {
        kcore::net::dns_server()
    }

    /// A free port for `vm` -- a VM, or a probe -- on `network` or alone,
    /// judged by the firewall in place `place` (`install`): its number, or
    /// why there is none. The first owner of a port gives it its inbox's
    /// room and its table of flows, taken here with no spin lock held.
    pub fn claim<W: Wake + 'static>(&self, vm: &Arc<W>, network: Option<&str>, place: usize)
        -> Result<usize, &'static str>
    {
        let vm: Arc<dyn Wake> = vm.clone();
        self.placed(place)?;
        let mut networks = self.networks.lock();
        let at = find_network(&mut networks, network)?;
        for i in 0..self.ports.ports.len() {
            if self.take(i, &vm, group(at, i), place, None)? {
                if let Some(n) = at.and_then(|n| networks.get_mut(n)) {
                    n.ports += 1;
                }
                return Ok(i);
            }
        }
        Err("every port of the switch is taken")
    }

    /// A free port for `vm` -- a VM, or a probe -- on cluster network
    /// `who.vni`, as `who` (`Identity`) -- its MAC its address's
    /// (`vnet_mac`) -- judged by the firewall in place `place`: its number,
    /// or why there is none. The network's first port here puts it behind
    /// `hv0` for NAT, where it goes out and NAT is on. Refused: an address
    /// another port here has; a network whose other ports here were given
    /// another subnet; a subnet that meets another network's here.
    pub fn claim_vnet<W: Wake + 'static>(&self, vm: &Arc<W>, who: &Identity, place: usize)
        -> Result<usize, &'static str>
    {
        who.check()?;
        let vm: Arc<dyn Wake> = vm.clone();
        self.placed(place)?;
        let mask = who.mask();
        let mut vnets = self.vnets.lock();
        if self.ports.port_by_ip(who.ip).is_some() {
            return Err("another port of the switch has that address");
        }
        let at = match vnets.iter().position(|v| v.vni == who.vni) {
            Some(at) => {
                let v = &vnets[at];
                if v.net != who.net || v.mask != mask || v.out != who.out {
                    return Err("the network's other ports here were given another subnet");
                }
                at
            }
            None => {
                if vnets.iter().any(|v| v.net & v.mask & mask == who.net & v.mask & mask) {
                    return Err("another cluster network here has that subnet");
                }
                /* Room for a network a port, taken with the switch */
                if vnets.len() == vnets.capacity() {
                    return Err("as many cluster networks as there are ports are here");
                }
                vnets.push(VnetHere { vni: who.vni, net: who.net, mask, out: who.out, ports: 0, served: 0,
                                      inside: None });
                vnets.len() - 1
            }
        };
        let mut got = self.behind_hv0(&mut vnets[at]).map(|_| None);
        if got.is_ok() {
            got = Err("every port of the switch is taken");
            for i in 0..self.ports.ports.len() {
                match self.take(i, &vm, VNET | who.vni, place, Some(who)) {
                    Ok(true) => {
                        got = Ok(Some(i));
                        break;
                    }
                    Ok(false) => {}
                    Err(why) => {
                        got = Err(why);
                        break;
                    }
                }
            }
        }
        match got {
            Ok(Some(i)) => {
                vnets[at].ports += 1;
                Ok(i)
            }
            Ok(None) | Err(_) => {
                /* A network made for no port: gone, and from behind `hv0`,
                 * with the lock down. */
                let gone = (vnets[at].ports == 0 && vnets[at].served == 0).then(|| vnets.swap_remove(at));
                drop(vnets);
                drop(gone);
                Err(got.err().unwrap_or("every port of the switch is taken"))
            }
        }
    }

    /// Port `port` for `vm`, and no other, on `network` or alone, judged by
    /// the firewall in place `place`: what a VM with its own address asks
    /// for (`port=`), the address and the MAC being the port's. Why not,
    /// when it is taken or there is no such port.
    pub fn claim_at<W: Wake + 'static>(&self, port: usize, vm: &Arc<W>, network: Option<&str>, place: usize)
        -> Result<usize, &'static str>
    {
        if port >= self.ports.ports.len() {
            return Err("there is no such port on the switch");
        }
        let vm: Arc<dyn Wake> = vm.clone();
        self.placed(place)?;
        let mut networks = self.networks.lock();
        let at = find_network(&mut networks, network)?;
        if self.take(port, &vm, group(at, port), place, None)? {
            if let Some(n) = at.and_then(|n| networks.get_mut(n)) {
                n.ports += 1;
            }
            Ok(port)
        } else {
            Err("that port of the switch is another VM's")
        }
    }

    /// Whether place `place` holds a firewall: a port judged by an empty one
    /// would let nothing through, and say nothing of why.
    fn placed(&self, place: usize) -> Result<(), &'static str> {
        match self.ports.firewalls.lock().get(place) {
            Some(_) => Ok(()),
            None => Err("no firewall is in that place of the switch's"),
        }
    }

    /// Port `port` shut while its VM keeps it: nothing from it or to it goes
    /// on from now (`Port::shut`) -- what hv does to a port vnet revoked.
    /// What is in its inbox stays for its VM to take.
    pub fn shut(&self, port: usize) {
        if let Some(p) = self.ports.ports.get(port) {
            let _inbox = p.inbox.lock();
            p.shut.store(true, Ordering::Release);
        }
    }

    /// Whether port `port` is shut (`shut`).
    pub fn is_shut(&self, port: usize) -> bool {
        self.ports.ports.get(port).is_some_and(|p| p.shut.load(Ordering::Acquire))
    }

    /// The public addresses this host serves from now on, `routes` -- for
    /// the cluster's ports here and on other hosts, this host's node `me`:
    /// each one to one in NAT to its port's address and MAC, its network
    /// behind `hv0`; those no longer among them unmapped, a network with no
    /// port and nothing served gone from behind `hv0`. What could not be
    /// served, said: a network that does not go out, no way out, NAT's
    /// refusal. Task context.
    pub fn serve(&self, me: u64, routes: &[Route]) -> Vec<String> {
        let mut said = Vec::new();
        if !routes.is_empty() && self.nat.lock().is_none() {
            if let Err(e) = self.way_out() {
                said.push(alloc::format!("no way out to serve public addresses on: {:?}", e));
                return said;
            }
        }
        let mut served = self.served.lock();
        let mut emptied = Vec::new();
        {
            let mut vnets = self.vnets.lock();
            /* Those kept: the same mapping -- the port's host is the
             * datapath's to know, and may change under it -- and those gone
             * unmapped now, before another takes their address */
            let mut i = 0;
            while i < served.len() {
                if let Some(r) = routes.iter().find(|r| same_mapping(r, &served[i].0)) {
                    served[i].0.node = r.node;
                    i += 1;
                    continue;
                }
                let (r, one) = served.swap_remove(i);
                drop(one);
                if let Some(at) = vnets.iter().position(|v| v.vni == r.vni) {
                    vnets[at].served = vnets[at].served.saturating_sub(1);
                    if vnets[at].ports == 0 && vnets[at].served == 0 {
                        emptied.push(vnets.swap_remove(at));
                    }
                }
            }
            for r in routes.iter() {
                if served.iter().any(|(s, _)| same_mapping(s, r)) {
                    continue;
                }
                if !r.out {
                    said.push(alloc::format!("{} is on network {}, which does not go out: not served",
                                             netwire::Ipv4(r.public), r.vni));
                    continue;
                }
                if served.try_reserve(1).is_err() {
                    said.push(String::from("out of memory for a public address"));
                    break;
                }
                let mask = prefix_mask(r.prefix);
                let at = match vnets.iter().position(|v| v.vni == r.vni) {
                    Some(at) => at,
                    None if vnets.len() < vnets.capacity() => {
                        vnets.push(VnetHere { vni: r.vni, net: r.net, mask, out: r.out, ports: 0, served: 0,
                                              inside: None });
                        vnets.len() - 1
                    }
                    None => {
                        said.push(String::from("as many cluster networks as there are ports are here"));
                        continue;
                    }
                };
                vnets[at].served += 1;
                let mapped = self.behind_hv0(&mut vnets[at]).map_err(String::from).and_then(|_| {
                    let nat = self.nat.lock();
                    let n = nat.as_ref().ok_or_else(|| String::from("NAT is off"))?;
                    n.map(r.public, r.inner, vnet_mac(r.inner), r.mtu).map_err(|e| String::from(e.name()))
                });
                match mapped {
                    Ok(one) => served.push((*r, one)),
                    Err(why) => {
                        said.push(alloc::format!("{} not served: {}", netwire::Ipv4(r.public), why));
                        vnets[at].served -= 1;
                        if vnets[at].ports == 0 && vnets[at].served == 0 {
                            emptied.push(vnets.swap_remove(at));
                        }
                    }
                }
            }
        }
        /* The datapath's view: the ports on other hosts served here, made
         * here and swapped in, the old one freed with the lock down */
        let mut view = Vec::new();
        if view.try_reserve_exact(served.len()).is_ok() {
            view.extend(served.iter().filter(|(r, _)| r.node != me).map(|(r, _)| (r.inner, r.vni, r.node)));
            let old = core::mem::replace(&mut *self.ports.served.lock(), view);
            drop(old);
        }
        drop(served);
        drop(emptied);
        said
    }

    /// The world's packets for ports elsewhere carried to the tunnel, the
    /// other hosts' ports' packets for the world handed to `hv0`, and those
    /// their share of its backlog kept out.
    pub fn served_counts(&self) -> (u64, u64, u64) {
        let p = &self.ports;
        (p.served_in.load(Ordering::Relaxed), p.served_out.load(Ordering::Relaxed),
         p.served_over_share.load(Ordering::Relaxed))
    }

    /// The public addresses served here: each address, its port's address
    /// and its host's node.
    pub fn served(&self) -> Vec<(u32, u32, u64)> {
        self.served.lock().iter().map(|(r, _)| (r.public, r.inner, r.node)).collect()
    }

    /// Port `port`'s public address routed to host `node` -- another host
    /// -- or, 0, to none or to this host's own NAT: where what its guest
    /// sends the world goes, and whose of the world's packets for it are
    /// let in (`from_tunnel_in`).
    pub fn public_way(&self, port: usize, node: u64) {
        if let Some(p) = self.ports.ports.get(port) {
            p.gateway.store(node, Ordering::Release);
        }
    }

    /// A packet of network `vni` for the world, out of the tunnel from host
    /// `node`'s port whose public address this host serves: into `hv0` from
    /// that port's MAC, as its own guests' are, for NAT to send out from the
    /// public address -- within the other hosts' share of `hv0`. Not for
    /// `hv0`'s own address, the host's networks, a group or the cluster's.
    /// Whether it was such a packet. The receive soft IRQ's.
    pub fn from_tunnel_out(&self, node: u64, vni: u32, packet: &[u8]) -> bool {
        use netwire::{eth, ip, ETH_HDR_LEN, ETH_TYPE_IP, IP_HDR_LEN};
        if packet.len() < IP_HDR_LEN || node == 0 {
            return false;
        }
        let (src, dst) = (ip::src(packet), ip::dst(packet));
        let ports = &self.ports;
        if !ports.served.lock().iter().any(|&(inner, v, n)| inner == src && v == vni && n == node) {
            return false;
        }
        if dst == HOST_IP || dst & MASK == SUBNET || to_group(dst) || ports.in_cluster(dst) {
            return false;
        }
        let len = ETH_HDR_LEN + packet.len();
        if len > FRAME {
            return false;
        }
        let mut frame = [0u8; FRAME];
        eth::write(&mut frame, &HOST_MAC, &vnet_mac(src), ETH_TYPE_IP);
        frame[ETH_HDR_LEN..len].copy_from_slice(packet);
        ports.to_host_served(&frame[..len]);
        true
    }

    /// The world's packet for the port of network `vni` here at its
    /// destination, out of the tunnel from host `node` -- the one that
    /// port's public address is routed to: from an address outside every
    /// network of the cluster and the host's, or from the network's gateway,
    /// which NAT's refusals come from. Into its inbox as a frame from the
    /// gateway's MAC, judged by its firewall, held to its owner and
    /// address. Whether it was for such a port. The receive soft IRQ's.
    pub fn from_tunnel_in(&self, node: u64, vni: u32, packet: &[u8]) -> bool {
        use netwire::{eth, ip, ETH_HDR_LEN, ETH_TYPE_IP, IP_HDR_LEN};
        if packet.len() < IP_HDR_LEN || vni == 0 || vni > VNI_MASK || node == 0 {
            return false;
        }
        let (src, dst) = (ip::src(packet), ip::dst(packet));
        let ports = &self.ports;
        let group = VNET | vni;
        let Some(q) = ports.port_by_ip(dst).filter(|&q| {
            ports.group(q) == group && ports.ports[q].gateway.load(Ordering::Acquire) == node
        }) else {
            return false;
        };
        let from_gateway = src == ports.gateway_of(q);
        if !from_gateway && (ports.in_cluster(src) || src & MASK == SUBNET || to_group(src)) {
            return false;
        }
        let len = ETH_HDR_LEN + packet.len();
        if len > FRAME {
            return false;
        }
        let mut frame = [0u8; FRAME];
        eth::write(&mut frame, &vnet_mac(dst), &HOST_MAC, ETH_TYPE_IP);
        frame[ETH_HDR_LEN..len].copy_from_slice(packet);
        if let Some(claim) = ports.judged(q, Dir::In, &frame[..len], false) {
            ports.deliver(q, &frame[..len], For { group: Some(group), ip: Some(dst), claim: Some(claim) });
        }
        true
    }

    /// What carries a cluster network's packets to its ports on other hosts
    /// from now on -- or nothing: the one before given back, to be let go of
    /// with no lock held.
    pub fn carrier(&self, c: Option<Arc<dyn Carrier>>) -> Option<Arc<dyn Carrier>> {
        core::mem::replace(&mut *self.ports.carrier.lock(), c)
    }

    /// An IPv4 packet of cluster network `vni` out of the tunnel -- its
    /// sender a port on another host, which the tunnel held to the table --
    /// for the port here at its destination: into its inbox as a frame from
    /// the sender's MAC, judged by its firewall, held to its owner and
    /// address. Whether it was for a port here at all. The receive soft
    /// IRQ's: nothing waits, nothing allocates.
    pub fn from_tunnel(&self, vni: u32, packet: &[u8]) -> bool {
        use netwire::{eth, ip, ETH_HDR_LEN, ETH_TYPE_IP, IP_HDR_LEN};
        if packet.len() < IP_HDR_LEN || vni == 0 || vni > VNI_MASK {
            return false;
        }
        let (from, to) = (ip::src(packet), ip::dst(packet));
        let ports = &self.ports;
        let group = VNET | vni;
        let Some(q) = ports.port_by_ip(to).filter(|&q| ports.group(q) == group) else { return false };
        let len = ETH_HDR_LEN + packet.len();
        if len > FRAME {
            return false;
        }
        let mut frame = [0u8; FRAME];
        eth::write(&mut frame, &vnet_mac(to), &vnet_mac(from), ETH_TYPE_IP);
        frame[ETH_HDR_LEN..len].copy_from_slice(packet);
        if let Some(claim) = ports.judged(q, Dir::In, &frame[..len], false) {
            ports.deliver(q, &frame[..len], For { group: Some(group), ip: Some(to), claim: Some(claim) });
        }
        true
    }

    /// The cluster network port `port` is on, by its number, if it is on
    /// one.
    pub fn vnet_of(&self, port: usize) -> Option<u32> {
        let g = self.ports.group(port);
        is_vnet(g).then_some(g & VNI_MASK)
    }

    /// Port `port`'s address: its VM's -- the one its cluster network gave
    /// it, or the port's own on a host's -- and the port's own while free.
    pub fn ip_of(&self, port: usize) -> u32 {
        self.ports.ip_of(port)
    }

    /// Port `port`'s MAC: its address's on a cluster network, the port's own
    /// on a host's.
    pub fn mac_of(&self, port: usize) -> [u8; 6] {
        self.ports.mac_of(port)
    }

    /// The cluster's networks, by vnet's table -- each its subnet's address
    /// and prefix, the subnets apart: what a cluster network's guest is
    /// kept from through its gateway. Taken whole, the old ones let go of
    /// with the lock down; false out of memory, the old ones kept.
    pub fn cluster_networks(&self, subnets: &[(u32, u8)]) -> bool {
        let mut new = Vec::new();
        if new.try_reserve_exact(subnets.len()).is_err() {
            return false;
        }
        for &(net, prefix) in subnets {
            let mask = prefix_mask(prefix);
            new.push((net & mask, mask));
        }
        new.sort_unstable_by_key(|&(net, _)| net);
        let old = core::mem::replace(&mut *self.ports.clusters.lock(), new);
        drop(old);
        true
    }

    /// What is woken as the switch puts an answer to `hv0` on its ring
    /// (`hand_in`): the task that hands them in, or nobody. The one there
    /// before is given back, to be let go of with no lock held.
    pub fn on_answers(&self, wake: Option<Arc<dyn Wake>>) -> Option<Arc<dyn Wake>> {
        core::mem::replace(&mut self.ports.answers.lock().wake, wake)
    }

    /// The switch's answers to `hv0` handed in, the ring emptied: how many
    /// it took. Task context: a frame handed in may take the heap.
    pub fn hand_in(&self) -> usize {
        let mut taken = 0;
        loop {
            let frame = {
                let mut answers = self.ports.answers.lock();
                if answers.count == 0 {
                    break;
                }
                let frame = answers.frames[answers.head];
                answers.head = (answers.head + 1) % TO_HOST;
                answers.count -= 1;
                frame
            };
            if self.ports.vnic.receive(&frame) {
                taken += 1;
            } else {
                self.ports.refused.fetch_add(1, Ordering::Relaxed);
            }
        }
        taken
    }

    /// The network port `port` is on, by name: None for one alone, on a
    /// cluster network, or free.
    pub fn network_of(&self, port: usize) -> Option<String> {
        let g = self.ports.group(port);
        if g == 0 || g & ALONE != 0 || is_vnet(g) {
            return None;
        }
        self.networks.lock().get(g as usize - 1).map(|n| n.name.clone())
    }

    /// Port `i` for `vm`, of `group`, judged by the firewall in place
    /// `place` -- as `who` on a cluster network, as the port's own on a
    /// host's -- if it is free: whether it was, or that its room could not
    /// be had.
    fn take(&self, i: usize, vm: &Arc<dyn Wake>, group: u32, place: usize, who: Option<&Identity>)
        -> Result<bool, &'static str>
    {
        let Some(p) = self.ports.ports.get(i) else { return Ok(false) };
        let roomless = {
            let inbox = p.inbox.lock();
            if inbox.owner.is_some() {
                return Ok(false);
            }
            inbox.frames.is_empty()
        };
        let flowless = p.fw.lock().flows.is_none();
        let mut room = Vec::new();
        let mut sends = Vec::new();
        if roomless {
            room.try_reserve_exact(INBOX_FRAMES).map_err(|_| "out of memory for its port's inbox")?;
            room.resize(INBOX_FRAMES, [0u8; FRAME]);
            sends.try_reserve_exact(WHOLE_SENDS).map_err(|_| "out of memory for its port's inbox")?;
            for _ in 0..WHOLE_SENDS {
                let mut send = Vec::new();
                send.try_reserve_exact(SEND).map_err(|_| "out of memory for its port's inbox")?;
                send.resize(SEND, 0);
                sends.push(send);
            }
        }
        let seed = flow_seed();
        let mut flows = None;
        if flowless {
            flows = Some(Flows::new(FLOWS, seed).ok_or("out of memory for its port's firewall")?);
        }
        let mut inbox = p.inbox.lock();
        /* Taken, or given its room, by another start meanwhile: the room
         * taken here goes after the lock, and the port is not this VM's. */
        if inbox.owner.is_none() && !(inbox.frames.is_empty() && room.is_empty()) {
            if inbox.frames.is_empty() {
                core::mem::swap(&mut inbox.frames, &mut room);
                core::mem::swap(&mut inbox.sends, &mut sends);
            }
            /* An Arc clone under the lock: a count going up, nothing
             * allocated. */
            inbox.owner = Some(vm.clone());
            inbox.claim += 1;
            let claim = inbox.claim;
            inbox.clear();
            p.waiting.store(0, Ordering::Release);
            p.dropped.store(0, Ordering::Relaxed);
            p.spoofed.store(0, Ordering::Relaxed);
            p.on_backlog.store(0, Ordering::Relaxed);
            p.over_share.store(0, Ordering::Relaxed);
            p.flooded.store(0, Ordering::Relaxed);
            p.quiet_until.store(0, Ordering::Release);
            p.takes_partial.store(false, Ordering::Release);
            p.takes_whole.store(false, Ordering::Release);
            p.isolated.store(0, Ordering::Relaxed);
            p.denied_in.store(0, Ordering::Relaxed);
            p.denied_out.store(0, Ordering::Relaxed);
            p.evicted.store(0, Ordering::Relaxed);
            p.misdirected.store(0, Ordering::Relaxed);
            p.oversize.store(0, Ordering::Relaxed);
            *p.too_big.lock() = TooBig::FULL;
            /* Who the VM is, before the group that says it is there */
            let (ip, net, mask, mtu, out) = match who {
                Some(w) => (w.ip, w.net, w.mask(), u32::from(w.mtu), w.out),
                None => (port_ip(i), 0, 0, 0, false),
            };
            p.ip.store(ip, Ordering::Relaxed);
            p.net.store(net, Ordering::Relaxed);
            p.mask.store(mask, Ordering::Relaxed);
            p.mtu.store(mtu, Ordering::Relaxed);
            p.out.store(out, Ordering::Relaxed);
            p.shut.store(false, Ordering::Relaxed);
            p.gateway.store(0, Ordering::Relaxed);
            p.group.store(group, Ordering::Release);
            *p.flood.lock() = Flood { tokens: FLOOD_BURST, at: kcore::time::boot_time_ns() };
            self.ports.in_use.fetch_add(1, Ordering::Relaxed);
            drop(inbox);
            /* Its firewall the new owner's, the inbox's lock down first --
             * never one inside the other. A frame judged meanwhile by the
             * old owner's flows carries the old claim, which its delivery
             * is refused for (`deliver_rx`). */
            let mut f = p.fw.lock();
            match f.flows.as_mut() {
                Some(had) => had.clear(seed),
                None => f.flows = flows.take(),
            }
            f.owned = true;
            f.claim = claim;
            f.place = place;
            f.refusals = Refusals::FULL;
            drop(f);
            drop(flows);
            return Ok(true);
        }
        drop(inbox);
        drop(room);
        drop(sends);
        drop(flows);
        Ok(false)
    }

    /// The port given up, and its VM with it -- dropped here, the lock down.
    /// What was the VM's goes first, and the port last: given up first, a
    /// start that claimed it in between had its own first IPv6 binding and
    /// NAT mapping forgotten by this release, and NAT's answers to the old
    /// VM's flows put in its inbox. The VM's tasks are joined by now, so
    /// nothing of it makes new ones meanwhile.
    pub fn release(&self, port: usize) {
        let Some(p) = self.ports.ports.get(port) else { return };
        /* Its IPv6 addresses go with its VM: the next to claim the port, or
         * any port, may have them. */
        self.ports.bindings.lock().release(port);
        /* Its public address first, mapped to the port's address no longer:
         * the next VM to claim the port has that address -- dropped with the
         * list's lock down. */
        let public = {
            let mut ones = self.ones.lock();
            ones.iter().position(|(p, _)| *p == port).map(|i| ones.swap_remove(i))
        };
        drop(public);
        /* The port's address goes with it to the next VM to claim it: what
         * NAT has mapped for this one's flows goes now, or the next would be
         * answered for them -- a TCP connection's sequence numbers too. */
        if let Some(n) = self.nat.lock().as_ref() {
            n.forget(self.ports.ip_of(port));
        }
        p.gateway.store(0, Ordering::Relaxed);
        let (owner, group) = {
            let mut inbox = p.inbox.lock();
            inbox.clear();
            p.waiting.store(0, Ordering::Release);
            let group = p.group.swap(0, Ordering::AcqRel);
            p.ip.store(port_ip(port), Ordering::Relaxed);
            (inbox.owner.take(), group)
        };
        /* Nothing through a free port's firewall; its flows are emptied for
         * the next claim, which keys them anew. */
        p.fw.lock().owned = false;
        if owner.is_some() {
            self.ports.in_use.fetch_sub(1, Ordering::Relaxed);
        }
        drop(owner);
        /* One port fewer on its network, whose name is the next's once it
         * has none -- and a cluster network with none here gone from behind
         * `hv0`, with the lock down. */
        if is_vnet(group) {
            let mut vnets = self.vnets.lock();
            let gone = match vnets.iter_mut().position(|v| v.vni == group & VNI_MASK) {
                Some(at) => {
                    vnets[at].ports = vnets[at].ports.saturating_sub(1);
                    (vnets[at].ports == 0 && vnets[at].served == 0).then(|| vnets.swap_remove(at))
                }
                None => None,
            };
            drop(vnets);
            drop(gone);
        } else if group != 0 && group & ALONE == 0 {
            if let Some(n) = self.networks.lock().get_mut(group as usize - 1) {
                n.ports = n.ports.saturating_sub(1);
            }
        }
    }

    /// The firewall in place `place` made `rules`, its counts begun again:
    /// what every port judged by it is judged by from the next frame on --
    /// and their flows judged again by it, those it no longer lets through
    /// let go (`recheck`): how many went. Copied in under the lock, which
    /// allocates nothing. Task context.
    pub fn install(&self, place: usize, rules: &Rules) -> Result<usize, &'static str> {
        let roomless = {
            let firewalls = self.ports.firewalls.lock();
            firewalls.places.get(place).ok_or("there is no such place for a firewall")?.is_none()
        };
        let mut room = None;
        if roomless {
            room = Some(firewall_room().ok_or("out of memory for a firewall")?);
        }
        {
            let mut firewalls = self.ports.firewalls.lock();
            let at = firewalls.places.get_mut(place).ok_or("there is no such place for a firewall")?;
            if at.is_none() {
                *at = room.take();
            }
            let f = at.as_mut().and_then(|b| b.first_mut()).ok_or("out of memory for a firewall")?;
            f.used = true;
            f.rules.copy_from(rules);
            f.hits = Hits::NONE;
        }
        /* Room taken for a place another install gave room meanwhile: freed
         * here, the lock down. */
        drop(room);
        Ok(self.recheck(place))
    }

    /// Place `place` emptied, if no port is judged by it: whether it was.
    pub fn uninstall(&self, place: usize) -> bool {
        if self.in_use(place) {
            return false;
        }
        let mut firewalls = self.ports.firewalls.lock();
        match firewalls.places.get_mut(place).and_then(|at| at.as_mut()).and_then(|b| b.first_mut()) {
            Some(f) => {
                f.used = false;
                f.hits = Hits::NONE;
                true
            }
            None => false,
        }
    }

    /// Whether a VM's port is judged by the firewall in place `place`.
    pub fn in_use(&self, place: usize) -> bool {
        self.ports.ports.iter().any(|p| {
            let f = p.fw.lock();
            f.owned && f.place == place
        })
    }

    /// The ports judged by the firewall in place `place`.
    pub fn ports_on(&self, place: usize) -> Vec<usize> {
        let mut on = Vec::new();
        if on.try_reserve_exact(MAX_PORTS).is_ok() {
            for (i, p) in self.ports.ports.iter().enumerate() {
                let f = p.fw.lock();
                if f.owned && f.place == place {
                    on.push(i);
                }
            }
        }
        on
    }

    /// What the firewall in place `place` has judged since its rules were
    /// set (`Hits`).
    pub fn hits(&self, place: usize) -> Option<Hits> {
        self.ports.firewalls.lock().get(place).map(|f| f.hits)
    }

    /// Whether port `port`'s firewall lets in a connection of TCP to its
    /// port `service` from `peer`: what `hv forward` asks of each client it
    /// accepts, its relay reaching the VM as the host, whom the firewall
    /// does not judge. By the rules: the relay's own connection is the flow,
    /// and counted with them.
    pub fn lets_in(&self, port: usize, service: u16, peer: u32) -> bool {
        let Some(p) = self.ports.ports.get(port) else { return false };
        let f = p.fw.lock();
        if !f.owned {
            return false;
        }
        let first = fw::First {
            dir: Dir::In, proto: netwire::IP_PROTO_TCP, port: Some(service), peer,
            same_net: self.ports.same_net(port, peer),
        };
        let mut firewalls = self.ports.firewalls.lock();
        let Some(fwl) = firewalls.get_mut(f.place) else { return false };
        let judged = fwl.rules.judge(&first);
        fwl.hits.count(Dir::In, match judged.by {
            fw::By::Rule(i) => Why::Rule(i),
            fw::By::Policy => Why::Policy,
        });
        let at = f.place;
        drop(firewalls);
        drop(f);
        let allowed = judged.action == fw::Action::Allow;
        if !allowed {
            p.denied_in.fetch_add(1, Ordering::Relaxed);
            if trace::on::<FwDeny>() {
                let (why, rule) = match judged.by {
                    fw::By::Rule(i) => (FwWhy::Rule, i.saturating_add(1)),
                    fw::By::Policy => (FwWhy::Policy, 0),
                };
                /* The client's own port is the relay's to know, not the
                 * switch's: 0. */
                denied_record(port, at, Dir::In, why, rule, peer, Some((service, 0)), netwire::IP_PROTO_TCP);
            }
        }
        allowed
    }

    /// Whether port `port`'s firewall may let in a connection of TCP to its
    /// port `service` from some address off its network
    /// (`Rules::may_let_in`): what `hv forward` says a forward is for
    /// nothing by.
    pub fn may_let_in(&self, port: usize, service: u16) -> bool {
        let Some(p) = self.ports.ports.get(port) else { return false };
        let f = p.fw.lock();
        f.owned && self.ports.firewalls.lock().get(f.place)
            .is_some_and(|fwl| fwl.rules.may_let_in(netwire::IP_PROTO_TCP, service))
    }

    /// Port `port`'s VM judged by the firewall in place `place` from now
    /// on, its open flows judged again by it -- those it does not let
    /// through let go: how many went, or why not.
    pub fn assign(&self, port: usize, place: usize) -> Result<usize, &'static str> {
        self.placed(place)?;
        let p = self.ports.ports.get(port).ok_or("there is no such port on the switch")?;
        let was = {
            let mut f = p.fw.lock();
            if !f.owned {
                return Err("no VM has that port of the switch");
            }
            core::mem::replace(&mut f.place, place)
        };
        Ok(if was == place { 0 } else { self.recheck_port(port, place) })
    }

    /// The flows of every port judged by the firewall in place `place`
    /// judged again by its rules (`recheck_port`): how many went.
    fn recheck(&self, place: usize) -> usize {
        (0..self.ports.ports.len()).map(|q| self.recheck_port(q, place)).sum()
    }

    /// Port `q`'s flows, if it is judged by the firewall in place `place`,
    /// judged again by its rules -- a part of its table at a time, its lock
    /// let go between -- and those they no longer let through let go: how
    /// many.
    fn recheck_port(&self, q: usize, place: usize) -> usize {
        const PART: usize = 256;
        let Some(p) = self.ports.ports.get(q) else { return 0 };
        let mut gone = 0;
        let mut from = 0;
        loop {
            let mut guard = p.fw.lock();
            let PortFw { owned, place: at, flows, .. } = &mut *guard;
            let Some(flows) = flows.as_mut().filter(|f| *owned && *at == place && from < f.capacity()) else {
                break;
            };
            let firewalls = self.ports.firewalls.lock();
            let Some(f) = firewalls.get(place) else { break };
            let ports = &self.ports;
            let (next, went) = flows.sweep(from, PART, kcore::time::boot_time_ns(), &mut |key, initiated| {
                fw::still_allowed(&f.rules, key, initiated, ports.same_net(q, key.peer))
            });
            gone += went;
            from = next;
        }
        gone
    }

    /// The backend a guest's NIC on `port` is built with.
    pub fn backend(&self, port: usize) -> PortBackend {
        PortBackend { port, ports: self.ports.clone() }
    }

    /// The net device `hv0` is, for TCP to the guests.
    pub fn host_nic(&self) -> Option<kcore::net::Nic> {
        self.ports.vnic.nic()
    }

    /// What the VM on a port lost, since it claimed it (`PortCounts`).
    pub fn counts(&self, port: usize) -> PortCounts {
        self.ports.ports.get(port).map_or(PortCounts::default(), |p| PortCounts {
            dropped: p.dropped.load(Ordering::Relaxed),
            spoofed: p.spoofed.load(Ordering::Relaxed),
            over_share: p.over_share.load(Ordering::Relaxed),
            flooded: p.flooded.load(Ordering::Relaxed),
            isolated: p.isolated.load(Ordering::Relaxed),
            misdirected: p.misdirected.load(Ordering::Relaxed),
            oversize: p.oversize.load(Ordering::Relaxed),
            denied_in: p.denied_in.load(Ordering::Relaxed),
            denied_out: p.denied_out.load(Ordering::Relaxed),
            evicted: p.evicted.load(Ordering::Relaxed),
            flows: p.fw.lock().flows.as_ref().map_or(0, |f| f.len()),
        })
    }

    /// What went to the host, what the host would not take, the DHCP
    /// messages answered, and the frames the switch does not carry.
    pub fn host_counts(&self) -> (u64, u64, u64, u64) {
        (self.ports.to_host.load(Ordering::Relaxed), self.ports.refused.load(Ordering::Relaxed),
         self.ports.dhcp.load(Ordering::Relaxed), self.ports.foreign.load(Ordering::Relaxed))
    }

    /// The router solicitations answered, and the IPv6 addresses bound.
    pub fn ipv6_counts(&self) -> (u64, usize) {
        (self.ports.advertised.load(Ordering::Relaxed), self.ports.bindings.lock().count())
    }

    /// The ARP requests answered for the cluster networks' guests and for
    /// `hv0`, and the echoes answered from a cluster network's gateway.
    pub fn vnet_counts(&self) -> (u64, u64) {
        (self.ports.arps.load(Ordering::Relaxed), self.ports.echoes.load(Ordering::Relaxed))
    }
}
