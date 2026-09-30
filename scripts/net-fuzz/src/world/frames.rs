//! Frames as the world's hosts make them: every header right unless it is
//! told to lie, and then about exactly the field it is told.

use netwire::{self as wire, ip, tcp, udp, ETH_HDR_LEN, ETH_TYPE_ARP, ETH_TYPE_IP, IP_HDR_LEN};

pub type Mac = [u8; 6];

pub fn eth(dst: Mac, src: Mac, ether_type: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0u8; ETH_HDR_LEN];
    wire::eth::write(&mut f, &dst, &src, ether_type);
    f.extend_from_slice(payload);
    f
}

pub fn arp(dst: Mac, src: Mac, op: u16, sender_mac: Mac, sender_ip: u32, target_mac: Mac, target_ip: u32) -> Vec<u8> {
    let mut a = [0u8; wire::ARP_LEN];
    wire::arp::write(&mut a, op, &sender_mac, sender_ip, &target_mac, target_ip);
    eth(dst, src, ETH_TYPE_ARP, &a)
}

/// An IPv4 header's every field, and the lies it may tell.
#[derive(Clone)]
pub struct Ip {
    pub src: u32,
    pub dst: u32,
    pub proto: u8,
    pub ttl: u8,
    pub id: u16,
    /// The flags and fragment offset word.
    pub frag: u16,
    pub tos: u8,
    /// Options, padded to a multiple of four with NOPs (at most 40 bytes).
    pub options: Vec<u8>,
    /// The version nibble: 4, or a lie.
    pub version: u8,
    /// The header length field in words, when it lies about the header.
    pub ihl_words: Option<u8>,
    /// The total length field, when it lies about the packet.
    pub total_len: Option<u16>,
    /// The header checksum, wrong.
    pub bad_sum: bool,
}

impl Ip {
    pub fn new(src: u32, dst: u32, proto: u8) -> Ip {
        Ip { src, dst, proto, ttl: 64, id: 0, frag: 0, tos: 0, options: Vec::new(), version: 4, ihl_words: None,
             total_len: None, bad_sum: false }
    }

    /// The header, then `payload`.
    pub fn packet(&self, payload: &[u8]) -> Vec<u8> {
        let mut options = self.options.clone();
        options.truncate(40);
        while options.len() % 4 != 0 {
            options.push(1);
        }
        let hlen = IP_HDR_LEN + options.len();
        let mut p = vec![0u8; hlen];
        let words = self.ihl_words.unwrap_or((hlen / 4) as u8) & 0x0F;
        p[ip::VERSION_IHL] = (self.version << 4) | words;
        p[ip::TOS] = self.tos;
        let total = self.total_len.unwrap_or((hlen + payload.len()).min(0xFFFF) as u16);
        wire::set_be16(&mut p, ip::TOTAL_LEN, total);
        wire::set_be16(&mut p, ip::ID, self.id);
        wire::set_be16(&mut p, ip::FRAG_OFF, self.frag);
        p[ip::TTL] = self.ttl;
        p[ip::PROTOCOL] = self.proto;
        wire::set_be32(&mut p, ip::SRC, self.src);
        wire::set_be32(&mut p, ip::DST, self.dst);
        p[IP_HDR_LEN..].copy_from_slice(&options);
        let sum = wire::checksum(&p[..hlen]);
        wire::set_be16(&mut p, ip::CHECKSUM, if self.bad_sum { sum ^ 0x0101 } else { sum });
        p.extend_from_slice(payload);
        p
    }
}

/// How a transport's checksum goes: right, none (UDP's 0), or wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sum {
    Right,
    Zero,
    Wrong,
}

pub fn udp(src: u32, dst: u32, sport: u16, dport: u16, payload: &[u8], sum: Sum) -> Vec<u8> {
    let mut d = vec![0u8; wire::UDP_HDR_LEN];
    udp::write(&mut d, sport, dport, payload.len());
    d.extend_from_slice(payload);
    let s = match sum {
        Sum::Zero => 0,
        Sum::Right => udp::checksum(src, dst, &d),
        Sum::Wrong => udp::checksum(src, dst, &d) ^ 0x8001,
    };
    wire::set_be16(&mut d, udp::CHECKSUM, s);
    d
}

/// A TCP segment: `options` after the fixed header, padded to a word.
#[allow(clippy::too_many_arguments)]
pub fn tcp(src: u32, dst: u32, sport: u16, dport: u16, seq: u32, ack: u32, flags: u8, window: u16, options: &[u8],
           payload: &[u8], sum: Sum) -> Vec<u8> {
    let mut opts = options.to_vec();
    opts.truncate(40);
    while opts.len() % 4 != 0 {
        opts.push(tcp::OPT_NOP);
    }
    let hlen = tcp::HDR_LEN + opts.len();
    let mut s = vec![0u8; hlen];
    tcp::write(&mut s, sport, dport, seq, ack, hlen, flags, window);
    s[tcp::HDR_LEN..].copy_from_slice(&opts);
    s.extend_from_slice(payload);
    let c = tcp::checksum(src, dst, &s);
    wire::set_be16(&mut s, tcp::CHECKSUM, if sum == Sum::Wrong { c ^ 0x0110 } else { c });
    s
}

/// The MSS option.
pub fn mss(value: u16) -> Vec<u8> {
    let mut o = vec![tcp::OPT_MSS, tcp::OPT_MSS_LEN];
    o.extend_from_slice(&value.to_be_bytes());
    o
}

pub fn icmp(kind: u8, code: u8, id: u16, seq: u16, payload: &[u8], sum: Sum) -> Vec<u8> {
    let mut m = vec![0u8; wire::ICMP_HDR_LEN];
    m.extend_from_slice(payload);
    let n = m.len();
    wire::icmp::write(&mut m, kind, code, id, seq, n);
    if sum == Sum::Wrong {
        m[wire::icmp::CHECKSUM] ^= 0x40;
    }
    m
}

/// An IPv4 frame: `ip`'s header over `payload`, from `src` to `dst`.
pub fn ipv4(dst: Mac, src: Mac, ip: &Ip, payload: &[u8]) -> Vec<u8> {
    eth(dst, src, ETH_TYPE_IP, &ip.packet(payload))
}
