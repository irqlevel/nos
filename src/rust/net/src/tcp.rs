//! TCP: the connections, the state machine and the timers behind them.
//!
//! One pool of 64 connections, a static rather than an allocation, with a
//! lock per connection and one over the pool. A segment arrives on the
//! receive path, is matched to a connection by its four addresses, and the
//! state machine runs under that connection's lock alone; the blocking calls
//! -- connect, accept, send, receive -- sleep in a task and look again.
//!
//! A connection buffers 8 KiB each way in its slot: enough for a shell, or a
//! request and its answer, and all that a connection someone else opens is
//! ever given. One this machine opens to fetch in bulk asks for more
//! (`Receive::Bulk`) -- a receive buffer of up to 512 KiB from the heap, and
//! its window scaled to match (RFC 7323) when the peer agrees: a transfer
//! moves a window a round trip, and 8 KiB a round trip is 200 KiB/s at 40 ms.
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
//! - **Nothing is allocated or freed under a lock.** A bulk buffer comes from
//!   the heap before `connect` takes one, and goes back once the connection
//!   is done with it: the timer takes it out under the locks and drops it
//!   after them.
//! - **The window offered never shrinks.** A scaled window is said in units
//!   of two to the shift, rounded down; one unit of the buffer is kept spare,
//!   so that the edge offered before can always be kept, rounded up, inside
//!   it (`offer_window`).
//! - **What arrives ahead of a hole is kept**, in the receive buffer where it
//!   belongs, as up to `MAX_OOO` stretches, and taken in whole when the hole
//!   fills. Dropped, one lost segment of a window of hundreds of kilobytes
//!   would cost everything sent after it.
//! - **A reset, a SYN or an acknowledgement a blind attacker could guess is
//!   answered, not obeyed** (RFC 5961): a RST counts only at exactly
//!   RCV.NXT, a SYN on a synchronized connection is answered with an ACK, and
//!   so is an acknowledgement of nothing sent, or of what is older than the
//!   peer's largest window. A window of 512 KiB is 64 times as easy to land
//!   a guess in as one of 8 KiB.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::nic::Nic;
use kcore::sync::{PreemptSpinGuard, PreemptSpinLock};
use kcore::trace;

use crate::abi;
use crate::wire::{self, eth, ip, tcp as seg, Mac, ETH_HDR_LEN, ETH_TYPE_IP,
                  IP_HDR_LEN, IP_PROTO_TCP};

pub const MAX_CONNECTIONS: usize = 64;
/// What a slot buffers each way, in the slot itself.
const RING_BUF: usize = 8192;
pub const SEND_BUF: usize = RING_BUF;
pub const RECV_BUF: usize = RING_BUF;

/// A bulk connection's receive buffer: the first of these the heap has --
/// the largest block the page allocator hands out (128 pages), then less.
const BULK_SIZES: [usize; 3] = [512 * 1024, 256 * 1024, 128 * 1024];
/// The most bulk buffers out at once, whoever asks: 4 MiB of heap.
const MAX_BULK: usize = 8;
/* Every one more than 16 bits say, so that its shift is never 0: a
 * connection scales exactly when this end's shift is not 0. */
const _: () = assert!(BULK_SIZES[BULK_SIZES.len() - 1] > u16::MAX as usize);

/// The stretches of what arrived ahead of a hole that a connection keeps.
/// Past that a segment ahead of one is dropped, as before there were any.
const MAX_OOO: usize = 8;

pub const DEFAULT_MSS: u16 = 536;
pub const OUR_MSS: u16 = 1460;

const INITIAL_RTO_MS: u64 = 1000;
const MAX_RTO_MS: u64 = 8000;
const MAX_RETRANSMITS: u32 = 8;
/// The most segments a timeout sends again of one connection: all 8 KiB of
/// its send buffer at an MSS of 128 bytes or more, and no storm of frames
/// from the timer at a smaller one.
const RESEND_SEGMENTS: usize = 64;
/// Linux's sixty seconds standing in for twice the maximum segment lifetime:
/// a segment of an old incarnation must not be matched by a new connection
/// reusing the same four addresses.
const TIME_WAIT_MS: u64 = 60_000;
const FIN_WAIT2_TIMEOUT_MS: u64 = 60_000;
const CONNECT_TIMEOUT_MS: u64 = 5000;
const TIMER_PERIOD_MS: u64 = 200;
/// The least time between two ACKs for segments from outside the window --
/// and for the challenges RFC 5961 answers a guess with.
const OOW_ACK_GAP_MS: u64 = 500;
const DEFAULT_TTL: u8 = 64;

const HASH_SIZE: usize = 32;
pub(crate) const EPHEMERAL_BASE: u16 = 49152;
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

/* ---- what a connection may take in ---- */

/// How much a connection may take in before its program reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Receive {
    /// The slot's own 8 KiB, the window unscaled: what a connection that
    /// talks rather than fetches -- a shell, a request and its answer --
    /// needs.
    Small,
    /// A buffer from the heap, as big as one comes (`BULK_SIZES`), and the
    /// window scaled to it when the peer agrees: a download. The slot's own
    /// when the heap has none to give, or `MAX_BULK` are out already.
    Bulk,
}

/// How many bulk buffers are out.
static BULK_OUT: AtomicUsize = AtomicUsize::new(0);

/// A bulk connection's receive buffer: counted while it lives, so that no
/// more than `MAX_BULK` are ever out, and dropped only where no lock is
/// held.
struct BulkBuf(Box<[u8]>);

impl BulkBuf {
    /// The largest of `BULK_SIZES` the heap has -- none once `MAX_BULK` are
    /// out. Allocates: no lock may be held.
    fn new() -> Option<BulkBuf> {
        /* Counted before it is taken: two connects at once cannot both have
         * the last one. */
        if BULK_OUT.fetch_add(1, Ordering::AcqRel) >= MAX_BULK {
            BULK_OUT.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        for size in BULK_SIZES {
            let mut bytes = Vec::new();
            if bytes.try_reserve_exact(size).is_ok() {
                bytes.resize(size, 0);
                return Some(BulkBuf(bytes.into_boxed_slice()));
            }
        }
        BULK_OUT.fetch_sub(1, Ordering::AcqRel);
        None
    }
}

impl Drop for BulkBuf {
    fn drop(&mut self) {
        BULK_OUT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The least shift that says a window as big as `size` in the 16 bits a
/// segment has for one.
fn wscale_for(size: usize) -> u8 {
    let mut shift = 0;
    while size >> shift > u16::MAX as usize && shift < seg::MAX_WSCALE {
        shift += 1;
    }
    shift
}

/* ---- the byte ring a connection buffers with ---- */

/// What a connection holds one way. `head` and `tail` only grow: what is
/// buffered lies between them, at their remainders by the size, and the
/// free space after the tail.
struct Ring {
    /// The slot's own bytes
    data: [u8; RING_BUF],
    /// A bulk connection's, in their place
    bulk: Option<BulkBuf>,
    head: usize,
    tail: usize,
    size: usize,
}

impl Ring {
    const fn new() -> Ring {
        Ring { data: [0; RING_BUF], bulk: None, head: 0, tail: 0, size: RING_BUF }
    }

    fn bytes(&self) -> &[u8] {
        match &self.bulk {
            Some(bulk) => &bulk.0,
            None => &self.data,
        }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        match &mut self.bulk {
            Some(bulk) => &mut bulk.0,
            None => &mut self.data,
        }
    }

    fn used(&self) -> usize {
        self.tail - self.head
    }

    fn free(&self) -> usize {
        self.size - self.used()
    }

    /// As much of `src` as fits into the free space, from `offset` bytes
    /// past the tail, with the tail left where it is: what arrived ahead of
    /// a hole goes where it belongs. The bytes copied. Two copies at most,
    /// either side of the wrap -- a byte at a time, each with a division,
    /// was a CPU's worth at the rates a bulk buffer takes.
    fn put(&mut self, offset: usize, src: &[u8]) -> usize {
        let free = self.free();
        if offset >= free {
            return 0;
        }
        let len = src.len().min(free - offset);
        let size = self.size;
        let at = (self.tail + offset) % size;
        let first = len.min(size - at);
        let bytes = self.bytes_mut();
        bytes[at..at + first].copy_from_slice(&src[..first]);
        bytes[..len - first].copy_from_slice(&src[first..len]);
        len
    }

    /// `len` bytes past the tail, which `put` left there, made part of what
    /// is buffered.
    fn commit(&mut self, len: usize) {
        self.tail += len.min(self.free());
    }

    fn write(&mut self, src: &[u8]) -> usize {
        let len = self.put(0, src);
        self.tail += len;
        len
    }

    /// Bytes without taking them.
    fn peek(&self, dst: &mut [u8], offset: usize) -> usize {
        let used = self.used();
        if offset >= used {
            return 0;
        }
        let len = dst.len().min(used - offset);
        let at = (self.head + offset) % self.size;
        let first = len.min(self.size - at);
        let bytes = self.bytes();
        dst[..first].copy_from_slice(&bytes[at..at + first]);
        dst[first..len].copy_from_slice(&bytes[..len - first]);
        len
    }

    fn read(&mut self, dst: &mut [u8]) -> usize {
        let len = self.peek(dst, 0);
        self.head += len;
        len
    }

    fn consume(&mut self, len: usize) {
        self.head += len.min(self.used());
    }

    fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
    }

    /// A bulk buffer in place of the slot's own, for a connection being set
    /// up: nothing is buffered yet.
    fn attach(&mut self, bulk: BulkBuf) {
        assert!(self.bulk.is_none() && self.used() == 0, "tcp: a bulk buffer onto a ring in use");
        self.size = bulk.0.len();
        self.bulk = Some(bulk);
        self.reset();
    }

    /// The bulk buffer back, if there is one, for the caller to drop once no
    /// lock is held -- and the slot's own in its place, empty.
    fn detach(&mut self) -> Option<BulkBuf> {
        self.size = RING_BUF;
        self.reset();
        self.bulk.take()
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
    /// What the receive buffer has free: how far past RCV.NXT a segment's
    /// data may go and still be kept
    rcv_wnd: u32,
    /// What our last outgoing segment advertised, in bytes
    advertised_wnd: u32,
    /// The right edge of the window last offered: no later offer falls
    /// short of it
    rcv_adv: u32,
    iss: u32,
    irs: u32,
    peer_mss: u16,

    /// The shift each end applies to the windows it advertises, once both
    /// SYNs said so (RFC 7323): this end's to what it offers, the peer's to
    /// what it is offered. 0 on a connection that does not scale.
    rcv_wscale: u8,
    snd_wscale: u8,
    /// The shift this end's SYN offers -- a bulk connection's -- if any
    wscale_offer: Option<u8>,
    /// The largest window the peer has offered: how far below SND.UNA an
    /// acknowledgement may still be one of its (RFC 5961 5.2)
    max_snd_wnd: u32,

    send_buf: Ring,
    recv_buf: Ring,
    /// What arrived ahead of a hole: stretches of the stream [start, end),
    /// in sequence numbers, each in the receive buffer where it belongs
    ooo: [(u32, u32); MAX_OOO],
    ooo_len: usize,

    rto_ms: u64,
    retransmit_at: u64,
    /// Retransmits with no acknowledgement in between
    retransmit_count: u32,
    /// Also the FIN-WAIT-2 deadline
    time_wait_at: u64,
    /// The zero-window probe's
    persist_at: u64,
    /// When a segment from outside the window was last answered
    oow_ack_at: u64,

    need_cleanup: bool,
    /// Our FIN has been acknowledged
    fin_acked: bool,
    /// An application still holds this: the cleanup timer must not recycle
    /// the slot until `close` gives it up
    owned_by_app: bool,
    /// A passive connection already handed out by `accept`
    accepted: bool,
}

impl Inner {
    const fn new() -> Inner {
        Inner {
            local_ip: 0, local_port: 0, remote_ip: 0, remote_port: 0,
            nic: None, peer_mac: [0; 6],
            state: State::Free,
            snd_una: 0, snd_nxt: 0, snd_wnd: 0, snd_wl1: 0, snd_wl2: 0,
            rcv_nxt: 0, rcv_wnd: RECV_BUF as u32, advertised_wnd: RECV_BUF as u32, rcv_adv: 0,
            iss: 0, irs: 0, peer_mss: DEFAULT_MSS,
            rcv_wscale: 0, snd_wscale: 0, wscale_offer: None, max_snd_wnd: 0,
            send_buf: Ring::new(),
            recv_buf: Ring::new(),
            ooo: [(0, 0); MAX_OOO], ooo_len: 0,
            rto_ms: INITIAL_RTO_MS,
            retransmit_at: 0, retransmit_count: 0, time_wait_at: 0, persist_at: 0, oow_ack_at: 0,
            need_cleanup: false, fin_acked: false, owned_by_app: false,
            accepted: false,
        }
    }

    /// Everything but the buffers' bytes.
    fn init(&mut self) {
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
        self.rcv_adv = 0;
        self.iss = 0;
        self.irs = 0;
        self.peer_mss = DEFAULT_MSS;
        self.rcv_wscale = 0;
        self.snd_wscale = 0;
        self.wscale_offer = None;
        self.max_snd_wnd = 0;
        /* A bulk buffer goes back before its slot does, outside the locks
         * this runs under: one still here would be dropped under them. */
        assert!(self.recv_buf.bulk.is_none(), "tcp: a slot handed out again with its bulk buffer");
        self.send_buf.reset();
        self.recv_buf.reset();
        self.ooo_len = 0;
        self.rto_ms = INITIAL_RTO_MS;
        self.retransmit_at = 0;
        self.retransmit_count = 0;
        self.time_wait_at = 0;
        self.persist_at = 0;
        self.oow_ack_at = 0;
        self.need_cleanup = false;
        self.fin_acked = false;
        self.owned_by_app = false;
        self.accepted = false;
    }
}

/// One connection: what its own lock guards, and the two flags a waiting
/// task reads without it.
pub struct Conn {
    inner: PreemptSpinLock<Inner>,
    /// Data has arrived; read without the lock by a waiting task
    data_ready: AtomicBool,
    /// The state left SYN-SENT or SYN-RECEIVED
    conn_ready: AtomicBool,
}

impl Conn {
    const fn new() -> Conn {
        Conn {
            inner: PreemptSpinLock::new(Inner::new()),
            data_ready: AtomicBool::new(false),
            conn_ready: AtomicBool::new(false),
        }
    }
}

/* ---- the pool: which slot is whose ---- */

/// No slot: the end of a hash chain, an empty bucket.
const NONE: i8 = -1;

/// What a slot is, as far as *finding* it goes -- and nothing more. A walk of
/// the pool reads these and never a connection's own state, so it takes no
/// connection's lock: the identity of a slot is the pool's to keep, written
/// when the slot is handed out and when it comes back, both under the pool
/// lock.
#[derive(Clone, Copy)]
struct Slot {
    used: bool,
    listening: bool,
    local_ip: u32,
    local_port: u16,
    remote_ip: u32,
    remote_port: u16,
    /// The device it belongs to: the one a listener was opened on, the one
    /// a connection came in or went out by. A segment from any other is
    /// not its own, whatever addresses it carries -- the guests' switch
    /// (`hv0`) reaches neither a service listening on eth0 nor a
    /// connection made over it.
    device: Option<Nic>,
}

impl Slot {
    const FREE: Slot = Slot {
        used: false, listening: false,
        local_ip: 0, local_port: 0, remote_ip: 0, remote_port: 0,
        device: None,
    };

    fn is(&self, local_ip: u32, local_port: u16, remote_ip: u32, remote_port: u16, nic: &Nic) -> bool {
        self.used && !self.listening
            && self.local_ip == local_ip && self.local_port == local_port
            && self.remote_ip == remote_ip && self.remote_port == remote_port
            && self.device.is_some_and(|d| d.is(nic))
    }
}

/// What the pool lock guards: the identity of every slot, the hash over the
/// connected ones, and where the next ephemeral port comes from.
struct Pool {
    slots: [Slot; MAX_CONNECTIONS],
    /// Head slot of each bucket
    buckets: [i8; HASH_SIZE],
    /// The chain through a bucket, per slot
    next: [i8; MAX_CONNECTIONS],
    hashed: [bool; MAX_CONNECTIONS],
    next_ephemeral: u16,
}

impl Pool {
    const fn new() -> Pool {
        Pool {
            slots: [Slot::FREE; MAX_CONNECTIONS],
            buckets: [NONE; HASH_SIZE],
            next: [NONE; MAX_CONNECTIONS],
            hashed: [false; MAX_CONNECTIONS],
            next_ephemeral: EPHEMERAL_BASE,
        }
    }

    /// The slot those four addresses name.
    fn lookup(&self, local_ip: u32, local_port: u16, remote_ip: u32, remote_port: u16, nic: &Nic)
        -> Option<usize>
    {
        let mut at = self.buckets[hash_index(local_ip, local_port, remote_ip, remote_port)];
        while at >= 0 {
            let index = at as usize;
            if self.slots[index].is(local_ip, local_port, remote_ip, remote_port, nic) {
                return Some(index);
            }
            at = self.next[index];
        }
        None
    }

    /// The listener for that address and port on `nic`, the device the SYN
    /// came in by: one opened on another device is not asked. One bound to
    /// an address must not take a segment aimed at another, while a
    /// wildcard one takes any.
    fn find_listener(&self, local_ip: u32, local_port: u16, nic: &Nic) -> Option<usize> {
        (0..MAX_CONNECTIONS).find(|&i| {
            let slot = &self.slots[i];
            slot.used && slot.listening && slot.local_port == local_port
                && (slot.local_ip == local_ip || slot.local_ip == 0)
                && slot.device.is_some_and(|d| d.is(nic))
        })
    }

    /// A free slot, taken as `slot`. The caller makes the connection itself
    /// ready, under the connection's own lock.
    fn alloc(&mut self, slot: Slot) -> Option<usize> {
        let index = (0..MAX_CONNECTIONS).find(|&i| !self.slots[i].used)?;
        self.slots[index] = slot;
        if !slot.listening {
            /* A listener is not in the hash: a SYN scans for it */
            let bucket = hash_index(slot.local_ip, slot.local_port,
                slot.remote_ip, slot.remote_port);
            self.next[index] = self.buckets[bucket];
            self.buckets[bucket] = index as i8;
            self.hashed[index] = true;
        }
        Some(index)
    }

    /// The slot back: out of the hash, and free for the next taker.
    fn free(&mut self, index: usize) {
        if self.hashed[index] {
            let slot = self.slots[index];
            let bucket = hash_index(slot.local_ip, slot.local_port,
                slot.remote_ip, slot.remote_port);

            if self.buckets[bucket] == index as i8 {
                self.buckets[bucket] = self.next[index];
            } else {
                let mut at = self.buckets[bucket];
                while at >= 0 {
                    if self.next[at as usize] == index as i8 {
                        self.next[at as usize] = self.next[index];
                        break;
                    }
                    at = self.next[at as usize];
                }
            }
            self.next[index] = NONE;
            self.hashed[index] = false;
        }
        self.slots[index] = Slot::FREE;
    }

    /// A local port nothing is using, or 0 once all the way round without
    /// one.
    fn alloc_ephemeral_port(&mut self) -> u16 {
        let start = self.next_ephemeral;
        loop {
            let port = self.next_ephemeral;
            self.next_ephemeral =
                if port >= EPHEMERAL_MAX { EPHEMERAL_BASE } else { port + 1 };

            if !self.slots.iter().any(|slot| slot.used && slot.local_port == port) {
                return port;
            }
            if self.next_ephemeral == start {
                return 0;
            }
        }
    }
}

/* ---- the one TCP ---- */

/// Two locks, and the types say which guards what: `pool` the identity of
/// the slots, each connection's own lock everything about that connection.
/// The order is pool first, then a connection; a connection found by a walk
/// of the pool is locked before the pool is let go of, which is what keeps
/// the slot from changing hands in between.
pub struct Tcp {
    pool: PreemptSpinLock<Pool>,
    conns: [Conn; MAX_CONNECTIONS],
    initialized: AtomicBool,

    tx_segments: AtomicUsize,
    rx_segments: AtomicUsize,
    rx_checksum_err: AtomicUsize,
    rx_too_short: AtomicUsize,
    retransmits: AtomicUsize,
    conn_count: AtomicUsize,
}

/// The one of these. A static, pool and all -- 64 connections with two 8 KiB
/// buffers each is about a megabyte, which is what the C++ had in .bss too,
/// and what nothing would hand out from the heap.
pub static TCP: Tcp = Tcp {
    pool: PreemptSpinLock::new(Pool::new()),
    conns: [const { Conn::new() }; MAX_CONNECTIONS],
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

/// A wait that nothing signals -- an answer, a connection, room in the
/// window, data -- polled with a sleep that doubles: a millisecond at first,
/// so what comes soon is seen at once, and up to ten, so that a wait that
/// goes on -- a listener's, an idle session's -- wakes its CPU a hundred
/// times a second and not a thousand. The sleeps were a millisecond each
/// and lasted a tick, ten; since a sleep ends at its deadline
/// (docs/scheduler.md, the local timer), a millisecond is one, and an idle
/// nos on the EX44 with sshd up and two sessions open woke 2,200 times a
/// second.
struct Poll {
    ms: u64,
}

impl Poll {
    const FIRST_MS: u64 = 1;
    const MOST_MS: u64 = 10;

    fn new() -> Poll {
        Poll { ms: Self::FIRST_MS }
    }

    fn sleep(&mut self) {
        kcore::task::sleep_ms(self.ms);
        self.ms = (self.ms * 2).min(Self::MOST_MS);
    }
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

        kcore::softirq::register_for(kcore::softirq::TYPE_TCP_TIMER, self, Tcp::on_softirq);

        let period = kcore::time::Duration::from_nanos(
            TIMER_PERIOD_MS * kcore::consts::NS_PER_MS);
        match kcore::timer::Timer::start_for(period, self, Tcp::on_tick) {
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

    /// Which slot a connection is: where it sits in the array, worked out
    /// from its address. None for a reference to something that is not one
    /// of the pool's.
    fn index_of(&'static self, conn: &Conn) -> Option<usize> {
        self.slot_at(conn as *const Conn as usize)
    }

    /// Which slot starts at `address`, if one does.
    fn slot_at(&'static self, address: usize) -> Option<usize> {
        let base = self.conns.as_ptr() as usize;
        let offset = address.checked_sub(base)?;
        let size = core::mem::size_of::<Conn>();
        if offset % size != 0 || offset / size >= MAX_CONNECTIONS {
            return None;
        }
        Some(offset / size)
    }

    /// The connection a caller outside Rust names by its address: one of the
    /// pool's, or none.
    pub fn by_handle(&'static self, handle: usize) -> Option<&'static Conn> {
        self.slot_at(handle).map(|index| &self.conns[index])
    }

    /// A fresh connection in a slot the pool has just handed out: locked,
    /// cleared, and the caller's to fill. The caller holds the pool lock, and
    /// holds no other connection's -- a free slot has no user, so this lock
    /// is only ever contended by a walk passing through.
    fn take_fresh(&'static self, index: usize) -> PreemptSpinGuard<'static, Inner> {
        let conn = &self.conns[index];
        let mut inner = conn.inner.lock();
        inner.init();
        conn.data_ready.store(false, Ordering::Release);
        conn.conn_ready.store(false, Ordering::Release);
        inner
    }
}

/* ---- segments out ---- */

impl Tcp {
    /// One segment of this connection. The caller holds its lock.
    ///
    /// A SYN carries the maximum segment size option, and the window scale
    /// when this end offers one: a header of 24 bytes, or 28, instead of 20.
    fn send_segment(&'static self, inner: &mut Inner, flags: u8, data: &[u8]) {
        let nic = match inner.nic {
            Some(nic) => nic,
            None => return,
        };

        let syn = flags & seg::SYN != 0;
        let wscale = if syn { inner.wscale_offer } else { None };
        let mut hdr_len = seg::HDR_LEN;
        if syn {
            hdr_len += seg::OPT_MSS_LEN as usize;
        }
        if wscale.is_some() {
            /* A NOP before it keeps the header a whole number of words */
            hdr_len += 1 + seg::OPT_WS_LEN as usize;
        }
        let tcp_len = hdr_len + data.len();
        let frame_len = ETH_HDR_LEN + IP_HDR_LEN + tcp_len;
        if frame_len > MAX_FRAME {
            return;
        }

        /* A SYN's window is never scaled (RFC 7323 2.2): it says what fits
         * in 16 bits, and the scaling starts after it. */
        let window = if syn {
            let window = inner.rcv_wnd.min(u16::MAX as u32);
            inner.advertised_wnd = window;
            window as u16
        } else {
            offer_window(inner)
        };

        let mut frame = [0u8; MAX_FRAME];
        eth::write(&mut frame, &inner.peer_mac, &nic.mac(), ETH_TYPE_IP);
        ip::write(&mut frame[ETH_HDR_LEN..], IP_PROTO_TCP,
            inner.local_ip, inner.remote_ip, tcp_len, 0);
        frame[ETH_HDR_LEN + ip::TTL] = DEFAULT_TTL;
        recompute_ip_checksum(&mut frame[ETH_HDR_LEN..]);

        let at = ETH_HDR_LEN + IP_HDR_LEN;
        seg::write(&mut frame[at..], inner.local_port, inner.remote_port,
            inner.snd_nxt, inner.rcv_nxt, hdr_len, flags, window);

        if syn {
            let opt = at + seg::HDR_LEN;
            frame[opt] = seg::OPT_MSS;
            frame[opt + 1] = seg::OPT_MSS_LEN;
            frame[opt + 2..opt + 4].copy_from_slice(&OUR_MSS.to_be_bytes());
            if let Some(shift) = wscale {
                frame[opt + 4] = seg::OPT_NOP;
                frame[opt + 5] = seg::OPT_WS;
                frame[opt + 6] = seg::OPT_WS_LEN;
                frame[opt + 7] = shift;
            }
        }
        if !data.is_empty() {
            frame[at + hdr_len..at + tcp_len].copy_from_slice(data);
        }

        let sum = seg::checksum(inner.local_ip, inner.remote_ip,
            &frame[at..at + tcp_len]);
        wire::set_be16(&mut frame[at..], seg::CHECKSUM, sum);

        nic.send_raw(&frame[..frame_len]);
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
        /* RFC 9293 3.10.7.4: take the advertisement only from a segment
         * that acknowledges something this end sent -- SND.UNA =< SEG.ACK
         * =< SND.NXT, so that one which is no part of the peer's stream
         * cannot set it -- and that is newer than the one that last set
         * it, so that a reordered stale one cannot undo it. Newer by its
         * sequence number, as the RFC has it, or by its acknowledgement, as
         * Linux has it (tcp_may_update_window): a retransmission carries
         * the peer's current acknowledgement and window under an old
         * sequence number, and a window kept from before it, with the
         * edge of what it acknowledges moved on, is room the peer never
         * offered. */
        if before_eq(inner.snd_una, ack) && before_eq(ack, inner.snd_nxt)
            && (before(inner.snd_una, ack) || before(inner.snd_wl1, segment_seq)
                || (inner.snd_wl1 == segment_seq && before_eq(inner.snd_wl2, ack)))
        {
            inner.snd_wnd = (wnd as u32) << inner.snd_wscale;
            inner.max_snd_wnd = inner.max_snd_wnd.max(inner.snd_wnd);
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

    /// A segment's data into the receive buffer, and the acknowledgement it
    /// requires. In order, it is taken -- and with it whatever had come
    /// ahead of it and now joins up; ahead of a hole, it is kept where it
    /// belongs for when the hole fills, and the acknowledgement is a
    /// duplicate, which is what has the peer send the hole again (RFC 5681
    /// 4.2). Shared by ESTABLISHED and the FIN-WAIT states, because the peer
    /// may keep sending after our FIN (RFC 793 half-close).
    fn process_payload(&'static self, conn: &Conn, inner: &mut Inner, seq: u32,
        payload: &[u8])
    {
        let mut seq = seq;
        let mut data = payload;

        /* What was taken already -- a retransmission's start -- cut off */
        if before(seq, inner.rcv_nxt) {
            let taken = inner.rcv_nxt.wrapping_sub(seq) as usize;
            data = &data[taken.min(data.len())..];
            seq = inner.rcv_nxt;
        }

        if !data.is_empty() {
            /* Not before RCV.NXT now, and in the window: a small number */
            let offset = seq.wrapping_sub(inner.rcv_nxt) as usize;
            if offset == 0 {
                let written = inner.recv_buf.write(data);
                inner.rcv_nxt = inner.rcv_nxt.wrapping_add(written as u32);
                take_ooo(inner);
                conn.data_ready.store(true, Ordering::Release);
            } else {
                let kept = inner.recv_buf.put(offset, data);
                if kept != 0 {
                    keep_ooo(inner, seq, seq.wrapping_add(kept as u32));
                }
            }
            inner.rcv_wnd = inner.recv_buf.free() as u32;
        }

        self.send_ack(inner);
    }

    /// The acknowledgement RFC 5961 answers a guess with -- a reset or a
    /// SYN in the window, an acknowledgement of nothing sent -- and RFC 9293
    /// a segment from outside the window: it says where this end is, which
    /// is news to a guess and a prompt to the real peer. At most one a half
    /// second, as Linux limits these (tcp_invalid_ratelimit): two ends that
    /// each find the other's segments outside their window -- both windows
    /// shut, data outstanding both ways -- would otherwise answer each
    /// other's answers for as long as they last.
    fn challenge(&'static self, inner: &mut Inner, now: u64) {
        if now >= inner.oow_ack_at.saturating_add(OOW_ACK_GAP_MS) {
            inner.oow_ack_at = now;
            self.send_ack(inner);
        }
    }

    /// What a timeout sends again: everything unacknowledged, from SND.UNA
    /// -- the send buffer holds exactly that -- in segments of the peer's
    /// MSS, as far as its window reaches, the first whatever the window, as
    /// the probe it then is; at most `RESEND_SEGMENTS` of them; then the FIN,
    /// when `fin` says one is out and all before it went. One segment a
    /// timeout, as it was, gave back a window lost whole -- a NIC's ring
    /// stalled under it -- a second a segment: two hours and more for 8 KiB
    /// at the MSS of one byte a peer may ask for. True if anything went.
    fn resend(&'static self, inner: &mut Inner, fin: bool) -> bool {
        let saved = inner.snd_nxt;
        let used = inner.send_buf.used();
        let mss = (inner.peer_mss as usize).clamp(1, OUR_MSS as usize);
        let window = inner.snd_wnd as usize;
        let mut segment = [0u8; OUR_MSS as usize];
        let mut offset = 0;
        let mut sent = 0;

        while offset < used && sent < RESEND_SEGMENTS && (sent == 0 || offset < window) {
            let len = (used - offset).min(mss);
            let got = inner.send_buf.peek(&mut segment[..len], offset);
            if got == 0 {
                break;
            }
            inner.snd_nxt = inner.snd_una.wrapping_add(offset as u32);
            self.send_segment(inner, seg::ACK_FLAG | seg::PSH, &segment[..got]);
            offset += got;
            sent += 1;
        }
        if fin && offset == used && sent < RESEND_SEGMENTS {
            inner.snd_nxt = inner.snd_una.wrapping_add(used as u32);
            self.send_segment(inner, seg::FIN | seg::ACK_FLAG, &[]);
            sent += 1;
        }

        inner.snd_nxt = saved;
        sent != 0
    }
}

/// The window to advertise in a segment that is no SYN, as its field says
/// it; and, kept, the right edge it offers. As much as the receive buffer
/// has free, in units of two to this end's shift and rounded down -- and
/// never short of the edge offered before: a window that shrinks takes back
/// room the peer was promised (RFC 9293 3.8.6.2.2), and a peer with data in
/// flight to it has it dropped. A scaled window keeps one unit of the buffer
/// spare, which is what makes the old edge, rounded up to a unit, always fit:
/// it was offered with a unit spare, and since then the window and the edge
/// have come down together, or the window has grown.
fn offer_window(inner: &mut Inner) -> u16 {
    let shift = inner.rcv_wscale;
    let unit = 1usize << shift;
    let free = inner.recv_buf.free();
    let room = if shift == 0 { free } else { free.saturating_sub(unit) };
    let mut units = (room >> shift).min(u16::MAX as usize);

    let owed = inner.rcv_adv.wrapping_sub(inner.rcv_nxt);
    if (owed as i32) > 0 {
        units = units.max((owed as usize).div_ceil(unit).min(u16::MAX as usize));
    }

    let window = (units << shift) as u32;
    let edge = inner.rcv_nxt.wrapping_add(window);
    if before(inner.rcv_adv, edge) {
        inner.rcv_adv = edge;
    }
    inner.advertised_wnd = window;
    units as u16
}

/// A stretch [start, end) of what arrived ahead of a hole, with the ones
/// kept already: folded into every one it overlaps or touches, or, with no
/// room left for another, not kept -- its bytes then lie in the buffer's
/// free space uncounted, and come again.
fn keep_ooo(inner: &mut Inner, start: u32, end: u32) {
    let (mut start, mut end) = (start, end);
    let mut i = 0;
    while i < inner.ooo_len {
        let (s, e) = inner.ooo[i];
        if before_eq(s, end) && before_eq(start, e) {
            if before(s, start) {
                start = s;
            }
            if before(end, e) {
                end = e;
            }
            inner.ooo_len -= 1;
            inner.ooo[i] = inner.ooo[inner.ooo_len];
        } else {
            i += 1;
        }
    }
    if inner.ooo_len < MAX_OOO {
        inner.ooo[inner.ooo_len] = (start, end);
        inner.ooo_len += 1;
    }
}

/// What was kept ahead of a hole and starts at RCV.NXT, or before it, now:
/// taken in as far as it goes -- its bytes are in the buffer already, where
/// the stream has them -- and anything RCV.NXT has passed forgotten.
fn take_ooo(inner: &mut Inner) {
    let mut i = 0;
    while i < inner.ooo_len {
        let (s, e) = inner.ooo[i];
        if before_eq(s, inner.rcv_nxt) {
            if before(inner.rcv_nxt, e) {
                /* Within the free space, where it was put: the buffer has
                 * room for all of it, and is not trusted to. */
                let len = (e.wrapping_sub(inner.rcv_nxt) as usize).min(inner.recv_buf.free());
                inner.recv_buf.commit(len);
                inner.rcv_nxt = inner.rcv_nxt.wrapping_add(len as u32);
            }
            inner.ooo_len -= 1;
            inner.ooo[i] = inner.ooo[inner.ooo_len];
            /* RCV.NXT moved: every stretch is looked at again */
            i = 0;
        } else {
            i += 1;
        }
    }
}

/// Whether some of a segment of `len` sequence numbers -- its data, and a
/// SYN and a FIN each one -- starting at `seq` lies in the receive window
/// (RFC 9293 3.10.7.4). A shut window takes only what is at its edge: for
/// the acknowledgement it carries, since there is no room for its data.
fn acceptable(inner: &Inner, seq: u32, len: u32) -> bool {
    let in_window = |s: u32| s.wrapping_sub(inner.rcv_nxt) < inner.rcv_wnd;
    if inner.rcv_wnd == 0 {
        return seq == inner.rcv_nxt;
    }
    if len == 0 {
        return in_window(seq);
    }
    in_window(seq) || in_window(seq.wrapping_add(len - 1))
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

        /* A reset is valid in every state but LISTEN -- and then only at
         * exactly RCV.NXT (RFC 5961 3.2). Anywhere else in the window it is
         * answered: a peer that really reset answers that with a reset at
         * the edge, and a host that only guessed the four addresses and a
         * number in the window learns nothing it can use. Taken anywhere in
         * the window, as it was, one guess in 8,192 of the sequence space
         * tore a bulk connection down blind. */
        if flags & seg::RST != 0 && inner.state != State::Listen {
            if inner.state == State::SynSent {
                /* Here it is only valid if it acknowledges our SYN */
                if flags & seg::ACK_FLAG == 0 || ack != inner.snd_nxt {
                    return Event::None;
                }
            } else if seq != inner.rcv_nxt {
                if seq.wrapping_sub(inner.rcv_nxt) < inner.rcv_wnd {
                    self.challenge(inner, now);
                }
                return Event::None;
            }

            inner.state = State::Closed;
            conn.conn_ready.store(true, Ordering::Release);
            conn.data_ready.store(true, Ordering::Release);
            return Event::Rst;
        }

        /* RFC 9293 3.10.7.4: in a synchronized state a segment counts only
         * when some of it is in the receive window. One outside it is an
         * old duplicate, or no part of the peer's stream at all: it is
         * answered with an ACK that says where this end is, and nothing in
         * it -- its acknowledgement, its window, its data -- is taken.
         * Without this a segment from anywhere in the sequence space set the
         * send window, and an off-path host with the four addresses could
         * shut a connection's window, or open it past what the peer can
         * take, and hold the peer's own updates off as older than its. The
         * ACK is also what a zero-window probe is sent to hear. */
        if matches!(inner.state, State::Established | State::FinWait1 | State::FinWait2
            | State::CloseWait | State::Closing | State::LastAck)
        {
            let seg_len = len + (flags & seg::SYN != 0) as u32 + (flags & seg::FIN != 0) as u32;
            if !acceptable(inner, seq, seg_len) {
                self.challenge(inner, now);
                return Event::None;
            }

            /* A SYN on a synchronized connection (RFC 5961 4.2): the peer
             * starting over, which answers this ACK with a reset at the
             * edge -- or a guess. Never a reason to drop the connection, as
             * a SYN in the window once was. */
            if flags & seg::SYN != 0 {
                self.challenge(inner, now);
                return Event::None;
            }

            /* An acknowledgement of nothing this end sent, or of what is
             * older than any window the peer has offered: no segment of the
             * peer's, and dropped whole, data and all (RFC 5961 5.2, RFC
             * 9293 3.10.7.4). The data of a guess that landed in the window
             * would otherwise be taken into the stream. */
            if flags & seg::ACK_FLAG != 0
                && (before(inner.snd_nxt, ack)
                    || before(ack, inner.snd_una.wrapping_sub(inner.max_snd_wnd)))
            {
                self.challenge(inner, now);
                return Event::None;
            }
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
                    inner.rcv_adv = inner.rcv_nxt;
                    inner.snd_una = ack;
                    /* A SYN's window is never scaled */
                    inner.snd_wnd = wnd as u32;
                    inner.max_snd_wnd = inner.snd_wnd;
                    inner.snd_wl1 = seq;
                    inner.snd_wl2 = ack;
                    inner.peer_mss = seg::parse_mss(segment, OUR_MSS, DEFAULT_MSS);
                    /* Scaled both ways from here when both SYNs said so
                     * (RFC 7323 2.2) -- and not at all when the peer's did
                     * not: the window this end offers stays within 16 bits,
                     * whatever its buffer. */
                    if let (Some(ours), Some(theirs)) =
                        (inner.wscale_offer, seg::parse_wscale(segment))
                    {
                        inner.rcv_wscale = ours;
                        inner.snd_wscale = theirs;
                    }
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
                    inner.snd_wnd = (wnd as u32) << inner.snd_wscale;
                    inner.max_snd_wnd = inner.max_snd_wnd.max(inner.snd_wnd);
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
    fn trace_event(&self, event: Event, local_port: u16, remote_port: u16, peer_mss: u16,
        receive: (usize, u8, u8))
    {
        let (recv_size, rcv_wscale, snd_wscale) = receive;
        match event {
            Event::Rst => trace!(0, "tcp: reset received, {} -> {}",
                local_port, remote_port),
            /* Whether the window is scaled is what says how fast it can go */
            Event::Connected if rcv_wscale != 0 => trace!(0,
                "tcp: connected {} -> {}, mss {}, receive {} KiB, window scale {}/{}",
                local_port, remote_port, peer_mss, recv_size / 1024, rcv_wscale, snd_wscale),
            Event::Connected => trace!(0, "tcp: connected {} -> {}, mss {}, receive {} KiB",
                local_port, remote_port, peer_mss, recv_size / 1024),
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

        /* Port 0 is nobody's (RFC 6335): no connection has it, and nothing
         * is answered to it. And TCP is between two hosts: a segment to a
         * broadcast address -- which the receive path lets through for UDP
         * -- is nobody's either (RFC 1122 4.2.3.10), and answered, it would
         * be answered from the broadcast address. */
        if local_port == 0 || remote_port == 0 || !crate::device::DEVICES.is_local(local_ip) {
            return;
        }

        let pool = self.pool.lock();

        /* The four addresses, exactly, on the device it came in by */
        if let Some(index) = pool.lookup(local_ip, local_port, remote_ip, remote_port, nic) {
            let conn = &self.conns[index];
            /* Locked before the pool is let go of: that is what stops the
             * slot changing hands in between. */
            let mut inner = conn.inner.lock();
            drop(pool);

            let event = self.handle(conn, &mut inner, segment, payload);
            let peer_mss = inner.peer_mss;
            let receive = (inner.recv_buf.size, inner.rcv_wscale, inner.snd_wscale);
            drop(inner);

            self.trace_event(event, local_port, remote_port, peer_mss, receive);
            return;
        }

        /* A SYN for something listening -- a SYN alone: in LISTEN an ACK
         * is answered with a reset, a RST ignored (RFC 9293 3.10.7.2), and
         * a SYN that carries either, or a FIN, is no request to connect. */
        if flags & (seg::SYN | seg::ACK_FLAG | seg::RST | seg::FIN) == seg::SYN
            && pool.find_listener(local_ip, local_port, nic).is_some()
        {
            let mut pool = pool;

            /* Dropped, as a full accept queue drops it: the peer tries again
             * and the pool keeps its slots. */
            if self.backlog_full(&pool, local_port) {
                return;
            }

            let index = match pool.alloc(Slot {
                used: true, listening: false,
                local_ip, local_port, remote_ip, remote_port,
                device: Some(*nic),
            }) {
                Some(index) => index,
                None => return,
            };

            let mut inner = self.take_fresh(index);
            drop(pool);

            inner.nic = Some(*nic);
            inner.local_ip = local_ip;
            inner.local_port = local_port;
            inner.remote_ip = remote_ip;
            inner.remote_port = remote_port;
            inner.state = State::SynReceived;
            inner.irs = seg::seq(segment);
            inner.rcv_nxt = inner.irs.wrapping_add(1);
            inner.rcv_adv = inner.rcv_nxt;
            inner.iss = initial_sequence();
            inner.snd_nxt = inner.iss;
            inner.snd_una = inner.iss;
            /* Unscaled: this end offers no window scale on a connection it
             * did not open, so neither end scales (RFC 7323 1.3) */
            inner.snd_wnd = seg::window(segment) as u32;
            inner.max_snd_wnd = inner.snd_wnd;
            inner.snd_wl1 = inner.irs;
            inner.snd_wl2 = 0;
            inner.peer_mss = seg::parse_mss(segment, OUR_MSS, DEFAULT_MSS);
            /* Where the frame came from: no resolution needed */
            inner.peer_mac = peer_mac;

            self.send_segment(&mut inner, seg::SYN | seg::ACK_FLAG, &[]);
            /* A SYN takes a sequence number */
            inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
            inner.retransmit_at = now_ms() + inner.rto_ms;

            self.conn_count.fetch_add(1, Ordering::Relaxed);
            return;
        }

        drop(pool);

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
    /// nobody has accepted. The caller holds the pool lock and no
    /// connection's: whether one is waiting to be accepted is its own state,
    /// read under its own lock, and the pool says which slots are worth
    /// asking.
    fn backlog_full(&'static self, pool: &Pool, port: u16) -> bool {
        let mut pending = 0;
        for i in 0..MAX_CONNECTIONS {
            let slot = &pool.slots[i];
            if !slot.used || slot.listening || slot.local_port != port {
                continue;
            }

            let inner = self.conns[i].inner.lock();
            if !inner.accepted && !inner.owned_by_app
                && matches!(inner.state,
                    State::SynReceived | State::Established | State::CloseWait)
            {
                pending += 1;
            }
        }
        pending >= LISTEN_BACKLOG
    }

    /// A destination unreachable quoting a segment this machine sent, come
    /// in by `nic`: an error from another device than the connection's is
    /// none of its business -- a guest behind the switch cannot abort a
    /// connection of nos's over eth0 with one.
    pub fn on_icmp_unreachable(&'static self, nic: &Nic, local_ip: u32, local_port: u16,
        remote_ip: u32, remote_port: u16, quoted_seq: u32)
    {
        if !self.initialized.load(Ordering::Acquire) {
            return;
        }

        let pool = self.pool.lock();
        let index = match pool.lookup(local_ip, local_port, remote_ip, remote_port, nic) {
            Some(index) => index,
            None => return,
        };
        let conn = &self.conns[index];
        let mut inner = conn.inner.lock();
        drop(pool);

        /* RFC 5927: honour the hard error only when the quoted sequence
         * number is one this machine sent and has not seen acknowledged --
         * which is what an off-path forgery cannot know. */
        if before(quoted_seq, inner.snd_una) || before(inner.snd_nxt, quoted_seq) {
            return;
        }

        inner.state = State::Closed;
        inner.retransmit_at = 0;
        inner.need_cleanup = true;
        conn.conn_ready.store(true, Ordering::Release);
        conn.data_ready.store(true, Ordering::Release);
        drop(inner);

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
    /// passes. `src_port` of 0 takes an ephemeral one; `receive` says how
    /// much it may take in before the caller reads.
    pub fn connect(&'static self, nic: &Nic, dst_ip: u32, dst_port: u16, src_port: u16,
        receive: Receive) -> Option<&'static Conn>
    {
        if !self.initialized.load(Ordering::Acquire) {
            return None;
        }

        /* A bulk buffer comes from the heap before any lock is taken -- and,
         * should the connect go no further, goes back after the last one is
         * let go of: it is the first thing here, so the last dropped. */
        let bulk = if receive == Receive::Bulk { BulkBuf::new() } else { None };

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

        let mut pool = self.pool.lock();

        let src_port = if src_port == 0 {
            let port = pool.alloc_ephemeral_port();
            if port == 0 {
                drop(pool);
                trace!(0, "tcp: no ephemeral port is free");
                return None;
            }
            port
        } else {
            /* Two connections with the same four addresses would take each
             * other's segments. */
            if pool.lookup(nic.ip(), src_port, dst_ip, dst_port, nic).is_some() {
                drop(pool);
                trace!(0, "tcp: port {} is already connected to that address", src_port);
                return None;
            }
            src_port
        };

        let local_ip = nic.ip();
        let index = match pool.alloc(Slot {
            used: true, listening: false,
            local_ip, local_port: src_port, remote_ip: dst_ip, remote_port: dst_port,
            device: Some(*nic),
        }) {
            Some(index) => index,
            None => {
                drop(pool);
                trace!(0, "tcp: no free connection");
                return None;
            }
        };

        let conn = &self.conns[index];
        let mut inner = self.take_fresh(index);
        drop(pool);

        inner.nic = Some(*nic);
        inner.local_ip = local_ip;
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
        /* Moved in, not allocated: the lock is held. Its SYN offers the
         * shift that says the whole of it. */
        if let Some(bulk) = bulk {
            inner.wscale_offer = Some(wscale_for(bulk.0.len()));
            inner.recv_buf.attach(bulk);
            inner.rcv_wnd = inner.recv_buf.free() as u32;
        }

        self.send_segment(&mut inner, seg::SYN, &[]);
        inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
        inner.retransmit_at = now_ms() + inner.rto_ms;
        drop(inner);
        self.conn_count.fetch_add(1, Ordering::Relaxed);

        /* Looked at once more after the last sleep, whatever the clock
         * says by then: an answer that came in during it is an answer. */
        let deadline = now_ms() + CONNECT_TIMEOUT_MS;
        let mut poll = Poll::new();
        loop {
            if conn.conn_ready.load(Ordering::Acquire) {
                let state = conn.inner.lock().state;

                /* Up -- and maybe closed by the peer already: a server
                 * that answers and closes at once has its answer and its
                 * FIN in before this task looks, and what it sent is
                 * still here to read. */
                if matches!(state, State::Established | State::CloseWait) {
                    return Some(conn);
                }
                /* Refused, or reset */
                self.close(conn);
                return None;
            }
            if now_ms() >= deadline {
                break;
            }
            poll.sleep();
        }

        trace!(0, "tcp: nothing answered {} -> {}", src_port, dst_port);
        self.close(conn);
        None
    }

    /// A passive open on `nic`: a SYN that comes in by it, for any address
    /// this machine has, and none that comes in by another device -- a
    /// guest behind the hypervisor's switch does not reach a service
    /// listening on eth0. A connection belongs to the device it arrives on.
    pub fn listen(&'static self, nic: &Nic, port: u16) -> Option<&'static Conn> {
        if !self.initialized.load(Ordering::Acquire) {
            return None;
        }

        let mut pool = self.pool.lock();

        /* A second listener on the same port would never be given anything */
        if pool.slots.iter().any(|slot| slot.used && slot.listening && slot.local_port == port) {
            drop(pool);
            trace!(0, "tcp: port {} is listened on already", port);
            return None;
        }

        /* The wildcard address, which a segment for any address matches:
         * bound to the device's address of the moment, the port would answer
         * nobody once a lease renewal moved it. */
        let index = pool.alloc(Slot {
            used: true, listening: true,
            local_ip: 0, local_port: port, remote_ip: 0, remote_port: 0,
            device: Some(*nic),
        })?;

        let mut inner = self.take_fresh(index);
        inner.nic = Some(*nic);
        inner.local_ip = 0;
        inner.local_port = port;
        inner.state = State::Listen;
        inner.owned_by_app = true;
        Some(&self.conns[index])
    }

    /// The next connection on a listener's port. None once the timeout
    /// passes with none, or once the listener is closed -- which is how a
    /// server's task waiting here sees a close from another.
    pub fn accept(&'static self, listener: &'static Conn, timeout_ms: u64)
        -> Option<&'static Conn>
    {
        /* The port, taken once: a close from another task frees the slot,
         * and a later listen may take it for another port. */
        let listener_index = self.index_of(listener)?;
        let port = {
            let pool = self.pool.lock();
            let slot = &pool.slots[listener_index];
            if !slot.used || !slot.listening {
                return None;
            }
            slot.local_port
        };

        let deadline = if timeout_ms != 0 { now_ms() + timeout_ms } else { 0 };
        let mut poll = Poll::new();

        loop {
            let pool = self.pool.lock();

            let slot = &pool.slots[listener_index];
            if !slot.used || !slot.listening || slot.local_port != port {
                return None;
            }

            for i in 0..MAX_CONNECTIONS {
                /* The pool says which slots arrived on this port; whether one
                 * is waiting to be accepted is its own to say. Never one of
                 * this machine's own connections that happens to use the
                 * port: those are owned from the start. */
                let slot = &pool.slots[i];
                if !slot.used || slot.listening || slot.local_port != port {
                    continue;
                }

                let mut inner = self.conns[i].inner.lock();
                /* One whose peer has closed already goes too: its reader
                 * sees the end of the stream and closes it, and nothing else
                 * would -- a CLOSE-WAIT nobody owns keeps its slot for good. */
                if !inner.accepted && !inner.owned_by_app
                    && matches!(inner.state, State::Established | State::CloseWait)
                {
                    inner.accepted = true;
                    inner.owned_by_app = true;
                    return Some(&self.conns[i]);
                }
            }
            drop(pool);

            if deadline != 0 && now_ms() >= deadline {
                return None;
            }
            poll.sleep();
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

        let mut poll = Poll::new();
        while sent < data.len() {
            let mut inner = conn.inner.lock();

            if !matches!(inner.state, State::Established | State::CloseWait) {
                return if sent > 0 { sent as isize } else { -1 };
            }

            let room = inner.send_buf.free();
            if room == 0 {
                drop(inner);
                if deadline != 0 && now_ms() >= deadline {
                    return sent as isize;
                }
                poll.sleep();
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
                drop(inner);
                if deadline != 0 && now_ms() >= deadline {
                    return sent as isize;
                }
                poll.sleep();
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

            self.send_segment(&mut inner, seg::ACK_FLAG | seg::PSH, &segment[..len]);
            inner.snd_nxt = inner.snd_nxt.wrapping_add(len as u32);

            if inner.retransmit_at == 0 {
                inner.retransmit_at = now_ms() + inner.rto_ms;
            }
            drop(inner);
            sent += chunk;
            /* Moving again: the next stall is waited for from the start */
            poll = Poll::new();
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
        let mut poll = Poll::new();

        loop {
            let mut inner = conn.inner.lock();

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
                    self.send_ack(&mut inner);
                }

                return got as isize;
            }

            let closing = matches!(inner.state, State::CloseWait | State::Closed
                | State::TimeWait | State::LastAck | State::Closing);
            drop(inner);

            if closing {
                return 0;
            }
            if deadline != 0 && now_ms() > deadline {
                return RECV_TIMEOUT;
            }
            poll.sleep();
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
        let index = match self.index_of(conn) {
            Some(index) => index,
            None => return false,
        };

        let mut pool = self.pool.lock();
        if !pool.slots[index].used || !pool.slots[index].listening {
            return false;
        }
        let port = pool.slots[index].local_port;

        {
            let mut inner = conn.inner.lock();
            inner.state = State::Free;
            inner.owned_by_app = false;
        }
        pool.free(index);

        self.reset_unaccepted(pool, port);
        true
    }

    /// Every connection on the port that nobody accepted is reset. Takes the
    /// pool's guard and lets go of it: the resets go out once the lock is
    /// down.
    fn reset_unaccepted(&'static self, pool: PreemptSpinGuard<'static, Pool>, port: u16) {
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
            let slot = &pool.slots[i];
            if !slot.used || slot.listening || slot.local_port != port {
                continue;
            }

            let conn = &self.conns[i];
            let mut inner = conn.inner.lock();
            if !inner.accepted && !inner.owned_by_app
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
        }
        drop(pool);

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

        let mut inner = conn.inner.lock();

        match inner.state {
            State::Established | State::SynReceived => {
                inner.state = State::FinWait1;
                inner.fin_acked = false;
                self.send_segment(&mut inner, seg::FIN | seg::ACK_FLAG, &[]);
                inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                inner.retransmit_at = now_ms() + inner.rto_ms;
            }
            State::CloseWait => {
                inner.state = State::LastAck;
                self.send_segment(&mut inner, seg::FIN | seg::ACK_FLAG, &[]);
                inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                inner.retransmit_at = now_ms() + inner.rto_ms;
            }
            State::SynSent => inner.state = State::Closed,
            /* A listener was dealt with above, under the pool lock -- which
             * is the only place a slot may be given back. */
            _ => {}
        }

        /* The application is done with this pointer. From here the cleanup
         * timer owns the slot and may recycle it once the connection is
         * closed. */
        inner.owned_by_app = false;
    }

    /// The abrupt close: a reset instead of the exchange, and the slot back
    /// at the next tick rather than after a minute of TIME-WAIT. For a
    /// connection a server refuses or drops.
    pub fn abort(&'static self, conn: &'static Conn) {
        /* A listener has nothing else to reset, so its abort is its close */
        if self.close_listener(conn) {
            return;
        }

        let mut inner = conn.inner.lock();

        match inner.state {
            State::SynReceived | State::Established | State::CloseWait
            | State::FinWait1 | State::FinWait2 | State::Closing | State::LastAck => {
                self.send_segment(&mut inner, seg::RST | seg::ACK_FLAG, &[]);
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
    }
}

/* ---- the timer ---- */

impl Tcp {
    /// Every two hundred milliseconds, from the soft IRQ the tick raises.
    pub fn process_retransmits(&'static self) {
        let now = now_ms();
        let mut any_cleanup = false;
        /* Bulk buffers given back: taken out under the locks, dropped once
         * none is held */
        let mut released: [Option<BulkBuf>; MAX_CONNECTIONS] = [const { None }; MAX_CONNECTIONS];

        /* What each connection owes, with only its own lock */
        for i in 0..MAX_CONNECTIONS {
            let conn = &self.conns[i];
            let mut inner = conn.inner.lock();

            if inner.state == State::Free {
                continue;
            }

            /* A bulk buffer goes back once nothing will be read from it, nor
             * taken into it, again: its program has let the connection go,
             * and the connection takes no data in -- not for the minute of
             * TIME-WAIT either. */
            if !inner.owned_by_app && matches!(inner.state, State::TimeWait | State::Closed) {
                if let Some(bulk) = inner.recv_buf.detach() {
                    inner.rcv_wnd = inner.recv_buf.free() as u32;
                    released[i] = Some(bulk);
                }
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
                continue;
            }

            if inner.state == State::Closed {
                inner.need_cleanup = true;
                any_cleanup = true;
                continue;
            }

            if inner.retransmit_at != 0 && now >= inner.retransmit_at {
                let mut retransmitted = false;

                match inner.state {
                    /* A handshake retransmit is about the state, not about
                     * buffered data */
                    State::SynSent => {
                        inner.snd_nxt = inner.snd_una;
                        self.send_segment(&mut inner, seg::SYN, &[]);
                        inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                        retransmitted = true;
                    }
                    State::SynReceived => {
                        inner.snd_nxt = inner.snd_una;
                        self.send_segment(&mut inner, seg::SYN | seg::ACK_FLAG, &[]);
                        inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                        retransmitted = true;
                    }
                    /* Whatever data is unacknowledged, and then our FIN */
                    State::FinWait1 | State::LastAck | State::Closing => {
                        retransmitted = self.resend(&mut inner, true);
                    }
                    _ => {
                        if inner.snd_una != inner.snd_nxt && inner.send_buf.used() > 0 {
                            retransmitted = self.resend(&mut inner, false);
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
                        /* The line goes out with the lock down: a trace is
                         * synchronous output. */
                        drop(inner);

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
                    self.send_segment(&mut inner, seg::ACK_FLAG, &[]);
                    inner.snd_nxt = saved;

                    inner.rto_ms = (inner.rto_ms * 2).min(MAX_RTO_MS);
                    inner.persist_at = now + inner.rto_ms;
                }
            } else {
                inner.persist_at = 0;
            }
        }

        /* And the slots that are done with, under the pool lock: giving a
         * slot back changes whose it is, and that is the pool's to say. */
        if any_cleanup {
            let mut pool = self.pool.lock();
            for i in 0..MAX_CONNECTIONS {
                let mut inner = self.conns[i].inner.lock();

                if inner.need_cleanup && inner.state == State::Closed
                    && !inner.owned_by_app
                {
                    /* Closed by its program since the walk above passed it:
                     * its bulk buffer goes back now, never with the slot. */
                    if let Some(bulk) = inner.recv_buf.detach() {
                        assert!(released[i].is_none(), "tcp: a slot's bulk buffer given back twice");
                        released[i] = Some(bulk);
                    }
                    inner.state = State::Free;
                    inner.need_cleanup = false;
                    pool.free(i);
                    self.conn_count.fetch_sub(1, Ordering::Relaxed);
                } else if inner.state != State::Closed {
                    inner.need_cleanup = false;
                }
                /* One still held by an application keeps the flag, so the
                 * next tick tries again once it has closed it. */
            }
        }

        /* No lock is held from here: the heap may have them back. */
        drop(released);
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

        let inner = self.conns[index].inner.lock();
        if inner.state == State::Free {
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
                recv_size: inner.recv_buf.size,
                window: inner.advertised_wnd as usize,
                /* This end's shift is never 0 on a connection that scales */
                scales: if inner.rcv_wscale != 0 {
                    Some((inner.rcv_wscale, inner.snd_wscale))
                } else {
                    None
                },
            })
        }
    }

    /// Who a connection is with, for a server that logs it.
    pub fn peer(&'static self, conn: &'static Conn) -> (u32, u16) {
        let inner = conn.inner.lock();
        (inner.remote_ip, inner.remote_port)
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
    /// The receive buffer: the slot's own, or a bulk connection's
    pub recv_size: usize,
    /// The window last offered, in bytes
    pub window: usize,
    /// This end's shift and the peer's, on a connection that scales
    pub scales: Option<(u8, u8)>,
}

impl Tcp {
    /// The periodic tick, from IPI context: it only raises the soft IRQ.
    fn on_tick(&'static self) {
        kcore::softirq::raise(kcore::softirq::TYPE_TCP_TIMER);
    }

    /// The soft IRQ the tick raised.
    fn on_softirq(&'static self) {
        self.process_retransmits();
    }
}
