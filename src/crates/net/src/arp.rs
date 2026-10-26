//! ARP: the cache that says which Ethernet address an IP address is at, and
//! the requests and replies that fill it.
//!
//! What the cache learns is RFC 826's merge: an ARP packet updates the entry
//! its sender has, and makes one only when it was addressed to this machine
//! -- so the requests every host on a busy link broadcasts for every other do
//! not fill the cache and push out the hosts the machine is talking to. It
//! learns only from ARP that is Ethernet and IPv4 through and through, from a
//! sender that is a host: a unicast MAC not this machine's own, an address a
//! host may have. A probe's sender, which has no address yet (RFC 5227), is
//! answered and not learnt; one that says it has this machine's address is
//! a conflict, and said so.
//!
//! Entries expire, so a host that changes its MAC is resolved again rather
//! than talked to at an address that has moved. A resolution that is not
//! cached sends a request and waits for the receive path to answer it,
//! retransmitting once a second so that one dropped broadcast does not cost
//! the whole timeout. A sender that cannot wait -- NAT, which forwards from
//! the receive path -- asks with `lookup_or_ask` instead: the cache's answer,
//! even one past its time, and a request sent to make it good again.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::frame::Frame;
use crate::nic::Nic;
use kcore::sync::SpinLock;
use kcore::time;
use kcore::trace;

use crate::wire::{self, arp, eth, Mac, ARP_LEN, ETH_HDR_LEN, ETH_TYPE_ARP, ETH_TYPE_IP,
                  ARP_OP_REPLY, ARP_OP_REQUEST, MAC_BROADCAST};

const CACHE_SIZE: usize = 16;

/// How long an entry is good for. A host that changes its MAC is resolved
/// again after this.
const TTL_MS: u64 = 300_000;

/// A resolution sends this many requests, a second apart.
const RESOLVE_TRIES: u32 = 3;
const TRY_MS: u64 = 1000;

/// What `lookup_or_ask` sends goes out at most this often, whatever is
/// behind it: a guest that floods an address nothing answers for is not a
/// flood of broadcasts.
const ASK_GAP_MS: u64 = 100;
/// An entry this close to its end is asked for again by `lookup_or_ask`
/// while it is still used, so that a flow through it never meets the gap
/// between the entry expiring and the answer coming back.
const REFRESH_MS: u64 = 30_000;

/// An address conflict is said at most this often: a host that keeps
/// claiming this machine's address is not a flood of lines.
const CONFLICT_GAP_MS: u64 = 1000;

/// ARP's hardware type for Ethernet.
const HW_ETHERNET: u16 = 1;
const MAC_LEN: u8 = 6;
const IPV4_LEN: u8 = 4;

#[derive(Clone, Copy)]
struct Entry {
    ip: u32,
    mac: Mac,
    made_ms: u64,
    valid: bool,
}

pub struct ArpTable {
    cache: SpinLock<[Entry; CACHE_SIZE]>,
    /// When `lookup_or_ask` last sent a request.
    asked_ms: AtomicU64,
    /// When a conflict was last said.
    conflict_ms: AtomicU64,
}

fn now_ms() -> u64 {
    time::boot_time().as_nanos() / kcore::consts::NS_PER_MS
}

impl ArpTable {
    pub fn new() -> Option<ArpTable> {
        Some(ArpTable {
            cache: SpinLock::new(
                [Entry { ip: 0, mac: [0; 6], made_ms: 0, valid: false }; CACHE_SIZE])?,
            asked_ms: AtomicU64::new(0),
            conflict_ms: AtomicU64::new(0),
        })
    }

    /// The address of `ip`, if it is cached and has not expired. One that
    /// has is resolved again -- the answer puts it back in its place.
    pub fn lookup(&self, ip: u32) -> Option<Mac> {
        let cache = self.cache.lock();
        let now = now_ms();

        cache.iter()
            .find(|entry| entry.valid && entry.ip == ip && now.saturating_sub(entry.made_ms) < TTL_MS)
            .map(|entry| entry.mac)
    }

    /// The address of `ip` from the cache, never waited for: for a sender
    /// that cannot sleep. An entry past its time is still used, as Linux
    /// uses a stale neighbour, while it is asked for again; a host that has
    /// moved is found at its new address one answer later. When it is not
    /// there at all, or is near or past its time, a request goes out -- one
    /// per `ASK_GAP_MS` at most -- and with none there the caller drops what
    /// it had to send, which its sender sends again.
    pub fn lookup_or_ask(&self, nic: &Nic, ip: u32) -> Option<Mac> {
        if !wire::host_address(ip, 0) {
            return None;
        }
        let now = now_ms();
        let (mac, ask) = {
            let cache = self.cache.lock();
            match cache.iter().find(|entry| entry.valid && entry.ip == ip) {
                Some(entry) => {
                    (Some(entry.mac), now.saturating_sub(entry.made_ms) >= TTL_MS - REFRESH_MS)
                }
                None => (None, true),
            }
        };

        /* The request goes out with no lock held */
        if ask && self.may_ask(now) {
            send(nic, ARP_OP_REQUEST, &MAC_BROADCAST, &[0; 6], ip);
        }
        mac
    }

    /// A request for `ip`, now: for a sender that knows what it will want
    /// before it wants it. It counts as `lookup_or_ask`'s last.
    pub fn ask(&self, nic: &Nic, ip: u32) {
        self.asked_ms.store(now_ms(), Ordering::Relaxed);
        send(nic, ARP_OP_REQUEST, &MAC_BROADCAST, &[0; 6], ip);
    }

    /// Whether a request may go out now, and if so the time taken: one
    /// caller of any two at once gets it.
    fn may_ask(&self, now: u64) -> bool {
        let last = self.asked_ms.load(Ordering::Relaxed);
        now.saturating_sub(last) >= ASK_GAP_MS
            && self.asked_ms.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
    }

    /// What an ARP packet says of its sender: the entry it has is updated,
    /// and one is made only when `make`.
    fn learn(&self, ip: u32, mac: &Mac, make: bool) {
        let mut cache = self.cache.lock();
        let now = now_ms();

        for entry in cache.iter_mut() {
            if entry.valid && entry.ip == ip {
                entry.mac = *mac;
                entry.made_ms = now;
                return;
            }
        }
        if !make {
            return;
        }

        for entry in cache.iter_mut() {
            if !entry.valid {
                *entry = Entry { ip, mac: *mac, made_ms: now, valid: true };
                return;
            }
        }

        /* Full: the entry heard from longest ago makes way -- one past its
         * time before any that is not, and never the one a flow is using,
         * which `lookup_or_ask` keeps fresh. */
        if let Some(oldest) = cache.iter_mut().min_by_key(|entry| entry.made_ms) {
            *oldest = Entry { ip, mac: *mac, made_ms: now, valid: true };
        }
    }

    /// What the receive path hands over: a request for this machine's
    /// address is answered, and the sender learnt as the module's head says.
    pub fn process(&self, nic: &Nic, frame: &[u8]) {
        if frame.len() < ETH_HDR_LEN + ARP_LEN {
            return;
        }

        let packet = &frame[ETH_HDR_LEN..];
        if wire::be16(packet, arp::HW_TYPE) != HW_ETHERNET
            || wire::be16(packet, arp::PROTO_TYPE) != ETH_TYPE_IP
            || packet[arp::HW_SIZE] != MAC_LEN
            || packet[arp::PROTO_SIZE] != IPV4_LEN
        {
            return;
        }
        let opcode = arp::opcode(packet);
        if opcode != ARP_OP_REQUEST && opcode != ARP_OP_REPLY {
            return;
        }

        let sender_mac = arp::sender_mac(packet);
        let sender_ip = arp::sender_ip(packet);
        let own = nic.ip();
        /* A sender that is no host: a group, everyone, nobody -- or this
         * machine's own frame come back. */
        if sender_mac[0] & 1 != 0 || sender_mac == [0; 6] || sender_mac == nic.mac() {
            return;
        }
        if sender_ip != 0 && !wire::host_address(sender_ip, nic.mask()) {
            return;
        }
        if own != 0 && sender_ip == own {
            self.conflict(&sender_mac, own);
            return;
        }

        let for_us = own != 0 && arp::target_ip(packet) == own;
        if sender_ip != 0 {
            self.learn(sender_ip, &sender_mac, for_us);
        }

        /* The reply goes out with no lock held */
        if for_us && opcode == ARP_OP_REQUEST {
            send(nic, ARP_OP_REPLY, &sender_mac, &sender_mac, sender_ip);
        }
    }

    /// Another host says it has this machine's address: said, at most once
    /// a second.
    fn conflict(&self, mac: &Mac, own: u32) {
        let now = now_ms();
        let last = self.conflict_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= CONFLICT_GAP_MS
            && self.conflict_ms.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
        {
            trace!(0, "arp: {} says it has this machine's address {}", wire::MacHex(*mac), wire::Ipv4(own));
        }
    }

    /// The address of `ip`: from the cache, or by asking for it. None when
    /// nothing answers.
    pub fn resolve(&self, nic: &Nic, ip: u32) -> Option<Mac> {
        /* Nothing a host may not be is asked for: nobody could answer. */
        if !wire::host_address(ip, 0) {
            return None;
        }
        if let Some(mac) = self.lookup(ip) {
            return Some(mac);
        }

        for _ in 0..RESOLVE_TRIES {
            send(nic, ARP_OP_REQUEST, &MAC_BROADCAST, &[0; 6], ip);

            /* The receive softirq is what fills the cache, so this has to
             * sleep rather than spin: a poll that never yields would keep
             * the answer from ever arriving. And a second by the clock, not
             * a thousand sleeps of a millisecond: a sleep ends at its CPU's
             * next scheduling point, which on an idle CPU is the tick, ten
             * milliseconds on -- a thousand of them were ten seconds a try,
             * half a minute for a host that is not there. */
            let deadline = now_ms() + TRY_MS;
            loop {
                if let Some(mac) = self.lookup(ip) {
                    return Some(mac);
                }
                if now_ms() >= deadline {
                    break;
                }
                kcore::task::sleep_ms(1);
            }
        }

        None
    }

    /// The Ethernet address a datagram to `dst` goes to out of `nic`: a
    /// broadcast's or a group's own, or -- asked of ARP, through the gateway
    /// for an address off the subnet -- the host's. None when nothing
    /// answers, and then nothing is sent: handed to the link's broadcast
    /// address instead, it would reach every host there but the one it was
    /// for (RFC 1122 3.3.6), and no router forwards it. Task context: ARP
    /// sleeps while it waits.
    pub fn destination(&self, nic: &Nic, dst: u32) -> Option<Mac> {
        match link_mac(nic, dst) {
            Some(mac) => Some(mac),
            None => self.resolve(nic, nic.route_ip(dst)),
        }
    }

    /// The cache, into `out`, one entry per (ip, mac) pair: how many there
    /// are, and what each one is.
    pub fn snapshot(&self, out: &mut [(u32, Mac)]) -> usize {
        let cache = self.cache.lock();

        let mut at = 0;
        for entry in cache.iter() {
            if entry.valid && at < out.len() {
                out[at] = (entry.ip, entry.mac);
                at += 1;
            }
        }
        at
    }
}

/// The Ethernet address a datagram to `dst` goes to without asking ARP: the
/// link's broadcast for the limited broadcast and for the broadcast of the
/// device's subnet, and a group's own address for a multicast (RFC 1112
/// 6.4). None for a unicast address, which is ARP's to answer.
pub fn link_mac(nic: &Nic, dst: u32) -> Option<Mac> {
    const MULTICAST_PREFIX: [u8; 3] = [0x01, 0x00, 0x5E];
    const GROUP_BITS: u32 = 0x007F_FFFF;

    let (ip, mask) = (nic.ip(), nic.mask());
    if dst == u32::MAX
        || (ip != 0 && mask.count_zeros() >= 2 && dst | mask == u32::MAX && dst & mask == ip & mask)
    {
        return Some(MAC_BROADCAST);
    }
    if dst >> 28 == 0xE {
        let group = (dst & GROUP_BITS).to_be_bytes();
        return Some([MULTICAST_PREFIX[0], MULTICAST_PREFIX[1], MULTICAST_PREFIX[2], group[1], group[2],
                     group[3]]);
    }
    None
}

/// One ARP packet out of the device: a request broadcast, or a reply to the
/// host that asked.
fn send(nic: &Nic, opcode: u16, eth_dst: &Mac, target_mac: &Mac, target_ip: u32) {
    let len = ETH_HDR_LEN + ARP_LEN;
    let mut frame = match Frame::alloc_tx(len) {
        Some(frame) => frame,
        None => {
            trace!(0, "arp: no frame to send with");
            return;
        }
    };

    frame.set_len(len);
    let mac = nic.mac();
    let ip = nic.ip();
    {
        let buf = frame.data_mut();
        buf[..len].fill(0);
        eth::write(buf, eth_dst, &mac, ETH_TYPE_ARP);
        arp::write(&mut buf[ETH_HDR_LEN..], opcode, &mac, ip, target_mac, target_ip);
    }

    if !nic.transmit(frame) {
        trace!(0, "arp: the frame could not be queued");
    }
}

/* Keeps the wire module in use for the accessors above */
pub use wire::ARP_LEN as PACKET_LEN;
