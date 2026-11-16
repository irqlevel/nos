//! What the firewall reads of a frame: of IPv4, the addresses, the protocol,
//! the ports or an echo's identifier, TCP's flags, and of an ICMP error the
//! packet it quotes; of IPv6, whether it is neighbour discovery. Read the way
//! the switch reads a frame, as a byte slice, each length held to what is
//! there: a header that does not fit is a broken packet, not a read past the
//! end.

use netwire::{eth, icmp, icmpv6, ip, ipv6, tcp, udp, ETH_HDR_LEN, ETH_TYPE_IP, ETH_TYPE_IPV6, ICMP_HDR_LEN,
              IP_HDR_LEN, IP_PROTO_ICMP, IP_PROTO_ICMPV6, IP_PROTO_TCP, IP_PROTO_UDP, UDP_HDR_LEN};

/* The ICMP errors whose quoted packet relates them to a flow (RFC 792):
 * destination unreachable, time exceeded, parameter problem. */
const PARAMETER_PROBLEM: u8 = 12;
/// What an error quotes at the least past the header: both ports, or an
/// echo's identifier.
const QUOTED_L4: usize = 8;

/// What a frame is, to the firewall.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Read {
    V4(V4),
    /// An IPv4 fragment: it carries no ports, or not the first packet's, so
    /// no rule and no flow can be held to it.
    Fragment,
    /// IPv4 whose headers do not hold together, or cut short.
    Broken,
    /// IPv6; `neighbour` for neighbour discovery and router solicitation,
    /// which find the way and carry nothing.
    V6 { neighbour: bool },
    /// Neither: ARP, or what the switch carries of no IP at all.
    NotIp,
}

/// An IPv4 packet, whole, its transport header there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct V4 {
    pub src: u32,
    pub dst: u32,
    pub proto: u8,
    /// TCP's or UDP's ports, source then destination.
    pub ports: Option<(u16, u16)>,
    /// TCP's flags; 0 for anything else.
    pub tcp_flags: u8,
    pub icmp: Option<Icmp>,
}

/// An ICMP message, as a flow sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icmp {
    /// An echo request or its reply, by its identifier.
    Echo { id: u16, request: bool },
    /// An error about a packet, which it quotes.
    Error(Quoted),
    /// Anything else, or an error that quotes too little to say what of.
    Other,
}

/// What an ICMP error quotes of the packet it is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Quoted {
    pub src: u32,
    pub dst: u32,
    pub proto: u8,
    /// TCP's or UDP's ports, source then destination.
    pub ports: Option<(u16, u16)>,
    /// An echo request's identifier, where it quotes one.
    pub echo: Option<u16>,
}

/// What `frame` -- Ethernet header first -- is.
pub fn read(frame: &[u8]) -> Read {
    if frame.len() < ETH_HDR_LEN {
        return Read::NotIp;
    }
    let body = &frame[ETH_HDR_LEN..];
    match eth::ether_type(frame) {
        ETH_TYPE_IP => read_v4(body),
        ETH_TYPE_IPV6 => Read::V6 { neighbour: neighbour(body) },
        _ => Read::NotIp,
    }
}

/// A send of TCP over IPv4 a guest hands its NIC whole, to be cut into
/// frames or handed on whole (`hv::offload::check_tcp4` held it up): its
/// headers, as each frame cut from it has them -- but for its IP length,
/// which is the whole send's and no frame's, and is not looked at.
pub fn read_send(frame: &[u8]) -> Read {
    if frame.len() < ETH_HDR_LEN || eth::ether_type(frame) != ETH_TYPE_IP {
        return Read::Broken;
    }
    let p = &frame[ETH_HDR_LEN..];
    if p.len() < IP_HDR_LEN || ip::version(p) != 4 || ip::protocol(p) != IP_PROTO_TCP {
        return Read::Broken;
    }
    let ihl = ip::header_len(p);
    if ihl == 0 || p.len() < ihl + tcp::HDR_LEN {
        return Read::Broken;
    }
    if ip::is_fragment(p) {
        return Read::Fragment;
    }
    let l4 = &p[ihl..];
    Read::V4(V4 {
        src: ip::src(p), dst: ip::dst(p), proto: IP_PROTO_TCP, ports: Some((tcp::src_port(l4), tcp::dst_port(l4))),
        tcp_flags: tcp::flags(l4), icmp: None,
    })
}

/// Whether an IPv6 packet is neighbour discovery or a router's: an ICMPv6
/// message from 133 to 137, whole -- not in fragments, which RFC 6980 has
/// none of.
fn neighbour(pkt: &[u8]) -> bool {
    match ipv6::upper(pkt) {
        ipv6::Upper::Found { protocol: IP_PROTO_ICMPV6, at, fragment: false } => pkt.get(at).is_some_and(|&kind| {
            (icmpv6::ROUTER_SOLICIT..=icmpv6::REDIRECT).contains(&kind)
        }),
        _ => false,
    }
}

/// An IPv4 packet, the frame's body.
fn read_v4(p: &[u8]) -> Read {
    if p.len() < IP_HDR_LEN || ip::version(p) != 4 {
        return Read::Broken;
    }
    let ihl = ip::header_len(p);
    let total = usize::from(ip::total_len(p));
    if ihl == 0 || total < ihl || total > p.len() {
        return Read::Broken;
    }
    if ip::is_fragment(p) {
        return Read::Fragment;
    }
    let l4 = &p[ihl..total];
    let proto = ip::protocol(p);
    let mut v4 = V4 { src: ip::src(p), dst: ip::dst(p), proto, ports: None, tcp_flags: 0, icmp: None };
    match proto {
        IP_PROTO_TCP => {
            if l4.len() < tcp::HDR_LEN {
                return Read::Broken;
            }
            v4.ports = Some((tcp::src_port(l4), tcp::dst_port(l4)));
            v4.tcp_flags = tcp::flags(l4);
        }
        IP_PROTO_UDP => {
            if l4.len() < UDP_HDR_LEN {
                return Read::Broken;
            }
            v4.ports = Some((udp::src_port(l4), udp::dst_port(l4)));
        }
        IP_PROTO_ICMP => {
            if l4.len() < ICMP_HDR_LEN {
                return Read::Broken;
            }
            v4.icmp = Some(match icmp::kind(l4) {
                icmp::ECHO_REQUEST => Icmp::Echo { id: icmp::id(l4), request: true },
                icmp::ECHO_REPLY => Icmp::Echo { id: icmp::id(l4), request: false },
                icmp::DEST_UNREACH | icmp::TIME_EXCEEDED | PARAMETER_PROBLEM => {
                    quoted(&l4[ICMP_HDR_LEN..]).map_or(Icmp::Other, Icmp::Error)
                }
                _ => Icmp::Other,
            });
        }
        _ => {}
    }
    Read::V4(v4)
}

/// The packet an ICMP error quotes: its header, and the start of what it
/// carried -- the first piece of a packet, since a later one quotes no
/// transport header.
fn quoted(q: &[u8]) -> Option<Quoted> {
    if q.len() < IP_HDR_LEN || ip::version(q) != 4 {
        return None;
    }
    let ihl = ip::header_len(q);
    let first = netwire::be16(q, ip::FRAG_OFF) & ip::FRAG_OFFSET_MASK == 0;
    if ihl == 0 || q.len() < ihl + QUOTED_L4 || !first {
        return None;
    }
    let l4 = &q[ihl..];
    let proto = ip::protocol(q);
    let mut out = Quoted { src: ip::src(q), dst: ip::dst(q), proto, ports: None, echo: None };
    match proto {
        IP_PROTO_TCP | IP_PROTO_UDP => out.ports = Some((udp::src_port(l4), udp::dst_port(l4))),
        IP_PROTO_ICMP if icmp::kind(l4) == icmp::ECHO_REQUEST => out.echo = Some(icmp::id(l4)),
        _ => return None,
    }
    Some(out)
}
