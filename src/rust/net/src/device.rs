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

use kcore::once::Once;
use kcore::percpu::{ConstInit, LocalCounter, PerCpu};
use kcore::sync::{IrqSpinLock, PreemptSpinLock};
use kcore::trace;

use crate::frame::{Frame, FrameQueue};
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

/// How many frames cross from a driver, or to one, in a single call.
const HANDLE_CHUNK: usize = 64;

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

/// The driver behind a device: the two things it is asked, and the word it
/// asked to be given back. A word and not a pointer, because that is all it
/// is to this layer -- which is also what lets a device be shared between
/// CPUs without anybody having to promise anything.
struct Driver {
    flush_tx: extern "C" fn(ctx: *mut u8),
    process_rx: extern "C" fn(ctx: *mut u8),
    ctx: usize,
}

impl Driver {
    fn flush_tx(&self) {
        (self.flush_tx)(self.ctx as *mut u8);
    }

    fn process_rx(&self) {
        (self.process_rx)(self.ctx as *mut u8);
    }
}

/// What a device is from the moment it is registered, and never changes.
struct Identity {
    driver: Driver,
    name: [u8; NAME_MAX],
    name_len: usize,
    mac: Mac,
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
    /// The listener's word, handed back with every call
    ctx: usize,
}

const NO_LISTENER: Listener = Listener { port: 0, frame_cb: None, batch_end_cb: None, ctx: 0 };

struct Listeners {
    table: [Listener; MAX_LISTENERS],
    count: usize,
}

/* ---- counters ----
 *
 * Per CPU, and added to with no bus lock. This is a datapath, and a counter
 * every arriving packet increments on one cache line is the thing this layer
 * has spent its history removing. The CPU is read once per batch, not once
 * per frame. */

#[repr(align(64))]
struct RxCounters {
    icmp: LocalCounter,
    udp: LocalCounter,
    tcp: LocalCounter,
    arp: LocalCounter,
    other: LocalCounter,
    drop: LocalCounter,
}

#[repr(align(64))]
struct TxCounters {
    icmp: LocalCounter,
    udp: LocalCounter,
    tcp: LocalCounter,
    arp: LocalCounter,
    other: LocalCounter,
}

impl ConstInit for RxCounters {
    const INIT: Self = RxCounters {
        icmp: LocalCounter::new(), udp: LocalCounter::new(), tcp: LocalCounter::new(),
        arp: LocalCounter::new(), other: LocalCounter::new(), drop: LocalCounter::new(),
    };
}

impl ConstInit for TxCounters {
    const INIT: Self = TxCounters {
        icmp: LocalCounter::new(), udp: LocalCounter::new(), tcp: LocalCounter::new(),
        arp: LocalCounter::new(), other: LocalCounter::new(),
    };
}

/* ---- a device ---- */

/// What the transmit lock guards.
struct Tx {
    queue: FrameQueue,
    /// Transmitted frames waiting to be released off the lock
    done: FrameQueue,
}

pub struct Device {
    used: AtomicBool,
    identity: Once<Identity>,

    ip: AtomicU32,
    mask: AtomicU32,
    gw: AtomicU32,

    /// Taken from a driver's interrupt handler, so interrupts go off with it
    tx: IrqSpinLock<Tx>,

    rx: IrqSpinLock<FrameQueue>,
    /// How long the receive queue is, left where the poll can see it without
    /// the lock: a hint, not an invariant
    rx_waiting: AtomicUsize,

    /// Never taken from a hard interrupt
    listeners: PreemptSpinLock<Listeners>,
    /// Callbacks running right now. An unlisten waits for this to drain, so
    /// that what the callback reaches may be freed after it returns.
    listener_in_flight: AtomicUsize,

    rx_proto: PerCpu<RxCounters>,
    tx_proto: PerCpu<TxCounters>,
    tx_packets: AtomicUsize,
    rx_packets: AtomicUsize,
}

impl Device {
    const fn new() -> Device {
        Device {
            used: AtomicBool::new(false),
            identity: Once::new(),
            ip: AtomicU32::new(0),
            mask: AtomicU32::new(0),
            gw: AtomicU32::new(0),
            tx: IrqSpinLock::new(Tx { queue: FrameQueue::new(), done: FrameQueue::new() }),
            rx: IrqSpinLock::new(FrameQueue::new()),
            rx_waiting: AtomicUsize::new(0),
            listeners: PreemptSpinLock::new(Listeners {
                table: [NO_LISTENER; MAX_LISTENERS],
                count: 0,
            }),
            listener_in_flight: AtomicUsize::new(0),
            rx_proto: PerCpu::new(),
            tx_proto: PerCpu::new(),
            tx_packets: AtomicUsize::new(0),
            rx_packets: AtomicUsize::new(0),
        }
    }

    pub fn name(&self) -> &[u8] {
        self.identity.get().map_or(&[], |identity| &identity.name[..identity.name_len])
    }

    pub fn mac(&self) -> Mac {
        self.identity.get().map_or([0; 6], |identity| identity.mac)
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

    fn driver(&self) -> Option<&Driver> {
        self.identity.get().map(|identity| &identity.driver)
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
    ///
    /// # Safety
    /// Called from inside this device's `flush_tx`, which is to say under
    /// its transmit lock.
    pub unsafe fn tx_done(&self, frame: Frame) {
        /* `submit_tx` and `drain_tx` hold the lock across the driver's
         * `flush_tx` and do not reach through their guard until it returns. */
        unsafe { self.tx.reenter() }.done.push(frame);
    }

    /// One queued frame, for a driver inside its own `flush_tx`.
    ///
    /// # Safety
    /// As `tx_done`.
    pub unsafe fn tx_dequeue(&self) -> Option<Frame> {
        let frame = unsafe { self.tx.reenter() }.queue.pop()?;
        self.tx_packets.fetch_add(1, Ordering::Relaxed);
        Some(frame)
    }

    /// The finished frames, released with the lock down and interrupts on.
    pub fn release_tx_done(&self) {
        loop {
            let frame = self.tx.lock().done.pop();
            match frame {
                /* Outside the lock, always: see `tx_done` */
                Some(frame) => drop(frame),
                None => return,
            }
        }
    }

    /// Queue frames and ring the doorbell once for the lot. Takes every
    /// frame; answers how many were queued, the rest released.
    ///
    /// The caller has counted them -- `count_tx` -- on its way here: that is
    /// bookkeeping, and the section below is the narrowest one on the
    /// transmit path.
    pub fn submit_tx(&self, mut frames: FrameQueue) -> usize {
        if frames.is_empty() {
            return 0;
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
        let mut guard = if kcore::trace::panic_active() {
            self.tx.try_lock()
        } else {
            Some(self.tx.lock())
        };

        let mut queued = 0;
        {
            let tx = match guard.as_mut() {
                Some(guard) => &mut **guard,
                /* The panic path, and the lock's holder is not coming back:
                 * see above. */
                None => unsafe { self.tx.steal() },
            };
            while tx.queue.len() < TX_CAPACITY {
                match frames.pop() {
                    Some(frame) => tx.queue.push(frame),
                    None => break,
                }
                queued += 1;
            }
            /* The borrow ends here, before the driver runs: its `flush_tx`
             * comes back in for this queue through `tx_dequeue`. */
        }

        if queued != 0 {
            if let Some(driver) = self.driver() {
                driver.flush_tx();
            }
        }

        /* No room for the rest: released along with what the driver
         * finishes -- or, with no lock to put them under, here. */
        match guard {
            Some(mut guard) => {
                while let Some(frame) = frames.pop() {
                    guard.done.push(frame);
                }
            }
            None => drop(frames),
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

    /// The transmit softirq: what a driver with nothing else to do owes.
    pub fn drain_tx(&self) {
        {
            let guard = self.tx.lock();
            let pending = !guard.queue.is_empty();
            if pending {
                if let Some(driver) = self.driver() {
                    /* Under the lock, and the guard untouched until it
                     * returns: what `tx_dequeue` needs of its caller. */
                    driver.flush_tx();
                }
            }
        }

        self.release_tx_done();
    }

    /// A frame the caller built whole, copied into one of the device's and
    /// queued.
    pub fn send_raw(&self, data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }

        let mut frame = match Frame::alloc_tx(data.len()) {
            Some(frame) => frame,
            None => return false,
        };
        if !frame.fill(data) {
            return false;
        }

        self.count_tx(&frame);
        let mut frames = FrameQueue::new();
        frames.push(frame);
        self.submit_tx(frames) == 1
    }

    /// One outgoing frame, counted by what it carries. Called where every
    /// driver's frames leave, so a driver with no classifier of its own is
    /// counted too.
    pub fn count_tx(&self, frame: &Frame) {
        let counters = self.tx_proto.here();
        let data = frame.bytes();

        if data.len() < ETH_HDR_LEN {
            counters.other.add(1);
            return;
        }

        match eth::ether_type(data) {
            ETH_TYPE_ARP => counters.arp.add(1),
            ETH_TYPE_IP if data.len() >= ETH_HDR_LEN + IP_HDR_LEN => {
                match ip::protocol(&data[ETH_HDR_LEN..]) {
                    IP_PROTO_ICMP => counters.icmp.add(1),
                    IP_PROTO_TCP => counters.tcp.add(1),
                    IP_PROTO_UDP => counters.udp.add(1),
                    _ => counters.other.add(1),
                }
            }
            _ => counters.other.add(1),
        }
    }
}

/* ---- receiving ---- */

impl Device {
    /// A whole harvest, under one acquisition rather than one per frame.
    /// Takes what there is room for; what is left in `frames` is the
    /// caller's still, to release once this returns -- with the lock down.
    pub fn enqueue_rx(&self, frames: &mut FrameQueue) -> usize {
        let mut queue = self.rx.lock();

        let mut taken = 0;
        while queue.len() < RX_CAPACITY {
            match frames.pop() {
                Some(frame) => queue.push(frame),
                None => break,
            }
            taken += 1;
        }
        self.rx_waiting.store(queue.len(), Ordering::Relaxed);
        taken
    }

    /// What the hardware has waiting, as a hint for the receive poll -- read
    /// without the lock, because it is a hint and not an invariant.
    pub fn rx_pending(&self) -> usize {
        self.rx_waiting.load(Ordering::Relaxed)
    }

    /// Take the whole receive queue and dispatch it to the protocols.
    ///
    /// The queue is spliced in one go and dispatched with the lock down.
    /// Frames arrive one at a time but leave in a run, and taking the lock
    /// per frame -- with interrupts off -- was the top of the receive path in
    /// a profile at thirty-seven thousand packets a second.
    pub fn drain_rx_and_dispatch(&'static self) {
        let mut batch = {
            let mut queue = self.rx.lock();
            self.rx_waiting.store(0, Ordering::Relaxed);
            queue.take()
        };

        /* Nothing arrived: no table to copy, no hold to take, no lock. The
         * drain runs on every softirq pass, most of which have no frames. */
        if batch.is_empty() {
            return;
        }

        /* Read once for the batch: the counters are per CPU, and the poll
         * that produced this batch does not migrate part way through it. */
        let counters = self.rx_proto.here();

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
            let table = self.listeners.lock();
            let count = table.count;
            listeners[..count].copy_from_slice(&table.table[..count]);
            if count != 0 {
                self.listener_in_flight.fetch_add(1, Ordering::AcqRel);
            }
            count
        };
        let listeners = &listeners[..count];

        while let Some(frame) = batch.pop() {
            self.rx_packets.fetch_add(1, Ordering::Relaxed);
            self.dispatch_one(counters, &frame, listeners);
            /* The receive path's reference goes here. A listener that kept
             * the frame took one of its own. */
            drop(frame);
        }

        /* The batch is dispatched: a listener that answers from here hands
         * its replies over now, together. Still inside the in-flight count,
         * so an unlisten waiting on it knows they have gone. */
        for listener in listeners {
            if let Some(batch_end) = listener.batch_end_cb {
                batch_end(listener.ctx as *mut u8);
            }
        }

        if count != 0 {
            self.listener_in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn dispatch_one(&'static self, counters: &RxCounters, frame: &Frame, listeners: &[Listener]) {
        let data = frame.bytes();

        if data.len() < ETH_HDR_LEN {
            counters.drop.add(1);
            return;
        }

        let nic = self.as_nic();

        match eth::ether_type(data) {
            ETH_TYPE_ARP => {
                counters.arp.add(1);
                if let Some(arp) = crate::abi::arp_table() {
                    arp.process(&nic, data);
                }
                return;
            }
            ETH_TYPE_IP if data.len() >= ETH_HDR_LEN + IP_HDR_LEN => {}
            _ => {
                counters.other.add(1);
                counters.drop.add(1);
                return;
            }
        }

        let packet = &data[ETH_HDR_LEN..];
        match ip::protocol(packet) {
            IP_PROTO_ICMP => {
                counters.icmp.add(1);
                if let Some(icmp) = crate::abi::icmp() {
                    icmp.process(&nic, data);
                }
            }
            IP_PROTO_TCP => {
                counters.tcp.add(1);
                crate::tcp::TCP.process(&nic, data);
            }
            IP_PROTO_UDP => {
                counters.udp.add(1);

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
                 * count keeps its context alive until it returns.
                 *
                 * The bytes are not looked at again from here: a listener
                 * that keeps the frame may answer in it where it lies. */
                for listener in listeners {
                    if listener.port != port {
                        continue;
                    }
                    /* A listener keeps the frame by taking a reference before
                     * it returns: the release after this is then not the last
                     * one. */
                    if let Some(cb) = listener.frame_cb {
                        cb(listener.ctx as *mut u8, frame.as_lent());
                    }
                    break;
                }
            }
            _ => {
                counters.other.add(1);
                counters.drop.add(1);
            }
        }
    }
}

/* ---- listeners ---- */

impl Device {
    /// This device as the handle every consumer-side call takes.
    pub(crate) fn as_nic(&'static self) -> kcore::net::Nic {
        kcore::net::Nic::from_handle(self as *const Device as usize)
            .unwrap_or_else(|| unreachable!())
    }

    /// Every UDP datagram to `port`, handed to `cb` with the frame itself.
    /// Never takes a port from whoever has it.
    pub fn listen_udp(&self, port: u16,
        cb: extern "C" fn(ctx: *mut u8, frame: usize),
        batch_end: Option<extern "C" fn(ctx: *mut u8)>,
        ctx: usize) -> i32
    {
        if port == 0 {
            return LISTEN_INVALID;
        }

        let mut listeners = self.listeners.lock();
        let count = listeners.count;

        if listeners.table[..count].iter().any(|listener| listener.port == port) {
            return LISTEN_PORT_TAKEN;
        }
        if count >= MAX_LISTENERS {
            return LISTEN_TABLE_FULL;
        }

        listeners.table[count] = Listener { port, frame_cb: Some(cb), batch_end_cb: batch_end, ctx };
        listeners.count = count + 1;
        LISTEN_OK
    }

    /// Takes away the listener registered on the port with this context, and
    /// nobody else's. Returns once no call of it is still running, so the
    /// caller may free what it reaches.
    ///
    /// Task context only: a listener that unregistered itself from inside its
    /// own callback would wait here for itself.
    pub fn unlisten_udp(&self, port: u16, ctx: usize) {
        {
            let mut listeners = self.listeners.lock();
            let count = listeners.count;

            let found = listeners.table[..count]
                .iter()
                .position(|listener| listener.port == port && listener.ctx == ctx);
            if let Some(at) = found {
                listeners.table.copy_within(at + 1..count, at);
                listeners.table[count - 1] = NO_LISTENER;
                listeners.count = count - 1;
            }
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

        for slot in self.rx_proto.iter() {
            stats.rx_icmp += slot.icmp.get();
            stats.rx_udp += slot.udp.get();
            stats.rx_tcp += slot.tcp.get();
            stats.rx_arp += slot.arp.get();
            stats.rx_other += slot.other.get();
            stats.rx_drop += slot.drop.get();
        }
        for slot in self.tx_proto.iter() {
            stats.tx_icmp += slot.icmp.get();
            stats.tx_udp += slot.udp.get();
            stats.tx_tcp += slot.tcp.get();
            stats.tx_arp += slot.arp.get();
            stats.tx_other += slot.other.get();
        }
        stats
    }
}

/// What `net` prints per device.
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
        self.devices[..self.count()].iter().find(|dev| dev.name() == name)
    }

    /// The device a handle names: one of the table's, or none. A handle is
    /// the device's address, so this is a range check and a division -- and
    /// what makes a word from outside something that can be trusted.
    pub fn by_handle(&'static self, handle: usize) -> Option<&'static Device> {
        let base = self.devices.as_ptr() as usize;
        let offset = handle.checked_sub(base)?;
        if offset % core::mem::size_of::<Device>() != 0 {
            return None;
        }
        self.devices.get(offset / core::mem::size_of::<Device>())
    }

    /// A driver's device. None when the table is full or the ops are not a
    /// device.
    ///
    /// # Safety
    /// `ops.name` is a NUL-terminated string.
    pub unsafe fn register(&'static self, ops: &DeviceOps) -> Option<&'static Device> {
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

        let mut name = [0u8; NAME_MAX];
        let mut name_len = 0;
        while name_len < NAME_MAX - 1 {
            let byte = unsafe { *ops.name.add(name_len) };
            if byte == 0 {
                break;
            }
            name[name_len] = byte;
            name_len += 1;
        }

        let identity = Identity {
            driver: Driver {
                flush_tx: ops.flush_tx,
                process_rx: ops.process_rx,
                ctx: ops.ctx as usize,
            },
            name,
            name_len,
            mac: ops.mac,
        };
        if dev.identity.set(identity).is_err() {
            return None;
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

        for dev in self.devices[..self.count()].iter() {
            if let Some(driver) = dev.driver() {
                driver.process_rx();
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
        for dev in self.devices[..self.count()].iter() {
            dev.drain_tx();
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

/* ---- what a driver calls ----
 *
 * A device crosses as a word -- its address -- and `by_handle` is what makes
 * a word from outside a device again: one of the table's, or nothing. So
 * these take any word at all. What they cannot check is a *frame's* word,
 * and the ones that take frames say so. */

/// Frames that came across as words, as a queue -- at most a chunk's worth.
///
/// # Safety
/// Each is a frame reference the caller gives up, of a frame on no queue.
unsafe fn queue_of(handles: &[usize], each: impl Fn(&Frame)) -> FrameQueue {
    let mut frames = FrameQueue::new();
    for &handle in handles {
        if let Some(frame) = unsafe { Frame::from_handle(handle) } {
            each(&frame);
            frames.push(frame);
        }
    }
    frames
}

/// Register a device. 0 when the table is full or the ops are not a device.
///
/// # Safety
/// `ops` points at a filled table whose name and context outlive the kernel.
#[no_mangle]
pub unsafe extern "C" fn kernel_netdev_register(ops: *const DeviceOps) -> usize {
    let ops = match unsafe { ops.as_ref() } {
        Some(ops) => ops,
        None => return 0,
    };

    match unsafe { DEVICES.register(ops) } {
        Some(dev) => dev as *const Device as usize,
        None => 0,
    }
}

#[no_mangle]
pub extern "C" fn kernel_netdev_set_ip(dev: usize, ip: u32) {
    if let Some(dev) = DEVICES.by_handle(dev) {
        dev.set_ip(ip);
    }
}

#[no_mangle]
pub extern "C" fn kernel_netdev_set_mask(dev: usize, mask: u32) {
    if let Some(dev) = DEVICES.by_handle(dev) {
        dev.set_mask(mask);
    }
}

#[no_mangle]
pub extern "C" fn kernel_netdev_set_gw(dev: usize, gw: u32) {
    if let Some(dev) = DEVICES.by_handle(dev) {
        dev.set_gw(gw);
    }
}

/// One queued frame, from inside the driver's own `flush_tx`. 0 when the
/// queue is empty.
///
/// # Safety
/// Called from inside `dev`'s `flush_tx`, and nowhere else: the queue is
/// guarded by the lock that is held there.
#[no_mangle]
pub unsafe extern "C" fn kernel_netdev_tx_dequeue(dev: usize) -> usize {
    DEVICES.by_handle(dev)
        .and_then(|dev| unsafe { dev.tx_dequeue() })
        .map_or(0, Frame::into_handle)
}

/// Nothing to do: the doorbell is the driver's, and the queue is drained
/// under the lock it already holds. Kept because a driver calls it.
#[no_mangle]
pub extern "C" fn kernel_netdev_tx_notify(_dev: usize) {}

/// A received frame into the stack. Takes it either way: what the queue had
/// no room for is released here.
///
/// # Safety
/// `frame` is a frame reference the caller gives up.
#[no_mangle]
pub unsafe extern "C" fn kernel_netdev_enqueue_rx(dev: usize, frame: usize) {
    unsafe { kernel_netdev_enqueue_rx_batch(dev, &frame, 1) };
}

/// A whole harvest, under one acquisition. Takes every frame; what the queue
/// had no room for is released here.
///
/// # Safety
/// `frames` points at `count` frame references the caller gives up.
#[no_mangle]
pub unsafe extern "C" fn kernel_netdev_enqueue_rx_batch(
    dev: usize, frames: *const usize, count: usize,
) -> usize {
    if frames.is_null() || count == 0 {
        return 0;
    }
    let handles = unsafe { core::slice::from_raw_parts(frames, count) };
    let dev = DEVICES.by_handle(dev);

    let mut taken = 0;
    for chunk in handles.chunks(HANDLE_CHUNK) {
        let mut frames = unsafe { queue_of(chunk, |_| ()) };
        if let Some(dev) = dev {
            taken += dev.enqueue_rx(&mut frames);
        }
        /* What found no room -- or no device -- is released here, with the
         * lock down. */
        drop(frames);
    }
    taken
}

/// A transmitted frame handed back for release once the lock is down.
///
/// # Safety
/// `frame` is a frame reference the caller gives up, and the call is made
/// from inside `dev`'s `flush_tx`.
#[no_mangle]
pub unsafe extern "C" fn kernel_netdev_tx_done(dev: usize, frame: usize) {
    let frame = unsafe { Frame::from_handle(frame) };
    if let (Some(dev), Some(frame)) = (DEVICES.by_handle(dev), frame) {
        unsafe { dev.tx_done(frame) };
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
    DEVICES.by_handle(dev).map_or(0, |dev| dev.ip())
}

/// # Safety
/// `out` takes six bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_net_mac(dev: usize, out: *mut u8) {
    let dev = match DEVICES.by_handle(dev) {
        Some(dev) => dev,
        None => return,
    };
    if out.is_null() {
        return;
    }
    let mac = dev.mac();
    unsafe { core::ptr::copy_nonoverlapping(mac.as_ptr(), out, mac.len()) };
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
    DEVICES.by_handle(dev).map_or(dst, |dev| dev.route_ip(dst))
}

/// A frame the caller built whole, out of the device: 0 queued, -1 not.
///
/// # Safety
/// `data` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_net_send_raw(dev: usize, data: *const u8, len: usize) -> i32 {
    let dev = match DEVICES.by_handle(dev) {
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
    match DEVICES.by_handle(dev) {
        Some(dev) => dev.listen_udp(port, cb, None, ctx as usize),
        None => LISTEN_INVALID,
    }
}

#[no_mangle]
pub extern "C" fn kernel_net_udp_listen_batch(
    dev: usize, port: u16, cb: extern "C" fn(ctx: *mut u8, frame: usize), ctx: *mut u8,
    batch_end: extern "C" fn(ctx: *mut u8),
) -> i32 {
    match DEVICES.by_handle(dev) {
        Some(dev) => dev.listen_udp(port, cb, Some(batch_end), ctx as usize),
        None => LISTEN_INVALID,
    }
}

#[no_mangle]
pub extern "C" fn kernel_net_udp_unlisten(dev: usize, port: u16, ctx: *mut u8) {
    if let Some(dev) = DEVICES.by_handle(dev) {
        dev.unlisten_udp(port, ctx as usize);
    }
}

/// Queues frames to transmit, one lock and one doorbell for the lot. Takes
/// every frame; answers how many were queued.
///
/// # Safety
/// `frames` points at `count` frame references the caller gives up.
#[no_mangle]
pub unsafe extern "C" fn kernel_net_submit_tx(
    dev: usize, frames: *const usize, count: usize,
) -> usize {
    if frames.is_null() || count == 0 {
        return 0;
    }
    let handles = unsafe { core::slice::from_raw_parts(frames, count) };
    let dev = DEVICES.by_handle(dev);

    let mut queued = 0;
    for chunk in handles.chunks(HANDLE_CHUNK) {
        match dev {
            Some(dev) => {
                let frames = unsafe { queue_of(chunk, |frame| dev.count_tx(frame)) };
                queued += dev.submit_tx(frames);
            }
            /* No such device: taken all the same, and released. */
            None => drop(unsafe { queue_of(chunk, |_| ()) }),
        }
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
/// All three are writable, or null.
#[no_mangle]
pub unsafe extern "C" fn kernel_net_rx_poll_stats(
    polls: *mut usize, work: *mut usize, stalls: *mut usize,
) {
    let (a, b, c) = DEVICES.poll_counts();
    unsafe {
        if let Some(polls) = polls.as_mut() { *polls = a; }
        if let Some(work) = work.as_mut() { *work = b; }
        if let Some(stalls) = stalls.as_mut() { *stalls = c; }
    }
}
