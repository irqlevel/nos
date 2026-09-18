//! A UDP server that exists to be hammered from outside, so that `profile`
//! has something to look at other than an idle machine.
//!
//! An idle twenty-CPU box spends about 0.06% of a core on anything at all,
//! and at that level the profile is dominated by the tick's own bookkeeping.
//! The paths worth seeing -- the frame pool, the driver rings, softirq
//! dispatch, the TLB shootdown behind every free -- only appear when packets
//! are actually moving.
//!
//! Echo mode answers every datagram, which exercises receive and transmit
//! together; sink mode drops them, which isolates the receive half. The reply
//! is the frame that arrived with its addresses swapped, built in the receive
//! callback itself -- a load generator that woke a task per packet would be
//! measuring the wakeup -- and the replies of one batch go to the NIC
//! together when it ends.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcore::consts::MAX_CPUS;
use kcore::net::{NetFrame, Nic, UdpListener};
use kcore::task::TaskHandle;
use kcore::trace;

use crate::wire::{eth, ip, udp, ETH_HDR_LEN, IP_HDR_LEN, IP_PROTO_UDP, UDP_HDR_LEN};

pub const DEFAULT_PORT: u16 = 9999;

/// Replies built during one receive batch, before they go to the NIC.
const MAX_PENDING: usize = 64;

const SAMPLE_MS: u64 = 1000;

/// Counters are per CPU and plain, not atomic. This is the datapath the
/// profile is about: one shared cache line incremented by twenty cores would
/// be the loudest line in the report, and the report would be about the
/// instrument. A count lost to a migration between reading the CPU id and
/// adding to its slot costs a statistic nothing worth an atomic.
#[repr(align(64))]
#[derive(Clone, Copy)]
struct PerCpu {
    rx_packets: usize,
    rx_bytes: usize,
    tx_packets: usize,
    tx_failed: usize,
}

const NO_COUNTS: PerCpu = PerCpu { rx_packets: 0, rx_bytes: 0, tx_packets: 0, tx_failed: 0 };

pub struct NetLoad {
    cpu: core::cell::UnsafeCell<[PerCpu; MAX_CPUS]>,

    /// Replies of the batch being dispatched, not yet handed over. Only the
    /// receive softirq touches them, and a softirq type runs on one CPU at a
    /// time, so there is no lock.
    pending: core::cell::UnsafeCell<[usize; MAX_PENDING]>,
    pending_count: core::cell::UnsafeCell<usize>,

    nic: core::cell::UnsafeCell<Option<Nic>>,
    port: core::cell::UnsafeCell<u16>,
    echo: AtomicBool,
    listener: core::cell::UnsafeCell<Option<UdpListener>>,
    task: core::cell::UnsafeCell<Option<TaskHandle>>,
    running: AtomicBool,
    started: AtomicBool,

    /// Sampled once a second by the task, so the shell can report a rate
    /// rather than a total nobody can divide in their head.
    rx_pps: AtomicUsize,
    tx_pps: AtomicUsize,
    rx_bps: AtomicUsize,
}

/* The counters are per CPU and the pending list is the softirq's alone */
unsafe impl Sync for NetLoad {}
unsafe impl Send for NetLoad {}

/// What `netload` reports. The C++ side declares the same struct.
#[repr(C)]
pub struct Stats {
    pub running: u32,
    pub port: u16,
    pub echo: u16,
    pub rx_packets: usize,
    pub rx_bytes: usize,
    pub tx_packets: usize,
    pub tx_failed: usize,
    pub rx_pps: usize,
    pub tx_pps: usize,
    pub rx_bps: usize,
}

impl NetLoad {
    /// Const, so the one of these can be a static rather than something made
    /// on the heap and reached through a pointer from the receive path.
    pub const fn new_const() -> NetLoad {
        NetLoad {
            cpu: core::cell::UnsafeCell::new([NO_COUNTS; MAX_CPUS]),
            pending: core::cell::UnsafeCell::new([0; MAX_PENDING]),
            pending_count: core::cell::UnsafeCell::new(0),
            nic: core::cell::UnsafeCell::new(None),
            port: core::cell::UnsafeCell::new(0),
            echo: AtomicBool::new(true),
            listener: core::cell::UnsafeCell::new(None),
            task: core::cell::UnsafeCell::new(None),
            running: AtomicBool::new(false),
            started: AtomicBool::new(false),
            rx_pps: AtomicUsize::new(0),
            tx_pps: AtomicUsize::new(0),
            rx_bps: AtomicUsize::new(0),
        }
    }

    pub fn is_running(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    pub fn reset_counters(&self) {
        let cpu = unsafe { &mut *self.cpu.get() };
        for slot in cpu.iter_mut() {
            *slot = NO_COUNTS;
        }
        self.rx_pps.store(0, Ordering::Relaxed);
        self.tx_pps.store(0, Ordering::Relaxed);
        self.rx_bps.store(0, Ordering::Relaxed);
    }

    fn totals(&self) -> (usize, usize, usize, usize) {
        let cpu = unsafe { &*self.cpu.get() };
        let mut totals = (0, 0, 0, 0);
        for slot in cpu.iter() {
            totals.0 += slot.rx_packets;
            totals.1 += slot.rx_bytes;
            totals.2 += slot.tx_packets;
            totals.3 += slot.tx_failed;
        }
        totals
    }

    fn slot(&self) -> &mut PerCpu {
        let index = (kcore::cpu::id() as usize).min(MAX_CPUS - 1);
        unsafe { &mut (*self.cpu.get())[index] }
    }

    /// One arrived datagram, from the receive softirq.
    fn on_frame(&self, handle: usize) {
        let frame = unsafe { NetFrame::lent(handle) };
        let len = frame.len();
        if len < ETH_HDR_LEN + IP_HDR_LEN + UDP_HDR_LEN {
            return;
        }

        let packet = &frame[ETH_HDR_LEN..];
        let ip_len = ip::header_len(packet);
        if ip_len == 0 || len < ETH_HDR_LEN + ip_len + UDP_HDR_LEN
            || ip::protocol(packet) != IP_PROTO_UDP
        {
            return;
        }

        {
            let slot = self.slot();
            slot.rx_packets += 1;
            slot.rx_bytes += len;
        }

        if !self.echo.load(Ordering::Relaxed) {
            return;
        }
        let nic = match unsafe { *self.nic.get() } {
            Some(nic) => nic,
            None => return,
        };

        /* The reply is the frame that arrived, its addresses swapped where
         * they lie -- no copy and no allocation.
         *
         * `udp::send` must never be called from here. It resolves through
         * ARP, which on a cache miss sends a request and then sleeps up to
         * three seconds waiting. This is the receive dispatch path: sleeping
         * in it stops every packet the machine would otherwise process, the
         * ICMP it needs to answer a ping and the datagrams carrying the shell
         * included. ARP entries expire after five minutes, so that miss is
         * not a rare case -- it is one every load test long enough to be
         * interesting. */
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                ffi::net::kernel_netframe_data(handle), len)
        };

        let requester = eth::src(bytes);
        eth::write(bytes, &requester, &nic.mac(), crate::wire::ETH_TYPE_IP);

        let packet = &mut bytes[ETH_HDR_LEN..];
        let (from, to) = (ip::src(packet), ip::dst(packet));
        crate::wire::set_be32(packet, ip::SRC, to);
        crate::wire::set_be32(packet, ip::DST, from);
        packet[ip::TTL] = 64;
        crate::wire::set_be16(packet, ip::CHECKSUM, 0);
        let sum = crate::wire::checksum(&packet[..ip_len]);
        crate::wire::set_be16(packet, ip::CHECKSUM, sum);

        let datagram = &mut bytes[ETH_HDR_LEN + ip_len..];
        let (src_port, dst_port) = (udp::src_port(datagram), udp::dst_port(datagram));
        crate::wire::set_be16(datagram, udp::SRC_PORT, dst_port);
        crate::wire::set_be16(datagram, udp::DST_PORT, src_port);
        /* Zero means "not computed", which IPv4 allows and which is what
         * this saves a pass over the payload for. */
        crate::wire::set_be16(datagram, udp::CHECKSUM, 0);

        /* Kept past this callback -- the dispatcher's release is then not the
         * last one -- and sent with the rest of the batch when it ends, or
         * now, if the batch has filled what is kept. */
        let kept = unsafe { NetFrame::retain(handle) };
        unsafe {
            let count = &mut *self.pending_count.get();
            (*self.pending.get())[*count] = kept.into_raw();
            *count += 1;
            if *count == MAX_PENDING {
                self.flush_replies();
            }
        }
    }

    /// The batch's replies to the NIC in one submission: one transmit lock
    /// and one doorbell for the lot. Each echo used to take both on its own,
    /// and under a flood the lock's release, just after the doorbell, was
    /// where a profile found the receive CPU spending most.
    fn flush_replies(&self) {
        let count = unsafe { *self.pending_count.get() };
        if count == 0 {
            return;
        }
        unsafe { *self.pending_count.get() = 0 };

        let nic = match unsafe { *self.nic.get() } {
            Some(nic) => nic,
            None => return,
        };

        let pending = unsafe { &(&*self.pending.get())[..count] };
        /* Every frame is taken: what found no room is released on the far
         * side. */
        let queued = unsafe { nic.transmit_raw(pending) };

        let slot = self.slot();
        slot.tx_packets += queued;
        slot.tx_failed += count - queued;
    }

    pub fn start(&'static self, nic: Nic, port: u16, echo: bool) -> bool {
        if port == 0 || self.started.swap(true, Ordering::AcqRel) {
            return false;
        }

        unsafe {
            *self.nic.get() = Some(nic);
            *self.port.get() = port;
            *self.pending_count.get() = 0;
        }
        self.echo.store(echo, Ordering::Release);
        self.reset_counters();

        let task = match kcore::task::spawn_with_ctx(
            "netload", run, self as *const _ as *mut u8)
        {
            Some(task) => task,
            None => {
                self.started.store(false, Ordering::Release);
                return false;
            }
        };

        /* Listener slots are few, and DHCP, DNS and the shell have taken
         * theirs already: a full table is a real outcome and has to be
         * reported, not left as a server that is running and never
         * dispatched to -- and so is a port someone else has. */
        match nic.listen_udp_batched(port, on_frame, on_batch_end,
            self as *const _ as *mut u8)
        {
            Ok(listener) => unsafe {
                *self.listener.get() = Some(listener);
                *self.task.get() = Some(task);
            },
            Err(err) => {
                trace!(0, "netload: port {} could not be listened on ({:?})", port, err);
                task.request_stop();
                drop(task);
                self.started.store(false, Ordering::Release);
                return false;
            }
        }

        self.running.store(true, Ordering::Release);
        trace!(0, "netload: started on port {}, {}", port, if echo { "echo" } else { "sink" });
        true
    }

    pub fn stop(&self) {
        if !self.started.load(Ordering::Acquire) {
            return;
        }

        /* Before the listener goes: a callback already inside `on_frame`
         * finishes, and the flag keeps a later one from starting. */
        self.running.store(false, Ordering::Release);

        /* Dropping the listener returns once no callback is still running --
         * a batch's end included, which hands over what that batch built --
         * so nothing should be left kept here; were anything, it goes out
         * rather than leaking. */
        unsafe { *self.listener.get() = None };
        self.flush_replies();

        let task = unsafe { (*self.task.get()).take() };
        if let Some(task) = task {
            task.request_stop();
            drop(task);
        }

        let port = unsafe { *self.port.get() };
        trace!(0, "netload: stopped on port {}", port);

        unsafe {
            *self.nic.get() = None;
            *self.port.get() = 0;
        }
        self.started.store(false, Ordering::Release);
    }

    fn run(&self) {
        let (mut last_rx, mut last_bytes, mut last_tx, _) = self.totals();

        while !kcore::task::stopping() {
            kcore::task::sleep_ms(SAMPLE_MS);

            let (rx, bytes, tx, failed) = self.totals();
            /* One second per sample, so the difference is the rate. */
            self.rx_pps.store(delta(rx, last_rx), Ordering::Relaxed);
            self.tx_pps.store(delta(tx, last_tx), Ordering::Relaxed);
            self.rx_bps.store(delta(bytes, last_bytes), Ordering::Relaxed);

            /* One line a second, over the netconsole, for as long as the load
             * runs. The point is not the numbers: it is that the line keeps
             * arriving. A machine goes deaf under load -- the shell stops
             * answering and so does ping -- and every channel that could say
             * why is a network channel. The netconsole only sends, so if
             * these lines continue after the machine has stopped receiving,
             * the machine is alive and the receive path is what died; if they
             * stop with it, the kernel itself is wedged. Nothing else here
             * can tell those apart. */
            let (misses, in_flight) = kcore::net::frame_pool_stats();
            let (polls, work, stalls) = kcore::net::rx_poll_stats();
            trace!(0, "netload: rx {} (+{}), tx {}, failed {}, pool misses {}, in flight {}, rx polls {}, poll work {}, stalls {}",
                rx, self.rx_pps.load(Ordering::Relaxed), tx, failed,
                misses, in_flight, polls, work, stalls);

            last_rx = rx;
            last_bytes = bytes;
            last_tx = tx;
        }
    }

    pub fn stats(&self) -> Stats {
        let (rx_packets, rx_bytes, tx_packets, tx_failed) = self.totals();
        Stats {
            running: self.is_running() as u32,
            port: unsafe { *self.port.get() },
            echo: self.echo.load(Ordering::Relaxed) as u16,
            rx_packets,
            rx_bytes,
            tx_packets,
            tx_failed,
            rx_pps: self.rx_pps.load(Ordering::Relaxed),
            tx_pps: self.tx_pps.load(Ordering::Relaxed),
            rx_bps: self.rx_bps.load(Ordering::Relaxed),
        }
    }

    /// What the index'th CPU received, for the per-CPU line: a load test that
    /// runs entirely on one core is measuring one core.
    pub fn cpu_rx(&self, index: usize) -> usize {
        if index >= MAX_CPUS {
            return 0;
        }
        unsafe { (*self.cpu.get())[index].rx_packets }
    }
}

/// A rate from two samples of a counter. `netload reset` zeroes the counters
/// under the sampler, so one found below its last sample has started again:
/// what it holds is the count since, not a difference to wrap around.
fn delta(now: usize, last: usize) -> usize {
    if now >= last { now - last } else { now }
}

extern "C" fn run(ctx: *mut u8) {
    if ctx.is_null() {
        return;
    }
    unsafe { &*(ctx as *const NetLoad) }.run();
}

extern "C" fn on_frame(ctx: *mut u8, frame: usize) {
    if ctx.is_null() {
        return;
    }
    let load = unsafe { &*(ctx as *const NetLoad) };
    if !load.running.load(Ordering::Acquire) {
        return;
    }
    load.on_frame(frame);
}

extern "C" fn on_batch_end(ctx: *mut u8) {
    if ctx.is_null() {
        return;
    }
    unsafe { &*(ctx as *const NetLoad) }.flush_replies();
}
