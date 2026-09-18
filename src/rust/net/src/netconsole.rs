//! The kernel log over UDP: `netconsole=ip:port`.
//!
//! Every line the tracer produces is captured the moment it is produced --
//! from any context, a hard IRQ's included -- into a ring, and a drain task
//! ships it to the collector. While the network is down the lines simply
//! accumulate; the ring keeps the newest and counts what it had to drop.
//!
//! On a machine with no serial port this is the only console there is, and
//! the three things it is careful about are all scars:
//!
//! - Every datagram carries a sequence number, because a log that stops
//!   because the uplink dropped the rest of a burst looks exactly like a log
//!   that stops because the machine wedged, and the difference is the whole
//!   answer when debugging a hang.
//! - The drain is paced. A backlog sent as fast as the NIC takes it is tens
//!   of datagrams in a couple of milliseconds, and everything past the first
//!   few dies in the narrowest queue on the way -- which is the far end of
//!   the log, the part worth reading.
//! - Records are consumed only once a datagram has actually gone. Consuming
//!   them first turned any transmit hiccup into a silent blackout.
//!
//! The panic path writes and drains without taking the lock: another CPU may
//! hold it and is on its way to a halt, so waiting for it would spend the
//! panic rather than report it.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcore::net::Nic;
use kcore::sync::IrqSpinLock;
use kcore::task::TaskHandle;
use kcore::trace;

use crate::abi;
use crate::udp;

/// A record is a two-byte length and that many bytes of text; either may
/// straddle the wrap, so both go through the byte-wise push and pop.
const RECORD_HDR: usize = 2;
const MAX_RECORD: usize = 512;

/// The four bytes 'N','O','S','C' and a little-endian u32 sequence number,
/// counting datagrams from the first sent. A collector that does not know
/// the header treats the whole datagram as text, so an old one still works
/// -- it just cannot report gaps.
const DGRAM_HDR: usize = 8;
const MAGIC: [u8; 4] = *b"NOSC";

/// Enough for a full boot log, so a collector started late still gets it.
/// Everything traced before the device has an address lives here, and on a
/// twenty-CPU server that is most of a boot -- measured at 74 KiB in QEMU,
/// and a real machine prints more.
const RING_SIZE: usize = 1024 * 1024;

/// One datagram, header included; well under the MTU.
const DGRAM_MAX: usize = 1400;

const IDLE_POLL_MS: u64 = 2;
const NO_LINK_POLL_MS: u64 = 200;
/// After a datagram the device refused. The records stay in the ring, so
/// this is how fast a wedged transmit path is retried -- without it the
/// drain loop spins a CPU and shreds the backlog.
const TX_RETRY_MS: u64 = 20;
/// Between two datagrams that both had something to send.
const SEND_PACE_MS: u64 = 1;

/// What the panic path will try to push out, so a wedged ring cannot turn a
/// panic into an endless loop.
const PANIC_MAX_PACKETS: u32 = 64;
/// Refusals after which it gives up on the device. A ring that is merely
/// full drains while the same datagram is retried; one that is wedged never
/// will.
const PANIC_MAX_TX_RETRIES: u32 = 8;
/// How much undrained backlog the panic path keeps in front of the report.
const PANIC_BACKLOG_KEEP: usize = 8 * 1024;

struct Ring {
    buf: [u8; RING_SIZE],
    /// The oldest byte
    head: usize,
    /// Bytes in the ring
    used: usize,
    /// Records evicted because the ring was full
    dropped: usize,
}

impl Ring {
    fn push(&mut self, src: &[u8]) {
        let tail = (self.head + self.used) % RING_SIZE;
        let first = (RING_SIZE - tail).min(src.len());

        self.buf[tail..tail + first].copy_from_slice(&src[..first]);
        if src.len() > first {
            self.buf[..src.len() - first].copy_from_slice(&src[first..]);
        }
        self.used += src.len();
    }

    /// Takes `len` bytes off the head, into `dst` when there is one.
    fn pop(&mut self, mut len: usize, mut dst: Option<&mut [u8]>) {
        if len > self.used {
            len = self.used;
        }
        let first = (RING_SIZE - self.head).min(len);

        if let Some(dst) = dst.as_mut() {
            dst[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
            if len > first {
                dst[first..len].copy_from_slice(&self.buf[..len - first]);
            }
        }

        self.head = (self.head + len) % RING_SIZE;
        self.used -= len;
    }

    /// The length of the record at the head, when there is a whole one.
    fn head_record_len(&self) -> Option<usize> {
        if self.used < RECORD_HDR {
            return None;
        }
        let lo = self.buf[self.head] as usize;
        let hi = self.buf[(self.head + 1) % RING_SIZE] as usize;
        let len = lo | (hi << 8);

        if len + RECORD_HDR > self.used { None } else { Some(len) }
    }

    fn drop_oldest(&mut self) {
        match self.head_record_len() {
            Some(len) => {
                self.pop(RECORD_HDR + len, None);
                self.dropped += 1;
            }
            None => {
                /* Should not happen; start again rather than spin */
                self.head = 0;
                self.used = 0;
            }
        }
    }

    /// Drop whole records off the head until the leading `region` bytes are
    /// down to `keep`.
    fn trim_head(&mut self, mut region: usize, keep: usize) {
        while region > keep {
            let len = match self.head_record_len() {
                Some(len) => len,
                None => break,
            };
            let record = RECORD_HDR + len;
            if record > region {
                break;
            }
            self.pop(record, None);
            self.dropped += 1;
            region -= record;
        }
    }

    fn append(&mut self, text: &[u8]) {
        let need = RECORD_HDR + text.len();
        if need > RING_SIZE {
            return;
        }

        /* Full: the newest lines are the interesting ones, so the oldest
         * records go rather than what is being logged now. */
        while self.used + need > RING_SIZE {
            self.drop_oldest();
        }

        self.push(&[(text.len() & 0xFF) as u8, ((text.len() >> 8) & 0xFF) as u8]);
        self.push(text);
    }

    /// As many whole records as fit in `out`, without consuming them. What
    /// they occupy in the ring comes back with the length filled, and the
    /// caller pops that once the datagram is actually out.
    fn peek_batch(&self, out: &mut [u8]) -> (usize, usize) {
        let mut filled = 0;
        let mut consumed = 0;
        let mut at = self.head;
        let mut left = self.used;

        loop {
            if left < RECORD_HDR {
                break;
            }
            let lo = self.buf[at] as usize;
            let hi = self.buf[(at + 1) % RING_SIZE] as usize;
            let len = lo | (hi << 8);

            if len + RECORD_HDR > left || filled + len > out.len() {
                break;
            }

            at = (at + RECORD_HDR) % RING_SIZE;
            left -= RECORD_HDR;

            let first = (RING_SIZE - at).min(len);
            out[filled..filled + first].copy_from_slice(&self.buf[at..at + first]);
            if len > first {
                out[filled + first..filled + len].copy_from_slice(&self.buf[..len - first]);
            }

            at = (at + len) % RING_SIZE;
            left -= len;
            filled += len;
            consumed += RECORD_HDR + len;
        }

        (filled, consumed)
    }
}

pub struct Netconsole {
    lock: IrqSpinLock,
    ring: core::cell::UnsafeCell<Ring>,
    packet: core::cell::UnsafeCell<[u8; DGRAM_MAX]>,

    dst_ip: core::cell::UnsafeCell<u32>,
    dst_port: core::cell::UnsafeCell<u16>,
    src_port: core::cell::UnsafeCell<u16>,
    /// nctail=N: bytes of backlog to keep when the link first comes up,
    /// 0 to keep all of it.
    tail_keep: core::cell::UnsafeCell<usize>,
    backlog_trimmed: AtomicBool,

    nic: core::cell::UnsafeCell<Option<Nic>>,
    task: core::cell::UnsafeCell<Option<TaskHandle>>,
    /// The drain task's id, so its own messages are not captured
    drain_id: AtomicUsize,
    enabled: AtomicBool,

    sent: AtomicUsize,
    tx_failed: AtomicUsize,
    seq: core::cell::UnsafeCell<u32>,
    /// Bytes of backlog in front of a panic's report
    panic_backlog: core::cell::UnsafeCell<usize>,
}

/* Everything inside is touched with the lock held, or by the panic path,
 * which runs with every other CPU on its way to a halt */
unsafe impl Sync for Netconsole {}
unsafe impl Send for Netconsole {}

/// The one netconsole. A static, ring and all, rather than something made
/// on the heap: capture is armed from the kernel command line, which is long
/// before the page allocator exists -- and holding the boot log that happens
/// before then is the whole point. Its lock allocates nothing for the same
/// reason.
pub static NETCONSOLE: Netconsole = Netconsole {
    lock: IrqSpinLock::new(),
    ring: core::cell::UnsafeCell::new(
        Ring { buf: [0; RING_SIZE], head: 0, used: 0, dropped: 0 }),
    packet: core::cell::UnsafeCell::new([0; DGRAM_MAX]),
    dst_ip: core::cell::UnsafeCell::new(0),
    dst_port: core::cell::UnsafeCell::new(0),
    src_port: core::cell::UnsafeCell::new(0),
    tail_keep: core::cell::UnsafeCell::new(0),
    backlog_trimmed: AtomicBool::new(false),
    nic: core::cell::UnsafeCell::new(None),
    task: core::cell::UnsafeCell::new(None),
    drain_id: AtomicUsize::new(0),
    enabled: AtomicBool::new(false),
    sent: AtomicUsize::new(0),
    tx_failed: AtomicUsize::new(0),
    seq: core::cell::UnsafeCell::new(0),
    panic_backlog: core::cell::UnsafeCell::new(0),
};

impl Netconsole {
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Arm capture from the kernel command line. Safe long before the network
    /// exists; primes the ring with what the log already holds, so lines
    /// traced before this point are not lost.
    pub fn setup(&'static self) -> bool {
        if self.is_enabled() {
            return false;
        }

        let mut ip = 0u32;
        let mut port = 0u16;
        let mut tail_kb = 0usize;
        if unsafe { ffi::net::kernel_netconsole_params(&mut ip, &mut port, &mut tail_kb) } == 0 {
            return false;
        }

        unsafe {
            *self.dst_ip.get() = ip;
            *self.dst_port.get() = port;
            *self.src_port.get() = port;
            /* A cap at or above the ring is the same as no cap at all */
            *self.tail_keep.get() = if tail_kb != 0 {
                let bytes = tail_kb * 1024;
                if bytes < RING_SIZE { bytes } else { 0 }
            } else {
                0
            };
        }

        /* Everything traced before this point is still in the kernel log --
         * replay it into the ring so the collector sees the whole boot, not
         * just the tail. */
        unsafe { ffi::net::kernel_dmesg_replay(replay_line, self as *const _ as *mut u8) };

        self.enabled.store(true, Ordering::Release);
        trace!(0, "netconsole: capturing for {}.{}.{}.{}:{}, backlog cap {} bytes",
            (ip >> 24) & 0xFF, (ip >> 16) & 0xFF, (ip >> 8) & 0xFF, ip & 0xFF,
            port, unsafe { *self.tail_keep.get() });
        true
    }

    /// Attach a device and start the drain task.
    pub fn start(&'static self, nic: Nic) -> bool {
        if !self.is_enabled() || unsafe { (*self.task.get()).is_some() } {
            return false;
        }

        unsafe { *self.nic.get() = Some(nic) };

        let task = match kcore::task::spawn_with_ctx(
            "netcon", run, self as *const _ as *mut u8)
        {
            Some(task) => task,
            None => {
                unsafe { *self.nic.get() = None };
                return false;
            }
        };
        /* The id before it runs: `log` uses it to recognise, and skip, the
         * messages the transmit path itself produces. */
        self.drain_id.store(task.id(), Ordering::Release);
        unsafe { *self.task.get() = Some(task) };

        /* Say it in the stream, not just in the counters: a backlog that
         * overflowed before the link came up makes a log that starts in the
         * middle, and nothing about it says so. */
        let dropped = {
            let _guard = self.lock.lock();
            unsafe { (*self.ring.get()).dropped }
        };
        if dropped != 0 {
            trace!(0, "netconsole: {} lines were dropped before the link came up", dropped);
        }

        trace!(0, "netconsole: started");
        true
    }

    pub fn stop(&self) {
        let task = unsafe { (*self.task.get()).take() };
        if let Some(task) = task {
            task.request_stop();
            drop(task);
        }
        self.drain_id.store(0, Ordering::Release);
        unsafe { *self.nic.get() = None };
    }

    /// The capture hook, called for every message the tracer produces and
    /// from the panic printer. Safe at any IRQ level.
    pub fn log(&self, text: &[u8]) {
        if !self.is_enabled() || text.is_empty() {
            return;
        }

        /* Skip whatever the drain task produces, IRQs taken on top of it
         * included: the transmit path traces, and capturing that would make
         * the loop generate its own work forever. Those lines still reach
         * the kernel log and the console. */
        let drain = self.drain_id.load(Ordering::Acquire);
        if drain != 0 && kcore::task::current_id() == drain {
            return;
        }

        let text = &text[..text.len().min(MAX_RECORD)];

        /* A panic runs with interrupts off and the other CPUs on their way
         * to a halt -- one of them may hold the lock and never release it,
         * so the panic path writes unlocked rather than deadlocking. */
        if unsafe { ffi::net::kernel_panic_active() } != 0 {
            unsafe { (*self.ring.get()).append(text) };
            return;
        }

        let _guard = self.lock.lock();
        unsafe { (*self.ring.get()).append(text) };
    }

    /// One datagram: the header, then the text already placed after it.
    /// Only a datagram the device took gets a number, so a gap in the
    /// sequence at the collector means the network lost it.
    fn send_batch(&self, text_len: usize) -> bool {
        let nic = match unsafe { *self.nic.get() } {
            Some(nic) => nic,
            None => return false,
        };
        if text_len == 0 {
            return false;
        }

        let (dst_ip, dst_port, src_port) = unsafe {
            (*self.dst_ip.get(), *self.dst_port.get(), *self.src_port.get())
        };
        let arp = match abi::arp_table() {
            Some(arp) => arp,
            None => return false,
        };

        let packet = unsafe { &mut *self.packet.get() };
        packet[..4].copy_from_slice(&MAGIC);
        let seq = unsafe { *self.seq.get() };
        packet[4..8].copy_from_slice(&seq.to_le_bytes());

        if !udp::send(&nic, arp, dst_ip, dst_port, nic.ip(), src_port,
            &packet[..DGRAM_HDR + text_len])
        {
            return false;
        }

        unsafe { *self.seq.get() = seq.wrapping_add(1) };
        true
    }

    fn run(&self) {
        while !kcore::task::stopping() {
            /* No address yet -- DHCP still running, or no static one set:
             * keep buffering, the backlog goes out as soon as there is one. */
            let has_link = match unsafe { *self.nic.get() } {
                Some(nic) => nic.ip() != 0,
                None => false,
            };
            if !has_link {
                kcore::task::sleep_ms(NO_LINK_POLL_MS);
                continue;
            }

            self.trim_backlog_once();

            let (len, consumed, dropped) = {
                let _guard = self.lock.lock();
                let ring = unsafe { &*self.ring.get() };
                let packet = unsafe { &mut *self.packet.get() };
                let (len, consumed) = ring.peek_batch(&mut packet[DGRAM_HDR..]);
                (len, consumed, ring.dropped)
            };

            if len == 0 {
                kcore::task::sleep_ms(IDLE_POLL_MS);
                continue;
            }

            if !self.send_batch(len) {
                /* The records are still in the ring. On a machine whose only
                 * console is this one, consuming them first turned any
                 * transmit hiccup into a silent blackout: the loop has no
                 * sleep while the ring is not empty, so it shredded the whole
                 * backlog at full speed, and every line traced afterwards
                 * with it. */
                self.tx_failed.fetch_add(1, Ordering::Relaxed);
                kcore::task::sleep_ms(TX_RETRY_MS);
                continue;
            }
            self.sent.fetch_add(1, Ordering::Relaxed);

            {
                let _guard = self.lock.lock();
                let ring = unsafe { &mut *self.ring.get() };
                /* An append may have evicted from the head while the lock
                 * was down, in which case what was just sent is already gone
                 * and popping again would eat live records. The drop count
                 * is the only other thing that moves the head. */
                if ring.dropped == dropped {
                    ring.pop(consumed, None);
                }
            }

            kcore::task::sleep_ms(SEND_PACE_MS);
        }
    }

    /// The first moment there is anywhere to send to. With `nctail=N` the
    /// boot log queued behind this point is cut down to the newest N KiB: on
    /// a machine that wedges seconds later, the whole network it will ever
    /// get goes on the lines nearest the wedge instead of on the head of the
    /// log, which is the part already understood.
    fn trim_backlog_once(&self) {
        if self.backlog_trimmed.load(Ordering::Acquire) {
            return;
        }

        let keep = unsafe { *self.tail_keep.get() };
        let trimmed = {
            let _guard = self.lock.lock();
            let ring = unsafe { &mut *self.ring.get() };
            let before = ring.dropped;
            if keep != 0 {
                let used = ring.used;
                ring.trim_head(used, keep);
            }
            ring.dropped - before
        };
        self.backlog_trimmed.store(true, Ordering::Release);

        if trimmed == 0 {
            return;
        }

        /* This one line has to reach the collector, and `log` drops whatever
         * the drain task produces -- so it goes into the ring by hand, ahead
         * of the backlog it is explaining. Every other drain-task message is
         * suppressed for a good reason; this one is emitted exactly once,
         * and it is the difference between a log that starts in the middle
         * and a log that says why. */
        let mut msg = [0u8; MAX_RECORD];
        let len = format_trimmed(&mut msg, trimmed, keep);
        {
            let _guard = self.lock.lock();
            unsafe { (*self.ring.get()).append(&msg[..len]) };
        }

        trace!(0, "netconsole: link up, dropped {} backlog msgs over the {} byte cap",
            trimmed, keep);
    }

    /// Called once a panic has started, before anything is printed:
    /// remembers how much undrained backlog sits in front of the report.
    pub fn panic_mark(&self) {
        if !self.is_enabled() {
            return;
        }
        /* Unlocked on purpose -- see `log` */
        unsafe { *self.panic_backlog.get() = (*self.ring.get()).used };
    }

    /// A best-effort drain from panic context: no locks, and only when the
    /// collector is already in the ARP cache -- with the other CPUs halted
    /// nothing would ever deliver a reply, so resolving would just burn the
    /// panic.
    pub fn panic_flush(&self) {
        if !self.is_enabled() {
            return;
        }
        let nic = match unsafe { *self.nic.get() } {
            Some(nic) if nic.ip() != 0 => nic,
            _ => return,
        };

        let arp = match abi::arp_table() {
            Some(arp) => arp,
            None => return,
        };
        let dst_ip = unsafe { *self.dst_ip.get() };
        if arp.lookup(nic.route_ip(dst_ip)).is_none() {
            return;
        }

        /* The report is at the tail, behind whatever the drain task had not
         * shipped. A machine that dies just after DHCP has the entire boot
         * log in front of it -- far more than this will carry -- so the old
         * end of that backlog goes, keeping a little for context. */
        unsafe {
            let ring = &mut *self.ring.get();
            let backlog = (*self.panic_backlog.get()).min(ring.used);
            ring.trim_head(backlog, PANIC_BACKLOG_KEEP);
        }

        let mut failures = 0;
        for _ in 0..PANIC_MAX_PACKETS {
            /* Unlocked on purpose -- see `log` */
            let (len, consumed) = unsafe {
                let ring = &*self.ring.get();
                let packet = &mut *self.packet.get();
                ring.peek_batch(&mut packet[DGRAM_HDR..])
            };
            if len == 0 {
                break;
            }

            if self.send_batch(len) {
                self.sent.fetch_add(1, Ordering::Relaxed);
                unsafe { (*self.ring.get()).pop(consumed, None) };
                failures = 0;
                continue;
            }

            /* Retry the same datagram: a transmit ring that is only full
             * drains while this spins. Give up before the report is spent on
             * a dead one. */
            self.tx_failed.fetch_add(1, Ordering::Relaxed);
            failures += 1;
            if failures >= PANIC_MAX_TX_RETRIES {
                break;
            }
        }
    }

    /// What the `netconsole` command reports.
    pub fn stats(&self) -> Stats {
        let _guard = self.lock.lock();
        let ring = unsafe { &*self.ring.get() };
        unsafe {
            Stats {
                enabled: self.is_enabled() as u32,
                dst_ip: *self.dst_ip.get(),
                dst_port: *self.dst_port.get(),
                src_port: *self.src_port.get(),
                attached: (*self.nic.get()).is_some() as u32,
                used: ring.used,
                capacity: RING_SIZE,
                dropped: ring.dropped,
                sent: self.sent.load(Ordering::Relaxed),
                tx_failed: self.tx_failed.load(Ordering::Relaxed),
                seq: *self.seq.get(),
                tail_keep: *self.tail_keep.get(),
                trimmed: self.backlog_trimmed.load(Ordering::Acquire) as u32,
            }
        }
    }
}

/// What the `netconsole` command prints. The C++ side declares the same
/// struct.
#[repr(C)]
pub struct Stats {
    pub enabled: u32,
    pub dst_ip: u32,
    pub dst_port: u16,
    pub src_port: u16,
    pub attached: u32,
    pub used: usize,
    pub capacity: usize,
    pub dropped: usize,
    pub sent: usize,
    pub tx_failed: usize,
    pub seq: u32,
    pub tail_keep: usize,
    pub trimmed: u32,
}

/// "netconsole: link up, dropped N backlog msgs over the M byte cap\n",
/// written by hand because this runs where a formatter's allocation would
/// not be welcome.
fn format_trimmed(out: &mut [u8], dropped: usize, keep: usize) -> usize {
    let mut at = put(out, 0, b"netconsole: link up, dropped ");
    at = put_num(out, at, dropped as u64);
    at = put(out, at, b" backlog msgs over the ");
    at = put_num(out, at, keep as u64);
    at = put(out, at, b" byte cap\n");
    at
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) -> usize {
    let take = bytes.len().min(out.len().saturating_sub(at));
    out[at..at + take].copy_from_slice(&bytes[..take]);
    at + take
}

fn put_num(out: &mut [u8], at: usize, mut value: u64) -> usize {
    let mut digits = [0u8; 20];
    let mut n = digits.len();
    loop {
        n -= 1;
        digits[n] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    put(out, at, &digits[n..])
}

/// Each message the kernel log already held, as `setup` replays them.
extern "C" fn replay_line(ctx: *mut u8, s: *const u8, len: usize) {
    if ctx.is_null() || s.is_null() || len == 0 {
        return;
    }
    let netconsole = unsafe { &*(ctx as *const Netconsole) };
    let text = unsafe { core::slice::from_raw_parts(s, len.min(MAX_RECORD)) };

    let _guard = netconsole.lock.lock();
    unsafe { (*netconsole.ring.get()).append(text) };
}

/// The task the drain runs in.
extern "C" fn run(ctx: *mut u8) {
    if ctx.is_null() {
        return;
    }
    let netconsole = unsafe { &*(ctx as *const Netconsole) };
    netconsole.run();
}
