//! TCP: the connections, the state machine and the timers behind them.
//!
//! One pool of 64 connections, a static rather than an allocation, with a
//! lock per connection and one over the pool. A segment arrives on the
//! receive path, is matched to a connection by its four addresses, and the
//! state machine runs under that connection's lock alone; the blocking calls
//! -- connect, accept, send, receive -- sleep in a task and look again.
//!
//! What is written down here rather than left to be rediscovered:
//!
//! - **Nothing traces or prints under a connection's lock.** A trace is
//!   synchronous output -- the serial port and the screen are written there
//!   and then -- and the task in `connect` is spinning on that lock. The
//!   "connected" line once held it for 25-33 ms on real hardware.
//! - **Sequence numbers are compared wrap-safely**, as signed 32-bit
//!   differences. Widening a `u32` subtraction instead gives a number that
//!   can never be negative, and the comparison silently stops working.
//! - **A pure ACK does not consume a sequence number**, so every place that
//!   sends one puts `snd_nxt` back afterwards.
//! - **Data never goes into a shut window.** The peer's duplicate ACKs would
//!   never advance anything, and the retransmit count would give up on a peer
//!   that is merely slow to read -- a suspended ssh client -- as a dead one,
//!   in under a minute. The persist timer probes instead.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcore::net::Nic;
use kcore::sync::PreemptSpinLock;
use kcore::trace;

use crate::abi;
use crate::wire::{self, eth, ip, tcp as seg, Mac, ETH_HDR_LEN, ETH_TYPE_IP,
                  IP_HDR_LEN, IP_PROTO_TCP};

pub const MAX_CONNECTIONS: usize = 64;
pub const SEND_BUF: usize = 8192;
pub const RECV_BUF: usize = 8192;

pub const DEFAULT_MSS: u16 = 536;
pub const OUR_MSS: u16 = 1460;

const INITIAL_RTO_MS: u64 = 1000;
const MAX_RTO_MS: u64 = 8000;
const MAX_RETRANSMITS: u32 = 8;
/// Linux's sixty seconds standing in for twice the maximum segment lifetime:
/// a segment of an old incarnation must not be matched by a new connection
/// reusing the same four addresses.
const TIME_WAIT_MS: u64 = 60_000;
const FIN_WAIT2_TIMEOUT_MS: u64 = 60_000;
const CONNECT_TIMEOUT_MS: u64 = 5000;
const TIMER_PERIOD_MS: u64 = 200;
const DEFAULT_TTL: u8 = 64;

const HASH_SIZE: usize = 32;
const EPHEMERAL_BASE: u16 = 49152;
const EPHEMERAL_MAX: u16 = 65535;

/// Connections a listener holds that nobody has accepted yet -- handshakes
/// under way, and ones done and waiting. Past this a SYN to its port is
/// dropped, as a full accept queue drops it: a flood of SYNs to a listening
/// port then costs the pool this many slots, not all of them.
const LISTEN_BACKLOG: usize = 16;

/// What `recv` answers below zero.
pub const RECV_ERROR: isize = -1;
pub const RECV_TIMEOUT: isize = -2;

const MAX_FRAME: usize = 1514;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum State {
    Free = 0,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    LastAck,
    Closing,
    TimeWait,
    Closed,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Free => "FREE",
            State::Listen => "LISTEN",
            State::SynSent => "SYN_SENT",
            State::SynReceived => "SYN_RCVD",
            State::Established => "ESTABLISHED",
            State::FinWait1 => "FIN_WAIT_1",
            State::FinWait2 => "FIN_WAIT_2",
            State::CloseWait => "CLOSE_WAIT",
            State::LastAck => "LAST_ACK",
            State::Closing => "CLOSING",
            State::TimeWait => "TIME_WAIT",
            State::Closed => "CLOSED",
        }
    }
}

/// What the state machine did that deserves a trace line. It runs under the
/// connection's lock, so it only reports; the line is written once the lock
/// is down.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Event {
    None,
    Rst,
    Connected,
    Accepted,
}

/* ---- sequence arithmetic ----
 *
 * Every comparison is a signed 32-bit difference, so it stays right across
 * the wrap. Widening the subtraction to a larger unsigned type gives a value
 * that can never be negative, and the comparison silently stops working. */

fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn before_eq(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

/* ---- the byte ring a connection buffers with ---- */

struct Ring {
    data: [u8; SEND_BUF],
    head: usize,
    tail: usize,
    size: usize,
}

impl Ring {
    const fn new(size: usize) -> Ring {
        Ring { data: [0; SEND_BUF], head: 0, tail: 0, size }
    }

    fn used(&self) -> usize {
        self.tail - self.head
    }

    fn free(&self) -> usize {
        self.size - self.used()
    }

    fn write(&mut self, src: &[u8]) -> usize {
        let len = src.len().min(self.free());
        for i in 0..len {
            let at = (self.tail + i) % self.size;
            self.data[at] = src[i];
        }
        self.tail += len;
        len
    }

    fn read(&mut self, dst: &mut [u8]) -> usize {
        let len = dst.len().min(self.used());
        for i in 0..len {
            let at = (self.head + i) % self.size;
            dst[i] = self.data[at];
        }
        self.head += len;
        len
    }

    /// Bytes without taking them.
    fn peek(&self, dst: &mut [u8], offset: usize) -> usize {
        let used = self.used();
        if offset >= used {
            return 0;
        }
        let len = dst.len().min(used - offset);
        for i in 0..len {
            let at = (self.head + offset + i) % self.size;
            dst[i] = self.data[at];
        }
        len
    }

    fn consume(&mut self, len: usize) {
        self.head += len.min(self.used());
    }

    fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
    }
}

/* ---- a connection ---- */

struct Inner {
    /* Who it is between */
    local_ip: u32,
    local_port: u16,
    remote_ip: u32,
    remote_port: u16,
    nic: Option<Nic>,
    peer_mac: Mac,

    state: State,

    /* Sequence tracking, as RFC 793 names it */
    snd_una: u32,
    snd_nxt: u32,
    snd_wnd: u32,
    /// The sequence number of the segment that last set the window
    snd_wl1: u32,
    /// Its acknowledgement number
    snd_wl2: u32,
    rcv_nxt: u32,
    rcv_wnd: u32,
    /// What our last outgoing segment advertised
    advertised_wnd: u32,
    iss: u32,
    irs: u32,
    peer_mss: u16,

    send_buf: Ring,
    recv_buf: Ring,

    rto_ms: u64,
    retransmit_at: u64,
    /// Retransmits with no acknowledgement in between
    retransmit_count: u32,
    /// Also the FIN-WAIT-2 deadline
    time_wait_at: u64,
    /// The zero-window probe's
    persist_at: u64,

    need_cleanup: bool,
    /// Our FIN has been acknowledged
    fin_acked: bool,
    /// An application still holds this: the cleanup timer must not recycle
    /// the slot until `close` gives it up
    owned_by_app: bool,
    /// A passive connection already handed out by `accept`
    accepted: bool,

    /// The next slot in this connection's hash bucket, or -1
    hash_next: i8,
    /// Whether it is in a bucket at all
    hashed: bool,
}

impl Inner {
    const fn new() -> Inner {
        Inner {
            local_ip: 0, local_port: 0, remote_ip: 0, remote_port: 0,
            nic: None, peer_mac: [0; 6],
            state: State::Free,
            snd_una: 0, snd_nxt: 0, snd_wnd: 0, snd_wl1: 0, snd_wl2: 0,
            rcv_nxt: 0, rcv_wnd: RECV_BUF as u32, advertised_wnd: RECV_BUF as u32,
            iss: 0, irs: 0, peer_mss: DEFAULT_MSS,
            send_buf: Ring::new(SEND_BUF),
            recv_buf: Ring::new(RECV_BUF),
            rto_ms: INITIAL_RTO_MS,
            retransmit_at: 0, retransmit_count: 0, time_wait_at: 0, persist_at: 0,
            need_cleanup: false, fin_acked: false, owned_by_app: false,
            accepted: false,
            hash_next: -1, hashed: false,
        }
    }

    /// Everything but the buffers' bytes and the hash link, which the pool
    /// keeps.
    fn init(&mut self) {
        let hash_next = self.hash_next;
        let hashed = self.hashed;
        self.local_ip = 0;
        self.local_port = 0;
        self.remote_ip = 0;
        self.remote_port = 0;
        self.nic = None;
        self.peer_mac = [0; 6];
        self.state = State::Free;
        self.snd_una = 0;
        self.snd_nxt = 0;
        self.snd_wnd = 0;
        self.snd_wl1 = 0;
        self.snd_wl2 = 0;
        self.rcv_nxt = 0;
        self.rcv_wnd = RECV_BUF as u32;
        self.advertised_wnd = RECV_BUF as u32;
        self.iss = 0;
        self.irs = 0;
        self.peer_mss = DEFAULT_MSS;
        self.send_buf.reset();
        self.recv_buf.reset();
        self.rto_ms = INITIAL_RTO_MS;
        self.retransmit_at = 0;
        self.retransmit_count = 0;
        self.time_wait_at = 0;
        self.persist_at = 0;
        self.need_cleanup = false;
        self.fin_acked = false;
        self.owned_by_app = false;
        self.accepted = false;
        self.hash_next = hash_next;
        self.hashed = hashed;
    }

    fn matches(&self, local_ip: u32, local_port: u16, remote_ip: u32, remote_port: u16)
        -> bool
    {
        self.state != State::Free
            && self.local_ip == local_ip && self.local_port == local_port
            && self.remote_ip == remote_ip && self.remote_port == remote_port
    }
}

pub struct Conn {
    lock: PreemptSpinLock,
    inner: core::cell::UnsafeCell<Inner>,
    /// Data has arrived; read without the lock by a waiting task
    data_ready: AtomicBool,
    /// The state left SYN-SENT or SYN-RECEIVED
    conn_ready: AtomicBool,
}

unsafe impl Sync for Conn {}
unsafe impl Send for Conn {}

impl Conn {
    const fn new() -> Conn {
        Conn {
            lock: PreemptSpinLock::new(),
            inner: core::cell::UnsafeCell::new(Inner::new()),
            data_ready: AtomicBool::new(false),
            conn_ready: AtomicBool::new(false),
        }
    }

    /// # Safety
    /// The caller holds this connection's lock.
    unsafe fn get(&self) -> &mut Inner {
        unsafe { &mut *self.inner.get() }
    }
}

/* ---- the one TCP ---- */

pub struct Tcp {
    /// Guards the pool: the hash buckets, allocation, and every walk of it.
    pool_lock: PreemptSpinLock,
    pool: [Conn; MAX_CONNECTIONS],
    /// Head slot of each bucket, or -1
    hash: core::cell::UnsafeCell<[i8; HASH_SIZE]>,
    next_ephemeral: core::cell::UnsafeCell<u16>,
    initialized: AtomicBool,

    tx_segments: AtomicUsize,
    rx_segments: AtomicUsize,
    rx_checksum_err: AtomicUsize,
    rx_too_short: AtomicUsize,
    retransmits: AtomicUsize,
    conn_count: AtomicUsize,
}

unsafe impl Sync for Tcp {}
unsafe impl Send for Tcp {}

/// The one of these. A static, pool and all -- 64 connections with two 8 KiB
/// buffers each is about a megabyte, which is what the C++ had in .bss too,
/// and what nothing would hand out from the heap.
pub static TCP: Tcp = Tcp {
    pool_lock: PreemptSpinLock::new(),
    pool: [const { Conn::new() }; MAX_CONNECTIONS],
    hash: core::cell::UnsafeCell::new([-1; HASH_SIZE]),
    next_ephemeral: core::cell::UnsafeCell::new(EPHEMERAL_BASE),
    initialized: AtomicBool::new(false),
    tx_segments: AtomicUsize::new(0),
    rx_segments: AtomicUsize::new(0),
    rx_checksum_err: AtomicUsize::new(0),
    rx_too_short: AtomicUsize::new(0),
    retransmits: AtomicUsize::new(0),
    conn_count: AtomicUsize::new(0),
};

fn now_ms() -> u64 {
    kcore::time::boot_time().as_nanos() / kcore::consts::NS_PER_MS
}

fn hash_index(local_ip: u32, local_port: u16, remote_ip: u32, remote_port: u16) -> usize {
    let mut h = local_ip ^ remote_ip ^ ((local_port as u32) << 16) ^ remote_port as u32;
    h ^= h >> 16;
    h ^= h >> 8;
    (h as usize) % HASH_SIZE
}

impl Tcp {
    pub fn init(&'static self) -> bool {
        if self.initialized.load(Ordering::Acquire) {
            return true;
        }

        kcore::softirq::register(kcore::softirq::TYPE_TCP_TIMER, on_softirq,
            core::ptr::null_mut());

        let period = kcore::time::Duration::from_nanos(
            TIMER_PERIOD_MS * kcore::consts::NS_PER_MS);
        match kcore::timer::Timer::start(period, on_tick, core::ptr::null_mut()) {
            Some(timer) => timer.leak(),
            None => {
                trace!(0, "tcp: the retransmit timer could not be started");
                return false;
            }
        }

        self.initialized.store(true, Ordering::Release);
        trace!(0, "tcp: initialized, up to {} connections", MAX_CONNECTIONS);
        true
    }

    fn slot(&'static self, index: usize) -> &'static Conn {
        &self.pool[index]
    }

    /// Which slot a connection pointer names, for the calls that arrive with
    /// one from an application.
    fn index_of(&'static self, conn: *const Conn) -> Option<usize> {
        for i in 0..MAX_CONNECTIONS {
            if core::ptr::eq(&self.pool[i], conn) {
                return Some(i);
            }
        }
        None
    }

    /* ---- the hash of four addresses ---- */

    /// The connection those four addresses name, with its lock held. The
    /// caller holds the pool lock.
    fn lookup_locked(&'static self, local_ip: u32, local_port: u16,
        remote_ip: u32, remote_port: u16) -> Option<&'static Conn>
    {
        let buckets = unsafe { &*self.hash.get() };
        let mut at = buckets[hash_index(local_ip, local_port, remote_ip, remote_port)];

        while at >= 0 {
            let conn = self.slot(at as usize);
            conn.lock.lock();
            let inner = unsafe { conn.get() };
            if inner.matches(local_ip, local_port, remote_ip, remote_port) {
                /* Handed to the caller still locked, as the C++ does: the
                 * pool lock is what stops it changing in between, and the
                 * caller lets go of that one first. */
                return Some(conn);
            }
            let next = inner.hash_next;
            unsafe { conn.lock.unlock() };
            at = next;
        }
        None
    }

    /// The listener for that address and port, with its lock held. A listener
    /// bound to one address must not take a segment aimed at another, while a
    /// wildcard one takes any.
    fn find_listener_locked(&'static self, local_ip: u32, local_port: u16)
        -> Option<&'static Conn>
    {
        for i in 0..MAX_CONNECTIONS {
            let conn = self.slot(i);
            conn.lock.lock();
            let inner = unsafe { conn.get() };
            if inner.state == State::Listen && inner.local_port == local_port
                && (inner.local_ip == local_ip || inner.local_ip == 0)
            {
                return Some(conn);
            }
            unsafe { conn.lock.unlock() };
        }
        None
    }

    /// The caller holds the pool lock and the connection's.
    fn insert_hash(&'static self, index: usize) {
        let conn = self.slot(index);
        let inner = unsafe { conn.get() };
        let bucket = hash_index(inner.local_ip, inner.local_port,
            inner.remote_ip, inner.remote_port);

        let buckets = unsafe { &mut *self.hash.get() };
        inner.hash_next = buckets[bucket];
        inner.hashed = true;
        buckets[bucket] = index as i8;
    }

    /// The caller holds the pool lock and the connection's.
    fn remove_hash(&'static self, index: usize) {
        let conn = self.slot(index);
        let inner = unsafe { conn.get() };
        if !inner.hashed {
            return;
        }
        let bucket = hash_index(inner.local_ip, inner.local_port,
            inner.remote_ip, inner.remote_port);

        let buckets = unsafe { &mut *self.hash.get() };
        if buckets[bucket] == index as i8 {
            buckets[bucket] = inner.hash_next;
        } else {
            let mut at = buckets[bucket];
            while at >= 0 {
                let prev = self.slot(at as usize);
                /* Every other connection in the bucket is untouched by this
                 * caller, so its link is read without its lock -- the pool
                 * lock is what keeps the chain still. */
                let prev_inner = unsafe { &mut *prev.inner.get() };
                if prev_inner.hash_next == index as i8 {
                    prev_inner.hash_next = inner.hash_next;
                    break;
                }
                at = prev_inner.hash_next;
            }
        }

        inner.hash_next = -1;
        inner.hashed = false;
    }

    /// A free slot, made ready. The caller holds the pool lock, and that is
    /// the only lock taken here -- deliberately.
    ///
    /// A free slot has no other user by definition, so its own lock guards
    /// nothing; taking it anyway would deadlock the one caller that matters.
    /// The SYN path holds the listener's lock while it allocates, and the
    /// listener is a slot in this very pool: a walk that locked each slot
    /// would spin on the one it already holds, with preemption off, for
    /// good.
    fn alloc_conn(&'static self) -> Option<usize> {
        for i in 0..MAX_CONNECTIONS {
            let conn = self.slot(i);
            let inner = unsafe { &mut *conn.inner.get() };
            if inner.state == State::Free {
                inner.init();
                conn.data_ready.store(false, Ordering::Release);
                conn.conn_ready.store(false, Ordering::Release);
                return Some(i);
            }
        }
        None
    }

    /// A local port nothing is using. The caller holds the pool lock.
    fn alloc_ephemeral_port(&'static self) -> u16 {
        let next = unsafe { &mut *self.next_ephemeral.get() };
        let start = *next;

        loop {
            let port = *next;
            *next = if *next >= EPHEMERAL_MAX { EPHEMERAL_BASE } else { *next + 1 };

            let mut taken = false;
            for i in 0..MAX_CONNECTIONS {
                let conn = self.slot(i);
                let inner = unsafe { &*conn.inner.get() };
                if inner.state != State::Free && inner.local_port == port {
                    taken = true;
                    break;
                }
            }
            if !taken {
                return port;
            }

            /* All the way round without one */
            if *next == start {
                return 0;
            }
        }
    }
}

/* ---- segments out ---- */

impl Tcp {
    /// One segment of this connection. The caller holds its lock.
    ///
    /// A SYN carries the maximum segment size option, which makes the header
    /// 24 bytes instead of 20.
    fn send_segment(&'static self, inner: &mut Inner, flags: u8, data: &[u8]) {
        let nic = match inner.nic {
            Some(nic) => nic,
            None => return,
        };

        let with_mss = flags & seg::SYN != 0;
        let hdr_len = if with_mss { 24 } else { seg::HDR_LEN };
        let tcp_len = hdr_len + data.len();
        let frame_len = ETH_HDR_LEN + IP_HDR_LEN + tcp_len;
        if frame_len > MAX_FRAME {
            return;
        }

        let mut frame = [0u8; MAX_FRAME];
        eth::write(&mut frame, &inner.peer_mac, &nic.mac(), ETH_TYPE_IP);
        ip::write(&mut frame[ETH_HDR_LEN..], IP_PROTO_TCP,
            inner.local_ip, inner.remote_ip, tcp_len, 0);
        frame[ETH_HDR_LEN + ip::TTL] = DEFAULT_TTL;
        recompute_ip_checksum(&mut frame[ETH_HDR_LEN..]);

        let at = ETH_HDR_LEN + IP_HDR_LEN;
        seg::write(&mut frame[at..], inner.local_port, inner.remote_port,
            inner.snd_nxt, inner.rcv_nxt, hdr_len, flags, inner.rcv_wnd as u16);

        if with_mss {
            let opt = at + seg::HDR_LEN;
            frame[opt] = seg::OPT_MSS;
            frame[opt + 1] = seg::OPT_MSS_LEN;
            frame[opt + 2..opt + 4].copy_from_slice(&OUR_MSS.to_be_bytes());
        }
        if !data.is_empty() {
            frame[at + hdr_len..at + tcp_len].copy_from_slice(data);
        }

        let sum = seg::checksum(inner.local_ip, inner.remote_ip,
            &frame[at..at + tcp_len]);
        wire::set_be16(&mut frame[at..], seg::CHECKSUM, sum);

        nic.send_raw(&frame[..frame_len]);
        inner.advertised_wnd = inner.rcv_wnd;
        self.tx_segments.fetch_add(1, Ordering::Relaxed);
    }

    /// An acknowledgement of what has arrived, which consumes no sequence
    /// number of its own -- so `snd_nxt` goes back where it was.
    fn send_ack(&'static self, inner: &mut Inner) {
        let saved = inner.snd_nxt;
        self.send_segment(inner, seg::ACK_FLAG, &[]);
        inner.snd_nxt = saved;
    }

    /// A reset to a peer this machine has no connection for.
    #[allow(clippy::too_many_arguments)]
    fn send_rst(&'static self, nic: &Nic, dst_mac: &Mac, src_ip: u32, dst_ip: u32,
        src_port: u16, dst_port: u16, seq: u32, ack: u32)
    {
        let tcp_len = seg::HDR_LEN;
        let frame_len = ETH_HDR_LEN + IP_HDR_LEN + tcp_len;
        let mut frame = [0u8; MAX_FRAME];

        eth::write(&mut frame, dst_mac, &nic.mac(), ETH_TYPE_IP);
        ip::write(&mut frame[ETH_HDR_LEN..], IP_PROTO_TCP, src_ip, dst_ip, tcp_len, 0);
        frame[ETH_HDR_LEN + ip::TTL] = DEFAULT_TTL;
        recompute_ip_checksum(&mut frame[ETH_HDR_LEN..]);

        let at = ETH_HDR_LEN + IP_HDR_LEN;
        seg::write(&mut frame[at..], src_port, dst_port, seq, ack,
            seg::HDR_LEN, seg::RST | seg::ACK_FLAG, 0);

        let sum = seg::checksum(src_ip, dst_ip, &frame[at..at + tcp_len]);
        wire::set_be16(&mut frame[at..], seg::CHECKSUM, sum);

        nic.send_raw(&frame[..frame_len]);
        self.tx_segments.fetch_add(1, Ordering::Relaxed);
    }

    /* ---- the pieces every state shares ---- */

    /// An acknowledgement: the window it advertises, and the bytes it frees.
    fn process_ack(&'static self, inner: &mut Inner, segment_seq: u32, ack: u32,
        wnd: u16, now: u64)
    {
        /* RFC 793: take the advertisement only from a segment newer than the
         * one that last set the window, so a reordered stale advertisement
         * cannot undo it. */
        if before(inner.snd_wl1, segment_seq)
            || (inner.snd_wl1 == segment_seq && before_eq(inner.snd_wl2, ack))
        {
            inner.snd_wnd = wnd as u32;
            inner.snd_wl1 = segment_seq;
            inner.snd_wl2 = ack;
        }

        /* snd_una < ack <= snd_nxt, wrap-safe */
        let advance = ack.wrapping_sub(inner.snd_una) as i32;
        if advance <= 0 || before(inner.snd_nxt, ack) {
            return;
        }

        /* The send buffer holds data bytes only; a FIN waiting to go out
         * takes a sequence number that is not in it, so the drain is capped
         * at what the buffer has. */
        let acked = (advance as usize).min(inner.send_buf.used());
        inner.send_buf.consume(acked);

        inner.snd_una = ack;
        inner.rto_ms = INITIAL_RTO_MS;
        inner.retransmit_count = 0;
        inner.retransmit_at = if inner.snd_una == inner.snd_nxt { 0 } else { now + inner.rto_ms };
    }

    /// Payload in order into the receive buffer, and the acknowledgement it
    /// requires -- a duplicate one when the segment was out of order. Shared
    /// by ESTABLISHED and the FIN-WAIT states, because the peer may keep
    /// sending after our FIN (RFC 793 half-close).
    fn process_payload(&'static self, conn: &Conn, inner: &mut Inner, seq: u32,
        payload: &[u8])
    {
        if seq == inner.rcv_nxt {
            let written = inner.recv_buf.write(payload);
            inner.rcv_nxt = inner.rcv_nxt.wrapping_add(written as u32);
            inner.rcv_wnd = inner.recv_buf.free() as u32;
            conn.data_ready.store(true, Ordering::Release);
        }

        self.send_ack(inner);
    }
}

/// The IP checksum again, over a header whose time to live has been changed
/// since `ip::write` left one.
fn recompute_ip_checksum(packet: &mut [u8]) {
    wire::set_be16(packet, ip::CHECKSUM, 0);
    let sum = wire::checksum(&packet[..IP_HDR_LEN]);
    wire::set_be16(packet, ip::CHECKSUM, sum);
}

/* ---- the state machine ---- */

impl Tcp {
    /// One segment against one connection, whose lock the caller holds.
    /// Answers with what deserves a trace line, which the caller writes once
    /// the lock is down.
    fn handle(&'static self, conn: &Conn, inner: &mut Inner, segment: &[u8],
        payload: &[u8]) -> Event
    {
        let seq = seg::seq(segment);
        let ack = seg::ack(segment);
        let flags = seg::flags(segment);
        let wnd = seg::window(segment);
        let now = now_ms();
        let len = payload.len() as u32;

        /* A reset is valid in every state but LISTEN, and only in window --
         * without that check any host that can guess the four addresses
         * could tear the connection down blind. */
        if flags & seg::RST != 0 && inner.state != State::Listen {
            let acceptable = if inner.state == State::SynSent {
                /* Here it is only valid if it acknowledges our SYN */
                flags & seg::ACK_FLAG != 0 && ack == inner.snd_nxt
            } else {
                seq == inner.rcv_nxt || seq.wrapping_sub(inner.rcv_nxt) < inner.rcv_wnd
            };
            if !acceptable {
                return Event::None;
            }

            inner.state = State::Closed;
            conn.conn_ready.store(true, Ordering::Release);
            conn.data_ready.store(true, Ordering::Release);
            return Event::Rst;
        }

        match inner.state {
            State::SynSent => {
                if flags & (seg::SYN | seg::ACK_FLAG) == (seg::SYN | seg::ACK_FLAG) {
                    if ack != inner.snd_nxt {
                        let nic = inner.nic;
                        if let Some(nic) = nic {
                            self.send_rst(&nic, &inner.peer_mac, inner.local_ip,
                                inner.remote_ip, inner.local_port, inner.remote_port,
                                ack, 0);
                        }
                        return Event::None;
                    }

                    inner.irs = seq;
                    inner.rcv_nxt = seq.wrapping_add(1);
                    inner.snd_una = ack;
                    inner.snd_wnd = wnd as u32;
                    inner.snd_wl1 = seq;
                    inner.snd_wl2 = ack;
                    inner.peer_mss = seg::parse_mss(segment, OUR_MSS, DEFAULT_MSS);
                    inner.state = State::Established;
                    inner.rto_ms = INITIAL_RTO_MS;
                    inner.retransmit_at = 0;

                    self.send_ack(inner);
                    conn.conn_ready.store(true, Ordering::Release);
                    return Event::Connected;
                }
            }

            State::SynReceived => {
                if flags & seg::ACK_FLAG != 0 && ack == inner.snd_nxt {
                    inner.snd_una = ack;
                    inner.snd_wnd = wnd as u32;
                    inner.snd_wl1 = seq;
                    inner.snd_wl2 = ack;
                    inner.state = State::Established;
                    inner.rto_ms = INITIAL_RTO_MS;
                    inner.retransmit_at = 0;
                    conn.conn_ready.store(true, Ordering::Release);
                    return Event::Accepted;
                }
            }

            State::Established => {
                /* RFC 793: a SYN in window here is an error -- reset. A
                 * retransmitted handshake SYN sits below the window and is
                 * ignored. */
                if flags & seg::SYN != 0
                    && (seq == inner.rcv_nxt
                        || seq.wrapping_sub(inner.rcv_nxt) < inner.rcv_wnd)
                {
                    let saved = inner.snd_nxt;
                    if let Some(nic) = inner.nic {
                        self.send_rst(&nic, &inner.peer_mac, inner.local_ip,
                            inner.remote_ip, inner.local_port, inner.remote_port,
                            saved, 0);
                    }
                    inner.state = State::Closed;
                    conn.conn_ready.store(true, Ordering::Release);
                    conn.data_ready.store(true, Ordering::Release);
                    return Event::None;
                }

                if flags & seg::ACK_FLAG != 0 {
                    self.process_ack(inner, seq, ack, wnd, now);
                }
                if !payload.is_empty() {
                    self.process_payload(conn, inner, seq, payload);
                }

                /* A FIN in order: the peer is done sending */
                if flags & seg::FIN != 0 && seq.wrapping_add(len) == inner.rcv_nxt {
                    inner.rcv_nxt = seq.wrapping_add(len).wrapping_add(1);
                    inner.state = State::CloseWait;
                    /* Wake a reader so it sees the end of the stream */
                    conn.data_ready.store(true, Ordering::Release);
                    self.send_ack(inner);
                }
            }

            State::FinWait1 => {
                /* Drain what the peer acknowledged alongside (or before) our
                 * FIN, so the retransmit timer stops resending stale bytes
                 * and our FIN can be acknowledged once everything is. */
                if flags & seg::ACK_FLAG != 0 {
                    self.process_ack(inner, seq, ack, wnd, now);
                    if inner.snd_una == inner.snd_nxt {
                        inner.fin_acked = true;
                    }
                }
                if !payload.is_empty() {
                    self.process_payload(conn, inner, seq, payload);
                }

                if flags & seg::FIN != 0 && seq.wrapping_add(len) == inner.rcv_nxt {
                    inner.rcv_nxt = seq.wrapping_add(len).wrapping_add(1);
                    self.send_ack(inner);

                    if inner.fin_acked {
                        inner.state = State::TimeWait;
                        inner.time_wait_at = now + TIME_WAIT_MS;
                    } else {
                        /* Both closed at once: they sent a FIN without
                         * acknowledging ours */
                        inner.state = State::Closing;
                    }
                } else if inner.fin_acked {
                    /* Ours is acknowledged, theirs has not come. RFC 1122
                     * 4.2.2.20 wants a timer here: a peer that never sends
                     * its FIN must not pin the slot for good. */
                    inner.state = State::FinWait2;
                    inner.retransmit_at = 0;
                    inner.time_wait_at = now + FIN_WAIT2_TIMEOUT_MS;
                }
            }

            State::FinWait2 => {
                if !payload.is_empty() {
                    self.process_payload(conn, inner, seq, payload);
                }
                if flags & seg::FIN != 0 && seq.wrapping_add(len) == inner.rcv_nxt {
                    inner.rcv_nxt = seq.wrapping_add(len).wrapping_add(1);
                    self.send_ack(inner);
                    inner.state = State::TimeWait;
                    inner.time_wait_at = now + TIME_WAIT_MS;
                }
            }

            State::Closing => {
                if flags & seg::ACK_FLAG != 0 {
                    self.process_ack(inner, seq, ack, wnd, now);
                    if inner.snd_una == inner.snd_nxt {
                        inner.state = State::TimeWait;
                        inner.time_wait_at = now + TIME_WAIT_MS;
                    }
                }
                /* Their FIN again, because our acknowledgement was lost */
                if flags & seg::FIN != 0 {
                    self.send_ack(inner);
                }
            }

            State::LastAck => {
                if flags & seg::ACK_FLAG != 0 {
                    self.process_ack(inner, seq, ack, wnd, now);
                    if inner.snd_una == inner.snd_nxt {
                        inner.state = State::Closed;
                        conn.conn_ready.store(true, Ordering::Release);
                    }
                }
            }

            State::CloseWait => {
                if flags & seg::ACK_FLAG != 0 {
                    self.process_ack(inner, seq, ack, wnd, now);
                }
                if flags & seg::FIN != 0 {
                    self.send_ack(inner);
                }
            }

            State::TimeWait => {
                if flags & seg::FIN != 0 {
                    self.send_ack(inner);
                    inner.time_wait_at = now + TIME_WAIT_MS;
                }
            }

            _ => {}
        }

        Event::None
    }

    /// The line for what the state machine reported, written with no lock
    /// held -- never under one. A trace is synchronous output: the serial
    /// port and the screen are written there and then, while the task in
    /// `connect` spins on the connection's lock, which goes up just before
    /// the line. Written under the lock, the "connected" line once held it
    /// for 25-33 ms on real hardware, forced disk log writes included.
    fn trace_event(&self, event: Event, local_port: u16, remote_port: u16, peer_mss: u16) {
        match event {
            Event::Rst => trace!(0, "tcp: reset received, {} -> {}",
                local_port, remote_port),
            Event::Connected => trace!(0, "tcp: connected {} -> {}, mss {}",
                local_port, remote_port, peer_mss),
            Event::Accepted => trace!(0, "tcp: accepted {} <- {}",
                local_port, remote_port),
            Event::None => {}
        }
    }
}

/* ---- segments in ---- */

impl Tcp {
    /// A frame the receive path says carries TCP.
    pub fn process(&'static self, nic: &Nic, frame: &[u8]) {
        if frame.len() < ETH_HDR_LEN + IP_HDR_LEN + seg::HDR_LEN {
            self.rx_too_short.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let packet = &frame[ETH_HDR_LEN..];
        let ip_len = ip::header_len(packet);
        if ip_len == 0 {
            self.rx_too_short.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let total = ip::total_len(packet) as usize;
        if ETH_HDR_LEN + total > frame.len() {
            self.rx_too_short.fetch_add(1, Ordering::Relaxed);
            return;
        }
        /* The header length comes off the wire, so guard the subtraction
         * below: without this the segment length could underflow and the
         * checksum would read far past the frame. */
        if total < ip_len + seg::HDR_LEN {
            self.rx_too_short.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let (src_ip, dst_ip) = (ip::src(packet), ip::dst(packet));
        let segment = &frame[ETH_HDR_LEN + ip_len..ETH_HDR_LEN + total];

        if seg::checksum(src_ip, dst_ip, segment) != 0 {
            self.rx_checksum_err.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.rx_segments.fetch_add(1, Ordering::Relaxed);

        let hdr_len = seg::header_len(segment);
        if hdr_len < seg::HDR_LEN || hdr_len > segment.len() {
            self.rx_too_short.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let payload = &segment[hdr_len..];

        let local_ip = dst_ip;
        let local_port = seg::dst_port(segment);
        let remote_ip = src_ip;
        let remote_port = seg::src_port(segment);
        let flags = seg::flags(segment);
        let peer_mac = eth::src(frame);

        self.pool_lock.lock();

        /* The four addresses, exactly */
        if let Some(conn) = self.lookup_locked(local_ip, local_port, remote_ip, remote_port) {
            unsafe { self.pool_lock.unlock() };

            let inner = unsafe { conn.get() };
            let event = self.handle(conn, inner, segment, payload);
            let peer_mss = inner.peer_mss;
            unsafe { conn.lock.unlock() };

            self.trace_event(event, local_port, remote_port, peer_mss);
            return;
        }

        /* A SYN for something listening */
        if flags & seg::SYN != 0 {
            if let Some(listener) = self.find_listener_locked(local_ip, local_port) {
                if self.backlog_full_locked(local_port) {
                    /* Dropped, as a full accept queue drops it: the peer
                     * tries again and the pool keeps its slots. */
                    unsafe { listener.lock.unlock() };
                    unsafe { self.pool_lock.unlock() };
                    return;
                }

                let index = match self.alloc_conn() {
                    Some(index) => index,
                    None => {
                        unsafe { listener.lock.unlock() };
                        unsafe { self.pool_lock.unlock() };
                        return;
                    }
                };

                let conn = self.slot(index);
                conn.lock.lock();
                {
                    let inner = unsafe { conn.get() };
                    inner.nic = Some(*nic);
                    inner.local_ip = local_ip;
                    inner.local_port = local_port;
                    inner.remote_ip = remote_ip;
                    inner.remote_port = remote_port;
                    inner.state = State::SynReceived;
                    inner.irs = seg::seq(segment);
                    inner.rcv_nxt = inner.irs.wrapping_add(1);
                    inner.iss = initial_sequence();
                    inner.snd_nxt = inner.iss;
                    inner.snd_una = inner.iss;
                    inner.snd_wnd = seg::window(segment) as u32;
                    inner.snd_wl1 = inner.irs;
                    inner.snd_wl2 = 0;
                    inner.peer_mss = seg::parse_mss(segment, OUR_MSS, DEFAULT_MSS);
                    /* Where the frame came from: no resolution needed */
                    inner.peer_mac = peer_mac;
                }
                self.insert_hash(index);

                unsafe { listener.lock.unlock() };
                unsafe { self.pool_lock.unlock() };

                {
                    let inner = unsafe { conn.get() };
                    self.send_segment(inner, seg::SYN | seg::ACK_FLAG, &[]);
                    /* A SYN takes a sequence number */
                    inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                    inner.retransmit_at = now_ms() + inner.rto_ms;
                }
                self.conn_count.fetch_add(1, Ordering::Relaxed);
                unsafe { conn.lock.unlock() };
                return;
            }
        }

        unsafe { self.pool_lock.unlock() };

        /* Nothing here for it: reset, unless it is one itself */
        if flags & seg::RST == 0 {
            let mut rst_seq = 0;
            let mut rst_ack = seg::seq(segment).wrapping_add(payload.len() as u32);
            if flags & seg::SYN != 0 {
                rst_ack = rst_ack.wrapping_add(1);
            }
            if flags & seg::FIN != 0 {
                /* A FIN takes a sequence number too */
                rst_ack = rst_ack.wrapping_add(1);
            }
            if flags & seg::ACK_FLAG != 0 {
                rst_seq = seg::ack(segment);
            }

            self.send_rst(nic, &peer_mac, local_ip, remote_ip,
                local_port, remote_port, rst_seq, rst_ack);
        }
    }

    /// Whether the port's listener holds all the connections it may that
    /// nobody has accepted. The caller holds the pool lock.
    fn backlog_full_locked(&'static self, port: u16) -> bool {
        let mut pending = 0;
        for i in 0..MAX_CONNECTIONS {
            /* A racy read of each state, as accept's first look is: enough
             * for a limit. */
            let inner = unsafe { &*self.pool[i].inner.get() };
            if inner.local_port == port && !inner.accepted && !inner.owned_by_app
                && matches!(inner.state,
                    State::SynReceived | State::Established | State::CloseWait)
            {
                pending += 1;
            }
        }
        pending >= LISTEN_BACKLOG
    }

    /// A destination unreachable quoting a segment this machine sent.
    pub fn on_icmp_unreachable(&'static self, local_ip: u32, local_port: u16,
        remote_ip: u32, remote_port: u16, quoted_seq: u32)
    {
        if !self.initialized.load(Ordering::Acquire) {
            return;
        }

        self.pool_lock.lock();
        let conn = self.lookup_locked(local_ip, local_port, remote_ip, remote_port);
        unsafe { self.pool_lock.unlock() };

        let conn = match conn {
            Some(conn) => conn,
            None => return,
        };
        let inner = unsafe { conn.get() };

        /* RFC 5927: honour the hard error only when the quoted sequence
         * number is one this machine sent and has not seen acknowledged --
         * which is what an off-path forgery cannot know. */
        if before(quoted_seq, inner.snd_una) || before(inner.snd_nxt, quoted_seq) {
            unsafe { conn.lock.unlock() };
            return;
        }

        inner.state = State::Closed;
        inner.retransmit_at = 0;
        inner.need_cleanup = true;
        conn.conn_ready.store(true, Ordering::Release);
        conn.data_ready.store(true, Ordering::Release);
        unsafe { conn.lock.unlock() };

        trace!(0, "tcp: icmp unreachable, {} -> {} aborted", local_port, remote_port);
    }
}

/// Where a connection's sequence numbers start. From the entropy pool: an
/// initial sequence number a stranger can guess is one they can inject into.
fn initial_sequence() -> u32 {
    kcore::random::random_u64().unwrap_or(0x1234_5678) as u32
}

/* ---- what an application calls ---- */

impl Tcp {
    /// An active open. Blocks until the connection is up, or the timeout
    /// passes. `src_port` of 0 takes an ephemeral one.
    pub fn connect(&'static self, nic: &Nic, dst_ip: u32, dst_port: u16, src_port: u16)
        -> Option<&'static Conn>
    {
        if !self.initialized.load(Ordering::Acquire) {
            return None;
        }

        /* Resolved before any lock is taken: it sleeps. Off-subnet goes to
         * the gateway. */
        let arp = abi::arp_table()?;
        let peer_mac = match arp.resolve(nic, nic.route_ip(dst_ip)) {
            Some(mac) => mac,
            None => {
                trace!(0, "tcp: nothing answered for the address to connect to");
                return None;
            }
        };

        self.pool_lock.lock();

        let src_port = if src_port == 0 {
            let port = self.alloc_ephemeral_port();
            if port == 0 {
                unsafe { self.pool_lock.unlock() };
                trace!(0, "tcp: no ephemeral port is free");
                return None;
            }
            port
        } else {
            /* Two connections with the same four addresses would take each
             * other's segments. */
            if let Some(dup) = self.lookup_locked(nic.ip(), src_port, dst_ip, dst_port) {
                unsafe { dup.lock.unlock() };
                unsafe { self.pool_lock.unlock() };
                trace!(0, "tcp: port {} is already connected to that address", src_port);
                return None;
            }
            src_port
        };

        let index = match self.alloc_conn() {
            Some(index) => index,
            None => {
                unsafe { self.pool_lock.unlock() };
                trace!(0, "tcp: no free connection");
                return None;
            }
        };

        let conn = self.slot(index);
        conn.lock.lock();
        {
            let inner = unsafe { conn.get() };
            inner.nic = Some(*nic);
            inner.local_ip = nic.ip();
            inner.local_port = src_port;
            inner.remote_ip = dst_ip;
            inner.remote_port = dst_port;
            inner.peer_mac = peer_mac;
            inner.iss = initial_sequence();
            inner.snd_nxt = inner.iss;
            inner.snd_una = inner.iss;
            inner.state = State::SynSent;
            /* The caller holds this until it closes it */
            inner.owned_by_app = true;
        }
        self.insert_hash(index);
        unsafe { self.pool_lock.unlock() };

        {
            let inner = unsafe { conn.get() };
            self.send_segment(inner, seg::SYN, &[]);
            inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
            inner.retransmit_at = now_ms() + inner.rto_ms;
        }
        unsafe { conn.lock.unlock() };
        self.conn_count.fetch_add(1, Ordering::Relaxed);

        let deadline = now_ms() + CONNECT_TIMEOUT_MS;
        while now_ms() < deadline {
            if conn.conn_ready.load(Ordering::Acquire) {
                conn.lock.lock();
                let state = unsafe { conn.get() }.state;
                unsafe { conn.lock.unlock() };

                if state == State::Established {
                    return Some(conn);
                }
                /* Refused, or reset */
                self.close(conn);
                return None;
            }
            kcore::task::sleep_ms(1);
        }

        trace!(0, "tcp: nothing answered {} -> {}", src_port, dst_port);
        self.close(conn);
        None
    }

    /// A passive open, at every address this machine has -- a connection
    /// belongs to the device it arrives on.
    pub fn listen(&'static self, nic: &Nic, port: u16) -> Option<&'static Conn> {
        if !self.initialized.load(Ordering::Acquire) {
            return None;
        }

        self.pool_lock.lock();

        /* A second listener on the same port would never be given anything */
        for i in 0..MAX_CONNECTIONS {
            let inner = unsafe { &*self.pool[i].inner.get() };
            if inner.state == State::Listen && inner.local_port == port {
                unsafe { self.pool_lock.unlock() };
                trace!(0, "tcp: port {} is listened on already", port);
                return None;
            }
        }

        let index = match self.alloc_conn() {
            Some(index) => index,
            None => {
                unsafe { self.pool_lock.unlock() };
                return None;
            }
        };

        let conn = self.slot(index);
        conn.lock.lock();
        {
            let inner = unsafe { conn.get() };
            inner.nic = Some(*nic);
            /* The wildcard, which a segment for any address matches: bound
             * to the device's address of the moment, the port would answer
             * nobody once a lease renewal moved it. */
            inner.local_ip = 0;
            inner.local_port = port;
            inner.state = State::Listen;
            inner.owned_by_app = true;
        }
        unsafe { conn.lock.unlock() };
        /* A listener is not in the hash: the SYN path scans for it */
        unsafe { self.pool_lock.unlock() };
        Some(conn)
    }

    /// The next connection on a listener's port. None once the timeout
    /// passes with none, or once the listener is closed -- which is how a
    /// server's task waiting here sees a close from another.
    pub fn accept(&'static self, listener: &'static Conn, timeout_ms: u64)
        -> Option<&'static Conn>
    {
        /* The port, taken once: a close from another task frees the slot,
         * and a later listen may take it for another port. */
        listener.lock.lock();
        let inner = unsafe { listener.get() };
        let listening = inner.state == State::Listen;
        let port = inner.local_port;
        unsafe { listener.lock.unlock() };
        if !listening {
            return None;
        }

        let deadline = if timeout_ms != 0 { now_ms() + timeout_ms } else { 0 };

        loop {
            self.pool_lock.lock();

            listener.lock.lock();
            let inner = unsafe { listener.get() };
            let still = inner.state == State::Listen && inner.local_port == port;
            unsafe { listener.lock.unlock() };
            if !still {
                unsafe { self.pool_lock.unlock() };
                return None;
            }

            for i in 0..MAX_CONNECTIONS {
                let conn = self.slot(i);
                if core::ptr::eq(conn, listener) {
                    continue;
                }

                conn.lock.lock();
                let inner = unsafe { conn.get() };
                /* One whose peer has closed already goes too: its reader
                 * sees the end of the stream and closes it, and nothing else
                 * would -- a CLOSE-WAIT nobody owns keeps its slot for good.
                 * Never one of this machine's own connections that happens
                 * to use the port. */
                let take = inner.local_port == port && !inner.accepted
                    && !inner.owned_by_app
                    && matches!(inner.state, State::Established | State::CloseWait);
                if take {
                    inner.accepted = true;
                    inner.owned_by_app = true;
                }
                unsafe { conn.lock.unlock() };

                if take {
                    unsafe { self.pool_lock.unlock() };
                    return Some(conn);
                }
            }
            unsafe { self.pool_lock.unlock() };

            if deadline != 0 && now_ms() >= deadline {
                return None;
            }
            kcore::task::sleep_ms(1);
        }
    }

    /// Queue and send. Answers with the bytes taken: all of them, fewer once
    /// the timeout passes with no room for the rest -- 0 when there was room
    /// for none -- or -1 when the connection went before any were.
    pub fn send(&'static self, conn: &'static Conn, data: &[u8], timeout_ms: u64) -> isize {
        let mut sent = 0;
        /* A deadline for the whole call: a caller that has to notice
         * something else meanwhile -- its server stopping -- calls again
         * with the rest. */
        let deadline = if timeout_ms != 0 { now_ms() + timeout_ms } else { 0 };

        while sent < data.len() {
            conn.lock.lock();
            let inner = unsafe { conn.get() };

            if !matches!(inner.state, State::Established | State::CloseWait) {
                unsafe { conn.lock.unlock() };
                return if sent > 0 { sent as isize } else { -1 };
            }

            let room = inner.send_buf.free();
            if room == 0 {
                unsafe { conn.lock.unlock() };
                if deadline != 0 && now_ms() >= deadline {
                    return sent as isize;
                }
                kcore::task::sleep_ms(1);
                continue;
            }

            /* Never past the peer's window: the bytes in flight must stay
             * within what it said it would take. */
            let in_flight = inner.snd_nxt.wrapping_sub(inner.snd_una);
            if inner.snd_wnd <= in_flight {
                /* Shut, or full. Not a byte into a shut window: the peer's
                 * duplicate acknowledgements would never advance anything,
                 * and the retransmit count would give up on a peer that is
                 * only slow to read -- a suspended ssh client -- as a dead
                 * one, in under a minute. The persist timer probes instead. */
                unsafe { conn.lock.unlock() };
                if deadline != 0 && now_ms() >= deadline {
                    return sent as isize;
                }
                kcore::task::sleep_ms(1);
                continue;
            }
            let window_room = (inner.snd_wnd - in_flight) as usize;

            let chunk = (data.len() - sent)
                .min(room)
                .min(inner.peer_mss as usize)
                .min(window_room);

            inner.send_buf.write(&data[sent..sent + chunk]);

            let mut segment = [0u8; OUR_MSS as usize];
            let at = inner.send_buf.used() - chunk;
            let len = inner.send_buf.peek(&mut segment[..chunk], at);

            self.send_segment(inner, seg::ACK_FLAG | seg::PSH, &segment[..len]);
            inner.snd_nxt = inner.snd_nxt.wrapping_add(len as u32);

            if inner.retransmit_at == 0 {
                inner.retransmit_at = now_ms() + inner.rto_ms;
            }
            unsafe { conn.lock.unlock() };
            sent += chunk;
        }

        sent as isize
    }

    /// What has arrived. 0 at the end of the stream, RECV_TIMEOUT once the
    /// timeout passes with the buffer still empty.
    pub fn recv(&'static self, conn: &'static Conn, buf: &mut [u8], timeout_ms: u64)
        -> isize
    {
        /* An idle deadline, not one for the whole transfer: every byte that
         * arrives pushes it out again, so a long but progressing download
         * never trips it while a peer that goes silent does. */
        let deadline = if timeout_ms != 0 { now_ms() + timeout_ms } else { 0 };

        loop {
            conn.lock.lock();
            let inner = unsafe { conn.get() };

            if inner.recv_buf.used() > 0 {
                let got = inner.recv_buf.read(buf);
                inner.rcv_wnd = inner.recv_buf.free() as u32;
                if inner.recv_buf.used() == 0 {
                    conn.data_ready.store(false, Ordering::Release);
                }

                /* If what we last advertised was too small for a segment,
                 * the peer may have stopped sending. Now that the reader has
                 * drained the buffer, say so unasked -- no other path would,
                 * and the peer would sit probing a window that has long
                 * since reopened. */
                if inner.advertised_wnd < OUR_MSS as u32
                    && inner.rcv_wnd >= OUR_MSS as u32
                    && matches!(inner.state,
                        State::Established | State::FinWait1 | State::FinWait2)
                {
                    self.send_ack(inner);
                }

                unsafe { conn.lock.unlock() };
                return got as isize;
            }

            let closing = matches!(inner.state, State::CloseWait | State::Closed
                | State::TimeWait | State::LastAck | State::Closing);
            unsafe { conn.lock.unlock() };

            if closing {
                return 0;
            }
            if deadline != 0 && now_ms() > deadline {
                return RECV_TIMEOUT;
            }
            kcore::task::sleep_ms(1);
        }
    }
}

/* ---- closing ---- */

impl Tcp {
    /// A listener's close, which also resets every connection that arrived
    /// on its port and was never accepted: nobody is left to accept them,
    /// and a peer that finished its handshake would otherwise sit holding a
    /// slot until it gave up by itself. False when it is not a listener.
    fn close_listener(&'static self, conn: &'static Conn) -> bool {
        self.pool_lock.lock();
        conn.lock.lock();
        let inner = unsafe { conn.get() };
        if inner.state != State::Listen {
            unsafe { conn.lock.unlock() };
            unsafe { self.pool_lock.unlock() };
            return false;
        }

        let port = inner.local_port;
        inner.state = State::Free;
        inner.owned_by_app = false;
        unsafe { conn.lock.unlock() };

        self.reset_unaccepted_and_unlock(port);
        true
    }

    /// The caller holds the pool lock, which this lets go of: every
    /// connection on the port that nobody accepted is reset, and the resets
    /// go out once the lock is down.
    fn reset_unaccepted_and_unlock(&'static self, port: u16) {
        /* What each reset goes out with, taken from the slots rather than
         * sent from them: by the time the lock is down the cleanup timer may
         * have handed those slots on. */
        struct Reset {
            nic: Option<Nic>,
            mac: Mac,
            local_ip: u32,
            remote_ip: u32,
            local_port: u16,
            remote_port: u16,
            seq: u32,
            ack: u32,
        }
        const NOTHING: Reset = Reset {
            nic: None, mac: [0; 6], local_ip: 0, remote_ip: 0,
            local_port: 0, remote_port: 0, seq: 0, ack: 0,
        };
        let mut resets = [NOTHING; MAX_CONNECTIONS];
        let mut count = 0;

        /* Under the pool lock throughout, as accept scans: a listen that
         * takes the port the moment the old listener's slot is free gets
         * nothing in before this walk is over, so what it marks is the old
         * one's. */
        for i in 0..MAX_CONNECTIONS {
            let conn = self.slot(i);
            conn.lock.lock();
            let inner = unsafe { conn.get() };
            if inner.local_port == port && !inner.accepted && !inner.owned_by_app
                && matches!(inner.state,
                    State::SynReceived | State::Established | State::CloseWait)
            {
                resets[count] = Reset {
                    nic: inner.nic,
                    mac: inner.peer_mac,
                    local_ip: inner.local_ip,
                    remote_ip: inner.remote_ip,
                    local_port: inner.local_port,
                    remote_port: inner.remote_port,
                    seq: inner.snd_nxt,
                    ack: inner.rcv_nxt,
                };
                count += 1;

                inner.state = State::Closed;
                inner.retransmit_at = 0;
                inner.need_cleanup = true;
                conn.conn_ready.store(true, Ordering::Release);
                conn.data_ready.store(true, Ordering::Release);
            }
            unsafe { conn.lock.unlock() };
        }
        unsafe { self.pool_lock.unlock() };

        for reset in resets.iter().take(count) {
            if let Some(nic) = reset.nic {
                self.send_rst(&nic, &reset.mac, reset.local_ip, reset.remote_ip,
                    reset.local_port, reset.remote_port, reset.seq, reset.ack);
            }
        }

        if count != 0 {
            trace!(0, "tcp: listener on port {} closed, {} connections it never accepted were reset",
                port, count);
        }
    }

    /// The graceful close: a FIN, and the exchange that follows.
    pub fn close(&'static self, conn: &'static Conn) {
        if self.close_listener(conn) {
            return;
        }

        conn.lock.lock();
        let inner = unsafe { conn.get() };

        match inner.state {
            State::Established | State::SynReceived => {
                inner.state = State::FinWait1;
                inner.fin_acked = false;
                self.send_segment(inner, seg::FIN | seg::ACK_FLAG, &[]);
                inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                inner.retransmit_at = now_ms() + inner.rto_ms;
            }
            State::CloseWait => {
                inner.state = State::LastAck;
                self.send_segment(inner, seg::FIN | seg::ACK_FLAG, &[]);
                inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                inner.retransmit_at = now_ms() + inner.rto_ms;
            }
            State::SynSent => inner.state = State::Closed,
            State::Listen => inner.state = State::Free,
            _ => {}
        }

        /* The application is done with this pointer. From here the cleanup
         * timer owns the slot and may recycle it once the connection is
         * closed. */
        inner.owned_by_app = false;
        unsafe { conn.lock.unlock() };
    }

    /// The abrupt close: a reset instead of the exchange, and the slot back
    /// at the next tick rather than after a minute of TIME-WAIT. For a
    /// connection a server refuses or drops.
    pub fn abort(&'static self, conn: &'static Conn) {
        /* A listener has nothing else to reset, so its abort is its close */
        if self.close_listener(conn) {
            return;
        }

        conn.lock.lock();
        let inner = unsafe { conn.get() };

        match inner.state {
            State::SynReceived | State::Established | State::CloseWait
            | State::FinWait1 | State::FinWait2 | State::Closing | State::LastAck => {
                self.send_segment(inner, seg::RST | seg::ACK_FLAG, &[]);
                inner.state = State::Closed;
            }
            State::SynSent | State::TimeWait => inner.state = State::Closed,
            _ => {}
        }

        inner.retransmit_at = 0;
        inner.need_cleanup = true;
        conn.conn_ready.store(true, Ordering::Release);
        conn.data_ready.store(true, Ordering::Release);
        /* As close: from here the slot is the cleanup timer's */
        inner.owned_by_app = false;
        unsafe { conn.lock.unlock() };
    }
}

/* ---- the timer ---- */

impl Tcp {
    /// Every two hundred milliseconds, from the soft IRQ the tick raises.
    pub fn process_retransmits(&'static self) {
        let now = now_ms();
        let mut any_cleanup = false;

        /* What each connection owes, with only its own lock */
        for i in 0..MAX_CONNECTIONS {
            let conn = self.slot(i);
            conn.lock.lock();
            let inner = unsafe { conn.get() };

            if inner.state == State::Free {
                unsafe { conn.lock.unlock() };
                continue;
            }

            /* TIME-WAIT out, and FIN-WAIT-2 out when the peer never sends
             * its FIN (RFC 1122 4.2.2.20) */
            if matches!(inner.state, State::TimeWait | State::FinWait2)
                && inner.time_wait_at != 0 && now >= inner.time_wait_at
            {
                inner.state = State::Closed;
                inner.need_cleanup = true;
                conn.conn_ready.store(true, Ordering::Release);
                conn.data_ready.store(true, Ordering::Release);
                any_cleanup = true;
                unsafe { conn.lock.unlock() };
                continue;
            }

            if inner.state == State::Closed {
                inner.need_cleanup = true;
                any_cleanup = true;
                unsafe { conn.lock.unlock() };
                continue;
            }

            if inner.retransmit_at != 0 && now >= inner.retransmit_at {
                let mut retransmitted = false;

                match inner.state {
                    /* A handshake retransmit is about the state, not about
                     * buffered data */
                    State::SynSent => {
                        inner.snd_nxt = inner.snd_una;
                        self.send_segment(inner, seg::SYN, &[]);
                        inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                        retransmitted = true;
                    }
                    State::SynReceived => {
                        inner.snd_nxt = inner.snd_una;
                        self.send_segment(inner, seg::SYN | seg::ACK_FLAG, &[]);
                        inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                        retransmitted = true;
                    }
                    State::FinWait1 | State::LastAck | State::Closing => {
                        /* Whatever data is still unacknowledged goes first */
                        let used = inner.send_buf.used();
                        if used > 0 {
                            let len = used.min(inner.peer_mss as usize);
                            let mut segment = [0u8; OUR_MSS as usize];
                            let got = inner.send_buf.peek(&mut segment[..len], 0);
                            if got > 0 {
                                let saved = inner.snd_nxt;
                                inner.snd_nxt = inner.snd_una;
                                self.send_segment(inner, seg::ACK_FLAG | seg::PSH,
                                    &segment[..got]);
                                inner.snd_nxt = saved;
                            }
                        } else {
                            inner.snd_nxt = inner.snd_una;
                            self.send_segment(inner, seg::FIN | seg::ACK_FLAG, &[]);
                            inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                        }
                        retransmitted = true;
                    }
                    _ => {
                        if inner.snd_una != inner.snd_nxt && inner.send_buf.used() > 0 {
                            let len = inner.send_buf.used().min(inner.peer_mss as usize);
                            let mut segment = [0u8; OUR_MSS as usize];
                            let got = inner.send_buf.peek(&mut segment[..len], 0);
                            if got > 0 {
                                let saved = inner.snd_nxt;
                                inner.snd_nxt = inner.snd_una;
                                self.send_segment(inner, seg::ACK_FLAG | seg::PSH,
                                    &segment[..got]);
                                inner.snd_nxt = saved;
                            }
                            retransmitted = true;
                        }
                    }
                }

                if retransmitted {
                    self.retransmits.fetch_add(1, Ordering::Relaxed);
                    inner.retransmit_count += 1;
                    if inner.retransmit_count > MAX_RETRANSMITS {
                        /* Nothing acknowledged after this many tries: give
                         * the slot back. Otherwise a dead peer pins it for
                         * good -- half-open handshakes, unacknowledged FINs
                         * and unacknowledged data all retransmit with no
                         * limit of their own. */
                        let (local, remote, state, count) = (inner.local_port,
                            inner.remote_port, inner.state, inner.retransmit_count);

                        inner.state = State::Closed;
                        inner.retransmit_at = 0;
                        inner.need_cleanup = true;
                        conn.conn_ready.store(true, Ordering::Release);
                        conn.data_ready.store(true, Ordering::Release);
                        any_cleanup = true;
                        unsafe { conn.lock.unlock() };

                        trace!(0, "tcp: {} -> {} in {} aborted after {} retransmits",
                            local, remote, state.name(), count);
                        continue;
                    }
                    inner.rto_ms = (inner.rto_ms * 2).min(MAX_RTO_MS);
                }
                inner.retransmit_at = now + inner.rto_ms;
            }

            /* The persist timer (RFC 9293 3.8.6.1): the peer said its window
             * is shut and everything sent is acknowledged, so nothing is
             * pending and nothing would ever go out again. Probe, so a lost
             * window update cannot stall the connection for good. The
             * probe's sequence number sits below the peer's window, which
             * forces a duplicate acknowledgement carrying the window it has
             * now. */
            if matches!(inner.state, State::Established | State::CloseWait)
                && inner.snd_wnd == 0 && inner.snd_una == inner.snd_nxt
                && inner.retransmit_at == 0
            {
                if inner.persist_at == 0 {
                    inner.persist_at = now + inner.rto_ms;
                } else if now >= inner.persist_at {
                    let saved = inner.snd_nxt;
                    inner.snd_nxt = inner.snd_una.wrapping_sub(1);
                    self.send_segment(inner, seg::ACK_FLAG, &[]);
                    inner.snd_nxt = saved;

                    inner.rto_ms = (inner.rto_ms * 2).min(MAX_RTO_MS);
                    inner.persist_at = now + inner.rto_ms;
                }
            } else {
                inner.persist_at = 0;
            }

            unsafe { conn.lock.unlock() };
        }

        /* And the slots that are done with, under the pool lock */
        if any_cleanup {
            self.pool_lock.lock();
            for i in 0..MAX_CONNECTIONS {
                let conn = self.slot(i);
                conn.lock.lock();
                let inner = unsafe { conn.get() };

                if inner.need_cleanup && inner.state == State::Closed
                    && !inner.owned_by_app
                {
                    self.remove_hash(i);
                    inner.state = State::Free;
                    inner.need_cleanup = false;
                    self.conn_count.fetch_sub(1, Ordering::Relaxed);
                } else if inner.state != State::Closed {
                    inner.need_cleanup = false;
                }
                /* One still held by an application keeps the flag, so the
                 * next tick tries again once it has closed it. */
                unsafe { conn.lock.unlock() };
            }
            unsafe { self.pool_lock.unlock() };
        }
    }

    /* ---- what the shell reports ---- */

    pub fn stats(&'static self) -> Stats {
        Stats {
            tx: self.tx_segments.load(Ordering::Relaxed),
            rx: self.rx_segments.load(Ordering::Relaxed),
            rx_err: self.rx_checksum_err.load(Ordering::Relaxed),
            rx_short: self.rx_too_short.load(Ordering::Relaxed),
            retransmits: self.retransmits.load(Ordering::Relaxed),
            conns: self.conn_count.load(Ordering::Relaxed),
        }
    }

    /// A snapshot of the index'th slot, or None when it is free. Taken under
    /// the lock and printed after it: the printer is the console or a shell
    /// socket, and neither belongs under a connection's lock.
    pub fn snapshot(&'static self, index: usize) -> Option<ConnInfo> {
        if index >= MAX_CONNECTIONS {
            return None;
        }

        let conn = self.slot(index);
        conn.lock.lock();
        let inner = unsafe { conn.get() };
        let info = if inner.state == State::Free {
            None
        } else {
            Some(ConnInfo {
                state: inner.state,
                local_ip: inner.local_ip,
                local_port: inner.local_port,
                remote_ip: inner.remote_ip,
                remote_port: inner.remote_port,
                send_used: inner.send_buf.used(),
                in_flight: inner.snd_nxt.wrapping_sub(inner.snd_una) as usize,
                recv_used: inner.recv_buf.used(),
            })
        };
        unsafe { conn.lock.unlock() };
        info
    }

    /// Who a connection is with, for a server that logs it.
    pub fn peer(&'static self, conn: &'static Conn) -> (u32, u16) {
        conn.lock.lock();
        let inner = unsafe { conn.get() };
        let peer = (inner.remote_ip, inner.remote_port);
        unsafe { conn.lock.unlock() };
        peer
    }
}

pub struct Stats {
    pub tx: usize,
    pub rx: usize,
    pub rx_err: usize,
    pub rx_short: usize,
    pub retransmits: usize,
    pub conns: usize,
}

pub struct ConnInfo {
    pub state: State,
    pub local_ip: u32,
    pub local_port: u16,
    pub remote_ip: u32,
    pub remote_port: u16,
    pub send_used: usize,
    pub in_flight: usize,
    pub recv_used: usize,
}

/// The periodic tick, from IPI context: it only raises the soft IRQ.
extern "C" fn on_tick(_ctx: *mut u8) {
    kcore::softirq::raise(kcore::softirq::TYPE_TCP_TIMER);
}

/// The soft IRQ the tick raised.
extern "C" fn on_softirq(_ctx: *mut u8) {
    TCP.process_retransmits();
}
