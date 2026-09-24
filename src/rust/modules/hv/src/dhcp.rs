//! The guests' DHCP server: what a guest that asks for an address is told.
//! It is the address its port has -- the one `ip=` gives a guest whose
//! kernel takes that -- with nos (`hv0`) as its router and the DNS server
//! nos was given as its own, so a distribution that configures its network
//! by DHCP, as most do, needs nothing on its command line.
//!
//! The switch answers it, not a service on `hv0`: a DHCP message from a port
//! is answered into that port and goes nowhere else. Nothing is remembered,
//! because there is nothing to choose -- a port has one address, and a lease
//! on it is always granted again.

use netwire::{self as wire, eth, udp, Mac, ETH_TYPE_IP, MAC_BROADCAST};

const SERVER_PORT: u16 = 67;
const CLIENT_PORT: u16 = 68;

/* The fixed part of a message (RFC 2131, section 2), where its fields are. */
const OP: usize = 0;
const HTYPE: usize = 1;
const HLEN: usize = 2;
const XID: usize = 4;
const FLAGS: usize = 10;
const CIADDR: usize = 12;
const YIADDR: usize = 16;
const GIADDR: usize = 24;
const CHADDR: usize = 28;
const CHADDR_LEN: usize = 16;
const COOKIE: usize = 236;
const OPTIONS: usize = 240;

const MAGIC_COOKIE: u32 = 0x6382_5363;
const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;
const HTYPE_ETHERNET: u8 = 1;
const HLEN_ETHERNET: u8 = 6;
/// The client cannot take a unicast before it has its address: answer to
/// everyone.
const FLAG_BROADCAST: u16 = 0x8000;

const OPT_PAD: u8 = 0;
const OPT_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MESSAGE_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_END: u8 = 255;

const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;

/// A day, renewed at half of it.
const LEASE_SECS: u32 = 86_400;
/// The shortest message BOOTP relays and old clients take (RFC 1542):
/// shorter ones are padded out to it.
const MIN_MESSAGE: usize = 300;
/// The options a reply carries at most: type, server, lease, mask, router,
/// DNS server and the end.
const REPLY_OPTIONS: usize = 3 + 6 + 6 + 6 + 6 + 6 + 1;
/// A reply's message, and the whole frame.
const BODY_MAX: usize =
    if OPTIONS + REPLY_OPTIONS > MIN_MESSAGE { OPTIONS + REPLY_OPTIONS } else { MIN_MESSAGE };
pub const REPLY_MAX: usize = udp::PAYLOAD_AT + BODY_MAX;

/// What the server says of itself and of the network.
pub struct Network {
    pub server_ip: u32,
    pub server_mac: Mac,
    pub mask: u32,
    /// The DNS server to hand out; 0 for none.
    pub dns: u32,
}

/// Whether a frame is a DHCP client's message -- UDP from port 68 to 67 --
/// which is this server's to answer, and nobody else's to see.
pub fn is_request(frame: &[u8]) -> bool {
    eth::ether_type(frame) == ETH_TYPE_IP
        && udp::parse(frame).is_some_and(|d| d.dst_port == SERVER_PORT && d.src_port == CLIENT_PORT)
}

/// What a message's options say: its type, the address asked for, and the
/// server it was asked of.
struct Asked {
    kind: Option<u8>,
    requested: Option<u32>,
    server: Option<u32>,
}

fn options(opts: &[u8]) -> Asked {
    let mut asked = Asked { kind: None, requested: None, server: None };
    let mut at = 0;
    while at < opts.len() {
        let code = opts[at];
        if code == OPT_END {
            break;
        }
        if code == OPT_PAD {
            at += 1;
            continue;
        }
        let Some(&len) = opts.get(at + 1) else { break };
        let len = usize::from(len);
        let Some(data) = opts.get(at + 2..at + 2 + len) else { break };
        match code {
            OPT_MESSAGE_TYPE if len == 1 => asked.kind = Some(data[0]),
            OPT_REQUESTED_IP if len == 4 => asked.requested = Some(wire::be32(data, 0)),
            OPT_SERVER_ID if len == 4 => asked.server = Some(wire::be32(data, 0)),
            _ => {}
        }
        at += 2 + len;
    }
    asked
}

/// One option into `buf` at `at`: where the next goes.
fn put(buf: &mut [u8], at: usize, code: u8, data: &[u8]) -> usize {
    buf[at] = code;
    buf[at + 1] = data.len() as u8;
    buf[at + 2..at + 2 + data.len()].copy_from_slice(data);
    at + 2 + data.len()
}

/// The answer to a client's message from the guest whose address is
/// `yours`, into `reply`: the frame's length, or None for a message that
/// wants none -- a release, a decline, a request of another server, or
/// anything that is not a well-formed request.
pub fn answer(frame: &[u8], yours: u32, net: &Network, reply: &mut [u8; REPLY_MAX]) -> Option<usize> {
    let d = udp::parse(frame)?;
    let m = d.payload;
    if m.len() < OPTIONS || m[OP] != BOOTREQUEST || m[HTYPE] != HTYPE_ETHERNET
        || m[HLEN] != HLEN_ETHERNET || wire::be32(m, COOKIE) != MAGIC_COOKIE
    {
        return None;
    }
    let asked = options(&m[OPTIONS..]);
    if asked.server.is_some_and(|s| s != net.server_ip) {
        return None;
    }
    let ciaddr = wire::be32(m, CIADDR);
    let kind = match asked.kind? {
        DISCOVER => OFFER,
        /* The address it asks for -- or, renewing, the one it has -- is its
         * port's, or it is told no and starts again. */
        REQUEST => match asked.requested.unwrap_or(ciaddr) {
            asked if asked == yours => ACK,
            _ => NAK,
        },
        _ => return None,
    };

    reply.fill(0);
    let body = &mut reply[udp::PAYLOAD_AT..];
    body[OP] = BOOTREPLY;
    body[HTYPE] = HTYPE_ETHERNET;
    body[HLEN] = HLEN_ETHERNET;
    body[XID..XID + 4].copy_from_slice(&m[XID..XID + 4]);
    body[FLAGS..FLAGS + 2].copy_from_slice(&m[FLAGS..FLAGS + 2]);
    if kind == ACK {
        wire::set_be32(body, CIADDR, ciaddr);
    }
    if kind != NAK {
        wire::set_be32(body, YIADDR, yours);
    }
    body[GIADDR..GIADDR + 4].copy_from_slice(&m[GIADDR..GIADDR + 4]);
    body[CHADDR..CHADDR + CHADDR_LEN].copy_from_slice(&m[CHADDR..CHADDR + CHADDR_LEN]);
    wire::set_be32(body, COOKIE, MAGIC_COOKIE);

    let mut at = put(body, OPTIONS, OPT_MESSAGE_TYPE, &[kind]);
    at = put(body, at, OPT_SERVER_ID, &net.server_ip.to_be_bytes());
    if kind != NAK {
        at = put(body, at, OPT_LEASE_TIME, &LEASE_SECS.to_be_bytes());
        at = put(body, at, OPT_MASK, &net.mask.to_be_bytes());
        at = put(body, at, OPT_ROUTER, &net.server_ip.to_be_bytes());
        if net.dns != 0 {
            at = put(body, at, OPT_DNS, &net.dns.to_be_bytes());
        }
    }
    body[at] = OPT_END;
    let len = (at + 1).max(MIN_MESSAGE);

    /* Where to (RFC 2131, section 4.1): a no to everyone; an answer to a
     * client renewing to the address it has; to everyone when it said it
     * cannot take a unicast yet; else to the address it is given. */
    let flags = wire::be16(m, FLAGS);
    let (dst_ip, dst_mac) = if kind == NAK {
        (u32::MAX, MAC_BROADCAST)
    } else if ciaddr != 0 {
        (ciaddr, eth::src(frame))
    } else if flags & FLAG_BROADCAST != 0 {
        (u32::MAX, MAC_BROADCAST)
    } else {
        (yours, eth::src(frame))
    };
    let route = udp::Route {
        src_mac: net.server_mac, dst_mac, src_ip: net.server_ip, dst_ip,
        src_port: SERVER_PORT, dst_port: CLIENT_PORT, dont_fragment: false,
    };
    let frame_len = udp::write_frame(reply, &route, len)?;
    let datagram = &mut reply[udp::PAYLOAD_AT - netwire::UDP_HDR_LEN..frame_len];
    let sum = udp::checksum(net.server_ip, dst_ip, datagram);
    wire::set_be16(datagram, udp::CHECKSUM, sum);
    Some(frame_len)
}
