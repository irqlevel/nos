//! The `netrx` target: what a guest's NIC puts into the buffers its driver
//! posts (`hv/src/devices/net.rs`), held to a model of its own -- a frame
//! into one chain, or, once the driver takes mergeable buffers, spread over
//! as many as it needs with their count in its header; a send of TCP carried
//! whole, with the header that says how to cut it. The driver here is a
//! well-behaved one -- the `net` target has the rest -- so that every used
//! element can be held to what the model says: which chain, how many bytes,
//! and the bytes themselves.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// What went into the buffers, for `HV_FUZZ_STATS`: frames into one chain,
/// spread over mergeable ones, sends carried whole, the most chains one
/// frame took, and frames dropped for want of room.
static ONE: AtomicU64 = AtomicU64::new(0);
static MERGED: AtomicU64 = AtomicU64::new(0);
static WHOLE: AtomicU64 = AtomicU64::new(0);
static MOST: AtomicU64 = AtomicU64::new(0);
static SHORT: AtomicU64 = AtomicU64::new(0);

pub fn report() {
    println!("  netrx: {} frames into one chain, {} spread over mergeable buffers, {} sends whole, {} chains at most for one, {} without room",
             ONE.load(Ordering::Relaxed), MERGED.load(Ordering::Relaxed), WHOLE.load(Ordering::Relaxed),
             MOST.load(Ordering::Relaxed), SHORT.load(Ordering::Relaxed));
}

use crate::devices::net::{self, Net};
use crate::devices::offload::{self, Partial, Rx, Takes, Tcp4, TxHeader};
use crate::memory::GuestMemory;
use crate::targets::{Input, Nic, NicState};

const MEM: u64 = 4 << 20;
const PAGE: u64 = 4096;
const QUEUE_SIZE: u64 = 256;
const RX_PFN: u64 = 1;
/// Where the buffers start, past the ring's three pages.
const BUFFERS: u64 = 0x10000;

/// The registers and features the driver touches.
const DEVICE_FEATURES: u16 = 0x00;
const DRIVER_FEATURES: u16 = 0x04;
const QUEUE_PFN: u16 = 0x08;
const QUEUE_SELECT: u16 = 0x0E;
const F_GUEST_CSUM: u32 = 1 << 1;
const F_GUEST_TSO4: u32 = 1 << 7;
const F_MRG_RXBUF: u32 = 1 << 15;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const MAX_MERGE: usize = 64;
const PASS: usize = QUEUE_SIZE as usize;

/// A chain the driver posted and the device has not used: its head and
/// its buffers.
struct Chain {
    head: u16,
    bufs: Vec<(u64, u32)>,
}

impl Chain {
    fn room(&self) -> u64 {
        self.bufs.iter().map(|b| u64::from(b.1)).sum()
    }
}

/// The driver: its ring, and where it is in it.
struct Driver {
    next_desc: u64,
    avail_idx: u16,
    used_seen: u16,
    next_buf: u64,
    posted: VecDeque<Chain>,
}

impl Driver {
    fn desc(i: u64) -> u64 {
        RX_PFN * PAGE + 16 * (i % QUEUE_SIZE)
    }
    fn avail() -> u64 {
        RX_PFN * PAGE + 16 * QUEUE_SIZE
    }
    fn used() -> u64 {
        (Self::avail() + 6 + 2 * QUEUE_SIZE).next_multiple_of(PAGE)
    }

    /// A chain of `sizes` writable buffers posted, if the ring has room.
    fn post(&mut self, mem: &GuestMemory, sizes: &[u32]) {
        let in_flight: usize = self.posted.iter().map(|c| c.bufs.len()).sum();
        if in_flight + sizes.len() > QUEUE_SIZE as usize || self.posted.len() >= QUEUE_SIZE as usize {
            return;
        }
        let head = (self.next_desc % QUEUE_SIZE) as u16;
        let mut bufs = Vec::new();
        for (i, &len) in sizes.iter().enumerate() {
            if self.next_buf + u64::from(len) > MEM {
                self.next_buf = BUFFERS;
            }
            let addr = self.next_buf;
            self.next_buf += u64::from(len).next_multiple_of(64);
            let last = i + 1 == sizes.len();
            let flags = DESC_WRITE | if last { 0 } else { DESC_NEXT };
            let next = ((self.next_desc + 1) % QUEUE_SIZE) as u16;
            let mut d = [0u8; 16];
            d[..8].copy_from_slice(&addr.to_le_bytes());
            d[8..12].copy_from_slice(&len.to_le_bytes());
            d[12..14].copy_from_slice(&flags.to_le_bytes());
            d[14..16].copy_from_slice(&next.to_le_bytes());
            mem.write(Self::desc(self.next_desc), &d).expect("the ring in memory");
            self.next_desc += 1;
            bufs.push((addr, len));
        }
        let slot = u64::from(self.avail_idx) % QUEUE_SIZE;
        mem.write(Self::avail() + 4 + 2 * slot, &head.to_le_bytes()).expect("the ring in memory");
        self.avail_idx = self.avail_idx.wrapping_add(1);
        mem.write(Self::avail() + 2, &self.avail_idx.to_le_bytes()).expect("the ring in memory");
        self.posted.push_back(Chain { head, bufs });
    }

    /// The used elements the device added since the last look.
    fn take_used(&mut self, mem: &GuestMemory) -> Vec<(u16, u32)> {
        let mut b = [0u8; 2];
        mem.read(Self::used() + 2, &mut b).expect("the ring in memory");
        let idx = u16::from_le_bytes(b);
        let mut out = Vec::new();
        while self.used_seen != idx {
            let slot = u64::from(self.used_seen) % QUEUE_SIZE;
            let mut e = [0u8; 8];
            mem.read(Self::used() + 4 + 8 * slot, &mut e).expect("the ring in memory");
            out.push((u32::from_le_bytes([e[0], e[1], e[2], e[3]]) as u16, u32::from_le_bytes([e[4], e[5], e[6], e[7]])));
            self.used_seen = self.used_seen.wrapping_add(1);
        }
        out
    }
}

/// What the model says one element of the used ring is: the chain, the
/// bytes in it.
struct Expect {
    chain: Chain,
    bytes: Vec<u8>,
}

/// The device's round, as the model has it: what waits goes in, frame by
/// frame, until one has no buffers enough yet.
fn model_round(pending: &mut VecDeque<(Vec<u8>, Rx)>, posted: &mut VecDeque<Chain>, merged: bool,
               takes: Takes) -> Vec<Expect> {
    let hdr = if merged { TxHeader::LEN + 2 } else { TxHeader::LEN };
    let mut out = Vec::new();
    for _ in 0..PASS {
        let Some((raw, rx)) = pending.front() else { break };
        let ok = match rx {
            Rx::Done | Rx::Partial(_) => (14..=net::MAX_FRAME).contains(&raw.len()),
            Rx::Whole(t) => takes.whole && raw.len() >= t.headers() && raw.len() <= offload::MAX_SEND,
        };
        if !ok {
            pending.pop_front();
            continue;
        }
        /* A checksum left to finish goes so to a driver that takes them;
         * for one that does not it is finished on the way, by the model's
         * own sum here. */
        let mut frame = raw.clone();
        let header = match rx {
            Rx::Partial(p) if !takes.partial => {
                let h = p.header();
                crate::offloadmodel::finish(&mut frame, usize::from(h.csum_start), usize::from(h.csum_offset));
                TxHeader::default()
            }
            rx => rx.header(),
        };
        let layout = match rx {
            Rx::Whole(t) => Some(*t),
            _ => None,
        };
        let frame = &frame;
        let need = (hdr + frame.len()) as u64;
        if merged {
            let mut room = 0u64;
            let mut n = 0usize;
            for c in posted.iter().take(MAX_MERGE) {
                room += c.room();
                n += 1;
                if room >= need {
                    break;
                }
            }
            if room < need {
                if posted.len() >= MAX_MERGE {
                    SHORT.fetch_add(1, Ordering::Relaxed);
                    pending.pop_front();
                    continue;
                }
                break;
            }
            MERGED.fetch_add(1, Ordering::Relaxed);
            MOST.fetch_max(n as u64, Ordering::Relaxed);
            if layout.is_some() {
                WHOLE.fetch_add(1, Ordering::Relaxed);
            }
            let mut data = header.bytes().to_vec();
            data.extend_from_slice(&(n as u16).to_le_bytes());
            data.extend_from_slice(frame);
            let mut at = 0usize;
            for _ in 0..n {
                let c = posted.pop_front().expect("the chains counted");
                let take = (c.room() as usize).min(data.len() - at);
                out.push(Expect { chain: c, bytes: data[at..at + take].to_vec() });
                at += take;
            }
        } else {
            let Some(c) = posted.pop_front() else { break };
            let bytes = if c.room() >= need {
                ONE.fetch_add(1, Ordering::Relaxed);
                if layout.is_some() {
                    WHOLE.fetch_add(1, Ordering::Relaxed);
                }
                let mut data = header.bytes().to_vec();
                data.extend_from_slice(frame);
                data
            } else {
                SHORT.fetch_add(1, Ordering::Relaxed);
                Vec::new()
            };
            out.push(Expect { chain: c, bytes });
        }
        pending.pop_front();
    }
    out
}

/// A send of TCP over IPv4 that holds up, `payload` bytes, to be cut by
/// `mss`: the frame and how.
fn tcp_send(r: &mut Input, payload: usize, mss: usize) -> Option<(Vec<u8>, Tcp4)> {
    let frame = crate::offloadmodel::build(r, 6, 20, 32, payload, 0x0800, 0x4000, 0x18, true);
    let layout = offload::check_tcp4(&frame, mss).ok()?;
    Some((frame, layout))
}

pub fn netrx_target(r: &mut Input) {
    let mem = GuestMemory::with_size(MEM);
    let state = Arc::new(Mutex::new(NicState::default()));
    let Ok(mut nic) = Net::new(Box::new(Nic(state.clone())), [2, 0, 0, 0, 0x64, 7]) else { return };
    let now = crate::time::boot_time_ns();

    /* The features, as a driver accepts them: mergeable buffers or not,
     * sends taken whole or not, from what is offered. */
    let offered = nic.io_read(DEVICE_FEATURES, 4);
    let mut accept = offered & r.pick(&[u32::MAX, u32::MAX, !F_MRG_RXBUF, !(F_GUEST_TSO4 | F_GUEST_CSUM),
                                         !(F_MRG_RXBUF | F_GUEST_TSO4), !F_GUEST_CSUM]);
    if r.u8() < 32 {
        accept &= r.u32();
    }
    nic.io_write(DRIVER_FEATURES, 4, accept, &mem, now);
    nic.io_write(QUEUE_SELECT, 2, 0, &mem, now);
    nic.io_write(QUEUE_PFN, 4, RX_PFN as u32, &mem, now);
    let merged = accept & F_MRG_RXBUF != 0;
    let takes = Takes {
        partial: accept & F_GUEST_CSUM != 0,
        whole: accept & (F_GUEST_TSO4 | F_GUEST_CSUM) == F_GUEST_TSO4 | F_GUEST_CSUM,
    };

    let mut driver = Driver { next_desc: 0, avail_idx: 0, used_seen: 0, next_buf: BUFFERS, posted: VecDeque::new() };
    let mut pending: VecDeque<(Vec<u8>, Rx)> = VecDeque::new();

    while let Some(op) = r.op(6) {
        match op {
            0 => {
                /* Buffers: Linux's mergeable ones, one of a size each, or a
                 * chain of pages for a send whole without them. */
                let n = 1 + r.below(8);
                for _ in 0..n {
                    let sizes: Vec<u32> = if r.u8() < 220 {
                        vec![r.pick(&[1526u32, 1536, 2048, 4096, 64, 12, 13])]
                    } else {
                        vec![4096; 1 + r.below(18) as usize]
                    };
                    driver.post(&mem, &sizes);
                }
            }
            1 => {
                /* A frame, of a length a frame has, mostly. */
                let len = if r.u8() < 240 { 14 + r.below(1501) as usize } else { r.pick(&[0usize, 13, 1515, 4000]) };
                let frame: Vec<u8> = (0..len).map(|i| (i as u8) ^ r.u8()).collect();
                state.lock().unwrap().rx.push_back((frame.clone(), Rx::Done));
                pending.push_back((frame, Rx::Done));
            }
            2 => {
                /* A send of TCP carried whole. */
                let payload = r.pick(&[0usize, 100, 1448, 3000, 30_000, 64_000]);
                let mss = r.pick(&[256usize, 536, 1448]);
                if let Some((frame, layout)) = tcp_send(r, payload, mss) {
                    state.lock().unwrap().rx.push_back((frame.clone(), Rx::Whole(layout)));
                    pending.push_back((frame, Rx::Whole(layout)));
                }
            }
            3 => {
                /* A frame whose checksum its sender left to finish: TCP or
                 * UDP, its place given right. */
                let proto = r.pick(&[6u8, 17]);
                let payload = r.below(1400) as usize;
                let frame = crate::offloadmodel::build(r, proto, 20, 20, payload, 0x0800, 0x4000, 0x18, true);
                let offset = if proto == 17 { 6 } else { 16 };
                if let Ok(p) = Partial::check(frame.len(), 34, offset) {
                    state.lock().unwrap().rx.push_back((frame.clone(), Rx::Partial(p)));
                    pending.push_back((frame, Rx::Partial(p)));
                }
            }
            _ => {
                /* A round of the device, and what it put in the buffers held
                 * to the model's. */
                nic.poll(&mem, crate::time::boot_time_ns());
                let told = state.lock().unwrap().takes;
                invariant!(told == takes, "the backend told the guest takes {:?}, its driver {:?}", told, takes);
                let want = model_round(&mut pending, &mut driver.posted, merged, takes);
                let got = driver.take_used(&mem);
                invariant!(got.len() == want.len(), "{} used elements, the model {} (merged {}, takes {:?})",
                           got.len(), want.len(), merged, takes);
                for (i, ((id, len), w)) in got.iter().zip(want.iter()).enumerate() {
                    invariant!(*id == w.chain.head && *len as usize == w.bytes.len(),
                               "used element {}: chain {} with {} bytes, the model chain {} with {}", i, id, len,
                               w.chain.head, w.bytes.len());
                    let mut bytes = Vec::new();
                    for &(addr, blen) in &w.chain.bufs {
                        let take = (blen as usize).min(w.bytes.len() - bytes.len());
                        let mut b = vec![0u8; take];
                        mem.read(addr, &mut b).expect("a buffer in memory");
                        bytes.extend_from_slice(&b);
                        if bytes.len() == w.bytes.len() {
                            break;
                        }
                    }
                    invariant!(bytes == w.bytes, "used element {}'s bytes are not the model's", i);
                }
            }
        }
    }
}
