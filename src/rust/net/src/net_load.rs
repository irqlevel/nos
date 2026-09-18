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

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};

use kcore::net::{AtomicNic, Lent, Nic, RxContext, RxOwned, TxBatch, UdpHandler, UdpListener};
use kcore::percpu::{ConstInit, LocalCounter, PerCpu};
use kcore::sync::PreemptSpinLock;
use kcore::task::TaskHandle;
use kcore::trace;

use crate::wire::{eth, ip, udp, ETH_HDR_LEN, IP_HDR_LEN, IP_PROTO_UDP, UDP_HDR_LEN};

pub const DEFAULT_PORT: u16 = 9999;

/// Replies built during one receive batch, before they go to the NIC.
const MAX_PENDING: usize = 64;

const SAMPLE_MS: u64 = 1000;

/// Counters are per CPU, and added to with no bus lock. This is the datapath
/// the profile is about: one shared cache line incremented by twenty cores
/// would be the loudest line in the report, and the report would be about
/// the instrument. A count lost to a migration between reading the CPU id
/// and adding to its slot costs a statistic nothing worth a locked
/// instruction.
#[repr(align(64))]
struct Counts {
    rx_packets: LocalCounter,
    rx_bytes: LocalCounter,
    tx_packets: LocalCounter,
    tx_failed: LocalCounter,
}

impl ConstInit for Counts {
    const INIT: Self = Counts {
        rx_packets: LocalCounter::new(),
        rx_bytes: LocalCounter::new(),
        tx_packets: LocalCounter::new(),
        tx_failed: LocalCounter::new(),
    };
}

pub struct NetLoad {
    counts: PerCpu<Counts>,

    /// Replies of the batch being dispatched, not yet handed over. The
    /// receive path's own -- it is reached with the `RxContext` a callback is
    /// lent -- so there is no lock on the way to it.
    pending: RxOwned<TxBatch<MAX_PENDING>>,

    /// Read once a packet
    nic: AtomicNic,
    port: AtomicU16,
    echo: AtomicBool,
    /* Never dropped under a lock: giving a port back waits for the receive
     * path, and giving a task back waits for the task. */
    listener: PreemptSpinLock<Option<UdpListener>>,
    task: PreemptSpinLock<Option<TaskHandle>>,
    running: AtomicBool,
    started: AtomicBool,

    /// Sampled once a second by the task, so the shell can report a rate
    /// rather than a total nobody can divide in their head.
    rx_pps: AtomicUsize,
    tx_pps: AtomicUsize,
    rx_bps: AtomicUsize,
}

/// What `netload` reports.
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
            counts: PerCpu::new(),
            pending: RxOwned::new(TxBatch::new()),
            nic: AtomicNic::none(),
            port: AtomicU16::new(0),
            echo: AtomicBool::new(true),
            listener: PreemptSpinLock::new(None),
            task: PreemptSpinLock::new(None),
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
        for counts in self.counts.iter() {
            counts.rx_packets.set(0);
            counts.rx_bytes.set(0);
            counts.tx_packets.set(0);
            counts.tx_failed.set(0);
        }
        self.rx_pps.store(0, Ordering::Relaxed);
        self.tx_pps.store(0, Ordering::Relaxed);
        self.rx_bps.store(0, Ordering::Relaxed);
    }

    fn totals(&self) -> (usize, usize, usize, usize) {
        let mut totals = (0, 0, 0, 0);
        for counts in self.counts.iter() {
            totals.0 += counts.rx_packets.get();
            totals.1 += counts.rx_bytes.get();
            totals.2 += counts.tx_packets.get();
            totals.3 += counts.tx_failed.get();
        }
        totals
    }

    /// One arrived datagram, from the receive softirq.
    fn receive(&self, frame: Lent<'_>, rx: &mut RxContext) {
        let ip_len = {
            let bytes = frame.bytes();
            let len = bytes.len();
            if len < ETH_HDR_LEN + IP_HDR_LEN + UDP_HDR_LEN {
                return;
            }

            let packet = &bytes[ETH_HDR_LEN..];
            let ip_len = ip::header_len(packet);
            if ip_len == 0 || len < ETH_HDR_LEN + ip_len + UDP_HDR_LEN
                || ip::protocol(packet) != IP_PROTO_UDP
            {
                return;
            }

            let counts = self.counts.here();
            counts.rx_packets.add(1);
            counts.rx_bytes.add(len);
            ip_len
        };

        if !self.echo.load(Ordering::Relaxed) {
            return;
        }
        let nic = match self.nic.get() {
            Some(nic) => nic,
            None => return,
        };

        /* The reply is the frame that arrived, its addresses swapped where
         * they lie -- no copy and no allocation. Kept past this callback --
         * the dispatcher's release is then not the last one.
         *
         * `udp::send` must never be called from here. It resolves through
         * ARP, which on a cache miss sends a request and then sleeps up to
         * three seconds waiting. This is the receive dispatch path: sleeping
         * in it stops every packet the machine would otherwise process, the
         * ICMP it needs to answer a ping and the datagrams carrying the shell
         * included. ARP entries expire after five minutes, so that miss is
         * not a rare case -- it is one every load test long enough to be
         * interesting. */
        let mut kept = frame.retain();
        let bytes = kept.data_mut();

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

        /* Sent with the rest of the batch when it ends, or now, if the batch
         * has filled what is kept. */
        let pending = self.pending.get(rx);
        if !pending.push(kept) {
            self.counts.here().tx_failed.add(1);
        }
        if pending.is_full() {
            self.flush_replies(rx);
        }
    }

    /// The batch's replies to the NIC in one submission: one transmit lock
    /// and one doorbell for the lot. Each echo used to take both on its own,
    /// and under a flood the lock's release, just after the doorbell, was
    /// where a profile found the receive CPU spending most.
    fn flush_replies(&self, rx: &mut RxContext) {
        let pending = self.pending.get(rx);
        let count = pending.len();
        if count == 0 {
            return;
        }

        let queued = match self.nic.get() {
            /* Every frame is taken: what found no room is released on the
             * far side. */
            Some(nic) => pending.send(&nic),
            None => {
                pending.clear();
                0
            }
        };

        let counts = self.counts.here();
        counts.tx_packets.add(queued);
        counts.tx_failed.add(count - queued);
    }

    pub fn start(&'static self, nic: Nic, port: u16, echo: bool) -> bool {
        if port == 0 || self.started.swap(true, Ordering::AcqRel) {
            return false;
        }

        self.nic.set(Some(nic));
        self.port.store(port, Ordering::Release);
        self.echo.store(echo, Ordering::Release);
        self.reset_counters();

        let task = match kcore::task::spawn_for("netload", self, NetLoad::run) {
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
        match nic.listen_batched(port, self) {
            Ok(listener) => {
                *self.listener.lock() = Some(listener);
                *self.task.lock() = Some(task);
            }
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

        /* Before the listener goes: a callback already inside `receive`
         * finishes, and the flag keeps a later one from starting. */
        self.running.store(false, Ordering::Release);

        /* Dropping the listener returns once no callback is still running --
         * a batch's end included, which hands over what that batch built --
         * so nothing is left gathered after it. Taken out under the lock and
         * dropped after: the drop waits. */
        let listener = self.listener.lock().take();
        drop(listener);

        let task = self.task.lock().take();
        if let Some(task) = task {
            task.request_stop();
            drop(task);
        }

        trace!(0, "netload: stopped on port {}", self.port.load(Ordering::Acquire));

        self.nic.set(None);
        self.port.store(0, Ordering::Release);
        self.started.store(false, Ordering::Release);
    }

    fn run(&'static self) {
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
            let (misses, in_flight) =
                (crate::frame::POOL.alloc_misses(), crate::frame::POOL.in_flight());
            let (polls, work, stalls) = crate::device::DEVICES.poll_counts();
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
            port: self.port.load(Ordering::Acquire),
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
        self.counts.get(index).map_or(0, |counts| counts.rx_packets.get())
    }
}

/// A rate from two samples of a counter. `netload reset` zeroes the counters
/// under the sampler, so one found below its last sample has started again:
/// what it holds is the count since, not a difference to wrap around.
fn delta(now: usize, last: usize) -> usize {
    if now >= last { now - last } else { now }
}

/// What the receive path hands every datagram on the load port to, and tells
/// when a batch of them has ended.
impl UdpHandler for NetLoad {
    fn on_frame(&'static self, frame: Lent<'_>, rx: &mut RxContext) {
        if self.running.load(Ordering::Acquire) {
            self.receive(frame, rx);
        }
    }

    fn on_batch_end(&'static self, rx: &mut RxContext) {
        self.flush_replies(rx);
    }
}
