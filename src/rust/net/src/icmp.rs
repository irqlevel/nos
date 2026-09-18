//! ICMP: the echo requests `ping` sends and answers, and the unreachables
//! that tell a TCP connection to stop trying.
//!
//! A ping is answered only when it is addressed to this machine: replying to
//! a broadcast, or to somebody else's address, is what turns a host into an
//! amplifier for someone else's flood.

use core::sync::atomic::{AtomicU64, Ordering};

use kcore::net::Nic;
use kcore::sync::SpinLock;
use kcore::time;
use kcore::trace;

use crate::arp::ArpTable;
use crate::wire::{self, eth, icmp, ip, ETH_HDR_LEN, ETH_TYPE_IP, ICMP_HDR_LEN,
                  IP_HDR_LEN, IP_PROTO_ICMP, IP_PROTO_TCP, MAC_BROADCAST};

/// What an echo request this kernel sends carries after the header.
const PAYLOAD_LEN: usize = 32;

/// The biggest frame this builds, headers included: an Ethernet MTU.
const MAX_FRAME: usize = 1514;

/// Per-subsystem trace level (kernel/trace.h: IcmpLL)
const ICMP_LL: u32 = 5;

#[derive(Clone, Copy)]
struct Reply {
    valid: bool,
    id: u16,
    seq: u16,
    at_ns: u64,
    /// When the request this would answer went out
    sent_ns: u64,
}

pub struct Icmp {
    /// The one echo exchange in flight: what was sent, and what came back
    reply: SpinLock<Reply>,

    echo_req_rx: AtomicU64,
    echo_reply_tx: AtomicU64,
    echo_reply_tx_fail: AtomicU64,
    echo_req_tx: AtomicU64,
    echo_reply_rx: AtomicU64,
    rx_too_short: AtomicU64,
    rx_other: AtomicU64,
    rx_bad_csum: AtomicU64,
}

/// What `icmpstat` reports, in the order the shell prints it.
pub struct Stats {
    pub echo_req_rx: u64,
    pub echo_req_tx: u64,
    pub echo_reply_rx: u64,
    pub echo_reply_tx: u64,
    pub echo_reply_tx_fail: u64,
    pub rx_other: u64,
    pub rx_too_short: u64,
    pub rx_bad_csum: u64,
}

impl Icmp {
    pub fn new() -> Option<Icmp> {
        Some(Icmp {
            reply: SpinLock::new(
                Reply { valid: false, id: 0, seq: 0, at_ns: 0, sent_ns: 0 })?,
            echo_req_rx: AtomicU64::new(0),
            echo_reply_tx: AtomicU64::new(0),
            echo_reply_tx_fail: AtomicU64::new(0),
            echo_req_tx: AtomicU64::new(0),
            echo_reply_rx: AtomicU64::new(0),
            rx_too_short: AtomicU64::new(0),
            rx_other: AtomicU64::new(0),
            rx_bad_csum: AtomicU64::new(0),
        })
    }

    /// What the receive path hands over.
    pub fn process(&self, nic: &Nic, frame: &[u8]) {
        if frame.len() < ETH_HDR_LEN + IP_HDR_LEN + ICMP_HDR_LEN {
            self.rx_too_short.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let packet = &frame[ETH_HDR_LEN..];
        /* Options make the header longer, and shift where the ICMP starts */
        let ip_len = ip::header_len(packet);
        if ip_len == 0 || frame.len() < ETH_HDR_LEN + ip_len + ICMP_HDR_LEN {
            self.rx_too_short.fetch_add(1, Ordering::Relaxed);
            return;
        }

        /* The whole ICMP message, from the IP total length, bounds-checked
         * before anything reads the body */
        let total = ip::total_len(packet) as usize;
        if total < ip_len + ICMP_HDR_LEN || ETH_HDR_LEN + total > frame.len() {
            self.rx_too_short.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let msg_len = total - ip_len;
        let msg = &frame[ETH_HDR_LEN + ip_len..ETH_HDR_LEN + ip_len + msg_len];

        /* A corrupt packet is dropped rather than echoed back */
        if wire::checksum(msg) != 0 {
            self.rx_bad_csum.fetch_add(1, Ordering::Relaxed);
            return;
        }

        match (icmp::kind(msg), icmp::code(msg)) {
            (icmp::ECHO_REQUEST, 0) => self.answer_echo(nic, frame, packet, msg),
            (icmp::ECHO_REPLY, 0) => {
                self.echo_reply_rx.fetch_add(1, Ordering::Relaxed);
                let mut reply = self.reply.lock();
                reply.valid = true;
                reply.id = icmp::id(msg);
                reply.seq = icmp::seq(msg);
                reply.at_ns = time::boot_time_ns();
            }
            (icmp::DEST_UNREACH, code) => {
                self.rx_other.fetch_add(1, Ordering::Relaxed);
                self.unreachable(code, msg);
            }
            _ => {
                self.rx_other.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn answer_echo(&self, nic: &Nic, frame: &[u8], packet: &[u8], msg: &[u8]) {
        self.echo_req_rx.fetch_add(1, Ordering::Relaxed);

        /* Only pings addressed to us: answering a broadcast, or an address
         * that is not ours, makes this host somebody else's amplifier. */
        if ip::dst(packet) != nic.ip() {
            return;
        }

        let reply_len = ETH_HDR_LEN + IP_HDR_LEN + msg.len();
        if reply_len > MAX_FRAME {
            return;
        }

        trace!(ICMP_LL, "icmp: echo request from {:#x} id {} seq {}",
            ip::src(packet), icmp::id(msg), icmp::seq(msg));

        let mut reply = [0u8; MAX_FRAME];
        let (src_ip, dst_ip) = (ip::dst(packet), ip::src(packet));

        eth::write(&mut reply, &eth::src(frame), &nic.mac(), ETH_TYPE_IP);
        ip::write(&mut reply[ETH_HDR_LEN..], IP_PROTO_ICMP, src_ip, dst_ip, msg.len(), 0);

        /* The whole message comes back, payload and all, as an echo reply.
         * The reply's own IP header carries no options, so the copy starts
         * where the request's ICMP did and lands at a fixed offset. */
        let at = ETH_HDR_LEN + IP_HDR_LEN;
        reply[at..at + msg.len()].copy_from_slice(msg);
        icmp::write(&mut reply[at..], icmp::ECHO_REPLY, 0,
            icmp::id(msg), icmp::seq(msg), msg.len());

        if nic.send_raw(&reply[..reply_len]) {
            self.echo_reply_tx.fetch_add(1, Ordering::Relaxed);
        } else {
            self.echo_reply_tx_fail.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A destination that could not be reached. The hard errors abort the
    /// quoted connection instead of leaving it to retransmit into a void
    /// (RFC 1122 4.2.3.9); the rest are counted and dropped.
    fn unreachable(&self, code: u8, msg: &[u8]) {
        if code != icmp::PROTO_UNREACH && code != icmp::PORT_UNREACH {
            return;
        }

        /* What comes back is the original IP header and the first 8 bytes of
         * its datagram -- enough to name the connection and where it was. */
        if msg.len() < ICMP_HDR_LEN + IP_HDR_LEN + 8 {
            return;
        }

        let quoted = &msg[ICMP_HDR_LEN..];
        let quoted_len = ip::header_len(quoted);
        if quoted_len == 0
            || msg.len() < ICMP_HDR_LEN + quoted_len + 8
            || ip::protocol(quoted) != IP_PROTO_TCP
        {
            return;
        }

        /* The quoted segment is one this machine sent: its source is our end */
        let seg = &quoted[quoted_len..];
        crate::tcp::TCP.on_icmp_unreachable(
            ip::src(quoted), wire::be16(seg, 0),
            ip::dst(quoted), wire::be16(seg, 2),
            wire::be32(seg, 4));
    }

    /// An echo request to `dst`, with the payload `ping` expects back.
    pub fn send_echo_request(&self, nic: &Nic, arp: &ArpTable, dst: u32, id: u16, seq: u16)
        -> bool
    {
        /* Off-subnet destinations are reached through the gateway */
        let target = nic.route_ip(dst);
        let dst_mac = arp.resolve(nic, target).unwrap_or(MAC_BROADCAST);

        let msg_len = ICMP_HDR_LEN + PAYLOAD_LEN;
        let frame_len = ETH_HDR_LEN + IP_HDR_LEN + msg_len;
        let mut frame = [0u8; MAX_FRAME];

        eth::write(&mut frame, &dst_mac, &nic.mac(), ETH_TYPE_IP);
        ip::write(&mut frame[ETH_HDR_LEN..], IP_PROTO_ICMP, nic.ip(), dst, msg_len, 0);

        let at = ETH_HDR_LEN + IP_HDR_LEN;
        for i in 0..PAYLOAD_LEN {
            frame[at + ICMP_HDR_LEN + i] = i as u8;
        }
        icmp::write(&mut frame[at..], icmp::ECHO_REQUEST, 0, id, seq, msg_len);

        {
            let mut reply = self.reply.lock();
            reply.valid = false;
            reply.sent_ns = time::boot_time_ns();
        }

        let ok = nic.send_raw(&frame[..frame_len]);
        if ok {
            self.echo_req_tx.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    /// The round trip of the reply to (id, seq), or None once the timeout
    /// passes with none.
    pub fn wait_reply(&self, id: u16, seq: u16, timeout_ms: u64) -> Option<u64> {
        let deadline = time::boot_time_ns() + timeout_ms * kcore::consts::NS_PER_MS;

        while time::boot_time_ns() < deadline {
            {
                let mut reply = self.reply.lock();
                if reply.valid && reply.id == id && reply.seq == seq {
                    reply.valid = false;
                    return Some(reply.at_ns.saturating_sub(reply.sent_ns));
                }
            }
            kcore::task::sleep_ms(10);
        }

        None
    }

    pub fn stats(&self) -> Stats {
        Stats {
            echo_req_rx: self.echo_req_rx.load(Ordering::Relaxed),
            echo_req_tx: self.echo_req_tx.load(Ordering::Relaxed),
            echo_reply_rx: self.echo_reply_rx.load(Ordering::Relaxed),
            echo_reply_tx: self.echo_reply_tx.load(Ordering::Relaxed),
            echo_reply_tx_fail: self.echo_reply_tx_fail.load(Ordering::Relaxed),
            rx_other: self.rx_other.load(Ordering::Relaxed),
            rx_too_short: self.rx_too_short.load(Ordering::Relaxed),
            rx_bad_csum: self.rx_bad_csum.load(Ordering::Relaxed),
        }
    }
}

