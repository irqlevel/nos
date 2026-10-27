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
//! nothing for the guests' own subnet (`net/src/nat.rs`), and the stack
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
//! its length, or a first fragment that stops short of it -- goes nowhere
//! either: a router advertisement could be one (RFC 7112). The packet looked
//! at is the one its receiver takes, to the length its header says.
//!
//! The switch is also the guests' way out: while it exists, NAT is on from
//! `hv0` through the device nos's default route is on, so a guest reaches
//! whatever nos can, from nos's address (`net/src/nat.rs`). And it answers
//! DHCP itself (`dhcp.rs`): a guest that asks is told its port's address,
//! nos as its router and the DNS server nos was given as its own -- which
//! `ip=` also carries, as its `dns0`.
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

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use hv::offload::{self, Partial, Rx, Takes, Tcp4};

use kcore::consts::NS_PER_SEC;
use kcore::net::{Nat, NatError};
use kcore::sync::{IrqSpinLock, Mutex};
use kcore::vnic::{Attached, Vnic, VnicSink, BACKLOG};

use netwire::{icmpv6, ipv6};

use crate::dhcp;
use crate::vms::Shared;

/// The most ports: a VM each.
pub const MAX_PORTS: usize = 16;
/// A port alone on the switch: its group (`Port::group`) is this and its
/// number, apart from every network's, which are 1 to `MAX_PORTS`.
const ALONE: u32 = 1 << 31;
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

/// The host's end: its name, MAC and address, and the subnet's mask.
const HOST_IF: &str = "hv0";
const HOST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0x64, 0x01];
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
    let ip = alloc::format!("ip={}::{}:{}:::off", dotted(port_ip(port)), dotted(HOST_IP), dotted(MASK));
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

/// What waits for one guest's NIC.
struct Inbox {
    /// The VM on the port; None while it is free.
    owner: Option<Arc<Shared>>,
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

/// A port's sends to every port: how many it may make now, and when that was
/// last topped up.
struct Flood {
    tokens: u64,
    at: u64,
}

impl Flood {
    const FULL: Flood = Flood { tokens: FLOOD_BURST, at: 0 };

    /// Whether one more may go at `now`: `FLOOD_BURST` at once, and
    /// `FLOOD_PER_SEC` after.
    fn take(&mut self, now: u64) -> bool {
        let earned = now.saturating_sub(self.at).saturating_mul(FLOOD_PER_SEC) / NS_PER_SEC;
        if earned != 0 {
            self.tokens = self.tokens.saturating_add(earned).min(FLOOD_BURST);
            self.at = now;
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
    /// ports on one share it -- or `ALONE` and its own; 0 while it is free.
    /// Written under `inbox`'s lock with its `owner`, so that read there it
    /// is the owner's (`deliver_rx`); read without, it was a moment ago.
    group: AtomicU32,
    /// Frames its guest sent to another network's guest, dropped.
    isolated: AtomicU64,
}

/// What a port's guest lost, since it claimed the port: frames for it its
/// inbox had no room for, frames it sent as another guest, frames for `hv0`
/// past its share of the backlog, sends to every port past their rate, and
/// frames for another network's guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PortCounts {
    pub dropped: u64,
    pub spoofed: u64,
    pub over_share: u64,
    pub flooded: u64,
    pub isolated: u64,
}

/// The ports, shared by the switch, every guest's NIC and `hv0`'s sink.
pub struct Ports {
    ports: Vec<Port>,
    /// The ports a VM has: how many `hv0`'s backlog is shared among.
    in_use: AtomicUsize,
    vnic: Vnic,
    to_host: AtomicU64,
    refused: AtomicU64,
    /// The DNS server DHCP hands out, as last asked of the kernel; 0 for
    /// none.
    dns: AtomicU32,
    /// DHCP messages answered.
    dhcp: AtomicU64,
    /// Frames from the guests the switch does not carry: neither IPv4, ARP
    /// nor IPv6, too short to be, IPv6 for nos -- which speaks none -- or
    /// IPv6 hiding what it carries.
    foreign: AtomicU64,
    /// The IPv6 addresses the guests have bound (`Bindings`).
    bindings: IrqSpinLock<Bindings>,
    /// Router solicitations answered.
    advertised: AtomicU64,
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

/// What `frame`, from the guest on `port`, is: IPv4 or ARP. The ARP fields
/// are read where the kernel's ARP table reads them, whatever the header
/// says of their sizes.
fn ingress(port: usize, frame: &[u8]) -> Ingress {
    use netwire::{arp, eth, ip, ARP_LEN, ETH_HDR_LEN, ETH_TYPE_ARP, ETH_TYPE_IP, IP_HDR_LEN};
    let Some(body) = frame.get(ETH_HDR_LEN..) else { return Ingress::Foreign };
    let (mac, addr) = (port_mac(port), port_ip(port));
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
    /// `ingress6`).
    fn ingress(&self, port: usize, frame: &[u8]) -> Ingress {
        if frame.len() >= netwire::ETH_HDR_LEN && netwire::eth::ether_type(frame) == netwire::ETH_TYPE_IPV6 {
            return self.ingress6(port, frame);
        }
        ingress(port, frame)
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

    /// A frame into a port's inbox, and its VM woken: from a guest of
    /// `group`, or from `hv0` or the switch itself (None).
    fn deliver(&self, port: usize, frame: &[u8], group: Option<u32>) {
        self.deliver_rx(port, frame, None, group);
    }

    /// A frame into a port's inbox -- its checksum left to finish, for a NIC
    /// that takes them so -- and its VM woken: from a guest of `group`, for
    /// the port's only if it is of that group, or from `hv0` or the switch
    /// itself (None), for anyone's.
    fn deliver_rx(&self, port: usize, frame: &[u8], partial: Option<Partial>, group: Option<u32>) {
        let Some(p) = self.ports.get(port) else { return };
        let mut inbox = p.inbox.lock();
        /* Nobody's, or another network's: the group asked here, where it
         * is the owner's -- the port may have changed hands since the
         * switch looked, and what was for the guest on it then is for
         * nobody now. */
        if inbox.owner.is_none() || group.is_some_and(|g| p.group.load(Ordering::Relaxed) != g) {
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

    /// A send of TCP up to 64 KiB from a guest of `group` into a port's
    /// inbox whole, for a VM of that group whose NIC takes it so, and its VM
    /// woken: false where it does not, or there is no room for it whole --
    /// the caller cuts it then.
    fn deliver_whole(&self, port: usize, frame: &[u8], layout: Tcp4, group: u32) -> bool {
        let Some(p) = self.ports.get(port) else { return true };
        if frame.len() > SEND {
            return false;
        }
        let mut inbox = p.inbox.lock();
        /* As `deliver_rx` asks, and whether its NIC takes the send whole --
         * here, for the same reason. */
        if inbox.owner.is_none() || p.group.load(Ordering::Relaxed) != group {
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
        if frame.len() >= netwire::ETH_HDR_LEN && matches!(self.ingress(from, frame), Ingress::Own | Ingress::Own6)
            && frame[0] & 1 == 0
        {
            if let Some(q) = port_of(&frame[..6]) {
                let group = self.group(from);
                if q != from && self.group(q) == group {
                    self.deliver_rx(q, frame, Some(partial), Some(group));
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
        let dst = &frame[..6];
        if dst[0] & 1 == 0 {
            if let Some(p) = port_of(dst) {
                let group = self.group(from);
                if p != from && self.group(p) == group && self.deliver_whole(p, frame, layout, group) {
                    return;
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
                Ingress::Foreign => {
                    self.foreign.fetch_add(1, Ordering::Relaxed);
                    return;
                }
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
            if let Some(p) = port_of(dst) {
                if Some(p) != from && from.map_or(true, |f| self.may_reach(f, p)) {
                    self.deliver(p, frame, from.map(|f| self.group(f)));
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
        /* To its own network's ports -- which `deliver` picks out -- or,
         * from `hv0`, every port. */
        let group = from.map(|f| self.group(f));
        for p in 0..self.ports.len() {
            if Some(p) != from {
                self.deliver(p, frame, group);
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
        self.deliver(port, &frame, None);
    }

    fn answer_dhcp(&self, port: usize, frame: &[u8]) {
        let network = dhcp::Network {
            server_ip: HOST_IP, server_mac: HOST_MAC, mask: MASK,
            dns: self.dns.load(Ordering::Relaxed),
        };
        let mut reply = [0u8; dhcp::REPLY_MAX];
        if let Some(len) = dhcp::answer(frame, port_ip(port), &network, &mut reply) {
            self.dhcp.fetch_add(1, Ordering::Relaxed);
            self.deliver(port, &reply[..len], None);
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
    /// Fields go in the order they are declared. NAT first: no guest's packet
    /// is sent on once the switch starts to go.
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
        for _ in 0..MAX_PORTS {
            ports.push(Port {
                inbox: IrqSpinLock::new(Inbox {
                    owner: None, frames: Vec::new(), lens: [0; INBOX_FRAMES], head: 0, count: 0,
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
                isolated: AtomicU64::new(0),
            });
        }
        let ports = Arc::new(Ports {
            ports, in_use: AtomicUsize::new(0), vnic, to_host: AtomicU64::new(0), refused: AtomicU64::new(0),
            dns: AtomicU32::new(0), dhcp: AtomicU64::new(0), foreign: AtomicU64::new(0),
            bindings: IrqSpinLock::new(bindings), advertised: AtomicU64::new(0),
        });
        let nat = Mutex::new(None).ok_or_else(|| String::from("out of memory for the switch"))?;
        let mut table = Vec::new();
        table.try_reserve_exact(MAX_PORTS).map_err(|_| String::from("out of memory for the switch"))?;
        let networks = Mutex::new(table).ok_or_else(|| String::from("out of memory for the switch"))?;
        let uplink = vnic.attach(Arc::new(Uplink(ports.clone())))
            .ok_or_else(|| String::from("hv0 has a sink attached already -- another load of hv?"))?;
        Ok(Switch { nat, _uplink: uplink, ports, networks })
    }

    /// The guests' way out: NAT on, if it is not yet, through the device
    /// nos's default route is on -- the address they go out from, or why
    /// there is none. Asked at every start of a guest with a NIC, so one
    /// that finds no way out does not keep the next from having one once
    /// nos has a lease. Task context.
    pub fn way_out(&self) -> Result<u32, NatError> {
        let mut nat = self.nat.lock();
        if let Some(n) = nat.as_ref() {
            return Ok(n.outer().ip());
        }
        let n = self.ports.vnic.nic().ok_or(NatError::Device)?.nat()?;
        let ip = n.outer().ip();
        *nat = Some(n);
        Ok(ip)
    }

    /// The address the guests go out from, while NAT is on.
    pub fn nat_address(&self) -> Option<u32> {
        self.nat.lock().as_ref().map(|n| n.outer().ip())
    }

    /// The DNS server the guests are given, asked of the kernel afresh: for
    /// `ip=`, and for what DHCP hands out from here on.
    pub fn dns(&self) -> Option<u32> {
        let dns = kcore::net::dns_server();
        self.ports.dns.store(dns.unwrap_or(0), Ordering::Relaxed);
        dns
    }

    /// A free port for `vm`, on `network` or alone: its number, or why there
    /// is none. The first VM on a port gives it its inbox's room, taken here
    /// with no spin lock held.
    pub fn claim(&self, vm: &Arc<Shared>, network: Option<&str>) -> Result<usize, &'static str> {
        let mut networks = self.networks.lock();
        let at = find_network(&mut networks, network)?;
        for i in 0..self.ports.ports.len() {
            if self.take(i, vm, group(at, i))? {
                if let Some(n) = at.and_then(|n| networks.get_mut(n)) {
                    n.ports += 1;
                }
                return Ok(i);
            }
        }
        Err("every port of the switch is taken")
    }

    /// Port `port` for `vm`, and no other, on `network` or alone: what a VM
    /// with its own address asks for (`port=`), the address and the MAC
    /// being the port's. Why not, when it is taken or there is no such port.
    pub fn claim_at(&self, port: usize, vm: &Arc<Shared>, network: Option<&str>) -> Result<usize, &'static str> {
        if port >= self.ports.ports.len() {
            return Err("there is no such port on the switch");
        }
        let mut networks = self.networks.lock();
        let at = find_network(&mut networks, network)?;
        if self.take(port, vm, group(at, port))? {
            if let Some(n) = at.and_then(|n| networks.get_mut(n)) {
                n.ports += 1;
            }
            Ok(port)
        } else {
            Err("that port of the switch is another VM's")
        }
    }

    /// The network port `port` is on, by name: None for one alone, or free.
    pub fn network_of(&self, port: usize) -> Option<String> {
        let g = self.ports.group(port);
        if g == 0 || g & ALONE != 0 {
            return None;
        }
        self.networks.lock().get(g as usize - 1).map(|n| n.name.clone())
    }

    /// Port `i` for `vm`, of `group`, if it is free: whether it was, or that
    /// its room could not be had.
    fn take(&self, i: usize, vm: &Arc<Shared>, group: u32) -> Result<bool, &'static str> {
        let Some(p) = self.ports.ports.get(i) else { return Ok(false) };
        let roomless = {
            let inbox = p.inbox.lock();
            if inbox.owner.is_some() {
                return Ok(false);
            }
            inbox.frames.is_empty()
        };
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
            p.group.store(group, Ordering::Release);
            *p.flood.lock() = Flood { tokens: FLOOD_BURST, at: kcore::time::boot_time_ns() };
            self.ports.in_use.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        drop(inbox);
        drop(room);
        drop(sends);
        Ok(false)
    }

    /// The port given up, and its VM with it -- dropped here, the lock down.
    pub fn release(&self, port: usize) {
        let Some(p) = self.ports.ports.get(port) else { return };
        let (owner, group) = {
            let mut inbox = p.inbox.lock();
            inbox.clear();
            p.waiting.store(0, Ordering::Release);
            (inbox.owner.take(), p.group.swap(0, Ordering::AcqRel))
        };
        if owner.is_some() {
            self.ports.in_use.fetch_sub(1, Ordering::Relaxed);
        }
        drop(owner);
        /* One port fewer on its network, whose name is the next's once it
         * has none. */
        if group != 0 && group & ALONE == 0 {
            if let Some(n) = self.networks.lock().get_mut(group as usize - 1) {
                n.ports = n.ports.saturating_sub(1);
            }
        }
        /* Its IPv6 addresses go with its VM: the next to claim the port, or
         * any port, may have them. */
        self.ports.bindings.lock().release(port);
        /* The port's address goes with it to the next VM to claim it: what
         * NAT has mapped for this one's flows goes now, or the next would be
         * answered for them -- a TCP connection's sequence numbers too. */
        if let Some(n) = self.nat.lock().as_ref() {
            n.forget(port_ip(port));
        }
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
}
