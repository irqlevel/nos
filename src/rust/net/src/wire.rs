//! What is actually on the wire: the headers a frame is made of, read and
//! written through a byte slice rather than a struct cast.
//!
//! Every accessor takes the offset of its header within the frame and the
//! slice the frame is in, so a header that does not fit is a `None` and not
//! a read past the end. The kernel is little-endian on both architectures
//! and the wire is big-endian, so every multi-byte field goes through
//! `from_be_bytes`/`to_be_bytes` -- there is no host-order path to forget.

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

/* ---- scalars ---- */

pub fn be16(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([buf[off], buf[off + 1]])
}

pub fn be32(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

pub fn set_be16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_be_bytes());
}

pub fn set_be32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

/* ---- Ethernet ---- */

pub mod eth {
    use super::*;

    pub const DST: usize = 0;
    pub const SRC: usize = 6;
    pub const TYPE: usize = 12;

    pub fn dst(frame: &[u8]) -> Mac {
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&frame[DST..DST + 6]);
        mac
    }

    pub fn src(frame: &[u8]) -> Mac {
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&frame[SRC..SRC + 6]);
        mac
    }

    pub fn ether_type(frame: &[u8]) -> u16 {
        be16(frame, TYPE)
    }

    /// Fill the header at the start of `frame`.
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

    pub fn opcode(arp: &[u8]) -> u16 {
        be16(arp, OPCODE)
    }

    pub fn sender_mac(arp: &[u8]) -> Mac {
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&arp[SENDER_MAC..SENDER_MAC + 6]);
        mac
    }

    /// Host byte order, as the rest of the kernel keeps an address.
    pub fn sender_ip(arp: &[u8]) -> u32 {
        be32(arp, SENDER_IP)
    }

    pub fn target_ip(arp: &[u8]) -> u32 {
        be32(arp, TARGET_IP)
    }

    /// A request or a reply, filled into `arp`.
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

    /// The header's own length in bytes, which a packet with options makes
    /// longer than 20. 0 when the field says something impossible.
    pub fn header_len(ip: &[u8]) -> usize {
        let words = (ip[VERSION_IHL] & 0x0F) as usize;
        if words < 5 { 0 } else { words * 4 }
    }

    pub fn version(ip: &[u8]) -> u8 {
        ip[VERSION_IHL] >> 4
    }

    pub fn total_len(ip: &[u8]) -> u16 {
        be16(ip, TOTAL_LEN)
    }

    pub fn protocol(ip: &[u8]) -> u8 {
        ip[PROTOCOL]
    }

    pub fn src(ip: &[u8]) -> u32 {
        be32(ip, SRC)
    }

    pub fn dst(ip: &[u8]) -> u32 {
        be32(ip, DST)
    }

    /// A 20-byte header with no options, checksum included.
    pub fn write(ip: &mut [u8], protocol: u8, src: u32, dst: u32, payload_len: usize, id: u16) {
        ip[VERSION_IHL] = 0x45;
        ip[TOS] = 0;
        set_be16(ip, TOTAL_LEN, (IP_HDR_LEN + payload_len) as u16);
        set_be16(ip, ID, id);
        set_be16(ip, FRAG_OFF, 0);
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

    pub fn src_port(udp: &[u8]) -> u16 {
        be16(udp, SRC_PORT)
    }

    pub fn dst_port(udp: &[u8]) -> u16 {
        be16(udp, DST_PORT)
    }

    pub fn length(udp: &[u8]) -> u16 {
        be16(udp, LENGTH)
    }

    /// The header only; the payload is the caller's to put after it. The
    /// checksum is left at 0, which IPv4 allows and this stack has always
    /// sent.
    pub fn write(udp: &mut [u8], src_port: u16, dst_port: u16, payload_len: usize) {
        set_be16(udp, SRC_PORT, src_port);
        set_be16(udp, DST_PORT, dst_port);
        set_be16(udp, LENGTH, (UDP_HDR_LEN + payload_len) as u16);
        set_be16(udp, CHECKSUM, 0);
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

    pub fn kind(icmp: &[u8]) -> u8 {
        icmp[TYPE]
    }

    pub fn code(icmp: &[u8]) -> u8 {
        icmp[CODE]
    }

    pub fn id(icmp: &[u8]) -> u16 {
        be16(icmp, ID)
    }

    pub fn seq(icmp: &[u8]) -> u16 {
        be16(icmp, SEQ)
    }

    /// The header, with the checksum taken over it and everything after it
    /// within `len` -- which is what an ICMP checksum covers.
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

    pub fn src_port(seg: &[u8]) -> u16 {
        be16(seg, SRC_PORT)
    }

    pub fn dst_port(seg: &[u8]) -> u16 {
        be16(seg, DST_PORT)
    }

    pub fn seq(seg: &[u8]) -> u32 {
        be32(seg, SEQ)
    }

    pub fn ack(seg: &[u8]) -> u32 {
        be32(seg, ACK)
    }

    pub fn flags(seg: &[u8]) -> u8 {
        seg[FLAGS]
    }

    pub fn window(seg: &[u8]) -> u16 {
        be16(seg, WINDOW)
    }

    /// The header's own length in bytes, options included. 0 when the field
    /// says something shorter than a header.
    pub fn header_len(seg: &[u8]) -> usize {
        let words = (seg[DATA_OFF] >> 4) as usize;
        if words < 5 { 0 } else { words * 4 }
    }

    /// The header, with no checksum yet: the caller takes it once the payload
    /// is in place, over the pseudo-header too.
    #[allow(clippy::too_many_arguments)]
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
