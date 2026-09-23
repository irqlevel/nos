//! The network devices: the queues between a driver and the stack, the UDP
//! listeners, the receive dispatch, and the table of them all.
//!
//! A driver registers as a `NetDriver` and is then only asked two things:
//! empty the transmit queue into the hardware, and harvest the hardware into
//! the receive queue. Everything between -- the queueing, the batching, the
//! protocol dispatch and the counters -- is here.
//!
//! Drivers and this crate's own services call in directly. What is left of a
//! C ABI, at the bottom, is what a loadable module binds by name
//! (`kcore::net`): a module is linked on its own.
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

use alloc::boxed::Box;
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

/// Why a UDP listener was refused
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenError {
    /// Someone has the port already -- the UDP shell, DHCP, another server.
    PortTaken,
    /// The device's listener table is full.
    TableFull,
    /// Port 0.
    Invalid,
}

/// What `kernel_net_udp_listen` answers a module (kcore::net::ListenError).
const LISTEN_OK: i32 = 0;
const LISTEN_PORT_TAKEN: i32 = 1;
const LISTEN_TABLE_FULL: i32 = 2;
const LISTEN_INVALID: i32 = 3;

/// How many frames cross from a driver, or to one, in a single call.
const HANDLE_CHUNK: usize = 64;

/* ---- what a driver gives the stack ---- */

/// A NIC's driver. There are two things the stack asks of it, and each comes
/// with the state only that call touches, lent for the length of the call:
///
/// - `flush_tx`, with the device's transmit lock held around it, so one at a
///   time per device: it is handed the transmit side as `&mut Self::Tx`;
/// - `process_rx`, from the receive soft IRQ, which the kernel runs on one
///   CPU at a time: it is handed the receive side as `&mut Self::Rx`.
///
/// Everything else of the driver -- what an interrupt handler or a shell
/// command looks at -- is `&self`, shared between whatever runs: registers,
/// atomics, locks. So a driver has no `UnsafeCell` with a comment saying who
/// may touch it; who may touch it is who is handed it. The transmit side
/// lives *inside* the transmit lock and the receive side behind the receive
/// path's `RxContext`, so none of that is a promise: it is how the borrow
/// checker sees it.
pub trait NetDriver: Sync + 'static {
    /// What only `flush_tx` touches: the transmit ring and what is on it.
    type Tx: Send + 'static;
    /// What only `process_rx` touches: the receive ring and what is posted.
    type Rx: Send + 'static;

    /// Give back what the hardware has finished sending and send what the
    /// stack has queued. Called under the device's transmit lock, interrupts
    /// off: no sleeping, no allocating -- and no *freeing*: a frame finished
    /// with goes to `queue.done`, never out of scope.
    fn flush_tx(&'static self, tx: &mut Self::Tx, queue: &mut TxQueue<'_>);

    /// Take what the hardware has received and hand it up.
    fn process_rx(&'static self, rx: &mut Self::Rx, queue: &mut RxQueue<'_>);
}

/// A driver's transmit side with its type erased: what the device keeps
/// under its transmit lock.
trait TxHalf: Send {
    fn flush(&mut self, queue: &mut TxQueue<'_>);
}

/// A driver's receive side, the same.
trait RxHalf: Send {
    fn process(&mut self, queue: &mut RxQueue<'_>);
}

struct TxOf<D: NetDriver> {
    driver: &'static D,
    state: D::Tx,
}

impl<D: NetDriver> TxHalf for TxOf<D> {
    fn flush(&mut self, queue: &mut TxQueue<'_>) {
        self.driver.flush_tx(&mut self.state, queue);
    }
}

struct RxOf<D: NetDriver> {
    driver: &'static D,
    state: D::Rx,
}

impl<D: NetDriver> RxHalf for RxOf<D> {
    fn process(&mut self, queue: &mut RxQueue<'_>) {
        self.driver.process_rx(&mut self.state, queue);
    }
}

/// The stack's transmit queue, for the length of one `flush_tx`: a borrow of
/// what the held transmit lock guards, which is why it cannot outlive the
/// call or be reached from anywhere else.
pub struct TxQueue<'a> {
    queue: &'a mut FrameQueue,
    done: &'a mut FrameQueue,
    sent: &'a AtomicUsize,
}

impl TxQueue<'_> {
    /// The next frame to send, or None when the queue is empty.
    ///
    /// The frame's buffer is what the hardware is about to read: keep it --
    /// in the ring's shadow of what is posted -- until the hardware says it
    /// is done, and then hand it to `done`. Dropped earlier, the buffer goes
    /// back to the pool and out again while the card is still reading it.
    pub fn dequeue(&mut self) -> Option<Frame> {
        let frame = self.queue.pop()?;
        self.sent.fetch_add(1, Ordering::Relaxed);
        Some(frame)
    }

    /// A transmitted frame, for release once the lock is down. Never dropped
    /// here instead: dropping frees, a free can reach the page allocator,
    /// which shoots down the TLB on every other CPU and waits for each -- and
    /// a CPU spinning on this lock has interrupts off and never answers.
    pub fn done(&mut self, frame: Frame) {
        self.done.push(frame);
    }
}

/// The stack's receive queue, for the length of one `process_rx`.
pub struct RxQueue<'a> {
    dev: &'a Device,
}

impl RxQueue<'_> {
    /// One received frame, to the stack. What the queue has no room for is
    /// released.
    pub fn enqueue(&mut self, frame: Frame) {
        let mut frames = FrameQueue::new();
        frames.push(frame);
        self.deliver(&mut frames);
    }

    /// A whole harvest at once: the receive queue's lock is taken once for
    /// the batch rather than once per frame, which at tens of thousands of
    /// packets a second is the difference between a lock acquisition being
    /// noise and being the top of the receive path in a profile. `frames` is
    /// empty after: what found no room is released here, with the lock down.
    pub fn deliver(&mut self, frames: &mut FrameQueue) {
        if frames.is_empty() {
            return;
        }
        self.dev.enqueue_rx(frames);
        drop(frames.take());
    }
}

/* ---- the receive path, as a value ---- */

/// Being inside the receive dispatch, as a value.
///
/// The kernel runs the receive soft IRQ on one CPU at a time, and the pass
/// in it -- `DeviceTable::process_all_rx` -- is what harvests every driver
/// and calls every listener. It makes one of these, and lends it down. So
/// there is one at a time, and holding it is what it means to be the only
/// code on the receive path right now -- which is what lets `RxOwned` hand
/// out its contents without a lock.
pub struct RxContext {
    _only_the_receive_pass_makes_one: (),
}

/// What only the receive path touches: a driver's receive ring, the replies
/// a listener gathers during a batch. No lock, because the `RxContext`
/// borrowed to reach in is the proof nobody else is there.
pub struct RxOwned<T>(core::cell::UnsafeCell<T>);

/* Reached on whichever CPU the receive soft IRQ runs on, one at a time. */
unsafe impl<T: Send> Sync for RxOwned<T> {}

impl<T> RxOwned<T> {
    pub const fn new(value: T) -> Self {
        Self(core::cell::UnsafeCell::new(value))
    }

    #[inline]
    pub fn get<'a>(&'a self, _rx: &'a mut RxContext) -> &'a mut T {
        /* There is one `RxContext` at a time and it is borrowed for as long
         * as what is returned here lives. */
        unsafe { &mut *self.0.get() }
    }
}

/// What a device is from the moment it is registered, and never changes.
struct Identity {
    name: [u8; NAME_MAX],
    name_len: usize,
    mac: Mac,
}

/* ---- listeners ---- */

/// What `Nic::listen` hands datagrams to: a service of this crate.
pub trait UdpHandler: Sync + 'static {
    /// One datagram's frame, lent for the length of the call.
    fn on_frame(&'static self, frame: Lent<'_>, rx: &mut RxContext);
}

/// A frame the receive path lends a listener for the length of one call.
pub struct Lent<'a>(&'a Frame);

impl Lent<'_> {
    /// The frame's bytes, Ethernet header first.
    pub fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }

    /// The frame, to keep past the call -- to answer in, where it lies. The
    /// receive path's own reference is then not the last, and it never looks
    /// at the bytes again.
    pub fn retain(self) -> Frame {
        self.0.retain()
    }
}

/// Who a datagram goes to.
#[derive(Clone, Copy)]
enum Sink {
    /// A service of this crate.
    Handler { handler: &'static dyn UdpHandler },
    /// A module's, across the C ABI: the frame as a word, lent for the call.
    /// `batch_end`: it is also told when a receive batch ends, for a
    /// listener that answers from the receive path and hands its replies
    /// over together -- the load target, which is a module.
    Callback {
        frame_cb: extern "C" fn(ctx: *mut u8, frame: usize),
        batch_end: Option<extern "C" fn(ctx: *mut u8)>,
        ctx: usize,
    },
}

/// What the receive path hands a datagram to.
#[derive(Clone, Copy)]
struct Listener {
    port: u16,
    /// What takes this listener away and nobody else's on the port: the
    /// handler's address, or a callback's context
    key: usize,
    sink: Option<Sink>,
}

const NO_LISTENER: Listener = Listener { port: 0, key: 0, sink: None };

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
    /// The driver's transmit side, from registration on. In here because
    /// this lock is what makes `flush_tx` one at a time: whoever holds it
    /// has the queue and the driver's ring together, and nobody else either.
    driver: Option<Box<dyn TxHalf>>,
}

impl Tx {
    /// The driver's turn: what is queued, into the hardware.
    fn flush(&mut self, sent: &AtomicUsize) {
        let Tx { queue, done, driver } = self;
        if let Some(driver) = driver.as_mut() {
            driver.flush(&mut TxQueue { queue, done, sent });
        }
    }
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
    /// The driver's receive side, from registration on: the receive path's
    rx_driver: Once<RxOwned<Box<dyn RxHalf>>>,

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
            tx: IrqSpinLock::new(Tx {
                queue: FrameQueue::new(), done: FrameQueue::new(), driver: None,
            }),
            rx: IrqSpinLock::new(FrameQueue::new()),
            rx_waiting: AtomicUsize::new(0),
            rx_driver: Once::new(),
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

    /// What the device is known by outside this crate: where it is in the
    /// table, which `DeviceTable::by_handle` turns back into the device.
    pub(crate) fn handle(&self) -> usize {
        self as *const Device as usize
    }
}

/* ---- transmitting ---- */

impl Device {
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

            /* The driver, with the queue and its own ring both lent out of
             * what this lock guards. */
            if queued != 0 {
                tx.flush(&self.tx_packets);
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

    /// How many more frames the queue has room for right now: what a sender
    /// that would rather wait than lose asks before it builds a batch.
    pub fn tx_room(&self) -> usize {
        TX_CAPACITY.saturating_sub(self.tx.lock().queue.len())
    }

    /// The transmit softirq: what a driver with nothing else to do owes.
    pub fn drain_tx(&self) {
        {
            let mut guard = self.tx.lock();
            if !guard.queue.is_empty() {
                guard.flush(&self.tx_packets);
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

    /// The driver's turn: what the hardware has received, into the receive
    /// queue. The receive pass's own, which is what `rx` says.
    fn harvest(&self, rx: &mut RxContext) {
        if let Some(driver) = self.rx_driver.get() {
            driver.get(rx).process(&mut RxQueue { dev: self });
        }
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
    fn drain_rx_and_dispatch(&'static self, rx: &mut RxContext) {
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
            self.dispatch_one(counters, &frame, listeners, rx);
            /* The receive path's reference goes here. A listener that kept
             * the frame took one of its own. */
            drop(frame);
        }

        /* The batch is dispatched: a listener that answers from here hands
         * its replies over now, together. Still inside the in-flight count,
         * so an unlisten waiting on it knows they have gone. */
        for listener in listeners {
            if let Some(Sink::Callback { batch_end: Some(batch_end), ctx, .. }) = listener.sink {
                batch_end(ctx as *mut u8);
            }
        }

        if count != 0 {
            self.listener_in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn dispatch_one(
        &'static self, counters: &RxCounters, frame: &Frame, listeners: &[Listener],
        rx: &mut RxContext,
    ) {
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
                    match listener.sink {
                        Some(Sink::Handler { handler }) => handler.on_frame(Lent(frame), rx),
                        Some(Sink::Callback { frame_cb, ctx, .. }) => {
                            frame_cb(ctx as *mut u8, frame.as_lent())
                        }
                        None => {}
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
    /// This device as what this crate's services hold.
    pub(crate) fn as_nic(&'static self) -> crate::nic::Nic {
        crate::nic::Nic::of(self)
    }

    /// Every UDP datagram to `port`, to a listener. Never takes a port from
    /// whoever has it.
    fn listen(&self, port: u16, key: usize, sink: Sink) -> Result<(), ListenError> {
        if port == 0 {
            return Err(ListenError::Invalid);
        }

        let mut listeners = self.listeners.lock();
        let count = listeners.count;

        if listeners.table[..count].iter().any(|listener| listener.port == port) {
            return Err(ListenError::PortTaken);
        }
        if count >= MAX_LISTENERS {
            return Err(ListenError::TableFull);
        }

        listeners.table[count] = Listener { port, key, sink: Some(sink) };
        listeners.count = count + 1;
        Ok(())
    }

    /// A service of this crate on `port`: the key to `unlisten_udp` it by.
    pub(crate) fn listen_handler(
        &self, port: u16, handler: &'static dyn UdpHandler,
    ) -> Result<usize, ListenError> {
        /* Where the handler is: what no other listener's key can be. */
        let key = handler as *const dyn UdpHandler as *const () as usize;
        self.listen(port, key, Sink::Handler { handler })?;
        Ok(key)
    }

    /// A module's callback on `port`, handed the frame as a word; `ctx` is
    /// its key.
    fn listen_callback(
        &self, port: u16, frame_cb: extern "C" fn(ctx: *mut u8, frame: usize),
        batch_end: Option<extern "C" fn(ctx: *mut u8)>, ctx: usize,
    ) -> Result<(), ListenError> {
        self.listen(port, ctx, Sink::Callback { frame_cb, batch_end, ctx })
    }

    /// Takes away the listener registered on the port with this key, and
    /// nobody else's. Returns once no call of it is still running, so the
    /// caller may free what it reaches.
    ///
    /// Task context only: a listener that unregistered itself from inside its
    /// own callback would wait here for itself.
    pub fn unlisten_udp(&self, port: u16, key: usize) {
        {
            let mut listeners = self.listeners.lock();
            let count = listeners.count;

            let found = listeners.table[..count]
                .iter()
                .position(|listener| listener.port == port && listener.key == key);
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

    /// The device whose subnet `dst` is on -- `hv0` for a guest of the
    /// hypervisor's -- or None when it is on none of them, and so for the
    /// default device's gateway.
    pub fn on_subnet(&'static self, dst: u32) -> Option<&'static Device> {
        self.devices[..self.count()].iter().find(|dev| {
            let mask = dev.mask.load(Ordering::Acquire);
            mask != 0 && dev.ip() != 0 && dst & mask == dev.ip() & mask
        })
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

    /// A driver's device, from here on called: `tx` and `rx` are the two
    /// halves only `flush_tx` and `process_rx` touch, and become the
    /// device's. None when the table is full or the name will not do -- and
    /// then the halves are *leaked*, not dropped: the hardware has been told
    /// where those rings are and may be running on them, so the memory stays
    /// until the driver has quiesced it, which is after this returns.
    pub fn register<D: NetDriver>(
        &'static self, name: &str, mac: Mac, driver: &'static D, tx: D::Tx, rx: D::Rx,
    ) -> Option<&'static Device> {
        /* Made before any lock is taken: nothing allocates under one. */
        let tx_half: Box<dyn TxHalf> = Box::new(TxOf { driver, state: tx });
        let rx_half: Box<dyn RxHalf> = Box::new(RxOf { driver, state: rx });

        let dev = match self.claim(name, mac) {
            Some(dev) => dev,
            None => {
                core::mem::forget(tx_half);
                core::mem::forget(rx_half);
                return None;
            }
        };

        /* Both halves in before the device can be found: the count below is
         * what the softirq passes walk, and the transmit path starts from a
         * device somebody was handed. */
        dev.tx.lock().driver = Some(tx_half);
        if dev.rx_driver.set(RxOwned::new(rx_half)).is_err() {
            /* The slot was this registration's alone: see `claim`. */
            return None;
        }

        self.count.store(self.count() + 1, Ordering::Release);

        /* One handler per softirq type, dispatching to every device */
        if !self.handlers_registered.swap(true, Ordering::AcqRel) {
            kcore::softirq::register_for(
                kcore::softirq::TYPE_NET_RX, self, DeviceTable::process_all_rx);
            kcore::softirq::register_for(
                kcore::softirq::TYPE_NET_TX, self, DeviceTable::process_all_tx);
        }

        trace!(0, "net: {} registered, mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            core::str::from_utf8(dev.name()).unwrap_or("?"),
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
        Some(dev)
    }

    /// The next slot, named. Registration is boot's, one driver at a time.
    fn claim(&'static self, name: &str, mac: Mac) -> Option<&'static Device> {
        if name.is_empty() || name.len() >= NAME_MAX || name.as_bytes().contains(&0) {
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

        let mut bytes = [0u8; NAME_MAX];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        let identity = Identity { name: bytes, name_len: name.len(), mac };
        if dev.identity.set(identity).is_err() {
            return None;
        }
        Some(dev)
    }

    /// The receive softirq: harvest every device, then dispatch what it
    /// harvested.
    pub fn process_all_rx(&'static self) {
        /* Test and clear: whoever gets the 1 owns the attribution for this
         * pass. */
        let polled = self.poll_pending.swap(0, Ordering::AcqRel) == 1;
        let mut pending = 0;

        /* This is the one place an `RxContext` comes from, and what makes it
         * true: this function is the receive soft IRQ's handler and nothing
         * else calls it, and the kernel runs a soft IRQ type on one CPU at a
         * time. */
        let mut rx = RxContext { _only_the_receive_pass_makes_one: () };

        for dev in self.devices[..self.count()].iter() {
            dev.harvest(&mut rx);
            /* After the harvest, before the dispatch: what the hardware had
             * waiting. */
            pending += dev.rx_pending();
            dev.drain_rx_and_dispatch(&mut rx);
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
    /// Only with `rxpoll=on`. It was a hypothesis about a driver stall, and
    /// on the machine it was meant for the stall came sooner with it on than
    /// off; and it has a cost. The tick that calls this is the BSP's, so
    /// under load the receive softirq runs on two CPUs in turn -- the BSP and
    /// the one the NIC's interrupt goes to -- with an IPI at every handover:
    /// on the AX41 a flood took 70% of each of two CPUs where it had taken
    /// one. The switch is here, not at the callers, because that is where it
    /// was once lost: it lived in the C++ function the tick called, and went
    /// with it. netload-test.py checks both ways.
    ///
    /// Raised only when the softirq is not already pending, so a pass this
    /// causes can be told from one an interrupt caused -- which is what makes
    /// the stall count evidence rather than a guess.
    pub fn poll_rx(&'static self) {
        if !kcore::net::rx_poll_on() || self.count() == 0 {
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

/* ---- what a module calls ----
 *
 * A module is linked on its own and binds these by name (`kcore::net` is
 * their wrapper); drivers and this crate's services call the device itself.
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
        Some(dev) => dev.handle(),
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
pub extern "C" fn kernel_net_udp_listen(
    dev: usize, port: u16, cb: extern "C" fn(ctx: *mut u8, frame: usize),
    batch_end: Option<extern "C" fn(ctx: *mut u8)>, ctx: *mut u8,
) -> i32 {
    let dev = match DEVICES.by_handle(dev) {
        Some(dev) => dev,
        None => return LISTEN_INVALID,
    };
    match dev.listen_callback(port, cb, batch_end, ctx as usize) {
        Ok(()) => LISTEN_OK,
        Err(ListenError::PortTaken) => LISTEN_PORT_TAKEN,
        Err(ListenError::TableFull) => LISTEN_TABLE_FULL,
        Err(ListenError::Invalid) => LISTEN_INVALID,
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

#[no_mangle]
pub extern "C" fn kernel_net_tx_room(dev: usize) -> usize {
    DEVICES.by_handle(dev).map_or(0, |dev| dev.tx_room())
}

/// Where a frame to `ip` goes on the wire: `ip` itself on the device's
/// subnet, the gateway off it, as ARP answers for it. What a module that
/// builds its own frames needs before the first one, and cannot work out for
/// itself: the lease and the ARP table are the layer's. Task context -- a
/// miss sends a request and sleeps for the answer.
#[no_mangle]
pub extern "C" fn kernel_net_resolve(dev: usize, ip: u32) -> ffi::net::Resolved {
    let nobody = ffi::net::Resolved { found: 0, mac: [0; 6] };

    let (dev, arp) = match (DEVICES.by_handle(dev), crate::abi::arp_table()) {
        (Some(dev), Some(arp)) => (dev, arp),
        _ => return nobody,
    };
    let nic = dev.as_nic();
    match arp.resolve(&nic, nic.route_ip(ip)) {
        Some(mac) => ffi::net::Resolved { found: 1, mac },
        None => nobody,
    }
}

/// Whether the receive path is keeping up: what the load target prints once
/// a second, over the netconsole, for as long as a load runs.
#[no_mangle]
pub extern "C" fn kernel_net_rx_stats() -> ffi::net::RxStats {
    let (polls, work, stalls) = DEVICES.poll_counts();
    ffi::net::RxStats {
        pool_misses: crate::frame::POOL.alloc_misses() as u64,
        pool_in_flight: crate::frame::POOL.in_flight() as u64,
        rx_polls: polls as u64,
        rx_poll_work: work as u64,
        rx_stalls: stalls as u64,
    }
}

/// Look at the receive path without waiting to be asked. From the tick.
#[no_mangle]
pub extern "C" fn rust_net_poll_rx() {
    DEVICES.poll_rx();
}
