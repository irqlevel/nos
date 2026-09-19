//! DNS: names to addresses, with a cache that honours the TTL it was given.
//!
//! One query is in flight at a time -- a mutex serialises callers -- and an
//! answer is accepted only when it comes from the configured resolver, on the
//! DNS port, carrying the transaction ID that was asked with. The ID alone is
//! weak protection against an off-path forgery, so it is not the only check,
//! and it is seeded from the cycle counter rather than counting 1, 2, 3.

use crate::nic::{Lent, Nic, RxContext, UdpHandler, UdpListener};
use kcore::sync::{Mutex, SpinLock};
use kcore::time;
use kcore::trace;

use crate::abi;
use crate::udp;

pub const SERVER_PORT: u16 = 53;
/// The source port queries go out from, fixed rather than ephemeral.
pub const CLIENT_PORT: u16 = 10053;

pub const DEFAULT_TIMEOUT_MS: u64 = 3000;
const POLL_INTERVAL_MS: u64 = 10;

pub const MAX_DOMAIN_LEN: usize = 253;
const MAX_PACKET_LEN: usize = 512;
const HEADER_LEN: usize = 12;

const FLAG_RD: u16 = 0x0100; /* recursion desired */
const FLAG_QR: u16 = 0x8000; /* this is a response */
const RCODE_MASK: u16 = 0x000F;
const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;
const A_RECORD_LEN: u16 = 4;
/// A name compression pointer, in the top two bits of a label length
const COMPRESS_FLAG: u8 = 0xC0;

const CACHE_SIZE: usize = 32;
/// However long a server says its record is good for, a day is the most this
/// cache will hold it.
const MAX_TTL_SEC: u64 = 86_400;

struct Entry {
    name: [u8; MAX_DOMAIN_LEN + 1],
    len: usize,
    ip: u32,
    expires_ms: u64,
    valid: bool,
}

struct Pending {
    id: u16,
    answered: bool,
    result: u32,
    ttl_sec: u64,
}

/// The cache, the query in flight, and who is being asked: taken from the
/// receive softirq as well as from a resolving task.
struct State {
    cache: [Entry; CACHE_SIZE],
    pending: Pending,
    next_id: u16,
    nic: Option<Nic>,
    server_ip: u32,
}

pub struct Dns {
    state: SpinLock<State>,
    /// One resolution at a time: there is one pending slot.
    resolving: Mutex<()>,
    /// Kept for as long as the resolver runs, which is for good.
    listener: SpinLock<Option<UdpListener>>,
    ready: core::sync::atomic::AtomicBool,
}

fn now_ms() -> u64 {
    time::boot_time().as_nanos() / kcore::consts::NS_PER_MS
}

fn be16(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([buf[off], buf[off + 1]])
}

impl Dns {
    pub fn new() -> Option<Dns> {
        const NOTHING: Entry = Entry {
            name: [0; MAX_DOMAIN_LEN + 1], len: 0, ip: 0, expires_ms: 0, valid: false,
        };

        Some(Dns {
            state: SpinLock::new(State {
                cache: [NOTHING; CACHE_SIZE],
                pending: Pending { id: 0, answered: false, result: 0, ttl_sec: 0 },
                next_id: 1,
                nic: None,
                server_ip: 0,
            })?,
            resolving: Mutex::new(())?,
            listener: SpinLock::new(None)?,
            ready: core::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(core::sync::atomic::Ordering::Acquire)
    }

    /// Start resolving through `server_ip` on `nic`. False when the port is
    /// taken or the server address is nothing.
    pub fn start(&'static self, nic: Nic, server_ip: u32) -> bool {
        if server_ip == 0 || self.is_ready() {
            return false;
        }

        /* Seeded from the entropy pool, so that the transaction IDs are not
         * predictably 1, 2, 3 -- which, with the source checks in `receive`,
         * is what an off-path forgery would have to get past. The C++ side
         * seeded this from the cycle counter; the pool is the better source
         * and by this point in boot it has been fed. */
        {
            let mut state = self.state.lock();
            state.next_id = (kcore::random::random_u64().unwrap_or(1) as u16) | 1;
            state.nic = Some(nic);
            state.server_ip = server_ip;
        }

        let listener = match nic.listen(CLIENT_PORT, self) {
            Ok(listener) => listener,
            Err(err) => {
                trace!(0, "dns: port {} could not be listened on ({:?})", CLIENT_PORT, err);
                return false;
            }
        };
        *self.listener.lock() = Some(listener);

        self.ready.store(true, core::sync::atomic::Ordering::Release);
        trace!(0, "dns: resolver started, server {}.{}.{}.{}",
            (server_ip >> 24) & 0xFF, (server_ip >> 16) & 0xFF,
            (server_ip >> 8) & 0xFF, server_ip & 0xFF);
        true
    }

    /* ---- the cache ---- */

    fn lookup(&self, name: &[u8]) -> Option<u32> {
        let mut state = self.state.lock();
        let now = now_ms();

        for entry in state.cache.iter_mut() {
            if !entry.valid || &entry.name[..entry.len] != name {
                continue;
            }
            if now >= entry.expires_ms {
                /* The record's time is up: ask again */
                entry.valid = false;
                return None;
            }
            return Some(entry.ip);
        }
        None
    }

    fn insert(&self, name: &[u8], ip: u32, ttl_sec: u64) {
        if name.len() > MAX_DOMAIN_LEN {
            return;
        }
        let ttl_sec = ttl_sec.min(MAX_TTL_SEC);

        let mut state = self.state.lock();
        let cache = &mut state.cache;
        let expires_ms = now_ms() + ttl_sec * 1000;

        let mut at = None;
        for (index, entry) in cache.iter().enumerate() {
            if entry.valid && &entry.name[..entry.len] == name {
                at = Some(index);
                break;
            }
            if !entry.valid && at.is_none() {
                at = Some(index);
            }
        }

        /* Nothing free and nothing matching: the first entry makes way */
        let entry = &mut cache[at.unwrap_or(0)];
        entry.name[..name.len()].copy_from_slice(name);
        entry.len = name.len();
        entry.ip = ip;
        entry.expires_ms = expires_ms;
        entry.valid = true;
    }

    pub fn flush(&self) {
        for entry in self.state.lock().cache.iter_mut() {
            entry.valid = false;
        }
    }

    /// The cache into `out`, as (name length, name, ip); how many there are.
    pub fn snapshot(&self, out: &mut [([u8; MAX_DOMAIN_LEN + 1], usize, u32)]) -> usize {
        let state = self.state.lock();

        let mut at = 0;
        for entry in state.cache.iter() {
            if entry.valid && at < out.len() {
                out[at] = (entry.name, entry.len, entry.ip);
                at += 1;
            }
        }
        at
    }

    /* ---- queries ---- */

    /// A query for `name`, out to the server.
    fn send_query(&self, name: &[u8], id: u16) -> bool {
        let (nic, server) = {
            let state = self.state.lock();
            (state.nic, state.server_ip)
        };
        let nic = match nic {
            Some(nic) => nic,
            None => return false,
        };

        let mut packet = [0u8; MAX_PACKET_LEN];
        packet[0..2].copy_from_slice(&id.to_be_bytes());
        packet[2..4].copy_from_slice(&FLAG_RD.to_be_bytes());
        packet[4..6].copy_from_slice(&1u16.to_be_bytes()); /* one question */

        let mut at = HEADER_LEN;
        let encoded = match encode_name(name, &mut packet[at..]) {
            Some(len) => len,
            None => return false,
        };
        at += encoded;

        if at + 4 > packet.len() {
            return false;
        }
        packet[at..at + 2].copy_from_slice(&TYPE_A.to_be_bytes());
        packet[at + 2..at + 4].copy_from_slice(&CLASS_IN.to_be_bytes());
        at += 4;

        let arp = match abi::arp_table() {
            Some(arp) => arp,
            None => return false,
        };
        udp::send(&nic, arp, server, SERVER_PORT, nic.ip(), CLIENT_PORT, &packet[..at])
    }

    /// A datagram on the client port, from the receive softirq.
    fn receive(&self, frame: &[u8]) {
        let datagram = match udp::parse(frame) {
            Some(datagram) => datagram,
            None => return,
        };

        /* Only what came from the resolver we asked, on the port we asked it
         * on. Without this an off-path host could inject an answer, and the
         * transaction ID alone is not much of a gate. */
        let server = self.state.lock().server_ip;
        if datagram.src_ip != server || datagram.src_port != SERVER_PORT {
            return;
        }

        self.process_response(datagram.payload);
    }

    fn process_response(&self, packet: &[u8]) {
        if packet.len() < HEADER_LEN {
            return;
        }

        let id = be16(packet, 0);
        let flags = be16(packet, 2);
        let questions = be16(packet, 4);
        let answers = be16(packet, 6);

        if flags & FLAG_QR == 0 {
            return;
        }
        if flags & RCODE_MASK != 0 {
            trace!(0, "dns: the server answered id {} with rcode {}", id, flags & RCODE_MASK);
            return;
        }

        /* The pending id is read under the lock that guards the writes below:
         * this runs in the softirq while a task is in `resolve`. */
        if id != self.state.lock().pending.id {
            return;
        }
        if answers == 0 {
            return;
        }

        /* Past the questions */
        let mut at = HEADER_LEN;
        for _ in 0..questions {
            at = match skip_name(packet, at) {
                Some(at) => at + 4, /* type and class */
                None => return,
            };
            if at > packet.len() {
                return;
            }
        }

        /* The first A record of the answers is the one taken */
        for _ in 0..answers {
            at = match skip_name(packet, at) {
                Some(at) => at,
                None => return,
            };

            /* type, class, ttl and the record's length */
            if at + 10 > packet.len() {
                return;
            }
            let kind = be16(packet, at);
            let class = be16(packet, at + 2);
            let ttl = u32::from_be_bytes([
                packet[at + 4], packet[at + 5], packet[at + 6], packet[at + 7]]);
            let rd_len = be16(packet, at + 8);
            at += 10;

            if at + rd_len as usize > packet.len() {
                return;
            }

            if kind == TYPE_A && class == CLASS_IN && rd_len == A_RECORD_LEN {
                let ip = u32::from_be_bytes([
                    packet[at], packet[at + 1], packet[at + 2], packet[at + 3]]);

                let mut state = self.state.lock();
                let pending = &mut state.pending;
                /* The id again under the lock: the resolver may have moved on
                 * to another query since the check above. */
                if id == pending.id && !pending.answered {
                    pending.result = ip;
                    pending.ttl_sec = ttl as u64;
                    pending.answered = true;
                }
                return;
            }

            at += rd_len as usize;
        }
    }

    /// The address of `name`: from the cache, or by asking. None once the
    /// timeout passes with no answer.
    pub fn resolve(&self, name: &[u8], timeout_ms: u64) -> Option<u32> {
        if !self.is_ready() || name.is_empty() || name.len() > MAX_DOMAIN_LEN {
            return None;
        }

        if let Some(ip) = self.lookup(name) {
            return Some(ip);
        }

        let _one_at_a_time = self.resolving.lock();

        /* Another caller may have resolved it while this one waited */
        if let Some(ip) = self.lookup(name) {
            return Some(ip);
        }

        let id = {
            let mut state = self.state.lock();
            let id = state.next_id;
            state.next_id = id.wrapping_add(1);
            state.pending.id = id;
            state.pending.answered = false;
            id
        };

        if !self.send_query(name, id) {
            trace!(0, "dns: the query could not be sent");
            return None;
        }

        let mut waited = 0;
        while waited < timeout_ms {
            kcore::task::sleep_ms(POLL_INTERVAL_MS);
            waited += POLL_INTERVAL_MS;

            let (answered, ip, ttl_sec) = {
                let state = self.state.lock();
                (state.pending.answered, state.pending.result, state.pending.ttl_sec)
            };

            if answered {
                /* RFC 1035: a TTL of zero means do not cache it */
                if ttl_sec > 0 {
                    self.insert(name, ip, ttl_sec);
                }
                trace!(0, "dns: resolved to {}.{}.{}.{}",
                    (ip >> 24) & 0xFF, (ip >> 16) & 0xFF, (ip >> 8) & 0xFF, ip & 0xFF);
                return Some(ip);
            }
        }

        trace!(0, "dns: nothing answered within {} ms", timeout_ms);
        None
    }
}

/// What the receive path hands every datagram on the client port to.
impl UdpHandler for Dns {
    fn on_frame(&'static self, frame: Lent<'_>, _rx: &mut RxContext) {
        self.receive(frame.bytes());
    }
}

/// A name in the form the wire takes it: `\3www\7example\3com\0`. None when
/// the name is empty, over-long, or has a label that is.
fn encode_name(name: &[u8], out: &mut [u8]) -> Option<usize> {
    if name.is_empty() || name.len() > MAX_DOMAIN_LEN || out.len() < name.len() + 2 {
        return None;
    }

    let mut at = 0;
    for label in name.split(|b| *b == b'.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        out[at] = label.len() as u8;
        out[at + 1..at + 1 + label.len()].copy_from_slice(label);
        at += 1 + label.len();
    }

    out[at] = 0; /* the root label */
    Some(at + 1)
}

/// Past the name at `at`, whether it is spelt out or a pointer into what came
/// before. None when it runs off the end.
fn skip_name(packet: &[u8], at: usize) -> Option<usize> {
    let mut at = at;

    while at < packet.len() {
        let len = packet[at];

        if len == 0 {
            return Some(at + 1);
        }
        if len & COMPRESS_FLAG == COMPRESS_FLAG {
            /* A pointer is two bytes and ends the name */
            return if at + 1 < packet.len() { Some(at + 2) } else { None };
        }
        at += 1 + len as usize;
    }

    None
}
