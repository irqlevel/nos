#![no_std]

//! netload: a UDP load test, from either end of the wire -- so that `profile`
//! has something to look at other than an idle machine.
//!
//!     insmod /netload.ko
//!     netload start                       the target: echo on port 9999
//!     netload start 7000 sink             ... or count and answer nothing
//!     netload send 10.0.0.2 9999          the source: as fast as the NIC takes
//!     netload send 10.0.0.2 9999 size=1472 pps=50000 secs=30 tasks=4
//!     netload                             what either has counted so far
//!     netload stop
//!
//! An idle twenty-CPU box spends about 0.06% of a core on anything at all,
//! and at that level the profile is dominated by the tick's own bookkeeping.
//! The paths worth seeing -- the frame pool, the driver rings, softirq
//! dispatch, the TLB shootdown behind every free -- only appear when packets
//! are actually moving. Two machines running this, one each way, move them
//! with nothing else in the picture.
//!
//! **The target.** Echo mode answers every datagram, which exercises receive
//! and transmit together; sink mode drops them, which isolates the receive
//! half. The reply is the frame that arrived with its addresses swapped,
//! built in the receive callback itself -- a load generator that woke a task
//! per packet would be measuring the wakeup -- and the replies of one batch
//! go to the NIC together when it ends.
//!
//! **The source.** `tasks` sender tasks each build their datagrams straight
//! into frames off the pool -- one copy of a template made once, and a
//! sequence number -- and hand them to the NIC a batch at a time. The
//! destination's Ethernet address is asked of ARP once, before the first
//! frame; if nothing answers, nothing is sent -- a load test does not fall
//! back to broadcast. What comes back to the source port is counted and
//! never answered, so a source pointed at a target of this same module sees
//! how much of what it sent made the round trip, and two sources pointed at
//! each other do not bounce one datagram for good.
//!
//! A datagram the source sends says which one it is: `NLD1`, the sender's
//! number, a sequence number -- both big-endian, four bytes and eight -- and
//! then filler whose every byte is its own offset. So whatever receives them
//! can tell what was lost, what was duplicated and what was damaged.
//!
//! Nothing here is `unsafe`: the frame a listener is lent, the receive
//! path's own state and the batch of frames are `kcore::net`'s types, and
//! the formats are `netwire`'s.

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kcore::cmd::{Command, Output};
use kcore::consts::MAX_CPUS;
use kcore::net::{Lent, NetFrame, Nic, RxContext, RxOwned, TxBatch, UdpHandler, UdpListener};
use kcore::percpu::{ConstInit, LocalCounter, PerCpu};
use kcore::sync::{PreemptSpinLock, TryLock};
use kcore::task::TaskHandle;
use kcore::time::{boot_time_ns, Duration};
use kcore::trace;

use netwire::{eth, ip, udp, Ipv4, Mac, ETH_HDR_LEN, ETH_TYPE_IP, IP_HDR_LEN, IP_PROTO_UDP,
              UDP_HDR_LEN};

const HELP: &str =
    "netload [start [port] [sink] | send <ip> <port> [opt=..] | stop | reset] - udp load test";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const USAGE: &str = "usage: netload [start [port] [sink] | send <ip> <port> [size=64] [pps=0] \
[secs=0] [count=0] [tasks=1] [sport=9998] | stop | reset]";

const DEFAULT_NIC: &str = "eth0";

/// Where the target listens unless told otherwise.
const DEFAULT_PORT: u16 = 9999;
/// Where the source sends from, and hears its echoes: not the target's, so
/// that one machine can be both.
const DEFAULT_SOURCE_PORT: u16 = 9998;

/// Replies built during one receive batch, before they go to the NIC.
const MAX_PENDING: usize = 64;
/// Datagrams a sender builds before it hands them over: one transmit lock
/// and one doorbell for the lot.
const SEND_BATCH: usize = 32;

/// What a datagram from the source starts with.
const MAGIC: [u8; 4] = *b"NLD1";
const SENDER_AT: usize = 4;
const SEQ_AT: usize = 8;
/// Magic, sender and sequence: the least a datagram from the source holds.
const MIN_SIZE: usize = 16;
const DEFAULT_SIZE: usize = 64;

const MAX_TASKS: usize = 16;

const NS_PER_SEC: u64 = 1_000_000_000;
const SAMPLE_NS: u64 = NS_PER_SEC;
/// How long a task sleeps between looks at whether it has been told to stop,
/// so that a `netload stop` does not wait out a whole sample.
const STOP_POLL_MS: u64 = 100;
/// Room a sender leaves in the NIC's transmit queue, of the 256 frames it
/// holds. A source that takes every slot the moment one opens keeps the
/// queue full for as long as it runs, and everybody else's frames then find
/// no room and are released: the shell's answers, the netconsole's lines --
/// on a machine whose only console is the network, the load test would
/// silence the one channel that says how it is going.
const TX_HEADROOM: usize = 64;
/// A sender with nothing due yet at the rate it was given.
const PACE_SLEEP: Duration = Duration::from_millis(1);
/// A sender the NIC's queue has no room for: long enough for the queue to
/// drain a good part of itself at a gigabit, short enough not to leave the
/// wire idle.
const FULL_QUEUE_SLEEP: Duration = Duration::from_nanos(100_000);

/* ------------------------------------------------------------------ */
/* Counters                                                            */
/* ------------------------------------------------------------------ */

/// Counters are per CPU, and added to with no bus lock. This is the datapath
/// the profile is about: one shared cache line incremented by twenty cores
/// would be the loudest line in the report, and the report would be about
/// the instrument. A count lost to a migration between reading the CPU id
/// and adding to its slot costs a statistic nothing worth a locked
/// instruction.
///
/// The target and the source each have a set, so that one machine can be
/// both and still say which did what. For the target `rx` is what arrived
/// and `tx` what it echoed; for the source `tx` is what it sent and `rx`
/// what came back.
#[repr(align(64))]
struct Counts {
    rx_packets: LocalCounter,
    rx_bytes: LocalCounter,
    /// Datagrams to our port that were not what we were listening for: for
    /// the source, not an echo from where it is sending.
    rx_strangers: LocalCounter,
    tx_packets: LocalCounter,
    tx_bytes: LocalCounter,
    /// Frames the NIC's queue had no room for, or the pool no frame for.
    tx_failed: LocalCounter,
}

impl ConstInit for Counts {
    const INIT: Self = Counts {
        rx_packets: LocalCounter::new(),
        rx_bytes: LocalCounter::new(),
        rx_strangers: LocalCounter::new(),
        tx_packets: LocalCounter::new(),
        tx_bytes: LocalCounter::new(),
        tx_failed: LocalCounter::new(),
    };
}

/// The module's own, so in its image rather than on anybody's stack or heap:
/// a set is a cache line for each CPU there could be.
static TARGET_COUNTS: PerCpu<Counts> = PerCpu::new();
static SOURCE_COUNTS: PerCpu<Counts> = PerCpu::new();

#[derive(Clone, Copy, Default)]
struct Totals {
    rx_packets: usize,
    rx_bytes: usize,
    rx_strangers: usize,
    tx_packets: usize,
    tx_bytes: usize,
    tx_failed: usize,
}

fn totals(counts: &PerCpu<Counts>) -> Totals {
    let mut sum = Totals::default();
    for cpu in counts.iter() {
        sum.rx_packets += cpu.rx_packets.get();
        sum.rx_bytes += cpu.rx_bytes.get();
        sum.rx_strangers += cpu.rx_strangers.get();
        sum.tx_packets += cpu.tx_packets.get();
        sum.tx_bytes += cpu.tx_bytes.get();
        sum.tx_failed += cpu.tx_failed.get();
    }
    sum
}

fn clear(counts: &PerCpu<Counts>) {
    for cpu in counts.iter() {
        cpu.rx_packets.set(0);
        cpu.rx_bytes.set(0);
        cpu.rx_strangers.set(0);
        cpu.tx_packets.set(0);
        cpu.tx_bytes.set(0);
        cpu.tx_failed.set(0);
    }
}

/// What a second's worth of the counters came to, sampled by a task so the
/// shell can report a rate rather than a total nobody can divide in their
/// head.
struct Rates {
    rx_pps: AtomicUsize,
    tx_pps: AtomicUsize,
    /// Bytes a second, of whichever way is the point: received for the
    /// target, sent for the source.
    bps: AtomicUsize,
}

impl Rates {
    const fn new() -> Self {
        Rates { rx_pps: AtomicUsize::new(0), tx_pps: AtomicUsize::new(0), bps: AtomicUsize::new(0) }
    }

    fn clear(&self) {
        self.rx_pps.store(0, Ordering::Relaxed);
        self.tx_pps.store(0, Ordering::Relaxed);
        self.bps.store(0, Ordering::Relaxed);
    }
}

/// A rate from two samples of a counter. `netload reset` zeroes the counters
/// under the sampler, so one found below its last sample has started again:
/// what it holds is the count since, not a difference to wrap around.
fn delta(now: usize, last: usize) -> usize {
    if now >= last { now - last } else { now }
}

/// Sleeps a sample's length, in pieces short enough that being told to stop
/// is heard soon. False once it has been.
fn sleep_a_sample() -> bool {
    let until = boot_time_ns().saturating_add(SAMPLE_NS);
    while boot_time_ns() < until {
        if kcore::task::stopping() {
            return false;
        }
        kcore::task::sleep_ms(STOP_POLL_MS);
    }
    !kcore::task::stopping()
}

/* ------------------------------------------------------------------ */
/* The target                                                          */
/* ------------------------------------------------------------------ */

struct Target {
    nic: Nic,
    /// Asked once: every reply carries it, and asking is a call out of the
    /// module on a path that exists to be measured.
    mac: Mac,
    port: u16,
    echo: bool,

    /// Replies of the batch being dispatched, not yet handed over. The
    /// receive path's own -- it is reached with the `RxContext` a callback is
    /// lent -- so there is no lock on the way to it.
    pending: RxOwned<TxBatch<MAX_PENDING>>,

    /// Cleared before the listener goes: a callback already inside finishes,
    /// and the flag keeps a later one from starting.
    running: AtomicBool,
    rates: Rates,
}

impl Target {
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

            let counts = TARGET_COUNTS.here();
            counts.rx_packets.add(1);
            counts.rx_bytes.add(len);
            ip_len
        };

        if !self.echo {
            return;
        }

        /* The reply is the frame that arrived, its addresses swapped where
         * they lie -- no copy and no allocation. Kept past this callback --
         * the dispatcher's release is then not the last one.
         *
         * Nothing that resolves an address may be called from here. ARP, on
         * a cache miss, sends a request and then sleeps up to three seconds
         * waiting. This is the receive dispatch path: sleeping in it stops
         * every packet the machine would otherwise process, the ICMP it
         * needs to answer a ping and the datagrams carrying the shell
         * included. ARP entries expire after five minutes, so that miss is
         * not a rare case -- it is one every load test long enough to be
         * interesting. The requester's address is in the frame: that is the
         * one the reply goes to. */
        let mut kept = frame.retain();
        let bytes = kept.data_mut();

        let requester = eth::src(bytes);
        eth::write(bytes, &requester, &self.mac, ETH_TYPE_IP);

        let packet = &mut bytes[ETH_HDR_LEN..];
        let (from, to) = (ip::src(packet), ip::dst(packet));
        netwire::set_be32(packet, ip::SRC, to);
        netwire::set_be32(packet, ip::DST, from);
        packet[ip::TTL] = 64;
        netwire::set_be16(packet, ip::CHECKSUM, 0);
        let sum = netwire::checksum(&packet[..ip_len]);
        netwire::set_be16(packet, ip::CHECKSUM, sum);

        let datagram = &mut bytes[ETH_HDR_LEN + ip_len..];
        let (src_port, dst_port) = (udp::src_port(datagram), udp::dst_port(datagram));
        netwire::set_be16(datagram, udp::SRC_PORT, dst_port);
        netwire::set_be16(datagram, udp::DST_PORT, src_port);
        /* Zero means "not computed", which IPv4 allows and which is what
         * this saves a pass over the payload for. */
        netwire::set_be16(datagram, udp::CHECKSUM, 0);

        /* Sent with the rest of the batch when it ends, or now, if the batch
         * has filled what is kept. */
        let pending = self.pending.get(rx);
        if !pending.push(kept) {
            TARGET_COUNTS.here().tx_failed.add(1);
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

        /* Every frame is taken: what found no room is released on the far
         * side. */
        let queued = pending.send(&self.nic);

        let counts = TARGET_COUNTS.here();
        counts.tx_packets.add(queued);
        counts.tx_failed.add(count - queued);
    }

    /// The target's task: a rate for the shell, and a line a second.
    fn sample(self: Arc<Self>) {
        let mut last = totals(&TARGET_COUNTS);

        while sleep_a_sample() {
            let now = totals(&TARGET_COUNTS);
            /* One second per sample, so the difference is the rate. */
            self.rates.rx_pps.store(delta(now.rx_packets, last.rx_packets), Ordering::Relaxed);
            self.rates.tx_pps.store(delta(now.tx_packets, last.tx_packets), Ordering::Relaxed);
            self.rates.bps.store(delta(now.rx_bytes, last.rx_bytes), Ordering::Relaxed);

            /* One line a second, over the netconsole, for as long as the load
             * runs. The point is not the numbers: it is that the line keeps
             * arriving. A machine goes deaf under load -- the shell stops
             * answering and so does ping -- and every channel that could say
             * why is a network channel. The netconsole only sends, so if
             * these lines continue after the machine has stopped receiving,
             * the machine is alive and the receive path is what died; if they
             * stop with it, the kernel itself is wedged. Nothing else here
             * can tell those apart. */
            let path = kcore::net::rx_stats();
            trace!(0, "netload: rx {} (+{}), tx {}, failed {}, pool misses {}, in flight {}, rx polls {}, poll work {}, stalls {}",
                now.rx_packets, self.rates.rx_pps.load(Ordering::Relaxed), now.tx_packets,
                now.tx_failed, path.pool_misses, path.pool_in_flight, path.rx_polls,
                path.rx_poll_work, path.rx_stalls);

            last = now;
        }
    }
}

/// What the receive path hands every datagram on the load port to, and tells
/// when a batch of them has ended.
impl UdpHandler for Target {
    fn on_frame(&self, frame: Lent<'_>, rx: &mut RxContext) {
        if self.running.load(Ordering::Acquire) {
            self.receive(frame, rx);
        }
    }

    fn on_batch_end(&self, rx: &mut RxContext) {
        self.flush_replies(rx);
    }
}

/// A target while it runs. Dropping one takes it down, in the order that
/// leaves nothing gathered and nothing running.
struct TargetRun {
    target: Arc<Target>,
    listener: Option<UdpListener>,
    sampler: Option<TaskHandle>,
}

impl Drop for TargetRun {
    fn drop(&mut self) {
        /* Before the listener goes: see `Target::running`. */
        let was_up = self.target.running.swap(false, Ordering::AcqRel);

        /* Dropping the listener returns once no callback is still running --
         * a batch's end included, which hands over what that batch built --
         * so nothing is left gathered after it. */
        drop(self.listener.take());

        if let Some(sampler) = self.sampler.take() {
            sampler.request_stop();
            drop(sampler);
        }
        if was_up {
            trace!(0, "netload: stopped on port {}", self.target.port);
        }
    }
}

/* ------------------------------------------------------------------ */
/* The source                                                          */
/* ------------------------------------------------------------------ */

/// What `netload send` was asked for.
#[derive(Clone, Copy)]
struct SendPlan {
    ip: u32,
    port: u16,
    source_port: u16,
    /// A datagram's payload, in bytes.
    size: usize,
    /// Datagrams a second over all senders; 0 for as fast as the NIC takes.
    pps: u64,
    /// 0 for until stopped.
    secs: u64,
    /// Datagrams over all senders; 0 for no limit.
    count: u64,
    tasks: usize,
}

struct Source {
    nic: Nic,
    plan: SendPlan,
    /// A whole frame, headers and filler: what every datagram starts as.
    template: Box<[u8]>,
    /// When the run is over whatever else, in boot time; 0 for never.
    deadline_ns: u64,
    started_ns: u64,
    /// When the last sender finished, in boot time; 0 while one runs.
    finished_ns: AtomicU64,
    /// Senders still sending.
    active: AtomicUsize,
    rates: Rates,
}

/// What one sender is to do: its number, and how much of the whole is its.
struct Share {
    sender: u32,
    /// 0 for no limit.
    count: u64,
    /// 0 for no limit.
    pps: u64,
}

impl Source {
    /// The frame every datagram is a copy of. None when the plan's size is
    /// not one a datagram can have.
    fn template(plan: &SendPlan, route: &udp::Route) -> Option<Box<[u8]>> {
        let mut frame = alloc::vec![0u8; udp::PAYLOAD_AT + plan.size];
        let len = udp::write_frame(&mut frame, route, plan.size)?;
        debug_assert!(len == frame.len());

        let payload = &mut frame[udp::PAYLOAD_AT..];
        for (at, byte) in payload.iter_mut().enumerate() {
            *byte = at as u8;
        }
        payload[..MAGIC.len()].copy_from_slice(&MAGIC);
        Some(frame.into_boxed_slice())
    }

    /// One sender's whole life: batches of datagrams until its share is sent,
    /// the run's time is up or it is told to stop.
    fn send(&self, share: &Share) {
        let counts = &SOURCE_COUNTS;
        let frame_len = self.template.len();
        let started = boot_time_ns();

        let mut batch: TxBatch<SEND_BATCH> = TxBatch::new();
        let mut seq: u64 = 0;

        while !kcore::task::stopping() {
            if share.count != 0 && seq >= share.count {
                break;
            }
            let now = boot_time_ns();
            if self.deadline_ns != 0 && now >= self.deadline_ns {
                break;
            }

            /* No more than the NIC's queue has room for, less what is left
             * for everybody else: what finds no room is released, not kept
             * for later, and a source that loses its own datagrams before
             * the wire is measuring itself. Another sender may take the room
             * first; that is what `failed` then counts. */
            let room = self.nic.tx_room().saturating_sub(TX_HEADROOM) as u64;
            if room == 0 {
                kcore::task::sleep(FULL_QUEUE_SLEEP);
                continue;
            }

            let mut burst = room.min(SEND_BATCH as u64);
            if share.count != 0 {
                burst = burst.min(share.count - seq);
            }
            if share.pps != 0 {
                /* What the rate says should have gone by now, less what has.
                 * Whole seconds and the rest apart, so that neither product
                 * can outgrow a u64 however long the run. */
                let elapsed = now - started;
                let due = (elapsed / NS_PER_SEC) * share.pps
                    + (elapsed % NS_PER_SEC) * share.pps / NS_PER_SEC;
                let owed = due.saturating_sub(seq);
                if owed == 0 {
                    kcore::task::sleep(PACE_SLEEP);
                    continue;
                }
                burst = burst.min(owed);
            }

            let mut built: u64 = 0;
            while built < burst {
                let mut frame = match NetFrame::alloc_tx(frame_len) {
                    Some(frame) => frame,
                    None => break,
                };
                let room = frame.data_raw_mut(frame_len);
                if room.len() < frame_len {
                    /* A frame with no room for what was asked of it: never
                     * from this pool, and not something to write past. */
                    break;
                }
                room.copy_from_slice(&self.template);

                let payload = &mut room[udp::PAYLOAD_AT..];
                payload[SENDER_AT..SENDER_AT + 4].copy_from_slice(&share.sender.to_be_bytes());
                payload[SEQ_AT..SEQ_AT + 8].copy_from_slice(&(seq + built).to_be_bytes());

                frame.set_len(frame_len);
                if !batch.push(frame) {
                    break;
                }
                built += 1;
            }

            if built == 0 {
                /* The pool had nothing: every frame is out, most of them on
                 * their way through the NIC. */
                counts.here().tx_failed.add(1);
                kcore::task::sleep(FULL_QUEUE_SLEEP);
                continue;
            }

            let queued = batch.send(&self.nic) as u64;
            /* A sequence number is spent whether or not its frame found room:
             * what the far end misses is then what this end counted failed. */
            seq += built;

            let here = counts.here();
            here.tx_packets.add(queued as usize);
            here.tx_bytes.add(queued as usize * frame_len);
            here.tx_failed.add((built - queued) as usize);

            if queued < built {
                /* Somebody else took the room that was there a moment ago,
                 * and what did not fit is gone. Let the queue drain. */
                kcore::task::sleep(FULL_QUEUE_SLEEP);
            }
        }

        if self.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.finished_ns.store(boot_time_ns().max(1), Ordering::Release);
        }
    }

    fn is_done(&self) -> bool {
        self.finished_ns.load(Ordering::Acquire) != 0
    }

    /// How long the run has been going, or went.
    fn elapsed_ns(&self) -> u64 {
        match self.finished_ns.load(Ordering::Acquire) {
            0 => boot_time_ns().saturating_sub(self.started_ns),
            finished => finished.saturating_sub(self.started_ns),
        }
    }

    /// The source's task: a rate for the shell, and a line a second while
    /// anything is being sent.
    fn sample(self: Arc<Self>) {
        let mut last = totals(&SOURCE_COUNTS);

        while sleep_a_sample() {
            let now = totals(&SOURCE_COUNTS);
            self.rates.tx_pps.store(delta(now.tx_packets, last.tx_packets), Ordering::Relaxed);
            self.rates.rx_pps.store(delta(now.rx_packets, last.rx_packets), Ordering::Relaxed);
            self.rates.bps.store(delta(now.tx_bytes, last.tx_bytes), Ordering::Relaxed);

            /* The same line the target writes, and for the same reason: it
             * is what says the machine is alive when nothing else can. Not
             * once the senders are done -- there is no load then to go deaf
             * under. */
            if !self.is_done() || now.tx_packets != last.tx_packets {
                let path = kcore::net::rx_stats();
                trace!(0, "netload: sent {} (+{}), failed {}, echoes {}, pool misses {}, in flight {}",
                    now.tx_packets, self.rates.tx_pps.load(Ordering::Relaxed), now.tx_failed,
                    now.rx_packets, path.pool_misses, path.pool_in_flight);
            }

            last = now;
        }
    }
}

/// What comes back to the source's port: counted, and never answered -- two
/// sources pointed at each other would otherwise bounce one datagram for
/// good.
impl UdpHandler for Source {
    fn on_frame(&self, frame: Lent<'_>, _rx: &mut RxContext) {
        let bytes = frame.bytes();
        let counts = SOURCE_COUNTS.here();

        match udp::parse(bytes) {
            Some(datagram)
                if datagram.src_ip == self.plan.ip && datagram.src_port == self.plan.port =>
            {
                counts.rx_packets.add(1);
                counts.rx_bytes.add(bytes.len());
            }
            _ => counts.rx_strangers.add(1),
        }
    }
}

/// A source while it runs. Dropping one takes it down: senders first, so
/// that nothing is sent after the echoes stop being counted.
struct SourceRun {
    source: Arc<Source>,
    senders: Vec<TaskHandle>,
    listener: Option<UdpListener>,
    sampler: Option<TaskHandle>,
    /// Whether it came up whole: one that did not was never said to start,
    /// and is not said to stop.
    announced: bool,
}

impl Drop for SourceRun {
    fn drop(&mut self) {
        for sender in self.senders.iter() {
            sender.request_stop();
        }
        /* Each waits for its task. */
        self.senders.clear();

        drop(self.listener.take());

        if let Some(sampler) = self.sampler.take() {
            sampler.request_stop();
            drop(sampler);
        }
        if self.announced {
            trace!(0, "netload: source to {}:{} stopped", Ipv4(self.source.plan.ip),
                self.source.plan.port);
        }
    }
}

/* ------------------------------------------------------------------ */
/* The module                                                          */
/* ------------------------------------------------------------------ */

/// What is running. A run is never dropped under its lock: giving a port
/// back waits for the receive path, and giving a task back waits for the
/// task -- it is taken out under the lock and dropped after.
struct State {
    /// Held by the one command that is starting or stopping something.
    /// Building a run allocates and sleeps -- ARP takes up to three seconds
    /// -- and taking one down waits, none of which the locks below can be
    /// held over; so they guard the slots, and this says whose turn it is.
    /// Never waited for: a second command is told the first is at it.
    control: TryLock<()>,
    target: PreemptSpinLock<Option<TargetRun>>,
    source: PreemptSpinLock<Option<SourceRun>>,
}

const BUSY: &str = "another netload command is starting or stopping something";

impl State {
    fn new() -> Self {
        State {
            control: TryLock::new(()),
            target: PreemptSpinLock::new(None),
            source: PreemptSpinLock::new(None),
        }
    }

    fn start_target(&self, port: u16, echo: bool) -> Result<(), &'static str> {
        let _turn = self.control.try_lock().ok_or(BUSY)?;

        if self.target.lock().is_some() {
            return Err("already running");
        }
        let run = build_target(port, echo)?;

        /* The slot is empty and nobody else fills it while the turn is ours;
         * should that ever stop being so, what was there is dropped here,
         * after the lock, like any other run. */
        let previous = self.target.lock().replace(run);
        drop(previous);
        Ok(())
    }

    fn start_source(&self, plan: SendPlan) -> Result<(), &'static str> {
        let _turn = self.control.try_lock().ok_or(BUSY)?;

        /* One that has finished is only waiting to be asked about: a new one
         * takes its place. One still sending is not interrupted. */
        let sending = self.source.lock().as_ref().map_or(false, |run| !run.source.is_done());
        if sending {
            return Err("already sending -- netload stop first");
        }
        let finished = self.source.lock().take();
        drop(finished);

        let run = build_source(plan)?;
        let previous = self.source.lock().replace(run);
        drop(previous);
        Ok(())
    }

    /// Takes down whatever runs, from a command: `Ok(false)` when nothing did.
    fn stop(&self) -> Result<bool, &'static str> {
        let _turn = self.control.try_lock().ok_or(BUSY)?;
        Ok(self.take_down())
    }

    /// Takes down whatever runs. False when nothing did.
    fn take_down(&self) -> bool {
        let target = self.target.lock().take();
        let source = self.source.lock().take();
        let any = target.is_some() || source.is_some();

        drop(target);
        drop(source);
        any
    }
}

fn build_target(port: u16, echo: bool) -> Result<TargetRun, &'static str> {
    let nic = Nic::find(DEFAULT_NIC).ok_or("no eth0")?;

    clear(&TARGET_COUNTS);
    let target = Arc::new(Target {
        nic,
        mac: nic.mac(),
        port,
        echo,
        pending: RxOwned::new(TxBatch::new()),
        running: AtomicBool::new(false),
        rates: Rates::new(),
    });

    let mut run = TargetRun { target: target.clone(), listener: None, sampler: None };

    /* Listener slots are few, and DHCP, DNS and the shell have taken theirs
     * already: a full table is a real outcome and has to be reported, not
     * left as a server that is running and never dispatched to -- and so is
     * a port someone else has. */
    match nic.listen(port, target.clone()) {
        Ok(listener) => run.listener = Some(listener),
        Err(err) => {
            trace!(0, "netload: port {} could not be listened on ({:?})", port, err);
            return Err(match err {
                kcore::net::ListenError::PortTaken => "the port is taken",
                kcore::net::ListenError::TableFull => "no listener slot left on the device",
                kcore::net::ListenError::Invalid => "not a port",
            });
        }
    }

    run.sampler = Some(
        kcore::task::spawn_with("netload", target.clone(), Target::sample).ok_or("no task for it")?,
    );

    target.running.store(true, Ordering::Release);
    trace!(0, "netload: started on port {}, {}", port, if echo { "echo" } else { "sink" });
    Ok(run)
}

fn build_source(plan: SendPlan) -> Result<SourceRun, &'static str> {
    let nic = Nic::find(DEFAULT_NIC).ok_or("no eth0")?;
    let src_ip = nic.ip();
    if src_ip == 0 {
        return Err("eth0 has no address yet");
    }

    /* Asked once, before the first frame, and here rather than in a sender:
     * the answer can take three seconds, and none is a reason not to start. */
    let dst_mac = nic.resolve(plan.ip).ok_or("nothing answers ARP for that address")?;

    let route = udp::Route {
        src_mac: nic.mac(),
        dst_mac,
        src_ip,
        dst_ip: plan.ip,
        src_port: plan.source_port,
        dst_port: plan.port,
        dont_fragment: false,
    };
    let template = Source::template(&plan, &route).ok_or("not a size a datagram can have")?;

    clear(&SOURCE_COUNTS);
    let now = boot_time_ns();
    let source = Arc::new(Source {
        nic,
        plan,
        template,
        deadline_ns: if plan.secs == 0 { 0 } else { now.saturating_add(plan.secs * NS_PER_SEC) },
        started_ns: now,
        finished_ns: AtomicU64::new(0),
        active: AtomicUsize::new(plan.tasks),
        rates: Rates::new(),
    });

    /* The echoes' port first: a sender that started before it would have
     * its first replies counted as nobody's. */
    let listener = nic.listen(plan.source_port, source.clone()).map_err(|err| match err {
        kcore::net::ListenError::PortTaken => "the source port is taken -- sport=",
        kcore::net::ListenError::TableFull => "no listener slot left on the device",
        kcore::net::ListenError::Invalid => "not a source port",
    })?;

    let mut run = SourceRun {
        source: source.clone(),
        senders: Vec::new(),
        listener: Some(listener),
        sampler: None,
        announced: false,
    };
    run.sampler = Some(
        kcore::task::spawn_with("netload", source.clone(), Source::sample).ok_or("no task for it")?,
    );

    let tasks = plan.tasks as u64;
    for index in 0..plan.tasks {
        /* The whole, shared out: the first sender takes what does not divide. */
        let extra = if index == 0 { 1 } else { 0 };
        let share = Share {
            sender: index as u32,
            count: if plan.count == 0 { 0 } else { plan.count / tasks + extra * (plan.count % tasks) },
            pps: if plan.pps == 0 { 0 } else { (plan.pps / tasks + extra * (plan.pps % tasks)).max(1) },
        };
        let name = format!("netload/{}", index);
        match kcore::task::spawn_with(&name, (source.clone(), share), |(source, share)| {
            source.send(&share)
        }) {
            Some(sender) => run.senders.push(sender),
            /* Dropping the run stops the ones that did start. The ones that
             * did not are not going to count themselves out. */
            None => return Err("no task for a sender"),
        }
    }

    run.announced = true;
    trace!(0, "netload: sending to {}:{} from port {}, {} byte datagrams, {} task(s)",
        Ipv4(plan.ip), plan.port, plan.source_port, plan.size, plan.tasks);
    Ok(run)
}

/* ------------------------------------------------------------------ */
/* The command                                                         */
/* ------------------------------------------------------------------ */

fn command(state: &State, args: &str, out: &mut Output) {
    let mut words = args.split_whitespace();
    let verb = match words.next() {
        Some(verb) => verb,
        None => return report(state, out),
    };

    match verb {
        "start" => {
            /* start [port] [sink], and `sink` alone means the default port. */
            let mut port = DEFAULT_PORT;
            let mut echo = true;
            match words.next() {
                None => {}
                Some("sink") => echo = false,
                Some(text) => match text.parse::<u16>() {
                    Ok(parsed) if parsed != 0 => {
                        port = parsed;
                        match words.next() {
                            None => {}
                            Some("sink") => echo = false,
                            Some(_) => { let _ = writeln!(out, "{}", USAGE); return; }
                        }
                    }
                    _ => { let _ = writeln!(out, "{}", USAGE); return; }
                },
            }

            match state.start_target(port, echo) {
                Ok(()) => {
                    let _ = writeln!(out, "netload: listening on udp {}, {}", port,
                        if echo { "echo" } else { "sink" });
                }
                Err(problem) => {
                    let _ = writeln!(out, "netload: could not start on port {}: {}", port, problem);
                }
            }
        }
        "send" => {
            let plan = match parse_send(&mut words) {
                Ok(plan) => plan,
                Err(problem) => {
                    let _ = writeln!(out, "netload: {}", problem);
                    let _ = writeln!(out, "{}", USAGE);
                    return;
                }
            };
            match state.start_source(plan) {
                Ok(()) => {
                    let _ = writeln!(out, "netload: sending to {}:{} from port {}, {} byte datagrams, {} task(s){}",
                        Ipv4(plan.ip), plan.port, plan.source_port, plan.size, plan.tasks,
                        Limits(&plan));
                }
                Err(problem) => {
                    let _ = writeln!(out, "netload: could not send to {}:{}: {}",
                        Ipv4(plan.ip), plan.port, problem);
                }
            }
        }
        "stop" => {
            let _ = writeln!(out, "netload: {}", match state.stop() {
                Ok(true) => "stopped",
                Ok(false) => "not running",
                Err(problem) => problem,
            });
        }
        "reset" => {
            clear(&TARGET_COUNTS);
            clear(&SOURCE_COUNTS);
            if let Some(run) = state.target.lock().as_ref() {
                run.target.rates.clear();
            }
            if let Some(run) = state.source.lock().as_ref() {
                run.source.rates.clear();
            }
            let _ = writeln!(out, "netload: counters cleared");
        }
        _ => {
            let _ = writeln!(out, "{}", USAGE);
        }
    }
}

/// `<ip> <port>` and then `key=value` in any order.
fn parse_send<'a>(words: &mut impl Iterator<Item = &'a str>) -> Result<SendPlan, &'static str> {
    let ip = words.next().and_then(|text| netwire::parse_ipv4(text.as_bytes()))
        .ok_or("send where? an address, as a dotted quad")?;
    if ip == 0 || ip == u32::MAX {
        return Err("not an address to load");
    }
    let port = words.next().and_then(|text| text.parse::<u16>().ok()).filter(|&port| port != 0)
        .ok_or("and a port")?;

    let mut plan = SendPlan {
        ip, port,
        source_port: DEFAULT_SOURCE_PORT,
        size: DEFAULT_SIZE,
        pps: 0,
        secs: 0,
        count: 0,
        tasks: 1,
    };

    for word in words {
        let (key, value) = word.split_once('=').ok_or("options are key=value")?;
        let number = value.parse::<u64>().map_err(|_| "an option's value is a number")?;
        match key {
            "size" => {
                if number < MIN_SIZE as u64 || number > udp::MAX_PAYLOAD as u64 {
                    return Err("size= is 16 to 1472 bytes of payload");
                }
                plan.size = number as usize;
            }
            "pps" => plan.pps = number,
            "secs" => plan.secs = number,
            "count" => plan.count = number,
            "tasks" => {
                if number == 0 || number > MAX_TASKS as u64 {
                    return Err("tasks= is 1 to 16");
                }
                plan.tasks = number as usize;
            }
            "sport" => {
                if number == 0 || number > u16::MAX as u64 {
                    return Err("sport= is a port");
                }
                plan.source_port = number as u16;
            }
            _ => return Err("size=, pps=, secs=, count=, tasks= or sport="),
        }
    }

    /* A count or a rate smaller than the senders it is shared between would
     * leave some with a share of nothing, which to a sender means no limit. */
    if plan.count != 0 && plan.count < plan.tasks as u64 {
        return Err("count= is less than tasks=");
    }
    if plan.pps != 0 && plan.pps < plan.tasks as u64 {
        return Err("pps= is less than tasks=");
    }
    Ok(plan)
}

/// The limits a run was given, as the line that announces it says them.
struct Limits<'a>(&'a SendPlan);

impl core::fmt::Display for Limits<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let plan = self.0;
        if plan.pps != 0 {
            write!(f, ", {} a second", plan.pps)?;
        }
        if plan.count != 0 {
            write!(f, ", {} of them", plan.count)?;
        }
        if plan.secs != 0 {
            write!(f, ", for {} s", plan.secs)?;
        }
        Ok(())
    }
}

fn report(state: &State, out: &mut Output) {
    /* What a line needs is copied out under the lock and printed after it:
     * printing may block, and the lock is a spinlock. */
    let target = state.target.lock().as_ref().map(|run| run.target.clone());
    let source = state.source.lock().as_ref().map(|run| run.source.clone());

    if target.is_none() && source.is_none() {
        let _ = writeln!(out, "netload: not running");
        return;
    }

    if let Some(target) = target {
        let sum = totals(&TARGET_COUNTS);
        let _ = writeln!(out, "netload: port {}, {}", target.port,
            if target.echo { "echo" } else { "sink" });
        let _ = writeln!(out, "rx {} packets, {} bytes", sum.rx_packets, sum.rx_bytes);
        let _ = writeln!(out, "tx {} packets, {} failed", sum.tx_packets, sum.tx_failed);
        let _ = writeln!(out, "rate {} rx-pps, {} tx-pps, {} rx-bytes/s",
            target.rates.rx_pps.load(Ordering::Relaxed),
            target.rates.tx_pps.load(Ordering::Relaxed),
            target.rates.bps.load(Ordering::Relaxed));
        /* Which CPUs the driver's interrupts actually landed on: a load test
         * that runs entirely on one core is measuring one core. */
        per_cpu(out, "per cpu rx:", &TARGET_COUNTS, |counts| counts.rx_packets.get());
    }

    if let Some(source) = source {
        let sum = totals(&SOURCE_COUNTS);
        let elapsed_ms = source.elapsed_ns() / 1_000_000;
        let _ = writeln!(out, "netload: sending to {}:{} from port {}, {} byte datagrams, {} task(s){} -- {} {}.{:03} s",
            Ipv4(source.plan.ip), source.plan.port, source.plan.source_port, source.plan.size,
            source.plan.tasks, Limits(&source.plan),
            if source.is_done() { "done in" } else { "for" },
            elapsed_ms / 1000, elapsed_ms % 1000);
        let _ = writeln!(out, "sent {} packets, {} bytes, {} failed",
            sum.tx_packets, sum.tx_bytes, sum.tx_failed);
        let _ = writeln!(out, "echoes {} packets, {} bytes, {} from elsewhere",
            sum.rx_packets, sum.rx_bytes, sum.rx_strangers);
        let _ = writeln!(out, "rate {} tx-pps, {} rx-pps, {} tx-bytes/s",
            source.rates.tx_pps.load(Ordering::Relaxed),
            source.rates.rx_pps.load(Ordering::Relaxed),
            source.rates.bps.load(Ordering::Relaxed));
        per_cpu(out, "per cpu tx:", &SOURCE_COUNTS, |counts| counts.tx_packets.get());
    }
}

fn per_cpu(out: &mut Output, title: &str, counts: &PerCpu<Counts>, of: fn(&Counts) -> usize) {
    let _ = write!(out, "{}", title);
    for cpu in 0..MAX_CPUS {
        let count = counts.get(cpu).map_or(0, of);
        if count != 0 {
            let _ = write!(out, " {}:{}", cpu, count);
        }
    }
    let _ = writeln!(out);
}

struct NetLoad {
    /* Taken away first on the way out, so that no command is starting a run
     * while the runs are being taken down. */
    cmd: Option<Command>,
    state: Arc<State>,
}

impl kmod::Module for NetLoad {}

impl Drop for NetLoad {
    fn drop(&mut self) {
        /* Returns once no call of the command is running: from here on
         * nothing starts anything. */
        drop(self.cmd.take());
        self.state.take_down();
    }
}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let state = Arc::new(State::new());

    let shared = state.clone();
    let cmd = Command::register("netload", HELP, move |args, out| command(&shared, args, out))?;

    Ok(Box::new(NetLoad { cmd: Some(cmd), state }))
}

kmod::module!(name: "netload", init: init);
