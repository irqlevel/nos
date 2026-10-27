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
pub const ETH_TYPE_IPV6: u16 = 0x86DD;

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

pub const IPV6_HDR_LEN: usize = 40;
pub const IP_PROTO_ICMPV6: u8 = 58;

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

/* ---- what an address may be ---- */

/// Whether a packet may go to `addr` at all (RFC 1122 3.2.1.3): not "this
/// network" (0/8), not a loopback address (127/8), which must never appear
/// outside a host, and not a reserved one (240/4) -- the limited broadcast
/// aside. A multicast group may be sent to.
#[inline]
pub fn deliverable(addr: u32) -> bool {
    let first = addr >> 24;
    first != 0 && first != 127 && (addr < 0xF000_0000 || addr == u32::MAX)
}

/// Whether a host may have `addr` on a subnet of `mask` (RFC 1122 3.2.1.3):
/// not "this network", not loopback, not a multicast or reserved address,
/// and not the subnet's broadcast when it has one -- a subnet of one or two
/// addresses does not. A mask of 0 is a subnet not known.
#[inline]
pub fn host_address(addr: u32, mask: u32) -> bool {
    let first = addr >> 24;
    first != 0 && first != 127 && addr < 0xE000_0000 && !(mask.count_zeros() >= 2 && addr | mask == u32::MAX)
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

    /// The checksum a datagram carries, over the pseudo-header and the
    /// datagram: what a sender that fills it in puts there. A sum that comes
    /// to 0 goes as 0xFFFF, the same number in one's complement, because 0
    /// in the field says there is no checksum at all (RFC 768).
    #[inline]
    pub fn checksum(src_ip: u32, dst_ip: u32, datagram: &[u8]) -> u16 {
        match super::transport_checksum(IP_PROTO_UDP, src_ip, dst_ip, datagram) {
            0 => 0xFFFF,
            sum => sum,
        }
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
    pub const TIME_EXCEEDED: u8 = 11;

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
    /// Window scale (RFC 7323 2.2): the shift its sender applies to every
    /// window it advertises after the SYN that carries it.
    pub const OPT_WS: u8 = 3;
    pub const OPT_WS_LEN: u8 = 3;
    /// The largest shift there is: a window of a gigabyte. A bigger one is
    /// taken as this (RFC 7323 2.3).
    pub const MAX_WSCALE: u8 = 14;

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

    /// What option `kind` says, when the segment carries one `len` bytes
    /// long: the bytes after its kind and length. The walk stops at the end
    /// of the list, and at an option whose length runs past the header; one
    /// of the kind but another length is passed over.
    fn option(seg: &[u8], kind: u8, len: u8) -> Option<&[u8]> {
        let hdr_len = header_len(seg);
        if hdr_len <= HDR_LEN || hdr_len > seg.len() {
            return None;
        }

        let options = &seg[HDR_LEN..hdr_len];
        let mut at = 0;
        while at < options.len() {
            let found = options[at];
            if found == OPT_END {
                break;
            }
            if found == OPT_NOP {
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
            if found == kind && opt_len == len as usize {
                return Some(&options[at + 2..at + opt_len]);
            }
            at += opt_len;
        }
        None
    }

    /// The peer's maximum segment size from a SYN's options, capped at ours.
    /// `default` when it named none.
    pub fn parse_mss(seg: &[u8], ours: u16, default: u16) -> u16 {
        match option(seg, OPT_MSS, OPT_MSS_LEN) {
            Some(value) => {
                let mss = be16(value, 0);
                if mss == 0 { default } else { mss.min(ours) }
            }
            None => default,
        }
    }

    /// The window scale a SYN asks for (RFC 7323 2.3): the shift its sender
    /// will apply to the windows it advertises after it, at most
    /// `MAX_WSCALE`. None when it asks for none -- and then neither end
    /// scales.
    pub fn parse_wscale(seg: &[u8]) -> Option<u8> {
        option(seg, OPT_WS, OPT_WS_LEN).map(|value| value[0].min(MAX_WSCALE))
    }

    /// The checksum a TCP segment carries: over the pseudo-header of
    /// addresses, protocol and length, then the segment itself. Answers 0
    /// over a segment whose own checksum is right.
    #[inline]
    pub fn checksum(src_ip: u32, dst_ip: u32, segment: &[u8]) -> u16 {
        super::transport_checksum(IP_PROTO_TCP, src_ip, dst_ip, segment)
    }
}

/* ---- IPv6 ---- */

/// IPv6's header, and the chain of extension headers after it (RFC 8200).
/// Nothing of nos's speaks it; the guests' switch reads it to tell whose a
/// packet is, and answers one kind of it (`icmpv6`).
pub mod ipv6 {
    use super::{be16, IPV6_HDR_LEN};

    /// An address, as the wire has it.
    pub type Addr = [u8; 16];

    pub const VERSION: u8 = 6;
    pub const PAYLOAD_LEN: usize = 4;
    pub const NEXT_HEADER: usize = 6;
    pub const HOP_LIMIT: usize = 7;
    pub const SRC: usize = 8;
    pub const DST: usize = 24;

    pub const UNSPECIFIED: Addr = [0; 16];

    /* The extension headers a chain is walked through */
    const HOP_BY_HOP: u8 = 0;
    const ROUTING: u8 = 43;
    const FRAGMENT: u8 = 44;
    const AUTHENTICATION: u8 = 51;
    const DESTINATION: u8 = 60;
    /// Extension headers walked at most: past them, what the packet carries
    /// is taken for hidden.
    const CHAIN_MAX: usize = 8;

    /// The version, from a packet at least `IPV6_HDR_LEN` long.
    pub fn version(pkt: &[u8]) -> u8 {
        pkt[0] >> 4
    }

    pub fn src(pkt: &[u8]) -> Addr {
        addr(pkt, SRC)
    }

    pub fn dst(pkt: &[u8]) -> Addr {
        addr(pkt, DST)
    }

    fn addr(pkt: &[u8], at: usize) -> Addr {
        let mut a = [0u8; 16];
        a.copy_from_slice(&pkt[at..at + 16]);
        a
    }

    pub fn is_multicast(a: &Addr) -> bool {
        a[0] == 0xFF
    }

    pub fn is_link_local(a: &Addr) -> bool {
        a[0] == 0xFE && a[1] & 0xC0 == 0x80
    }

    /// The modified EUI-64 interface identifier an Ethernet address makes
    /// (RFC 4291, appendix A): FF:FE in its middle, its universal bit
    /// flipped.
    pub fn eui64(mac: &super::Mac) -> [u8; 8] {
        [mac[0] ^ 0x02, mac[1], mac[2], 0xFF, 0xFE, mac[3], mac[4], mac[5]]
    }

    /// The address `prefix` -- its first eight bytes -- and `mac`'s EUI-64
    /// make.
    pub fn with_eui64(prefix: &[u8; 8], mac: &super::Mac) -> Addr {
        let mut a = [0u8; 16];
        a[..8].copy_from_slice(prefix);
        a[8..].copy_from_slice(&eui64(mac));
        a
    }

    /// What a packet carries, past its extension headers.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Upper {
        /// This protocol's header, at this offset in the packet -- in its
        /// first fragment, if a fragment header came before it.
        Found { protocol: u8, at: usize, fragment: bool },
        /// A fragment past the first: what it carries is the first's to say.
        LaterFragment,
        /// The chain runs past the packet, or past `CHAIN_MAX` headers, and
        /// so hides what it carries -- which a packet that passes a check of
        /// what it carries must not do (RFC 7112): one that does is dropped.
        Hidden,
    }

    /// What `pkt` -- a packet from its IPv6 header on -- carries: the chain
    /// of hop-by-hop, routing, destination options, fragment and
    /// authentication headers walked to the first header that is none of
    /// them, every one of them inside the packet.
    pub fn upper(pkt: &[u8]) -> Upper {
        if pkt.len() < IPV6_HDR_LEN {
            return Upper::Hidden;
        }
        let mut next = pkt[NEXT_HEADER];
        let mut at = IPV6_HDR_LEN;
        let mut fragment = false;
        for _ in 0..CHAIN_MAX {
            let len = match next {
                HOP_BY_HOP | ROUTING | DESTINATION => match pkt.get(at + 1) {
                    Some(&units) => (usize::from(units) + 1) * 8,
                    None => return Upper::Hidden,
                },
                AUTHENTICATION => match pkt.get(at + 1) {
                    Some(&units) => (usize::from(units) + 2) * 4,
                    None => return Upper::Hidden,
                },
                FRAGMENT => {
                    if pkt.len() < at + 8 {
                        return Upper::Hidden;
                    }
                    if be16(pkt, at + 2) & 0xFFF8 != 0 {
                        return Upper::LaterFragment;
                    }
                    fragment = true;
                    8
                }
                protocol => return Upper::Found { protocol, at, fragment },
            };
            if pkt.len() < at + len {
                return Upper::Hidden;
            }
            next = pkt[at];
            at += len;
        }
        Upper::Hidden
    }

    /// The checksum over IPv6's pseudo-header -- the addresses, the length
    /// and the protocol -- and an upper-layer message (RFC 8200, 8.1):
    /// answers 0 over a message whose own checksum is right.
    pub fn checksum(protocol: u8, src: &Addr, dst: &Addr, msg: &[u8]) -> u16 {
        let mut sum: u64 = 0;
        for a in [src, dst] {
            for pair in a.chunks_exact(2) {
                sum += u64::from(u16::from_be_bytes([pair[0], pair[1]]));
            }
        }
        sum += msg.len() as u64;
        sum += u64::from(protocol);
        let mut at = 0;
        while at + 1 < msg.len() {
            sum += u64::from(u16::from_be_bytes([msg[at], msg[at + 1]]));
            at += 2;
        }
        if at < msg.len() {
            sum += u64::from(msg[at]) << 8;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !(sum as u16)
    }

    /// The 40-byte header, for a payload of `len` bytes.
    pub fn write(pkt: &mut [u8], len: u16, next: u8, hop_limit: u8, src: &Addr, dst: &Addr) {
        pkt[..4].copy_from_slice(&[VERSION << 4, 0, 0, 0]);
        pkt[PAYLOAD_LEN..PAYLOAD_LEN + 2].copy_from_slice(&len.to_be_bytes());
        pkt[NEXT_HEADER] = next;
        pkt[HOP_LIMIT] = hop_limit;
        pkt[SRC..SRC + 16].copy_from_slice(src);
        pkt[DST..DST + 16].copy_from_slice(dst);
    }
}

/// ICMPv6's messages of neighbour discovery and multicast listening (RFC
/// 4861, RFC 3810), and the options neighbour discovery carries.
pub mod icmpv6 {
    pub const MLD_QUERY: u8 = 130;
    pub const MLD_REPORT: u8 = 131;
    pub const MLD_DONE: u8 = 132;
    pub const MLD2_REPORT: u8 = 143;
    pub const ROUTER_SOLICIT: u8 = 133;
    pub const ROUTER_ADVERT: u8 = 134;
    pub const NEIGHBOR_SOLICIT: u8 = 135;
    pub const NEIGHBOR_ADVERT: u8 = 136;
    pub const REDIRECT: u8 = 137;

    /// Where each message's options begin: past its fixed part.
    pub const RS_OPTIONS: usize = 8;
    pub const RA_OPTIONS: usize = 16;
    pub const NS_OPTIONS: usize = 24;
    pub const NA_OPTIONS: usize = 24;
    /// Where a solicitation's or an advertisement's target is.
    pub const TARGET: usize = 8;

    pub const OPT_SOURCE_LLA: u8 = 1;
    pub const OPT_TARGET_LLA: u8 = 2;
    pub const OPT_PREFIX: u8 = 3;
    pub const OPT_MTU: u8 = 5;
    /// An option's length is in units of this many bytes.
    pub const OPT_UNIT: usize = 8;

    /// Whether every link-layer address option of a message from `at` on --
    /// a source's or a target's -- is `mac`, and the options are well
    /// formed: none of length 0 (RFC 4861, 4.6), none past the message.
    pub fn link_addresses_are(msg: &[u8], at: usize, mac: &super::Mac) -> bool {
        let mut at = at;
        while at < msg.len() {
            let (Some(&kind), Some(&units)) = (msg.get(at), msg.get(at + 1)) else { return false };
            let len = usize::from(units) * OPT_UNIT;
            if len == 0 || msg.len() < at + len {
                return false;
            }
            if (kind == OPT_SOURCE_LLA || kind == OPT_TARGET_LLA) && (len != OPT_UNIT || msg[at + 2..at + 8] != mac[..]) {
                return false;
            }
            at += len;
        }
        true
    }
}

/// The checksum over a transport's pseudo-header -- the addresses, the
/// protocol and the length -- and then its segment: TCP's, and UDP's but for
/// the one number UDP sends differently (`udp::checksum`). Answers 0 over a
/// segment whose own checksum is right.
#[inline]
pub fn transport_checksum(protocol: u8, src_ip: u32, dst_ip: u32, segment: &[u8]) -> u16 {
    let mut sum: u32 = 0;

    /* The pseudo-header, as 16-bit words */
    sum += (src_ip >> 16) & 0xFFFF;
    sum += src_ip & 0xFFFF;
    sum += (dst_ip >> 16) & 0xFFFF;
    sum += dst_ip & 0xFFFF;
    sum += protocol as u32;
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
