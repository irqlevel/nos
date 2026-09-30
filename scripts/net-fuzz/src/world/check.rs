//! What the machine sends, checked as a receiver on the wire would check it,
//! and against what this stack says of itself: every header whole and every
//! checksum right; nothing longer than a frame; nothing from an address the
//! machine does not have -- a stack that answers for an address it does
//! not own speaks as somebody else -- nor to one no packet may go to; no
//! options, no fragments, no protocol it does not speak; TCP's flags in
//! combinations that mean something.

use netwire::{self as wire, arp, eth, icmp, ip, tcp, udp, ARP_LEN, ETH_HDR_LEN, ETH_TYPE_ARP, ETH_TYPE_IP,
              ICMP_HDR_LEN, IP_HDR_LEN, IP_PROTO_ICMP, IP_PROTO_TCP, IP_PROTO_UDP, MAC_BROADCAST, MAX_FRAME,
              UDP_HDR_LEN};

use crate::machine::ETH0_MAC;

/// What the machine may send as.
#[derive(Clone)]
pub struct Expect {
    /// Its NIC's address.
    pub mac: [u8; 6],
    /// Its IPv4 addresses.
    pub ips: Vec<u32>,
    /// Its DHCP client may send from 0.0.0.0, having no address yet.
    pub dhcp_from_nothing: bool,
}

impl Default for Expect {
    fn default() -> Expect {
        Expect { mac: ETH0_MAC, ips: vec![super::ETH0_IP], dhcp_from_nothing: false }
    }
}

/// What a checked IPv4 frame is: the parts a model looks at next.
pub struct Checked<'a> {
    pub src: u32,
    pub dst: u32,
    pub proto: u8,
    /// The transport's bytes: the segment, datagram or message.
    pub l4: &'a [u8],
}

/// Every frame the machine sends: a finding when it is not one a stack
/// should send.
pub fn tx(e: &Expect, f: &[u8]) {
    if let Err(why) = validate(e, f) {
        panic!("invariant: {} -- the machine sent {}", why, describe(f));
    }
}

macro_rules! ensure {
    ($cond:expr, $($fmt:tt)*) => {
        if !$cond {
            return Err(format!($($fmt)*));
        }
    };
}

fn validate(e: &Expect, f: &[u8]) -> Result<(), String> {
    ensure!(f.len() >= ETH_HDR_LEN && f.len() <= MAX_FRAME, "a frame of {} bytes", f.len());
    ensure!(eth::src(f) == e.mac, "a frame from {:02x?}, not its NIC's address", eth::src(f));
    match eth::ether_type(f) {
        ETH_TYPE_ARP => arp_tx(e, f),
        ETH_TYPE_IP => ipv4_tx(e, f),
        other => Err(format!("a frame of EtherType {:#06x}", other)),
    }
}

fn arp_tx(e: &Expect, f: &[u8]) -> Result<(), String> {
    ensure!(f.len() == ETH_HDR_LEN + ARP_LEN, "an ARP frame of {} bytes", f.len());
    let a = &f[ETH_HDR_LEN..];
    ensure!(wire::be16(a, arp::HW_TYPE) == 1 && wire::be16(a, arp::PROTO_TYPE) == ETH_TYPE_IP
            && a[arp::HW_SIZE] == 6 && a[arp::PROTO_SIZE] == 4, "an ARP message not of Ethernet and IPv4");
    ensure!(arp::sender_mac(a) == e.mac, "an ARP message with another's MAC as its sender");
    ensure!(e.ips.contains(&arp::sender_ip(a)) || arp::sender_ip(a) == 0,
            "an ARP message saying an address the machine does not have is its");
    ensure!(arp::sender_ip(a) == 0 || usable(arp::sender_ip(a)), "an ARP message saying {} is the machine's, an \
            address no host may have", wire::Ipv4(arp::sender_ip(a)));
    /* A reply goes where the request said its sender is: whether that is
     * somewhere a reply should go is the ARP model's to say. */
    match arp::opcode(a) {
        1 => ensure!(eth::dst(f) == MAC_BROADCAST, "an ARP request not broadcast"),
        2 => ensure!(arp::sender_ip(a) != 0, "an ARP reply saying 0.0.0.0 is the machine's"),
        op => return Err(format!("an ARP message of opcode {}", op)),
    }
    Ok(())
}

/// Whether a host may have `addr` (RFC 1122 3.2.1.3): not "this network",
/// not loopback, not multicast or reserved.
fn usable(addr: u32) -> bool {
    addr >> 24 != 0 && addr >> 24 != 127 && addr < 0xE000_0000
}

/// Whether `addr` is one no packet may be sent to (RFC 1122 3.2.1.3): "this
/// network", a loopback address -- which must never appear outside a host
/// -- or a reserved one. The limited broadcast and a multicast group may be.
fn undeliverable(addr: u32) -> bool {
    addr >> 24 == 0 || addr >> 24 == 127 || (addr >> 28 == 0xF && addr != u32::MAX)
}

/// Whether `addr` is a broadcast of the subnet eth0 is on, as it is now.
fn subnet_broadcast(addr: u32) -> bool {
    net::Nic::find("eth0").is_some_and(|nic| {
        let (ip, mask) = (nic.ip(), nic.mask());
        ip != 0 && mask.count_zeros() >= 2 && addr | mask == u32::MAX && addr & mask == ip & mask
    })
}

fn ipv4_tx(e: &Expect, f: &[u8]) -> Result<(), String> {
    ensure!(f.len() >= ETH_HDR_LEN + IP_HDR_LEN, "an IPv4 frame of {} bytes", f.len());
    let p = &f[ETH_HDR_LEN..];
    ensure!(p[ip::VERSION_IHL] == 0x45, "an IPv4 header {:#04x}: version 4 and no options is all this stack sends",
            p[ip::VERSION_IHL]);
    ensure!(usize::from(ip::total_len(p)) == p.len(), "an IPv4 packet of {} bytes in {} of frame",
            ip::total_len(p), p.len());
    ensure!(wire::checksum(&p[..IP_HDR_LEN]) == 0, "an IPv4 header with a wrong checksum");
    ensure!(p[ip::TTL] != 0, "a packet with no time to live");
    ensure!(wire::be16(p, ip::FRAG_OFF) & !ip::DONT_FRAGMENT == 0, "a fragment");
    let (src, dst) = (ip::src(p), ip::dst(p));
    let proto = ip::protocol(p);
    let l4 = &p[IP_HDR_LEN..];
    let dhcp = proto == IP_PROTO_UDP && l4.len() >= UDP_HDR_LEN && udp::src_port(l4) == 68 && udp::dst_port(l4) == 67;
    ensure!(e.ips.contains(&src) || (src == 0 && dhcp && e.dhcp_from_nothing),
            "a packet from an address the machine does not have (it has {:08x?})", e.ips);
    ensure!(src == 0 || usable(src), "a packet from {}, an address no host may have", wire::Ipv4(src));
    ensure!(!undeliverable(dst), "a packet to {}, an address no packet goes to", wire::Ipv4(dst));
    /* RFC 1122 3.3.6: what goes to the link's broadcast or a multicast
     * address is a broadcast or a multicast -- a unicast there is somebody
     * else's datagram handed to everyone on the link. */
    if eth::dst(f)[0] & 1 != 0 {
        ensure!(dst == u32::MAX || dst >> 28 == 0xE || subnet_broadcast(dst),
                "a packet to {} sent to the link address {:02x?}", wire::Ipv4(dst), eth::dst(f));
    }
    /* A group's datagram goes to the group's own link address (RFC 1112
     * 6.4). */
    if dst >> 28 == 0xE {
        let g = (dst & 0x007F_FFFF).to_be_bytes();
        ensure!(eth::dst(f) == [0x01, 0x00, 0x5E, g[1], g[2], g[3]], "a packet to the group {} sent to {:02x?}",
                wire::Ipv4(dst), eth::dst(f));
    }
    match proto {
        IP_PROTO_ICMP => {
            ensure!(l4.len() >= ICMP_HDR_LEN, "an ICMP message of {} bytes", l4.len());
            ensure!(wire::checksum(l4) == 0, "an ICMP message with a wrong checksum");
            ensure!(matches!(icmp::kind(l4), icmp::ECHO_REPLY | icmp::ECHO_REQUEST) && icmp::code(l4) == 0,
                    "an ICMP message of type {} code {}", icmp::kind(l4), icmp::code(l4));
        }
        IP_PROTO_UDP => {
            ensure!(l4.len() >= UDP_HDR_LEN && usize::from(udp::length(l4)) == l4.len(), "a UDP datagram whose \
                    length is not its own");
            ensure!(udp::src_port(l4) != 0 && udp::dst_port(l4) != 0, "a UDP datagram to or from port 0");
            let sum = wire::be16(l4, udp::CHECKSUM);
            ensure!(sum == 0 || wire::transport_checksum(IP_PROTO_UDP, src, dst, l4) == 0,
                    "a UDP datagram with a wrong checksum");
        }
        IP_PROTO_TCP => tcp_tx(src, dst, l4)?,
        other => return Err(format!("IP protocol {}", other)),
    }
    Ok(())
}

fn tcp_tx(src: u32, dst: u32, s: &[u8]) -> Result<(), String> {
    ensure!(s.len() >= tcp::HDR_LEN, "a TCP segment of {} bytes", s.len());
    let hlen = tcp::header_len(s);
    ensure!(hlen >= tcp::HDR_LEN && hlen <= s.len(), "a TCP header of {} bytes in a segment of {}", hlen, s.len());
    ensure!(tcp::checksum(src, dst, s) == 0, "a TCP segment with a wrong checksum");
    ensure!(tcp::src_port(s) != 0 && tcp::dst_port(s) != 0, "a TCP segment to or from port 0");
    let flags = tcp::flags(s);
    let syn = flags & tcp::SYN != 0;
    let fin = flags & tcp::FIN != 0;
    let rst = flags & tcp::RST != 0;
    ensure!(!(syn && (fin || rst)), "a TCP segment with flags {:#04x}", flags);
    ensure!(flags & 0xC0 == 0 && s[tcp::DATA_OFF] & 0x0F == 0, "a TCP segment with reserved bits set");
    let payload = s.len() - hlen;
    ensure!(!(rst && payload != 0) && !(syn && payload != 0), "data on a SYN or a RST");
    ensure!(syn || rst || flags & tcp::ACK_FLAG != 0, "a TCP segment with no ACK, flags {:#04x}", flags);
    /* The options this stack sends: an MSS on a SYN, and nothing else. */
    if hlen > tcp::HDR_LEN {
        ensure!(syn && hlen == tcp::HDR_LEN + 4 && s[tcp::HDR_LEN] == tcp::OPT_MSS && s[tcp::HDR_LEN + 1] == 4,
                "TCP options {:02x?}", &s[tcp::HDR_LEN..hlen]);
    }
    Ok(())
}

/// The frame's IPv4 parts, when it is an IPv4 frame -- for a model to look
/// at after `tx` has checked it.
pub fn ipv4_parts(f: &[u8]) -> Option<Checked<'_>> {
    if f.len() < ETH_HDR_LEN + IP_HDR_LEN || eth::ether_type(f) != ETH_TYPE_IP {
        return None;
    }
    let p = &f[ETH_HDR_LEN..];
    let ihl = ip::header_len(p).max(IP_HDR_LEN).min(p.len());
    Some(Checked { src: ip::src(p), dst: ip::dst(p), proto: ip::protocol(p), l4: &p[ihl..] })
}

fn flag_names(flags: u8) -> String {
    let names = [(tcp::SYN, "SYN"), (tcp::ACK_FLAG, "ACK"), (tcp::FIN, "FIN"), (tcp::RST, "RST"), (tcp::PSH, "PSH")];
    let v: Vec<&str> = names.iter().filter(|(b, _)| flags & b != 0).map(|(_, n)| *n).collect();
    v.join("|")
}

/// A frame, in a line: for a finding's report, and `--trace`.
pub fn describe(f: &[u8]) -> String {
    if f.len() < ETH_HDR_LEN {
        return format!("a runt of {} bytes", f.len());
    }
    match eth::ether_type(f) {
        ETH_TYPE_ARP if f.len() >= ETH_HDR_LEN + ARP_LEN => {
            let a = &f[ETH_HDR_LEN..];
            format!("ARP op {} {} ({:02x?}) asks for {} -> {:02x?}", arp::opcode(a), wire::Ipv4(arp::sender_ip(a)),
                    arp::sender_mac(a), wire::Ipv4(arp::target_ip(a)), eth::dst(f))
        }
        ETH_TYPE_IP if f.len() >= ETH_HDR_LEN + IP_HDR_LEN => {
            let p = &f[ETH_HDR_LEN..];
            let ihl = ip::header_len(p).max(IP_HDR_LEN).min(p.len());
            let l4 = &p[ihl..];
            let head = format!("{} -> {} ttl {} len {}", wire::Ipv4(ip::src(p)), wire::Ipv4(ip::dst(p)), p[ip::TTL],
                               ip::total_len(p));
            match ip::protocol(p) {
                IP_PROTO_TCP if l4.len() >= tcp::HDR_LEN => {
                    let hlen = tcp::header_len(l4).max(tcp::HDR_LEN).min(l4.len());
                    format!("TCP {} ports {} -> {} [{}] seq {} ack {} win {} data {}", head, tcp::src_port(l4),
                            tcp::dst_port(l4), flag_names(tcp::flags(l4)), tcp::seq(l4), tcp::ack(l4),
                            tcp::window(l4), l4.len() - hlen)
                }
                IP_PROTO_UDP if l4.len() >= UDP_HDR_LEN => {
                    format!("UDP {} ports {} -> {} length {}", head, udp::src_port(l4), udp::dst_port(l4),
                            udp::length(l4))
                }
                IP_PROTO_ICMP if l4.len() >= ICMP_HDR_LEN => {
                    format!("ICMP {} type {} code {} id {} seq {}", head, icmp::kind(l4), icmp::code(l4), icmp::id(l4),
                            icmp::seq(l4))
                }
                proto => format!("IP proto {} {}", proto, head),
            }
        }
        t => format!("EtherType {:#06x}, {} bytes", t, f.len()),
    }
}
