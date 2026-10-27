//! The `virtq` target: the notifications between a virtio driver and its
//! device -- the driver's kicks, the device's interrupts, and with EVENT_IDX
//! each side's word for when it wants to hear of the other's work -- held to
//! what a driver relies on: every request it makes served and heard of,
//! every frame it sends sent, every frame for it received, with no
//! notification but those the protocol has it make, and no round of the
//! device but those the run loop is sure to make. A kick the device needed
//! and did not ask for is a request left on the ring; an interrupt the
//! driver asked for and did not get, a request served that nobody hears of:
//! either is a guest's disk or network stopped for good, with nothing in the
//! host to say so.
//!
//! The driver is Linux's: a split ring kept as `virtio_ring.c` keeps it --
//! the kick decided by the device's word or its flag, callbacks turned off,
//! on and put off, the event moved as it takes what was used, an interrupt
//! remembered so that callbacks need not be turned off again -- and driven as
//! `virtio_blk.c` and `virtio_net.c` drive it: requests handed over in
//! batches and finished in the interrupt, a NAPI poll that takes frames to a
//! budget and refills -- or puts the refill off, the ring run dry -- and a
//! transmit queue stopped when it is full until an interrupt says there is
//! room. The `blk`, `net` and `netrx` targets have drivers that do anything;
//! this one's is well-behaved, with EVENT_IDX and without, so that a request
//! never served is the device's doing.
//!
//! The run loop's rounds are the first CPU's, and it is sure to make one when
//! the disk's backend has served a request (its task rings that CPU), when a
//! frame comes for the guest and its wake is not held back (`quiet_until`),
//! when a receive interrupt is due (`rx_due`), and when a pass ended at its
//! bound (`more`): those, and a notify -- which runs the device at once --
//! are all the device is given here. The CPU's other exits make rounds too,
//! and a held frame is looked at again in each -- but a guest that does not
//! exit has none of them, and a device that waits on them is a guest's
//! network that stalls for as long as it does not.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::devices::blk::{self, Blk};
use crate::devices::net::{self, Net};
use crate::devices::virtio::Raise;
use crate::memory::GuestMemory;
use crate::targets::{Disk, DiskState, Input};
use crate::time;

/// For `HV_FUZZ_STATS`: kicks made and spared, interrupts taken, rings run
/// with EVENT_IDX and without, requests and frames carried, refills put off.
static KICKS: AtomicU64 = AtomicU64::new(0);
static SPARED: AtomicU64 = AtomicU64::new(0);
static INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static EVENT_RUNS: AtomicU64 = AtomicU64::new(0);
static FLAG_RUNS: AtomicU64 = AtomicU64::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static FRAMES_SENT: AtomicU64 = AtomicU64::new(0);
static FRAMES_RECEIVED: AtomicU64 = AtomicU64::new(0);
static DRY: AtomicU64 = AtomicU64::new(0);
static STARVED: AtomicU64 = AtomicU64::new(0);
static LATE: AtomicU64 = AtomicU64::new(0);
static STOPPED: AtomicU64 = AtomicU64::new(0);

pub fn report() {
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    println!("  virtq: {} runs with EVENT_IDX, {} without; {} kicks made, {} spared; {} interrupts taken; \
              {} requests, {} frames sent, {} received; {} refills put off, {} frames come to a ring with no \
              buffers, {} transmit queues stopped; {} chains made available by another CPU while the device worked",
             l(&EVENT_RUNS), l(&FLAG_RUNS), l(&KICKS), l(&SPARED), l(&INTERRUPTS), l(&REQUESTS),
             l(&FRAMES_SENT), l(&FRAMES_RECEIVED), l(&DRY), l(&STARVED), l(&STOPPED), l(&LATE));
}

const MEM: u64 = 8 << 20;
const PAGE: u64 = 4096;
const SIZE: u16 = 256;

/* The legacy header's registers, and with MSI-X on its vector registers. */
const DEVICE_FEATURES: u16 = 0x00;
const DRIVER_FEATURES: u16 = 0x04;
const QUEUE_PFN: u16 = 0x08;
const QUEUE_SELECT: u16 = 0x0E;
const QUEUE_NOTIFY: u16 = 0x10;
const DEVICE_STATUS: u16 = 0x12;
const ISR_STATUS: u16 = 0x13;
const CONFIG_VECTOR: u16 = 0x14;
const QUEUE_VECTOR: u16 = 0x16;
const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;

const F_MAC: u32 = 1 << 5;
const F_MRG_RXBUF: u32 = 1 << 15;
const F_STATUS: u32 = 1 << 16;
const F_EVENT_IDX: u32 = 1 << 29;

const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const AVAIL_NO_INTERRUPT: u16 = 1;
const USED_NO_NOTIFY: u16 = 1;

/// Rounds of the run loop and the driver's work before the machine is
/// called settled: far more than any input here needs.
const SETTLE_STEPS: usize = 4096;

/// VIRTQ_TRACE=1 on a replay: what the driver and the run loop did, step
/// by step.
fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VIRTQ_TRACE").is_some())
}

macro_rules! vt {
    ($($arg:tt)*) => {
        if tracing() {
            eprintln!($($arg)*);
        }
    };
}

/// `vring_need_event`, the model's own: whether moving an index from `old`
/// to `new` passed `event`.
fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

/// One split ring, as Linux's driver keeps it (`struct vring_virtqueue`):
/// each descriptor's buffer a page of its own.
struct Ring {
    base: u64,
    bufs: u64,
    event: bool,
    free: Vec<u16>,
    /// The chains made available and not yet used: their descriptors, by
    /// head.
    chains: HashMap<u16, Vec<u16>>,
    avail_shadow: u16,
    num_added: u16,
    last_used: u16,
    flags_shadow: u16,
    event_triggered: bool,
}

impl Ring {
    fn new(base: u64, bufs: u64, event: bool) -> Ring {
        Ring {
            base,
            bufs,
            event,
            free: (0..SIZE).rev().collect(),
            chains: HashMap::new(),
            avail_shadow: 0,
            num_added: 0,
            last_used: 0,
            flags_shadow: 0,
            event_triggered: false,
        }
    }

    fn avail(&self) -> u64 {
        self.base + 16 * u64::from(SIZE)
    }
    fn used(&self) -> u64 {
        (self.avail() + 6 + 2 * u64::from(SIZE)).next_multiple_of(PAGE)
    }
    fn used_event(&self) -> u64 {
        self.avail() + 4 + 2 * u64::from(SIZE)
    }
    fn avail_event(&self) -> u64 {
        self.used() + 4 + 8 * u64::from(SIZE)
    }
    fn buf(&self, d: u16) -> u64 {
        self.bufs + PAGE * u64::from(d)
    }
    fn word(mem: &GuestMemory, at: u64) -> u16 {
        mem.read_u16(at).expect("the ring in memory")
    }
    fn put(mem: &GuestMemory, at: u64, value: u16) {
        mem.write_u16(at, value).expect("the ring in memory");
    }

    fn num_free(&self) -> usize {
        self.free.len()
    }

    /// `virtqueue_add_split`: a chain of buffers -- (length, the device
    /// writes it) -- made available and its index published; None when the
    /// ring has not descriptors enough. The buffers' addresses come back.
    fn add(&mut self, mem: &GuestMemory, segs: &[(u32, bool)]) -> Option<Vec<u64>> {
        let (head, addrs) = self.prepare(mem, segs)?;
        self.publish(mem, head);
        Some(addrs)
    }

    /// A chain's descriptors written and taken, not yet made available: its
    /// head, and its buffers' addresses.
    fn prepare(&mut self, mem: &GuestMemory, segs: &[(u32, bool)]) -> Option<(u16, Vec<u64>)> {
        if segs.is_empty() || self.free.len() < segs.len() {
            return None;
        }
        let descs: Vec<u16> = (0..segs.len()).map(|_| self.free.pop().expect("counted")).collect();
        let mut addrs = Vec::new();
        for (i, &(len, write)) in segs.iter().enumerate() {
            let last = i + 1 == segs.len();
            let flags = (if write { DESC_WRITE } else { 0 }) | (if last { 0 } else { DESC_NEXT });
            let next = if last { 0 } else { descs[i + 1] };
            let mut e = [0u8; 16];
            e[..8].copy_from_slice(&self.buf(descs[i]).to_le_bytes());
            e[8..12].copy_from_slice(&len.to_le_bytes());
            e[12..14].copy_from_slice(&flags.to_le_bytes());
            e[14..16].copy_from_slice(&next.to_le_bytes());
            mem.write(self.base + 16 * u64::from(descs[i]), &e).expect("the ring in memory");
            addrs.push(self.buf(descs[i]));
        }
        let head = descs[0];
        self.chains.insert(head, descs);
        Some((head, addrs))
    }

    /// The chain at `head` made available: its entry, then the index.
    fn publish(&mut self, mem: &GuestMemory, head: u16) {
        Self::put(mem, self.avail() + 4 + 2 * u64::from(self.avail_shadow % SIZE), head);
        self.avail_shadow = self.avail_shadow.wrapping_add(1);
        Self::put(mem, self.avail() + 2, self.avail_shadow);
        self.num_added = self.num_added.wrapping_add(1);
    }

    /// The chain another CPU made available while the device worked
    /// (`GuestMemory::post_late`), the index on by one; and whether that
    /// driver kicked, by the `avail_event` it read then -- its own decision,
    /// for its own chain.
    fn published_late(&mut self, mem: &GuestMemory, seen: u16) -> bool {
        let idx = Self::word(mem, self.avail() + 2);
        let old = self.avail_shadow;
        invariant!(idx == old.wrapping_add(1), "a late post at index {}, the driver's {}", idx, old);
        self.avail_shadow = idx;
        let kick = if self.event { need_event(seen, idx, old) } else { Self::word(mem, self.used()) & USED_NO_NOTIFY == 0 };
        if kick { &KICKS } else { &SPARED }.fetch_add(1, Ordering::Relaxed);
        LATE.fetch_add(1, Ordering::Relaxed);
        kick
    }

    /// `virtqueue_kick_prepare_split`: whether the device asked to hear of
    /// what was added since the last time.
    fn kick_prepare(&mut self, mem: &GuestMemory) -> bool {
        let new = self.avail_shadow;
        let old = new.wrapping_sub(self.num_added);
        self.num_added = 0;
        let kick = if self.event {
            need_event(Self::word(mem, self.avail_event()), new, old)
        } else {
            Self::word(mem, self.used()) & USED_NO_NOTIFY == 0
        };
        if new != old {
            if kick { &KICKS } else { &SPARED }.fetch_add(1, Ordering::Relaxed);
        }
        kick
    }

    fn more_used(&self, mem: &GuestMemory) -> bool {
        Self::word(mem, self.used() + 2) != self.last_used
    }

    /// `virtqueue_get_buf_ctx_split`: the next chain used -- its head's
    /// buffer, the bytes written -- its descriptors free again; with
    /// callbacks on, the event moved past it.
    fn get_buf(&mut self, mem: &GuestMemory) -> Option<(u64, u32)> {
        if !self.more_used(mem) {
            return None;
        }
        let mut e = [0u8; 8];
        mem.read(self.used() + 4 + 8 * u64::from(self.last_used % SIZE), &mut e).expect("the ring in memory");
        let id = u32::from_le_bytes([e[0], e[1], e[2], e[3]]);
        let len = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
        invariant!(id < u32::from(SIZE), "a used element for descriptor {}, past the ring", id);
        let head = id as u16;
        let descs = self.chains.remove(&head);
        invariant!(descs.is_some(), "a used element for chain {}, which the driver has not made available", head);
        self.free.extend_from_slice(&descs.unwrap_or_default());
        self.last_used = self.last_used.wrapping_add(1);
        if self.flags_shadow & AVAIL_NO_INTERRUPT == 0 {
            Self::put(mem, self.used_event(), self.last_used);
        }
        Some((self.buf(head), len))
    }

    /// `virtqueue_disable_cb_split`: no interrupt wanted -- the event put
    /// where the device will not reach it soon (Linux's 0), or the flag;
    /// neither once the device has interrupted, which it will not do again
    /// until the event moves.
    fn disable_cb(&mut self, mem: &GuestMemory) {
        if self.flags_shadow & AVAIL_NO_INTERRUPT != 0 {
            return;
        }
        self.flags_shadow |= AVAIL_NO_INTERRUPT;
        if self.event_triggered {
            return;
        }
        if self.event {
            Self::put(mem, self.used_event(), 0);
        } else {
            Self::put(mem, self.avail(), self.flags_shadow);
        }
    }

    fn callbacks_on(&mut self, mem: &GuestMemory) {
        self.event_triggered = false;
        if self.flags_shadow & AVAIL_NO_INTERRUPT != 0 {
            self.flags_shadow &= !AVAIL_NO_INTERRUPT;
            if !self.event {
                Self::put(mem, self.avail(), self.flags_shadow);
            }
        }
    }

    /// `virtqueue_enable_cb_prepare` and `virtqueue_poll`: callbacks on, the
    /// event at the next element; true when nothing was used meanwhile, and
    /// false -- the driver looks again -- when something was.
    fn enable_cb(&mut self, mem: &GuestMemory) -> bool {
        self.callbacks_on(mem);
        Self::put(mem, self.used_event(), self.last_used);
        !self.more_used(mem)
    }

    /// `virtqueue_enable_cb_delayed_split`: on, the event three quarters of
    /// the way through what is outstanding; false when that much was used
    /// already.
    fn enable_cb_delayed(&mut self, mem: &GuestMemory) -> bool {
        self.callbacks_on(mem);
        let bufs = (u32::from(self.avail_shadow.wrapping_sub(self.last_used)) * 3 / 4) as u16;
        Self::put(mem, self.used_event(), self.last_used.wrapping_add(bufs));
        Self::word(mem, self.used() + 2).wrapping_sub(self.last_used) <= bufs
    }

    /// `vring_interrupt`: whether there is anything for the callback.
    fn interrupt(&mut self, mem: &GuestMemory) -> bool {
        if !self.more_used(mem) {
            return false;
        }
        if self.event {
            self.event_triggered = true;
        }
        true
    }
}

/// How the device's interrupts reach the driver: the INTx line and its
/// status register; or MSI-X, a vector a queue -- the configuration's 0 --
/// or every queue on one (Linux's when it has not vectors enough).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Irq {
    Intx,
    PerQueue,
    Shared,
}

/// What the driver's CPU has to take: a line up, a vector each.
struct Pending {
    irq: Irq,
    queues: usize,
    intx: bool,
    vectors: u32,
}

impl Pending {
    /// What a call into the device left: its `raise`, and its line as it is
    /// now -- a level, up until the driver reads the status.
    fn note(&mut self, raise: Raise, line: bool) {
        match self.irq {
            Irq::Intx => self.intx |= line,
            _ => self.vectors |= raise.vectors,
        }
    }

    /// The queues whose interrupt the CPU takes now, all of them taken.
    fn take(&mut self) -> Vec<usize> {
        match self.irq {
            Irq::Intx => {
                if !std::mem::replace(&mut self.intx, false) {
                    return Vec::new();
                }
                (0..self.queues).collect()
            }
            Irq::PerQueue => {
                let v = std::mem::replace(&mut self.vectors, 0);
                (0..self.queues).filter(|q| v & (1 << (q + 1)) != 0).collect()
            }
            Irq::Shared => {
                let v = std::mem::replace(&mut self.vectors, 0);
                if v & 2 != 0 { (0..self.queues).collect() } else { Vec::new() }
            }
        }
    }
}

/// The vector a queue was given.
fn vector(irq: Irq, queue: usize) -> u16 {
    match irq {
        Irq::Intx => 0xFFFF,
        Irq::PerQueue => 1 + queue as u16,
        Irq::Shared => 1,
    }
}

/// What a driver writes to bring a device up, as Linux's legacy
/// `virtio_pci` does: status, the features it takes of those `offered`,
/// each queue's page and vector, ready. What it took comes back.
fn bring_up(write: &mut dyn FnMut(u16, u8, u32), offered: u32, accept: u32, pfns: &[u64], irq: Irq) -> u32 {
    write(DEVICE_STATUS, 1, 0);
    write(DEVICE_STATUS, 1, STATUS_ACKNOWLEDGE);
    write(DEVICE_STATUS, 1, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    let features = offered & accept;
    write(DRIVER_FEATURES, 4, features);
    if irq != Irq::Intx {
        write(CONFIG_VECTOR, 2, 0);
    }
    for (q, &pfn) in pfns.iter().enumerate() {
        write(QUEUE_SELECT, 2, q as u32);
        write(QUEUE_PFN, 4, pfn as u32);
        if irq != Irq::Intx {
            write(QUEUE_VECTOR, 2, u32::from(vector(irq, q)));
        }
    }
    write(DEVICE_STATUS, 1, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
    features
}

pub fn virtq_target(r: &mut Input) {
    if r.bool() { blk_run(r) } else { net_run(r) }
}

/* ---- the disk ---- */

const BLK_PFN: u64 = 1;
const BLK_BUFS: u64 = 1 << 20;
const DISK_BYTES: u64 = 1 << 20;
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const S_UNSET: u8 = 0xFF;

struct BlkWorld {
    mem: GuestMemory,
    disk: Blk,
    state: Arc<Mutex<DiskState>>,
    ring: Ring,
    pending: Pending,
    /// The requests made and not yet heard of: their status byte's address,
    /// by their header's.
    outstanding: HashMap<u64, u64>,
    /// A request another CPU makes available while the device works
    /// (`arm_late`), and whether its driver kicked once it did.
    late: bool,
    late_head: u16,
    late_kick: bool,
}

impl BlkWorld {
    /// A call into the device, and what it left the driver's CPU to take --
    /// and the late request, if the device's look at the ring had it made.
    fn note(&mut self, raise: Raise) {
        let line = self.disk.line();
        self.pending.note(raise, line);
        if self.late {
            if let Some(seen) = self.mem.late_posted() {
                self.late = false;
                self.late_kick |= self.ring.published_late(&self.mem, seen);
            }
        }
    }

    /// A request made by another CPU of the guest's, published after the
    /// device's next few looks at the ring -- between its last and its ask,
    /// some of the time -- and kicked for by what that CPU read then.
    fn arm_late(&mut self, r: &mut Input) {
        if self.late {
            return;
        }
        let sector = r.below(DISK_BYTES / blk::SECTOR - 1);
        let Some((head, addrs)) = self.ring.prepare(&self.mem, &[(16, false), (512, true), (1, true)]) else { return };
        let mut h = [0u8; 16];
        h[..4].copy_from_slice(&T_IN.to_le_bytes());
        h[8..].copy_from_slice(&sector.to_le_bytes());
        self.mem.write(addrs[0], &h).expect("a buffer in memory");
        self.mem.write(addrs[2], &[S_UNSET]).expect("a buffer in memory");
        self.outstanding.insert(addrs[0], addrs[2]);
        REQUESTS.fetch_add(1, Ordering::Relaxed);
        self.mem.post_late(self.ring.avail() + 2, self.ring.avail() + 4, self.ring.avail_event(), SIZE, head,
                           r.below(4) as usize);
        self.late = true;
        self.late_head = head;
    }

    /// The late request's kick, when its driver made one; or, still not
    /// made when the run ends, made now as any other.
    fn late_kicks(&mut self, end: bool) {
        if end && self.late && self.mem.cancel_late() {
            self.late = false;
            self.ring.publish(&self.mem, self.late_head);
            self.late_kick |= self.ring.kick_prepare(&self.mem);
        }
        if std::mem::take(&mut self.late_kick) {
            self.kick();
        }
    }

    fn kick(&mut self) {
        let raise = self.disk.io_write(QUEUE_NOTIFY, 2, 0, &self.mem, time::boot_time_ns());
        self.note(raise);
    }

    /// A round of the run loop's: what the backend served given back.
    fn round(&mut self) {
        let raise = self.disk.poll(&self.mem);
        self.note(raise);
    }

    /// `virtio_queue_rq` for each of a batch, `virtio_commit_rqs` at its
    /// end -- or a kick decided after each, as a batch of one has it.
    fn submit(&mut self, r: &mut Input) {
        let each = r.u8() < 64;
        for _ in 0..1 + r.below(12) {
            let kind = r.pick(&[T_IN, T_IN, T_OUT, T_OUT, T_FLUSH]);
            let sectors = 1 + r.below(8);
            let sector = r.below(DISK_BYTES / blk::SECTOR - sectors);
            let mut segs = vec![(16u32, false)];
            if kind != T_FLUSH {
                segs.push(((sectors * blk::SECTOR) as u32, kind == T_IN));
            }
            segs.push((1, true));
            let Some(addrs) = self.ring.add(&self.mem, &segs) else { break };
            let mut h = [0u8; 16];
            h[..4].copy_from_slice(&kind.to_le_bytes());
            h[8..].copy_from_slice(&sector.to_le_bytes());
            self.mem.write(addrs[0], &h).expect("a buffer in memory");
            let status = *addrs.last().expect("a status buffer");
            self.mem.write(status, &[S_UNSET]).expect("a buffer in memory");
            self.outstanding.insert(addrs[0], status);
            REQUESTS.fetch_add(1, Ordering::Relaxed);
            if each && self.ring.kick_prepare(&self.mem) {
                self.kick();
            }
        }
        if self.ring.kick_prepare(&self.mem) {
            self.kick();
        }
    }

    /// The CPU takes its interrupts: `vp_interrupt` reads the status for
    /// the line, and `virtblk_done` takes what was used, callbacks off,
    /// until turning them on finds nothing more.
    fn interrupts(&mut self) -> bool {
        let queues = self.pending.take();
        if self.pending.irq == Irq::Intx && !queues.is_empty() {
            self.disk.io_read(ISR_STATUS, 1);
        }
        let mut any = false;
        for _ in queues {
            INTERRUPTS.fetch_add(1, Ordering::Relaxed);
            if !self.ring.interrupt(&self.mem) {
                continue;
            }
            any = true;
            loop {
                self.ring.disable_cb(&self.mem);
                while let Some((header, _)) = self.ring.get_buf(&self.mem) {
                    /* The request heard of: its status written. */
                    let status = self.outstanding.remove(&header);
                    invariant!(status.is_some(), "a request used twice");
                    let mut b = [0u8];
                    self.mem.read(status.unwrap_or_default(), &mut b).expect("a buffer in memory");
                    invariant!(b[0] <= 2, "a request given back with its status {:#x}", b[0]);
                }
                if self.ring.enable_cb(&self.mem) {
                    break;
                }
            }
        }
        any
    }

    /// Everything that can happen without the driver doing more: the
    /// backend serving all it has, the rounds that brings, the interrupts
    /// taken -- until nothing changes.
    fn settle(&mut self) {
        for _ in 0..SETTLE_STEPS {
            let served = {
                let mut s = self.state.lock().unwrap();
                let n = s.queued();
                s.ready = n;
                n
            };
            if served != 0 || self.disk.more() {
                self.round();
            }
            let took = self.interrupts();
            if served == 0 && !self.disk.more() && !took {
                return;
            }
        }
        panic!("invariant: the disk never settled in {} steps", SETTLE_STEPS);
    }
}

fn blk_run(r: &mut Input) {
    let mem = GuestMemory::with_size(MEM);
    let state = Arc::new(Mutex::new(DiskState::new(DISK_BYTES, false)));
    let Ok(mut disk) = Blk::new(Box::new(Disk(state.clone())), "virtq") else { return };
    let irq = r.pick(&[Irq::Intx, Irq::PerQueue, Irq::Shared]);
    if irq != Irq::Intx {
        disk.offer_msix(2);
        disk.set_msix_enabled(true);
    }
    if r.u8() < 32 {
        mem.refuse_maps(true);
    }
    let accept = if r.bool() { u32::MAX } else { !F_EVENT_IDX };
    let offered = disk.io_read(DEVICE_FEATURES, 4);
    let features = {
        let mut write = |o: u16, s: u8, v: u32| {
            disk.io_write(o, s, v, &mem, time::boot_time_ns());
        };
        bring_up(&mut write, offered, accept, &[BLK_PFN], irq)
    };
    let event = features & F_EVENT_IDX != 0;
    if event { &EVENT_RUNS } else { &FLAG_RUNS }.fetch_add(1, Ordering::Relaxed);
    let mut w = BlkWorld {
        mem,
        disk,
        state,
        ring: Ring::new(BLK_PFN * PAGE, BLK_BUFS, event),
        pending: Pending { irq, queues: 1, intx: false, vectors: 0 },
        outstanding: HashMap::new(),
        late: false,
        late_head: 0,
        late_kick: false,
    };
    while let Some(op) = r.op(7) {
        match op {
            0 | 1 => w.submit(r),
            6 => w.arm_late(r),
            2 => {
                /* The backend serves some of what it has, in any order; the
                 * disk's task rings the first CPU for them. */
                let n = {
                    let mut s = w.state.lock().unwrap();
                    let n = (1 + r.below(4) as usize).min(s.queued());
                    s.ready = n;
                    s.order = r.u64();
                    s.fail = r.u8() % 16 == 0;
                    n
                };
                if n != 0 {
                    w.round();
                }
            }
            3 => {
                w.interrupts();
            }
            4 => w.settle(),
            _ => time::advance(r.pick(&[1_000u64, 50_000, 2_000_000])),
        }
        w.late_kicks(false);
    }
    w.late_kicks(true);
    w.settle();
    held_settled_blk(&mut w);
}

/// After the disk settled: nothing the driver made available is still with
/// the device, the backend or the used ring.
fn held_settled_blk(w: &mut BlkWorld) {
    let used = Ring::word(&w.mem, w.ring.used() + 2);
    invariant!(w.ring.chains.is_empty() && w.outstanding.is_empty(),
               "{} requests never heard of (event {}, {:?}): the device used {} of {} made available, the driver took {} -- {}",
               w.ring.chains.len(), w.ring.event, w.pending.irq, used, w.ring.avail_shadow, w.ring.last_used,
               if used != w.ring.avail_shadow { "a kick the device needed and never asked for" }
               else { "an interrupt the driver asked for and never got" });
}

/* ---- the NIC ---- */

const RX_PFN: u64 = 8;
const TX_PFN: u64 = 16;
const RX_BUFS: u64 = 1 << 20;
const TX_BUFS: u64 = 3 << 20;
const RXQ: usize = 0;
const TXQ: usize = 1;
const HDR: u32 = 10;
const HDR_MRG: u32 = 12;
/// The free descriptors under which the driver stops its transmit queue,
/// and at which it wakes it: 2 + MAX_SKB_FRAGS, the most a send may take.
const TX_ROOM: usize = 2 + 17;

/// The world outside, as the NIC's backend: frames sent counted, frames for
/// the guest waiting, and until when their wakes are held back.
#[derive(Default)]
struct WireState {
    sent: u64,
    rx: VecDeque<Vec<u8>>,
    quiet_until: u64,
}

struct Wire(Arc<Mutex<WireState>>);

impl net::Backend for Wire {
    fn send(&mut self, frame: &[u8]) {
        invariant!(frame.len() <= net::MAX_FRAME, "a frame of {} bytes sent", frame.len());
        self.0.lock().unwrap().sent += 1;
    }
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        let f = self.0.lock().unwrap().rx.pop_front()?;
        let n = f.len().min(buf.len());
        buf[..n].copy_from_slice(&f[..n]);
        Some(n)
    }
    fn quiet_until(&mut self, until_ns: u64) {
        self.0.lock().unwrap().quiet_until = until_ns;
    }
}

struct NetWorld {
    mem: GuestMemory,
    nic: Net,
    wire: Arc<Mutex<WireState>>,
    rx: Ring,
    tx: Ring,
    pending: Pending,
    merged: bool,
    /// The receive buffers' size.
    buf_len: u32,
    /// NAPI's: the receive poll scheduled, and the transmit one.
    napi_rx: bool,
    napi_tx: bool,
    /// A refill the NAPI poll put off, for the refill work to do.
    refill_owed: bool,
    /// Frames handed over to be received, and received; sent, and stopped
    /// for want of room.
    given: u64,
    received: u64,
    xmitted: u64,
    stopped: bool,
    /// When the run loop comes back for the device by itself, as it last
    /// asked (`rx_due`): the loop sleeps to that, or arms its timer for it,
    /// and is not woken past it -- the clock here moves on by every read,
    /// and the device's answer at a later one would not say it any more.
    due: Option<u64>,
    /// A frame or a buffer another CPU makes available while the device
    /// works (`arm_late`): its queue and head; and each queue's kick its
    /// driver made once it did.
    late: Option<(usize, u16)>,
    late_kick: [bool; 2],
}

impl NetWorld {
    /// A call into the device, and what it left: the driver's CPU an
    /// interrupt to take, the run loop a round to come back for.
    fn note(&mut self, raise: Raise) {
        let line = self.nic.line();
        self.pending.note(raise, line);
        self.due = self.nic.rx_due(time::peek());
        if let Some((q, _)) = self.late {
            if let Some(seen) = self.mem.late_posted() {
                self.late = None;
                let ring = if q == RXQ { &mut self.rx } else { &mut self.tx };
                self.late_kick[q] |= ring.published_late(&self.mem, seen);
                if q == TXQ {
                    self.xmitted += 1;
                }
            }
        }
    }

    /// A frame to send, or a buffer to receive into, made by another CPU of
    /// the guest's and published after the device's next few looks at its
    /// ring -- between its last and its ask, some of the time -- and kicked
    /// for by what that CPU read then. Not on a stopped transmit queue,
    /// which the stack hands no frame.
    fn arm_late(&mut self, r: &mut Input) {
        if self.late.is_some() {
            return;
        }
        let q = if r.bool() { RXQ } else { TXQ };
        let hdr = if self.merged { HDR_MRG } else { HDR };
        let prepared = if q == RXQ {
            self.rx.prepare(&self.mem, &[(self.buf_len, true)])
        } else if !self.stopped {
            self.tx.prepare(&self.mem, &[(hdr, false), (60 + r.below(1455) as u32, false)])
        } else {
            None
        };
        let Some((head, _)) = prepared else { return };
        let ring = if q == RXQ { &self.rx } else { &self.tx };
        self.mem.post_late(ring.avail() + 2, ring.avail() + 4, ring.avail_event(), SIZE, head, r.below(4) as usize);
        self.late = Some((q, head));
    }

    /// The late chain's kick, when its driver made one; or, still not made
    /// when the run ends, made now as any other.
    fn late_kicks(&mut self, end: bool) {
        if end {
            if let Some((q, head)) = self.late.take() {
                if self.mem.cancel_late() {
                    let ring = if q == RXQ { &mut self.rx } else { &mut self.tx };
                    ring.publish(&self.mem, head);
                    self.late_kick[q] |= ring.kick_prepare(&self.mem);
                    if q == TXQ {
                        self.xmitted += 1;
                    }
                }
            }
        }
        for q in [RXQ, TXQ] {
            if std::mem::take(&mut self.late_kick[q]) {
                self.kick(q);
            }
        }
    }

    /// Whether the round the device asked for is due by now.
    fn round_due(&self) -> bool {
        self.nic.more() || self.due.is_some_and(|due| due <= time::peek())
    }

    fn kick(&mut self, queue: usize) {
        let raise = self.nic.io_write(QUEUE_NOTIFY, 2, queue as u32, &self.mem, time::boot_time_ns());
        self.note(raise);
        vt!("kick {}: {}", queue, self.state());
    }

    fn round(&mut self) {
        let raise = self.nic.poll(&self.mem, time::boot_time_ns());
        self.note(raise);
        vt!("round: {}", self.state());
    }

    /// For the trace: where each ring and the backend stand.
    fn state(&self) -> String {
        let w = self.wire.lock().unwrap();
        format!("t={} rx avail {} used {} taken {} avail_event {} used_event {} flags {} | tx avail {} used {} | \
                 backend {} quiet_until {} | pending intx {} vectors {:#x} | napi rx {} tx {}",
                time::peek(), self.rx.avail_shadow, Ring::word(&self.mem, self.rx.used() + 2), self.rx.last_used,
                Ring::word(&self.mem, self.rx.avail_event()), Ring::word(&self.mem, self.rx.used_event()),
                Ring::word(&self.mem, self.rx.avail()), self.tx.avail_shadow, Ring::word(&self.mem, self.tx.used() + 2),
                w.rx.len(), w.quiet_until, self.pending.intx, self.pending.vectors, self.napi_rx, self.napi_tx)
    }

    /// `try_fill_recv`: buffers onto every free descriptor -- or `most` of
    /// them, the rest failing for want of memory, which leaves the refill
    /// work owed, as Linux schedules it -- and the kick the device asked
    /// for.
    fn fill(&mut self, most: usize) {
        let mut added = 0;
        while added < most && self.rx.add(&self.mem, &[(self.buf_len, true)]).is_some() {
            added += 1;
        }
        if added == most && self.rx.num_free() != 0 {
            self.refill_owed = true;
        }
        if added != 0 && self.rx.kick_prepare(&self.mem) {
            self.kick(RXQ);
        }
    }

    /// Frames for the guest from the world: their wake rings the first CPU
    /// unless the device has it held back, its round coming at `rx_due`.
    fn arrive(&mut self, n: u64, r: &mut Input) {
        if self.rx.chains.is_empty() {
            STARVED.fetch_add(1, Ordering::Relaxed);
        }
        for _ in 0..n {
            let len = 60 + r.below(1455) as usize;
            self.wire.lock().unwrap().rx.push_back(vec![0x5A; len]);
            self.given += 1;
        }
        let now = time::boot_time_ns();
        let quiet_until = self.wire.lock().unwrap().quiet_until;
        vt!("arrive {} (quiet until {})", n, quiet_until);
        if quiet_until <= now {
            self.round();
        } else {
            /* Waking nobody: the device has to have a round coming for them
             * by the end of its quiet. */
            invariant!(self.due.is_some_and(|due| due <= quiet_until),
                       "frames that wake nobody until {}, and the device's round due {:?}", quiet_until, self.due);
        }
    }

    /// `start_xmit` with NAPI's transmit completion: what was sent taken
    /// back first, callbacks off; the frame added; the queue stopped when it
    /// could not take another, with callbacks put off to three quarters of
    /// what is out; the kick decided at the end of a batch.
    fn xmit(&mut self, r: &mut Input) {
        /* A batch of a few, mostly; now and then one that fills the ring
         * before its kick, and stops the queue. */
        let most = if r.u8() < 24 { 160 } else { 8 };
        let batch = 1 + r.below(most);
        for i in 0..batch {
            if self.stopped {
                break;
            }
            let last = i + 1 == batch;
            /* What was sent taken back first; at a batch's end callbacks put
             * off to three quarters of what is out, and taken back again
             * when that much is in already. */
            loop {
                self.tx.disable_cb(&self.mem);
                self.free_tx();
                if !last || self.tx.enable_cb_delayed(&self.mem) {
                    break;
                }
            }
            let hdr = if self.merged { HDR_MRG } else { HDR };
            let len = 60 + r.below(1455) as u32;
            let added = self.tx.add(&self.mem, &[(hdr, false), (len, false)]);
            invariant!(added.is_some(), "a transmit queue not stopped with no room on its ring");
            self.xmitted += 1;
            if self.tx.num_free() < TX_ROOM {
                self.stopped = true;
                STOPPED.fetch_add(1, Ordering::Relaxed);
                if !self.tx.enable_cb_delayed(&self.mem) {
                    self.tx.disable_cb(&self.mem);
                    self.napi_tx = true;
                }
            }
            if (last || self.stopped) && self.tx.kick_prepare(&self.mem) {
                self.kick(TXQ);
            }
        }
    }

    fn free_tx(&mut self) {
        while let Some((_, len)) = self.tx.get_buf(&self.mem) {
            invariant!(len == 0, "a transmit chain given back with {} bytes written", len);
            FRAMES_SENT.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The CPU takes its interrupts: the receive queue's schedules its NAPI
    /// poll, callbacks off (`skb_recv_done`); the transmit queue's, its own
    /// (`skb_xmit_done`).
    fn interrupts(&mut self) -> bool {
        let queues = self.pending.take();
        if self.pending.irq == Irq::Intx && !queues.is_empty() {
            self.nic.io_read(ISR_STATUS, 1);
        }
        let mut any = false;
        for q in queues {
            INTERRUPTS.fetch_add(1, Ordering::Relaxed);
            let ring = if q == RXQ { &mut self.rx } else { &mut self.tx };
            if !ring.interrupt(&self.mem) {
                continue;
            }
            any = true;
            ring.disable_cb(&self.mem);
            if q == RXQ { self.napi_rx = true } else { self.napi_tx = true }
        }
        any
    }

    /// `virtnet_poll`: frames taken to the budget -- a frame's every buffer,
    /// as many as its header says -- the ring refilled once it is half
    /// empty, or the refill put off; and under the budget, callbacks on
    /// again, or the poll goes on when something came meanwhile.
    fn napi_rx_poll(&mut self, budget: u64, put_off: bool) -> bool {
        if !self.napi_rx {
            return false;
        }
        vt!("napi rx budget {} put_off {}: {}", budget, put_off, self.state());
        let mut got = 0u64;
        while got < budget {
            let Some((buf, len)) = self.rx.get_buf(&self.mem) else { break };
            let hdr = if self.merged { HDR_MRG } else { HDR };
            invariant!(len >= hdr, "a frame of {} bytes, under the header's {}", len, hdr);
            if self.merged {
                let mut b = [0u8; 2];
                self.mem.read(buf + u64::from(HDR), &mut b).expect("a buffer in memory");
                let n = u16::from_le_bytes(b);
                invariant!(n >= 1, "a frame in {} buffers", n);
                for _ in 1..n {
                    invariant!(self.rx.get_buf(&self.mem).is_some(), "a frame's header says {} buffers, fewer were used", n);
                }
            }
            got += 1;
            self.received += 1;
            FRAMES_RECEIVED.fetch_add(1, Ordering::Relaxed);
        }
        if self.rx.num_free() > (budget.min(u64::from(SIZE)) / 2) as usize {
            if put_off {
                self.refill_owed = true;
                DRY.fetch_add(1, Ordering::Relaxed);
            } else {
                self.fill(usize::MAX);
            }
        }
        if got < budget {
            if self.rx.enable_cb(&self.mem) {
                self.napi_rx = false;
            } else {
                self.rx.disable_cb(&self.mem);
            }
        }
        true
    }

    /// `virtnet_poll_tx`: what was sent taken back, the queue woken when
    /// there is room, callbacks on -- or the poll goes on.
    fn napi_tx_poll(&mut self) -> bool {
        if !self.napi_tx {
            return false;
        }
        self.tx.disable_cb(&self.mem);
        self.free_tx();
        if self.stopped && self.tx.num_free() >= TX_ROOM {
            self.stopped = false;
        }
        if self.tx.enable_cb(&self.mem) {
            self.napi_tx = false;
        } else {
            self.tx.disable_cb(&self.mem);
        }
        true
    }

    /// The refill work, which comes when the NAPI poll put a refill off --
    /// and fills the ring, or `most` of it.
    fn refill_work(&mut self, most: usize) -> bool {
        if !std::mem::replace(&mut self.refill_owed, false) {
            return false;
        }
        vt!("refill work");
        self.fill(most);
        true
    }

    /// Everything that can happen with no more frames either way: the
    /// rounds the run loop is sure to make -- a pass at its bound, a
    /// receive interrupt due -- the interrupts taken and the NAPI polls and
    /// the refill work they bring, until nothing changes.
    fn settle(&mut self) {
        for _ in 0..SETTLE_STEPS {
            let now = time::peek();
            let mut busy = false;
            if self.nic.more() {
                self.round();
                busy = true;
            } else if let Some(due) = self.due {
                if due > now {
                    time::advance(due - now);
                }
                self.round();
                busy = true;
            }
            busy |= self.interrupts();
            busy |= self.napi_rx_poll(64, false);
            busy |= self.napi_tx_poll();
            busy |= self.refill_work(usize::MAX);
            if !busy {
                return;
            }
        }
        panic!("invariant: the NIC never settled in {} steps", SETTLE_STEPS);
    }
}

fn net_run(r: &mut Input) {
    let mem = GuestMemory::with_size(MEM);
    let wire = Arc::new(Mutex::new(WireState::default()));
    let Ok(mut nic) = Net::new(Box::new(Wire(wire.clone())), [2, 0, 0, 0, 0x64, 9]) else { return };
    if r.u8() < 160 {
        nic.set_rx_moderation(r.pick(&[1_000u64, 20_000, 100_000]));
    }
    let irq = r.pick(&[Irq::Intx, Irq::PerQueue, Irq::Shared]);
    if irq != Irq::Intx {
        nic.offer_msix(3);
        nic.set_msix_enabled(true);
    }
    if r.u8() < 32 {
        mem.refuse_maps(true);
    }
    /* Its MAC and link status, and of the rest EVENT_IDX and mergeable
     * buffers, or not: no offload, whose frames are the `offload` and
     * `netrx` targets'. */
    let mut accept = F_MAC | F_STATUS;
    if r.bool() {
        accept |= F_EVENT_IDX;
    }
    if r.bool() {
        accept |= F_MRG_RXBUF;
    }
    let offered = nic.io_read(DEVICE_FEATURES, 4);
    let features = {
        let mut write = |o: u16, s: u8, v: u32| {
            nic.io_write(o, s, v, &mem, time::boot_time_ns());
        };
        bring_up(&mut write, offered, accept, &[RX_PFN, TX_PFN], irq)
    };
    let event = features & F_EVENT_IDX != 0;
    let merged = features & F_MRG_RXBUF != 0;
    if event { &EVENT_RUNS } else { &FLAG_RUNS }.fetch_add(1, Ordering::Relaxed);
    /* Mergeable buffers of any size a frame may span; one buffer a frame
     * without them, the header and the longest frame in it. */
    let buf_len = if merged { r.pick(&[512u32, 1536, 4096]) } else { r.pick(&[1536u32, 4096]) };
    let mut w = NetWorld {
        mem,
        nic,
        wire,
        rx: Ring::new(RX_PFN * PAGE, RX_BUFS, event),
        tx: Ring::new(TX_PFN * PAGE, TX_BUFS, event),
        pending: Pending { irq, queues: 2, intx: false, vectors: 0 },
        merged,
        buf_len,
        napi_rx: false,
        napi_tx: false,
        refill_owed: false,
        given: 0,
        received: 0,
        xmitted: 0,
        stopped: false,
        due: None,
        late: None,
        late_kick: [false; 2],
    };
    /* `virtnet_open`: the receive ring filled, callbacks on. */
    let most = if r.u8() < 48 { r.below(8) as usize } else { usize::MAX };
    w.fill(most);
    w.rx.enable_cb(&w.mem);
    while let Some(op) = r.op(10) {
        match op {
            9 => w.arm_late(r),
            0 | 1 => {
                let most = if r.bool() { 4 } else { 64 };
                let n = 1 + r.below(most);
                w.arrive(n, r);
            }
            2 | 3 => w.xmit(r),
            4 => {
                w.interrupts();
            }
            5 => {
                let budget = r.pick(&[1u64, 4, 64]);
                let put_off = r.u8() < 64;
                w.napi_rx_poll(budget, put_off);
                w.napi_tx_poll();
            }
            6 => {
                let most = if r.bool() { r.below(4) as usize } else { usize::MAX };
                w.refill_work(most);
            }
            7 => w.settle(),
            _ => {
                /* Time going by, and the round it made due on the way: the
                 * run loop sleeps to that, and is not woken past it. */
                time::advance(r.pick(&[1_000u64, 20_000, 200_000]));
                if w.round_due() {
                    w.round();
                }
            }
        }
        w.late_kicks(false);
    }
    w.late_kicks(true);
    w.settle();
    held_settled_net(&mut w);
}

/// After the NIC settled: every frame sent, and taken back once the queue
/// stopped; every frame for the guest received; nothing left with the
/// device.
fn held_settled_net(w: &mut NetWorld) {
    let sent = w.wire.lock().unwrap().sent;
    let tx_used = Ring::word(&w.mem, w.tx.used() + 2);
    invariant!(sent == w.xmitted,
               "{} frames sent of {} (event {}, {:?}): the device used {} of {} made available -- {}",
               sent, w.xmitted, w.tx.event, w.pending.irq, tx_used, w.tx.avail_shadow,
               "a kick the device needed and never asked for");
    invariant!(!w.stopped, "the transmit queue stopped for good (event {}, {:?}): {} of {} used, the driver took {} -- \
                            an interrupt the driver asked for and never got",
               w.tx.event, w.pending.irq, tx_used, w.tx.avail_shadow, w.tx.last_used);
    let waiting = w.wire.lock().unwrap().rx.len();
    let rx_used = Ring::word(&w.mem, w.rx.used() + 2);
    invariant!(w.received == w.given,
               "{} frames received of {} (event {}, {:?}, merged {}, buffers of {}): {} still with the backend; the device \
                used {} of {} made available, the driver took {} -- {}",
               w.received, w.given, w.rx.event, w.pending.irq, w.merged, w.buf_len, waiting, rx_used,
               w.rx.avail_shadow, w.rx.last_used,
               if rx_used != w.rx.last_used { "an interrupt the driver asked for and never got" }
               else { "a kick the device needed and never asked for" });
}
