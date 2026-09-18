//! ARP: the cache that says which Ethernet address an IP address is at, and
//! the requests and replies that fill it.
//!
//! Entries expire, so a host that changes its MAC is resolved again rather
//! than talked to at an address that has moved. A resolution that is not
//! cached sends a request and waits for the receive path to answer it,
//! retransmitting once a second so that one dropped broadcast does not cost
//! the whole timeout.

use kcore::net::{NetFrame, Nic};
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

#[derive(Clone, Copy)]
struct Entry {
    ip: u32,
    mac: Mac,
    made_ms: u64,
    valid: bool,
}

pub struct ArpTable {
    lock: SpinLock,
    cache: core::cell::UnsafeCell<[Entry; CACHE_SIZE]>,
}

/* Everything inside is touched with the lock held */
unsafe impl Sync for ArpTable {}
unsafe impl Send for ArpTable {}

fn now_ms() -> u64 {
    time::boot_time().as_nanos() / kcore::consts::NS_PER_MS
}

impl ArpTable {
    pub fn new() -> Option<ArpTable> {
        Some(ArpTable {
            lock: SpinLock::new()?,
            cache: core::cell::UnsafeCell::new(
                [Entry { ip: 0, mac: [0; 6], made_ms: 0, valid: false }; CACHE_SIZE]),
        })
    }

    /// The address of `ip`, if it is cached and has not expired.
    pub fn lookup(&self, ip: u32) -> Option<Mac> {
        let _guard = self.lock.lock();
        let cache = unsafe { &mut *self.cache.get() };
        let now = now_ms();

        for entry in cache.iter_mut() {
            if !entry.valid || entry.ip != ip {
                continue;
            }
            if now.saturating_sub(entry.made_ms) >= TTL_MS {
                /* Expired: resolve it again rather than use it */
                entry.valid = false;
                return None;
            }
            return Some(entry.mac);
        }
        None
    }

    pub fn insert(&self, ip: u32, mac: &Mac) {
        let _guard = self.lock.lock();
        let cache = unsafe { &mut *self.cache.get() };
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

        /* Full: the first entry makes way */
        cache[0] = Entry { ip, mac: *mac, made_ms: now, valid: true };
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
        let _guard = self.lock.lock();
        let cache = unsafe { &*self.cache.get() };

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
    let mut frame = match NetFrame::alloc_tx(len) {
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
