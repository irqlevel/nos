//! ARP: the cache that says which Ethernet address an IP address is at, and
//! the requests and replies that fill it.
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

use crate::wire::{self, arp, eth, Mac, ARP_LEN, ETH_HDR_LEN, ETH_TYPE_ARP,
                  ARP_OP_REPLY, ARP_OP_REQUEST, MAC_BROADCAST};

const CACHE_SIZE: usize = 16;

/// How long an entry is good for. A host that changes its MAC is resolved
/// again after this.
const TTL_MS: u64 = 300_000;

/// A resolution sends this many requests, a second apart.
const RESOLVE_TRIES: u32 = 3;
const POLL_PER_TRY_MS: u64 = 1000;

/// What `lookup_or_ask` sends goes out at most this often, whatever is
/// behind it: a guest that floods an address nothing answers for is not a
/// flood of broadcasts.
const ASK_GAP_MS: u64 = 100;
/// An entry this close to its end is asked for again by `lookup_or_ask`
/// while it is still used, so that a flow through it never meets the gap
/// between the entry expiring and the answer coming back.
const REFRESH_MS: u64 = 30_000;

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

    pub fn insert(&self, ip: u32, mac: &Mac) {
        let mut cache = self.cache.lock();
        let now = now_ms();

        for entry in cache.iter_mut() {
            if entry.valid && entry.ip == ip {
                entry.mac = *mac;
                entry.made_ms = now;
                return;
            }
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

    /// What the receive path hands over: a request for this machine is
    /// answered, and either kind teaches the cache where the sender is.
    pub fn process(&self, nic: &Nic, frame: &[u8]) {
        if frame.len() < ETH_HDR_LEN + ARP_LEN {
            return;
        }

        let packet = &frame[ETH_HDR_LEN..];
        let opcode = arp::opcode(packet);
        let sender_mac = arp::sender_mac(packet);
        let sender_ip = arp::sender_ip(packet);

        let answer = match opcode {
            ARP_OP_REQUEST => {
                self.insert(sender_ip, &sender_mac);
                arp::target_ip(packet) == nic.ip()
            }
            ARP_OP_REPLY => {
                self.insert(sender_ip, &sender_mac);
                false
            }
            _ => false,
        };

        /* The reply goes out with no lock held */
        if answer {
            send(nic, ARP_OP_REPLY, &sender_mac, &sender_mac, sender_ip);
        }
    }

    /// The address of `ip`: from the cache, or by asking for it. None when
    /// nothing answers.
    pub fn resolve(&self, nic: &Nic, ip: u32) -> Option<Mac> {
        if let Some(mac) = self.lookup(ip) {
            return Some(mac);
        }

        for _ in 0..RESOLVE_TRIES {
            send(nic, ARP_OP_REQUEST, &MAC_BROADCAST, &[0; 6], ip);

            /* The receive softirq is what fills the cache, so this has to
             * sleep rather than spin: a poll that never yields would keep
             * the answer from ever arriving. */
            for _ in 0..POLL_PER_TRY_MS {
                if let Some(mac) = self.lookup(ip) {
                    return Some(mac);
                }
                kcore::task::sleep_ms(1);
            }
        }

        None
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
