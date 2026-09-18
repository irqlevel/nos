//! The network devices: the queues between a driver and the stack, the UDP
//! listeners, the receive dispatch, and the table of them all.
//!
//! A driver registers an ops table and is then only asked two things: empty
//! the transmit queue into the hardware, and harvest the hardware into the
//! receive queue. Everything between -- the queueing, the batching, the
//! protocol dispatch and the counters -- is here.
//!
//! Three things are the way they are because a profile said so, and they are
//! worth not undoing:
//!
//! - The receive queue is **spliced whole** and dispatched with the lock
//!   down. Taking and releasing it per frame, with interrupts off, was the
//!   top of the receive path at thirty-seven thousand packets a second.
//! - The listener table is **copied once per batch**, not looked up per
//!   datagram, and a callback runs with the lock down. Under the lock it
//!   held the NIC's own interrupt back for the length of every callback.
//! - The per-protocol counters are **per CPU and plain**. Shared atomics on
//!   the datapath are what all of this exists to remove.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use kcore::consts::MAX_CPUS;
use kcore::sync::{IrqSpinLock, PreemptSpinLock};
use kcore::trace;

use crate::frame::{self, NetFrame};
use crate::wire::{eth, ip, udp, Mac, ETH_HDR_LEN, ETH_TYPE_ARP, ETH_TYPE_IP,
                  IP_HDR_LEN, IP_PROTO_ICMP, IP_PROTO_TCP, IP_PROTO_UDP, UDP_HDR_LEN};

pub const MAX_DEVICES: usize = 16;
const NAME_MAX: usize = 32;

const TX_CAPACITY: usize = 256;
const RX_CAPACITY: usize = 256;

/// DHCP, DNS and the shell take three at boot; every block server takes one
/// more.
pub const MAX_LISTENERS: usize = 16;

/// What `listen_udp` answers.
pub const LISTEN_OK: i32 = 0;
pub const LISTEN_PORT_TAKEN: i32 = 1;
pub const LISTEN_TABLE_FULL: i32 = 2;
pub const LISTEN_INVALID: i32 = 3;

/* ---- a queue of frames ---- */

/// Frames threaded through their own links. The device only ever adds at the
/// end, takes from the front, or moves the lot, so a queue with a tail
/// pointer does everything the doubly-linked list it replaces did -- and can
/// live in a `static`, which a self-referential circular list cannot.
struct FrameQueue {
    head: *mut NetFrame,
    tail: *mut NetFrame,
    count: usize,
}

impl FrameQueue {
    const fn new() -> FrameQueue {
        FrameQueue { head: core::ptr::null_mut(), tail: core::ptr::null_mut(), count: 0 }
    }

    fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    /// # Safety
    /// `frame` is a live frame on no other queue.
    unsafe fn push(&mut self, frame: *mut NetFrame) {
        unsafe { (*frame).link.flink = core::ptr::null_mut() };
        if self.tail.is_null() {
            self.head = frame;
        } else {
            unsafe { (*self.tail).link.flink = frame as *mut frame::ListEntry };
        }
        self.tail = frame;
        self.count += 1;
    }

    fn pop(&mut self) -> *mut NetFrame {
        let frame = self.head;
        if frame.is_null() {
            return frame;
        }

        self.head = unsafe { (*frame).link.flink } as *mut NetFrame;
        if self.head.is_null() {
            self.tail = core::ptr::null_mut();
        }
        self.count -= 1;
        unsafe { (*frame).link.flink = core::ptr::null_mut() };
        frame
    }

    /// Everything, in one move, leaving this one empty.
    fn take(&mut self) -> FrameQueue {
        let taken = FrameQueue { head: self.head, tail: self.tail, count: self.count };
        self.head = core::ptr::null_mut();
        self.tail = core::ptr::null_mut();
        self.count = 0;
        taken
    }
}

/* ---- what a driver gives the stack ---- */

/// The ops a driver registers. `ffi::net::NetDeviceOps` is the same struct.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DeviceOps {
    pub name: *const u8,
    pub mac: [u8; 6],
    /// Empty the transmit queue into the hardware. Called with the transmit
    /// lock held, so it must not sleep, allocate or free.
    pub flush_tx: extern "C" fn(ctx: *mut u8),
    /// Harvest the hardware into the receive queue.
    pub process_rx: extern "C" fn(ctx: *mut u8),
    pub ctx: *mut u8,
}

/* ---- listeners ---- */

/// What the receive path hands a datagram to.
#[derive(Clone, Copy)]
struct Listener {
    port: u16,
    /// The frame itself, lent for the call
    frame_cb: Option<extern "C" fn(ctx: *mut u8, frame: usize)>,
    /// Called at the end of a receive batch, for a listener that answers
    /// from the receive path and hands its replies over together
    batch_end_cb: Option<extern "C" fn(ctx: *mut u8)>,
    ctx: *mut u8,
}

const NO_LISTENER: Listener = Listener {
    port: 0, frame_cb: None, batch_end_cb: None, ctx: core::ptr::null_mut(),
};

/* ---- counters ----
 *
 * Per CPU and plain, not shared and atomic. This is a datapath, and a
 * counter every arriving packet increments on one cache line is the thing
 * this layer has spent its history removing. The CPU is read once per batch,
 * not once per frame. */

#[repr(align(64))]
#[derive(Clone, Copy)]
struct RxCounters {
    icmp: usize,
    udp: usize,
    tcp: usize,
    arp: usize,
    other: usize,
    drop: usize,
}

#[repr(align(64))]
#[derive(Clone, Copy)]
struct TxCounters {
    icmp: usize,
    udp: usize,
    tcp: usize,
    arp: usize,
    other: usize,
    total: usize,
}

const NO_RX: RxCounters = RxCounters { icmp: 0, udp: 0, tcp: 0, arp: 0, other: 0, drop: 0 };
const NO_TX: TxCounters = TxCounters { icmp: 0, udp: 0, tcp: 0, arp: 0, other: 0, total: 0 };

/* ---- a device ---- */

pub struct Device {
    used: AtomicBool,
    ops: core::cell::UnsafeCell<Option<DeviceOps>>,
    name: core::cell::UnsafeCell<[u8; NAME_MAX]>,
    name_len: core::cell::UnsafeCell<usize>,
    mac: core::cell::UnsafeCell<Mac>,

    ip: AtomicU32,
    mask: AtomicU32,
    gw: AtomicU32,

    /// Taken from a driver's interrupt handler, so interrupts go off with it
    tx_lock: IrqSpinLock,
    tx_queue: core::cell::UnsafeCell<FrameQueue>,
    /// Transmitted frames waiting to be released off the lock
    tx_done: core::cell::UnsafeCell<FrameQueue>,

    rx_lock: IrqSpinLock,
    rx_queue: core::cell::UnsafeCell<FrameQueue>,

    /// Guards the listener table; never taken from a hard interrupt
    listener_lock: PreemptSpinLock,
    listeners: core::cell::UnsafeCell<[Listener; MAX_LISTENERS]>,
    listener_count: core::cell::UnsafeCell<usize>,
    /// Callbacks running right now. An unlisten waits for this to drain, so
    /// that what the callback reaches may be freed after it returns.
    listener_in_flight: AtomicUsize,

    rx_proto: core::cell::UnsafeCell<[RxCounters; MAX_CPUS]>,
    tx_proto: core::cell::UnsafeCell<[TxCounters; MAX_CPUS]>,
    tx_packets: AtomicUsize,
    rx_packets: AtomicUsize,
}

unsafe impl Sync for Device {}
unsafe impl Send for Device {}

impl Device {
    const fn new() -> Device {
        Device {
            used: AtomicBool::new(false),
            ops: core::cell::UnsafeCell::new(None),
            name: core::cell::UnsafeCell::new([0; NAME_MAX]),
            name_len: core::cell::UnsafeCell::new(0),
            mac: core::cell::UnsafeCell::new([0; 6]),
            ip: AtomicU32::new(0),
            mask: AtomicU32::new(0),
            gw: AtomicU32::new(0),
            tx_lock: IrqSpinLock::new(),
            tx_queue: core::cell::UnsafeCell::new(FrameQueue::new()),
            tx_done: core::cell::UnsafeCell::new(FrameQueue::new()),
            rx_lock: IrqSpinLock::new(),
            rx_queue: core::cell::UnsafeCell::new(FrameQueue::new()),
            listener_lock: PreemptSpinLock::new(),
            listeners: core::cell::UnsafeCell::new([NO_LISTENER; MAX_LISTENERS]),
            listener_count: core::cell::UnsafeCell::new(0),
            listener_in_flight: AtomicUsize::new(0),
            rx_proto: core::cell::UnsafeCell::new([NO_RX; MAX_CPUS]),
            tx_proto: core::cell::UnsafeCell::new([NO_TX; MAX_CPUS]),
            tx_packets: AtomicUsize::new(0),
            rx_packets: AtomicUsize::new(0),
        }
    }

    pub fn name(&self) -> &[u8] {
        let len = unsafe { *self.name_len.get() };
        unsafe { &(&*self.name.get())[..len] }
    }

    pub fn mac(&self) -> Mac {
        unsafe { *self.mac.get() }
    }

    pub fn ip(&self) -> u32 {
        self.ip.load(Ordering::Acquire)
    }

    pub fn set_ip(&self, ip: u32) {
        self.ip.store(ip, Ordering::Release);
    }

    pub fn set_mask(&self, mask: u32) {
        self.mask.store(mask, Ordering::Release);
    }

    pub fn set_gw(&self, gw: u32) {
        self.gw.store(gw, Ordering::Release);
    }

    /// What to ask ARP for to reach `dst`: the gateway when it is off the
    /// subnet, `dst` itself when it is on it.
    pub fn route_ip(&self, dst: u32) -> u32 {
        let (mask, gw) = (self.mask.load(Ordering::Acquire), self.gw.load(Ordering::Acquire));
        if mask != 0 && gw != 0 && (dst & mask) != (self.ip() & mask) {
            gw
        } else {
            dst
        }
    }

    fn ops(&self) -> Option<DeviceOps> {
        unsafe { *self.ops.get() }
    }

    fn cpu_slot(&self) -> usize {
        (kcore::cpu::id() as usize).min(MAX_CPUS - 1)
    }
}

/* ---- transmitting ---- */

impl Device {
    /// A frame the driver has finished with, for release once the transmit
    /// lock is down.
    ///
    /// A driver must **never** release a transmitted frame from inside
    /// `flush_tx`. That runs under the transmit lock with interrupts off, and
    /// a release reaches the page allocator, which shoots down the TLB on
    /// every other CPU and waits for each to answer -- and a CPU spinning on
    /// this lock has interrupts off, so it never can. The two then wait for
    /// each other for good.
    pub fn tx_done(&self, frame: *mut NetFrame) {
        unsafe { (*self.tx_done.get()).push(frame) };
    }

    /// The finished frames, released with the lock down and interrupts on.
    pub fn release_tx_done(&self) {
        loop {
            let flags = self.tx_lock.lock_flags();
            let frame = unsafe { (*self.tx_done.get()).pop() };
            unsafe { self.tx_lock.unlock_flags(flags) };

            if frame.is_null() {
                return;
            }
            /* Outside the lock, always: see `tx_done` */
            unsafe { frame::put(frame) };
        }
    }

    /// Queue frames and ring the doorbell once for the lot. Takes every
    /// frame; answers how many were queued, the rest released.
    pub fn submit_tx_batch(&self, frames: &[*mut NetFrame]) -> usize {
        if frames.is_empty() {
            return 0;
        }

        /* Before the lock: this is bookkeeping, and the section below is the
         * narrowest one on the transmit path. */
        for &frame in frames {
            self.count_tx_frame(frame);
        }

        /* A panic report has to leave through here, and the lock it needs
         * may be held by a CPU that is never going to release it -- which is
         * precisely the failure a panic most often reports. Blocking here
         * means the report is never written, which is how a deadlocked
         * transmit path produces a machine that dies in silence.
         *
         * So once a panic has begun, take the lock if it is free and go on
         * without it if it is not. Going on without it can race the holder
         * into the driver's ring; on a machine that is already dying, a
         * corrupted ring costs nothing and the report is worth everything. */
        let panicking = unsafe { ffi::panic::kernel_panic_active() } != 0;
        let (flags, acquired) = if panicking {
            self.tx_lock.try_lock_flags()
        } else {
            (self.tx_lock.lock_flags(), true)
        };

        let mut queued = 0;
        {
            let queue = unsafe { &mut *self.tx_queue.get() };
            while queued < frames.len() && queue.count < TX_CAPACITY {
                unsafe { queue.push(frames[queued]) };
                queued += 1;
            }
        }

        if queued != 0 {
            if let Some(ops) = self.ops() {
                (ops.flush_tx)(ops.ctx);
            }
        }

        /* No room for the rest: released along with what the driver
         * finishes. */
        if acquired {
            for &frame in frames.iter().skip(queued) {
                self.tx_done(frame);
            }
            unsafe { self.tx_lock.unlock_flags(flags) };
        } else {
            unsafe { kcore::cpu::irq_restore(flags) };
            for &frame in frames.iter().skip(queued) {
                unsafe { frame::put(frame) };
            }
        }

        /* Off the lock, always -- and only with interrupts on. A frame from
         * the allocator goes back through a free whose TLB shootdown waits
         * for every other CPU to answer, and a caller with interrupts off
         * cannot answer one itself: two such CPUs would wait on each other
         * for good. That caller leaves the release to the transmit softirq. */
        if kcore::cpu::interrupts_enabled() {
            self.release_tx_done();
        } else {
            kcore::softirq::raise(kcore::softirq::TYPE_NET_TX);
        }

        queued
    }

    /// One queued frame, for a driver inside its own `flush_tx` -- the lock
    /// is already held there. Null when the queue is empty.
    pub fn tx_dequeue(&self) -> *mut NetFrame {
        let frame = unsafe { (*self.tx_queue.get()).pop() };
        if !frame.is_null() {
            self.tx_packets.fetch_add(1, Ordering::Relaxed);
        }
        frame
    }

    /// The transmit softirq: what a driver with nothing else to do owes.
    pub fn drain_tx(&self) {
        let flags = self.tx_lock.lock_flags();
        let pending = unsafe { !(*self.tx_queue.get()).is_empty() };
        if pending {
            if let Some(ops) = self.ops() {
                (ops.flush_tx)(ops.ctx);
            }
        }
        unsafe { self.tx_lock.unlock_flags(flags) };

        self.release_tx_done();
    }

    /// A frame the caller built whole, copied into one of the device's and
    /// queued.
    pub fn send_raw(&self, data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }

        let frame = frame::alloc_tx(data.len());
        if frame.is_null() {
            return false;
        }

        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), (*frame).data, data.len());
            (*frame).len = data.len();
        }
        self.submit_tx_batch(&[frame]) == 1
    }

    /// One outgoing frame, counted by what it carries. Called where every
    /// driver's frames leave, so a driver with no classifier of its own is
    /// counted too.
    fn count_tx_frame(&self, frame: *mut NetFrame) {
        let counters = unsafe { &mut (*self.tx_proto.get())[self.cpu_slot()] };
        counters.total += 1;

        let len = unsafe { (*frame).len };
        let data = unsafe { core::slice::from_raw_parts((*frame).data, len) };
        if len < ETH_HDR_LEN {
            counters.other += 1;
            return;
        }

        match eth::ether_type(data) {
            ETH_TYPE_ARP => counters.arp += 1,
            ETH_TYPE_IP if len >= ETH_HDR_LEN + IP_HDR_LEN => {
                match ip::protocol(&data[ETH_HDR_LEN..]) {
                    IP_PROTO_ICMP => counters.icmp += 1,
                    IP_PROTO_TCP => counters.tcp += 1,
                    IP_PROTO_UDP => counters.udp += 1,
                    _ => counters.other += 1,
                }
            }
            _ => counters.other += 1,
        }
    }
}

/* ---- receiving ---- */

impl Device {
    /// A harvested frame. False when the queue is full, and the caller then
    /// releases it.
    pub fn enqueue_rx(&self, frame: *mut NetFrame) -> bool {
        let flags = self.rx_lock.lock_flags();
        let queue = unsafe { &mut *self.rx_queue.get() };
        let room = queue.count < RX_CAPACITY;
        if room {
            unsafe { queue.push(frame) };
        }
        unsafe { self.rx_lock.unlock_flags(flags) };
        room
    }

    /// A whole harvest, under one acquisition rather than one per frame.
    /// Answers how many were taken; the caller releases the rest.
    pub fn enqueue_rx_batch(&self, frames: &[*mut NetFrame]) -> usize {
        let flags = self.rx_lock.lock_flags();
        let queue = unsafe { &mut *self.rx_queue.get() };

        let mut taken = 0;
        while taken < frames.len() && queue.count < RX_CAPACITY {
            unsafe { queue.push(frames[taken]) };
            taken += 1;
        }
        unsafe { self.rx_lock.unlock_flags(flags) };
        taken
    }

    /// What the hardware has waiting, as a hint for the receive poll -- read
    /// without the lock, because it is a hint and not an invariant.
    pub fn rx_pending(&self) -> usize {
        unsafe { (*self.rx_queue.get()).count }
    }

    /// Take the whole receive queue and dispatch it to the protocols.
    ///
    /// The queue is spliced in one go and dispatched with the lock down.
    /// Frames arrive one at a time but leave in a run, and taking the lock
    /// per frame -- with interrupts off -- was the top of the receive path in
    /// a profile at thirty-seven thousand packets a second.
    pub fn drain_rx_and_dispatch(&'static self) {
        let batch = {
            let flags = self.rx_lock.lock_flags();
            let taken = unsafe { (*self.rx_queue.get()).take() };
            unsafe { self.rx_lock.unlock_flags(flags) };
            taken
        };

        /* Nothing arrived: no table to copy, no hold to take, no lock. The
         * drain runs on every softirq pass, most of which have no frames. */
        if batch.is_empty() {
            return;
        }
        let mut batch = batch;

        /* Read once for the batch: the counters are per CPU, and the poll
         * that produced this batch does not migrate part way through it. */
        let slot = self.cpu_slot();

        /* One look at the listener table for the whole batch. It was an
         * acquire and release per datagram -- with interrupts off, plus a
         * pair of atomics on the in-flight count -- which a profile put among
         * the top entries of the receive path. The table has a few slots and
         * changes when a server starts or stops, so copying it per batch
         * costs nothing and the copy is good for the length of one.
         *
         * The in-flight count is raised once for the batch and dropped at the
         * end, which is what lets an unlisten wait for callbacks to finish
         * before its caller frees their context. */
        let mut listeners = [NO_LISTENER; MAX_LISTENERS];
        let count = {
            self.listener_lock.lock();
            let count = unsafe { *self.listener_count.get() };
            listeners[..count]
                .copy_from_slice(unsafe { &(&*self.listeners.get())[..count] });
            if count != 0 {
                self.listener_in_flight.fetch_add(1, Ordering::AcqRel);
            }
            unsafe { self.listener_lock.unlock() };
            count
        };

        loop {
            let frame = batch.pop();
            if frame.is_null() {
                break;
            }
            self.rx_packets.fetch_add(1, Ordering::Relaxed);

            let len = unsafe { (*frame).len };
            let data = unsafe { core::slice::from_raw_parts((*frame).data, len) };
            self.dispatch_one(slot, frame, data, &listeners[..count]);

            unsafe { frame::put(frame) };
        }

        /* The batch is dispatched: a listener that answers from here hands
         * its replies over now, together. Still inside the in-flight count,
         * so an unlisten waiting on it knows they have gone. */
        for listener in listeners[..count].iter() {
            if let Some(batch_end) = listener.batch_end_cb {
                batch_end(listener.ctx);
            }
        }

        if count != 0 {
            self.listener_in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn dispatch_one(&'static self, slot: usize, frame: *mut NetFrame, data: &[u8],
        listeners: &[Listener])
    {
        let counters = unsafe { &mut (*self.rx_proto.get())[slot] };

        if data.len() < ETH_HDR_LEN {
            counters.drop += 1;
            return;
        }

        let nic = self.as_nic();

        match eth::ether_type(data) {
            ETH_TYPE_ARP => {
                counters.arp += 1;
                if let Some(arp) = crate::abi::arp_table() {
                    arp.process(&nic, data);
                }
                return;
            }
            ETH_TYPE_IP if data.len() >= ETH_HDR_LEN + IP_HDR_LEN => {}
            _ => {
                counters.other += 1;
                counters.drop += 1;
                return;
            }
        }

        let packet = &data[ETH_HDR_LEN..];
        match ip::protocol(packet) {
            IP_PROTO_ICMP => {
                counters.icmp += 1;
                if let Some(icmp) = crate::abi::icmp() {
                    icmp.process(&nic, data);
                }
            }
            IP_PROTO_TCP => {
                counters.tcp += 1;
                crate::tcp::TCP.process(&nic, data);
            }
            IP_PROTO_UDP => {
                counters.udp += 1;

                let ip_len = ip::header_len(packet);
                if ip_len == 0 || data.len() < ETH_HDR_LEN + ip_len + UDP_HDR_LEN {
                    return;
                }
                let port = udp::dst_port(&data[ETH_HDR_LEN + ip_len..]);

                /* The callback ran under the listener lock -- with interrupts
                 * off -- on every datagram. On the CPU the NIC's interrupt
                 * targets, that held the card's own interrupt back for the
                 * length of every callback, and forbade the callback anything
                 * that might block. The listener was taken out under the lock
                 * above; it is called with the lock down, and the in-flight
                 * count keeps its context alive until it returns. */
                for listener in listeners.iter() {
                    if listener.port != port {
                        continue;
                    }
                    /* A listener keeps the frame by taking a reference before
                     * it returns: the release after this is then not the last
                     * one. */
                    if let Some(cb) = listener.frame_cb {
                        cb(listener.ctx, frame as usize);
                    }
                    break;
                }
            }
            _ => {
                counters.other += 1;
                counters.drop += 1;
            }
        }
    }
}

/* ---- listeners ---- */

impl Device {
    /// This device as the handle every consumer-side call takes.
    pub(crate) fn as_nic(&'static self) -> kcore::net::Nic {
        unsafe { kcore::net::Nic::from_handle(self as *const Device as usize) }
            .unwrap_or_else(|| unreachable!())
    }

    /// Every UDP datagram to `port`, handed to `cb` with the frame itself.
    /// Never takes a port from whoever has it.
    pub fn listen_udp(&self, port: u16,
        cb: extern "C" fn(ctx: *mut u8, frame: usize),
        batch_end: Option<extern "C" fn(ctx: *mut u8)>,
        ctx: *mut u8) -> i32
    {
        if port == 0 {
            return LISTEN_INVALID;
        }

        self.listener_lock.lock();
        let count = unsafe { *self.listener_count.get() };
        let listeners = unsafe { &mut *self.listeners.get() };

        for listener in listeners[..count].iter() {
            if listener.port == port {
                unsafe { self.listener_lock.unlock() };
                return LISTEN_PORT_TAKEN;
            }
        }
        if count >= MAX_LISTENERS {
            unsafe { self.listener_lock.unlock() };
            return LISTEN_TABLE_FULL;
        }

        listeners[count] = Listener { port, frame_cb: Some(cb), batch_end_cb: batch_end, ctx };
        unsafe { *self.listener_count.get() = count + 1 };
        unsafe { self.listener_lock.unlock() };
        LISTEN_OK
    }

    /// Takes away the listener registered on the port with this context, and
    /// nobody else's. Returns once no call of it is still running, so the
    /// caller may free what it reaches.
    ///
    /// Task context only: a listener that unregistered itself from inside its
    /// own callback would wait here for itself.
    pub fn unlisten_udp(&self, port: u16, ctx: *mut u8) {
        {
            self.listener_lock.lock();
            let count = unsafe { *self.listener_count.get() };
            let listeners = unsafe { &mut *self.listeners.get() };

            for i in 0..count {
                if listeners[i].port != port || listeners[i].ctx != ctx {
                    continue;
                }
                for j in i..count - 1 {
                    listeners[j] = listeners[j + 1];
                }
                listeners[count - 1] = NO_LISTENER;
                unsafe { *self.listener_count.get() = count - 1 };
                break;
            }
            unsafe { self.listener_lock.unlock() };
        }

        /* No new dispatch can find it now; wait out any that took it before
         * the lock. The count covers every listener on the device, not just
         * this port -- the table is compacted on removal, so a per-slot count
         * would not stay with its slot -- and a callback is microseconds, so
         * waiting for a neighbour's costs nothing worth a second structure. */
        while self.listener_in_flight.load(Ordering::Acquire) != 0 {
            kcore::task::sleep_ms(1);
        }
    }

    /* ---- what the shell reports ---- */

    pub fn stats(&self) -> Stats {
        let mut stats = Stats {
            tx_total: self.tx_packets.load(Ordering::Relaxed),
            rx_total: self.rx_packets.load(Ordering::Relaxed),
            rx_drop: 0, rx_icmp: 0, rx_udp: 0, rx_tcp: 0, rx_arp: 0, rx_other: 0,
            tx_icmp: 0, tx_udp: 0, tx_tcp: 0, tx_arp: 0, tx_other: 0,
        };

        let rx = unsafe { &*self.rx_proto.get() };
        let tx = unsafe { &*self.tx_proto.get() };
        for slot in rx.iter() {
            stats.rx_icmp += slot.icmp;
            stats.rx_udp += slot.udp;
            stats.rx_tcp += slot.tcp;
            stats.rx_arp += slot.arp;
            stats.rx_other += slot.other;
            stats.rx_drop += slot.drop;
        }
        for slot in tx.iter() {
            stats.tx_icmp += slot.icmp;
            stats.tx_udp += slot.udp;
            stats.tx_tcp += slot.tcp;
            stats.tx_arp += slot.arp;
            stats.tx_other += slot.other;
        }
        stats
    }
}

/// What `net` prints per device. The C++ side declares the same struct.
#[repr(C)]
pub struct Stats {
    pub tx_total: usize,
    pub rx_total: usize,
    pub rx_drop: usize,
    pub rx_icmp: usize,
    pub rx_udp: usize,
    pub rx_tcp: usize,
    pub rx_arp: usize,
    pub rx_other: usize,
    pub tx_icmp: usize,
    pub tx_udp: usize,
    pub tx_tcp: usize,
    pub tx_arp: usize,
    pub tx_other: usize,
}

/* ---- the table of them ---- */

pub struct DeviceTable {
    devices: [Device; MAX_DEVICES],
    count: AtomicUsize,

    /// Whether the pass being dispatched is one a poll asked for
    poll_pending: AtomicUsize,
    rx_polls: AtomicUsize,
    rx_poll_work: AtomicUsize,
    rx_stalls: AtomicUsize,
    /// Whether the previous pass was a poll's
    last_pass_was_poll: AtomicBool,
    handlers_registered: AtomicBool,
}

unsafe impl Sync for DeviceTable {}
unsafe impl Send for DeviceTable {}

pub static DEVICES: DeviceTable = DeviceTable {
    devices: [const { Device::new() }; MAX_DEVICES],
    count: AtomicUsize::new(0),
    poll_pending: AtomicUsize::new(0),
    rx_polls: AtomicUsize::new(0),
    rx_poll_work: AtomicUsize::new(0),
    rx_stalls: AtomicUsize::new(0),
    last_pass_was_poll: AtomicBool::new(false),
    handlers_registered: AtomicBool::new(false),
};

impl DeviceTable {
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    pub fn at(&'static self, index: usize) -> Option<&'static Device> {
        if index >= self.count() {
            return None;
        }
        Some(&self.devices[index])
    }

    pub fn find(&'static self, name: &[u8]) -> Option<&'static Device> {
        for i in 0..self.count() {
            let dev = &self.devices[i];
            if dev.name() == name {
                return Some(dev);
            }
        }
        None
    }

    /// A driver's device. None when the table is full or the ops are not a
    /// device.
    pub fn register(&'static self, ops: &DeviceOps) -> Option<&'static Device> {
        if ops.name.is_null() {
            return None;
        }

        let index = self.count();
        if index >= MAX_DEVICES {
            return None;
        }
        let dev = &self.devices[index];
        if dev.used.swap(true, Ordering::AcqRel) {
            return None;
        }

        unsafe {
            *dev.ops.get() = Some(*ops);
            *dev.mac.get() = ops.mac;

            let name = &mut *dev.name.get();
            let mut len = 0;
            while len < NAME_MAX - 1 && *ops.name.add(len) != 0 {
                name[len] = *ops.name.add(len);
                len += 1;
            }
            *dev.name_len.get() = len;
        }

        self.count.store(index + 1, Ordering::Release);

        /* One handler per softirq type, dispatching to every device */
        if !self.handlers_registered.swap(true, Ordering::AcqRel) {
            kcore::softirq::register(kcore::softirq::TYPE_NET_RX, on_rx_softirq,
                core::ptr::null_mut());
            kcore::softirq::register(kcore::softirq::TYPE_NET_TX, on_tx_softirq,
                core::ptr::null_mut());
        }

        let mac = ops.mac;
        trace!(0, "net: {} registered, mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            core::str::from_utf8(dev.name()).unwrap_or("?"),
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
        Some(dev)
    }

    /// The receive softirq: harvest every device, then dispatch what it
    /// harvested.
    pub fn process_all_rx(&'static self) {
        /* Test and clear: whoever gets the 1 owns the attribution for this
         * pass. */
        let polled = self.poll_pending.swap(0, Ordering::AcqRel) == 1;
        let mut pending = 0;

        let count = self.count();
        for i in 0..count {
            let dev = &self.devices[i];
            if let Some(ops) = dev.ops() {
                (ops.process_rx)(ops.ctx);
            }
            /* After the harvest, before the dispatch: what the hardware had
             * waiting. */
            pending += dev.rx_pending();
            dev.drain_rx_and_dispatch();
        }

        if polled && pending != 0 {
            self.rx_poll_work.fetch_add(1, Ordering::Relaxed);

            /* Two polls in a row finding work, with no interrupt-driven pass
             * between them, is the shape of a wakeup that is not coming. One
             * on its own is just the poll winning a race against an interrupt
             * already in flight. */
            if self.last_pass_was_poll.load(Ordering::Acquire) {
                self.rx_stalls.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.last_pass_was_poll.store(polled, Ordering::Release);
    }

    pub fn process_all_tx(&'static self) {
        for i in 0..self.count() {
            self.devices[i].drain_tx();
        }
    }

    /// Look at the receive path without waiting to be asked.
    ///
    /// A driver whose only source of liveness is its own interrupt has no
    /// recovery from a lost one: a wakeup missed while the ring is full means
    /// nothing ever looks at that ring again -- which is how a bare metal
    /// machine goes permanently deaf while the rest of the kernel runs on.
    /// This is the fallback: a lost wakeup then costs a tick instead of the
    /// rest of the uptime.
    ///
    /// Raised only when the softirq is not already pending, so a pass this
    /// causes can be told from one an interrupt caused -- which is what makes
    /// the stall count evidence rather than a guess.
    pub fn poll_rx(&'static self) {
        if self.count() == 0 {
            return;
        }
        if kcore::softirq::is_pending(kcore::softirq::TYPE_NET_RX) {
            /* An interrupt has already asked; leave it to say so. */
            return;
        }

        self.rx_polls.fetch_add(1, Ordering::Relaxed);
        self.poll_pending.store(1, Ordering::Release);
        kcore::softirq::raise(kcore::softirq::TYPE_NET_RX);
    }

    pub fn poll_counts(&self) -> (usize, usize, usize) {
        (self.rx_polls.load(Ordering::Relaxed),
         self.rx_poll_work.load(Ordering::Relaxed),
         self.rx_stalls.load(Ordering::Relaxed))
    }
}

extern "C" fn on_rx_softirq(_ctx: *mut u8) {
    DEVICES.process_all_rx();
}

extern "C" fn on_tx_softirq(_ctx: *mut u8) {
    DEVICES.process_all_tx();
}

/* ---- what a driver calls ---- */

/// # Safety
/// `dev` is a handle `kernel_netdev_register` or `kernel_net_find` gave out.
unsafe fn device_of(dev: usize) -> Option<&'static Device> {
    if dev == 0 {
        None
    } else {
        Some(unsafe { &*(dev as *const Device) })
    }
}

/// Register a device. 0 when the table is full or the ops are not a device.
///
/// # Safety
/// `ops` points at a filled table whose name and context outlive the kernel.
#[no_mangle]
pub unsafe extern "C" fn kernel_netdev_register(ops: *const DeviceOps) -> usize {
    if ops.is_null() {
        return 0;
    }
    let ops = unsafe { &*ops };

    match DEVICES.register(ops) {
        Some(dev) => dev as *const Device as usize,
        None => 0,
    }
}

#[no_mangle]
pub extern "C" fn kernel_netdev_set_ip(dev: usize, ip: u32) {
    if let Some(dev) = unsafe { device_of(dev) } {
        dev.set_ip(ip);
    }
}

#[no_mangle]
pub extern "C" fn kernel_netdev_set_mask(dev: usize, mask: u32) {
    if let Some(dev) = unsafe { device_of(dev) } {
        dev.set_mask(mask);
    }
}

#[no_mangle]
pub extern "C" fn kernel_netdev_set_gw(dev: usize, gw: u32) {
    if let Some(dev) = unsafe { device_of(dev) } {
        dev.set_gw(gw);
    }
}

/// One queued frame, from inside the driver's own `flush_tx`. 0 when the
/// queue is empty.
#[no_mangle]
pub extern "C" fn kernel_netdev_tx_dequeue(dev: usize) -> usize {
    match unsafe { device_of(dev) } {
        Some(dev) => dev.tx_dequeue() as usize,
        None => 0,
    }
}

/// Nothing to do: the doorbell is the driver's, and the queue is drained
/// under the lock it already holds. Kept because a driver calls it.
#[no_mangle]
pub extern "C" fn kernel_netdev_tx_notify(_dev: usize) {}

/// A received frame into the stack. Takes it either way: what the queue had
/// no room for is released here.
#[no_mangle]
pub extern "C" fn kernel_netdev_enqueue_rx(dev: usize, frame: usize) {
    let (dev, frame) = match (unsafe { device_of(dev) }, frame) {
        (Some(dev), frame) if frame != 0 => (dev, frame as *mut NetFrame),
        _ => return,
    };

    if !dev.enqueue_rx(frame) {
        unsafe { frame::put(frame) };
    }
}

/// A whole harvest, under one acquisition. Takes every frame; what the queue
/// had no room for is released here.
///
/// # Safety
/// `frames` points at `count` frame handles the caller gives up.
#[no_mangle]
pub unsafe extern "C" fn kernel_netdev_enqueue_rx_batch(
    dev: usize, frames: *const usize, count: usize,
) -> usize {
    let dev = match unsafe { device_of(dev) } {
        Some(dev) => dev,
        None => return 0,
    };
    if frames.is_null() || count == 0 {
        return 0;
    }

    let handles = unsafe { core::slice::from_raw_parts(frames, count) };
    let mut pointers = [core::ptr::null_mut(); 64];
    let mut taken = 0;
    let mut at = 0;

    while at < handles.len() {
        let chunk = (handles.len() - at).min(pointers.len());
        for i in 0..chunk {
            pointers[i] = handles[at + i] as *mut NetFrame;
        }
        let got = dev.enqueue_rx_batch(&pointers[..chunk]);
        taken += got;

        for &frame in pointers[got..chunk].iter() {
            unsafe { frame::put(frame) };
        }
        if got < chunk {
            /* The queue is full; the rest go the same way */
            at += chunk;
            for &handle in handles[at..].iter() {
                unsafe { frame::put(handle as *mut NetFrame) };
            }
            break;
        }
        at += chunk;
    }

    taken
}

/// A transmitted frame handed back for release once the lock is down.
#[no_mangle]
pub extern "C" fn kernel_netdev_tx_done(dev: usize, frame: usize) {
    if let (Some(dev), true) = (unsafe { device_of(dev) }, frame != 0) {
        dev.tx_done(frame as *mut NetFrame);
    }
}

/* ---- frames ---- */

#[no_mangle]
pub extern "C" fn kernel_netframe_alloc_tx(len: usize) -> usize {
    frame::alloc_tx(len) as usize
}

#[no_mangle]
pub extern "C" fn kernel_netframe_alloc_rx(len: usize) -> usize {
    frame::alloc_rx(len) as usize
}

#[no_mangle]
pub extern "C" fn kernel_netframe_data(frame: usize) -> *mut u8 {
    if frame == 0 {
        return core::ptr::null_mut();
    }
    unsafe { (*(frame as *const NetFrame)).data }
}

#[no_mangle]
pub extern "C" fn kernel_netframe_data_phys(frame: usize) -> u64 {
    if frame == 0 {
        return 0;
    }
    unsafe { (*(frame as *const NetFrame)).data_phys as u64 }
}

#[no_mangle]
pub extern "C" fn kernel_netframe_len(frame: usize) -> usize {
    if frame == 0 {
        return 0;
    }
    unsafe { (*(frame as *const NetFrame)).len }
}

#[no_mangle]
pub extern "C" fn kernel_netframe_set_len(frame: usize, len: usize) {
    if frame != 0 {
        unsafe { (*(frame as *mut NetFrame)).len = len };
    }
}

#[no_mangle]
pub extern "C" fn kernel_netframe_get(frame: usize) {
    if frame != 0 {
        unsafe { frame::get(frame as *mut NetFrame) };
    }
}

#[no_mangle]
pub extern "C" fn kernel_netframe_put(frame: usize) {
    if frame != 0 {
        unsafe { frame::put(frame as *mut NetFrame) };
    }
}

/* ---- what a consumer calls ---- */

/// The device of that name, or 0.
///
/// # Safety
/// `name` points at `name_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_net_find(name: *const u8, name_len: usize) -> usize {
    if name.is_null() || name_len == 0 {
        return 0;
    }
    let name = unsafe { core::slice::from_raw_parts(name, name_len) };

    match DEVICES.find(name) {
        Some(dev) => dev as *const Device as usize,
        None => 0,
    }
}

#[no_mangle]
pub extern "C" fn kernel_net_ip(dev: usize) -> u32 {
    unsafe { device_of(dev) }.map_or(0, |dev| dev.ip())
}

/// # Safety
/// `out` takes six bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_net_mac(dev: usize, out: *mut u8) {
    let dev = match unsafe { device_of(dev) } {
        Some(dev) => dev,
        None => return,
    };
    if out.is_null() {
        return;
    }
    let mac = dev.mac();
    unsafe { core::ptr::copy_nonoverlapping(mac.as_ptr(), out, 6) };
}

#[no_mangle]
pub extern "C" fn kernel_net_set_ip(dev: usize, ip: u32) {
    kernel_netdev_set_ip(dev, ip);
}

#[no_mangle]
pub extern "C" fn kernel_net_set_mask(dev: usize, mask: u32) {
    kernel_netdev_set_mask(dev, mask);
}

#[no_mangle]
pub extern "C" fn kernel_net_set_gw(dev: usize, gw: u32) {
    kernel_netdev_set_gw(dev, gw);
}

#[no_mangle]
pub extern "C" fn kernel_net_route_ip(dev: usize, dst: u32) -> u32 {
    unsafe { device_of(dev) }.map_or(dst, |dev| dev.route_ip(dst))
}

/// A frame the caller built whole, out of the device: 0 queued, -1 not.
///
/// # Safety
/// `data` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_net_send_raw(dev: usize, data: *const u8, len: usize) -> i32 {
    let dev = match unsafe { device_of(dev) } {
        Some(dev) => dev,
        None => return -1,
    };
    if data.is_null() || len == 0 {
        return -1;
    }

    let data = unsafe { core::slice::from_raw_parts(data, len) };
    if dev.send_raw(data) { 0 } else { -1 }
}

#[no_mangle]
pub extern "C" fn kernel_net_udp_listen(
    dev: usize, port: u16, cb: extern "C" fn(ctx: *mut u8, frame: usize), ctx: *mut u8,
) -> i32 {
    match unsafe { device_of(dev) } {
        Some(dev) => dev.listen_udp(port, cb, None, ctx),
        None => LISTEN_INVALID,
    }
}

#[no_mangle]
pub extern "C" fn kernel_net_udp_listen_batch(
    dev: usize, port: u16, cb: extern "C" fn(ctx: *mut u8, frame: usize), ctx: *mut u8,
    batch_end: extern "C" fn(ctx: *mut u8),
) -> i32 {
    match unsafe { device_of(dev) } {
        Some(dev) => dev.listen_udp(port, cb, Some(batch_end), ctx),
        None => LISTEN_INVALID,
    }
}

#[no_mangle]
pub extern "C" fn kernel_net_udp_unlisten(dev: usize, port: u16, ctx: *mut u8) {
    if let Some(dev) = unsafe { device_of(dev) } {
        dev.unlisten_udp(port, ctx);
    }
}

/// Queues frames to transmit, one lock and one doorbell for the lot. Takes
/// every frame; answers how many were queued.
///
/// # Safety
/// `frames` points at `count` frame handles the caller gives up.
#[no_mangle]
pub unsafe extern "C" fn kernel_net_submit_tx(
    dev: usize, frames: *const usize, count: usize,
) -> usize {
    let dev = match unsafe { device_of(dev) } {
        Some(dev) => dev,
        None => return 0,
    };
    if frames.is_null() || count == 0 {
        return 0;
    }

    let handles = unsafe { core::slice::from_raw_parts(frames, count) };
    let mut pointers = [core::ptr::null_mut(); 64];
    let mut queued = 0;
    let mut at = 0;

    while at < handles.len() {
        let chunk = (handles.len() - at).min(pointers.len());
        for i in 0..chunk {
            pointers[i] = handles[at + i] as *mut NetFrame;
        }
        queued += dev.submit_tx_batch(&pointers[..chunk]);
        at += chunk;
    }
    queued
}

/// Look at the receive path without waiting to be asked. From the tick.
#[no_mangle]
pub extern "C" fn rust_net_poll_rx() {
    DEVICES.poll_rx();
}

/// Polls issued, polls that found work, and polls that found work with no
/// interrupt-driven pass since the last one.
///
/// # Safety
/// All three are writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_net_rx_poll_stats(
    polls: *mut usize, work: *mut usize, stalls: *mut usize,
) {
    let (a, b, c) = DEVICES.poll_counts();
    unsafe {
        if !polls.is_null() { *polls = a; }
        if !work.is_null() { *work = b; }
        if !stalls.is_null() { *stalls = c; }
    }
}

#[no_mangle]
pub extern "C" fn rust_net_device_count() -> usize {
    DEVICES.count()
}

/// The index'th device: its handle, or 0 past the end.
#[no_mangle]
pub extern "C" fn rust_net_device_at(index: usize) -> usize {
    match DEVICES.at(index) {
        Some(dev) => dev as *const Device as usize,
        None => 0,
    }
}

/// Its name into `out`, NUL-terminated; the length, or 0.
///
/// # Safety
/// `out` takes `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_net_device_name(dev: usize, out: *mut u8, cap: usize) -> usize {
    let dev = match unsafe { device_of(dev) } {
        Some(dev) => dev,
        None => return 0,
    };
    if out.is_null() || cap == 0 {
        return 0;
    }

    let name = dev.name();
    let len = name.len().min(cap - 1);
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), out, len);
        *out.add(len) = 0;
    }
    len
}

/// What `net` prints for it.
///
/// # Safety
/// `out` points at a Stats.
#[no_mangle]
pub unsafe extern "C" fn rust_net_device_stats(dev: usize, out: *mut Stats) {
    let dev = match unsafe { device_of(dev) } {
        Some(dev) => dev,
        None => return,
    };
    if out.is_null() {
        return;
    }
    unsafe { *out = dev.stats() };
}

/// A UDP datagram out of the device, the destination resolved through ARP:
/// 0 sent, -1 not. Task context -- the resolution may wait.
///
/// # Safety
/// `data` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_net_send_udp(
    dev: usize, dst_ip: u32, dst_port: u16, src_ip: u32, src_port: u16,
    data: *const u8, len: usize,
) -> i32 {
    let dev = match unsafe { device_of(dev) } {
        Some(dev) => dev,
        None => return -1,
    };
    if data.is_null() && len != 0 {
        return -1;
    }

    let payload = if len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(data, len) }
    };
    let arp = match crate::abi::arp_table() {
        Some(arp) => arp,
        None => return -1,
    };

    let nic = dev.as_nic();
    if crate::udp::send(&nic, arp, dst_ip, dst_port, src_ip, src_port, payload) {
        0
    } else {
        -1
    }
}
