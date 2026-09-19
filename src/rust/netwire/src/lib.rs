//! What is actually on the wire: the headers a frame is made of, read and
//! written through a byte slice rather than a struct cast.
//!
//! Every accessor takes the offset of its header within the frame and the
//! slice the frame is in, so a header that does not fit is a `None` and not
//! a read past the end. The kernel is little-endian on both architectures
//! and the wire is big-endian, so every multi-byte field goes through
//! `from_be_bytes`/`to_be_bytes` -- there is no host-order path to forget.
//!
//! A crate of its own, with no kernel in it, because two things that cannot
//! share a crate with a kernel in it both need it: the network layer
//! (`net`, which re-exports it as `net::wire`), and a loadable module that
//! answers from the receive path or builds its own frames -- linked on its
//! own, so the layer's statics are not something it can link against.

#![no_std]

/// An Ethernet address.
pub type Mac = [u8; 6];

pub const MAC_BROADCAST: Mac = [0xFF; 6];

pub const ETH_HDR_LEN: usize = 14;
pub const ETH_TYPE_IP: u16 = 0x0800;
pub const ETH_TYPE_ARP: u16 = 0x0806;

pub const ARP_LEN: usize = 28;
pub const ARP_OP_REQUEST: u16 = 1;
pub const ARP_OP_REPLY: u16 = 2;
pub const ARP_HW_ETHERNET: u16 = 1;

pub const IP_HDR_LEN: usize = 20;
pub const IP_PROTO_ICMP: u8 = 1;
pub const IP_PROTO_TCP: u8 = 6;
pub const IP_PROTO_UDP: u8 = 17;

pub const UDP_HDR_LEN: usize = 8;
pub const ICMP_HDR_LEN: usize = 8;

/// An Ethernet frame, headers included, as this stack sends them: no jumbo
/// frames and no VLAN tag.
pub const MAX_FRAME: usize = 1514;

/* ---- addresses, as people write them ---- */

/// An IPv4 address in host byte order, which is how the kernel keeps one,
/// printed as a dotted quad.
pub struct Ipv4(pub u32);

impl core::fmt::Display for Ipv4 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = self.0.to_be_bytes();
        write!(f, "{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    }
}

/// An Ethernet address printed as six hex pairs, lower case.
pub struct MacHex(pub Mac);

impl core::fmt::Display for MacHex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5])
    }
}

/// A dotted quad into host byte order, or None: four decimal numbers of at
/// most 255, three dots, and nothing else -- no sign, no space, no empty
/// part. Strict, because it is also what tells an address from a host name.
pub fn parse_ipv4(text: &[u8]) -> Option<u32> {
    let mut parts = [0u32; 4];
    let mut part = 0;
    let mut digits = 0;

    for &b in text {
        if b == b'.' {
            if digits == 0 || part == 3 {
                return None;
            }
            part += 1;
            digits = 0;
            continue;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        parts[part] = parts[part] * 10 + (b - b'0') as u32;
        if parts[part] > 255 {
            return None;
        }
        digits += 1;
    }

    if part != 3 || digits == 0 {
        return None;
    }
    Some((parts[0] << 24) | (parts[1] << 16) | (parts[2] << 8) | parts[3])
}

/* ---- scalars ---- */

#[inline]
pub fn be16(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([buf[off], buf[off + 1]])
}

#[inline]
pub fn be32(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
pub fn set_be16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_be_bytes());
}

#[inline]
pub fn set_be32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

#[inline]
pub fn be64(buf: &[u8], off: usize) -> u64 {
    ((be32(buf, off) as u64) << 32) | be32(buf, off + 4) as u64
}

#[inline]
pub fn set_be64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_be_bytes());
}

/* ---- Ethernet ---- */

pub mod eth {
    use super::*;

    pub const DST: usize = 0;
    pub const SRC: usize = 6;
    pub const TYPE: usize = 12;

    #[inline]
    pub fn dst(frame: &[u8]) -> Mac {
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&frame[DST..DST + 6]);
        mac
    }

    #[inline]
    pub fn src(frame: &[u8]) -> Mac {
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&frame[SRC..SRC + 6]);
        mac
    }

    #[inline]
    pub fn ether_type(frame: &[u8]) -> u16 {
        be16(frame, TYPE)
    }

    /// Fill the header at the start of `frame`.
    #[inline]
    pub fn write(frame: &mut [u8], dst: &Mac, src: &Mac, ether_type: u16) {
        frame[DST..DST + 6].copy_from_slice(dst);
        frame[SRC..SRC + 6].copy_from_slice(src);
        set_be16(frame, TYPE, ether_type);
    }
}

/* ---- ARP ---- */

pub mod arp {
    use super::*;

    /* Offsets within the ARP packet, which starts after the Ethernet header */
    pub const HW_TYPE: usize = 0;
    pub const PROTO_TYPE: usize = 2;
    pub const HW_SIZE: usize = 4;
    pub const PROTO_SIZE: usize = 5;
    pub const OPCODE: usize = 6;
    pub const SENDER_MAC: usize = 8;
    pub const SENDER_IP: usize = 14;
    pub const TARGET_MAC: usize = 18;
    pub const TARGET_IP: usize = 24;

    #[inline]
    pub fn opcode(arp: &[u8]) -> u16 {
        be16(arp, OPCODE)
    }

    #[inline]
    pub fn sender_mac(arp: &[u8]) -> Mac {
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&arp[SENDER_MAC..SENDER_MAC + 6]);
        mac
    }

    /// Host byte order, as the rest of the kernel keeps an address.
    #[inline]
    pub fn sender_ip(arp: &[u8]) -> u32 {
        be32(arp, SENDER_IP)
    }

    #[inline]
    pub fn target_ip(arp: &[u8]) -> u32 {
        be32(arp, TARGET_IP)
    }

    /// A request or a reply, filled into `arp`.
    #[inline]
    pub fn write(
        arp: &mut [u8], opcode: u16, sender_mac: &Mac, sender_ip: u32,
        target_mac: &Mac, target_ip: u32,
    ) {
        set_be16(arp, HW_TYPE, ARP_HW_ETHERNET);
        set_be16(arp, PROTO_TYPE, ETH_TYPE_IP);
        arp[HW_SIZE] = 6;
        arp[PROTO_SIZE] = 4;
        set_be16(arp, OPCODE, opcode);
        arp[SENDER_MAC..SENDER_MAC + 6].copy_from_slice(sender_mac);
        set_be32(arp, SENDER_IP, sender_ip);
        arp[TARGET_MAC..TARGET_MAC + 6].copy_from_slice(target_mac);
        set_be32(arp, TARGET_IP, target_ip);
    }
}

/* ---- IP ---- */

pub mod ip {
    use super::*;

    pub const VERSION_IHL: usize = 0;
    pub const TOS: usize = 1;
    pub const TOTAL_LEN: usize = 2;
    pub const ID: usize = 4;
    pub const FRAG_OFF: usize = 6;
    pub const TTL: usize = 8;
    pub const PROTOCOL: usize = 9;
    pub const CHECKSUM: usize = 10;
    pub const SRC: usize = 12;
    pub const DST: usize = 16;

    /* The flags and the fragment offset share the word at FRAG_OFF */
    /// "Do not fragment": a router with too small a link drops the packet
    /// and says so, rather than cutting it up.
    pub const DONT_FRAGMENT: u16 = 0x4000;
    pub const MORE_FRAGMENTS: u16 = 0x2000;
    pub const FRAG_OFFSET_MASK: u16 = 0x1FFF;

    /// The header's own length in bytes, which a packet with options makes
    /// longer than 20. 0 when the field says something impossible.
    #[inline]
    pub fn header_len(ip: &[u8]) -> usize {
        let words = (ip[VERSION_IHL] & 0x0F) as usize;
        if words < 5 { 0 } else { words * 4 }
    }

    #[inline]
    pub fn version(ip: &[u8]) -> u8 {
        ip[VERSION_IHL] >> 4
    }

    #[inline]
    pub fn total_len(ip: &[u8]) -> u16 {
        be16(ip, TOTAL_LEN)
    }

    #[inline]
    pub fn protocol(ip: &[u8]) -> u8 {
        ip[PROTOCOL]
    }

    #[inline]
    pub fn src(ip: &[u8]) -> u32 {
        be32(ip, SRC)
    }

    #[inline]
    pub fn dst(ip: &[u8]) -> u32 {
        be32(ip, DST)
    }

    /// A piece of a packet rather than a whole one: more pieces follow it,
    /// or it is not the first. There is no reassembly in this kernel, so a
    /// fragment is never anybody's -- and what follows the IP header of one
    /// that is not the first is not a transport header at all.
    #[inline]
    pub fn is_fragment(ip: &[u8]) -> bool {
        be16(ip, FRAG_OFF) & (MORE_FRAGMENTS | FRAG_OFFSET_MASK) != 0
    }

    /// A 20-byte header with no options, checksum included.
    #[inline]
    pub fn write(ip: &mut [u8], protocol: u8, src: u32, dst: u32, payload_len: usize, id: u16) {
        write_flags(ip, protocol, src, dst, payload_len, id, 0)
    }

    /// As `write`, with the flags word given: `DONT_FRAGMENT`, or 0.
    #[inline]
    pub fn write_flags(
        ip: &mut [u8], protocol: u8, src: u32, dst: u32, payload_len: usize, id: u16, flags: u16,
    ) {
        ip[VERSION_IHL] = 0x45;
        ip[TOS] = 0;
        set_be16(ip, TOTAL_LEN, (IP_HDR_LEN + payload_len) as u16);
        set_be16(ip, ID, id);
        set_be16(ip, FRAG_OFF, flags);
        ip[TTL] = 64;
        ip[PROTOCOL] = protocol;
        set_be16(ip, CHECKSUM, 0);
        set_be32(ip, SRC, src);
        set_be32(ip, DST, dst);

        let sum = super::checksum(&ip[..IP_HDR_LEN]);
        set_be16(ip, CHECKSUM, sum);
    }
}

/* ---- UDP ---- */

pub mod udp {
    use super::*;

    pub const SRC_PORT: usize = 0;
    pub const DST_PORT: usize = 2;
    pub const LENGTH: usize = 4;
    pub const CHECKSUM: usize = 6;

    #[inline]
    pub fn src_port(udp: &[u8]) -> u16 {
        be16(udp, SRC_PORT)
    }

    #[inline]
    pub fn dst_port(udp: &[u8]) -> u16 {
        be16(udp, DST_PORT)
    }

    #[inline]
    pub fn length(udp: &[u8]) -> u16 {
        be16(udp, LENGTH)
    }

    /// The header only; the payload is the caller's to put after it. The
    /// checksum is left at 0, which IPv4 allows and this stack has always
    /// sent.
    #[inline]
    pub fn write(udp: &mut [u8], src_port: u16, dst_port: u16, payload_len: usize) {
        set_be16(udp, SRC_PORT, src_port);
        set_be16(udp, DST_PORT, dst_port);
        set_be16(udp, LENGTH, (UDP_HDR_LEN + payload_len) as u16);
        set_be16(udp, CHECKSUM, 0);
    }

    /// What fits in one datagram out of this stack.
    pub const MAX_PAYLOAD: usize = MAX_FRAME - ETH_HDR_LEN - IP_HDR_LEN - UDP_HDR_LEN;

    /// Where a datagram's payload starts in a frame whose IP header carries
    /// no options -- which is every frame [`write_frame`] makes.
    pub const PAYLOAD_AT: usize = ETH_HDR_LEN + IP_HDR_LEN + UDP_HDR_LEN;

    /// A received datagram: who it is from, and what it carries.
    pub struct Datagram<'a> {
        pub src_ip: u32,
        pub dst_ip: u32,
        pub src_port: u16,
        pub dst_port: u16,
        pub payload: &'a [u8],
        /// Where `payload` starts in the frame: for whoever hands the frame's
        /// own bytes on rather than a copy of them -- a disk's DMA, say.
        pub payload_at: usize,
    }

    /// What a frame off the wire holds, if it holds a whole UDP datagram at
    /// all.
    ///
    /// The IP header's own length is honoured, so a packet carrying options
    /// puts its UDP header where this looks for it; a length that disagrees
    /// with the frame is a None rather than a read past the end.
    #[inline]
    pub fn parse(frame: &[u8]) -> Option<Datagram<'_>> {
        if frame.len() < ETH_HDR_LEN + IP_HDR_LEN + UDP_HDR_LEN {
            return None;
        }

        let packet = &frame[ETH_HDR_LEN..];
        let ip_len = ip::header_len(packet);
        if ip_len == 0 || frame.len() < ETH_HDR_LEN + ip_len + UDP_HDR_LEN {
            return None;
        }
        if ip::protocol(packet) != IP_PROTO_UDP {
            return None;
        }

        let datagram = &frame[ETH_HDR_LEN + ip_len..];
        let length = self::length(datagram) as usize;
        if length < UDP_HDR_LEN || length > datagram.len() {
            return None;
        }

        Some(Datagram {
            src_ip: ip::src(packet),
            dst_ip: ip::dst(packet),
            src_port: src_port(datagram),
            dst_port: dst_port(datagram),
            payload: &datagram[UDP_HDR_LEN..length],
            payload_at: ETH_HDR_LEN + ip_len + UDP_HDR_LEN,
        })
    }

    /// Who a datagram is from and who it is for, Ethernet addresses included:
    /// everything [`write_frame`] puts in front of a payload.
    pub struct Route {
        pub src_mac: Mac,
        pub dst_mac: Mac,
        /// Host byte order, as the rest of the kernel keeps an address.
        pub src_ip: u32,
        pub dst_ip: u32,
        pub src_port: u16,
        pub dst_port: u16,
        /// Whether a router may cut the datagram up to fit a smaller link.
        /// One sized to the path on purpose, and answered whole or not at
        /// all, says it may not.
        pub dont_fragment: bool,
    }

    /// The Ethernet, IP and UDP headers of a datagram of `payload_len` bytes,
    /// at the start of `frame`; the payload goes at [`PAYLOAD_AT`]. The
    /// frame's whole length, or None when `frame` has no room for it or the
    /// payload is more than one datagram out of this stack holds.
    #[inline]
    pub fn write_frame(frame: &mut [u8], route: &Route, payload_len: usize) -> Option<usize> {
        let frame_len = PAYLOAD_AT + payload_len;
        if payload_len > MAX_PAYLOAD || frame.len() < frame_len {
            return None;
        }
        write_headers(frame, route, payload_len)?;
        Some(frame_len)
    }

    /// Only the headers, into `headers` -- which need hold no more than
    /// them, [`PAYLOAD_AT`] bytes: the payload may be put behind them by
    /// something that is not the CPU, a disk's DMA straight into the frame.
    /// No limit on the payload but the one the length fields have: a sender
    /// that knows its path takes a larger frame than this stack's own sends.
    /// None when `headers` is short or the lengths would not fit their
    /// fields.
    #[inline]
    pub fn write_headers(headers: &mut [u8], route: &Route, payload_len: usize) -> Option<()> {
        if headers.len() < PAYLOAD_AT
            || payload_len > u16::MAX as usize - IP_HDR_LEN - UDP_HDR_LEN
        {
            return None;
        }

        let flags = if route.dont_fragment { ip::DONT_FRAGMENT } else { 0 };
        eth::write(headers, &route.dst_mac, &route.src_mac, ETH_TYPE_IP);
        ip::write_flags(&mut headers[ETH_HDR_LEN..], IP_PROTO_UDP, route.src_ip, route.dst_ip,
                        UDP_HDR_LEN + payload_len, 0, flags);
        write(&mut headers[ETH_HDR_LEN + IP_HDR_LEN..], route.src_port, route.dst_port, payload_len);
        Some(())
    }
}

/* ---- ICMP ---- */

pub mod icmp {
    use super::*;

    pub const TYPE: usize = 0;
    pub const CODE: usize = 1;
    pub const CHECKSUM: usize = 2;
    pub const ID: usize = 4;
    pub const SEQ: usize = 6;

    pub const ECHO_REPLY: u8 = 0;
    pub const DEST_UNREACH: u8 = 3;
    pub const ECHO_REQUEST: u8 = 8;

    /* Destination Unreachable codes that are hard errors for TCP
     * (RFC 1122 4.2.3.9) */
    pub const PROTO_UNREACH: u8 = 2;
    pub const PORT_UNREACH: u8 = 3;

    #[inline]
    pub fn kind(icmp: &[u8]) -> u8 {
        icmp[TYPE]
    }

    #[inline]
    pub fn code(icmp: &[u8]) -> u8 {
        icmp[CODE]
    }

    #[inline]
    pub fn id(icmp: &[u8]) -> u16 {
        be16(icmp, ID)
    }

    #[inline]
    pub fn seq(icmp: &[u8]) -> u16 {
        be16(icmp, SEQ)
    }

    /// The header, with the checksum taken over it and everything after it
    /// within `len` -- which is what an ICMP checksum covers.
    #[inline]
    pub fn write(icmp: &mut [u8], kind: u8, code: u8, id: u16, seq: u16, len: usize) {
        icmp[TYPE] = kind;
        icmp[CODE] = code;
        set_be16(icmp, CHECKSUM, 0);
        set_be16(icmp, ID, id);
        set_be16(icmp, SEQ, seq);

        let sum = super::checksum(&icmp[..len]);
        set_be16(icmp, CHECKSUM, sum);
    }
}

/* ---- TCP ---- */

pub mod tcp {
    use super::*;

    pub const HDR_LEN: usize = 20;

    pub const SRC_PORT: usize = 0;
    pub const DST_PORT: usize = 2;
    pub const SEQ: usize = 4;
    pub const ACK: usize = 8;
    pub const DATA_OFF: usize = 12;
    pub const FLAGS: usize = 13;
    pub const WINDOW: usize = 14;
    pub const CHECKSUM: usize = 16;
    pub const URGENT: usize = 18;

    pub const FIN: u8 = 0x01;
    pub const SYN: u8 = 0x02;
    pub const RST: u8 = 0x04;
    pub const PSH: u8 = 0x08;
    pub const ACK_FLAG: u8 = 0x10;

    pub const OPT_END: u8 = 0;
    pub const OPT_NOP: u8 = 1;
    pub const OPT_MSS: u8 = 2;
    pub const OPT_MSS_LEN: u8 = 4;

    #[inline]
    pub fn src_port(seg: &[u8]) -> u16 {
        be16(seg, SRC_PORT)
    }

    #[inline]
    pub fn dst_port(seg: &[u8]) -> u16 {
        be16(seg, DST_PORT)
    }

    #[inline]
    pub fn seq(seg: &[u8]) -> u32 {
        be32(seg, SEQ)
    }

    #[inline]
    pub fn ack(seg: &[u8]) -> u32 {
        be32(seg, ACK)
    }

    #[inline]
    pub fn flags(seg: &[u8]) -> u8 {
        seg[FLAGS]
    }

    #[inline]
    pub fn window(seg: &[u8]) -> u16 {
        be16(seg, WINDOW)
    }

    /// The header's own length in bytes, options included. 0 when the field
    /// says something shorter than a header.
    #[inline]
    pub fn header_len(seg: &[u8]) -> usize {
        let words = (seg[DATA_OFF] >> 4) as usize;
        if words < 5 { 0 } else { words * 4 }
    }

    /// The header, with no checksum yet: the caller takes it once the payload
    /// is in place, over the pseudo-header too.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn write(
        seg: &mut [u8], src_port: u16, dst_port: u16, seq: u32, ack: u32,
        header_len: usize, flags: u8, window: u16,
    ) {
        set_be16(seg, SRC_PORT, src_port);
        set_be16(seg, DST_PORT, dst_port);
        set_be32(seg, SEQ, seq);
        set_be32(seg, ACK, ack);
        seg[DATA_OFF] = ((header_len / 4) as u8) << 4;
        seg[FLAGS] = flags;
        set_be16(seg, WINDOW, window);
        set_be16(seg, CHECKSUM, 0);
        set_be16(seg, URGENT, 0);
    }

    /// The peer's maximum segment size from a SYN's options, capped at ours.
    /// `default` when it named none.
    pub fn parse_mss(seg: &[u8], ours: u16, default: u16) -> u16 {
        let len = header_len(seg);
        if len <= HDR_LEN || len > seg.len() {
            return default;
        }

        let options = &seg[HDR_LEN..len];
        let mut at = 0;
        while at < options.len() {
            let kind = options[at];
            if kind == OPT_END {
                break;
            }
            if kind == OPT_NOP {
                at += 1;
                continue;
            }
            if at + 1 >= options.len() {
                break;
            }
            let opt_len = options[at + 1] as usize;
            if opt_len < 2 || at + opt_len > options.len() {
                break;
            }
            if kind == OPT_MSS && opt_len == OPT_MSS_LEN as usize {
                let mss = be16(options, at + 2);
                return if mss == 0 { default } else { mss.min(ours) };
            }
            at += opt_len;
        }
        default
    }

    /// The checksum a TCP segment carries: over the pseudo-header of
    /// addresses, protocol and length, then the segment itself. Answers 0
    /// over a segment whose own checksum is right.
    #[inline]
    pub fn checksum(src_ip: u32, dst_ip: u32, segment: &[u8]) -> u16 {
        let mut sum: u32 = 0;

        /* The pseudo-header, as 16-bit words */
        sum += (src_ip >> 16) & 0xFFFF;
        sum += src_ip & 0xFFFF;
        sum += (dst_ip >> 16) & 0xFFFF;
        sum += dst_ip & 0xFFFF;
        sum += IP_PROTO_TCP as u32;
        sum += segment.len() as u32;

        let mut at = 0;
        while at + 1 < segment.len() {
            sum += ((segment[at] as u32) << 8) | segment[at + 1] as u32;
            at += 2;
        }
        if at < segment.len() {
            sum += (segment[at] as u32) << 8;
        }

        while sum >> 16 != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }

        !(sum as u16)
    }
}

/// The internet checksum (RFC 1071): the one's complement of the one's
/// complement sum of the 16-bit words, with an odd last byte padded.
#[inline]
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;

    let mut at = 0;
    while at + 1 < data.len() {
        sum += ((data[at] as u32) << 8) | data[at + 1] as u32;
        at += 2;
    }
    if at < data.len() {
        sum += (data[at] as u32) << 8;
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    !(sum as u16)
}
