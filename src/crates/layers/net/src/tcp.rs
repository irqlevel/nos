//! TCP: the connections, the state machine and the timers behind them.
//!
//! One pool of 64 connections, a static rather than an allocation, with a
//! lock per connection and one over the pool. A segment arrives on the
//! receive path, is matched to a connection by its four addresses, and the
//! state machine runs under that connection's lock alone; the blocking calls
//! -- connect, accept, send, receive -- sleep in a task and look again.
//!
//! A connection buffers 8 KiB each way in its slot: enough for a shell, or a
//! request and its answer. A transfer moves a window a round trip, and 8 KiB
//! a round trip is 130 KiB/s at 62 ms -- what `hv forward` carried between
//! the EX44 and a browser. One this machine opens to fetch in bulk asks for
//! more (`Receive::Bulk`) -- a receive buffer of up to 512 KiB from the heap,
//! and its window scaled to match (RFC 7323) when the peer agrees -- and one
//! that sends as much as it takes in, a cluster's link between hosts, asks
//! for that much each way (`Receive::Both`); a listener
//! that relays in bulk (`Buffers::Bulk`) has every connection it accepts
//! given bulk buffers both ways, as `accept` hands it out, its window scaled
//! from the SYN-ACK on when the peer's SYN offers to.
//!
//! With a window of hundreds of kilobytes in flight the sender has to stop
//! short of what the path takes, and find a loss quicker than its timer
//! does: congestion control (RFC 5681) -- ten segments to start with (RFC
//! 6928), slow start and congestion avoidance. When both SYNs say SACK (RFC
//! 2018), as every stack of today's does:
//!
//! - the receiver says, in every acknowledgement, what it holds past a
//!   hole, newest first, and what came twice (D-SACK, RFC 2883);
//! - the sender keeps each segment in flight in a table (`Sent`) -- held by
//!   the peer, lost, sent again, and when it last went -- and finds a loss
//!   by time rather than by count (RACK, RFC 8985): a segment sent before
//!   one the peer is known to have had, and longer ago than a round trip
//!   and a reordering window. The window is none until the path is seen to
//!   reorder, then a quarter of the least round trip, a quarter more for
//!   every round in which a D-SACK said a copy was not needed;
//! - what is found lost starts a recovery that PRR paces (RFC 6937): every
//!   hole sent again in the round trip it is found in -- NewReno filled one
//!   a round trip -- and the window brought down to half over that round
//!   trip rather than at once; a recovery the peer says every copy of came
//!   twice was for nothing, and the window goes back (RFC 3708);
//! - two round trips with no acknowledgement send the newest segment again,
//!   a loss probe (RFC 8985 7), whose acknowledgement says by SACK what is
//!   missing -- where only the timeout would have;
//! - a timeout counts lost what the peer does not hold and sends it again as
//!   slow start grows the window, never what the peer said it had;
//! - a peer that lets go of what it SACKed (RFC 2018 8) is found out when
//!   its acknowledgement stops short of it, and what it said before is not
//!   gone by.
//!
//! With a peer that does not take SACK the sender is NewReno (RFC 6582): a
//! lost segment sent again on the third duplicate acknowledgement, and the
//! losses after it in the same window one a partial acknowledgement; a
//! timeout sends one segment again, and the acknowledgements it brings
//! send the rest of what was outstanding as the window grows -- go-back-N,
//! ACK by ACK. A connection of 8 KiB never has more in flight than its first
//! window, so for it little but a timeout changes.
//!
//! The timeout is the round trip's, measured a segment at a time (RFC 6298,
//! Karn's rule: nothing timed across a retransmission) and no less than
//! 200 ms, where it was a second whatever the round trip: a peer may hold
//! its acknowledgement that long to send it with an answer, as Linux does.
//! A connection whose data or FIN goes unacknowledged for 100 s is given up,
//! however many tries that took; a handshake, after eight. The timer is a
//! task of its own that sleeps until the earliest retransmission any
//! connection has or the next 200 ms sweep, whichever comes first, so a
//! segment goes again at its timeout, not at the next of looks 200 ms
//! apart; the sweeps are for the rest of what a connection owes.
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
//!   the heap before `connect` takes one -- and before `accept` hands a bulk
//!   listener's connection out, once one is waiting -- and goes back once the
//!   connection is done with it: the timer takes it out under the locks and
//!   drops it after them.
//! - **The window offered never shrinks.** A scaled window is said in units
//!   of two to the shift, rounded down; one unit of the buffer is kept spare,
//!   so that the edge offered before can always be kept, rounded up, inside
//!   it (`offer_window`).
//! - **What arrives ahead of a hole is kept**, in the receive buffer where it
//!   belongs, as up to `MAX_OOO` stretches, and taken in whole when the hole
//!   fills. Dropped, one lost segment of a window of hundreds of kilobytes
//!   would cost everything sent after it.
//! - **The table of segments in flight covers exactly the send buffer**:
//!   SND.UNA to the end of the data sent, a segment for every send's, which
//!   is never cut otherwise -- a copy goes as its original went. A full table
//!   holds a send back as a full buffer does, so a peer whose MSS is a byte
//!   costs a table's worth of bytes in flight, not memory.
//! - **A send waiting for room sleeps on its connection's event**, which
//!   every segment the connection takes in signals: polled, as it was, a
//!   sender a loss had stopped stood still for as many round trips of a
//!   link's as the poll's 10 ms held.
//! - **A reset, a SYN or an acknowledgement a blind attacker could guess is
//!   answered, not obeyed** (RFC 5961): a RST counts only at exactly
//!   RCV.NXT, a SYN on a synchronized connection is answered with an ACK, and
//!   so is an acknowledgement of nothing sent, or of what is older than the
//!   peer's largest window. A window of 512 KiB is 64 times as easy to land
//!   a guess in as one of 8 KiB.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::nic::Nic;
use kcore::once::Once;
use kcore::sync::{PreemptSpinGuard, PreemptSpinLock};
use kcore::task::TaskHandle;
use kcore::klog;

use crate::abi;
use crate::wire::{self, eth, ip, tcp as seg, Mac, ETH_HDR_LEN, ETH_TYPE_IP,
                  IP_HDR_LEN, IP_PROTO_TCP};
use kcore::trace::{self, TcpAck, TcpLoss, TcpLossWhat, TcpRecv, TcpRecovery, TcpRtt, TcpSend, TcpVerdict, TcpWait,
                   TcpWaitFor, TcpWhy};

pub const MAX_CONNECTIONS: usize = 64;
/* A slot's number is a byte in its tracepoints' records */
const _: () = assert!(MAX_CONNECTIONS <= u8::MAX as usize + 1);
/// What a slot buffers each way, in the slot itself.
const RING_BUF: usize = 8192;
pub const SEND_BUF: usize = RING_BUF;
pub const RECV_BUF: usize = RING_BUF;

/// A bulk connection's receive buffer: the first of these the heap has --
/// the largest block the page allocator hands out (128 pages), then less.
const BULK_SIZES: [usize; 3] = [512 * 1024, 256 * 1024, 128 * 1024];
/// The most bulk buffers out at once, whoever asks -- a download's receive
/// buffer, both of a bulk listener's connection's: 16 MiB of heap.
const MAX_BULK: usize = 32;
/* Every one more than 16 bits say, so that its shift is never 0: a
 * connection scales exactly when this end's shift is not 0. */
const _: () = assert!(BULK_SIZES[BULK_SIZES.len() - 1] > u16::MAX as usize);

/// The stretches of what arrived ahead of a hole that a connection keeps.
/// Past that a segment ahead of one is dropped, as before there were any.
/// A path that reorders -- a segment overtaken by a dozen others, each
/// leaving a hole of its own -- has a dozen at once: eight dropped what a
/// jitter of 4 ms reordered at 10 MB/s, and every one dropped is a segment
/// sent twice.
const MAX_OOO: usize = 32;

/// The segments a connection keeps track of in flight (`Sent`): its own
/// 8 KiB's, in the slot -- an eighth of a KiB a segment, so that small
/// writes still fill the buffer -- and a bulk send buffer's, one for every
/// `BULK_SEG_BYTES` of it, from the heap with it. Past that a send waits,
/// as for room in the buffer: a peer whose MSS is a few bytes has as few
/// of them in flight.
const SMALL_SEGS: usize = 64;
const BULK_SEG_BYTES: usize = 512;

pub const DEFAULT_MSS: u16 = 536;
pub const OUR_MSS: u16 = 1460;

/// The congestion window a connection starts with (RFC 6928): ten segments,
/// within 14,600 bytes and two segments at least.
const IW_SEGMENTS: u32 = 10;
const IW_BYTES: u32 = 14_600;
/// The duplicate acknowledgements that say a segment was lost (RFC 5681
/// 3.2), rather than overtaken -- and the segments held past a hole that
/// say the same to RACK, before the path has been seen to reorder.
const DUP_ACKS: u32 = 3;
/// How long the least round trip RACK's reordering window is counted in
/// stands, before a newer one takes its place however long it is: a path's
/// round trip can grow for good.
const MIN_RTT_WINDOW_MS: u64 = 30_000;
/// RACK's reordering window at its widest, in quarters of the least round
/// trip -- the smoothed round trip caps it before that -- and the
/// recoveries it is kept for after a D-SACK last widened it (RFC 8985).
const REO_MULT_MAX: u32 = 255;
const REO_PERSIST: u32 = 16;
/// A loss probe's least wait, and what it waits more for a lone segment: a
/// peer's longest delayed acknowledgement (RFC 8985 7.2's WCDelAckT).
const PTO_MIN_MS: u64 = 2;
const WC_DELACK_MS: u64 = 200;
/// What the congestion window grows to at most: far past any buffer, short
/// of where adding to it could wrap.
const CWND_MAX: u32 = 1 << 30;
/// The retransmission timeout before the round trip has been measured
/// (RFC 6298 2.1).
const INITIAL_RTO_MS: u64 = 1000;
pub const MAX_RTO_MS: u64 = 8000;
/// The least a retransmission timeout is once the round trip has been
/// measured. RFC 6298 2.4 says a second; this is Linux's floor
/// (TCP_RTO_MIN), and for the same reason: a peer that delays its
/// acknowledgements holds the one for a lone segment -- the last of a
/// request, a line of an ssh session -- 40 ms and up to 200 (Linux's
/// delayed ACK), hoping to send it with an answer. A floor under that sends
/// every such segment again, and Karn's rule keeps the delay out of the
/// estimate, so it would go on doing so: here, against the Linux guests a
/// forward relays to over the switch, a fraction of a millisecond away.
/// nos itself acknowledges every segment at once, so a link both of whose
/// ends are nos may have less (plans/tcp.md, T2).
pub const MIN_RTO_MS: u64 = 200;
/// The clock's granularity in a timeout computed (RFC 6298 2.3's G): its
/// deadlines are kept in milliseconds.
const RTO_GRANULARITY_US: u64 = 1000;
/// A handshake given up after this many retransmits.
const MAX_RETRANSMITS: u32 = 8;
/// Data or a FIN unacknowledged this long, and the connection is given up
/// (RFC 1122 4.2.3.5's R2: at least 100 s). By time, not by count: from the
/// floor, eight doublings are over in half a minute, which would cut an ssh
/// session off a flaky link.
const GIVE_UP_MS: u64 = 100_000;
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
/// How often the timer's task runs the pass whether or not a retransmission
/// is due: for TIME-WAIT and FIN-WAIT-2 running out, the persist timer and
/// slots given back, none of which wants it sooner.
pub const SWEEP_MS: u64 = 200;
/// How long a connection half open -- a SYN answered, its ACK not come -- is
/// kept: a SYN-ACK and its one retransmission, at the timeout's first
/// second. Where it was until the eighth retransmission, some 47 s, every
/// SYN from an address that is nobody's held a place in its listener's
/// backlog that long (plans/tcp.md, T3).
pub const HALF_OPEN_MS: u64 = 3000;
/// The least floor a connection may be given of its own (`set_rto_floor`):
/// a link both of whose ends are nos, which acknowledges every segment at
/// once, so no delayed acknowledgement waits under it (plans/tcp.md, T2).
pub const LINK_MIN_RTO_MS: u64 = 20;
/// The longest a receive sleeps on its event before it looks again, a
/// signal or not: what a signal missed would cost at most.
const RX_LOOK_MS: u64 = 100;
/// The same for a send waiting for room: in the send buffer, in the windows
const TX_LOOK_MS: u64 = 100;
/// The most networks a listener answers SYNs from when it is told which
/// (`listen_allowed`): a cluster's hosts each by its address, with room.
pub const ALLOW_MAX: usize = 64;
/// Resets to segments for no connection, held to RST_RATE a second with
/// bursts of RST_BURST, over the whole stack (FreeBSD's `icmplim` is 200):
/// a flood at closed ports, its source addresses forged, does not make this
/// host a sender of as many resets to them.
const RST_RATE: u64 = 200;
pub const RST_BURST: u64 = 200;
pub const RST_INTERVAL_US: u64 = 1_000_000 / RST_RATE;
/// How far ahead of now the bucket's next may be and still be sent: a burst
const RST_AHEAD_US: u64 = RST_INTERVAL_US * (RST_BURST - 1);
/// The least time between two ACKs for segments from outside the window --
/// and for the challenges RFC 5961 answers a guess with.
const OOW_ACK_GAP_MS: u64 = 500;
const DEFAULT_TTL: u8 = 64;

const HASH_SIZE: usize = 32;
pub(crate) const EPHEMERAL_BASE: u16 = 49152;
const EPHEMERAL_MAX: u16 = 65535;

/// Connections a listener holds that nobody has accepted yet -- handshakes
/// under way, and ones done and waiting -- at most, and by default. Past its
/// own a SYN to its port is dropped, as a full accept queue drops it: a
/// flood of SYNs to a listening port then costs the pool that many slots,
/// not all of them -- and a server that takes few at a time (`listen_with`)
/// asks for fewer, so that two ports flooded do not take half the pool.
pub const LISTEN_BACKLOG: usize = 16;
const _: () = assert!(LISTEN_BACKLOG <= u8::MAX as usize);

/// What `recv` answers below zero.
pub const RECV_ERROR: isize = -1;
pub const RECV_TIMEOUT: isize = -2;

const MAX_FRAME: usize = 1514;
/// A SACK option's head: two NOPs, its kind and its length.
const SACK_OPT_HEAD: usize = 4;

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

/// How much an active open buffers: what it may take in before its program
/// reads it, and what it may have sent and not yet had acknowledged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Receive {
    /// The slot's own 8 KiB each way, the window unscaled: what a
    /// connection that talks rather than fetches -- a shell, a request and
    /// its answer -- needs.
    Small,
    /// A receive buffer from the heap, as big as one comes (`BULK_SIZES`),
    /// and the window scaled to it when the peer agrees: a download, whose
    /// request fits the slot's own 8 KiB to send. The slot's own when the
    /// heap has none to give, or `MAX_BULK` are out already.
    Bulk,
    /// Bulk buffers both ways: a link that sends as much as it takes in --
    /// two hosts of a cluster, whose 8 KiB in flight would hold a 1 GbE
    /// link at a 0.3 ms round trip to some 27 MB/s. Each the slot's own
    /// when the heap has none to give, or `MAX_BULK` are out already.
    Both,
}

/// How many bulk buffers are out.
static BULK_OUT: AtomicUsize = AtomicUsize::new(0);

/// A bulk connection's buffer one way: counted while it lives, so that no
/// more than `MAX_BULK` are ever out, and dropped only where no lock is
/// held. A send buffer's comes with the table of the segments in flight
/// from it (`Sent`), which the connection takes while it has the buffer.
struct BulkBuf {
    bytes: Box<[u8]>,
    segs: Option<Box<[Seg]>>,
}

impl BulkBuf {
    /// The largest of `BULK_SIZES` the heap has -- none once `MAX_BULK` are
    /// out. Allocates: no lock may be held.
    fn new() -> Option<BulkBuf> {
        Self::make(false)
    }

    /// The same for sending: with its table of segments, one for every
    /// `BULK_SEG_BYTES` of it -- or none at all when the heap has not
    /// both.
    fn new_send() -> Option<BulkBuf> {
        Self::make(true)
    }

    fn make(send: bool) -> Option<BulkBuf> {
        /* Counted before it is taken: two connects at once cannot both have
         * the last one. */
        if BULK_OUT.fetch_add(1, Ordering::AcqRel) >= MAX_BULK {
            BULK_OUT.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        for size in BULK_SIZES {
            let mut bytes = Vec::new();
            if bytes.try_reserve_exact(size).is_err() {
                continue;
            }
            bytes.resize(size, 0);
            let segs = if send {
                let mut segs = Vec::new();
                if segs.try_reserve_exact(size / BULK_SEG_BYTES).is_err() {
                    continue;
                }
                segs.resize(size / BULK_SEG_BYTES, NO_SEG);
                Some(segs.into_boxed_slice())
            } else {
                None
            };
            return Some(BulkBuf { bytes: bytes.into_boxed_slice(), segs });
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

/// What every connection a listener accepts buffers each way.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Buffers {
    /// The slot's own 8 KiB each way, the window unscaled: a server that
    /// talks -- a shell, a request and its answer.
    Small,
    /// Bulk buffers both ways, given as `accept` hands the connection out,
    /// and the window scaled from the SYN-ACK on when the peer's SYN offers
    /// to: a relay of a forwarded port, carrying a download either way. The
    /// slot's own when the heap has none, or `MAX_BULK` are out already.
    Bulk,
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
            Some(bulk) => &bulk.bytes,
            None => &self.data,
        }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        match &mut self.bulk {
            Some(bulk) => &mut bulk.bytes,
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
        self.size = bulk.bytes.len();
        self.bulk = Some(bulk);
        self.reset();
    }

    /// A bulk buffer in place of the slot's own on a connection under way:
    /// what is buffered, and what lies past it in the free space -- what
    /// arrived ahead of a hole, where the stream has it -- moved across in
    /// the stream's order from the head on, so that each byte keeps its
    /// place from the head. The bulk buffer is the larger: all of it fits.
    fn grow(&mut self, mut bulk: BulkBuf) {
        assert!(self.bulk.is_none() && bulk.bytes.len() >= self.size, "tcp: a ring grown into less, or twice");
        let used = self.used();
        let size = self.size;
        let at = self.head % size;
        let first = size - at;
        bulk.bytes[..first].copy_from_slice(&self.data[at..]);
        bulk.bytes[first..size].copy_from_slice(&self.data[..at]);
        self.size = bulk.bytes.len();
        self.bulk = Some(bulk);
        self.head = 0;
        self.tail = used;
    }

    /// The bulk buffer back, if there is one, for the caller to drop once no
    /// lock is held -- and the slot's own in its place, empty.
    fn detach(&mut self) -> Option<BulkBuf> {
        self.size = RING_BUF;
        self.reset();
        self.bulk.take()
    }
}

/* ---- what is in flight, a segment at a time ---- */

/// What has become of a segment in flight, as far as the peer has said.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fate {
    /// Sent, and neither known to have arrived nor found lost: in flight
    Flying,
    /// Found lost -- by RACK, by a timeout -- and not sent again since: not
    /// in flight, and the next to go
    Lost,
    /// Found lost and sent again: the copy is in flight
    Resent,
    /// Held by the peer past a hole (SACK): not in flight, not to go again
    Sacked,
}

/// One segment in flight: from the end of the one before -- SND.UNA, for
/// the oldest -- to `end`.
#[derive(Clone, Copy)]
struct Seg {
    end: u32,
    fate: Fate,
    /// Sent more than once -- found lost, or a probe's: an acknowledgement
    /// of it could be any copy's (Karn), and a D-SACK of it says a copy was
    /// not needed
    copies: bool,
    /// When it last went (boot ns)
    sent_ns: u64,
}

const NO_SEG: Seg = Seg { end: 0, fate: Fate::Flying, copies: false, sent_ns: 0 };

/// The segments in flight, oldest first, in a ring: the slot's own
/// `SMALL_SEGS`, or a bulk send buffer's table while the connection has
/// one. What it says of each is all the sender knows of where its stream
/// stands: what the peer holds (`Sacked`), what is lost and waits to go
/// again, what is in flight. It covers exactly what the send buffer holds
/// -- SND.UNA to the end of the data sent; a FIN after the data is none of
/// its segments -- and keeps count of the bytes of the fates that are not
/// in flight, so that what is (`Inner::pipe`) is had without a walk.
struct Sent {
    small: [Seg; SMALL_SEGS],
    big: Option<Box<[Seg]>>,
    head: usize,
    len: usize,
    /// Bytes, and segments, the peer holds past a hole; bytes lost and not
    /// sent again
    sacked: u32,
    sacked_segs: u32,
    lost: u32,
}

impl Sent {
    const fn new() -> Sent {
        Sent { small: [NO_SEG; SMALL_SEGS], big: None, head: 0, len: 0, sacked: 0, sacked_segs: 0, lost: 0 }
    }

    fn slots(&self) -> &[Seg] {
        match &self.big {
            Some(big) => big,
            None => &self.small,
        }
    }

    fn slots_mut(&mut self) -> &mut [Seg] {
        match &mut self.big {
            Some(big) => big,
            None => &mut self.small,
        }
    }

    #[inline]
    fn cap(&self) -> usize {
        self.slots().len()
    }

    #[inline]
    fn is_full(&self) -> bool {
        self.len >= self.cap()
    }

    /// The i'th oldest, i below `len`.
    #[inline]
    fn at(&self, i: usize) -> Seg {
        let cap = self.cap();
        self.slots()[(self.head + i) % cap]
    }

    #[inline]
    fn at_mut(&mut self, i: usize) -> &mut Seg {
        let cap = self.cap();
        let at = (self.head + i) % cap;
        &mut self.slots_mut()[at]
    }

    /// Where the i'th oldest starts: the end of the one before, or `una`.
    #[inline]
    fn start(&self, i: usize, una: u32) -> u32 {
        if i == 0 { una } else { self.at(i - 1).end }
    }

    /// The i'th oldest's bytes not yet acknowledged.
    #[inline]
    fn len_of(&self, i: usize, una: u32) -> u32 {
        self.at(i).end.wrapping_sub(self.start(i, una))
    }

    /// A segment sent, from the end of the newest to `end`, at `now_ns`.
    /// The caller has looked: there is room.
    fn push(&mut self, end: u32, now_ns: u64) {
        assert!(!self.is_full(), "tcp: a segment sent past the table of those in flight");
        let cap = self.cap();
        let at = (self.head + self.len) % cap;
        self.slots_mut()[at] = Seg { end, fate: Fate::Flying, copies: false, sent_ns: now_ns };
        self.len += 1;
    }

    /// The i'th oldest's fate, its bytes from `una` on moved between the
    /// counts.
    fn set_fate(&mut self, i: usize, una: u32, fate: Fate) {
        let was = self.at(i).fate;
        if was == fate {
            return;
        }
        let len = self.len_of(i, una);
        match was {
            Fate::Sacked => {
                self.sacked -= len;
                self.sacked_segs -= 1;
            }
            Fate::Lost => self.lost -= len,
            _ => {}
        }
        match fate {
            Fate::Sacked => {
                self.sacked += len;
                self.sacked_segs += 1;
            }
            Fate::Lost => self.lost += len,
            _ => {}
        }
        self.at_mut(i).fate = fate;
    }

    /// The first segment that ends past `seq` -- the one `seq` lies in,
    /// when it is in flight -- or `len` when none does. The ends rise from
    /// the oldest on, within half the sequence space.
    fn index_after(&self, seq: u32) -> usize {
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if before(seq, self.at(mid).end) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    /// The oldest, acknowledged from `una` up to `ack`: what of it is,
    /// taken off -- the whole of it when `ack` is at its end or past it, the
    /// front of it, out of its fate's count, when `ack` falls inside it.
    /// What it was, and how many bytes the acknowledgement took off it --
    /// None when it took none.
    fn ack_oldest(&mut self, una: u32, ack: u32) -> Option<(Seg, u32)> {
        if self.len == 0 || !before(una, ack) {
            return None;
        }
        let seg = self.at(0);
        if before(ack, seg.end) {
            /* Inside it: the rest stays, its count cut by what went */
            let cut = ack.wrapping_sub(una);
            match seg.fate {
                Fate::Sacked => self.sacked -= cut,
                Fate::Lost => self.lost -= cut,
                _ => {}
            }
            return Some((seg, cut));
        }
        let len = seg.end.wrapping_sub(una);
        match seg.fate {
            Fate::Sacked => {
                self.sacked -= len;
                self.sacked_segs -= 1;
            }
            Fate::Lost => self.lost -= len,
            _ => {}
        }
        self.head = (self.head + 1) % self.cap();
        self.len -= 1;
        Some((seg, len))
    }

    /// Every segment the peer was said to hold back in flight: it let
    /// them go (reneged, RFC 2018 8).
    fn unsack_all(&mut self, una: u32) {
        for i in 0..self.len {
            if self.at(i).fate == Fate::Sacked {
                self.set_fate(i, una, Fate::Flying);
            }
        }
    }

    /// Everything not held by the peer counted lost: a timeout's word
    /// (RFC 6675 5.1).
    fn lose_all(&mut self, una: u32) {
        for i in 0..self.len {
            if matches!(self.at(i).fate, Fate::Flying | Fate::Resent) {
                self.set_fate(i, una, Fate::Lost);
            }
        }
    }

    /// Every segment counted lost and not sent again back in flight: the
    /// losses were not (an undo).
    fn unlose_all(&mut self, una: u32) {
        for i in 0..self.len {
            if self.at(i).fate == Fate::Lost {
                self.set_fate(i, una, Fate::Flying);
            }
        }
    }

    /// Nothing in flight: the slot's start, a connection's end.
    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.sacked = 0;
        self.sacked_segs = 0;
        self.lost = 0;
    }

    /// A bulk send buffer's table in place of the slot's own, with nothing
    /// in flight.
    fn attach(&mut self, big: Box<[Seg]>) {
        assert!(self.big.is_none() && self.len == 0 && !big.is_empty(), "tcp: a segment table onto one in use");
        self.big = Some(big);
        self.clear();
    }

    /// The bulk table back, for the caller to put with its buffer -- and
    /// the slot's own in its place, empty.
    fn detach(&mut self) -> Option<Box<[Seg]>> {
        self.clear();
        self.big.take()
    }
}

/// RACK's state (RFC 8985 6.1): the most recently sent segment the peer is
/// known to have had, which every segment sent before it is held to, and
/// what the reordering seen so far makes of the time it is given.
struct Rack {
    /// When it went (boot ns) and where it ends: 0 before any
    sent_ns: u64,
    end: u32,
    /// The round trip it took (us)
    rtt_us: u64,
    /// The furthest end the peer is known to have (RACK.fack)
    fack: u32,
    /// The least round trip seen (us), and when it was (boot ms):
    /// reordering is the path's, and its window is counted in quarters of
    /// this
    min_rtt_us: u64,
    min_rtt_ms: u64,
    /// A segment came in after one sent later than it, never sent again:
    /// the path reorders
    reordering: bool,
    /// The reordering window in quarters of the least round trip, grown by
    /// a round that saw a D-SACK; and the recoveries before it shrinks
    /// back to one
    reo_mult: u32,
    reo_persist: u32,
    /// The round a D-SACK last widened the window in: until SND.UNA passes
    /// this, another does not
    dsack_round: Option<u32>,
}

impl Rack {
    const fn new() -> Rack {
        Rack { sent_ns: 0, end: 0, rtt_us: 0, fack: 0, min_rtt_us: 0, min_rtt_ms: 0, reordering: false,
               reo_mult: 1, reo_persist: 0, dsack_round: None }
    }
}

/// A loss probe in flight (RFC 8985 7): SND.NXT when it went, and the end
/// of the segment it copied -- the newest in flight; this end never has new
/// data at hand to probe with, its program holding it until there is room.
#[derive(Clone, Copy)]
struct Probe {
    end: u32,
    seg_end: u32,
}

/// A recovery that may yet turn out to have been for nothing (RFC 3708,
/// D-SACK): where it began and what was outstanding then, the copies it
/// sent and how many of them the peer said came twice -- all of them, and
/// the window and threshold go back to what they were.
#[derive(Clone, Copy)]
struct Undo {
    from: u32,
    to: u32,
    copies: u32,
    twice: u32,
    cwnd: u32,
    ssthresh: u32,
    /// SND.NXT when the recovery ended: a copy sent since is not its own.
    /// D-SACKs of its copies count until the next recovery starts, as
    /// Linux keeps its undo_marker -- they come a round trip after the
    /// copies, which may be after the recovery is over -- or until every
    /// one would be too old to take (`take_dsack`)
    until: Option<u32>,
}

/// What the retransmission deadline is for when it comes before the
/// timeout: RACK's wait for a segment overtaken to turn out lost, or a
/// loss probe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Early {
    None,
    Reorder,
    Probe,
}

/* ---- a connection ---- */

/// What a loss has the sender doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Recovery {
    None,
    /// The third duplicate acknowledgement sent a segment again: until
    /// everything outstanding then is acknowledged (`recover`), each partial
    /// acknowledgement sends the next hole (RFC 6582).
    Fast,
    /// A timeout sent one segment again: everything outstanding then
    /// (`recover`) counts as lost, and goes again from `retx_nxt` as the
    /// window allows, sent from the acknowledgements that come in.
    Timeout,
}

/// A connection's retransmission deadline, in a module of its own so that
/// its one way to be set is `set`: which tells the timer (`Timer::note`),
/// so no deadline is set that the timer's task will not wake for in time.
/// `inner.retransmit_at = ...` does not compile.
mod deadline {
    use core::sync::atomic::{AtomicU64, Ordering};
    use kcore::once::Once;
    use kcore::sync::Event;

    /// What the timer's task sleeps to: the earliest retransmission
    /// deadline (boot ms), u64::MAX for none -- never later than the
    /// earliest a connection has, and earlier when one set since was
    /// acknowledged, which costs a pass that finds nothing due -- and the
    /// event that wakes the task when a deadline comes sooner than that.
    pub struct Timer {
        due: AtomicU64,
        wake: Once<Event>,
    }

    impl Timer {
        pub const fn new() -> Timer {
            Timer { due: AtomicU64::new(u64::MAX), wake: Once::new() }
        }

        /// The event made, before the task is started: false when it could
        /// not be.
        pub fn setup(&self) -> bool {
            if self.wake.get().is_none() {
                match Event::new() {
                    /* Another's made first is as good: this one is dropped */
                    Some(event) => drop(self.wake.set(event)),
                    None => return false,
                }
            }
            true
        }

        /// The task's to wait on; None before `setup`.
        pub fn event(&self) -> Option<&Event> {
            self.wake.get()
        }

        /// The earliest deadline set (boot ms).
        #[inline]
        pub fn due(&self) -> u64 {
            self.due.load(Ordering::SeqCst)
        }

        /// The pass's start: over from none. What its walk finds set, and
        /// what is set meanwhile (`note`), makes it again.
        pub fn restart(&self) {
            self.due.store(u64::MAX, Ordering::SeqCst);
        }

        /// A deadline `at`: the earliest brought forward to it, and the task
        /// woken, should it be sooner. Called with the connection's lock
        /// held, which is what makes this sound against the pass's
        /// `restart`: a deadline set while the pass walks is either one the
        /// walk finds under the same lock, or one set after the walk had the
        /// lock -- and so after the restart, which the load below then sees.
        /// A plain load first, since a deadline a timeout away is seldom the
        /// earliest: an acknowledgement that sets one costs no locked
        /// instruction on a line every CPU's acknowledgements share.
        #[inline]
        pub fn note(&self, at: u64) {
            if at < self.due.load(Ordering::SeqCst) && at < self.due.fetch_min(at, Ordering::SeqCst) {
                if let Some(wake) = self.wake.get() {
                    wake.signal();
                }
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct Deadline(u64);

    impl Deadline {
        pub const NONE: Deadline = Deadline(0);

        /// When, in boot ms; 0 for none.
        #[inline]
        pub fn at(self) -> u64 {
            self.0
        }

        #[inline]
        pub fn is_set(self) -> bool {
            self.0 != 0
        }

        /// Set, and come by `now`.
        #[inline]
        pub fn due(self, now: u64) -> bool {
            self.0 != 0 && now >= self.0
        }

        /// Set for `at`, and the timer told. Under the connection's lock.
        #[inline]
        pub fn set(&mut self, at: u64, timer: &Timer) {
            self.0 = at.max(1);
            timer.note(self.0);
        }

        #[inline]
        pub fn clear(&mut self) {
            self.0 = 0;
        }
    }
}

use deadline::{Deadline, Timer};

struct Inner {
    /// Which slot of the pool this is: what its tracepoints say it by
    slot: u8,
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

    /* Congestion control (RFC 5681, RFC 6582) */
    /// How much may be in flight, the peer's window aside
    cwnd: u32,
    /// Where slow start gives way to congestion avoidance
    ssthresh: u32,
    /// Duplicate acknowledgements in a row
    dup_acks: u32,
    recovery: Recovery,
    /// The highest sequence number sent when the recovery began: an
    /// acknowledgement of all of it ends the recovery, and only one past it
    /// starts another
    recover: u32,
    /// In a timeout's recovery, the next sequence number to send again
    retx_nxt: u32,
    /// A connection of a bulk listener's, which `accept` gives bulk buffers
    bulk_wanted: bool,

    /* Selective acknowledgements (RFC 2018, 2883) and what a sender makes
     * of them: RACK-TLP (RFC 8985) to find what is lost, PRR (RFC 6937) to
     * send in a recovery, D-SACK to take back one that was for nothing */
    /// Both SYNs said SACK: this end sends blocks of what it holds past a
    /// hole, and goes by the peer's
    sack_ok: bool,
    /// A duplicate's stretch, said once, first, in the next segment's SACK
    /// option (D-SACK, RFC 2883)
    dsack: Option<(u32, u32)>,
    /// The segments in flight
    sent: Sent,
    rack: Rack,
    probe: Option<Probe>,
    undo: Option<Undo>,
    /// PRR: what was in flight as the recovery began (RecoverFS), and the
    /// bytes delivered and sent since
    recover_fs: u32,
    prr_delivered: u32,
    prr_out: u32,
    /// The timeout's own deadline (boot ms, 0 for none), and an earlier
    /// one's -- RACK's, a probe's: `retransmit_at` is the sooner of them
    rto_at: u64,
    early: Early,
    early_at: u64,

    rto_ms: u64,
    /// The least its timeout is once the round trip is measured: MIN_RTO_MS,
    /// or less for a link whose other end acknowledges at once
    min_rto_ms: u64,
    /// When what is outstanding goes again
    retransmit_at: Deadline,
    /// Retransmits with no acknowledgement in between
    retransmit_count: u32,
    /// The smoothed round trip and its variation (RFC 6298 2), in
    /// microseconds: 0 before the first sample.
    srtt_us: u64,
    rttvar_us: u64,
    /// The segment being timed: the sequence number whose acknowledgement
    /// ends it, and when it went out (boot ns) -- 0 when none is. Karn's
    /// rule: nothing is timed across a retransmission, whose
    /// acknowledgement could be the original's or the copy's.
    rtt_seq: u32,
    rtt_sent_ns: u64,
    /// When the first retransmission since anything was last acknowledged
    /// went: what giving up counts from. 0 while nothing waits.
    stalled_since_ms: u64,
    /// When a connection a listener took was opened, its SYN come (boot
    /// ms): what giving it up half open counts from.
    opened_ms: u64,
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
            slot: 0,
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
            cwnd: IW_BYTES, ssthresh: u32::MAX, dup_acks: 0, recovery: Recovery::None, recover: 0, retx_nxt: 0,
            bulk_wanted: false,
            sack_ok: false, dsack: None, sent: Sent::new(), rack: Rack::new(), probe: None, undo: None,
            recover_fs: 0, prr_delivered: 0, prr_out: 0, rto_at: 0, early: Early::None, early_at: 0,
            rto_ms: INITIAL_RTO_MS,
            min_rto_ms: MIN_RTO_MS,
            retransmit_at: Deadline::NONE, retransmit_count: 0, srtt_us: 0, rttvar_us: 0, rtt_seq: 0, rtt_sent_ns: 0,
            stalled_since_ms: 0, opened_ms: 0, time_wait_at: 0, persist_at: 0, oow_ack_at: 0,
            need_cleanup: false, fin_acked: false, owned_by_app: false,
            accepted: false,
        }
    }

    /// What is outstanding to go again a timeout from `now`, the timer told.
    fn retransmit_in_rto(&mut self, now: u64, timer: &Timer) {
        self.rto_at = now + self.rto_ms;
        self.arm(timer);
    }

    /// RACK's wait or a probe, at `at` (boot ms) -- before the timeout,
    /// which stays as it was.
    fn arm_early(&mut self, early: Early, at: u64, timer: &Timer) {
        self.early = early;
        self.early_at = at;
        self.arm(timer);
    }

    fn clear_early(&mut self, timer: &Timer) {
        self.early = Early::None;
        self.early_at = 0;
        self.arm(timer);
    }

    /// The deadline the timer sees: the sooner of the timeout's and the
    /// early one's.
    fn arm(&mut self, timer: &Timer) {
        let early = if self.early != Early::None { self.early_at } else { 0 };
        let at = match (self.rto_at, early) {
            (0, 0) => 0,
            (0, e) => e,
            (r, 0) => r,
            (r, e) => r.min(e),
        };
        if at == 0 {
            self.retransmit_at.clear();
        } else {
            self.retransmit_at.set(at, timer);
        }
    }

    /// No retransmission deadline of any kind.
    fn disarm(&mut self) {
        self.rto_at = 0;
        self.early = Early::None;
        self.early_at = 0;
        self.retransmit_at.clear();
    }

    /// What is in flight against the congestion window (RFC 6675's pipe):
    /// the data sent and not acknowledged, less what the peer holds past a
    /// hole and what is lost and not yet sent again.
    #[inline]
    fn pipe(&self) -> u32 {
        (self.send_buf.used() as u32).saturating_sub(self.sent.sacked + self.sent.lost)
    }

    /// A bulk send buffer in place of the slot's own, and its table in
    /// place of the slot's: nothing is in flight.
    fn attach_send(&mut self, mut bulk: BulkBuf) {
        if let Some(segs) = bulk.segs.take() {
            self.sent.attach(segs);
        }
        self.send_buf.attach(bulk);
    }

    /// Whether either buffer is a bulk one, which goes back to the heap.
    fn has_bulk(&self) -> bool {
        self.recv_buf.bulk.is_some() || self.send_buf.bulk.is_some()
    }

    /// The bulk send buffer back with its table, if there is one, for the
    /// caller to drop once no lock is held.
    fn detach_send(&mut self) -> Option<BulkBuf> {
        let segs = self.sent.detach();
        let bulk = self.send_buf.detach();
        assert!(bulk.is_some() || segs.is_none(), "tcp: a segment table without its send buffer");
        bulk.map(|mut b| {
            b.segs = segs;
            b
        })
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
        assert!(self.recv_buf.bulk.is_none() && self.send_buf.bulk.is_none(),
                "tcp: a slot handed out again with its bulk buffer");
        self.send_buf.reset();
        self.recv_buf.reset();
        self.ooo_len = 0;
        assert!(self.sent.big.is_none(), "tcp: a slot handed out again with its segment table");
        self.sent.clear();
        self.sack_ok = false;
        self.dsack = None;
        self.rack = Rack::new();
        self.probe = None;
        self.undo = None;
        self.recover_fs = 0;
        self.prr_delivered = 0;
        self.prr_out = 0;
        self.cwnd = IW_BYTES;
        self.ssthresh = u32::MAX;
        self.dup_acks = 0;
        self.recovery = Recovery::None;
        self.recover = 0;
        self.retx_nxt = 0;
        self.bulk_wanted = false;
        self.rto_ms = INITIAL_RTO_MS;
        self.min_rto_ms = MIN_RTO_MS;
        self.disarm();
        self.retransmit_count = 0;
        self.srtt_us = 0;
        self.rttvar_us = 0;
        self.rtt_seq = 0;
        self.rtt_sent_ns = 0;
        self.stalled_since_ms = 0;
        self.opened_ms = 0;
        self.time_wait_at = 0;
        self.persist_at = 0;
        self.oow_ack_at = 0;
        self.need_cleanup = false;
        self.fin_acked = false;
        self.owned_by_app = false;
        self.accepted = false;
    }
}

/// One connection: what its own lock guards, and what a waiting task reads
/// without it.
pub struct Conn {
    inner: PreemptSpinLock<Inner>,
    /// Data has arrived; read without the lock by a waiting task
    data_ready: AtomicBool,
    /// The state left SYN-SENT or SYN-RECEIVED
    conn_ready: AtomicBool,
    /// What a task waiting in `recv` sleeps on: signalled each time data,
    /// an end or a reset comes (`ready`), and by a wake. Made at `init`; a
    /// receive before it -- or a second task receiving at once, an event
    /// having one waiter -- polls as receives did.
    rx: Once<kcore::sync::Event>,
    rx_waiting: AtomicBool,
    /// What a task waiting in `send` for room sleeps on: signalled by every
    /// segment the connection takes in -- an acknowledgement that frees the
    /// send buffer, opens a window or ends a recovery -- and as it ends.
    /// Where a send slept a millisecond and then up to ten between looks, a
    /// sender a loss had stopped stood still for as many round trips of a
    /// link's. Made at `init`, one waiter at a time, as `rx` is.
    tx: Once<kcore::sync::Event>,
    tx_waiting: AtomicBool,
    /// A wake asked for (`Tcp::wake`), under the lock: the receive under
    /// way, or the next, comes back as from a timeout.
    kicked: AtomicBool,
    /// Bumped, under the lock, each time the slot is handed out: what a wake
    /// is held to, so that one asked of a connection gone since reaches
    /// nobody -- least of all the slot's next connection.
    generation: AtomicU64,
}

impl Conn {
    const fn new() -> Conn {
        Conn {
            inner: PreemptSpinLock::new(Inner::new()),
            data_ready: AtomicBool::new(false),
            conn_ready: AtomicBool::new(false),
            rx: Once::new(),
            rx_waiting: AtomicBool::new(false),
            tx: Once::new(),
            tx_waiting: AtomicBool::new(false),
            kicked: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        }
    }

    /// Data, an end or a reset has come: said to a task waiting in `recv` --
    /// and to one waiting in `send`, which an end or a reset concerns too.
    #[inline]
    fn ready(&self) {
        self.data_ready.store(true, Ordering::Release);
        if let Some(e) = self.rx.get() {
            e.signal();
        }
        self.room();
    }

    /// Something that may have made room for a send: said to a task waiting
    /// in `send`. A signal is kept by the event until the waiter takes it,
    /// so one sent as the waiter looks, before it sleeps, is not lost --
    /// and costs it a look that finds nothing, at most, when it came for
    /// something it had seen already.
    #[inline]
    fn room(&self) {
        if let Some(e) = self.tx.get() {
            e.signal();
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
    /// A listener's: the connections it holds that nobody has accepted yet,
    /// at most, 1..=LISTEN_BACKLOG. 0 for a connection.
    backlog: u8,
    /// A listener's: what its connections buffer is `Buffers::Bulk`
    bulk: bool,
}

impl Slot {
    const FREE: Slot = Slot {
        used: false, listening: false,
        local_ip: 0, local_port: 0, remote_ip: 0, remote_port: 0,
        device: None, backlog: 0, bulk: false,
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
/// Who a listener answers SYNs from: anyone, or the networks it was given
/// -- each kept as its address and mask, in host order. A fixed array, so
/// that setting one and giving it back allocates and frees nothing under
/// the pool's lock.
#[derive(Clone, Copy)]
struct Allow {
    /// Whether `nets` is a list at all: a listener told nothing answers
    /// anyone
    only: bool,
    count: u8,
    nets: [(u32, u32); ALLOW_MAX],
}

impl Allow {
    const ANY: Allow = Allow { only: false, count: 0, nets: [(0, 0); ALLOW_MAX] };

    /// `nets` as (address, prefix length) pairs; None for more than
    /// ALLOW_MAX of them or a prefix past 32. An address's bits past its
    /// prefix are dropped: 10.1.2.3/8 is 10.0.0.0/8.
    fn of(nets: &[(u32, u8)]) -> Option<Allow> {
        if nets.len() > ALLOW_MAX {
            return None;
        }
        let mut allow = Allow { only: true, count: 0, nets: [(0, 0); ALLOW_MAX] };
        for (i, &(ip, prefix)) in nets.iter().enumerate() {
            if prefix > 32 {
                return None;
            }
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
            allow.nets[i] = (ip & mask, mask);
        }
        /* Fits a byte: ALLOW_MAX does */
        allow.count = nets.len() as u8;
        Some(allow)
    }

    fn admits(&self, ip: u32) -> bool {
        !self.only || self.nets[..usize::from(self.count)].iter().any(|&(net, mask)| ip & mask == net)
    }
}

const _: () = assert!(ALLOW_MAX <= u8::MAX as usize, "an allowlist's count is a byte");

struct Pool {
    slots: [Slot; MAX_CONNECTIONS],
    /// A listener's allowlist, by its slot: kept beside the slots rather
    /// than in them, so that the walks of the slots stay short
    allows: [Allow; MAX_CONNECTIONS],
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
            allows: [Allow::ANY; MAX_CONNECTIONS],
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
        self.allows[index] = Allow::ANY;
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
    /// Segments from an address their port's listener does not answer
    syn_refused: AtomicUsize,
    /// Connections given up half open
    half_open: AtomicUsize,
    /// Slots taken back from connections their programs had let go of, for
    /// a new connection with every slot taken (`Tcp::reclaim`)
    reclaimed: AtomicUsize,
    /// Resets for no connection not sent: the bucket was empty
    rst_held: AtomicUsize,
    /// The reset bucket (GCRA): when its next reset is due, boot us
    rst_due_us: AtomicU64,
    /* What the senders of connections that take SACK made of their losses */
    /// Recoveries RACK started; timeouts, and those found to have been for
    /// nothing (F-RTO); loss probes sent; recoveries taken back as for
    /// nothing (D-SACK); D-SACKs taken; peers found to have let go of what
    /// they had SACKed
    recoveries: AtomicUsize,
    timeouts: AtomicUsize,
    spurious: AtomicUsize,
    probes: AtomicUsize,
    undos: AtomicUsize,
    dsacks: AtomicUsize,
    reneged: AtomicUsize,
    /* What a receiver turned away that its peer will have to send again --
     * once without a word, and every byte of it a retransmission whose
     * cause nothing showed */
    /// Data segments from outside the window, old copies apart; segments
    /// whose acknowledgement was of nothing sent or older than the peer's
    /// largest window (RFC 5961 5.2); bytes in the window not kept -- no
    /// room in the buffer, or no stretch left to keep them past a hole in
    rx_outside: AtomicUsize,
    rx_badack: AtomicUsize,
    rx_unkept: AtomicUsize,
    /// Of those, bytes inside an edge offered that found no room: a promise
    /// broken, which `offer_window` rules out -- held to none by net-fuzz
    rx_overrun: AtomicUsize,

    /// What the timer's task sleeps to, and what wakes it
    timer: Timer,
    /// Taken by the one start that makes the task: an event has one waiter
    timer_started: AtomicBool,
    /// The task: taken out under the lock, and dropped -- which waits for
    /// it -- after
    timer_task: PreemptSpinLock<Option<TaskHandle>>,
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
    syn_refused: AtomicUsize::new(0),
    half_open: AtomicUsize::new(0),
    reclaimed: AtomicUsize::new(0),
    rst_held: AtomicUsize::new(0),
    rst_due_us: AtomicU64::new(0),
    recoveries: AtomicUsize::new(0),
    timeouts: AtomicUsize::new(0),
    spurious: AtomicUsize::new(0),
    probes: AtomicUsize::new(0),
    undos: AtomicUsize::new(0),
    dsacks: AtomicUsize::new(0),
    reneged: AtomicUsize::new(0),
    rx_outside: AtomicUsize::new(0),
    rx_badack: AtomicUsize::new(0),
    rx_unkept: AtomicUsize::new(0),
    rx_overrun: AtomicUsize::new(0),
    timer: Timer::new(),
    timer_started: AtomicBool::new(false),
    timer_task: PreemptSpinLock::new(None),
};

fn now_ms() -> u64 {
    kcore::time::boot_time().as_nanos() / kcore::consts::NS_PER_MS
}

fn now_ns() -> u64 {
    kcore::time::boot_time().as_nanos()
}

/// A round trip measured, `sample_us`, into the connection's estimate, and
/// the timeout it gives (RFC 6298 2.2 and 2.3).
fn rtt_sample(inner: &mut Inner, sample_us: u64) {
    let r = sample_us.max(1);
    if inner.srtt_us == 0 {
        inner.srtt_us = r;
        inner.rttvar_us = r / 2;
    } else {
        inner.rttvar_us = (3 * inner.rttvar_us + inner.srtt_us.abs_diff(r)) / 4;
        inner.srtt_us = (7 * inner.srtt_us + r) / 8;
    }
}

/// The retransmission timeout the estimate gives: SRTT + max(G, 4 RTTVAR),
/// held to the connection's floor (MIN_RTO_MS unless given its own) and
/// MAX_RTO_MS -- the initial second before the first sample.
fn rtt_rto(inner: &Inner) -> u64 {
    if inner.srtt_us == 0 {
        return INITIAL_RTO_MS;
    }
    let us = inner.srtt_us.saturating_add(RTO_GRANULARITY_US.max(inner.rttvar_us.saturating_mul(4)));
    us.div_ceil(1000).clamp(inner.min_rto_ms, MAX_RTO_MS)
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

/// `wait_room`, recorded (`tcp_wait`): what the send waited for, and how
/// long -- the call's own time, its pauses in the network's hands.
fn wait_for(conn: &Conn, poll: &mut Poll, deadline: u64, who: (u8, u16, u16), what: TcpWaitFor) {
    if !trace::on::<TcpWait>() {
        wait_room(conn, poll, deadline);
        return;
    }
    let from = now_ns();
    wait_room(conn, poll, deadline);
    trace_wait(who.0, who.1, who.2, what, now_ns().saturating_sub(from));
}

/// A send's wait for room: on the connection's event, which every segment
/// it takes in signals (`Conn::room`), for no longer than TX_LOOK_MS -- a
/// missed signal's whole cost -- nor past the call's deadline; polled, as
/// sends were, when the event was never made or another task's send has
/// it.
fn wait_room(conn: &Conn, poll: &mut Poll, deadline: u64) {
    let now = now_ms();
    let wait = if deadline != 0 { deadline.saturating_sub(now).max(1) } else { TX_LOOK_MS }.min(TX_LOOK_MS);
    match conn.tx.get() {
        Some(event) if !conn.tx_waiting.swap(true, Ordering::Acquire) => {
            event.wait_for(kcore::time::Duration::from_millis(wait));
            conn.tx_waiting.store(false, Ordering::Release);
        }
        _ => poll.sleep(),
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

        if !self.timer.setup() {
            klog!(0, "tcp: no event for the timer's task");
            return false;
        }
        /* Each slot's receive event and its send's: one that cannot be made
         * leaves its slot's receives, or sends, polling, as they did */
        for conn in self.conns.iter() {
            for event in [&conn.rx, &conn.tx] {
                if event.get().is_none() {
                    if let Some(made) = kcore::sync::Event::new() {
                        drop(event.set(made));
                    }
                }
            }
        }

        self.initialized.store(true, Ordering::Release);
        klog!(0, "tcp: initialized, up to {} connections", MAX_CONNECTIONS);
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
        inner.slot = index as u8;
        conn.data_ready.store(false, Ordering::Release);
        conn.conn_ready.store(false, Ordering::Release);
        /* A new connection: no wake asked of the last reaches it */
        conn.generation.fetch_add(1, Ordering::AcqRel);
        conn.kicked.store(false, Ordering::Release);
        inner
    }

    /// A slot for `slot`: a free one, or -- every one taken -- one taken back
    /// from a connection its program has let go of (`reclaim`).
    fn take_slot(&'static self, pool: &mut Pool, slot: Slot) -> Option<usize> {
        pool.alloc(slot).or_else(|| if self.reclaim(pool) { pool.alloc(slot) } else { None })
    }

    /// With every slot taken, take one back from the connections whose
    /// programs have let them go and that only wait out their close: one
    /// closed already and not yet swept, then the one in TIME-WAIT the
    /// longest, then FIN-WAIT-2, then the rest of a close nobody will read.
    /// Linux does the same past what it keeps of each (`tcp_max_tw_buckets`,
    /// `tcp_max_orphans`). Without it, anyone who could open and close
    /// connections to a port -- one a relay closes first, `hv forward`'s --
    /// or leave them closing held every slot the machine has for the minute
    /// of each TIME-WAIT, and nos's sshd and its API turned everyone away.
    ///
    /// One with nothing left to give back to the heap is freed here, and
    /// true. One that still has a bulk buffer, which is not freed with the
    /// pool's lock held, is closed for the sweep to free, and false: the new
    /// connection is refused this once -- a SYN dropped, which its peer
    /// sends again. The pool's lock is held, and each connection's is taken
    /// under it, in the order the cleanup takes them.
    fn reclaim(&'static self, pool: &mut Pool) -> bool {
        /* The rank of a connection let go of and closing, lower first */
        fn rank(state: State) -> Option<u8> {
            match state {
                State::Closed => Some(0),
                State::TimeWait => Some(1),
                State::FinWait2 => Some(2),
                State::FinWait1 | State::Closing | State::LastAck => Some(3),
                _ => None,
            }
        }
        let mut best: Option<(u8, u64, usize)> = None;
        for i in 0..MAX_CONNECTIONS {
            if !pool.slots[i].used || pool.slots[i].listening {
                continue;
            }
            let inner = self.conns[i].inner.lock();
            if inner.owned_by_app {
                continue;
            }
            let Some(r) = rank(inner.state) else { continue };
            let key = (r, inner.time_wait_at);
            if best.is_none_or(|(br, bt, _)| key < (br, bt)) {
                best = Some((r, inner.time_wait_at, i));
            }
        }
        let Some((_, _, i)) = best else { return false };
        let conn = &self.conns[i];
        let mut inner = conn.inner.lock();
        /* Under the pool's lock since the look: nothing has taken it */
        if inner.owned_by_app || rank(inner.state).is_none() {
            return false;
        }
        self.reclaimed.fetch_add(1, Ordering::Relaxed);
        if inner.has_bulk() {
            inner.state = State::Closed;
            inner.disarm();
            inner.need_cleanup = true;
            conn.conn_ready.store(true, Ordering::Release);
            /* The sweep at once, under the connection's lock as `note` asks */
            self.timer.note(now_ms());
            return false;
        }
        inner.state = State::Free;
        inner.disarm();
        inner.need_cleanup = false;
        drop(inner);
        pool.free(i);
        self.conn_count.fetch_sub(1, Ordering::Relaxed);
        true
    }
}

/* ---- segments out ---- */

impl Tcp {
    /// One segment of this connection. The caller holds its lock.
    ///
    /// A SYN carries the maximum segment size option, SACK permitted --
    /// always on this end's SYN, and on a SYN-ACK when the peer's SYN had
    /// it -- and the window scale when this end offers one: a header of up
    /// to 32 bytes. Any other segment of a connection that takes SACK
    /// carries the blocks of what came ahead of a hole, newest first, and a
    /// duplicate's before them (D-SACK) -- as many as the header and the
    /// frame have room for: none on a full data segment, which the
    /// acknowledgement every segment that comes is answered with carries
    /// instead.
    fn send_segment(&'static self, inner: &mut Inner, flags: u8, data: &[u8], why: Option<TcpWhy>) {
        let nic = match inner.nic {
            Some(nic) => nic,
            None => return,
        };

        let syn = flags & seg::SYN != 0;
        let wscale = if syn { inner.wscale_offer } else { None };
        let sack_perm = syn && (inner.state == State::SynSent || inner.sack_ok);
        let mut hdr_len = seg::HDR_LEN;
        if syn {
            hdr_len += seg::OPT_MSS_LEN as usize;
        }
        if sack_perm {
            /* Two NOPs before it, as Linux sends it without timestamps */
            hdr_len += 2 + seg::OPT_SACK_PERM_LEN as usize;
        }
        if wscale.is_some() {
            /* A NOP before it keeps the header a whole number of words */
            hdr_len += 1 + seg::OPT_WS_LEN as usize;
        }
        let mut sacks = [(0u32, 0u32); seg::MAX_SACK_BLOCKS];
        let mut blocks = 0;
        let mut dsacked = false;
        if !syn && flags & seg::RST == 0 && inner.sack_ok && (inner.dsack.is_some() || inner.ooo_len != 0) {
            let room = MAX_FRAME.saturating_sub(ETH_HDR_LEN + IP_HDR_LEN + seg::HDR_LEN + data.len());
            let fit = (room.saturating_sub(SACK_OPT_HEAD) / seg::SACK_BLOCK_LEN).min(seg::MAX_SACK_BLOCKS);
            if fit != 0 {
                if let Some(dup) = inner.dsack.take() {
                    sacks[0] = dup;
                    blocks = 1;
                    dsacked = true;
                }
                for &stretch in inner.ooo[..inner.ooo_len].iter().take(fit - blocks) {
                    sacks[blocks] = stretch;
                    blocks += 1;
                }
                hdr_len += SACK_OPT_HEAD + blocks * seg::SACK_BLOCK_LEN;
            }
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
            /* Before the peer's SYN there is no RCV.NXT to offer it past:
             * the SYN-ACK's arrival counts it (`handle`) */
            if inner.state != State::SynSent {
                let edge = inner.rcv_nxt.wrapping_add(window);
                if before(inner.rcv_adv, edge) {
                    inner.rcv_adv = edge;
                }
            }
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
            let mut opt = at + seg::HDR_LEN;
            frame[opt] = seg::OPT_MSS;
            frame[opt + 1] = seg::OPT_MSS_LEN;
            frame[opt + 2..opt + 4].copy_from_slice(&OUR_MSS.to_be_bytes());
            opt += 4;
            if sack_perm {
                frame[opt..opt + 4].copy_from_slice(&[seg::OPT_NOP, seg::OPT_NOP, seg::OPT_SACK_PERM,
                                                      seg::OPT_SACK_PERM_LEN]);
                opt += 4;
            }
            if let Some(shift) = wscale {
                frame[opt] = seg::OPT_NOP;
                frame[opt + 1] = seg::OPT_WS;
                frame[opt + 2] = seg::OPT_WS_LEN;
                frame[opt + 3] = shift;
            }
        } else if blocks != 0 {
            let opt = at + seg::HDR_LEN;
            frame[opt..opt + 4].copy_from_slice(&[seg::OPT_NOP, seg::OPT_NOP, seg::OPT_SACK,
                                                  (2 + blocks * seg::SACK_BLOCK_LEN) as u8]);
            for (k, &(left, right)) in sacks[..blocks].iter().enumerate() {
                let b = opt + SACK_OPT_HEAD + k * seg::SACK_BLOCK_LEN;
                frame[b..b + 4].copy_from_slice(&left.to_be_bytes());
                frame[b + 4..b + 8].copy_from_slice(&right.to_be_bytes());
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

        match why {
            Some(why) if trace::on::<TcpSend>() => trace::emit(&TcpSend {
                seq: inner.snd_nxt,
                ack: inner.rcv_nxt,
                cwnd: inner.cwnd,
                pipe: inner.pipe(),
                pid: trace::pid(),
                lport: inner.local_port,
                rport: inner.remote_port,
                len: data.len() as u16,
                wnd: window,
                slot: inner.slot,
                flags,
                why: why as u8,
                recovery: recovery_of(inner) as u8,
            }),
            None if trace::on::<TcpAck>() => trace::emit(&TcpAck {
                ack: inner.rcv_nxt,
                edge: inner.rcv_adv,
                free: inner.recv_buf.free() as u32,
                lport: inner.local_port,
                rport: inner.remote_port,
                wnd: window,
                slot: inner.slot,
                blocks: blocks as u8,
                dsack: u8::from(dsacked),
                ooo: inner.ooo_len as u8,
                _pad: [0; 2],
            }),
            _ => {}
        }
    }

    /// An acknowledgement of what has arrived, which consumes no sequence
    /// number of its own -- so `snd_nxt` goes back where it was.
    fn send_ack(&'static self, inner: &mut Inner) {
        let saved = inner.snd_nxt;
        self.send_segment(inner, seg::ACK_FLAG, &[], None);
        inner.snd_nxt = saved;
    }

    /// Whether a reset for no connection may go now -- taken from the
    /// bucket if so: a GCRA, one word for the whole stack, each reset
    /// moving the time the next is due on by RST_INTERVAL_US, and none sent
    /// while that is more than a burst ahead of now.
    fn reset_allowed(&'static self) -> bool {
        let now = kcore::time::boot_time_ns() / 1000;
        let mut due = self.rst_due_us.load(Ordering::Relaxed);
        loop {
            let from = due.max(now);
            if from - now > RST_AHEAD_US {
                self.rst_held.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            match self.rst_due_us.compare_exchange_weak(due, from + RST_INTERVAL_US,
                Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(seen) => due = seen,
            }
        }
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

    /// An acknowledgement: the window it advertises, the bytes it frees, and
    /// what it says of the path (congestion control). `pure` when the
    /// segment carries nothing else -- no data, no SYN, no FIN -- which a
    /// duplicate acknowledgement has to be.
    #[allow(clippy::too_many_arguments)]
    fn process_ack(&'static self, inner: &mut Inner, segment_seq: u32, ack: u32,
        wnd: u16, now: u64, pure: bool, sacks: &[(u32, u32)])
    {
        let window_was = inner.snd_wnd;
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

        if inner.sack_ok {
            self.sack_ack(inner, ack, sacks, now, pure);
            return;
        }

        /* snd_una < ack <= snd_nxt, wrap-safe */
        let advance = ack.wrapping_sub(inner.snd_una) as i32;
        if advance <= 0 || before(inner.snd_nxt, ack) {
            /* Nothing new acknowledged. A duplicate acknowledgement when it
             * is nothing else (RFC 5681 2): at SND.UNA, carrying nothing,
             * the window as it was, with data outstanding -- the peer
             * saying a segment came past a hole. */
            if advance == 0 && pure && inner.snd_wnd == window_was && inner.send_buf.used() > 0 {
                self.duplicate_ack(inner);
            }
            return;
        }

        /* The segments it covers off the table, which is kept here too:
         * nothing but SACK reads its fates */
        let mut una = inner.snd_una;
        while let Some((seg, _)) = inner.sent.ack_oldest(una, ack) {
            if before(ack, seg.end) {
                break;
            }
            una = seg.end;
        }
        let acked = self.take_ack(inner, ack, now);
        self.newly_acked(inner, acked);
    }

    /// What `ack` -- past SND.UNA, not past SND.NXT -- acknowledges taken
    /// off the send buffer, the segment timed, if it is one of them, a round
    /// trip (RFC 6298 3), the backoff over (5.7) and the timeout started
    /// again (5.3) -- or stopped, with nothing left. The bytes of data
    /// newly acknowledged: the send buffer holds data alone, and a FIN
    /// takes a sequence number that is not in it, so the drain is capped at
    /// what the buffer has.
    fn take_ack(&'static self, inner: &mut Inner, ack: u32, now: u64) -> u32 {
        let advance = ack.wrapping_sub(inner.snd_una) as usize;
        let acked = advance.min(inner.send_buf.used());
        inner.send_buf.consume(acked);

        inner.snd_una = ack;
        if inner.rtt_sent_ns != 0 && before_eq(inner.rtt_seq, ack) {
            let sample_us = now_ns().saturating_sub(inner.rtt_sent_ns) / 1000;
            rtt_sample(inner, sample_us);
            inner.rtt_sent_ns = 0;
            if trace::on::<TcpRtt>() {
                let us = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
                trace::emit(&TcpRtt {
                    sample_us: us(sample_us),
                    srtt_us: us(inner.srtt_us),
                    rttvar_us: us(inner.rttvar_us),
                    rto_ms: us(rtt_rto(inner)),
                    min_rtt_us: us(inner.rack.min_rtt_us),
                    lport: inner.local_port,
                    rport: inner.remote_port,
                    slot: inner.slot,
                    _pad: [0; 3],
                });
            }
        }
        inner.rto_ms = rtt_rto(inner);
        inner.retransmit_count = 0;
        inner.stalled_since_ms = 0;
        if inner.snd_una == inner.snd_nxt {
            inner.disarm();
        } else {
            inner.retransmit_in_rto(now, &self.timer);
        }
        acked as u32
    }

    /// An acknowledgement on a connection that takes SACK: what it says of
    /// each segment in flight -- acknowledged, held past a hole, received
    /// twice -- what RACK finds lost of what is left, the recovery that
    /// starts or ends, the copies PRR lets go, and the timers: RACK's
    /// wait, a probe, the timeout.
    fn sack_ack(&'static self, inner: &mut Inner, ack: u32, sacks: &[(u32, u32)], now: u64, pure: bool) {
        let now_ns = now_ns();
        let prior_fack = inner.rack.fack;
        let moved = before(inner.snd_una, ack) && before_eq(ack, inner.snd_nxt);
        let mut delivered = 0;
        let mut acked = 0;
        /* A segment sent before the recovery's start and never again, newly
         * delivered -- and no copy with it: in a timeout's recovery, what
         * says the timeout was for nothing. A copy delivered alongside is
         * what explains an acknowledgement that jumps over originals: the
         * copy filled a hole past which the peer held them, and said so to
         * nobody -- a peer that agreed to SACK and sends no blocks. Taken
         * for a pause, that undid every timeout's recovery, and the
         * connection crawled from one backed-off timeout to the next. */
        let mut original = false;
        let mut copy = false;

        if moved {
            /* The segments it covers off the table -- each whole one the
             * peer had not SACKed newly delivered -- then the bytes */
            let mut una = inner.snd_una;
            while let Some((seg, bytes)) = inner.sent.ack_oldest(una, ack) {
                if seg.fate != Fate::Sacked {
                    delivered += bytes;
                }
                /* Cut, not whole: RACK hears of a segment once all of it
                 * is in */
                if before(ack, seg.end) {
                    break;
                }
                if seg.fate != Fate::Sacked {
                    original |= !seg.copies && before_eq(seg.end, inner.recover);
                    copy |= seg.copies;
                    rack_delivered(&mut inner.rack, seg, now_ns, now, prior_fack);
                }
                una = seg.end;
            }
            acked = self.take_ack(inner, ack, now);
            if before(inner.rack.fack, inner.snd_una) {
                inner.rack.fack = inner.snd_una;
            }
        }

        /* The oldest in flight held, by the peer's word, past SND.UNA --
         * which the acknowledgement would have passed had the peer kept it:
         * it let go of what it had SACKed (RFC 2018 8). Nothing it said
         * before is to be gone by -- this acknowledgement's own blocks,
         * taken next, say what it holds now -- and RACK finds again what
         * is lost. */
        if inner.sent.len != 0 && inner.sent.at(0).fate == Fate::Sacked {
            inner.sent.unsack_all(inner.snd_una);
            self.reneged.fetch_add(1, Ordering::Relaxed);
            if trace::on::<TcpLoss>() {
                trace_loss(inner, TcpLossWhat::Reneged);
            }
        }

        /* The blocks: a D-SACK first, when the first is one -- under the
         * acknowledgement, or inside the second (RFC 2883 4) */
        let mut dsack = false;
        let mut probe_twice = false;
        for (k, &(left, right)) in sacks.iter().enumerate() {
            if k == 0 && (before_eq(right, ack) || sacks.get(1).is_some_and(|&(l, r)| {
                before_eq(l, left) && before_eq(right, r)
            })) {
                if let Some(of_probe) = self.take_dsack(inner, left, right) {
                    dsack = true;
                    probe_twice |= of_probe;
                }
                continue;
            }
            delivered += sack_mark(inner, left, right, now_ns, now, prior_fack, &mut original, &mut copy);
        }

        if inner.recovery == Recovery::Timeout && original && !copy {
            self.spurious_timeout(inner);
        }
        self.try_undo(inner);
        self.probe_acked(inner, ack, moved, probe_twice, pure && delivered == 0);

        /* Everything outstanding at the loss acknowledged: the recovery is
         * over; and one whose copies are all further back than the peer's
         * largest window waits for D-SACKs of them no longer -- none would
         * be taken, and the sequence space would come round under it */
        if inner.recovery != Recovery::None && !before(inner.snd_una, inner.recover) {
            self.end_recovery(inner);
        }
        if let Some(u) = inner.undo {
            if u.until.is_some() && before(u.to, inner.snd_una.wrapping_sub(inner.max_snd_wnd)) {
                inner.undo = None;
            }
        }

        rack_dsack_round(inner, dsack);
        let (newly_lost, wait) = rack_detect(inner, now_ns);
        if inner.recovery == Recovery::None && inner.sent.lost != 0 {
            self.enter_recovery(inner);
        }
        match inner.recovery {
            Recovery::Fast => prr(inner, delivered, moved, newly_lost),
            Recovery::Timeout | Recovery::None if acked != 0 => grow_cwnd(inner, acked),
            _ => {}
        }
        self.send_lost(inner, now_ns);
        self.set_early(inner, now, wait, moved);
    }

    /// A D-SACK block, [left, right): something the peer received twice --
    /// held to what was sent, and to no more than its largest window back.
    /// A copy a recovery sent counts against it, and the path is taken for
    /// one that reorders: the original came after all. Some, saying
    /// whether it was the loss probe's copy, when it is one; None when it
    /// is no block of this connection's.
    fn take_dsack(&'static self, inner: &mut Inner, left: u32, right: u32) -> Option<bool> {
        if !before(left, right) || before(inner.snd_nxt, right)
            || before(left, inner.snd_una.wrapping_sub(inner.max_snd_wnd))
        {
            return None;
        }
        self.dsacks.fetch_add(1, Ordering::Relaxed);
        let of_probe = inner.probe.is_some_and(|p| right == p.seg_end);
        if !of_probe {
            if let Some(u) = inner.undo.as_mut() {
                if !before(left, u.from) && before_eq(right, u.to) && u.twice < u.copies {
                    u.twice += 1;
                    inner.rack.reordering = true;
                }
            }
        }
        Some(of_probe)
    }

    /// A recovery every copy of which the peer said it received twice: it
    /// was for nothing -- the path reordered, or delayed -- and the window
    /// and the threshold go back to what they were before it (RFC 3708),
    /// what it counted lost back in flight.
    fn try_undo(&'static self, inner: &mut Inner) {
        let Some(u) = inner.undo else { return };
        if u.copies == 0 || u.twice < u.copies {
            return;
        }
        inner.cwnd = inner.cwnd.max(u.cwnd);
        inner.ssthresh = inner.ssthresh.max(u.ssthresh);
        if inner.recovery != Recovery::None {
            inner.recovery = Recovery::None;
            inner.sent.unlose_all(inner.snd_una);
        }
        inner.undo = None;
        self.undos.fetch_add(1, Ordering::Relaxed);
        if trace::on::<TcpLoss>() {
            trace_loss(inner, TcpLossWhat::Undo);
        }
    }

    /// A timeout that was for nothing (RFC 5682's F-RTO, SACK-enhanced, as
    /// Linux has it): an acknowledgement since says, newly, that the peer
    /// had a segment sent before the timeout and never sent again, and no
    /// copy -- not everything outstanding was lost, the path paused. The window and the
    /// threshold back to what they were, what the timeout counted lost back
    /// in flight -- RACK finds again what is -- and the recovery over, where
    /// it would have sent the whole window again as slow start let it.
    fn spurious_timeout(&'static self, inner: &mut Inner) {
        if let Some(u) = inner.undo.take() {
            inner.cwnd = inner.cwnd.max(u.cwnd);
            inner.ssthresh = inner.ssthresh.max(u.ssthresh);
        }
        inner.recovery = Recovery::None;
        inner.sent.unlose_all(inner.snd_una);
        self.spurious.fetch_add(1, Ordering::Relaxed);
        if trace::on::<TcpLoss>() {
            trace_loss(inner, TcpLossWhat::Spurious);
        }
    }

    /// A loss probe out, and an acknowledgement of everything it went
    /// after (RFC 8985 7.4): received twice -- the probe and what it copied
    /// -- or a duplicate acknowledgement that says nothing else, and
    /// nothing was lost; past it, and the probe repaired a loss -- the
    /// window halved, as for one.
    fn probe_acked(&'static self, inner: &mut Inner, ack: u32, moved: bool, twice: bool, bare_dup: bool) {
        let Some(p) = inner.probe else { return };
        if before(ack, p.end) {
            return;
        }
        if twice || (!moved && bare_dup) {
            inner.probe = None;
        } else if before(p.end, ack) {
            inner.probe = None;
            let mss = seg_size(inner);
            inner.ssthresh = (inner.cwnd / 2).max(2 * mss);
            inner.cwnd = inner.ssthresh;
            if trace::on::<TcpLoss>() {
                trace_loss(inner, TcpLossWhat::ProbeRepair);
            }
        }
    }

    /// A recovery from what RACK found lost (RFC 6675's, with PRR): the
    /// threshold half of what is in flight, and what it was and the window
    /// were kept for an undo; until everything outstanding now is
    /// acknowledged, PRR says what goes.
    fn enter_recovery(&'static self, inner: &mut Inner) {
        let mss = seg_size(inner);
        let flight = inner.snd_nxt.wrapping_sub(inner.snd_una);
        inner.undo = Some(Undo { from: inner.snd_una, to: inner.snd_nxt, copies: 0, twice: 0, cwnd: inner.cwnd,
                                 ssthresh: inner.ssthresh, until: None });
        inner.ssthresh = (flight / 2).max(2 * mss);
        inner.recover = inner.snd_nxt;
        inner.recovery = Recovery::Fast;
        inner.recover_fs = flight.max(1);
        inner.prr_delivered = 0;
        inner.prr_out = 0;
        inner.probe = None;
        self.recoveries.fetch_add(1, Ordering::Relaxed);
        if trace::on::<TcpLoss>() {
            trace_loss(inner, TcpLossWhat::Recovery);
        }
    }

    /// A recovery over, everything outstanding at its start acknowledged:
    /// after RACK's, the window the threshold says (PRR's end); after a
    /// timeout's, the one slow start has grown to. Its copies' D-SACKs are
    /// still looked for, and RACK's reordering window shrinks back after
    /// sixteen recoveries with no D-SACK.
    fn end_recovery(&'static self, inner: &mut Inner) {
        if inner.recovery == Recovery::Fast {
            inner.cwnd = inner.ssthresh.max(seg_size(inner));
        }
        inner.recovery = Recovery::None;
        if let Some(u) = inner.undo.as_mut() {
            u.until = Some(inner.snd_nxt);
        }
        inner.rack.reo_persist = inner.rack.reo_persist.saturating_sub(1);
        if inner.rack.reo_persist == 0 {
            inner.rack.reo_mult = 1;
        }
        if trace::on::<TcpLoss>() {
            trace_loss(inner, TcpLossWhat::End);
        }
    }

    /// Segments counted lost sent again, oldest first, as far as the
    /// congestion window goes over what is in flight, `RESEND_SEGMENTS` at
    /// most a call -- and in a timeout's recovery, the FIN after them once
    /// they have all gone.
    fn send_lost(&'static self, inner: &mut Inner, now_ns: u64) {
        let mut sent = 0;
        let mut i = 0;
        while inner.sent.lost != 0 && i < inner.sent.len && sent < RESEND_SEGMENTS {
            if inner.sent.at(i).fate != Fate::Lost {
                i += 1;
                continue;
            }
            let len = inner.sent.len_of(i, inner.snd_una);
            if inner.pipe().saturating_add(len) > inner.cwnd {
                break;
            }
            self.resend(inner, i, now_ns, TcpWhy::Again);
            self.retransmits.fetch_add(1, Ordering::Relaxed);
            sent += 1;
            i += 1;
        }
        if inner.recovery == Recovery::Timeout && inner.sent.lost == 0 && fin_sent(inner.state)
            && before(inner.retx_nxt, inner.snd_nxt) && before(inner.snd_una, inner.snd_nxt)
            && inner.pipe() < inner.cwnd
        {
            let end = inner.snd_una.wrapping_add(inner.send_buf.used() as u32);
            self.send_fin_again(inner, end, TcpWhy::Again);
            inner.retx_nxt = inner.snd_nxt;
            self.retransmits.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The i'th segment in flight sent again, as it went: its bytes -- from
    /// SND.UNA on, for the oldest. A copy, now in flight in place of the one
    /// counted lost; one of the recovery's, for PRR and an undo; and
    /// Karn's rule kept: nothing is timed across it.
    fn resend(&'static self, inner: &mut Inner, i: usize, now_ns: u64, why: TcpWhy) {
        let una = inner.snd_una;
        let start = inner.sent.start(i, una);
        let mut segment = [0u8; OUR_MSS as usize];
        let len = (inner.sent.len_of(i, una) as usize).min(segment.len());
        let got = inner.send_buf.peek(&mut segment[..len], start.wrapping_sub(una) as usize);
        let saved = inner.snd_nxt;
        inner.snd_nxt = start;
        self.send_segment(inner, seg::ACK_FLAG | seg::PSH, &segment[..got], Some(why));
        inner.snd_nxt = saved;
        karn(inner, start, inner.sent.at(i).end);
        if inner.sent.at(i).fate == Fate::Lost {
            inner.sent.set_fate(i, una, Fate::Resent);
        }
        let s = inner.sent.at_mut(i);
        s.copies = true;
        s.sent_ns = now_ns;
        if inner.recovery == Recovery::Fast {
            inner.prr_out = inner.prr_out.saturating_add(got as u32);
        }
        if let Some(u) = inner.undo.as_mut() {
            if u.until.is_none() {
                u.copies += 1;
            }
        }
    }

    /// The early deadline once an acknowledgement is taken: RACK's wait for
    /// what it may yet find lost; or else a loss probe's (`probe_at`) --
    /// started again only by an acknowledgement that moved SND.UNA, as by
    /// new data sent; or none.
    fn set_early(&'static self, inner: &mut Inner, now: u64, wait_ns: u64, moved: bool) {
        if wait_ns != 0 {
            let at = now + wait_ns.div_ceil(kcore::consts::NS_PER_MS).max(1);
            inner.arm_early(Early::Reorder, at, &self.timer);
        } else if inner.early == Early::Probe && !moved && probe_at(inner, now).is_some() {
            /* Armed already, and nothing delivered since to say it should
             * wait longer */
        } else if let Some(at) = probe_at(inner, now) {
            inner.arm_early(Early::Probe, at, &self.timer);
        } else if inner.early != Early::None {
            inner.clear_early(&self.timer);
        }
    }

    /// The loss probe (RFC 8985 7.3): two round trips without an
    /// acknowledgement, and the newest segment in flight goes again -- the
    /// one whose loss no later segment would show -- so that its
    /// acknowledgement says, by SACK, what is missing, where a timeout
    /// would have had to. Its copy stays one in flight, not one counted
    /// lost; the timeout started again behind it.
    fn send_probe(&'static self, inner: &mut Inner, now: u64, now_ns: u64) {
        let i = inner.sent.len - 1;
        let una = inner.snd_una;
        let start = inner.sent.start(i, una);
        let mut segment = [0u8; OUR_MSS as usize];
        let len = (inner.sent.len_of(i, una) as usize).min(segment.len());
        let got = inner.send_buf.peek(&mut segment[..len], start.wrapping_sub(una) as usize);
        let saved = inner.snd_nxt;
        inner.snd_nxt = start;
        self.send_segment(inner, seg::ACK_FLAG | seg::PSH, &segment[..got], Some(TcpWhy::Probe));
        inner.snd_nxt = saved;
        karn(inner, start, inner.sent.at(i).end);
        let s = inner.sent.at_mut(i);
        s.copies = true;
        s.sent_ns = now_ns;
        let seg_end = s.end;
        inner.probe = Some(Probe { end: inner.snd_nxt, seg_end });
        self.probes.fetch_add(1, Ordering::Relaxed);
        self.retransmits.fetch_add(1, Ordering::Relaxed);
        inner.early = Early::None;
        inner.early_at = 0;
        inner.retransmit_in_rto(now, &self.timer);
    }

    /// The early deadline come, before the timeout: RACK looks again, and
    /// sends what it finds lost -- starting a recovery for it -- or the loss
    /// probe goes.
    fn on_early(&'static self, inner: &mut Inner, now: u64) {
        let early = inner.early;
        inner.clear_early(&self.timer);
        if !inner.sack_ok || !matches!(inner.state, State::Established | State::CloseWait | State::FinWait1
            | State::Closing | State::LastAck)
        {
            return;
        }
        let now_ns = now_ns();
        match early {
            Early::Reorder => {
                let (lost, wait) = rack_detect(inner, now_ns);
                if inner.recovery == Recovery::None && inner.sent.lost != 0 {
                    self.enter_recovery(inner);
                }
                /* As if a segment's worth had been delivered, as Linux has
                 * it: the first copy of a recovery goes in any case, and a
                 * later one would otherwise wait for an acknowledgement */
                if inner.recovery == Recovery::Fast && lost != 0 {
                    prr(inner, seg_size(inner), false, lost);
                }
                self.send_lost(inner, now_ns);
                self.set_early(inner, now, wait, false);
            }
            Early::Probe => {
                if inner.recovery == Recovery::None && inner.sent.len != 0 && inner.probe.is_none()
                    && inner.sent.sacked == 0
                {
                    self.send_probe(inner, now, now_ns);
                }
            }
            Early::None => {}
        }
    }

    /// A duplicate acknowledgement: the third, out of recovery, sends the
    /// segment at SND.UNA again and halves the window (RFC 5681 3.2) --
    /// unless the acknowledgement covers no more than the last recovery did,
    /// which says only that the segments that recovery sent again came twice
    /// (RFC 6582 3.2); each one more in a fast recovery is a segment that
    /// left the network, and lets one more in.
    fn duplicate_ack(&'static self, inner: &mut Inner) {
        let mss = seg_size(inner);
        inner.dup_acks = inner.dup_acks.saturating_add(1);
        match inner.recovery {
            Recovery::None if inner.dup_acks == DUP_ACKS && before(inner.recover, inner.snd_una) => {
                let flight = inner.snd_nxt.wrapping_sub(inner.snd_una);
                inner.ssthresh = (flight / 2).max(2 * mss);
                inner.recover = inner.snd_nxt;
                inner.recovery = Recovery::Fast;
                let una = inner.snd_una;
                self.send_again(inner, una, TcpWhy::Again);
                inner.cwnd = inner.ssthresh.saturating_add(3 * mss);
                self.retransmits.fetch_add(1, Ordering::Relaxed);
            }
            Recovery::Fast => inner.cwnd = inner.cwnd.saturating_add(mss).min(CWND_MAX),
            _ => {}
        }
    }

    /// What congestion control makes of `acked` bytes newly acknowledged.
    fn newly_acked(&'static self, inner: &mut Inner, acked: u32) {
        let mss = seg_size(inner);
        inner.dup_acks = 0;
        match inner.recovery {
            Recovery::Fast if before(inner.snd_una, inner.recover) => {
                /* A partial acknowledgement (RFC 6582 3.2 step 5): the next
                 * hole of the same window, sent at once; the window less
                 * what was acknowledged, and a segment back for the one
                 * sent */
                let una = inner.snd_una;
                self.send_again(inner, una, TcpWhy::Again);
                inner.cwnd = inner.cwnd.saturating_sub(acked).saturating_add(mss).max(mss);
            }
            Recovery::Fast => {
                /* All that was outstanding at the loss acknowledged: the
                 * window the loss left (RFC 6582 3.2 step 3) */
                let flight = inner.snd_nxt.wrapping_sub(inner.snd_una);
                inner.cwnd = inner.ssthresh.min(flight.saturating_add(mss)).max(mss);
                inner.recovery = Recovery::None;
            }
            Recovery::Timeout => {
                grow_cwnd(inner, acked);
                if before(inner.snd_una, inner.recover) {
                    if before(inner.retx_nxt, inner.snd_una) {
                        inner.retx_nxt = inner.snd_una;
                    }
                    self.retransmit_window(inner);
                } else {
                    inner.recovery = Recovery::None;
                }
            }
            Recovery::None => grow_cwnd(inner, acked),
        }
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

        /* What was taken already -- a retransmission's start -- cut off,
         * and said: a D-SACK of it tells its sender the copy was not needed
         * (RFC 2883) */
        if before(seq, inner.rcv_nxt) {
            let taken = inner.rcv_nxt.wrapping_sub(seq) as usize;
            let dup = taken.min(data.len());
            if inner.sack_ok && dup != 0 {
                inner.dsack = Some((seq, seq.wrapping_add(dup as u32)));
            }
            data = &data[dup..];
            seq = inner.rcv_nxt;
        }

        if !data.is_empty() {
            /* Not before RCV.NXT now, and in the window: a small number */
            let offset = seq.wrapping_sub(inner.rcv_nxt) as usize;
            if offset == 0 {
                let written = inner.recv_buf.write(data);
                if written < data.len() {
                    self.rx_unkept.fetch_add(data.len() - written, Ordering::Relaxed);
                    self.overrun(inner, seq.wrapping_add(written as u32), data.len() - written);
                }
                inner.rcv_nxt = inner.rcv_nxt.wrapping_add(written as u32);
                take_ooo(inner);
                conn.ready();
            } else {
                if inner.sack_ok {
                    if let Some(dup) = ooo_overlap(inner, seq, seq.wrapping_add(data.len() as u32)) {
                        inner.dsack = Some(dup);
                    }
                }
                let kept = inner.recv_buf.put(offset, data);
                if kept < data.len() {
                    self.overrun(inner, seq.wrapping_add(kept as u32), data.len() - kept);
                }
                let held = kept != 0 && keep_ooo(inner, seq, seq.wrapping_add(kept as u32));
                let lost = if held { data.len() - kept } else { data.len() };
                if lost != 0 {
                    self.rx_unkept.fetch_add(lost, Ordering::Relaxed);
                }
            }
            inner.rcv_wnd = inner.recv_buf.free() as u32;
        }

        self.send_ack(inner);
    }

    /// `len` bytes at `seq` the buffer had no room for: as many of them as
    /// lie before the furthest edge offered counted, as a promise broken.
    fn overrun(&self, inner: &Inner, seq: u32, len: usize) {
        let end = seq.wrapping_add(len as u32);
        let to = if before(inner.rcv_adv, end) { inner.rcv_adv } else { end };
        if before(seq, to) {
            self.rx_overrun.fetch_add(to.wrapping_sub(seq) as usize, Ordering::Relaxed);
        }
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

    /// One segment of what was sent before, again: the stream from `seq` --
    /// which the send buffer holds from SND.UNA on -- as much as a segment
    /// carries. The bytes it carried: 0 when the buffer has none there.
    fn send_again(&'static self, inner: &mut Inner, seq: u32, why: TcpWhy) -> u32 {
        let offset = seq.wrapping_sub(inner.snd_una) as usize;
        let used = inner.send_buf.used();
        if offset >= used {
            return 0;
        }
        let len = (used - offset).min(seg_size(inner) as usize);
        let mut segment = [0u8; OUR_MSS as usize];
        let got = inner.send_buf.peek(&mut segment[..len], offset);
        let saved = inner.snd_nxt;
        inner.snd_nxt = seq;
        self.send_segment(inner, seg::ACK_FLAG | seg::PSH, &segment[..got], Some(why));
        inner.snd_nxt = saved;
        karn(inner, seq, seq.wrapping_add(got as u32));
        got as u32
    }

    /// The FIN again, at the end of the data.
    fn send_fin_again(&'static self, inner: &mut Inner, seq: u32, why: TcpWhy) {
        let saved = inner.snd_nxt;
        inner.snd_nxt = seq;
        self.send_segment(inner, seg::FIN | seg::ACK_FLAG, &[], Some(why));
        inner.snd_nxt = saved;
        inner.rtt_sent_ns = 0;
    }

    /// In a timeout's recovery: more of what was outstanding when it came,
    /// from where the last went, as far as the congestion window and the
    /// peer's window reach -- and the FIN after the data, when it is one of
    /// them. Without selective acknowledgements nothing says what of it
    /// arrived, so all of it counts as lost (go-back-N): what is in flight
    /// is what has gone again and is not acknowledged. At most
    /// `RESEND_SEGMENTS` an acknowledgement.
    fn retransmit_window(&'static self, inner: &mut Inner) {
        let mss = seg_size(inner);
        let limit = inner.cwnd.min(inner.snd_wnd);
        let end = inner.snd_una.wrapping_add(inner.send_buf.used() as u32);
        let fin = fin_sent(inner.state);
        let mut sent = 0;
        while sent < RESEND_SEGMENTS && before(inner.retx_nxt, inner.recover) {
            let pipe = inner.retx_nxt.wrapping_sub(inner.snd_una);
            if before(inner.retx_nxt, end) {
                let len = end.wrapping_sub(inner.retx_nxt).min(mss);
                if pipe.saturating_add(len) > limit {
                    break;
                }
                let retx = inner.retx_nxt;
                let got = self.send_again(inner, retx, TcpWhy::Again);
                if got == 0 {
                    break;
                }
                inner.retx_nxt = retx.wrapping_add(got);
            } else if fin && inner.retx_nxt == end && pipe < limit {
                self.send_fin_again(inner, end, TcpWhy::Again);
                inner.retx_nxt = end.wrapping_add(1);
            } else {
                break;
            }
            sent += 1;
        }
    }

    /// A retransmission timeout with something outstanding (RFC 5681 3.1,
    /// RFC 6298 5): the threshold half of what was in flight -- at the
    /// first timeout of a loss, not at each backoff after it -- the window
    /// one segment, and the first of what is outstanding sent again whatever
    /// the windows say, the probe it then is; the acknowledgements it brings
    /// send the rest (`retransmit_window`). One segment a timeout once gave
    /// back a window lost whole a second a segment; now it is a round trip
    /// for each doubling of the window. True if anything went.
    fn timeout(&'static self, inner: &mut Inner) -> bool {
        if inner.sack_ok {
            return self.sack_timeout(inner);
        }
        let mss = seg_size(inner);
        if inner.recovery != Recovery::Timeout {
            let flight = inner.snd_nxt.wrapping_sub(inner.snd_una);
            inner.ssthresh = (flight / 2).max(2 * mss);
            inner.recover = inner.snd_nxt;
        }
        inner.cwnd = mss;
        inner.recovery = Recovery::Timeout;
        inner.dup_acks = 0;
        inner.retx_nxt = inner.snd_una;

        let end = inner.snd_una.wrapping_add(inner.send_buf.used() as u32);
        if before(inner.retx_nxt, end) {
            let una = inner.snd_una;
            let got = self.send_again(inner, una, TcpWhy::Timeout);
            inner.retx_nxt = una.wrapping_add(got);
            got != 0
        } else if fin_sent(inner.state) && before(inner.snd_una, inner.snd_nxt) {
            self.send_fin_again(inner, end, TcpWhy::Timeout);
            inner.retx_nxt = end.wrapping_add(1);
            true
        } else {
            false
        }
    }
}

/// Karn's rule for [start, end) sent again: the segment timed -- the one
/// ending at `rtt_seq` -- is timed no more if it is in there, its
/// acknowledgement being either copy's; any other stays timed. Stopped by
/// every copy of anything, as it was, the timing never lasted the round
/// trip of a path that had grown longer -- each loss probe went after two
/// of the old round trips -- and the timeout stayed under it: spurious
/// timeouts, one after another, the whole window sent again for each.
#[inline]
fn karn(inner: &mut Inner, start: u32, end: u32) {
    if inner.rtt_sent_ns != 0 && before(start, inner.rtt_seq) && before_eq(inner.rtt_seq, end) {
        inner.rtt_sent_ns = 0;
    }
}

/// The sender's recovery, as its records say it.
#[inline]
fn recovery_of(inner: &Inner) -> TcpRecovery {
    match inner.recovery {
        Recovery::None => TcpRecovery::None,
        Recovery::Fast => TcpRecovery::Fast,
        Recovery::Timeout => TcpRecovery::Timeout,
    }
}

/// `tcp_loss`: what the sender made of a loss, with its windows and what
/// is in flight after it. The caller has asked `on`.
fn trace_loss(inner: &Inner, what: TcpLossWhat) {
    trace::emit(&TcpLoss {
        cwnd: inner.cwnd,
        ssthresh: inner.ssthresh,
        pipe: inner.pipe(),
        sacked: inner.sent.sacked,
        lost: inner.sent.lost,
        una: inner.snd_una,
        nxt: inner.snd_nxt,
        lport: inner.local_port,
        rport: inner.remote_port,
        slot: inner.slot,
        what: what as u8,
        _pad: [0; 2],
    });
}

/// `tcp_wait`: a program's call that waited `ns` for `what`.
fn trace_wait(slot: u8, lport: u16, rport: u16, what: TcpWaitFor, ns: u64) {
    trace::emit(&TcpWait {
        ns: u32::try_from(ns).unwrap_or(u32::MAX),
        pid: trace::pid(),
        lport,
        rport,
        slot,
        what: what as u8,
        _pad: [0; 2],
    });
}

/// Whether a segment sent at `t1` ending at `e1` went after one sent at `t2`
/// ending at `e2`: by the time, and by where it ends when the times are the
/// same (RFC 8985's RACK_sent_after).
#[inline]
fn sent_after(t1: u64, e1: u32, t2: u64, e2: u32) -> bool {
    t1 > t2 || (t1 == t2 && before(e2, e1))
}

/// RACK told of a segment newly delivered (RFC 8985 6.2, steps 1 to 3):
/// reordering, when one never sent again comes in after one past it had --
/// against what was known before this acknowledgement, which is the RFC's
/// walk in order of the ends; the least round trip, from one never sent
/// again; and the round trip and the most recently sent of the segments
/// delivered -- unless it is a copy acknowledged too soon to be the
/// copy's, which the original's acknowledgement then was.
fn rack_delivered(rack: &mut Rack, seg: Seg, now_ns: u64, now_ms: u64, prior_fack: u32) {
    if before(seg.end, prior_fack) && !seg.copies {
        rack.reordering = true;
    }
    if before(rack.fack, seg.end) {
        rack.fack = seg.end;
    }
    let rtt_us = (now_ns.saturating_sub(seg.sent_ns) / 1000).max(1);
    if !seg.copies && (rack.min_rtt_us == 0 || rtt_us <= rack.min_rtt_us
        || now_ms.saturating_sub(rack.min_rtt_ms) > MIN_RTT_WINDOW_MS)
    {
        rack.min_rtt_us = rtt_us;
        rack.min_rtt_ms = now_ms;
    }
    if seg.copies && rtt_us < rack.min_rtt_us {
        return;
    }
    rack.rtt_us = rtt_us;
    if sent_after(seg.sent_ns, seg.end, rack.sent_ns, rack.end) {
        rack.sent_ns = seg.sent_ns;
        rack.end = seg.end;
    }
}

/// A SACK block, [left, right): every segment in flight wholly inside it
/// held by the peer, each newly so handed to RACK -- `original` said when
/// one was sent before the recovery's start and never again, `copy` when
/// one was a copy. The
/// bytes newly held. A block that does not lie wholly past SND.UNA and
/// within the data sent says nothing at all (Linux's
/// tcp_is_sackblock_valid): a receiver never SACKs what is at its own
/// acknowledgement, so one that reaches SND.UNA is an old acknowledgement's,
/// overtaken -- cut to SND.UNA, it once had the oldest segment taken for
/// held, and the next acknowledgement took that for a reneging and threw
/// away everything the peer had said. One that covers only part of a
/// segment says nothing of that segment, which this end never sends cut
/// otherwise.
#[allow(clippy::too_many_arguments)]
fn sack_mark(inner: &mut Inner, left: u32, right: u32, now_ns: u64, now_ms: u64, prior_fack: u32,
             original: &mut bool, copy: &mut bool) -> u32 {
    let una = inner.snd_una;
    let data_end = una.wrapping_add(inner.send_buf.used() as u32);
    if !before(left, right) || !before(una, left) || before(data_end, right) {
        return 0;
    }
    let mut delivered = 0;
    let mut i = inner.sent.index_after(left);
    while i < inner.sent.len {
        let seg = inner.sent.at(i);
        if before(right, seg.end) {
            break;
        }
        if seg.fate != Fate::Sacked && !before(inner.sent.start(i, una), left) {
            delivered += inner.sent.len_of(i, una);
            inner.sent.set_fate(i, una, Fate::Sacked);
            *original |= !seg.copies && before_eq(seg.end, inner.recover);
            *copy |= seg.copies;
            rack_delivered(&mut inner.rack, seg, now_ns, now_ms, prior_fack);
        }
        i += 1;
    }
    delivered
}

/// RACK's reordering window (RFC 8985 6.2 step 4), in us: none while the
/// path has not been seen to reorder and a recovery is under way, or three
/// segments are held past a hole -- the duplicate acknowledgements' rule;
/// otherwise a quarter of the least round trip for every round that saw a
/// D-SACK, and never past the smoothed round trip.
fn reo_wnd_us(inner: &Inner) -> u64 {
    if !inner.rack.reordering && (inner.recovery != Recovery::None || inner.sent.sacked_segs >= DUP_ACKS) {
        return 0;
    }
    (u64::from(inner.rack.reo_mult) * inner.rack.min_rtt_us / 4).min(inner.srtt_us)
}

/// The window grown by a round that saw a D-SACK (RFC 8985 6.2 step 4): a
/// quarter of the least round trip more, once a round -- until SND.UNA
/// passes where SND.NXT was when the last one did -- and kept for sixteen
/// recoveries after (`end_recovery`).
fn rack_dsack_round(inner: &mut Inner, dsack: bool) {
    if let Some(round) = inner.rack.dsack_round {
        if !before(inner.snd_una, round) {
            inner.rack.dsack_round = None;
        }
    }
    if dsack && inner.rack.dsack_round.is_none() {
        inner.rack.dsack_round = Some(inner.snd_nxt);
        inner.rack.reo_mult = (inner.rack.reo_mult + 1).min(REO_MULT_MAX);
        inner.rack.reo_persist = REO_PERSIST;
    }
}

/// RACK's look for what is lost (RFC 8985 6.2 step 5): every segment in
/// flight sent before the most recent one known delivered, and longer ago
/// than that one's round trip and the reordering window, counted lost --
/// its copy too, when it had been sent again. The walk is in the order of
/// the stream and ends at the first segment never sent again that went
/// after the one delivered: every later one went later still. The bytes
/// newly lost, and how long until the next of the rest would be (ns; 0 when
/// none waits).
fn rack_detect(inner: &mut Inner, now_ns: u64) -> (u32, u64) {
    if inner.rack.sent_ns == 0 || inner.sent.len == 0 {
        return (0, 0);
    }
    let after = inner.rack.rtt_us.saturating_add(reo_wnd_us(inner)).saturating_mul(1000);
    let una = inner.snd_una;
    let (mut lost, mut wait) = (0, 0);
    for i in 0..inner.sent.len {
        let seg = inner.sent.at(i);
        if matches!(seg.fate, Fate::Sacked | Fate::Lost) {
            continue;
        }
        if sent_after(seg.sent_ns, seg.end, inner.rack.sent_ns, inner.rack.end) {
            if seg.copies {
                continue;
            }
            break;
        }
        let due = seg.sent_ns.saturating_add(after);
        if now_ns >= due {
            lost += inner.sent.len_of(i, una);
            inner.sent.set_fate(i, una, Fate::Lost);
        } else {
            wait = wait.max(due - now_ns);
        }
    }
    (lost, wait)
}

/// PRR (RFC 6937, its slow-start reduction bound, and Linux's): how much a
/// recovery sends for what each acknowledgement says was delivered -- in
/// proportion, the threshold to what was in flight at the start, while
/// what is in flight is above the threshold; up to the threshold, and no
/// more than what was delivered and a segment, below it -- set as the
/// window over what is in flight. The recovery's first copy goes at once,
/// and nothing in flight never means nothing to send.
fn prr(inner: &mut Inner, delivered: u32, moved: bool, newly_lost: u32) {
    let mss = i64::from(seg_size(inner));
    inner.prr_delivered = inner.prr_delivered.saturating_add(delivered);
    let pipe = inner.pipe();
    let (ssthresh, out) = (i64::from(inner.ssthresh), i64::from(inner.prr_out));
    let sndcnt = if i64::from(pipe) > ssthresh {
        let share = (u64::from(inner.prr_delivered) * u64::from(inner.ssthresh))
            .div_ceil(u64::from(inner.recover_fs.max(1)));
        i64::try_from(share).unwrap_or(i64::MAX) - out
    } else {
        let mut limit = (i64::from(inner.prr_delivered) - out).max(i64::from(delivered));
        if moved && newly_lost == 0 {
            limit += mss;
        }
        (ssthresh - i64::from(pipe)).min(limit)
    };
    let mut sndcnt = sndcnt.max(0);
    if out == 0 || pipe == 0 {
        sndcnt = sndcnt.max(mss);
    }
    inner.cwnd = (i64::from(pipe) + sndcnt).clamp(0, i64::from(CWND_MAX)) as u32;
}

/// When a loss probe would go (RFC 8985 7.2), if one would: on a connection
/// that takes SACK, with data in flight, out of any recovery, nothing held
/// past a hole and no probe out already -- two smoothed round trips on,
/// and the peer's longest delayed acknowledgement more for one segment
/// alone, unless the peer acknowledges at once (a link given its own
/// floor); and only before the timeout, which would send the same.
fn probe_at(inner: &Inner, now: u64) -> Option<u64> {
    if !inner.sack_ok || inner.recovery != Recovery::None || inner.sent.len == 0 || inner.sent.sacked != 0
        || inner.probe.is_some() || inner.srtt_us == 0 || inner.rto_at == 0
    {
        return None;
    }
    let mut pto_us = 2 * inner.srtt_us;
    if inner.sent.len == 1 && inner.min_rto_ms >= MIN_RTO_MS {
        pto_us += WC_DELACK_MS * 1000;
    }
    let at = now + pto_us.div_ceil(1000).max(PTO_MIN_MS);
    (at < inner.rto_at).then_some(at)
}

impl Tcp {
    /// A timeout on a connection that takes SACK (RFC 6675 5.1, RFC 5681
    /// 3.1): everything in flight the peer does not hold counted lost -- a
    /// copy sent again with it -- the window one segment and the oldest
    /// sent again whatever the windows say; the acknowledgements it brings
    /// send the rest of what is lost, from the oldest on, as slow start
    /// grows the window, and no more than that: what the peer said it holds
    /// is not sent twice. The threshold halves at the first timeout of a
    /// loss -- not at RACK's recovery's, which halved it already, nor at a
    /// backoff -- and what it and the window were is kept for an undo, as
    /// at a recovery's start. True if anything went.
    fn sack_timeout(&'static self, inner: &mut Inner) -> bool {
        let mss = seg_size(inner);
        match inner.recovery {
            Recovery::None => {
                let flight = inner.snd_nxt.wrapping_sub(inner.snd_una);
                inner.undo = Some(Undo { from: inner.snd_una, to: inner.snd_nxt, copies: 0, twice: 0,
                                         cwnd: inner.cwnd, ssthresh: inner.ssthresh, until: None });
                inner.ssthresh = (flight / 2).max(2 * mss);
                inner.recover = inner.snd_nxt;
            }
            Recovery::Fast => {
                if let Some(u) = inner.undo.as_mut() {
                    u.to = inner.snd_nxt;
                }
                inner.recover = inner.snd_nxt;
            }
            Recovery::Timeout => {}
        }
        inner.cwnd = mss;
        inner.recovery = Recovery::Timeout;
        inner.probe = None;
        inner.dup_acks = 0;
        inner.retx_nxt = inner.snd_una;
        inner.early = Early::None;
        inner.early_at = 0;
        inner.sent.lose_all(inner.snd_una);
        self.timeouts.fetch_add(1, Ordering::Relaxed);
        if trace::on::<TcpLoss>() {
            trace_loss(inner, TcpLossWhat::Timeout);
        }

        if inner.sent.len != 0 {
            self.resend(inner, 0, now_ns(), TcpWhy::Timeout);
            true
        } else if fin_sent(inner.state) && before(inner.snd_una, inner.snd_nxt) {
            let end = inner.snd_una.wrapping_add(inner.send_buf.used() as u32);
            self.send_fin_again(inner, end, TcpWhy::Timeout);
            inner.retx_nxt = inner.snd_nxt;
            true
        } else {
            false
        }
    }
}

/// The most of the stream one segment carries: the peer's MSS, no more than
/// this end's, never 0.
fn seg_size(inner: &Inner) -> u32 {
    u32::from(inner.peer_mss).clamp(1, u32::from(OUR_MSS))
}

/// The congestion window a connection starts with (RFC 6928): ten segments,
/// within 14,600 bytes, two at least.
fn initial_window(mss: u32) -> u32 {
    (IW_SEGMENTS * mss).min((2 * mss).max(IW_BYTES))
}

/// Congestion control from the handshake on (RFC 5681 3.1): the initial
/// window, a threshold as high as it goes, no loss yet -- and `recover`
/// just behind SND.UNA, so that the first loss may start a recovery.
fn start_congestion(inner: &mut Inner) {
    inner.cwnd = initial_window(seg_size(inner));
    inner.ssthresh = u32::MAX;
    inner.dup_acks = 0;
    inner.recovery = Recovery::None;
    inner.recover = inner.snd_una.wrapping_sub(1);
    inner.retx_nxt = inner.snd_una;
    inner.rack = Rack::new();
    inner.rack.fack = inner.snd_una;
    inner.probe = None;
    inner.undo = None;
}

/// The window grown by `acked` bytes acknowledged: by as much, up to a
/// segment, in slow start (RFC 5681 3.1; RFC 3465's L of one segment), by a
/// segment a round trip past the threshold.
fn grow_cwnd(inner: &mut Inner, acked: u32) {
    let mss = seg_size(inner);
    let more = if inner.cwnd < inner.ssthresh {
        acked.min(mss)
    } else {
        (mss.saturating_mul(mss) / inner.cwnd.max(1)).max(1)
    };
    inner.cwnd = inner.cwnd.saturating_add(more).min(CWND_MAX);
}

/// Whether this end's FIN has gone: what a timeout sends again after the
/// data.
fn fin_sent(state: State) -> bool {
    matches!(state, State::FinWait1 | State::Closing | State::LastAck)
}

/// The window to advertise in a segment that is no SYN, as its field says
/// it; and, kept, the furthest right edge offered. As much as the receive
/// buffer has free, in units of two to this end's shift and rounded down:
/// no edge is ever offered past the buffer's end, so whatever an offer let
/// the peer send has room when it comes. That end -- RCV.NXT and what is
/// free past it -- never moves back: data taken in moves RCV.NXT on by as
/// much as it fills, a read gives room back. So an unscaled window never
/// shrinks; a scaled one may fall back by less than its unit, once RCV.NXT
/// has moved on by bytes that are no whole number of units -- the retracted
/// window of RFC 7323 2.4, which a receiver may offer instead of one past
/// its buffer, and must go on honouring the old edge of (`acceptable`): it
/// lies in the free space. Holding the old edge instead, rounded up, as
/// this did, crept up to a unit past it with every acknowledgement while
/// nobody read, and past the buffer's end within a few: the peer's data
/// there was lost, and its acknowledgements past RCV.NXT were dropped with
/// the window shut -- 12% of a link's rate, lost on a clean network.
///
/// And the edge moves on only by a step at a time (`window_opens`): the
/// receiver's half of silly window avoidance (RFC 1122 4.2.3.3). A reader
/// that frees a few bytes at a time -- and RCV.NXT's own moves against the
/// unit, which shift a scaled edge by a few bytes either way -- would have
/// a sender that fills every opening send a segment of a few bytes into
/// each.
fn offer_window(inner: &mut Inner) -> u16 {
    let shift = inner.rcv_wscale;
    let free = inner.recv_buf.free();
    /* In TIME-WAIT and CLOSED nothing is taken in, and a bulk buffer goes
     * back: the window there is the one offered last -- what the slot's own
     * buffer has free took 504 KiB of it back in the acknowledgement of a
     * FIN sent again (net-fuzz, seed 11) */
    let room = if matches!(inner.state, State::TimeWait | State::Closed) {
        owed(inner)
    } else if window_opens(inner) {
        free
    } else {
        owed(inner).min(free)
    };
    let units = (room >> shift).min(u16::MAX as usize);
    let window = (units << shift) as u32;
    let edge = inner.rcv_nxt.wrapping_add(window);
    if before(inner.rcv_adv, edge) {
        inner.rcv_adv = edge;
    }
    inner.advertised_wnd = window;
    units as u16
}

/// What the window offered owes past RCV.NXT: none once RCV.NXT is at its
/// edge or past it.
fn owed(inner: &Inner) -> usize {
    let owed = inner.rcv_adv.wrapping_sub(inner.rcv_nxt);
    if (owed as i32) > 0 { owed as usize } else { 0 }
}

/// Whether the window's right edge would move on now: room past it for a
/// full segment, or for half the buffer if that is less (RFC 1122 4.2.3.3's
/// min(Fr * RCV.BUFF, Eff.snd.MSS), Fr a half) -- the least the receiver
/// lets it move by.
fn window_opens(inner: &Inner) -> bool {
    let step = (OUR_MSS as usize).min(inner.recv_buf.size / 2);
    inner.recv_buf.free() >= owed(inner) + step
}

/// A stretch [start, end) of what arrived ahead of a hole, with the ones
/// kept already: folded into every one it overlaps or touches, and the
/// whole made the newest -- what the first block of the next SACK option
/// says (RFC 2018 4) -- or, with no room left for another, not kept: its
/// bytes then lie in the buffer's free space uncounted, and come again.
/// The stretches are kept newest first, each where it was last added to:
/// the order the option repeats them in. Whether it was kept.
fn keep_ooo(inner: &mut Inner, start: u32, end: u32) -> bool {
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
            remove_ooo(inner, i);
        } else {
            i += 1;
        }
    }
    if inner.ooo_len < MAX_OOO {
        inner.ooo.copy_within(0..inner.ooo_len, 1);
        inner.ooo[0] = (start, end);
        inner.ooo_len += 1;
        true
    } else {
        false
    }
}

/// The i'th stretch forgotten, the others kept in their order.
fn remove_ooo(inner: &mut Inner, i: usize) {
    inner.ooo.copy_within(i + 1..inner.ooo_len, i);
    inner.ooo_len -= 1;
}

/// What of [start, end) arrived already, ahead of a hole: its overlap with
/// the first stretch kept that it overlaps -- a duplicate, for a D-SACK.
fn ooo_overlap(inner: &Inner, start: u32, end: u32) -> Option<(u32, u32)> {
    inner.ooo[..inner.ooo_len].iter().find_map(|&(s, e)| {
        let (from, to) = (if before(start, s) { s } else { start }, if before(e, end) { e } else { end });
        before(from, to).then_some((from, to))
    })
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
            remove_ooo(inner, i);
            /* RCV.NXT moved: every stretch is looked at again */
            i = 0;
        } else {
            i += 1;
        }
    }
}

/// Whether some of a segment of `len` sequence numbers -- its data, and a
/// SYN and a FIN each one -- starting at `seq` lies in the receive window
/// (RFC 9293 3.10.7.4): what the buffer has free past RCV.NXT, or as far as
/// the furthest edge offered, if a retracted window fell back from it --
/// any segment that was in the window of an acknowledgement sent is in it
/// (RFC 7323 2.4). One of no length is taken at the right edge too: an
/// acknowledgement from a peer that filled the window, which Linux and BSD
/// take and RFC 9293 asks a shut window to make allowance for -- dropped,
/// its sender's acknowledgements of this end's data were lost whenever this
/// end's buffer filled. A shut window takes data only at its edge, for the
/// acknowledgement it carries: there is no room for the data.
fn acceptable(inner: &Inner, seq: u32, len: u32) -> bool {
    let window = inner.rcv_wnd.max(owed(inner) as u32);
    let past = seq.wrapping_sub(inner.rcv_nxt);
    if len == 0 {
        return past <= window;
    }
    if window == 0 {
        return seq == inner.rcv_nxt;
    }
    past < window || seq.wrapping_add(len - 1).wrapping_sub(inner.rcv_nxt) < window
}

/// Whether `len` bytes of data at `seq` all lie before RCV.NXT, and within
/// a receive buffer of it: a copy of what was taken in -- not a guess from
/// anywhere in the sequence space.
fn old_duplicate(inner: &Inner, seq: u32, len: u32) -> bool {
    before_eq(seq.wrapping_add(len), inner.rcv_nxt) && inner.rcv_nxt.wrapping_sub(seq) <= inner.recv_buf.size as u32
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
        payload: &[u8]) -> (Event, TcpVerdict)
    {
        let seq = seg::seq(segment);
        let ack = seg::ack(segment);
        let flags = seg::flags(segment);
        let wnd = seg::window(segment);
        let now = now_ms();
        let len = payload.len() as u32;
        /* Nothing but an acknowledgement: what a duplicate one has to be */
        let pure = payload.is_empty() && flags & (seg::SYN | seg::FIN) == 0;
        /* What it says the peer holds past a hole, on a connection that
         * takes SACK -- parsed once, here */
        let mut sack_blocks = [(0u32, 0u32); seg::MAX_SACK_BLOCKS];
        let sacks = if inner.sack_ok && flags & seg::ACK_FLAG != 0 {
            seg::sack_blocks(segment, &mut sack_blocks)
        } else {
            0
        };
        let sacks = &sack_blocks[..sacks];

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
                    return (Event::None, TcpVerdict::ResetRefused);
                }
            } else if seq != inner.rcv_nxt {
                if seq.wrapping_sub(inner.rcv_nxt) < inner.rcv_wnd {
                    self.challenge(inner, now);
                }
                return (Event::None, TcpVerdict::ResetRefused);
            }

            inner.state = State::Closed;
            conn.conn_ready.store(true, Ordering::Release);
            conn.ready();
            return (Event::Rst, TcpVerdict::Reset);
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
                /* A copy of data taken in already -- sent again by a sender
                 * that took it for lost, or a keepalive -- is answered at
                 * once, whatever the limit on answers, and on a connection
                 * that takes SACK with a D-SACK of it: what tells its sender
                 * the copy was not needed (RFC 2883 4). Data is never part
                 * of an ACK loop, and the answer goes to the peer. */
                if len != 0 && flags & (seg::SYN | seg::RST) == 0 && old_duplicate(inner, seq, len) {
                    if inner.sack_ok {
                        inner.dsack = Some((seq, seq.wrapping_add(len)));
                    }
                    self.send_ack(inner);
                    return (Event::None, TcpVerdict::Copy);
                }
                if len != 0 {
                    self.rx_outside.fetch_add(1, Ordering::Relaxed);
                }
                self.challenge(inner, now);
                return (Event::None, TcpVerdict::Outside);
            }

            /* A SYN on a synchronized connection (RFC 5961 4.2): the peer
             * starting over, which answers this ACK with a reset at the
             * edge -- or a guess. Never a reason to drop the connection, as
             * a SYN in the window once was. */
            if flags & seg::SYN != 0 {
                self.challenge(inner, now);
                return (Event::None, TcpVerdict::Syn);
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
                self.rx_badack.fetch_add(1, Ordering::Relaxed);
                self.challenge(inner, now);
                return (Event::None, TcpVerdict::BadAck);
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
                        return (Event::None, TcpVerdict::BadAck);
                    }

                    inner.irs = seq;
                    inner.rcv_nxt = seq.wrapping_add(1);
                    /* What our SYN offered, from the stream's start */
                    inner.rcv_adv = inner.rcv_nxt.wrapping_add(inner.advertised_wnd);
                    inner.snd_una = ack;
                    /* A SYN's window is never scaled */
                    inner.snd_wnd = wnd as u32;
                    inner.max_snd_wnd = inner.snd_wnd;
                    inner.snd_wl1 = seq;
                    inner.snd_wl2 = ack;
                    inner.peer_mss = seg::parse_mss(segment, OUR_MSS, DEFAULT_MSS);
                    /* This end's SYN offered SACK: taken both ways when the
                     * peer's says so too (RFC 2018 2) */
                    inner.sack_ok = seg::sack_permitted(segment);
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
                    inner.rto_ms = rtt_rto(inner);
                    inner.disarm();
                    start_congestion(inner);

                    self.send_ack(inner);
                    conn.conn_ready.store(true, Ordering::Release);
                    return (Event::Connected, TcpVerdict::Taken);
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
                    inner.rto_ms = rtt_rto(inner);
                    inner.disarm();
                    start_congestion(inner);
                    conn.conn_ready.store(true, Ordering::Release);
                    return (Event::Accepted, TcpVerdict::Taken);
                }
            }

            State::Established => {
                if flags & seg::ACK_FLAG != 0 {
                    self.process_ack(inner, seq, ack, wnd, now, pure, sacks);
                }
                if !payload.is_empty() {
                    self.process_payload(conn, inner, seq, payload);
                }

                /* A FIN in order: the peer is done sending */
                if flags & seg::FIN != 0 && seq.wrapping_add(len) == inner.rcv_nxt {
                    inner.rcv_nxt = seq.wrapping_add(len).wrapping_add(1);
                    inner.state = State::CloseWait;
                    /* Wake a reader so it sees the end of the stream */
                    conn.ready();
                    self.send_ack(inner);
                }
            }

            State::FinWait1 => {
                /* Drain what the peer acknowledged alongside (or before) our
                 * FIN, so the retransmit timer stops resending stale bytes
                 * and our FIN can be acknowledged once everything is. */
                if flags & seg::ACK_FLAG != 0 {
                    self.process_ack(inner, seq, ack, wnd, now, pure, sacks);
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
                    inner.disarm();
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
                    self.process_ack(inner, seq, ack, wnd, now, pure, sacks);
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
                    self.process_ack(inner, seq, ack, wnd, now, pure, sacks);
                    if inner.snd_una == inner.snd_nxt {
                        inner.state = State::Closed;
                        conn.conn_ready.store(true, Ordering::Release);
                    }
                }
            }

            State::CloseWait => {
                if flags & seg::ACK_FLAG != 0 {
                    self.process_ack(inner, seq, ack, wnd, now, pure, sacks);
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

        (Event::None, TcpVerdict::Taken)
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
            Event::Rst => klog!(0, "tcp: reset received, {} -> {}",
                local_port, remote_port),
            /* Whether the window is scaled is what says how fast it can go */
            Event::Connected if rcv_wscale != 0 => klog!(0,
                "tcp: connected {} -> {}, mss {}, receive {} KiB, window scale {}/{}",
                local_port, remote_port, peer_mss, recv_size / 1024, rcv_wscale, snd_wscale),
            Event::Connected => klog!(0, "tcp: connected {} -> {}, mss {}, receive {} KiB",
                local_port, remote_port, peer_mss, recv_size / 1024),
            Event::Accepted => klog!(0, "tcp: accepted {} <- {}",
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

            /* The receiver as the segment finds it, for its record */
            let before = trace::on::<TcpRecv>().then(|| {
                let owed = inner.rcv_adv.wrapping_sub(inner.rcv_nxt);
                (inner.rcv_nxt, inner.recv_buf.free() as u32, if (owed as i32) > 0 { owed } else { 0 })
            });
            let (event, verdict) = self.handle(conn, &mut inner, segment, payload);
            if let Some((rcv_nxt, free, owed)) = before {
                let mut blocks = [(0u32, 0u32); seg::MAX_SACK_BLOCKS];
                trace::emit(&TcpRecv {
                    seq: seg::seq(segment),
                    ack: seg::ack(segment),
                    rcv_nxt,
                    free,
                    owed,
                    lport: local_port,
                    rport: remote_port,
                    len: payload.len() as u16,
                    wnd: seg::window(segment),
                    slot: inner.slot,
                    flags,
                    verdict: verdict as u8,
                    sacks: seg::sack_blocks(segment, &mut blocks) as u8,
                });
            }
            let peer_mss = inner.peer_mss;
            let receive = (inner.recv_buf.size, inner.rcv_wscale, inner.snd_wscale);
            drop(inner);
            /* Whatever it was -- an acknowledgement above all -- a send
             * waiting for room looks again */
            conn.room();

            self.trace_event(event, local_port, remote_port, peer_mss, receive);
            return;
        }

        /* For a port listened on by a listener that does not answer this
         * address: dropped, whatever it is -- no SYN-ACK, no reset, nothing
         * kept, before the backlog and a slot -- so that to a stranger the
         * port is not there at all, an ACK's probe answered no more than a
         * SYN (plans/tcp.md, T3). */
        let listener = pool.find_listener(local_ip, local_port, nic);
        if listener.is_some_and(|l| !pool.allows[l].admits(remote_ip)) {
            drop(pool);
            self.syn_refused.fetch_add(1, Ordering::Relaxed);
            return;
        }

        /* A SYN for something listening -- a SYN alone: in LISTEN an ACK
         * is answered with a reset, a RST ignored (RFC 9293 3.10.7.2), and
         * a SYN that carries either, or a FIN, is no request to connect. */
        let listener = listener.filter(|_| flags & (seg::SYN | seg::ACK_FLAG | seg::RST | seg::FIN) == seg::SYN);
        if let Some(listener) = listener {
            let mut pool = pool;

            /* Dropped, as a full accept queue drops it: the peer tries again
             * and the pool keeps its slots. */
            let backlog = usize::from(pool.slots[listener].backlog);
            let bulk = pool.slots[listener].bulk;
            if self.backlog_full(&pool, local_port, backlog) {
                return;
            }

            let index = match self.take_slot(&mut pool, Slot {
                used: true, listening: false,
                local_ip, local_port, remote_ip, remote_port,
                device: Some(*nic), backlog: 0, bulk: false,
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
            inner.opened_ms = now_ms();
            inner.irs = seg::seq(segment);
            inner.rcv_nxt = inner.irs.wrapping_add(1);
            inner.rcv_adv = inner.rcv_nxt;
            inner.iss = initial_sequence();
            inner.snd_nxt = inner.iss;
            inner.snd_una = inner.iss;
            /* A SYN's window is never scaled (RFC 7323 2.2) */
            inner.snd_wnd = seg::window(segment) as u32;
            inner.max_snd_wnd = inner.snd_wnd;
            inner.snd_wl1 = inner.irs;
            inner.snd_wl2 = 0;
            inner.peer_mss = seg::parse_mss(segment, OUR_MSS, DEFAULT_MSS);
            /* SACK both ways when the SYN offers it, which the SYN-ACK
             * then says too (RFC 2018 2) */
            inner.sack_ok = seg::sack_permitted(segment);
            /* Where the frame came from: no resolution needed */
            inner.peer_mac = peer_mac;
            /* A bulk listener's connection scales its window when the
             * peer's SYN offers to -- by the shift its largest buffer
             * needs, which `accept` gives it -- and neither end does
             * otherwise (RFC 7323 1.3). Its SYN-ACK says what the slot's
             * own 8 KiB has room for, unscaled as a SYN's window is. */
            if bulk {
                inner.bulk_wanted = true;
                if let Some(theirs) = seg::parse_wscale(segment) {
                    let ours = wscale_for(BULK_SIZES[0]);
                    inner.wscale_offer = Some(ours);
                    inner.rcv_wscale = ours;
                    inner.snd_wscale = theirs;
                }
            }

            self.send_segment(&mut inner, seg::SYN | seg::ACK_FLAG, &[], Some(TcpWhy::Handshake));
            /* A SYN takes a sequence number */
            inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
            inner.retransmit_in_rto(now_ms(), &self.timer);

            self.conn_count.fetch_add(1, Ordering::Relaxed);
            return;
        }

        drop(pool);

        /* Nothing here for it: reset, unless it is one itself -- or the
         * stack has sent as many as it sends anybody for now */
        if flags & seg::RST == 0 && self.reset_allowed() {
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
    /// nobody has accepted -- `backlog`, its own. The caller holds the pool
    /// lock and no connection's: whether one is waiting to be accepted is
    /// its own state, read under its own lock, and the pool says which slots
    /// are worth asking.
    fn backlog_full(&'static self, pool: &Pool, port: u16, backlog: usize) -> bool {
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
        pending >= backlog
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
        inner.disarm();
        inner.need_cleanup = true;
        conn.conn_ready.store(true, Ordering::Release);
        conn.ready();
        drop(inner);

        klog!(0, "tcp: icmp unreachable, {} -> {} aborted", local_port, remote_port);
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

        /* Bulk buffers come from the heap before any lock is taken -- and,
         * should the connect go no further, go back after the last one is
         * let go of: they are the first things here, so the last dropped. */
        let bulk = if receive != Receive::Small { BulkBuf::new() } else { None };
        let send_bulk = if receive == Receive::Both { BulkBuf::new_send() } else { None };

        /* Resolved before any lock is taken: it sleeps. Off-subnet goes to
         * the gateway. */
        let arp = abi::arp_table()?;
        let peer_mac = match arp.resolve(nic, nic.route_ip(dst_ip)) {
            Some(mac) => mac,
            None => {
                klog!(0, "tcp: nothing answered for the address to connect to");
                return None;
            }
        };

        let mut pool = self.pool.lock();

        let src_port = if src_port == 0 {
            let port = pool.alloc_ephemeral_port();
            if port == 0 {
                drop(pool);
                klog!(0, "tcp: no ephemeral port is free");
                return None;
            }
            port
        } else {
            /* Two connections with the same four addresses would take each
             * other's segments. */
            if pool.lookup(nic.ip(), src_port, dst_ip, dst_port, nic).is_some() {
                drop(pool);
                klog!(0, "tcp: port {} is already connected to that address", src_port);
                return None;
            }
            src_port
        };

        let local_ip = nic.ip();
        let index = match self.take_slot(&mut pool, Slot {
            used: true, listening: false,
            local_ip, local_port: src_port, remote_ip: dst_ip, remote_port: dst_port,
            device: Some(*nic), backlog: 0, bulk: false,
        }) {
            Some(index) => index,
            None => {
                drop(pool);
                klog!(0, "tcp: no free connection");
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
            inner.wscale_offer = Some(wscale_for(bulk.bytes.len()));
            inner.recv_buf.attach(bulk);
            inner.rcv_wnd = inner.recv_buf.free() as u32;
        }
        /* Nothing is in the send buffer before the SYN */
        if let Some(bulk) = send_bulk {
            inner.attach_send(bulk);
        }

        self.send_segment(&mut inner, seg::SYN, &[], Some(TcpWhy::Handshake));
        inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
        inner.retransmit_in_rto(now_ms(), &self.timer);
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

        klog!(0, "tcp: nothing answered {} -> {}", src_port, dst_port);
        self.close(conn);
        None
    }

    /// A passive open on `nic`: a SYN that comes in by it, for any address
    /// this machine has, and none that comes in by another device -- a
    /// guest behind the hypervisor's switch does not reach a service
    /// listening on eth0. A connection belongs to the device it arrives on.
    /// It holds LISTEN_BACKLOG connections nobody has accepted, at most.
    pub fn listen(&'static self, nic: &Nic, port: u16) -> Option<&'static Conn> {
        self.listen_with(nic, port, LISTEN_BACKLOG, Buffers::Small)
    }

    /// `listen`, holding `backlog` connections nobody has accepted at most --
    /// taken into 1..=LISTEN_BACKLOG -- each buffering what `buffers` says.
    pub fn listen_with(&'static self, nic: &Nic, port: u16, backlog: usize, buffers: Buffers)
        -> Option<&'static Conn>
    {
        self.listen_as(nic, port, backlog, buffers, Allow::ANY)
    }

    /// `listen_with`, answering `nets` alone -- each an address and a
    /// prefix length, host order: a segment from any other address for the
    /// port is dropped before its listener's backlog and before a slot,
    /// with no SYN-ACK and no reset (plans/tcp.md, T3). None for more than
    /// ALLOW_MAX networks or a prefix past 32, as for whatever `listen_with`
    /// refuses.
    pub fn listen_allowed(&'static self, nic: &Nic, port: u16, backlog: usize, buffers: Buffers,
        nets: &[(u32, u8)]) -> Option<&'static Conn>
    {
        self.listen_as(nic, port, backlog, buffers, Allow::of(nets)?)
    }

    /// A listener's allowlist changed: `nets` from now on, anyone for None.
    /// What it holds already is its own. False for a connection that is no
    /// listener, or for more than ALLOW_MAX networks or a prefix past 32.
    pub fn set_allowed(&'static self, listener: &'static Conn, nets: Option<&[(u32, u8)]>) -> bool {
        let allow = match nets {
            Some(nets) => match Allow::of(nets) {
                Some(allow) => allow,
                None => return false,
            },
            None => Allow::ANY,
        };
        let Some(index) = self.index_of(listener) else { return false };
        let mut pool = self.pool.lock();
        let slot = &pool.slots[index];
        if !(slot.used && slot.listening) {
            return false;
        }
        pool.allows[index] = allow;
        true
    }

    /// A connection's own floor for its timeout, `ms`: from LINK_MIN_RTO_MS
    /// up to the MIN_RTO_MS every connection starts with, for a link both
    /// of whose ends are nos -- what ipcnet's are -- where no delayed
    /// acknowledgement waits under 200 ms. It takes effect from the next
    /// round trip measured: the timeout of the moment is never lowered by
    /// it, which may be backed off from a loss that the next sample alone
    /// says is over (RFC 6298, 5.7). False for a value out of that range,
    /// or a connection that is no slot's or a listener.
    pub fn set_rto_floor(&'static self, conn: &'static Conn, ms: u64) -> bool {
        if !(LINK_MIN_RTO_MS..=MIN_RTO_MS).contains(&ms) || self.index_of(conn).is_none() {
            return false;
        }
        let mut inner = conn.inner.lock();
        if matches!(inner.state, State::Free | State::Listen) {
            return false;
        }
        inner.min_rto_ms = ms;
        inner.rto_ms = inner.rto_ms.max(ms);
        true
    }

    /// Whether a listener answers `ip`: None for a connection that is no
    /// listener.
    pub fn admits(&'static self, listener: &'static Conn, ip: u32) -> Option<bool> {
        let index = self.index_of(listener)?;
        let pool = self.pool.lock();
        let slot = &pool.slots[index];
        if !(slot.used && slot.listening) {
            return None;
        }
        Some(pool.allows[index].admits(ip))
    }

    fn listen_as(&'static self, nic: &Nic, port: u16, backlog: usize, buffers: Buffers, allow: Allow)
        -> Option<&'static Conn>
    {
        if !self.initialized.load(Ordering::Acquire) {
            return None;
        }

        let mut pool = self.pool.lock();

        /* A second listener on the same port would never be given anything */
        if pool.slots.iter().any(|slot| slot.used && slot.listening && slot.local_port == port) {
            drop(pool);
            klog!(0, "tcp: port {} is listened on already", port);
            return None;
        }

        /* The wildcard address, which a segment for any address matches:
         * bound to the device's address of the moment, the port would answer
         * nobody once a lease renewal moved it. */
        let index = self.take_slot(&mut pool, Slot {
            used: true, listening: true,
            local_ip: 0, local_port: port, remote_ip: 0, remote_port: 0,
            device: Some(*nic),
            /* LISTEN_BACKLOG fits a byte: checked where it is defined */
            backlog: backlog.clamp(1, LISTEN_BACKLOG) as u8,
            bulk: buffers == Buffers::Bulk,
        })?;
        pool.allows[index] = allow;

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
        /* A bulk listener's connection is given its buffers as it is taken:
         * from the heap, so with no lock held, once one is waiting -- a
         * listener nobody connects to takes none, each of which would cost
         * every CPU a TLB shootdown going back -- and, should they go to
         * nobody, dropped after the locks: declared before them, they are
         * the last thing dropped. */
        let mut bulk: Option<(Option<BulkBuf>, Option<BulkBuf>)> = None;

        'look: loop {
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
                    if inner.bulk_wanted && bulk.is_none() {
                        /* Its buffers first, off the locks; then the
                         * listener's connections are looked through again */
                        drop(inner);
                        drop(pool);
                        bulk = Some((BulkBuf::new(), BulkBuf::new_send()));
                        continue 'look;
                    }
                    inner.accepted = true;
                    inner.owned_by_app = true;
                    if inner.bulk_wanted {
                        if let Some((recv, send)) = bulk.as_mut() {
                            self.attach_bulk(&mut inner, recv, send);
                        }
                    }
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

    /// A bulk listener's connection given its buffers, as `accept` hands it
    /// out -- each taken out of its `Option` only when it is put in place, so
    /// that one not used is dropped by the caller, after the locks: what has
    /// arrived already moved across into the receive buffer, the send buffer
    /// in place while nothing is in it, and the room offered at once, since
    /// the peer has been held to the 8 KiB the SYN-ACK said.
    fn attach_bulk(&'static self, inner: &mut Inner, recv: &mut Option<BulkBuf>, send: &mut Option<BulkBuf>) {
        if inner.recv_buf.bulk.is_none() {
            if let Some(buf) = recv.take() {
                inner.recv_buf.grow(buf);
                inner.rcv_wnd = inner.recv_buf.free() as u32;
            }
        }
        if inner.send_buf.bulk.is_none() && inner.send_buf.used() == 0 {
            if let Some(buf) = send.take() {
                inner.attach_send(buf);
            }
        }
        if inner.state == State::Established {
            self.send_ack(inner);
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
            let who = (inner.slot, inner.local_port, inner.remote_port);
            if room == 0 {
                drop(inner);
                if deadline != 0 && now_ms() >= deadline {
                    return sent as isize;
                }
                wait_for(conn, &mut poll, deadline, who, TcpWaitFor::Buffer);
                continue;
            }

            /* Never past the peer's window: the bytes in flight must stay
             * within what it said it would take -- nor past the congestion
             * window, what the path is thought to take: against what is in
             * flight, which on a connection that takes SACK is less what
             * the peer holds past a hole and what is lost. Nothing new
             * while what was lost waits to go again, which goes first --
             * the whole of what was outstanding, after a timeout, on a
             * connection that does not take SACK -- nor once the table of
             * what is in flight is full. */
            let flight = inner.snd_nxt.wrapping_sub(inner.snd_una);
            let in_flight = if inner.sack_ok { inner.pipe() } else { flight };
            let window_room = inner.snd_wnd.saturating_sub(flight).min(inner.cwnd.saturating_sub(in_flight));
            let held = if inner.sack_ok { inner.sent.lost != 0 } else { inner.recovery == Recovery::Timeout };
            /* And on a connection that takes SACK no segment cut short by
             * the congestion window alone, while anything is in flight:
             * PRR's window grows by bytes, and a runt a time is a slot of the
             * table, a header and an acknowledgement each */
            let want = (data.len() - sent).min(room).min(inner.peer_mss as usize);
            let usable = inner.snd_wnd.saturating_sub(flight) as usize;
            let runt = inner.sack_ok && flight != 0
                && (inner.cwnd.saturating_sub(in_flight) as usize) < want.min(usable);
            /* Nor, on any connection, one the peer's window cuts short
             * while anything is in flight: the sender's half of silly
             * window avoidance (RFC 1122 4.2.3.4). The acknowledgement
             * coming brings more window, where a segment sent into the few
             * bytes open now is a frame, a header and an acknowledgement
             * for those few. Sent into all the same once the window is half
             * the largest the peer offered -- a peer whose buffer is that
             * small -- and with nothing in flight, which is the RFC's
             * override timer come at once: no acknowledgement would come to
             * open it. What the caller has left that fits goes as it is. */
            let silly = flight != 0 && usable < want && usable < (inner.max_snd_wnd / 2) as usize;
            if window_room == 0 || held || runt || silly || inner.sent.is_full() {
                /* Shut, or full. Not a byte into a shut window: the peer's
                 * duplicate acknowledgements would never advance anything,
                 * and the retransmit count would give up on a peer that is
                 * only slow to read -- a suspended ssh client -- as a dead
                 * one, in under a minute. The persist timer probes instead. */
                let what = if inner.sent.is_full() {
                    TcpWaitFor::Table
                } else if held {
                    TcpWaitFor::Lost
                } else if usable == 0 || silly {
                    TcpWaitFor::Window
                } else {
                    TcpWaitFor::Cwnd
                };
                drop(inner);
                if deadline != 0 && now_ms() >= deadline {
                    return sent as isize;
                }
                wait_for(conn, &mut poll, deadline, who, what);
                continue;
            }
            let window_room = window_room as usize;

            let chunk = (data.len() - sent)
                .min(room)
                .min(inner.peer_mss as usize)
                .min(window_room);

            inner.send_buf.write(&data[sent..sent + chunk]);

            let mut segment = [0u8; OUR_MSS as usize];
            let at = inner.send_buf.used() - chunk;
            let len = inner.send_buf.peek(&mut segment[..chunk], at);

            self.send_segment(&mut inner, seg::ACK_FLAG | seg::PSH, &segment[..len], Some(TcpWhy::New));
            inner.snd_nxt = inner.snd_nxt.wrapping_add(len as u32);
            let sent_ns = now_ns();
            let end = inner.snd_nxt;
            inner.sent.push(end, sent_ns);
            if inner.recovery == Recovery::Fast {
                inner.prr_out = inner.prr_out.saturating_add(len as u32);
            }
            /* One segment timed a round trip, from its first sending */
            if inner.rtt_sent_ns == 0 {
                inner.rtt_seq = inner.snd_nxt;
                inner.rtt_sent_ns = sent_ns;
            }

            let now = now_ms();
            if inner.rto_at == 0 {
                inner.retransmit_in_rto(now, &self.timer);
            }
            /* New data sent: the loss probe two round trips from now, unless
             * RACK waits on something sooner (RFC 8985 7.2) */
            if inner.early != Early::Reorder {
                if let Some(at) = probe_at(&inner, now) {
                    inner.arm_early(Early::Probe, at, &self.timer);
                }
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
                /* A wake is had by the first receive to come back after it,
                 * with data or without */
                conn.kicked.store(false, Ordering::Release);

                /* If what we last advertised was too small for a segment,
                 * the peer may have stopped sending. Now that the reader has
                 * drained the buffer far enough for the edge to move on,
                 * say so unasked -- no other path would, and the peer would
                 * sit probing a window that has long since reopened. */
                if inner.advertised_wnd < OUR_MSS as u32
                    && window_opens(&inner)
                    && matches!(inner.state,
                        State::Established | State::FinWait1 | State::FinWait2)
                {
                    self.send_ack(&mut inner);
                }

                return got as isize;
            }

            let closing = matches!(inner.state, State::CloseWait | State::Closed
                | State::TimeWait | State::LastAck | State::Closing);
            let who = (inner.slot, inner.local_port, inner.remote_port);
            drop(inner);

            if closing {
                return 0;
            }
            /* A wake: back to the caller, who has something else to do */
            if conn.kicked.swap(false, Ordering::AcqRel) {
                return RECV_TIMEOUT;
            }
            let now = now_ms();
            if deadline != 0 && now > deadline {
                return RECV_TIMEOUT;
            }
            let wait = if deadline != 0 { (deadline - now).max(1) } else { RX_LOOK_MS }.min(RX_LOOK_MS);
            let from = if trace::on::<TcpWait>() { now_ns() } else { 0 };
            match conn.rx.get() {
                Some(event) if !conn.rx_waiting.swap(true, Ordering::Acquire) => {
                    /* What came since the look above is kept by the event:
                     * nothing is lost between the look and the sleep */
                    event.wait_for(kcore::time::Duration::from_millis(wait));
                    conn.rx_waiting.store(false, Ordering::Release);
                }
                _ => poll.sleep(),
            }
            if from != 0 {
                trace_wait(who.0, who.1, who.2, TcpWaitFor::Data, now_ns().saturating_sub(from));
            }
        }
    }

    /// A task waiting in `recv` on `conn` -- or the next to -- back at once,
    /// as from a timeout, if `generation` is still the connection's: what a
    /// task that waits both on its connection and on something of its own
    /// is woken by, by whoever has that something (ipcnet's queues). The
    /// first receive to come back after it has it, with data or without.
    pub fn wake(&'static self, conn: &'static Conn, generation: u64) {
        {
            let _inner = conn.inner.lock();
            if conn.generation.load(Ordering::Acquire) != generation {
                return;
            }
            conn.kicked.store(true, Ordering::Release);
        }
        if let Some(e) = conn.rx.get() {
            e.signal();
        }
    }

    /// What a wake of `conn` is held to: the connection's generation.
    pub fn generation(&'static self, conn: &'static Conn) -> u64 {
        conn.generation.load(Ordering::Acquire)
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
                inner.disarm();
                inner.need_cleanup = true;
                conn.conn_ready.store(true, Ordering::Release);
                conn.ready();
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
            klog!(0, "tcp: listener on port {} closed, {} connections it never accepted were reset",
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
                self.send_segment(&mut inner, seg::FIN | seg::ACK_FLAG, &[], Some(TcpWhy::Close));
                inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                inner.retransmit_in_rto(now_ms(), &self.timer);
            }
            State::CloseWait => {
                inner.state = State::LastAck;
                self.send_segment(&mut inner, seg::FIN | seg::ACK_FLAG, &[], Some(TcpWhy::Close));
                inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                inner.retransmit_in_rto(now_ms(), &self.timer);
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
                self.send_segment(&mut inner, seg::RST | seg::ACK_FLAG, &[], Some(TcpWhy::Reset));
                inner.state = State::Closed;
            }
            State::SynSent | State::TimeWait => inner.state = State::Closed,
            _ => {}
        }

        inner.disarm();
        inner.need_cleanup = true;
        conn.conn_ready.store(true, Ordering::Release);
        conn.ready();
        /* As close: from here the slot is the cleanup timer's */
        inner.owned_by_app = false;
    }
}

/* ---- the timer ---- */

impl Tcp {
    /// The timer's task's: when a retransmission is due, and every
    /// `SWEEP_MS` whether or not.
    fn process_retransmits(&'static self) {
        /* The timer's earliest, over from none: what this walk finds set,
         * and what is set meanwhile, makes it again */
        self.timer.restart();
        let mut earliest = u64::MAX;
        let now = now_ms();
        let mut any_cleanup = false;
        /* Bulk buffers given back, a connection's two: taken out under the
         * locks, dropped once none is held */
        let mut released: [(Option<BulkBuf>, Option<BulkBuf>); MAX_CONNECTIONS] =
            [const { (None, None) }; MAX_CONNECTIONS];

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
                    released[i].0 = Some(bulk);
                }
                /* Nothing is sent from either state: what the buffer held is
                 * acknowledged, or will never be */
                if let Some(bulk) = inner.detach_send() {
                    released[i].1 = Some(bulk);
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
                conn.ready();
                any_cleanup = true;
                continue;
            }

            if inner.state == State::Closed {
                inner.need_cleanup = true;
                any_cleanup = true;
                continue;
            }

            /* Half open past its time: given up without a word -- a SYN
             * flood's would be a line each -- and its place in the
             * listener's backlog with it. Nobody owns it: nobody accepted
             * it. */
            if inner.state == State::SynReceived && now >= inner.opened_ms.saturating_add(HALF_OPEN_MS) {
                inner.state = State::Closed;
                inner.disarm();
                inner.need_cleanup = true;
                any_cleanup = true;
                self.half_open.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            /* An early deadline come before the timeout's -- RACK's wait,
             * a loss probe -- is no timeout: nothing backs off, and no try
             * counts towards giving up */
            let rto_due = inner.rto_at != 0 && now >= inner.rto_at;
            if inner.retransmit_at.due(now) && !rto_due && inner.early != Early::None {
                self.on_early(&mut inner, now);
            } else if inner.retransmit_at.due(now) {
                let mut retransmitted = false;

                match inner.state {
                    /* A handshake retransmit is about the state, not about
                     * buffered data */
                    State::SynSent => {
                        inner.snd_nxt = inner.snd_una;
                        self.send_segment(&mut inner, seg::SYN, &[], Some(TcpWhy::Timeout));
                        inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                        retransmitted = true;
                    }
                    State::SynReceived => {
                        inner.snd_nxt = inner.snd_una;
                        self.send_segment(&mut inner, seg::SYN | seg::ACK_FLAG, &[], Some(TcpWhy::Timeout));
                        inner.snd_nxt = inner.snd_nxt.wrapping_add(1);
                        retransmitted = true;
                    }
                    /* Whatever data is unacknowledged, and then our FIN */
                    State::FinWait1 | State::LastAck | State::Closing => {
                        retransmitted = self.timeout(&mut inner);
                    }
                    _ => {
                        if inner.snd_una != inner.snd_nxt && inner.send_buf.used() > 0 {
                            retransmitted = self.timeout(&mut inner);
                        }
                    }
                }

                if retransmitted {
                    self.retransmits.fetch_add(1, Ordering::Relaxed);
                    inner.retransmit_count += 1;
                    if inner.stalled_since_ms == 0 {
                        inner.stalled_since_ms = now;
                    }
                    let handshake = matches!(inner.state, State::SynSent | State::SynReceived);
                    let given_up = if handshake {
                        inner.retransmit_count > MAX_RETRANSMITS
                    } else {
                        now.saturating_sub(inner.stalled_since_ms) >= GIVE_UP_MS
                    };
                    if given_up {
                        /* Nothing acknowledged after this many tries: give
                         * the slot back. Otherwise a dead peer pins it for
                         * good -- half-open handshakes, unacknowledged FINs
                         * and unacknowledged data all retransmit with no
                         * limit of their own. */
                        let (local, remote, state, count) = (inner.local_port,
                            inner.remote_port, inner.state, inner.retransmit_count);

                        inner.state = State::Closed;
                        inner.disarm();
                        inner.need_cleanup = true;
                        conn.conn_ready.store(true, Ordering::Release);
                        conn.ready();
                        any_cleanup = true;
                        /* The line goes out with the lock down: a trace is
                         * synchronous output. */
                        drop(inner);

                        klog!(0, "tcp: {} -> {} in {} aborted after {} retransmits",
                            local, remote, state.name(), count);
                        continue;
                    }
                    inner.rto_ms = (inner.rto_ms * 2).min(MAX_RTO_MS);
                }
                inner.retransmit_in_rto(now, &self.timer);
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
                && !inner.retransmit_at.is_set()
            {
                if inner.persist_at == 0 {
                    inner.persist_at = now + inner.rto_ms;
                } else if now >= inner.persist_at {
                    let saved = inner.snd_nxt;
                    inner.snd_nxt = inner.snd_una.wrapping_sub(1);
                    self.send_segment(&mut inner, seg::ACK_FLAG, &[], Some(TcpWhy::Persist));
                    inner.snd_nxt = saved;

                    inner.rto_ms = (inner.rto_ms * 2).min(MAX_RTO_MS);
                    inner.persist_at = now + inner.rto_ms;
                }
            } else {
                inner.persist_at = 0;
            }

            if inner.retransmit_at.is_set() {
                earliest = earliest.min(inner.retransmit_at.at());
            }
        }
        if earliest != u64::MAX {
            self.timer.note(earliest);
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
                     * its bulk buffers go back now, never with the slot. */
                    if let Some(bulk) = inner.recv_buf.detach() {
                        assert!(released[i].0.is_none(), "tcp: a slot's bulk buffer given back twice");
                        released[i].0 = Some(bulk);
                    }
                    if let Some(bulk) = inner.detach_send() {
                        assert!(released[i].1.is_none(), "tcp: a slot's bulk buffer given back twice");
                        released[i].1 = Some(bulk);
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
            syn_refused: self.syn_refused.load(Ordering::Relaxed),
            half_open: self.half_open.load(Ordering::Relaxed),
            reclaimed: self.reclaimed.load(Ordering::Relaxed),
            rst_held: self.rst_held.load(Ordering::Relaxed),
            recoveries: self.recoveries.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            spurious: self.spurious.load(Ordering::Relaxed),
            probes: self.probes.load(Ordering::Relaxed),
            undos: self.undos.load(Ordering::Relaxed),
            dsacks: self.dsacks.load(Ordering::Relaxed),
            reneged: self.reneged.load(Ordering::Relaxed),
            rx_outside: self.rx_outside.load(Ordering::Relaxed),
            rx_badack: self.rx_badack.load(Ordering::Relaxed),
            rx_unkept: self.rx_unkept.load(Ordering::Relaxed),
            rx_overrun: self.rx_overrun.load(Ordering::Relaxed),
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
                cwnd: inner.cwnd as usize,
                /* This end's shift is never 0 on a connection that scales */
                scales: if inner.rcv_wscale != 0 {
                    Some((inner.rcv_wscale, inner.snd_wscale))
                } else {
                    None
                },
                retransmit_at: inner.retransmit_at.at(),
                opened_ms: inner.opened_ms,
                rto_ms: inner.rto_ms,
                min_rto_ms: inner.min_rto_ms,
                sack: inner.sack_ok,
                recovery: match inner.recovery {
                    Recovery::None => None,
                    Recovery::Fast => Some("fast"),
                    Recovery::Timeout => Some("timeout"),
                },
                sacked: inner.sent.sacked as usize,
                lost: inner.sent.lost as usize,
            })
        }
    }

    /// The index'th slot's table of segments in flight held to what it
    /// says of itself, for a fuzzer: that it covers exactly what the send
    /// buffer holds, its ends rising from SND.UNA, and that its counts are
    /// its fates' -- a count that drifted from them would steer every
    /// recovery wrong without overflowing; and the furthest edge its window
    /// offered within the receive buffer. What is wrong, when something is.
    pub fn audit_sent(&'static self, index: usize) -> Option<&'static str> {
        if index >= MAX_CONNECTIONS {
            return None;
        }
        let inner = self.conns[index].inner.lock();
        if inner.state == State::Free {
            return None;
        }
        let (mut at, mut total, mut sacked, mut sacked_segs, mut lost) = (inner.snd_una, 0u64, 0u32, 0u32, 0u32);
        for i in 0..inner.sent.len {
            let seg = inner.sent.at(i);
            if !before(at, seg.end) {
                return Some("a segment in flight ends where it starts, or before");
            }
            let len = seg.end.wrapping_sub(at);
            total += u64::from(len);
            match seg.fate {
                Fate::Sacked => {
                    sacked += len;
                    sacked_segs += 1;
                }
                Fate::Lost => lost += len,
                _ => {}
            }
            at = seg.end;
        }
        if total != inner.send_buf.used() as u64 {
            return Some("the segments in flight are not what the send buffer holds");
        }
        if sacked != inner.sent.sacked || sacked_segs != inner.sent.sacked_segs {
            return Some("what the peer holds past a hole is miscounted");
        }
        if lost != inner.sent.lost {
            return Some("what is lost is miscounted");
        }
        /* And the receiver's side: no edge offered past the buffer's end --
         * but in TIME-WAIT and CLOSED, where nothing is taken in and the
         * buffer goes back */
        if !matches!(inner.state, State::TimeWait | State::Closed) && owed(&inner) > inner.recv_buf.free() {
            return Some("the window offered reaches past the receive buffer");
        }
        None
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
    /// Segments -- SYNs, and any other -- from an address their port's
    /// listener does not answer, dropped
    pub syn_refused: usize,
    /// Connections given up half open, their ACK never come
    pub half_open: usize,
    /// Slots taken back for a new connection, every one taken, from
    /// connections their programs had let go of
    pub reclaimed: usize,
    /// Resets for no connection the bucket held back
    pub rst_held: usize,
    /// Of connections that take SACK: recoveries RACK started, timeouts and
    /// those found spurious, loss probes, recoveries undone, D-SACKs taken,
    /// peers that reneged
    pub recoveries: usize,
    pub timeouts: usize,
    pub spurious: usize,
    pub probes: usize,
    pub undos: usize,
    pub dsacks: usize,
    pub reneged: usize,
    /// What a receiver turned away: data segments from outside the window
    /// (old copies apart), segments with an acknowledgement RFC 5961 drops,
    /// and bytes in the window not kept -- of which those inside an edge
    /// offered that found no room, which must be none
    pub rx_outside: usize,
    pub rx_badack: usize,
    pub rx_unkept: usize,
    pub rx_overrun: usize,
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
    /// The congestion window: how much the sender lets be in flight, the
    /// peer's window allowing
    pub cwnd: usize,
    /// This end's shift and the peer's, on a connection that scales
    pub scales: Option<(u8, u8)>,
    /// When what is outstanding goes again (boot ms), 0 for never
    pub retransmit_at: u64,
    /// When a connection a listener took had its SYN (boot ms): what
    /// giving it up half open counts from
    pub opened_ms: u64,
    /// Its retransmission timeout of the moment, and the least the round
    /// trip may make it (`set_rto_floor`)
    pub rto_ms: u64,
    pub min_rto_ms: u64,
    /// Both SYNs said SACK; the recovery under way, if one is; the bytes in
    /// flight the peer holds past a hole, and those lost and not sent again
    pub sack: bool,
    pub recovery: Option<&'static str>,
    pub sacked: usize,
    pub lost: usize,
}

impl Tcp {
    /// The timer's task started: by the kernel's boot after `init`, and by
    /// a fuzzer at each input, whose machine boots with no thread. Pinned
    /// to CPU 0, as a waiter whose wakeups are to be prompt is: one moved
    /// as it blocks would be woken at the other CPU's next scheduling
    /// point. False when it could not be started.
    pub fn start_timer(&'static self) -> bool {
        if self.timer.event().is_none() {
            return false;
        }
        if self.timer_started.swap(true, Ordering::AcqRel) {
            return true;
        }
        match kcore::task::spawn_on_with("tcp/timer", 1, self, Tcp::timer_task) {
            Some(task) => {
                *self.timer_task.lock() = Some(task);
                true
            }
            None => {
                self.timer_started.store(false, Ordering::Release);
                klog!(0, "tcp: the timer's task could not be started");
                false
            }
        }
    }

    /// The timer's task stopped and waited for: what a fuzzer's input ends
    /// with, every task of its machine held to having ended. The kernel
    /// never stops it.
    pub fn stop_timer(&'static self) {
        let task = self.timer_task.lock().take();
        if let Some(task) = task {
            task.request_stop();
            if let Some(wake) = self.timer.event() {
                wake.signal();
            }
            drop(task);
            self.timer_started.store(false, Ordering::Release);
        }
    }

    /// The timer's task: asleep until a retransmission is due or the sweep
    /// is, whichever is first -- woken sooner by a deadline set before the
    /// one it sleeps to -- and the pass run then, here.
    fn timer_task(tcp: &'static Tcp) {
        let Some(wake) = tcp.timer.event() else { return };
        let mut sweep_at = 0;
        while !kcore::task::stopping() {
            let now = now_ms();
            if now >= sweep_at || now >= tcp.timer.due() {
                if now >= sweep_at {
                    sweep_at = now + SWEEP_MS;
                }
                tcp.process_retransmits();
                continue;
            }
            /* Both later than now: a deadline set meanwhile that is not has
             * signalled, and the wait returns at once. Saturating all the
             * same: a wrap here would be a sleep of ages, every
             * retransmission stopped without a word. */
            let until = sweep_at.min(tcp.timer.due());
            wake.wait_for(kcore::time::Duration::from_millis(until.saturating_sub(now)));
        }
    }
}
