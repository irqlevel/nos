//! The guests' network: the switch between their NICs and `hv0`
//! (`modules/hv/src/net.rs`), with its DHCP server, and NAT
//! (`net/src/nat.rs`), which takes a guest's packets out into the world and
//! the world's answers back. Frames here are the guests' own, and the
//! world's: anything at all, and every field of them a lie now and then.

use std::collections::VecDeque;
use std::sync::Arc;

use netwire::{arp, udp, ARP_LEN, ETH_HDR_LEN, ETH_TYPE_ARP, MAC_BROADCAST, MAX_FRAME};

use crate::dhcp;
use crate::network;
use crate::nic::Backend as _;
use crate::offload;
use crate::switch::{self, Switch, MAX_PORTS};
use crate::targets::Input;
use crate::vms::Shared;
use crate::vnic;

/* The switch's own constants, as its source has them: the model below says
 * from them where every frame must go. */
const HOST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0x64, 0x01];
const HOST_IP: u32 = 0x0A00_6401;
const MASK: u32 = 0xFFFF_FF00;
const PORT_MAC_PREFIX: [u8; 5] = [0x02, 0, 0, 0, 0x64];
const SWITCH_PORT_BASE: usize = 2;
const INBOX_FRAMES: usize = 256;
/// The IPv6 addresses a port has bound at once, past which a new one takes
/// the place of its least recently used.
const ADDRS6_PER_PORT: usize = 8;
/// The networks the ports are put on: a port on none is alone.
const NETS: [&str; 2] = ["lab", "dev"];
/// The link-local prefix, and the switch's: a guest's addresses in them are
/// its MAC's EUI-64 (RFC 4291).
const LINK_LOCAL: [u8; 8] = [0xFE, 0x80, 0, 0, 0, 0, 0, 0];
const PREFIX6: [u8; 8] = [0xFD, 0x6E, 0x6F, 0x73, 0, 0x64, 0, 0];

/// The address `prefix` and port `p`'s MAC make.
fn eui64(prefix: &[u8; 8], p: usize) -> [u8; 16] {
    let m = switch::port_mac(p);
    let mut a = [0u8; 16];
    a[..8].copy_from_slice(prefix);
    a[8..].copy_from_slice(&[m[0] ^ 2, m[1], m[2], 0xFF, 0xFE, m[3], m[4], m[5]]);
    a
}

/// The switch's router advertisement to port `p`'s guest at `to`, laid out
/// as RFC 4861 lays one out (4.2, 4.6) -- the model's own bytes: from `hv0`'s
/// MAC and link-local address, hop limit 255; a current hop limit of 64,
/// no flags, a router lifetime of 0, no reachable time or retransmit timer;
/// `hv0`'s MAC, an MTU of 1500, and the prefix -- a /64, on the link and to
/// make addresses in, for good.
fn router_advert(p: usize, to: &[u8; 16]) -> Vec<u8> {
    let from = [0xFE, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFE, 0, 0x64, 0x01];
    let mut msg = vec![134u8, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    msg.extend_from_slice(&[1, 1]);
    msg.extend_from_slice(&HOST_MAC);
    msg.extend_from_slice(&[5, 1, 0, 0]);
    msg.extend_from_slice(&1500u32.to_be_bytes());
    msg.extend_from_slice(&[3, 4, 64, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]);
    msg.extend_from_slice(&PREFIX6);
    msg.extend_from_slice(&[0; 8]);
    let mut sum: u64 = msg.len() as u64 + 58;
    for w in from.chunks(2).chain(to.chunks(2)).chain(msg.chunks(2)) {
        sum += u64::from(u16::from_be_bytes([w[0], w[1]]));
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    msg[2..4].copy_from_slice(&(!(sum as u16)).to_be_bytes());
    let mut f = switch::port_mac(p).to_vec();
    f.extend_from_slice(&HOST_MAC);
    f.extend_from_slice(&[0x86, 0xDD, 0x60, 0, 0, 0]);
    f.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    f.extend_from_slice(&[58, 255]);
    f.extend_from_slice(&from);
    f.extend_from_slice(to);
    f.extend_from_slice(&msg);
    f
}

/// Whether the options of a neighbour discovery message, `opts` on, are
/// well made -- none of length 0, none past the message -- and every
/// link-layer address among them, a source's or a target's, `mac`.
fn nd_options_are(opts: &[u8], mac: &[u8; 6]) -> bool {
    let mut i = 0;
    while i < opts.len() {
        if i + 1 >= opts.len() {
            return false;
        }
        let len = usize::from(opts[i + 1]) * 8;
        if len == 0 || i + len > opts.len() {
            return false;
        }
        if (opts[i] == 1 || opts[i] == 2) && (len != 8 || opts[i + 2..i + 8] != mac[..]) {
            return false;
        }
        i += len;
    }
    true
}

/// `n` bytes of payload from `seed`: varied, and not the input's to spend --
/// drawn from the input a byte at a time, a frame's payload took most of a
/// script, and left room for a few steps.
fn noise(seed: u32, n: usize) -> Vec<u8> {
    let mut x = u64::from(seed) | 1 << 40;
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(n);
    v
}

fn port_of(mac: &[u8]) -> Option<usize> {
    if mac[..5] != PORT_MAC_PREFIX {
        return None;
    }
    usize::from(mac[5]).checked_sub(SWITCH_PORT_BASE).filter(|p| *p < MAX_PORTS)
}

/// What the switch must make of a frame from port `p`, said from what port
/// security is: a guest's frame is IPv4 or ARP and its own -- from its MAC,
/// an IPv4 packet from its address and an ARP message with its MAC and
/// address as the sender's, or no address yet -- or it goes nowhere. The one
/// packet with no address yet is a DHCP client's, answered and seen by
/// nobody.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Admit {
    Own,
    /// IPv6 of its own: to the other guests, never to `hv0`.
    Own6,
    /// A router solicitation of its own: answered, and seen by nobody.
    Solicit,
    Dhcp,
    Spoofed,
    Foreign,
}

fn admit(p: usize, f: &[u8]) -> Admit {
    let (mac, me) = (switch::port_mac(p), switch::port_ip(p));
    let from_mac = f[6..12] == mac;
    let own = match wire::be16(f, 12) {
        ETH_TYPE_IP if f.len() >= ETH_HDR_LEN + IP_HDR_LEN => {
            let src = wire::be32(f, ETH_HDR_LEN + ip::SRC);
            if dhcp::is_request(f) && (src == 0 || src == me) {
                return if from_mac { Admit::Dhcp } else { Admit::Spoofed };
            }
            src == me
        }
        ETH_TYPE_ARP if f.len() >= ETH_HDR_LEN + ARP_LEN => {
            let a = &f[ETH_HDR_LEN..];
            arp::sender_mac(a) == mac && (arp::sender_ip(a) == 0 || arp::sender_ip(a) == me)
        }
        _ => return Admit::Foreign,
    };
    if own && from_mac { Admit::Own } else { Admit::Spoofed }
}

/// Where the switch must put each frame, and what it must count: said from
/// what the switch is for, and held against what it does.
/// Sends of TCP the switch carried whole and cut, for `HV_FUZZ_STATS`.
static WHOLE_SENDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Ports changed hands as a guest sent, after the send and at one of its
/// locks; and which of the model's ends each came to (`hand_over`).
static HANDOVERS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];
static HANDOVER_ENDS: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];
static CUT_SENDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// IPv6 frames from the guests by what the model made of each (`Admit`'s
/// order), and addresses a port bound in the place of its oldest.
static ADMIT6: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];
static EVICTED6: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What an inbox entry is, by the header it goes to its guest with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Done,
    Partial(usize, usize),
    Whole,
}

#[derive(Clone)]
struct Model {
    owned: [bool; MAX_PORTS],
    /// The network each port is on -- a place in `NETS` -- or None, alone:
    /// which guests it reaches.
    net: [Option<usize>; MAX_PORTS],
    /// Frames each port's guest sent to another network's guest.
    isolated: [u64; MAX_PORTS],
    /// What waits for each port's NIC: frames, frames whose checksum is left
    /// to finish, and sends carried whole.
    inbox: Vec<VecDeque<(Vec<u8>, Kind)>>,
    /// Whether each port's NIC takes checksums left to finish, and sends
    /// whole.
    takes_partial: [bool; MAX_PORTS],
    takes: [bool; MAX_PORTS],
    dropped: [u64; MAX_PORTS],
    /// Frames each port's guest sent as another, since it claimed the port.
    spoofed: [u64; MAX_PORTS],
    /// Frames delivered to each port's VM, which it is woken for once each
    /// -- but while its NIC holds its receive interrupts (`quiet`) and the
    /// inbox is under half full.
    woken: [u64; MAX_PORTS],
    quiet: [bool; MAX_PORTS],
    /// Each port's frames on `hv0`'s backlog since the stack's last pass,
    /// and those its share kept out; its sends to every port their rate
    /// kept out, and its bucket for them: tokens, and when topped up.
    on_backlog: [usize; MAX_PORTS],
    over_share: [u64; MAX_PORTS],
    flooded: [u64; MAX_PORTS],
    flood: [(u64, u64); MAX_PORTS],
    to_host: u64,
    refused: u64,
    dhcp: u64,
    /// Guests' frames of neither IPv4, ARP nor IPv6, IPv6 for `hv0`, and
    /// IPv6 hiding what it carries.
    foreign: u64,
    /// The IPv6 addresses bound, each to a port, with when it last used it
    /// (RFC 6620's first come, first served) -- a network's own, by its
    /// group (`group`) -- and the count of uses.
    bound: BTreeMap<(u32, [u8; 16]), (usize, u64)>,
    uses: u64,
    /// Router solicitations answered.
    advertised: u64,
    /// What the stack must have been handed through `hv0`.
    stack: Vec<Vec<u8>>,
    dns: u32,
    refuse: bool,
    /// A port the next frame for -- or kept from, as another network's --
    /// is lost instead: what one sent as the port changes hands may come
    /// to, for the VM going or the VM coming (`hand_over`).
    lose: Option<usize>,
}

impl Model {
    fn new() -> Model {
        Model {
            owned: [false; MAX_PORTS],
            net: [None; MAX_PORTS],
            isolated: [0; MAX_PORTS],
            inbox: vec![VecDeque::new(); MAX_PORTS],
            dropped: [0; MAX_PORTS],
            spoofed: [0; MAX_PORTS],
            woken: [0; MAX_PORTS],
            quiet: [false; MAX_PORTS],
            takes_partial: [false; MAX_PORTS],
            takes: [false; MAX_PORTS],
            on_backlog: [0; MAX_PORTS],
            over_share: [0; MAX_PORTS],
            flooded: [0; MAX_PORTS],
            flood: [(switch::FLOOD_BURST, 0); MAX_PORTS],
            to_host: 0,
            refused: 0,
            dhcp: 0,
            foreign: 0,
            bound: BTreeMap::new(),
            uses: 0,
            advertised: 0,
            stack: Vec::new(),
            dns: 0,
            refuse: false,
            lose: None,
        }
    }

    /// Port `p` given to a VM, on network `net` or alone: its counts and
    /// what its NIC takes a new port's.
    fn claim(&mut self, p: usize, net: Option<usize>) {
        self.owned[p] = true;
        self.net[p] = net;
        self.isolated[p] = 0;
        self.inbox[p].clear();
        self.woken[p] = 0;
        self.quiet[p] = false;
        self.takes_partial[p] = false;
        self.takes[p] = false;
        self.dropped[p] = 0;
        self.spoofed[p] = 0;
        self.on_backlog[p] = 0;
        self.over_share[p] = 0;
        self.flooded[p] = 0;
        self.flood[p] = (switch::FLOOD_BURST, time::peek());
    }

    /// Port `p` given up, with what waited for its VM and the IPv6 addresses
    /// it had bound: how many times that VM was woken.
    fn release(&mut self, p: usize) -> u64 {
        self.owned[p] = false;
        self.net[p] = None;
        self.inbox[p].clear();
        self.bound.retain(|_, b| b.0 != p);
        self.woken[p]
    }

    /// Port `p` given up and given to another VM, on network `net` or
    /// alone: how many times the VM that had it was woken.
    fn hand_over(&mut self, p: usize, net: Option<usize>) -> u64 {
        let woken = self.release(p);
        self.claim(p, net);
        woken
    }

    /// What port `p`'s guest lost, as the switch counts it.
    fn counts(&self, p: usize) -> switch::PortCounts {
        switch::PortCounts { dropped: self.dropped[p], spoofed: self.spoofed[p], over_share: self.over_share[p],
                             flooded: self.flooded[p], isolated: self.isolated[p] }
    }

    /// Whether the frame at hand is lost for port `p` (`lose`): once.
    fn lost(&mut self, p: usize) -> bool {
        let lost = self.lose == Some(p);
        if lost {
            self.lose = None;
        }
        lost
    }

    fn deliver(&mut self, p: usize, frame: &[u8]) {
        self.deliver_kind(p, frame, Kind::Done);
    }

    /// Port `p`'s group, the switch's way of saying whom it reaches: a
    /// network's, shared by its ports, or one of its own -- and 0 for a
    /// port with no VM, which a NIC this target keeps past its port's
    /// release still sends from.
    fn group(&self, p: usize) -> u32 {
        if !self.owned[p] {
            return 0;
        }
        match self.net[p] {
            Some(n) => 1 + n as u32,
            None => 0x8000_0000 | p as u32,
        }
    }

    /// Whether ports `p` and `q` are a VM's each, on one network.
    fn same(&self, p: usize, q: usize) -> bool {
        self.owned[p] && self.owned[q] && self.group(p) == self.group(q)
    }

    /// Whether IPv6 address `a` is port `p`'s to use: bound to it, now if
    /// it was nobody's -- in the place of its least recently used, if it
    /// has as many as it may -- and not if it is another's.
    fn claim6(&mut self, p: usize, a: [u8; 16]) -> bool {
        self.uses += 1;
        let now = self.uses;
        let key = (self.group(p), a);
        match self.bound.get_mut(&key) {
            Some(b) if b.0 != p => return false,
            Some(b) => {
                b.1 = now;
                return true;
            }
            None => {}
        }
        let mine: Vec<((u32, [u8; 16]), u64)> =
            self.bound.iter().filter(|(_, b)| b.0 == p).map(|(k, b)| (*k, b.1)).collect();
        if mine.len() == ADDRS6_PER_PORT {
            let oldest = mine.iter().min_by_key(|m| m.1).map(|m| m.0).expect("a port's addresses");
            self.bound.remove(&oldest);
            EVICTED6.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.bound.insert(key, (p, now));
        true
    }

    /// What the switch must make of a frame from port `p`: IPv6 by its
    /// rules (`admit6`), the rest by `admit`'s.
    fn admit(&mut self, p: usize, f: &[u8]) -> Admit {
        if f.len() >= ETH_HDR_LEN && wire::be16(f, 12) == 0x86DD {
            let a = self.admit6(p, f);
            ADMIT6[a as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return a;
        }
        admit(p, f)
    }

    /// An IPv6 frame from port `p`, said from what a switch guarding its
    /// guests' addresses is (RFC 6620, 7113, 6980): the packet taken to the
    /// length its header says, past which the frame is none of it, or none
    /// at all if that is past the frame; from the port's MAC; the chain of
    /// extension headers walked to what the packet carries, or it is hidden
    /// and goes nowhere. A router's advertisement or redirect is not a
    /// guest's to send, nor is a DHCPv6 server's reply; neighbour discovery
    /// comes whole and gives the port's MAC wherever it gives one, and a
    /// target that is one address. The source is the port's, bound as it
    /// is used, or none -- only while finding an address: a solicitation of
    /// a router's or of a neighbour's, which binds what it looks for if it
    /// is nobody's, or a listener's report. A neighbour advertisement's
    /// target is the port's too.
    fn admit6(&mut self, p: usize, f: &[u8]) -> Admit {
        if f.len() < ETH_HDR_LEN + 40 || f[ETH_HDR_LEN] >> 4 != 6 {
            return Admit::Foreign;
        }
        let end = ETH_HDR_LEN + 40 + usize::from(wire::be16(f, ETH_HDR_LEN + 4));
        if end > f.len() {
            return Admit::Foreign;
        }
        let pkt = &f[ETH_HDR_LEN..end];
        let mac = switch::port_mac(p);
        if f[6..12] != mac {
            return Admit::Spoofed;
        }
        let (mut next, mut at, mut fragment) = (pkt[6], 40usize, false);
        let mut carried = None;
        let mut later = false;
        for _ in 0..8 {
            match next {
                0 | 43 | 60 | 51 => {
                    let Some(&units) = pkt.get(at + 1) else { return Admit::Foreign };
                    let len = if next == 51 { (usize::from(units) + 2) * 4 } else { (usize::from(units) + 1) * 8 };
                    if at + len > pkt.len() {
                        return Admit::Foreign;
                    }
                    next = pkt[at];
                    at += len;
                }
                44 => {
                    if at + 8 > pkt.len() {
                        return Admit::Foreign;
                    }
                    if wire::be16(pkt, at + 2) >> 3 != 0 {
                        later = true;
                        break;
                    }
                    fragment = true;
                    next = pkt[at];
                    at += 8;
                }
                proto => {
                    carried = Some(proto);
                    break;
                }
            }
        }
        if carried.is_none() && !later {
            return Admit::Foreign;
        }
        let mut kind = None;
        let mut target = None;
        if carried == Some(58) {
            let msg = &pkt[at..];
            let Some(&k) = msg.first() else { return Admit::Foreign };
            kind = Some(k);
            if k == 134 || k == 137 {
                return Admit::Spoofed;
            }
            let fixed = match k {
                133 => Some(8),
                135 | 136 => Some(24),
                _ => None,
            };
            if let Some(fixed) = fixed {
                if fragment || msg.len() < fixed || !nd_options_are(&msg[fixed..], &mac) {
                    return Admit::Spoofed;
                }
                if k != 133 {
                    let t: [u8; 16] = msg[8..24].try_into().expect("16 bytes");
                    if t[0] == 0xFF {
                        return Admit::Spoofed;
                    }
                    target = Some(t);
                }
            }
        }
        if carried == Some(17) && at + 2 <= pkt.len() && wire::be16(pkt, at) == 547 {
            return Admit::Spoofed;
        }
        let src: [u8; 16] = pkt[8..24].try_into().expect("16 bytes");
        if src == [0; 16] {
            match kind {
                Some(135) => {
                    if let Some(t) = target {
                        self.claim6(p, t);
                    }
                }
                Some(133 | 131 | 143) => {}
                _ => return Admit::Spoofed,
            }
        } else if src[0] == 0xFF {
            return Admit::Spoofed;
        } else {
            if !self.claim6(p, src) {
                return Admit::Spoofed;
            }
            if kind == Some(136) && !self.claim6(p, target.expect("an advertisement's target")) {
                return Admit::Spoofed;
            }
        }
        if kind == Some(133) { Admit::Solicit } else { Admit::Own6 }
    }

    fn deliver_kind(&mut self, p: usize, frame: &[u8], kind: Kind) {
        if !self.owned[p] || self.lost(p) {
            return;
        }
        if self.inbox[p].len() == INBOX_FRAMES || frame.len() > MAX_FRAME {
            self.dropped[p] += 1;
            return;
        }
        self.inbox[p].push_back((frame.to_vec(), kind));
        if !(self.quiet[p] && self.inbox[p].len() < INBOX_FRAMES / 2) {
            self.woken[p] += 1;
        }
    }

    /// A send of TCP whole into port `p`'s inbox, as the switch has it: for
    /// a NIC that takes them, with room in the inbox and for one more of the
    /// eight it holds -- false otherwise, and the caller cuts it.
    fn deliver_whole(&mut self, p: usize, frame: &[u8]) -> bool {
        if frame.len() > offload::MAX_SEND {
            return false;
        }
        if !self.owned[p] || self.lost(p) {
            return true;
        }
        if !self.takes[p] {
            return false;
        }
        let whole = self.inbox[p].iter().filter(|e| e.1 == Kind::Whole).count();
        if self.inbox[p].len() == INBOX_FRAMES || whole == 8 {
            return false;
        }
        self.inbox[p].push_back((frame.to_vec(), Kind::Whole));
        if !(self.quiet[p] && self.inbox[p].len() < INBOX_FRAMES / 2) {
            self.woken[p] += 1;
        }
        WHOLE_SENDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// A send of TCP up to 64 KiB from port `from`: whole to the port it is
    /// for where that port takes it, cut into frames -- the offload model's
    /// cut -- and forwarded each otherwise.
    fn send_tcp4(&mut self, from: usize, frame: &[u8], mss: usize) {
        if frame.len() < ETH_HDR_LEN {
            return;
        }
        if matches!(self.admit(from, frame), Admit::Own) && frame[0] & 1 == 0 {
            if let Some(q) = port_of(&frame[..6]) {
                if q != from && self.same(from, q) && self.deliver_whole(q, frame) {
                    return;
                }
            }
        }
        CUT_SENDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for piece in crate::offloadmodel::cut(frame, mss).expect("a send that held up") {
            self.forward(Some(from), &piece);
        }
    }

    /// A frame from port `from` whose checksum its guest left at `start +
    /// offset`: so to the guest on its network it is for where that guest's
    /// NIC takes it so, and finished -- by the offload model's sum -- for
    /// that guest otherwise, and forwarded for anyone else.
    fn send_partial(&mut self, from: usize, frame: &[u8], start: usize, offset: usize) {
        let mut f = frame.to_vec();
        crate::offloadmodel::finish(&mut f, start, offset);
        if frame.len() >= ETH_HDR_LEN && matches!(self.admit(from, frame), Admit::Own | Admit::Own6)
            && frame[0] & 1 == 0
        {
            if let Some(q) = port_of(&frame[..6]) {
                if q != from && self.same(from, q) {
                    if self.takes_partial[q] {
                        self.deliver_kind(q, frame, Kind::Partial(start, offset));
                    } else {
                        self.deliver(q, &f);
                    }
                    return;
                }
            }
        }
        self.forward(Some(from), &f);
    }

    /// What port `p`'s NIC says it takes.
    fn takes(&mut self, p: usize, takes: offload::Takes) {
        self.takes_partial[p] = takes.partial;
        self.takes[p] = takes.whole;
    }

    /// What port `p`'s NIC takes into a buffer of `room` bytes, as the switch
    /// hands it: a whole send it has no room for is dropped, counted, and
    /// the next looked at.
    fn take(&mut self, p: usize, room: usize) -> Option<(Vec<u8>, Kind)> {
        if !self.owned[p] {
            return None;
        }
        while let Some((f, kind)) = self.inbox[p].pop_front() {
            if kind == Kind::Whole && f.len() > room {
                self.dropped[p] += 1;
                continue;
            }
            return Some((f, kind));
        }
        None
    }

    /// A guest's frame for `hv0`: within its port's share of the backlog --
    /// all of it over the ports a VM has, counted from the stack's last
    /// pass -- or kept out, counted against the port.
    fn to_host(&mut self, from: Option<usize>, frame: &[u8]) {
        let Some(p) = from else { return };
        let in_use = self.owned.iter().filter(|o| **o).count().max(1);
        if self.on_backlog[p] >= vnic::BACKLOG / in_use {
            self.over_share[p] += 1;
            return;
        }
        if self.refuse || frame.len() > MAX_FRAME {
            self.refused += 1;
        } else {
            self.on_backlog[p] += 1;
            self.to_host += 1;
            self.stack.push(frame.to_vec());
        }
    }

    /// Whether port `p` may send to every port now: `FLOOD_BURST` at once,
    /// and `FLOOD_PER_SEC` after.
    fn may_flood(&mut self, p: usize) -> bool {
        let now = time::peek();
        let (mut tokens, mut at) = self.flood[p];
        let earned = now.saturating_sub(at).saturating_mul(switch::FLOOD_PER_SEC) / NS_PER_SEC;
        if earned != 0 {
            tokens = tokens.saturating_add(earned).min(switch::FLOOD_BURST);
            at = now;
        }
        let may = tokens != 0;
        self.flood[p] = (if may { tokens - 1 } else { tokens }, at);
        may
    }

    fn forward(&mut self, from: Option<usize>, frame: &[u8]) {
        if frame.len() < ETH_HDR_LEN {
            return;
        }
        if let Some(p) = from {
            match self.admit(p, frame) {
                Admit::Own | Admit::Own6 => {}
                Admit::Solicit => {
                    /* Answered at the address it came from, or every node's
                     * for none: the switch's advertisement, to it alone. */
                    let asked: [u8; 16] = frame[ETH_HDR_LEN + 8..ETH_HDR_LEN + 24].try_into().expect("16 bytes");
                    let to = if asked == [0; 16] {
                        [0xFF, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
                    } else {
                        asked
                    };
                    self.advertised += 1;
                    self.deliver(p, &router_advert(p, &to));
                    return;
                }
                Admit::Dhcp => {
                    let net = dhcp::Network { server_ip: HOST_IP, server_mac: HOST_MAC, mask: MASK, dns: self.dns };
                    let mut reply = [0u8; dhcp::REPLY_MAX];
                    if let Some(len) = dhcp::answer(frame, switch::port_ip(p), &net, &mut reply) {
                        self.dhcp += 1;
                        self.deliver(p, &reply[..len]);
                    }
                    return;
                }
                Admit::Spoofed => {
                    self.spoofed[p] += 1;
                    return;
                }
                Admit::Foreign => {
                    self.foreign += 1;
                    return;
                }
            }
        }
        let dst = &frame[..6];
        /* IPv6 is the guests' alone: nos speaks none. */
        let v6 = wire::be16(frame, 12) == 0x86DD;
        if dst[0] & 1 == 0 {
            if dst == HOST_MAC {
                if !v6 {
                    self.to_host(from, frame);
                } else if from.is_some() {
                    self.foreign += 1;
                }
                return;
            }
            if let Some(q) = port_of(dst) {
                if Some(q) != from && !self.lost(q) {
                    /* Another network's guest is out of its reach; a free
                     * port takes nothing anyway. */
                    match from {
                        Some(p) if self.owned[q] && !self.same(p, q) => self.isolated[p] += 1,
                        _ => self.deliver(q, frame),
                    }
                }
                return;
            }
        }
        if let Some(p) = from {
            if !self.may_flood(p) {
                self.flooded[p] += 1;
                return;
            }
        }
        /* Every port of its network's, or -- from `hv0` -- every port. */
        for q in 0..MAX_PORTS {
            if Some(q) != from && from.map_or(true, |p| self.same(p, q)) {
                self.deliver(q, frame);
            }
        }
        if !v6 {
            self.to_host(from, frame);
        }
    }
}

/// A MAC a guest on port `p` might send from: its own mostly -- or
/// another port's, `hv0`'s, anyone's.
fn guest_src_mac(r: &mut Input, p: usize) -> [u8; 6] {
    match r.u8() {
        0..=223 => switch::port_mac(p),
        224..=239 => switch::port_mac(r.below(MAX_PORTS as u64) as usize),
        240..=243 => HOST_MAC,
        _ => [r.u8(), r.u8(), r.u8(), r.u8(), r.u8(), r.u8()],
    }
}

/// An address a guest on port `p` might send from: its own mostly -- or
/// none yet, another port's, `hv0`'s, anyone's.
fn guest_src_ip(r: &mut Input, p: usize) -> u32 {
    match r.u8() {
        0..=199 => switch::port_ip(p),
        200..=215 => 0,
        216..=239 => switch::port_ip(r.below(MAX_PORTS as u64) as usize),
        240..=243 => HOST_IP,
        _ => r.u32(),
    }
}

/// An IPv6 address a guest on port `p` might send from or name: its own
/// mostly, link-local or in the switch's prefix -- or another port's, one
/// of a few more in the prefix than a port may have bound, none, a
/// multicast one, anyone's.
fn addr6(r: &mut Input, p: usize) -> [u8; 16] {
    let q = if r.u8() < 208 { p } else { r.below(MAX_PORTS as u64) as usize };
    match r.u8() % 16 {
        0..=5 => eui64(&LINK_LOCAL, q),
        6..=9 => eui64(&PREFIX6, q),
        10..=12 => {
            let mut a = [0u8; 16];
            a[..8].copy_from_slice(&PREFIX6);
            a[15] = 1 + r.below(12) as u8;
            a
        }
        13 => [0; 16],
        14 => [0xFF, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, r.pick(&[1u8, 2, 0x16])],
        _ => core::array::from_fn(|_| r.u8()),
    }
}

/// An IPv6 frame a guest on port `p` might send: neighbour discovery --
/// solicitations of routers and of neighbours, advertisements, and a
/// router's advertisement or redirect it has no business sending -- a
/// multicast listener's report or query, an echo, UDP now and then from
/// DHCPv6's server's port, TCP; behind extension headers, in fragments,
/// past the chain the switch walks; its length in its header right mostly,
/// past the frame or short of the message now and then, the frame running
/// on past it; from its MAC and addresses, and with its MAC in its options,
/// mostly.
fn frame6(r: &mut Input, p: usize) -> Vec<u8> {
    let src_mac = guest_src_mac(r, p);
    let dst_mac: [u8; 6] = match r.u8() % 6 {
        0 | 1 => [0x33, 0x33, 0, 0, 0, r.pick(&[1u8, 2, 0x16])],
        2 => [0x33, 0x33, 0xFF, 0, 0x64, (SWITCH_PORT_BASE + r.below(MAX_PORTS as u64) as usize) as u8],
        3 => HOST_MAC,
        4 => MAC_BROADCAST,
        _ => switch::port_mac(r.below(MAX_PORTS as u64) as usize),
    };
    let src = addr6(r, p);
    let dst = addr6(r, p);
    let lla = |r: &mut Input, kind: u8, m: &mut Vec<u8>| {
        m.extend_from_slice(&[kind, 1]);
        m.extend_from_slice(&guest_src_mac(r, p));
    };
    let (proto, mut l4): (u8, Vec<u8>) = match r.u8() % 12 {
        0 | 1 => {
            let mut m = vec![133, 0, 0, 0, 0, 0, 0, 0];
            if r.bool() {
                lla(r, 1, &mut m);
            }
            (58, m)
        }
        2 | 3 => {
            let mut m = vec![135, 0, 0, 0, 0, 0, 0, 0];
            m.extend_from_slice(&addr6(r, p));
            if r.bool() {
                lla(r, 1, &mut m);
            }
            (58, m)
        }
        4 | 5 => {
            let mut m = vec![136, 0, 0, 0, r.pick(&[0x20u8, 0x60, 0xA0]), 0, 0, 0];
            m.extend_from_slice(&addr6(r, p));
            if r.bool() {
                lla(r, 2, &mut m);
            }
            (58, m)
        }
        6 => {
            let mut m = vec![r.pick(&[134u8, 137]), 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            m.extend_from_slice(&addr6(r, p));
            m.extend_from_slice(&addr6(r, p));
            (58, m)
        }
        7 => {
            let mut m = vec![r.pick(&[143u8, 131, 130, 132]), 0, 0, 0, 0, 0, 0, 1];
            m.extend_from_slice(&[0xFF, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xFF, 0, 0x64, 2]);
            (58, m)
        }
        8 => (58, noise(r.u32(), 8 + r.below(64) as usize).into_iter().enumerate()
                  .map(|(i, b)| if i == 0 { r.pick(&[128u8, 129, 1, 3, 4]) } else { b }).collect()),
        9 | 10 => {
            let sport = r.pick(&[547u16, 546, 5353, 40000]);
            let mut m = noise(r.u32(), 8 + r.below(64) as usize);
            m[..2].copy_from_slice(&sport.to_be_bytes());
            (17, m)
        }
        _ => (6, noise(r.u32(), 20 + r.below(64) as usize)),
    };
    /* An option made wrong now and then: of length 0, or running past the
     * message. */
    if proto == 58 && r.u8() < 16 {
        l4.extend_from_slice(&[r.pick(&[1u8, 2, 3, 5]), r.pick(&[0u8, 2, 9])]);
    }
    let chain: Vec<u8> = match r.u8() % 16 {
        0 => vec![0],
        1 => vec![60],
        2 => vec![44],
        3 => vec![0, 44],
        4 => vec![60; 9],
        5 => vec![51],
        6 => vec![43],
        _ => vec![],
    };
    let later = r.u8() < 12;
    let mut ext = Vec::new();
    for (i, &h) in chain.iter().enumerate() {
        let next = chain.get(i + 1).copied().unwrap_or(proto);
        match h {
            44 => ext.extend_from_slice(&[next, 0, 0, if later { 0x81 } else { 1 }, 0, 0, 0, 7]),
            51 => ext.extend_from_slice(&[next, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            _ => ext.extend_from_slice(&[next, 0, 1, 4, 0, 0, 0, 0]),
        }
    }
    let first = chain.first().copied().unwrap_or(proto);
    let payload = ext.len() + l4.len();
    let said = match r.u8() % 16 {
        0 => payload + 1 + r.below(64) as usize,
        1 => r.below(payload as u64 + 1) as usize,
        _ => payload,
    };
    let mut f = dst_mac.to_vec();
    f.extend_from_slice(&src_mac);
    f.extend_from_slice(&[0x86, 0xDD, if r.u8() < 248 { 0x60 } else { 0x40 }, 0, 0, 0]);
    f.extend_from_slice(&(said.min(0xFFFF) as u16).to_be_bytes());
    f.extend_from_slice(&[first, 255]);
    f.extend_from_slice(&src);
    f.extend_from_slice(&dst);
    f.extend_from_slice(&ext);
    f.extend_from_slice(&l4);
    if r.u8() < 16 {
        f.extend_from_slice(&noise(r.u32(), 1 + r.below(32) as usize));
    }
    f
}

/// An echo request over IPv6 from port `p`'s guest, from `src`, to port
/// `q`'s.
fn echo6(p: usize, src: &[u8; 16], q: usize) -> Vec<u8> {
    let mut f = switch::port_mac(q).to_vec();
    f.extend_from_slice(&switch::port_mac(p));
    f.extend_from_slice(&[0x86, 0xDD, 0x60, 0, 0, 0, 0, 16, 58, 64]);
    f.extend_from_slice(src);
    f.extend_from_slice(&eui64(&LINK_LOCAL, q));
    f.extend_from_slice(&[128, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    f
}

/// A frame a guest -- or the host -- might send: IPv4, ARP or IPv6, as the
/// guest or as another, now and then another protocol; to a port, to `hv0`,
/// to everyone, to nobody in particular; a DHCP request; of any length.
fn frame(r: &mut Input, from: Option<usize>) -> Vec<u8> {
    let src = match from {
        Some(p) => guest_src_mac(r, p),
        None => HOST_MAC,
    };
    if r.u8() < 24 {
        if let Some(p) = from {
            return dhcp_request(r, p);
        }
    }
    if r.u8() < 40 {
        if let Some(p) = from {
            return frame6(r, p);
        }
    }
    let dst: [u8; 6] = match r.u8() % 10 {
        0..=3 => switch::port_mac(r.below(MAX_PORTS as u64) as usize),
        4 => HOST_MAC,
        5 => MAC_BROADCAST,
        6 => [0x01, 0, 0x5E, 0, 0, r.u8()],
        7 => [0x02, 0, 0, 0, 0x64, r.pick(&[0, 1, 0xFF, 18, 17])],
        _ => [r.u8() & 0xFE, r.u8(), r.u8(), r.u8(), r.u8(), r.u8()],
    };
    let len = match r.u8() % 8 {
        0 => r.below(ETH_HDR_LEN as u64 + 1) as usize,
        1 => MAX_FRAME + r.below(600) as usize,
        _ => ETH_HDR_LEN + r.below((MAX_FRAME - ETH_HDR_LEN) as u64 + 1) as usize,
    };
    let mut f = noise(r.u32(), len);
    for (i, b) in dst.iter().chain(src.iter()).enumerate() {
        if i < f.len() {
            f[i] = *b;
        }
    }
    /* What the switch looks at: the protocol, and the source it claims --
     * the guest's own, or not. */
    let kind = match r.u8() % 8 {
        0..=3 => ETH_TYPE_IP,
        4 | 5 => ETH_TYPE_ARP,
        6 => 0x86DD,
        _ => r.u16(),
    };
    if f.len() >= ETH_HDR_LEN {
        wire::set_be16(&mut f, 12, kind);
    }
    let p = from.unwrap_or(0);
    if kind == ETH_TYPE_IP && f.len() >= ETH_HDR_LEN + IP_HDR_LEN {
        let ip = guest_src_ip(r, p);
        wire::set_be32(&mut f, ETH_HDR_LEN + ip::SRC, ip);
    }
    if kind == ETH_TYPE_ARP && f.len() >= ETH_HDR_LEN + ARP_LEN {
        let (mac, ip) = (guest_src_mac(r, p), guest_src_ip(r, p));
        f[ETH_HDR_LEN + arp::SENDER_MAC..ETH_HDR_LEN + arp::SENDER_MAC + 6].copy_from_slice(&mac);
        wire::set_be32(&mut f, ETH_HDR_LEN + arp::SENDER_IP, ip);
    }
    f
}

/// A send of TCP up to 64 KiB from port `p`'s guest, held up by its NIC: to
/// port `q`'s, `hv0` or everyone -- as itself mostly. The send, the segment
/// size it is cut by and how it held up, or None for one that did not.
fn tcp4_send(r: &mut Input, p: usize, q: usize) -> Option<(Vec<u8>, usize, offload::Tcp4)> {
    let (dst_mac, dst_ip) = match r.u8() % 4 {
        0 => (HOST_MAC, HOST_IP),
        1 => ([0xFF; 6], switch::port_ip(q)),
        _ => (switch::port_mac(q), switch::port_ip(q)),
    };
    let src_ip = if r.u8() < 16 { switch::port_ip(q) } else { switch::port_ip(p) };
    let payload = r.pick(&[0usize, 300, 1448, 4000, 20_000]);
    let mss = r.pick(&[256usize, 536, 1448]);
    let mut f = crate::offloadmodel::build(r, 6, 20, 20, payload, 0x0800, 0x4000, 0x18, true);
    f[..6].copy_from_slice(&dst_mac);
    f[6..12].copy_from_slice(&switch::port_mac(p));
    f[ETH_HDR_LEN + 12..ETH_HDR_LEN + 16].copy_from_slice(&src_ip.to_be_bytes());
    f[ETH_HDR_LEN + 16..ETH_HDR_LEN + 20].copy_from_slice(&dst_ip.to_be_bytes());
    let layout = offload::check_tcp4(&f, mss).ok()?;
    Some((f, mss, layout))
}

/// A frame from port `p`'s guest whose checksum it left to finish: to port
/// `q`'s, `hv0` or everyone -- as itself mostly. The frame, where its
/// checksum is summed from and put, and the place checked; None for a place
/// that is not in it.
fn partial_send(r: &mut Input, p: usize, q: usize) -> Option<(Vec<u8>, usize, usize, offload::Partial)> {
    let (dst_mac, dst_ip) = match r.u8() % 4 {
        0 => (HOST_MAC, HOST_IP),
        1 => ([0xFF; 6], switch::port_ip(q)),
        _ => (switch::port_mac(q), switch::port_ip(q)),
    };
    let src_ip = if r.u8() < 16 { switch::port_ip(q) } else { switch::port_ip(p) };
    let proto = r.pick(&[6u8, 17]);
    let payload = r.below(1400) as usize;
    let offset = if proto == 17 { 6 } else { 16 };
    /* Over IPv4, or now and then over IPv6 -- from its own address or
     * another's, now and then made to sum to 0, which UDP's goes as all
     * ones. */
    let (mut f, start) = if r.u8() < 64 {
        let (mut f, l4) = crate::offloadmodel::build6(r, proto, &[], payload);
        let from = if r.u8() < 16 { eui64(&LINK_LOCAL, q) } else { eui64(&LINK_LOCAL, p) };
        f[ETH_HDR_LEN + 8..ETH_HDR_LEN + 24].copy_from_slice(&from);
        f[ETH_HDR_LEN + 24..ETH_HDR_LEN + 40].copy_from_slice(&eui64(&PREFIX6, q));
        (f, l4)
    } else {
        let mut f = crate::offloadmodel::build(r, proto, 20, 20, payload, 0x0800, 0x4000, 0x18, true);
        f[ETH_HDR_LEN + 12..ETH_HDR_LEN + 16].copy_from_slice(&src_ip.to_be_bytes());
        f[ETH_HDR_LEN + 16..ETH_HDR_LEN + 20].copy_from_slice(&dst_ip.to_be_bytes());
        (f, 34)
    };
    f[..6].copy_from_slice(&dst_mac);
    f[6..12].copy_from_slice(&switch::port_mac(p));
    if r.u8() < 32 {
        crate::offloadmodel::sum_to_zero(&mut f, start, start + offset);
    }
    let partial = offload::Partial::check(f.len(), start as u16, offset as u16).ok()?;
    Some((f, start, offset, partial))
}

/// What one guest sends, as `hand_over` has it sent.
enum Sent {
    Frame(Vec<u8>),
    Partial(Vec<u8>, usize, usize, offload::Partial),
    Tcp4(Vec<u8>, usize, offload::Tcp4),
}

/// Port `q` changing hands as the guest on port `s` sends -- a frame, to
/// `q` mostly, one whose checksum it left to finish, a send of TCP: `q`'s
/// VM gone and another on it, on a network or alone, its NIC saying what it
/// takes now and then, just before one of the locks the switch takes for
/// the send, or after it. What the switch looked at without a lock was the
/// VM going's; where it puts the frame, under one, is the VM coming's.
/// Every guest must have what it would have had if the port had changed
/// hands before the send or after it -- the NIC saying what it takes before
/// the send or after, the frame lost, now and then, to the VM going or the
/// VM coming -- and none of them another network's frame.
fn hand_over(r: &mut Input, sw: &Arc<Switch>, m: &mut Model, s: usize, q: usize,
             vms: &mut [Option<Arc<Shared>>], backends: &mut [Option<switch::PortBackend>]) {
    use std::sync::atomic::Ordering::Relaxed;
    let net = r.pick(&[None, Some(0usize), Some(1)]);
    let takes = r.bool().then(|| offload::Takes { partial: r.bool(), whole: r.bool() });
    let sent = match r.u8() % 4 {
        0 | 1 => {
            let mut f = frame(r, Some(s));
            if f.len() >= ETH_HDR_LEN && r.u8() < 160 {
                f[..6].copy_from_slice(&switch::port_mac(q));
            }
            Sent::Frame(f)
        }
        2 => match partial_send(r, s, q) {
            Some((f, start, offset, partial)) => Sent::Partial(f, start, offset, partial),
            None => return,
        },
        _ => match tcp4_send(r, s, q) {
            Some((f, mss, layout)) => Sent::Tcp4(f, mss, layout),
            None => return,
        },
    };
    /* Before which of the switch's locks: the first, for a send of TCP --
     * which may be cut, and a change between two of its frames would be one
     * no order of the change and one send makes. */
    let k = if matches!(sent, Sent::Tcp4(..)) { 0 } else { r.below(4) as u32 };
    let send = |v: &mut Model| match &sent {
        Sent::Frame(f) => v.forward(Some(s), f),
        Sent::Partial(f, start, offset, _) => v.send_partial(s, f, *start, *offset),
        Sent::Tcp4(f, mss, _) => v.send_tcp4(s, f, *mss),
    };
    /* The model's ends -- the change, what the new NIC takes and the send,
     * in each order they may come in, the frame lost to the port's VM or
     * not -- and how many times the VM going was woken. */
    let mut ends: Vec<(&str, Model, u64)> = [
        ("change, takes, send", 0, false), ("change, takes, send lost", 0, true),
        ("change, send, takes", 1, false), ("change, send lost, takes", 1, true),
        ("send, change, takes", 2, false), ("send lost, change, takes", 2, true),
    ]
    .iter()
    .map(|&(name, order, lose)| {
        let mut v = m.clone();
        let mut left = 0;
        if order < 2 {
            left = v.hand_over(q, net);
        }
        if let (0, Some(t)) = (order, takes) {
            v.takes(q, t);
        }
        v.lose = lose.then_some(q);
        send(&mut v);
        v.lose = None;
        if order == 2 {
            left = v.hand_over(q, net);
        }
        if let (1 | 2, Some(t)) = (order, takes) {
            v.takes(q, t);
        }
        (name, v, left)
    })
    .collect();
    /* The switch's: the change made at the lock, by what would make it --
     * the port released, another VM's claim on it, its NIC saying what it
     * takes. */
    let name = net.map(|n| NETS[n]);
    let changed: Arc<std::sync::Mutex<Option<(Result<usize, &'static str>, Arc<Shared>)>>> = Arc::default();
    let change: Box<dyn FnOnce()> = {
        let (sw, changed) = (sw.clone(), changed.clone());
        Box::new(move || {
            sw.release(q);
            let vm = Arc::new(Shared::default());
            let got = sw.claim_at(q, &vm, name);
            if let Some(t) = takes {
                sw.backend(q).takes(t);
            }
            *changed.lock().expect("the change's slot") = Some((got, vm));
        })
    };
    crate::sync::before_lock(k, change);
    if let Some(b) = backends[s].as_mut() {
        match &sent {
            Sent::Frame(f) => b.send(f),
            Sent::Partial(f, _, _, partial) => b.send_partial(&mut f.clone(), *partial),
            Sent::Tcp4(f, _, layout) => b.send_tcp4(f, *layout),
        }
    }
    let at_lock = match crate::sync::take_before_lock() {
        Some(change) => {
            change();
            false
        }
        None => true,
    };
    HANDOVERS[usize::from(at_lock)].fetch_add(1, Relaxed);
    let (got, vm) = changed.lock().expect("the change's slot").take().expect("the port changed hands");
    invariant!(got == Ok(q), "port {} given to a VM as it changed hands: {:?}", q, got);
    invariant!(sw.network_of(q).as_deref() == name, "port {} on {:?} after it changed hands, put on {:?}", q,
               sw.network_of(q), name);
    let went = vms[q].replace(vm.clone());
    let mut came = sw.backend(q);
    let stack = network::take_stack();
    let hv0_in = network::take_hv0_in();
    let (went_woken, came_woken) = (went.as_ref().map(|v| v.woken.load(Relaxed)), vm.woken.load(Relaxed));
    /* What waits for the VM coming, each as its NIC is handed it. */
    let mut waiting = Vec::new();
    let mut buf = vec![0u8; offload::MAX_SEND];
    while let Some((n, rx)) = came.recv_with(&mut buf) {
        let kind = match rx {
            offload::Rx::Done => Kind::Done,
            offload::Rx::Partial(p) => {
                let h = p.header();
                Kind::Partial(usize::from(h.csum_start), usize::from(h.csum_offset))
            }
            offload::Rx::Whole(_) => Kind::Whole,
        };
        waiting.push((buf[..n].to_vec(), kind));
    }
    backends[q] = Some(came);
    let is = |v: &Model, left: u64| {
        (0..MAX_PORTS).all(|p| sw.counts(p) == v.counts(p))
            && sw.host_counts() == (v.to_host, v.refused, v.dhcp, v.foreign)
            && sw.ipv6_counts() == (v.advertised, v.bound.len())
            && stack == v.stack && hv0_in == v.stack
            && went_woken.map_or(true, |w| w == left)
            && came_woken == v.woken[q]
            && v.inbox[q].iter().eq(waiting.iter())
    };
    let Some(at) = ends.iter().position(|(_, v, left)| is(v, *left)) else {
        let said: Vec<String> = ends.iter().map(|(name, v, left)| {
            format!("{}: {:?} and {:?}, {} to hv0, woken {} and {}, waiting {:?}", name, v.counts(s), v.counts(q),
                    v.stack.len(), left, v.woken[q], v.inbox[q].iter().map(|e| (e.0.len(), e.1)).collect::<Vec<_>>())
        }).collect();
        panic!("invariant: port {} changed hands {} as port {} sent: {:?} and {:?}, {} to hv0, the VM going woken \
                {:?} and the VM coming {}, waiting {:?} -- none of the model's ends ({})", q,
               if at_lock { "at a lock" } else { "after" }, s, sw.counts(s), sw.counts(q), stack.len(), went_woken,
               came_woken, waiting.iter().map(|e| (e.0.len(), e.1)).collect::<Vec<_>>(), said.join("; "));
    };
    HANDOVER_ENDS[at].fetch_add(1, Relaxed);
    *m = ends.swap_remove(at).1;
    m.stack.clear();
    m.inbox[q].clear();
}

/// A DHCP client's DISCOVER or REQUEST from port `p`, as udhcpc sends one:
/// from no address yet, or -- renewing -- its own; now and then as another.
fn dhcp_request(r: &mut Input, p: usize) -> Vec<u8> {
    let mut m = vec![0u8; 300];
    m[0] = 1;
    m[1] = 1;
    m[2] = 6;
    m[4..8].copy_from_slice(&r.u32().to_le_bytes());
    m[28..34].copy_from_slice(&switch::port_mac(p));
    m[236..240].copy_from_slice(&0x6382_5363u32.to_be_bytes());
    let kind = r.pick(&[1u8, 3, 3, 7]);
    let mut opts = vec![53, 1, kind];
    if kind == 3 {
        let asked = if r.u8() < 200 { switch::port_ip(p) } else { r.u32() };
        opts.extend_from_slice(&[50, 4]);
        opts.extend_from_slice(&asked.to_be_bytes());
    }
    opts.push(255);
    m[240..240 + opts.len()].copy_from_slice(&opts);
    let src_mac = guest_src_mac(r, p);
    let src_ip = if r.u8() < 192 { 0 } else { guest_src_ip(r, p) };
    let route = udp::Route {
        src_mac, dst_mac: MAC_BROADCAST, src_ip, dst_ip: u32::MAX,
        src_port: 68, dst_port: 67, dont_fragment: false,
    };
    let mut f = vec![0u8; udp::PAYLOAD_AT + m.len()];
    let n = udp::write_frame(&mut f, &route, m.len()).expect("a DHCP request fits a frame");
    f[udp::PAYLOAD_AT..n].copy_from_slice(&m);
    f.truncate(n);
    f
}

/// The switch: guests on ports, and `hv0`, sending frames of every kind at
/// each other; what each port's NIC takes, what the stack is handed and
/// every count, against the model.
pub fn switch_target(r: &mut Input) {
    network::reset();
    let dns = r.pick(&[0u32, 0x0808_0808, 0x0A00_0203]);
    network::dns::UPSTREAM.store(dns, std::sync::atomic::Ordering::Relaxed);
    let sw = Arc::new(Switch::new().expect("a switch"));
    let mut m = Model::new();
    let mut vms: Vec<Option<Arc<Shared>>> = vec![None; MAX_PORTS];
    let mut backends: Vec<Option<switch::PortBackend>> = (0..MAX_PORTS).map(|_| None).collect();
    /* The most VMs this input puts on the switch at once: a few, mostly --
     * a port's first VM has its inbox's 384 KiB zeroed, which on every port
     * of every input was most of what the target spent -- and now and then
     * one more than there are ports. */
    let most = if r.u8() < 24 { MAX_PORTS + 1 } else { 1 + r.below(4) as usize };

    while let Some(op) = r.op(17) {
        match op {
            0 | 10 => {
                /* A VM on the switch, the first free port -- or, now and
                 * then, VMs until there is none. */
                for _ in 0..if op == 10 && r.u8() < 64 { MAX_PORTS + 1 } else { 1 } {
                if m.owned.iter().filter(|o| **o).count() >= most {
                    break;
                }
                let vm = Arc::new(Shared::default());
                /* On a network, or alone. */
                let net = r.pick(&[None, None, Some(0usize), Some(1)]);
                let name = net.map(|n| NETS[n]);
                /* The first free port -- or, now and then, the port a VM
                 * asks for (`port=`): that one when it is free, and no other,
                 * one past the last among those asked for. */
                let (got, want) = if r.u8() < 64 {
                    let q = r.below(MAX_PORTS as u64 + 1) as usize;
                    (sw.claim_at(q, &vm, name), (q < MAX_PORTS && !m.owned[q]).then_some(q))
                } else {
                    (sw.claim(&vm, name), (0..MAX_PORTS).find(|&p| !m.owned[p]))
                };
                match (got, want) {
                    (Ok(p), Some(q)) => {
                        invariant!(p == q, "a VM given port {}, port {} being the one it should have", p, q);
                        invariant!(sw.network_of(p).as_deref() == name, "port {} on {:?}, put on {:?}", p,
                                   sw.network_of(p), name);
                        m.claim(p, net);
                        vms[p] = Some(vm);
                        backends[p] = Some(sw.backend(p));
                    }
                    (Err(_), None) => {}
                    (got, want) => panic!("invariant: a claim came to {:?} with port {:?} free", got, want),
                }
                }
            }
            1 => {
                let p = r.below(MAX_PORTS as u64) as usize;
                sw.release(p);
                let woken = m.release(p);
                if let Some(vm) = vms[p].take() {
                    invariant!(vm.woken.load(std::sync::atomic::Ordering::Relaxed) == woken,
                               "port {}'s VM woken {} times for {} frames", p,
                               vm.woken.load(std::sync::atomic::Ordering::Relaxed), woken);
                }
            }
            2..=4 => {
                /* A guest's frame, from a port that has a NIC on it -- or a
                 * burst of it, past its share of hv0's backlog and past its
                 * rate to every port. */
                let p = r.below(MAX_PORTS as u64) as usize;
                if backends[p].is_some() {
                    let f = frame(r, Some(p));
                    let n = if r.u8() < 16 { 1 + r.below(300) } else { 1 };
                    for _ in 0..n {
                        m.forward(Some(p), &f);
                        if let Some(b) = backends[p].as_mut() {
                            b.send(&f);
                        }
                    }
                }
            }
            5 => {
                /* The host's frame, out of `hv0` -- or a burst of them, past
                 * what an inbox holds. */
                let n = if r.u8() < 16 { INBOX_FRAMES + r.below(40) as usize } else { 1 };
                let f = frame(r, None);
                for _ in 0..n {
                    m.forward(None, &f);
                    if let Some(sink) = vnic::sink() {
                        sink.on_frame(&f);
                    }
                }
            }
            6 | 7 => {
                /* A NIC taking what waits for it, into a buffer of any size:
                 * a frame as `recv` takes one, or -- a NIC that takes sends
                 * whole -- with how it is cut, `recv_whole`'s. */
                let p = r.below(MAX_PORTS as u64) as usize;
                if let Some(b) = backends[p].as_mut() {
                    let whole_buf = r.u8() < 64;
                    let room = if whole_buf { offload::MAX_SEND } else { r.pick(&[MAX_FRAME, 60, 2048, 14, 0]) };
                    let mut buf = vec![0u8; room];
                    let got = if whole_buf {
                        b.recv_with(&mut buf)
                    } else {
                        b.recv(&mut buf).map(|n| (n, offload::Rx::Done))
                    };
                    let want = m.take(p, room).map(|(mut w, kind)| {
                        /* `recv` finishes a checksum left to; `recv_with`
                         * hands it on so. */
                        if let (Kind::Partial(start, offset), false) = (kind, whole_buf) {
                            crate::offloadmodel::finish(&mut w, start, offset);
                        }
                        (w, kind)
                    });
                    match (got, want) {
                        (None, None) => {}
                        (Some((n, rx)), Some((w, kind))) => {
                            let expect = w.len().min(buf.len());
                            invariant!(n == expect && buf[..n] == w[..n],
                                       "port {} took {} bytes, {} expected, of a frame of {}", p, n, expect, w.len());
                            if whole_buf {
                                let same = match (rx, kind) {
                                    (offload::Rx::Done, Kind::Done) | (offload::Rx::Whole(_), Kind::Whole) => true,
                                    (offload::Rx::Partial(q), Kind::Partial(start, offset)) => {
                                        let h = q.header();
                                        usize::from(h.csum_start) == start && usize::from(h.csum_offset) == offset
                                    }
                                    _ => false,
                                };
                                invariant!(same, "port {} took a frame as {:?}, the model {:?}", p, rx, kind);
                            }
                        }
                        (got, want) => panic!("invariant: port {} took {:?}, the model has {:?} bytes waiting",
                                              p, got.map(|g| g.0), want.map(|w| w.0.len())),
                    }
                }
            }
            12 => {
                /* A guest's send of TCP up to 64 KiB, held up by its NIC: to
                 * another port, `hv0`, everyone -- as itself mostly. */
                let p = r.below(MAX_PORTS as u64) as usize;
                if backends[p].is_some() {
                    let q = r.below(MAX_PORTS as u64) as usize;
                    if let Some((f, mss, layout)) = tcp4_send(r, p, q) {
                        m.send_tcp4(p, &f, mss);
                        if let Some(b) = backends[p].as_mut() {
                            b.send_tcp4(&f, layout);
                        }
                    }
                }
            }
            13 => {
                /* A port's NIC taking checksums partial and sends whole, or
                 * not. */
                let p = r.below(MAX_PORTS as u64) as usize;
                if let Some(b) = backends[p].as_mut() {
                    let takes = offload::Takes { partial: r.bool(), whole: r.bool() };
                    b.takes(takes);
                    m.takes_partial[p] = takes.partial;
                    m.takes[p] = takes.whole;
                }
            }
            14 => {
                /* A guest's frame whose checksum it left to finish: to
                 * another port, `hv0`, everyone -- as itself mostly. */
                let p = r.below(MAX_PORTS as u64) as usize;
                if backends[p].is_some() {
                    let q = r.below(MAX_PORTS as u64) as usize;
                    if let Some((f, start, offset, partial)) = partial_send(r, p, q) {
                        m.send_partial(p, &f, start, offset);
                        if let Some(b) = backends[p].as_mut() {
                            b.send_partial(&mut f.clone(), partial);
                        }
                    }
                }
            }
            8 => {
                /* The kernel's DNS server asked for again; the stack
                 * refusing frames, or not; a way out asked for, which with
                 * no uplink is none. */
                let dns = r.pick(&[0u32, 0x0808_0808, 0x0A00_0203]);
                network::dns::UPSTREAM.store(dns, std::sync::atomic::Ordering::Relaxed);
                invariant!(sw.dns() == (dns != 0).then_some(dns), "the switch told another DNS server");
                m.dns = dns;
                m.refuse = r.u8() < 64;
                vnic::set_refuse(m.refuse);
                invariant!(sw.way_out() == Err(crate::net::NatError::NoUplink), "a way out with no uplink");
                invariant!(sw.nat_address().is_none(), "NAT on with no way out");
                /* What the module makes of a port for its guest's command line. */
                let p = r.below(MAX_PORTS as u64) as usize;
                let param = switch::ip_param(p, sw.dns());
                invariant!(param.starts_with("ip=10.0.100."), "a port's ip= of {}", param);
                let _ = (switch::nat_why(crate::net::NatError::Busy), sw.host_nic().map(|n| n.ip()));
            }
            15 => {
                /* A guest with many addresses, as privacy extensions give
                 * one: an echo from each of up to a dozen of the prefix's,
                 * past what a port may have bound -- its oldest let go --
                 * some of them now and then another port's. */
                let p = r.below(MAX_PORTS as u64) as usize;
                if backends[p].is_some() {
                    let (n, base, q) = (1 + r.below(12) as usize, r.u8() & 3, r.below(MAX_PORTS as u64) as usize);
                    for i in 0..n {
                        let mut a = [0u8; 16];
                        a[..8].copy_from_slice(&PREFIX6);
                        a[14] = base;
                        a[15] = i as u8;
                        let f = echo6(p, &a, q);
                        m.forward(Some(p), &f);
                        if let Some(b) = backends[p].as_mut() {
                            b.send(&f);
                        }
                    }
                }
            }
            16 => {
                /* A port changing hands as a guest on another sends: its VM
                 * gone and another on it, on a network or alone -- one more
                 * VM, where the port was free. */
                let s = r.below(MAX_PORTS as u64) as usize;
                let q = r.below(MAX_PORTS as u64) as usize;
                let on = m.owned.iter().filter(|o| **o).count();
                if m.owned[s] && backends[s].is_some() && q != s && (m.owned[q] || on < most) {
                    hand_over(r, &sw, &mut m, s, q, &mut vms, &mut backends);
                }
            }
            9 => {
                /* Time going by: a port's rate to every port earned back. */
                time::advance(r.pick(&[NS_PER_SEC / 1000, NS_PER_SEC / 10, NS_PER_SEC, 10 * NS_PER_SEC]));
            }
            11 => {
                /* A port's NIC holding its receive interrupts, or done: a
                 * frame for it then wakes nobody while the inbox is under
                 * half full. Open-ended or none, so that the model and the
                 * switch, reading the clock a step apart, never disagree on
                 * a window's end -- its end is the NIC target's. */
                let p = r.below(MAX_PORTS as u64) as usize;
                if let Some(b) = backends[p].as_mut() {
                    let quiet = r.bool();
                    b.quiet_until(if quiet { u64::MAX / 2 } else { 0 });
                    m.quiet[p] = quiet;
                }
            }
            _ => {}
        }
        /* The counts, and what the stack was handed, as the model has them. */
        for p in 0..MAX_PORTS {
            invariant!(sw.counts(p) == m.counts(p), "port {} lost {:?}, the model {:?}", p, sw.counts(p),
                       m.counts(p));
        }
        let counts = sw.host_counts();
        let want_counts = (m.to_host, m.refused, m.dhcp, m.foreign);
        invariant!(counts == want_counts, "host counts {:?}, the model {:?}", counts, want_counts);
        let six = sw.ipv6_counts();
        invariant!(six == (m.advertised, m.bound.len()), "{} router advertisements and {} addresses bound, the \
                   model {} and {}", six.0, six.1, m.advertised, m.bound.len());
        let stack = network::take_stack();
        let hv0_in = network::take_hv0_in();
        /* A pass of the stack's over hv0's backlog: every port's share
         * counted afresh. */
        m.on_backlog = [0; MAX_PORTS];
        let want = core::mem::take(&mut m.stack);
        invariant!(stack == want && hv0_in == want, "the stack was handed {} frames, hv0 {}, the model {}",
                   stack.len(), hv0_in.len(), want.len());
    }
    /* Every NIC takes what is still waiting for it: all of it, in order. */
    for p in 0..MAX_PORTS {
        let Some(b) = backends[p].as_mut() else { continue };
        let mut buf = vec![0u8; MAX_FRAME];
        loop {
            let got = b.recv(&mut buf);
            let want = m.take(p, MAX_FRAME).map(|(mut w, kind)| {
                if let Kind::Partial(start, offset) = kind {
                    crate::offloadmodel::finish(&mut w, start, offset);
                }
                (w, kind)
            });
            match (got, want) {
                (None, None) => break,
                (Some(n), Some((w, _))) => invariant!(buf[..n] == w[..], "port {} took another frame than was sent", p),
                (got, want) => panic!("invariant: port {} drained {:?}, the model {:?} bytes", p, got,
                                      want.map(|w| w.0.len())),
            }
        }
    }
}

/* ---- NAT: the guests' packets out, the world's answers back ---- */

use std::collections::BTreeMap;

use netwire::{icmp, ip, tcp, ETH_TYPE_IP, ICMP_HDR_LEN, IP_HDR_LEN, IP_PROTO_ICMP, IP_PROTO_TCP, IP_PROTO_UDP,
              UDP_HDR_LEN};

use crate::consts::NS_PER_SEC;
use crate::device::{DEVICES, ETH0, HV0};
use crate::wire;
use crate::nat::{self, COUNTERS, PORT_BASE, PORT_LAST};
use crate::time;

const SUBNET: u32 = 0x0A00_6400;
const ETH0_MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x56];
const GW_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0, 2, 2];
/// Where the guests' flows go: a few places, so that flows repeat.
const REMOTES: [u32; 5] = [0x5DB8_D822, 0x0101_0101, 0x0808_0808, 0x8EFA_0001, 0xC633_6407];
/// The shortest a mapping outlives its flow's last packet (a TCP reset's):
/// one seen within it is live, whatever else the table has done.
const LIVE_NS: u64 = 10 * NS_PER_SEC;

fn guest_mac(ip: u32) -> [u8; 6] {
    [0x02, 0, 0, 0, 0x64, ip as u8]
}

/// An IPv4 packet: a header of `ihl` bytes (options, NOPs, to its length),
/// `total` its total length -- the true one or a lie -- its checksum right
/// unless `bad_sum`; `l4` after it.
#[allow(clippy::too_many_arguments)]
fn ipv4(ihl: usize, id: u16, frag: u16, ttl: u8, proto: u8, src: u32, dst: u32, l4: &[u8], total: usize,
        bad_sum: bool) -> Vec<u8> {
    let mut p = vec![1u8; ihl];
    p[ip::VERSION_IHL] = 0x40 | (ihl / 4) as u8;
    p[ip::TOS] = 0;
    wire::set_be16(&mut p, ip::TOTAL_LEN, total as u16);
    wire::set_be16(&mut p, ip::ID, id);
    wire::set_be16(&mut p, ip::FRAG_OFF, frag);
    p[ip::TTL] = ttl;
    p[ip::PROTOCOL] = proto;
    wire::set_be16(&mut p, ip::CHECKSUM, 0);
    wire::set_be32(&mut p, ip::SRC, src);
    wire::set_be32(&mut p, ip::DST, dst);
    let sum = wire::checksum(&p);
    wire::set_be16(&mut p, ip::CHECKSUM, if bad_sum { sum ^ 0x5555 } else { sum });
    p.extend_from_slice(l4);
    p
}

/// How a transport's checksum is sent: right, none (UDP's 0), or wrong.
#[derive(Clone, Copy, PartialEq)]
enum Sum {
    Right,
    Zero,
    Wrong,
}

fn sum_kind(r: &mut Input, proto: u8) -> Sum {
    match r.u8() % 16 {
        0 => Sum::Wrong,
        1 | 2 if proto == IP_PROTO_UDP => Sum::Zero,
        _ => Sum::Right,
    }
}

/// A TCP segment, UDP datagram or ICMP message of `payload` bytes, from
/// `src`:`sport` to `dst`:`dport` (an ICMP message's identifier `sport`).
#[allow(clippy::too_many_arguments)]
fn transport(r: &mut Input, proto: u8, src: u32, dst: u32, sport: u16, dport: u16, payload: usize, sum: Sum,
             tcp_flags: u8, icmp_kind: u8) -> Vec<u8> {
    let fill = |r: &mut Input, b: &mut [u8]| {
        let n = b.len();
        b.copy_from_slice(&noise(r.u32(), n));
    };
    let mut l4 = match proto {
        IP_PROTO_TCP => {
            let hlen = tcp::HDR_LEN + 4 * r.below(4) as usize;
            let mut seg = vec![tcp::OPT_NOP; hlen + payload];
            tcp::write(&mut seg, sport, dport, r.u32(), r.u32(), hlen, tcp_flags, r.u16());
            fill(r, &mut seg[hlen..]);
            let s = tcp::checksum(src, dst, &seg);
            wire::set_be16(&mut seg, tcp::CHECKSUM, s);
            seg
        }
        IP_PROTO_UDP => {
            let mut d = vec![0u8; UDP_HDR_LEN + payload];
            udp::write(&mut d, sport, dport, payload);
            fill(r, &mut d[UDP_HDR_LEN..]);
            let s = if sum == Sum::Zero { 0 } else { udp::checksum(src, dst, &d) };
            wire::set_be16(&mut d, udp::CHECKSUM, s);
            d
        }
        IP_PROTO_ICMP => {
            let mut m = vec![0u8; ICMP_HDR_LEN + payload];
            fill(r, &mut m[ICMP_HDR_LEN..]);
            let n = m.len();
            icmp::write(&mut m, icmp_kind, 0, sport, r.u16(), n);
            m
        }
        _ => {
            let mut b = vec![0u8; 8 + payload];
            fill(r, &mut b);
            b
        }
    };
    if sum == Sum::Wrong {
        let at = match proto {
            IP_PROTO_TCP => tcp::CHECKSUM,
            IP_PROTO_UDP => udp::CHECKSUM,
            _ => icmp::CHECKSUM,
        };
        if l4.len() >= at + 2 {
            let v = wire::be16(&l4, at) ^ 0x1234;
            wire::set_be16(&mut l4, at, if v == 0 { 1 } else { v });
        }
    }
    l4
}

fn ethernet(dst: [u8; 6], src: [u8; 6], packet: &[u8]) -> Vec<u8> {
    let mut f = vec![0u8; ETH_HDR_LEN];
    f[..6].copy_from_slice(&dst);
    f[6..12].copy_from_slice(&src);
    wire::set_be16(&mut f, 12, ETH_TYPE_IP);
    f.extend_from_slice(packet);
    f
}

/// Where a transport's port -- or an echo's identifier -- and checksum are.
fn fields(proto: u8) -> (usize, usize, usize) {
    match proto {
        IP_PROTO_TCP => (tcp::SRC_PORT, tcp::DST_PORT, tcp::CHECKSUM),
        IP_PROTO_UDP => (udp::SRC_PORT, udp::DST_PORT, udp::CHECKSUM),
        _ => (icmp::ID, icmp::ID, icmp::CHECKSUM),
    }
}

/// Whether a transport's checksum holds, over its pseudo-header too; None
/// for a UDP datagram that carries none.
fn l4_sum_holds(proto: u8, src: u32, dst: u32, l4: &[u8]) -> Option<bool> {
    match proto {
        IP_PROTO_UDP if wire::be16(l4, udp::CHECKSUM) == 0 => None,
        IP_PROTO_TCP | IP_PROTO_UDP => Some(wire::transport_checksum(proto, src, dst, l4) == 0),
        _ => Some(wire::checksum(l4) == 0),
    }
}

/// A flow's key, as NAT maps it: protocol, the guest's end, the far end.
type Key = (u8, u32, u16, u32, u16);

struct Flow {
    key: Key,
    mac: [u8; 6],
    /// When a packet of it last went through, either way.
    last: u64,
}

/// What NAT has done, as far as it can be seen from outside: which port
/// each flow went out on, when each was last seen, what went out. Ordered
/// maps, whose order is the same in every process: a flow is chosen by it,
/// and an input has to choose the same one when it is replayed.
#[derive(Default)]
struct NatModel {
    by_port: BTreeMap<u16, Flow>,
    by_key: BTreeMap<Key, u16>,
    /// The ports of the flows seen last, the latest at the back: what the
    /// world's answers mostly go to.
    recent: VecDeque<u16>,
    /// Packets that went out, for the world's ICMP errors to quote.
    sent: Vec<Vec<u8>>,
    /// Flows mapped for a packet that then did not go out -- no next hop,
    /// no frame -- and the guest's MAC: the table has them, and an answer
    /// on the port is let in.
    unsent: BTreeMap<Key, [u8; 6]>,
    /// The networks `nat allow` let through, as (network, mask): NAT's word
    /// outlives NAT going off and on, and so does this.
    allowed: Vec<(u32, u32)>,
    /// Packets that met a full table wanting a new mapping, this input: one
    /// in `SHARE_SAMPLE` is judged by its share (`check_share`), and no
    /// more than `SHARE_JUDGED`; and whether a flood filled the table yet.
    full_seen: usize,
    judged: usize,
    flooded: bool,
}

impl NatModel {
    /// The model of a NAT gone off and on again: a new table, the same
    /// word on where the guests may go.
    fn fresh(&mut self) {
        let allowed = core::mem::take(&mut self.allowed);
        *self = NatModel { allowed, ..NatModel::default() };
    }
}

/// The networks a guest is kept from, as (network, mask) -- the fuzzer's own
/// list, not `nat.rs`'s: link-local, where a cloud's metadata server is, the
/// private networks, and the shared space.
const KEPT_NETS: [(u32, u32); 5] = [(0xA9FE_0000, 0xFFFF_0000), (0x0A00_0000, 0xFF00_0000),
                                    (0xAC10_0000, 0xFFF0_0000), (0xC0A8_0000, 0xFFFF_0000),
                                    (0x6440_0000, 0xFFC0_0000)];
/// Where a guest's packets for those go in an input: the metadata server,
/// a host of each private network and of the shared space.
const KEPT_HOSTS: [u32; 5] = [0xA9FE_A9FE, 0x0A01_0203, 0xAC1F_0001, 0xC0A8_0164, 0x647F_FFFE];

/// Whether NAT must keep a guest's packet of `proto` for `dst`, at TCP's or
/// UDP's port `port`, from the world, by the model: a kept network, or the
/// uplink's own subnet -- but a network `allow` let through, and the DNS
/// server the guests are given at port 53, by UDP or by TCP.
fn kept(m: &NatModel, proto: u8, dst: u32, port: u16) -> bool {
    let dns = network::dns::upstream();
    if dns != 0 && dst == dns && port == 53 && (proto == IP_PROTO_UDP || proto == IP_PROTO_TCP) {
        return false;
    }
    let (ip, mask) = (ETH0.ip(), ETH0.mask());
    let denied = KEPT_NETS.iter().any(|&(n, k)| dst & k == n) || (mask != 0 && dst & mask == ip & mask);
    denied && !m.allowed.iter().any(|&(n, k)| dst & k == n)
}

/// What a guest's IPv4 frame is for, as NAT reads it: its protocol, where
/// it goes, and TCP's or UDP's destination port -- 0 for anything else, or
/// a transport header too short to have one.
fn target_of(frame: &[u8]) -> (u8, u32, u16) {
    let p = &frame[ETH_HDR_LEN..];
    let ihl = ip::header_len(p);
    let len = usize::from(ip::total_len(p)).min(p.len());
    let proto = ip::protocol(p);
    let l4 = if ihl <= len { &p[ihl..len] } else { &p[..0] };
    let port = match proto {
        IP_PROTO_TCP if l4.len() >= tcp::HDR_LEN => wire::be16(l4, tcp::DST_PORT),
        IP_PROTO_UDP if l4.len() >= UDP_HDR_LEN => wire::be16(l4, udp::DST_PORT),
        _ => 0,
    };
    (proto, ip::dst(p), port)
}

/// `nat allow` or `nat deny` of a network -- a kept one mostly, now and then
/// anything, everything (0.0.0.0/0), or no network at all -- through the
/// shell command a person types, and the model with it; then the networks
/// NAT lets through held to the model's.
fn allow_op(r: &mut Input, m: &mut NatModel, nat: &nat::Nat) {
    let (net, bits) = match r.u8() % 8 {
        0..=3 => {
            let (n, k) = r.pick(&KEPT_NETS);
            (n, k.count_ones())
        }
        4 => (r.pick(&KEPT_HOSTS), 32),
        5 => (ETH0.ip(), r.pick(&[32u32, 24, 16])),
        6 => (r.u32(), r.below(33) as u32),
        _ => (0, 0),
    };
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    let allow = r.u8() < 160;
    let garbage = r.u8() < 16;
    let args = if garbage {
        r.pick(&["allow", "allow 1.2.3", "deny 1.2.3.4/33", "allow 1.2.3.4 5.6.7.8", "frob 1.2.3.4", "allow /8"])
            .to_string()
    } else {
        format!("{} {}/{}", if allow { "allow" } else { "deny" }, wire::Ipv4(net), bits)
    };
    let mut out = crate::cmd::Output::default();
    nat::shell(&args, &mut out);
    if !garbage {
        let key = (net & mask, mask);
        if !allow {
            m.allowed.retain(|x| *x != key);
        } else if !m.allowed.contains(&key) && m.allowed.len() < nat::ALLOWED_MAX {
            m.allowed.push(key);
        }
    }
    let mut nets = [(0, 0); nat::ALLOWED_MAX];
    let n = nat.allowed(&mut nets);
    let mut got = nets[..n].to_vec();
    let mut want = m.allowed.clone();
    got.sort_unstable();
    want.sort_unstable();
    invariant!(got == want, "nat {:?}: NAT lets {:?} through, the model {:?} ({:?})", args, got, want, out.0);
}

/// What NAT answered a guest with for a packet it denied: to the guest's MAC
/// and address from `hv0`'s, a destination unreachable, communication
/// administratively prohibited, every sum right, quoting the packet's header
/// and the eight bytes after it.
fn check_reject(frame: &[u8], back: &[u8]) {
    REJECTS_CHECKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    const ADMIN_PROHIBITED: u8 = 13;
    let p = &frame[ETH_HDR_LEN..];
    let ihl = ip::header_len(p);
    let quoted = (ihl + 8).min(usize::from(ip::total_len(p)));
    invariant!(back.len() == ETH_HDR_LEN + IP_HDR_LEN + ICMP_HDR_LEN + quoted,
               "a reject of {} bytes for a packet quoting {}", back.len(), quoted);
    invariant!(back[..6] == frame[6..12] && back[6..12] == HOST_MAC && wire::be16(back, 12) == ETH_TYPE_IP,
               "a reject sent to {:02x?} from {:02x?}", &back[..6], &back[6..12]);
    let o = &back[ETH_HDR_LEN..];
    invariant!(ip::protocol(o) == IP_PROTO_ICMP && ip::src(o) == HOST_IP && ip::dst(o) == ip::src(p)
               && wire::checksum(&o[..IP_HDR_LEN]) == 0, "a reject's header wrong");
    let m = &o[IP_HDR_LEN..];
    invariant!(icmp::kind(m) == icmp::DEST_UNREACH && icmp::code(m) == ADMIN_PROHIBITED && wire::checksum(m) == 0,
               "a reject of type {} code {}, or its sum wrong", icmp::kind(m), icmp::code(m));
    invariant!(m[ICMP_HDR_LEN..] == p[..quoted], "a reject quoting other bytes than the packet's");
}

/// The flow NAT's table has on `port`, live, as `nat` lists it.
fn table_flow(port: u16) -> Option<Key> {
    const NONE: nat::Mapping = nat::Mapping { proto: 0, inner_ip: 0, inner_port: 0, remote_ip: 0, remote_port: 0,
                                              port: 0, left_ns: 0 };
    thread_local! {
        /// Room for the whole table, taken once.
        static ALL: std::cell::RefCell<Vec<nat::Mapping>> = std::cell::RefCell::new(vec![NONE; nat::ENTRIES]);
    }
    let nat = crate::abi::nat()?;
    ALL.with_borrow_mut(|all| {
        let (_, _, _, n) = nat.state(all)?;
        all[..n].iter().find(|m| m.port == port)
            .map(|m| (m.proto, m.inner_ip, m.inner_port, m.remote_ip, m.remote_port))
    })
}

/// Whether NAT's table has a live mapping of `inner_ip`'s, as `nat` lists
/// them.
fn table_maps(inner_ip: u32) -> bool {
    const NONE: nat::Mapping = nat::Mapping { proto: 0, inner_ip: 0, inner_port: 0, remote_ip: 0, remote_port: 0,
                                              port: 0, left_ns: 0 };
    thread_local! {
        static ALL: std::cell::RefCell<Vec<nat::Mapping>> = std::cell::RefCell::new(vec![NONE; nat::ENTRIES]);
    }
    let Some(nat) = crate::abi::nat() else { return false };
    ALL.with_borrow_mut(|all| {
        nat.state(all).is_some_and(|(_, _, _, n)| all[..n].iter().any(|m| m.inner_ip == inner_ip))
    })
}

/// The inner hosts NAT's table keeps a share for at once -- `nat.rs`'s
/// `HOSTS`, which the model has to know to judge a packet given no room.
const NAT_HOSTS: usize = 64;

/// NAT's table, whole, as `nat` lists it: how many live mappings each host
/// holds, and whose each port is (0 for none), by its place from
/// `PORT_BASE`.
struct Shares {
    counts: Vec<(u32, usize)>,
    owners: Vec<u32>,
}

impl Shares {
    fn count(&self, ip: u32) -> usize {
        self.counts.iter().find(|&&(h, _)| h == ip).map_or(0, |&(_, n)| n)
    }
}

fn table_shares() -> Shares {
    const NONE: nat::Mapping = nat::Mapping { proto: 0, inner_ip: 0, inner_port: 0, remote_ip: 0, remote_port: 0,
                                              port: 0, left_ns: 0 };
    thread_local! {
        static ALL: std::cell::RefCell<Vec<nat::Mapping>> = std::cell::RefCell::new(vec![NONE; nat::ENTRIES]);
    }
    let mut shares = Shares { counts: Vec::new(), owners: vec![0; nat::ENTRIES] };
    if let Some(nat) = crate::abi::nat() {
        ALL.with_borrow_mut(|all| {
            if let Some((_, _, _, n)) = nat.state(all) {
                for m in &all[..n] {
                    match shares.counts.iter_mut().find(|(h, _)| *h == m.inner_ip) {
                        Some((_, c)) => *c += 1,
                        None => shares.counts.push((m.inner_ip, 1)),
                    }
                    shares.owners[usize::from(m.port - PORT_BASE)] = m.inner_ip;
                }
            }
        });
    }
    shares
}

/// The port NAT's table has `key`'s flow on, live: one listing of it.
fn table_port(key: Key) -> Option<u16> {
    const NONE: nat::Mapping = nat::Mapping { proto: 0, inner_ip: 0, inner_port: 0, remote_ip: 0, remote_port: 0,
                                              port: 0, left_ns: 0 };
    thread_local! {
        static ALL: std::cell::RefCell<Vec<nat::Mapping>> = std::cell::RefCell::new(vec![NONE; nat::ENTRIES]);
    }
    let nat = crate::abi::nat()?;
    ALL.with_borrow_mut(|all| {
        let (_, _, _, n) = nat.state(all)?;
        all[..n].iter()
            .find(|m| (m.proto, m.inner_ip, m.inner_port, m.remote_ip, m.remote_port) == key)
            .map(|m| m.port)
    })
}

/// What a full table held before a guest's packet that may need a new
/// mapping -- one of a flow the model knows no live mapping of: its hosts'
/// shares and whose each port was. None for a table not full, and for a
/// packet of a flow the model has mapped, which the table would not take a
/// flow for: listing a full table for every packet was most of what the
/// target spent.
fn shares_if_full(frame: &[u8], m: &mut NatModel) -> Option<Shares> {
    if try_packet_key(frame).is_some_and(|key| m.by_key.contains_key(&key)) {
        return None;
    }
    let nat = crate::abi::nat()?;
    let (_, _, active, _) = nat.state(&mut [])?;
    if active != nat::ENTRIES {
        return None;
    }
    m.full_seen += 1;
    if m.full_seen % SHARE_SAMPLE != 1 || m.judged == SHARE_JUDGED {
        return None;
    }
    m.judged += 1;
    Some(table_shares())
}

/// One in this many packets that meet a full table wanting a new mapping is
/// judged by the share: each judging lists the whole table, and a flood met
/// a full one hundreds of thousands of times a gate. Counted per input, so
/// that a finding comes again from its seed and iteration.
const SHARE_SAMPLE: usize = 8;
const SHARE_JUDGED: usize = 64;

/// The port NAT sent a guest's packet out from: its source port, or its
/// echo's identifier.
fn out_port(out: &[u8]) -> u16 {
    let o = &out[ETH_HDR_LEN..];
    let ihl = ip::header_len(o);
    let (s, _, _) = fields(ip::protocol(o));
    wire::be16(&o[ihl..], s)
}

/// The flow a packet is of, if it is whole enough to say: an IPv4 header
/// and the transport's ports, or an echo's identifier.
fn try_packet_key(frame: &[u8]) -> Option<Key> {
    let p = frame.get(ETH_HDR_LEN..)?;
    if p.len() < IP_HDR_LEN {
        return None;
    }
    let ihl = ip::header_len(p);
    let (s, d, _) = fields(ip::protocol(p));
    if p.len() < ihl + s.max(d) + 2 {
        return None;
    }
    Some(packet_key(frame))
}

/// The flow a guest's packet is of: its protocol, source, source port,
/// destination and destination port -- 0 for ICMP's.
fn packet_key(frame: &[u8]) -> Key {
    let p = &frame[ETH_HDR_LEN..];
    let ihl = ip::header_len(p);
    let (s, d, _) = fields(ip::protocol(p));
    let l4 = &p[ihl..];
    let remote_port = if ip::protocol(p) == IP_PROTO_ICMP { 0 } else { wire::be16(l4, d) };
    (ip::protocol(p), ip::src(p), wire::be16(l4, s), ip::dst(p), remote_port)
}

/// A guest's packet given a mapping by taking a live flow's, or given no
/// room, held to the share a full table keeps: the flow taken is one of the
/// host holding the most -- the asker's own when it holds as many as any,
/// of several holding the most the lowest address's -- and a packet finds
/// no room only from a host past the `NAT_HOSTS` that hold mappings. The
/// flow taken is gone from the model.
fn check_share(frame: &[u8], before: [usize; 9], after: [usize; 9], shares: Option<Shares>, sent: Option<u16>,
               m: &mut NatModel) {
    if after[3] == before[3] && after[8] == before[8] {
        return;
    }
    /* A packet NAT made a mapping for, or wanted to: whole enough for its
     * ports to be read. */
    let key = packet_key(frame);
    let asker = key.1;
    if after[3] > before[3] {
        let now = table_shares();
        invariant!(now.counts.len() >= NAT_HOSTS && now.count(asker) == 0,
                   "{}'s packet found no room, {} hosts holding mappings ({} of them its own)", wire::Ipv4(asker),
                   now.counts.len(), now.count(asker));
    }
    if after[8] == before[8] {
        return;
    }
    /* Its port: the one it went out from -- or, not sent, the table's. */
    let Some(port) = sent.or_else(|| table_port(key)) else {
        panic!("invariant: a live flow taken for {:?}, which has no mapping after", key)
    };
    /* Taken for a flow the model had a mapping of: gone from the table
     * without the model seeing it go -- run out, or taken for another --
     * and the table's word is all there is. Forgotten, not judged. */
    let Some(before) = shares else {
        forget_port(m, port);
        return;
    };
    let victim = before.owners[usize::from(port - PORT_BASE)];
    invariant!(victim != 0, "port {} given to {:?} for a live flow's, and none was on it", port, key);
    let mine = before.count(asker);
    let most = before.counts.iter().map(|&(_, n)| n).max().unwrap_or(0);
    let want = if mine >= most {
        asker
    } else {
        before.counts.iter().filter(|&&(_, n)| n == most).map(|&(ip, _)| ip).min().unwrap_or(0)
    };
    invariant!(victim == want, "{}'s new flow took a flow of {}, which holds {}; {} holds {} and the most is {}: \
                                the flow was {}'s to give", wire::Ipv4(asker), wire::Ipv4(victim),
               before.count(victim), wire::Ipv4(asker), mine, most, wire::Ipv4(want));
    TAKEN_CHECKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    forget_port(m, port);
}

/// The flow the model had on `port`, gone: a full table took it.
fn forget_port(m: &mut NatModel, port: u16) {
    if let Some(f) = m.by_port.remove(&port) {
        if m.by_key.get(&f.key) == Some(&port) {
            m.by_key.remove(&f.key);
        }
    }
    m.recent.retain(|&p| p != port);
}

/// Packets the oracle judged: sent out, sent back -- for `HV_FUZZ_STATS`.
pub static OUT_CHECKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Packets NAT denied, checked; and the rejects their guests were answered with.
pub static DENIED_CHECKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static REJECTS_CHECKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static BACK_CHECKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MUST_LET_IN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static ANSWERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static NO_NAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Flows a full table took from a host for another's, each checked.
pub static TAKEN_CHECKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn report() {
    use std::sync::atomic::Ordering::Relaxed;
    println!("  switch: {} sends of TCP carried whole, {} cut", WHOLE_SENDS.load(Relaxed), CUT_SENDS.load(Relaxed));
    let ends: Vec<u64> = HANDOVER_ENDS.iter().map(|n| n.load(Relaxed)).collect();
    println!("  switch: ports changed hands at a lock of a send {} times, after it {}; ended as the change, what \
              the NIC takes and the send {} ({} lost), the change, the send and what it takes {} ({} lost), the \
              send, the change and what it takes {} ({} lost)", HANDOVERS[1].load(Relaxed),
             HANDOVERS[0].load(Relaxed), ends[0], ends[1], ends[2], ends[3], ends[4], ends[5]);
    let six: Vec<u64> = ADMIT6.iter().map(|n| n.load(Relaxed)).collect();
    println!("  switch: IPv6 frames {} carried, {} solicitations answered, {} spoofed, {} hiding what they carry \
              or of no protocol; {} addresses bound in the place of a port's oldest", six[Admit::Own6 as usize],
             six[Admit::Solicit as usize], six[Admit::Spoofed as usize], six[Admit::Foreign as usize],
             EVICTED6.load(Relaxed));
    if OUT_CHECKED.load(Relaxed) + BACK_CHECKED.load(Relaxed) != 0 {
        println!("  nat: {} packets checked out, {} back ({} of them answers a live flow had to let in); {} answers \
                  tried; {} inputs with no NAT", OUT_CHECKED.load(Relaxed), BACK_CHECKED.load(Relaxed),
                 MUST_LET_IN.load(Relaxed), ANSWERS.load(Relaxed), NO_NAT.load(Relaxed));
        println!("  nat: {} packets denied, checked; {} rejects checked; {} flows a full table took, checked",
                 DENIED_CHECKED.load(Relaxed), REJECTS_CHECKED.load(Relaxed), TAKEN_CHECKED.load(Relaxed));
    }
}

fn counters() -> [usize; 9] {
    let c = &COUNTERS;
    let l = |a: &std::sync::atomic::AtomicUsize| a.load(std::sync::atomic::Ordering::Relaxed);
    [l(&c.out), l(&c.back), l(&c.mapped), l(&c.full), l(&c.no_hop), l(&c.no_frame), l(&c.refused), l(&c.denied),
     l(&c.taken)]
}

/// A packet NAT took is counted once, as what became of it -- sent out,
/// sent back, dropped for one reason, denied -- and one it did not take not
/// at all.
fn check_counted(taken: bool, before: [usize; 9], after: [usize; 9]) {
    let d: Vec<usize> = before.iter().zip(after.iter()).map(|(b, a)| a - b).collect();
    let fates = d[0] + d[1] + d[3] + d[4] + d[5] + d[6] + d[7];
    if taken {
        invariant!(fates == 1 && d[2] <= 1 && d[8] <= d[2], "a packet NAT took counted {:?}", d);
    } else {
        invariant!(fates == 0 && d[2] == 0 && d[8] == 0, "a packet NAT left to the stack counted {:?}", d);
    }
}

/// Every byte of `out` as it was in `inp`, but those at `skip`.
fn same_but(inp: &[u8], out: &[u8], skip: &[core::ops::Range<usize>], what: &str) {
    for i in 0..inp.len().min(out.len()) {
        if !skip.iter().any(|r| r.contains(&i)) {
            invariant!(inp[i] == out[i], "{}: byte {} changed", what, i);
        }
    }
}

impl NatModel {
    fn saw_out(&mut self, key: Key, port: u16, mac: [u8; 6]) {
        let now = time::peek();

        if let Some(f) = self.by_port.get(&port) {
            invariant!(f.key == key || now >= f.last.saturating_add(LIVE_NS),
                       "port {} given to {:?} while {:?} was live on it", port, key, f.key);
        }
        if let Some(&old) = self.by_key.get(&key) {
            let live = self.by_port.get(&old).is_some_and(|f| f.key == key && now < f.last.saturating_add(LIVE_NS));
            invariant!(old == port || !live, "live flow {:?} moved from port {} to {}", key, old, port);
        }
        self.by_port.insert(port, Flow { key, mac, last: now });
        self.by_key.insert(key, port);
        self.recent.retain(|&p| p != port);
        self.recent.push_back(port);
        if self.recent.len() > 16 {
            self.recent.pop_front();
        }
    }
}

/// What NAT sent out for a guest's packet: its source the uplink's address
/// and a port of NAT's, one hop on, its checksums what they were -- right
/// where they were right -- and nothing else changed.
fn check_out(frame: &[u8], out: &[u8], m: &mut NatModel) {
    OUT_CHECKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = &frame[ETH_HDR_LEN..];
    let ihl = ip::header_len(p);
    let len = usize::from(ip::total_len(p));
    invariant!(out.len() == ETH_HDR_LEN + len, "a packet of {} sent as {} bytes", len, out.len());
    let o = &out[ETH_HDR_LEN..];
    let hop = ETH0.route_ip(ip::dst(p));
    let known = crate::abi::arp_table().and_then(|a| a.lookup(hop));
    invariant!(known == Some(out[..6].try_into().unwrap()) && out[6..12] == ETH0_MAC && wire::be16(out, 12) == ETH_TYPE_IP,
               "sent to {:02x?} from {:02x?}, the hop {:?}", &out[..6], &out[6..12], known);
    invariant!(ip::src(o) == ETH0.ip() && ip::dst(o) == ip::dst(p) && o[ip::TTL] == p[ip::TTL] - 1,
               "a header rewritten wrong: {:08x} -> {:08x}, ttl {}", ip::src(o), ip::dst(o), o[ip::TTL]);
    invariant!(wire::checksum(&o[..ihl]) == 0, "a header sent with a wrong sum");
    same_but(&p[..ihl], &o[..ihl], &[ip::TTL..ip::TTL + 1, ip::CHECKSUM..ip::CHECKSUM + 2, ip::SRC..ip::SRC + 4],
             "the header");
    let proto = ip::protocol(p);
    let (pl4, ol4) = (&p[ihl..len], &o[ihl..len]);
    let (sport, dport, sum) = fields(proto);
    let external = wire::be16(ol4, sport);
    invariant!((PORT_BASE..=PORT_LAST).contains(&external), "a flow out on port {}", external);
    same_but(pl4, ol4, &[sport..sport + 2, sum..sum + 2], "the transport header or payload");
    let (before, after) = (l4_sum_holds(proto, ip::src(p), ip::dst(p), pl4), l4_sum_holds(proto, ip::src(o), ip::dst(o), ol4));
    invariant!(before == after, "a transport sum {:?} going out, {:?} gone out", before, after);
    let remote_port = if proto == IP_PROTO_ICMP { 0 } else { wire::be16(pl4, dport) };
    let key = (proto, ip::src(p), wire::be16(pl4, sport), ip::dst(p), remote_port);
    m.saw_out(key, external, frame[6..12].try_into().unwrap());
    if m.sent.len() < 64 {
        m.sent.push(out.to_vec());
    }
}

/// A guest's packet, from `hv0`: mostly for the world -- a new flow, or one
/// that went out before going on -- and every field of it wrong now and then.
fn outbound(r: &mut Input, m: &mut NatModel) {
    let known: Vec<Key> = m.by_key.keys().copied().collect();
    let follow = !known.is_empty() && r.u8() < 110;
    let (guest, src, dst, proto, sport, dport) = if follow {
        let (proto, src, sport, dst, dport) = known[r.below(known.len() as u64) as usize];
        (src, src, dst, proto, sport, dport)
    } else {
        let guest = SUBNET | (2 + r.below(16) as u32);
        let src = match r.u8() % 16 {
            0 => HOST_IP,
            1 => r.u32(),
            2 => guest ^ 0x100,
            _ => guest,
        };
        let dst = match r.u8() % 16 {
            0..=7 => r.pick(&REMOTES),
            8 => r.pick(&KEPT_HOSTS),
            /* The uplink's own subnet, and the DNS server the guests are
             * given -- theirs to ask at port 53 alone. */
            9 if r.bool() => (ETH0.ip() & ETH0.mask()) | (2 + r.below(200) as u32),
            9 if network::dns::upstream() != 0 => network::dns::upstream(),
            9 => r.pick(&KEPT_HOSTS),
            10 => SUBNET | r.below(256) as u32,
            11 => ETH0.ip(),
            12 => ETH0.ip() | !ETH0.mask(),
            13 => r.pick(&[0xE000_0001, 0x7F00_0001, 0x0102_0304, 0xFFFF_FFFF]),
            _ => r.u32(),
        };
        let proto = r.pick(&[IP_PROTO_TCP, IP_PROTO_TCP, IP_PROTO_TCP, IP_PROTO_UDP, IP_PROTO_UDP, IP_PROTO_UDP,
                             IP_PROTO_ICMP, IP_PROTO_ICMP, 47]);
        let sport = r.pick(&[40000u16, 40001, 50000, 1024]).wrapping_add(if r.u8() < 16 { r.u16() } else { 0 });
        let dport = r.pick(&[80u16, 443, 53, 0]).wrapping_add(if r.u8() < 16 { r.u16() } else { 0 });
        (guest, src, dst, proto, sport, dport)
    };
    let flags = r.pick(&[tcp::SYN, tcp::SYN, tcp::ACK_FLAG, tcp::SYN | tcp::ACK_FLAG, tcp::FIN | tcp::ACK_FLAG,
                         tcp::RST, tcp::PSH | tcp::ACK_FLAG]);
    let kind = r.pick(&[icmp::ECHO_REQUEST, icmp::ECHO_REQUEST, icmp::ECHO_REQUEST, icmp::ECHO_REPLY, icmp::DEST_UNREACH]);
    /* Now and then more than a frame holds, which NAT cannot copy. */
    let payload = if r.u8() < 4 { 1400 + r.below(200) as usize } else { r.below(300) as usize };
    let sum = sum_kind(r, proto);
    let mut l4 = transport(r, proto, src, dst, sport, dport, payload, sum, flags, kind);
    if r.u8() < 8 {
        let n = r.below(l4.len() as u64) as usize;
        l4.truncate(n);
    }
    let ttl = if r.u8() < 240 { 64 } else { r.pick(&[0u8, 1, 2, 255]) };
    let ihl = if r.u8() < 230 { IP_HDR_LEN } else { IP_HDR_LEN + 4 * (1 + r.below(10) as usize) };
    let frag = match r.u8() % 32 {
        0 => ip::MORE_FRAGMENTS,
        1 => 1,
        2..=9 => ip::DONT_FRAGMENT,
        _ => 0,
    };
    let honest = ihl + l4.len();
    let total = match r.u8() % 32 {
        0 => honest + 1 + r.below(50) as usize,
        1 => r.below(ihl as u64 + 1) as usize,
        2 => r.below(honest as u64 + 1) as usize,
        _ => honest,
    };
    let packet = ipv4(ihl, r.u16(), frag, ttl, proto, src, dst, &l4, total, r.u8() < 8);
    let mut frame = ethernet(HOST_MAC, guest_mac(guest & 0xFF), &packet);
    if r.u8() < 16 {
        frame.extend((0..r.below(40)).map(|i| i as u8));
    }

    let shares = shares_if_full(&frame, m);
    let before = counters();
    let taken = nat::intercept(&HV0, &frame);
    let after = counters();
    check_counted(taken, before, after);
    let sent = ETH0.take_sent();
    check_share(&frame, before, after, shares, sent.first().map(|o| out_port(o)), m);
    let back = HV0.take_sent();
    let (target_proto, target, target_port) = target_of(&frame);
    if after[7] > before[7] {
        /* Denied: for where the model keeps the guests from, and out of
         * neither device but as a reject to its guest. */
        DENIED_CHECKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        invariant!(kept(m, target_proto, target, target_port), "a guest's packet for {}:{} denied, where it may go",
                   wire::Ipv4(target), target_port);
        invariant!(sent.is_empty() && back.len() <= 1, "a denied packet sent on: {} out, {} back", sent.len(),
                   back.len());
        if let Some(b) = back.first() {
            check_reject(&frame, b);
        }
    } else {
        invariant!(back.is_empty(), "a guest's packet sent back to the guests");
    }
    invariant!(sent.len() <= 1 && (sent.is_empty() || taken), "{} packets sent for one ({})", sent.len(), taken);
    match sent.first() {
        Some(out) => {
            invariant!(!kept(m, target_proto, target, target_port),
                       "a guest's packet for {}:{} sent out, where NAT keeps the guests from", wire::Ipv4(target),
                       target_port);
            check_out(&frame, out, m)
        }
        None if after[2] > before[2] => {
            /* Mapped, and not sent: the key as the packet had it. */
            let p = &frame[ETH_HDR_LEN..];
            let ihl = ip::header_len(p);
            let (s, d, _) = fields(ip::protocol(p));
            let l4 = &p[ihl..];
            let remote_port = if ip::protocol(p) == IP_PROTO_ICMP { 0 } else { wire::be16(l4, d) };
            let key = (ip::protocol(p), ip::src(p), wire::be16(l4, s), ip::dst(p), remote_port);
            m.unsent.insert(key, frame[6..12].try_into().unwrap());
        }
        None => {}
    }
}

/// What NAT sent back to a guest for the world's packet on port `port`:
/// for the flow on it, to the guest's address and port and MAC, one hop on,
/// sums as they were; and only from the far end the flow went to.
fn check_back(frame: &[u8], back: &[u8], m: &mut NatModel) {
    BACK_CHECKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = &frame[ETH_HDR_LEN..];
    let ihl = ip::header_len(p);
    let len = usize::from(ip::total_len(p));
    invariant!(back.len() == ETH_HDR_LEN + len, "an answer of {} sent back as {} bytes", len, back.len());
    let o = &back[ETH_HDR_LEN..];
    let proto = ip::protocol(p);
    let (pl4, ol4) = (&p[ihl..len], &o[ihl..len]);
    let error = proto == IP_PROTO_ICMP && matches!(icmp::kind(pl4), icmp::DEST_UNREACH | icmp::TIME_EXCEEDED);
    /* Whose it is: the port it came to -- or, an error, the port of the
     * packet it quotes -- and from where. */
    let (port, flow_proto, far_ip, far_port) = if error {
        let q = &pl4[ICMP_HDR_LEN..];
        let qihl = ip::header_len(q);
        let qproto = ip::protocol(q);
        let (s, d, _) = fields(qproto);
        let far_port = if qproto == IP_PROTO_ICMP { 0 } else { wire::be16(&q[qihl..], d) };
        (wire::be16(&q[qihl..], s), qproto, ip::dst(q), far_port)
    } else {
        let (_, d, _) = fields(proto);
        let far_port = if proto == IP_PROTO_ICMP { 0 } else { wire::be16(pl4, fields(proto).0) };
        (wire::be16(pl4, d), proto, ip::src(p), far_port)
    };
    /* Whose the port is: the table's word -- a mapping may be made for a
     * packet that never went out, on a port the model last saw another flow
     * on -- and never another flow's that was live on it. */
    let Some(key) = table_flow(port) else {
        panic!("invariant: an answer sent back for port {}, which NAT has no mapping on", port)
    };
    let now = time::peek();
    /* An answer keeps its flow alive; an error, NAT holds, says nothing of
     * that -- and the model must not say more than NAT does. */
    let (mac, last) = match m.by_port.get(&port) {
        Some(f) if f.key == key => (f.mac, if error { f.last } else { now }),
        Some(f) => {
            invariant!(now >= f.last.saturating_add(LIVE_NS), "port {} is {:?}'s in the table, {:?} live on it",
                       port, key, f.key);
            (m.unsent.get(&key).copied().unwrap_or(back[..6].try_into().unwrap()), if error { 0 } else { now })
        }
        None => (m.unsent.get(&key).copied().unwrap_or(back[..6].try_into().unwrap()), if error { 0 } else { now }),
    };
    m.by_port.insert(port, Flow { key, mac, last });
    m.by_key.insert(key, port);
    let flow = m.by_port.get_mut(&port).expect("the flow, known now");
    let (kproto, inner_ip, inner_port, remote_ip, remote_port) = flow.key;
    invariant!(kproto == flow_proto && remote_ip == far_ip && remote_port == far_port,
               "an answer from {:08x}:{} let in to flow {:?}", far_ip, far_port, flow.key);
    invariant!(back[..6] == flow.mac && back[6..12] == HOST_MAC && wire::be16(back, 12) == ETH_TYPE_IP,
               "an answer sent to {:02x?} from {:02x?}", &back[..6], &back[6..12]);
    invariant!(ip::dst(o) == inner_ip && ip::src(o) == ip::src(p) && o[ip::TTL] == p[ip::TTL] - 1,
               "an answer's header rewritten wrong");
    invariant!(wire::checksum(&o[..ihl]) == 0, "an answer sent back with a wrong header sum");
    same_but(&p[..ihl], &o[..ihl], &[ip::TTL..ip::TTL + 1, ip::CHECKSUM..ip::CHECKSUM + 2, ip::DST..ip::DST + 4],
             "an answer's header");
    if error {
        invariant!(wire::checksum(ol4) == 0, "an ICMP error sent back with a wrong sum");
        let (q, oq) = (&pl4[ICMP_HDR_LEN..], &ol4[ICMP_HDR_LEN..]);
        let qihl = ip::header_len(q);
        let (s, _, qsum) = fields(flow_proto);
        invariant!(ip::src(oq) == inner_ip && wire::be16(&oq[qihl..], s) == inner_port,
                   "an ICMP error's quote rewritten wrong");
        invariant!(wire::checksum(&oq[..qihl]) == 0, "an ICMP error's quote with a wrong header sum");
        same_but(q, oq, &[ip::CHECKSUM..ip::CHECKSUM + 2, ip::SRC..ip::SRC + 4, qihl + s..qihl + s + 2,
                          qihl + qsum..qihl + qsum + 2], "an ICMP error's quote");
    } else {
        let (_, d, sum) = fields(proto);
        invariant!(wire::be16(ol4, d) == inner_port, "an answer to port {} of the guest's {}", wire::be16(ol4, d),
                   inner_port);
        same_but(pl4, ol4, &[d..d + 2, sum..sum + 2], "an answer's transport header or payload");
        let before = l4_sum_holds(proto, ip::src(p), ip::dst(p), pl4);
        let after = l4_sum_holds(proto, ip::src(o), ip::dst(o), ol4);
        invariant!(before == after, "an answer's transport sum {:?} coming in, {:?} sent back", before, after);
    }
}

/// The world's packet, to the uplink: an answer to a flow that went out --
/// from where it went, mostly -- an ICMP error quoting a packet that went
/// out, or anything.
fn inbound(r: &mut Input, m: &mut NatModel) {
    /* A flow seen last, mostly; now and then any. */
    let ports: Vec<u16> = if r.u8() < 224 && !m.recent.is_empty() {
        m.recent.iter().copied().collect()
    } else {
        m.by_port.keys().copied().collect()
    };
    let flows: Vec<(u16, Key, u64)> = ports.iter().map(|p| (*p, m.by_port[p].key, m.by_port[p].last)).collect();
    let pick = |r: &mut Input| flows[r.below(flows.len() as u64) as usize];
    let ttl = if r.u8() < 240 { 64 } else { r.pick(&[0u8, 1, 2]) };
    let kind = r.u8() % 16;
    let (frame, must_let_in) = if kind < 11 && !flows.is_empty() {
        ANSWERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (port, key, last) = pick(r);
        let (proto, _, _, remote_ip, remote_port) = key;
        let src = if r.u8() < 220 { remote_ip } else { r.pick(&REMOTES) };
        let sport = if r.u8() < 220 { remote_port } else { r.u16() };
        let dport = if r.u8() < 230 { port } else { r.pick(&[port.wrapping_add(1), PORT_BASE - 1, PORT_LAST + 1]) };
        let proto2 = if r.u8() < 245 { proto } else { r.pick(&[IP_PROTO_TCP, IP_PROTO_UDP, IP_PROTO_ICMP]) };
        let flags = r.pick(&[tcp::ACK_FLAG, tcp::SYN | tcp::ACK_FLAG, tcp::FIN | tcp::ACK_FLAG, tcp::RST,
                             tcp::PSH | tcp::ACK_FLAG]);
        let icmp_kind = if r.u8() < 240 { icmp::ECHO_REPLY } else { icmp::ECHO_REQUEST };
        let sum = sum_kind(r, proto2);
        let (tsrc, tdst) = if proto2 == IP_PROTO_ICMP { (dport, 0) } else { (sport, dport) };
        let payload = r.below(200) as usize;
        let l4 = transport(r, proto2, src, ETH0.ip(), tsrc, tdst, payload, sum, flags, icmp_kind);
        let bad_sum = r.u8() < 4;
        let packet = ipv4(IP_HDR_LEN, r.u16(), 0, ttl, proto2, src, ETH0.ip(), &l4, IP_HDR_LEN + l4.len(), bad_sum);
        /* A well-made answer from the flow's far end, to its port, while it
         * is live for certain: NAT must let it in. */
        let answer = proto2 == proto && src == remote_ip && (proto == IP_PROTO_ICMP || sport == remote_port)
            && dport == port && !bad_sum && ttl > 1 && (proto != IP_PROTO_ICMP || icmp_kind == icmp::ECHO_REPLY)
            && time::peek() < last.saturating_add(LIVE_NS);
        (ethernet(ETH0_MAC, GW_MAC, &packet), answer)
    } else if kind < 14 && !m.sent.is_empty() {
        /* A router's error about a packet that went out: its header and
         * the start of its transport -- or more -- quoted. */
        let sent = &m.sent[r.below(m.sent.len() as u64) as usize];
        let q = &sent[ETH_HDR_LEN..];
        let qihl = ip::header_len(q);
        let keep = if r.u8() < 200 { qihl + 8 } else { q.len() };
        let mut quoted = q[..keep.min(q.len())].to_vec();
        if r.u8() < 8 {
            let i = r.below(quoted.len() as u64) as usize;
            quoted[i] ^= 0x40;
        }
        let mut msg = vec![0u8; ICMP_HDR_LEN];
        msg.extend_from_slice(&quoted);
        let n = msg.len();
        icmp::write(&mut msg, r.pick(&[icmp::DEST_UNREACH, icmp::TIME_EXCEEDED]), icmp::PORT_UNREACH, 0, 0, n);
        if r.u8() < 8 {
            msg[icmp::CHECKSUM] ^= 0x10;
        }
        let router = if r.bool() { ETH0.gw() } else { r.u32() };
        let packet = ipv4(IP_HDR_LEN, r.u16(), 0, ttl, IP_PROTO_ICMP, router, ETH0.ip(), &msg, IP_HDR_LEN + msg.len(), false);
        (ethernet(ETH0_MAC, GW_MAC, &packet), false)
    } else {
        let n = r.below(200) as usize;
        let mut f = noise(r.u32(), n);
        if f.len() >= ETH_HDR_LEN + IP_HDR_LEN && r.bool() {
            wire::set_be16(&mut f, 12, ETH_TYPE_IP);
            f[ETH_HDR_LEN] = 0x45;
        }
        (f, false)
    };

    let before = counters();
    let taken = nat::intercept(&ETH0, &frame);
    check_counted(taken, before, counters());
    invariant!(ETH0.take_sent().is_empty(), "the world's packet sent back out to the world");
    let back = HV0.take_sent();
    invariant!(back.len() <= 1 && (back.is_empty() || taken), "{} packets sent back for one", back.len());
    if must_let_in {
        MUST_LET_IN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    match back.first() {
        Some(b) => check_back(&frame, b, m),
        None => invariant!(!must_let_in || HV0.refuse.load(std::sync::atomic::Ordering::Relaxed),
                           "a live flow's answer not let in"),
    }
}

/// NAT between `hv0` and an uplink with a gateway: guests' packets out, the
/// world's back, time passing, the table filled, NAT off and on again.
pub fn nat_target(r: &mut Input) {
    /* NAT's own self-test, as each boot runs it: its table's shares, in
     * the cases a random input seldom makes -- ties, every slot held. */
    static SELFTEST: std::sync::Once = std::sync::Once::new();
    SELFTEST.call_once(|| invariant!(nat::selftest(), "NAT's boot self-test failed"));
    network::reset();
    let outer_ip = r.pick(&[0xC0A8_010Au32, 0x0A00_020F, 0x5F10_2030]);
    let mask = r.pick(&[0xFFFF_FF00u32, 0xFFFF_0000, 0]);
    let gw = if r.u8() < 240 { (outer_ip & 0xFFFF_FF00) | 1 } else { r.pick(&[0u32, 0x0808_0808]) };
    HV0.set(HOST_MAC, HOST_IP, MASK, 0);
    ETH0.set(ETH0_MAC, outer_ip, mask, gw);
    if r.u8() < 32 {
        crate::device::SPARE.set([0x52, 0x54, 0, 0, 0, 9], r.u32(), MASK, 0);
    }
    /* The DNS server the guests are given: none, one on the uplink's
     * subnet, one in the world, QEMU's user network's, or a cloud's
     * metadata server, which answers DNS there too. */
    network::dns::UPSTREAM.store(r.pick(&[0u32, (outer_ip & mask) | 53, 0x0808_0808, 0x0A00_0203, 0xA9FE_A9FE]),
                                 std::sync::atomic::Ordering::Relaxed);
    if r.u8() < 230 {
        network::arp_learn(gw, GW_MAC);
    }
    let nat = crate::abi::nat().expect("NAT");
    let on = nat.enable(&HV0, None);
    let Ok(outer) = on else {
        NO_NAT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        invariant!(DEVICES.uplink(&HV0).is_none() || ETH0.ip() == 0, "NAT refused with an uplink: {:?}", on.err());
        return;
    };
    invariant!(core::ptr::eq(outer, &ETH0), "NAT out through {:?}", core::str::from_utf8(outer.name()));
    invariant!(matches!(nat.enable(&HV0, None), Err(nat::NatError::Busy)), "NAT on twice");
    let mut m = NatModel::default();
    /* Now and then the host lets the guests through to a kept network, as
     * a test machine does its own -- by the shell command a person types. */
    for _ in 0..r.below(3) {
        allow_op(r, &mut m, nat);
    }

    while let Some(op) = r.op(10) {
        match op {
            0..=3 => outbound(r, &mut m),
            4..=6 => inbound(r, &mut m),
            7 if r.u8() < 128 => {}
            7 => time::advance(r.pick(&[NS_PER_SEC, 9 * NS_PER_SEC, 11 * NS_PER_SEC, 59 * NS_PER_SEC,
                                        61 * NS_PER_SEC, 239 * NS_PER_SEC, 241 * NS_PER_SEC, 299 * NS_PER_SEC,
                                        301 * NS_PER_SEC, 7439 * NS_PER_SEC, 7441 * NS_PER_SEC])),
            8 => {
                /* Many flows at once, as many as the table holds and past
                 * it: each checked as any other. */
                /* One flood that fills the table, at most, an input: what
                 * comes after it is what the share is for. */
                let n = if r.u8() < 4 && !m.flooded { 4200 } else { r.pick(&[8u32, 64, 256]) };
                m.flooded |= n == 4200;
                let guest = SUBNET | (2 + r.below(16) as u32);
                let remote = r.pick(&REMOTES);
                /* The flood's datagrams are one, drawn from the input once
                 * and sent from a port each, with no checksum -- which IPv4
                 * lets UDP have, and NAT keeps as none: drawn for each, a
                 * flood took more bytes than an input has, and nothing came
                 * after it. A smaller burst draws each, as any packet does. */
                let datagram = transport(r, IP_PROTO_UDP, guest, remote, 10000, 53, 4, Sum::Right, 0, 0);
                for i in 0..n {
                    let sport = 10000u16.wrapping_add(i as u16);
                    let l4 = if n == 4200 {
                        let mut l4 = datagram.clone();
                        wire::set_be16(&mut l4, udp::SRC_PORT, sport);
                        wire::set_be16(&mut l4, udp::CHECKSUM, 0);
                        l4
                    } else {
                        transport(r, IP_PROTO_UDP, guest, remote, sport, 53, 4, Sum::Right, 0, 0)
                    };
                    let packet = ipv4(IP_HDR_LEN, i as u16, 0, 64, IP_PROTO_UDP, guest, remote, &l4,
                                      IP_HDR_LEN + l4.len(), false);
                    let frame = ethernet(HOST_MAC, guest_mac(guest), &packet);
                    let shares = shares_if_full(&frame, &mut m);
                    let before = counters();
                    let taken = nat::intercept(&HV0, &frame);
                    let after = counters();
                    check_counted(taken, before, after);
                    let sent = ETH0.take_sent();
                    check_share(&frame, before, after, shares, sent.first().map(|o| out_port(o)), &mut m);
                    if let Some(out) = sent.first() {
                        check_out(&frame, out, &mut m);
                    }
                }
            }
            _ => match r.u8() % 7 {
                0 => {
                    /* What `nat` shows. */
                    let mut shown = [nat::Mapping { proto: 0, inner_ip: 0, inner_port: 0, remote_ip: 0,
                                                    remote_port: 0, port: 0, left_ns: 0 }; 8];
                    if let Some((_, _, active, n)) = nat.state(&mut shown) {
                        invariant!(n <= shown.len() && active <= nat::ENTRIES, "nat state {} shown of {}", n, active);
                        for s in &shown[..n] {
                            invariant!((PORT_BASE..=PORT_LAST).contains(&s.port) && s.left_ns > 0,
                                       "a mapping shown on port {} with {} ns left", s.port, s.left_ns);
                        }
                    }
                    let mut out = crate::cmd::Output::default();
                    nat::shell("", &mut out);
                    invariant!(out.0.starts_with("nat: on"), "nat said {:?}", out.0.lines().next());
                }
                1 => {
                    let on = r.bool();
                    HV0.refuse.store(on, std::sync::atomic::Ordering::Relaxed);
                    ETH0.refuse.store(r.u8() < 64, std::sync::atomic::Ordering::Relaxed);
                }
                2 => {
                    if r.bool() {
                        network::arp_learn(ETH0.gw(), GW_MAC);
                    } else if let Some(arp) = crate::abi::arp_table() {
                        arp.known.lock().unwrap().clear();
                    }
                }
                3 => {
                    /* Off and on again, through the C ABI a module has: a new
                     * table, every mapping gone. */
                    invariant!(nat::kernel_net_nat_disable(HV0.handle()) == 1, "NAT was not on");
                    invariant!(nat::kernel_net_nat_disable(HV0.handle()) == 0, "NAT off twice");
                    let on = nat::kernel_net_nat_enable(HV0.handle());
                    invariant!(on.code == 0 && on.outer == ETH0.handle(), "NAT back on: {:?}", on);
                    m.fresh();
                }
                4 => {
                    /* A device NAT cannot take -- none, hv0 as its own way
                     * out -- NAT on while it is, and `nat` while it is off. */
                    invariant!(nat::kernel_net_nat_enable(0).code == 2, "NAT on for no device");
                    invariant!(nat.enable(&HV0, Some(&HV0)).is_err(), "NAT through hv0 itself");
                    invariant!(nat::kernel_net_nat_enable(HV0.handle()).code == 3, "NAT on twice");
                    if r.bool() {
                        nat::kernel_net_nat_disable(HV0.handle());
                        let mut out = crate::cmd::Output::default();
                        nat::shell("", &mut out);
                        invariant!(out.0.starts_with("nat: off"), "nat said {:?} when off", out.0.lines().next());
                        let gw = ETH0.gw();
                        ETH0.set(ETH0_MAC, ETH0.ip(), ETH0.mask(), 0);
                        invariant!(nat::kernel_net_nat_enable(HV0.handle()).code == 1, "NAT on with no uplink");
                        ETH0.set(ETH0_MAC, ETH0.ip(), ETH0.mask(), gw);
                        invariant!(nat::kernel_net_nat_enable(HV0.handle()).code == 0, "NAT back on");
                        m.fresh();
                    }
                }
                5 => allow_op(r, &mut m, nat),
                _ => {
                    /* A guest gone (`Switch::release`): every mapping of its
                     * address forgotten through the C ABI the module has,
                     * and none of them left -- the next guest on its port has
                     * its address, and must be answered for no flow of the
                     * one before. Its flows go from the model too: an answer
                     * on one of their ports is the table's to drop now. */
                    let guest = if r.u8() < 230 { SUBNET | (2 + r.below(16) as u32) } else { r.u32() };
                    nat::kernel_net_nat_forget(HV0.handle(), guest);
                    let gone: Vec<u16> = m.by_port.iter().filter(|(_, f)| f.key.1 == guest).map(|(p, _)| *p).collect();
                    for p in &gone {
                        if let Some(f) = m.by_port.remove(p) {
                            m.by_key.remove(&f.key);
                        }
                    }
                    m.recent.retain(|p| !gone.contains(p));
                    m.unsent.retain(|k, _| k.1 != guest);
                    invariant!(!table_maps(guest), "NAT still maps a flow of {:#x}'s after forgetting it", guest);
                }
            },
        }
    }
}

/* ---- the whole of it: guests on the switch, NAT on through it ---- */

/// Whether `f` is sent as the guest on port `q`, which is all the switch may
/// let a guest send: IPv4 from its MAC and address, ARP from its MAC with
/// its MAC and address -- or none yet -- as the sender's.
fn as_guest(q: usize, f: &[u8]) -> bool {
    let (mac, me) = (switch::port_mac(q), switch::port_ip(q));
    if f.len() < ETH_HDR_LEN || f[6..12] != mac {
        return false;
    }
    match wire::be16(f, 12) {
        ETH_TYPE_IP => f.len() >= ETH_HDR_LEN + IP_HDR_LEN && wire::be32(f, ETH_HDR_LEN + ip::SRC) == me,
        ETH_TYPE_ARP if f.len() >= ETH_HDR_LEN + ARP_LEN => {
            let a = &f[ETH_HDR_LEN..];
            arp::sender_mac(a) == mac && (arp::sender_ip(a) == 0 || arp::sender_ip(a) == me)
        }
        /* IPv6 from none of another port's addresses: which of the rest
         * are whose, the switch target's model says. */
        0x86DD if f.len() >= ETH_HDR_LEN + 40 => {
            let src = &f[ETH_HDR_LEN + 8..ETH_HDR_LEN + 24];
            !(0..MAX_PORTS).any(|o| o != q && (src == eui64(&LINK_LOCAL, o) || src == eui64(&PREFIX6, o)))
        }
        _ => false,
    }
}

/// What `hv0` took in while port `p` sent: nothing but its guest's own.
fn check_hv0_took(p: usize) {
    for f in network::take_hv0_in() {
        invariant!(as_guest(p, &f), "hv0 took a frame from port {} not sent as its guest: type {:04x}, {} bytes",
                   p, if f.len() >= ETH_HDR_LEN { wire::be16(&f, 12) } else { 0 }, f.len());
    }
}

/// Guests on the switch with NAT on -- turned on as the module does, by the
/// switch's way out -- their packets for the world through the switch, `hv0`
/// and NAT out of the uplink, the world's answers back in through NAT, `hv0`
/// and the switch to a port. What a guest's NIC takes from the world must
/// be for that guest: a packet another guest's flow was answered with, in
/// this guest's inbox, is one guest reading another's traffic. And what one guest
/// takes from another, or `hv0` from a guest, must be sent as that guest.
pub fn guestnet_target(r: &mut Input) {
    network::reset();
    let outer_ip = 0xC0A8_010A;
    let gw = 0xC0A8_0101;
    ETH0.set(ETH0_MAC, outer_ip, 0xFFFF_FF00, gw);
    network::arp_learn(gw, GW_MAC);
    let sw = Switch::new().expect("a switch");
    invariant!(sw.way_out() == Ok(outer_ip), "no way out through an uplink with a gateway");
    invariant!(sw.nat_address() == Some(outer_ip), "NAT not on after the way out");
    let n = 2 + r.below(3) as usize;
    let mut backends = Vec::new();
    let mut vms = Vec::new();
    for _ in 0..n {
        let vm = Arc::new(Shared::default());
        let p = sw.claim(&vm, None).expect("a port");
        backends.push(sw.backend(p));
        vms.push(vm);
    }
    /* What went out, for the world to answer. */
    let mut sent: Vec<Vec<u8>> = Vec::new();

    while let Some(op) = r.op(8) {
        match op {
            0 | 1 => {
                /* A guest's packet for the world: as itself -- or, now and
                 * then, with another guest's address, or another's MAC. */
                let p = r.below(n as u64) as usize;
                let other = r.below(n as u64) as usize;
                let src_ip = if r.u8() < 16 { switch::port_ip(other) } else { switch::port_ip(p) };
                let src_mac = if r.u8() < 16 { switch::port_mac(other) } else { switch::port_mac(p) };
                let proto = r.pick(&[IP_PROTO_UDP, IP_PROTO_TCP, IP_PROTO_ICMP]);
                let dst = r.pick(&REMOTES);
                let sport = r.pick(&[40000u16, 40001, 5353]);
                let dport = r.pick(&[53u16, 80]);
                let flags = if r.u8() < 200 { tcp::SYN } else { tcp::ACK_FLAG };
                let payload = r.below(64) as usize;
                let l4 = transport(r, proto, src_ip, dst, sport, dport, payload, Sum::Right, flags, icmp::ECHO_REQUEST);
                let packet = ipv4(IP_HDR_LEN, r.u16(), 0, 64, proto, src_ip, dst, &l4, IP_HDR_LEN + l4.len(), false);
                backends[p].send(&ethernet(HOST_MAC, src_mac, &packet));
                check_hv0_took(p);
                sent.extend(ETH0.take_sent());
            }
            5 => {
                /* A guest's ARP message -- a request, a reply, a probe --
                 * for hv0 or everyone: as itself, or saying another guest's
                 * address, or hv0's, is its. */
                let p = r.below(n as u64) as usize;
                let other = r.below(n as u64) as usize;
                let sender_ip = match r.u8() % 8 {
                    0 => 0,
                    1 => switch::port_ip(other),
                    2 => HOST_IP,
                    _ => switch::port_ip(p),
                };
                let sender_mac = if r.u8() < 16 { switch::port_mac(other) } else { switch::port_mac(p) };
                let src_mac = if r.u8() < 16 { switch::port_mac(other) } else { switch::port_mac(p) };
                let target = r.pick(&[HOST_IP, switch::port_ip(other), switch::port_ip(p)]);
                let mut a = [0u8; ARP_LEN];
                arp::write(&mut a, r.pick(&[1u16, 2]), &sender_mac, sender_ip, &[0; 6], target);
                let dst = if r.u8() < 128 { MAC_BROADCAST } else { HOST_MAC };
                let mut f = ethernet(dst, src_mac, &a);
                wire::set_be16(&mut f, 12, ETH_TYPE_ARP);
                backends[p].send(&f);
                check_hv0_took(p);
            }
            6 => {
                /* A guest's DHCP request -- from no address, its own or
                 * another's -- or a frame of a protocol the switch does not
                 * carry: IPv6, as every Linux sends at boot. */
                let p = r.below(n as u64) as usize;
                let f = if r.u8() < 128 {
                    dhcp_request(r, p)
                } else {
                    let mut f = ethernet(MAC_BROADCAST, switch::port_mac(p), &noise(r.u32(), 40 + r.below(64) as usize));
                    wire::set_be16(&mut f, 12, 0x86DD);
                    f
                };
                backends[p].send(&f);
                check_hv0_took(p);
            }
            2 | 3 if !sent.is_empty() => {
                /* The world answers something that went out. */
                let out = &sent[r.below(sent.len() as u64) as usize];
                let q = &out[ETH_HDR_LEN..];
                let ihl = ip::header_len(q);
                let proto = ip::protocol(q);
                let (s, d, _) = fields(proto);
                let l4 = &q[ihl..];
                let (tsrc, tdst) = if proto == IP_PROTO_ICMP { (wire::be16(l4, s), 0) } else {
                    (wire::be16(l4, d), wire::be16(l4, s))
                };
                let kind = if proto == IP_PROTO_ICMP { icmp::ECHO_REPLY } else { 0 };
                let payload = r.below(64) as usize;
                let ans = transport(r, proto, ip::dst(q), outer_ip, tsrc, tdst, payload, Sum::Right,
                                    tcp::SYN | tcp::ACK_FLAG, kind);
                let packet = ipv4(IP_HDR_LEN, r.u16(), 0, 64, proto, ip::dst(q), outer_ip, &ans,
                                  IP_HDR_LEN + ans.len(), false);
                nat::intercept(&ETH0, &ethernet(ETH0_MAC, GW_MAC, &packet));
            }
            4 => {
                /* A guest takes what came for it: from another guest, only
                 * what that guest sent as itself; from the world, only what
                 * is for its own address. */
                let p = r.below(n as u64) as usize;
                let mut buf = vec![0u8; MAX_FRAME];
                while let Some(len) = backends[p].recv(&mut buf) {
                    let f = &buf[..len];
                    invariant!(len >= ETH_HDR_LEN, "port {} took a runt of {} bytes", p, len);
                    let from = &f[6..12];
                    invariant!(from == HOST_MAC || port_of(from).is_some_and(|q| as_guest(q, f)),
                               "port {} took a frame from {:02x?} not sent as the guest there", p, from);
                    if len >= ETH_HDR_LEN + IP_HDR_LEN && wire::be16(f, 12) == ETH_TYPE_IP {
                        let ip4 = &f[ETH_HDR_LEN..];
                        let from_world = ip::src(ip4) & MASK != SUBNET;
                        invariant!(!from_world || ip::dst(ip4) == switch::port_ip(p),
                                   "port {} took the world's packet for {:08x}, another guest's", p, ip::dst(ip4));
                    }
                }
            }
            _ => time::advance(r.pick(&[0, NS_PER_SEC, 30 * NS_PER_SEC])),
        }
    }
    drop(sw);
}
