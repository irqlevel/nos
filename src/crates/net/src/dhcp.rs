//! DHCP: how the machine gets its address, keeps it -- and lets it go.
//!
//! A task does DISCOVER, REQUEST, and then keeps the lease as RFC 2131 4.4.5
//! has a client keep one: from T1, half its time, it asks the server that
//! granted it to renew it -- unicast, as the RFC asks, falling back to
//! broadcast when the server's Ethernet address cannot be resolved; from T2,
//! seven eighths of its time, any server, broadcast, which is rebinding;
//! each again at half the time left. When a server refuses, or the time is
//! up, the address comes off the device and the task starts again from
//! DISCOVER: a machine that went on using an address past its lease would be
//! answering for somebody else's.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::nic::{Lent, Nic, RxContext, UdpHandler, UdpListener};
use kcore::sync::SpinLock;
use kcore::task::TaskHandle;
use kcore::trace;

use crate::abi;
use crate::wire::{eth, ip, udp, Ipv4, Mac, ETH_HDR_LEN, ETH_TYPE_IP, IP_HDR_LEN,
                  IP_PROTO_UDP, MAC_BROADCAST, UDP_HDR_LEN};

pub const CLIENT_PORT: u16 = 68;
pub const SERVER_PORT: u16 = 67;

/// The fixed part of a DHCP message, before the options.
const PACKET_LEN: usize = 236;
const MAGIC_COOKIE: u32 = 0x6382_5363;

/* Offsets within the DHCP message */
const OP: usize = 0;
const HTYPE: usize = 1;
const HLEN: usize = 2;
const XID: usize = 4;
const FLAGS: usize = 10;
const CIADDR: usize = 12;
const YIADDR: usize = 16;
const CHADDR: usize = 28;

const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;
const FLAG_BROADCAST: u16 = 0x8000;

/* Option codes */
const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MESSAGE_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_PARAM_REQUEST: u8 = 55;
const OPT_END: u8 = 255;

/* Message types */
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;

/// What a built message fits in.
const MAX_FRAME: usize = 600;
/// What a received one is kept in.
const RX_MAX: usize = 1500;

const EXCHANGE_TIMEOUT_MS: u64 = 3000;
const POLL_INTERVAL_MS: u64 = 10;
const TRIES: u32 = 3;
/// Between two tries at getting a lease, and after the last of them.
const TRY_GAP_MS: u64 = 2000;
const NO_LEASE_MS: u64 = 5000;

/// The shortest lease the client keeps to: a shorter one is taken as this,
/// as dhcpcd takes one, so that a server that says 0 does not have the
/// client asking again and again. T1 is then ten seconds, which was always
/// the least it waited.
const MIN_LEASE_SECS: u32 = 20;
/// A lease with no end (RFC 2131 3.3).
const INFINITE_LEASE: u32 = 0xFFFF_FFFF;
/// The least time between two tries at renewing, or at rebinding. RFC 2131
/// 4.4.5 says sixty seconds, which a lease of the least length would never
/// get to try twice.
const RETRY_MIN_MS: u64 = 10_000;
/// How long a wait for T1 sleeps at a time: how soon a stop is heard.
const WAIT_STEP_MS: u64 = 1000;

/// What the lease turned out to be.
#[derive(Clone, Copy, Default)]
pub struct Lease {
    pub ip: u32,
    pub mask: u32,
    pub router: u32,
    pub dns: u32,
    pub server_ip: u32,
    pub lease_secs: u32,
}

/// When a lease is renewed, rebound and let go of: milliseconds since boot,
/// counted from when the REQUEST that got it went out (RFC 2131 4.4.1).
struct Times {
    t1: u64,
    t2: u64,
    end: u64,
}

impl Times {
    /// None for a lease with no end.
    fn of(start_ms: u64, secs: u32) -> Option<Times> {
        if secs == INFINITE_LEASE {
            return None;
        }
        let len = u64::from(secs.max(MIN_LEASE_SECS)) * 1000;
        Some(Times { t1: start_ms + len / 2, t2: start_ms + len / 8 * 7, end: start_ms + len })
    }
}

/// What a REQUEST is for (RFC 2131 4.3.2).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// The address just offered, of the server that offered it.
    Select,
    /// The lease held, of the server that granted it.
    Renew,
    /// The lease held, of any server.
    Rebind,
}

struct Received {
    buf: [u8; RX_MAX],
    len: usize,
    ready: bool,
}

/// What the task, the listener and the shell all reach: one lock over all
/// of it. None of it is on a path where the lock could matter.
struct State {
    rx: Received,
    /// What the last ACK said -- never what an offer did.
    lease: Lease,
    offered_ip: u32,
    server_id: u32,
    xid: u32,
    nic: Option<Nic>,
}

pub struct Dhcp {
    state: SpinLock<State>,
    /* The two things that must never be dropped under a lock: giving a port
     * back waits for the receive path to leave the callback -- which takes
     * `state` -- and giving a task back waits for the task. Each is taken out
     * under its lock and let go of after. */
    listener: SpinLock<Option<UdpListener>>,
    task: SpinLock<Option<TaskHandle>>,

    naked: AtomicBool,
    ready: AtomicBool,
    running: AtomicBool,
}

fn now_ms() -> u64 {
    kcore::time::boot_time().as_nanos() / kcore::consts::NS_PER_MS
}

/// Sleeps until `deadline`, in milliseconds since boot, a second at a time
/// so that a stop is heard: false when the task is asked to stop first.
fn sleep_until(deadline: u64) -> bool {
    loop {
        if kcore::task::stopping() {
            return false;
        }
        let now = now_ms();
        if now >= deadline {
            return true;
        }
        kcore::task::sleep_ms((deadline - now).min(WAIT_STEP_MS));
    }
}

impl Dhcp {
    pub fn new() -> Option<Dhcp> {
        Some(Dhcp {
            state: SpinLock::new(State {
                rx: Received { buf: [0; RX_MAX], len: 0, ready: false },
                lease: Lease::default(),
                offered_ip: 0,
                server_id: 0,
                xid: 0,
                nic: None,
            })?,
            listener: SpinLock::new(None)?,
            task: SpinLock::new(None)?,
            naked: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            running: AtomicBool::new(false),
        })
    }

    /// Whether the client holds a lease: bound, renewing or rebinding.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn lease(&self) -> Lease {
        self.state.lock().lease
    }

    /// Start the client on `nic`. False when one is running already.
    pub fn start(&'static self, nic: Nic) -> bool {
        if self.running.swap(true, Ordering::AcqRel) {
            return false;
        }

        {
            let mut state = self.state.lock();
            state.nic = Some(nic);
            /* The transaction id is the client's own; the entropy pool is a
             * better source than the boot time the C++ used, and by this
             * point it has been fed. */
            state.xid = kcore::random::random_u64().unwrap_or(0x1234_5678) as u32;
        }
        self.ready.store(false, Ordering::Release);

        match kcore::task::spawn_for("dhcp", self, Dhcp::run) {
            Some(task) => {
                *self.task.lock() = Some(task);
                true
            }
            None => {
                self.running.store(false, Ordering::Release);
                trace!(0, "dhcp: no task to run in");
                false
            }
        }
    }

    /// Stop the client and give up the port. Waits for the task to leave.
    /// The address stays on the device -- the way down still logs over it
    /// -- but nobody keeps its lease any more, so the client is not ready.
    pub fn stop(&self) {
        let task = self.task.lock().take();
        if let Some(task) = task {
            task.request_stop();
            /* Dropping the handle waits for the task and releases it */
            drop(task);
        }

        self.unlisten();
        self.state.lock().nic = None;
        self.ready.store(false, Ordering::Release);
        self.running.store(false, Ordering::Release);
    }

    fn nic(&self) -> Option<Nic> {
        self.state.lock().nic
    }

    fn listen(&'static self) {
        if self.listener.lock().is_some() {
            return;
        }
        let nic = match self.nic() {
            Some(nic) => nic,
            None => return,
        };

        match nic.listen(CLIENT_PORT, self) {
            Ok(listener) => *self.listener.lock() = Some(listener),
            Err(err) => trace!(0, "dhcp: port {} could not be listened on ({:?})",
                CLIENT_PORT, err),
        }
    }

    fn unlisten(&self) {
        /* Dropping the listener takes it off the port and returns once no
         * call of the callback is still running -- so it is dropped with the
         * lock down, which the statement's end sees to. */
        let listener = self.listener.lock().take();
        drop(listener);
    }

    /* ---- the exchange ---- */

    /// Forget any earlier answer *before* a request goes out, never after:
    /// the answer can be back before the send returns. The receive softirq
    /// may run on this CPU on the way out of the transmit path, and QEMU's
    /// devices answer within the doorbell's write -- a flag cleared after
    /// the send threw every one of those answers away.
    fn arm(&self) {
        self.state.lock().rx.ready = false;
        self.naked.store(false, Ordering::Release);
    }

    /// A received datagram on the client port, from the receive softirq.
    /// Only an answer in this client's transaction is kept: DHCP answers are
    /// broadcast, and another client's -- or a stranger's -- must not take
    /// the one place this client's own answer needs.
    fn receive(&self, frame: &[u8]) {
        let msg = match udp::parse(frame) {
            Some(datagram) => datagram.payload,
            None => return,
        };
        if msg.len() < PACKET_LEN + 4 || msg[OP] != BOOTREPLY {
            return;
        }
        let xid = u32::from_be_bytes([msg[XID], msg[XID + 1], msg[XID + 2], msg[XID + 3]]);

        let mut state = self.state.lock();
        let mine = state.nic.is_some_and(|nic| msg[CHADDR..CHADDR + 6] == nic.mac());
        if xid != state.xid || !mine {
            return;
        }
        let rx = &mut state.rx;

        if rx.ready {
            /* The one before it has not been looked at yet */
            return;
        }

        let len = frame.len().min(RX_MAX);
        rx.buf[..len].copy_from_slice(&frame[..len]);
        rx.len = len;
        rx.ready = true;
    }

    /// Waits for a message of `want`; false once the timeout passes, at
    /// once when the server says no, and when the task is asked to stop.
    fn wait_for(&self, want: u8, timeout_ms: u64) -> bool {
        let mut left = timeout_ms;

        while left > 0 && !kcore::task::stopping() {
            let mut frame = [0u8; RX_MAX];
            let mut len = 0;
            {
                let mut state = self.state.lock();
                let rx = &mut state.rx;
                if rx.ready {
                    len = rx.len;
                    frame[..len].copy_from_slice(&rx.buf[..len]);
                    rx.ready = false;
                }
            }

            if len != 0 {
                if self.parse(&frame[..len], want) {
                    return true;
                }
                /* A NAK for this transaction means the lease was refused:
                 * start again rather than wait out the whole timeout. */
                if self.naked.load(Ordering::Acquire) {
                    return false;
                }
            }

            kcore::task::sleep_ms(POLL_INTERVAL_MS);
            left = left.saturating_sub(POLL_INTERVAL_MS);
        }

        false
    }

    fn discover(&self) -> bool {
        let nic = match self.nic() {
            Some(nic) => nic,
            None => return false,
        };

        let mut frame = [0u8; MAX_FRAME];
        let len = self.build_discover(&nic, &mut frame);

        self.arm();
        nic.send_raw(&frame[..len]);
        self.wait_for(OFFER, EXCHANGE_TIMEOUT_MS)
    }

    /// A REQUEST, and up to `timeout_ms` for its answer.
    fn request(&self, ask: Ask, timeout_ms: u64) -> bool {
        let nic = match self.nic() {
            Some(nic) => nic,
            None => return false,
        };

        let mut frame = [0u8; MAX_FRAME];
        let len = self.build_request(&nic, &mut frame, ask);

        self.arm();
        nic.send_raw(&frame[..len]);
        self.wait_for(ACK, timeout_ms)
    }

    /* ---- building ---- */

    /// The headers and the fixed part, shared by both messages. Answers
    /// where the options begin.
    fn build_head(
        &self, nic: &Nic, frame: &mut [u8], dst_mac: &Mac, src_ip: u32, dst_ip: u32,
        options_len: usize, flags: u16, ciaddr: u32,
    ) -> usize {
        let dhcp_len = PACKET_LEN + 4 + options_len;
        let datagram_len = UDP_HDR_LEN + dhcp_len;

        eth::write(frame, dst_mac, &nic.mac(), ETH_TYPE_IP);
        ip::write(&mut frame[ETH_HDR_LEN..], IP_PROTO_UDP, src_ip, dst_ip, datagram_len, 0);
        /* DHCP goes out with a longer time to live than the stack's
         * default, as every client does -- so the checksum ip::write left
         * has to be taken again over the changed header. */
        frame[ETH_HDR_LEN + ip::TTL] = 128;
        recompute_ip_checksum(&mut frame[ETH_HDR_LEN..]);

        let at = ETH_HDR_LEN + IP_HDR_LEN;
        udp::write(&mut frame[at..], CLIENT_PORT, SERVER_PORT, dhcp_len);

        let msg = at + UDP_HDR_LEN;
        frame[msg + OP] = BOOTREQUEST;
        frame[msg + HTYPE] = 1; /* Ethernet */
        frame[msg + HLEN] = 6;
        let xid = self.state.lock().xid;
        frame[msg + XID..msg + XID + 4].copy_from_slice(&xid.to_be_bytes());
        frame[msg + FLAGS..msg + FLAGS + 2].copy_from_slice(&flags.to_be_bytes());
        frame[msg + CIADDR..msg + CIADDR + 4].copy_from_slice(&ciaddr.to_be_bytes());
        frame[msg + CHADDR..msg + CHADDR + 6].copy_from_slice(&nic.mac());

        let options = msg + PACKET_LEN;
        frame[options..options + 4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
        options + 4
    }

    fn build_discover(&self, nic: &Nic, frame: &mut [u8]) -> usize {
        /* type(3) + parameter request(5) + end(1) */
        let options_len = 3 + 5 + 1;
        let mut at = self.build_head(
            nic, frame, &MAC_BROADCAST, 0, 0xFFFF_FFFF, options_len,
            FLAG_BROADCAST, 0);

        at = put_option(frame, at, OPT_MESSAGE_TYPE, &[DISCOVER]);
        at = put_option(frame, at, OPT_PARAM_REQUEST,
            &[OPT_SUBNET_MASK, OPT_ROUTER, OPT_DNS]);
        frame[at] = OPT_END;
        at + 1
    }

    fn build_request(&self, nic: &Nic, frame: &mut [u8], ask: Ask) -> usize {
        let (lease, offered, server) = {
            let state = self.state.lock();
            (state.lease, state.offered_ip, state.server_id)
        };

        /* Selecting (after an offer): the address asked for and the server
         * it came from. Renewing or rebinding: neither, and the address
         * carried in ciaddr instead -- some servers refuse or ignore a
         * renewal that still carries options 50 and 54 (RFC 2131). */
        let selecting = ask == Ask::Select;
        let options_len = if selecting { 3 + 6 + 6 + 1 } else { 3 + 1 };

        /* RFC 2131 4.3.2: a renewal is unicast to the server that granted
         * the lease; with no answer for its address it goes to everyone, as
         * rebinding does. */
        let mut dst_mac = MAC_BROADCAST;
        let mut unicast = false;
        if ask == Ask::Renew && lease.server_ip != 0 {
            if let Some(arp) = abi::arp_table() {
                if let Some(mac) = arp.resolve(nic, nic.route_ip(lease.server_ip)) {
                    dst_mac = mac;
                    unicast = true;
                }
            }
        }

        let held = if selecting { 0 } else { lease.ip };
        let mut at = self.build_head(
            nic, frame, &dst_mac,
            held,
            if unicast { lease.server_ip } else { 0xFFFF_FFFF },
            options_len,
            /* Renewing or rebinding, this machine holds a routable address,
             * so the server can answer it directly: no broadcast flag. */
            if selecting { FLAG_BROADCAST } else { 0 },
            held);

        at = put_option(frame, at, OPT_MESSAGE_TYPE, &[REQUEST]);
        if selecting {
            at = put_option(frame, at, OPT_REQUESTED_IP, &offered.to_be_bytes());
            at = put_option(frame, at, OPT_SERVER_ID, &server.to_be_bytes());
        }
        frame[at] = OPT_END;
        at + 1
    }

    /* ---- parsing ---- */

    /// A received message: true when it is the kind being waited for, and
    /// what it said has been taken from it.
    fn parse(&self, frame: &[u8], want: u8) -> bool {
        let nic = match self.nic() {
            Some(nic) => nic,
            None => return false,
        };

        let datagram = match udp::parse(frame) {
            Some(datagram) => datagram,
            None => return false,
        };
        let msg = datagram.payload;
        if msg.len() < PACKET_LEN + 4 {
            return false;
        }

        let xid = u32::from_be_bytes([msg[XID], msg[XID + 1], msg[XID + 2], msg[XID + 3]]);
        if xid != self.state.lock().xid {
            return false;
        }
        if msg[CHADDR..CHADDR + 6] != nic.mac() {
            return false;
        }
        if msg[OP] != BOOTREPLY {
            return false;
        }

        /* The offered address stays in a local until the message has been
         * judged: a NAK, or one of the wrong kind, must not overwrite a good
         * offer that the request still needs. */
        let yiaddr = u32::from_be_bytes(
            [msg[YIADDR], msg[YIADDR + 1], msg[YIADDR + 2], msg[YIADDR + 3]]);

        let options = &msg[PACKET_LEN..];
        let cookie = u32::from_be_bytes([options[0], options[1], options[2], options[3]]);
        if cookie != MAGIC_COOKIE {
            return false;
        }

        let mut kind = 0;
        let mut found = Lease { ip: yiaddr, ..Lease::default() };

        let options = &options[4..];
        let mut at = 0;
        while at < options.len() {
            let code = options[at];
            if code == OPT_END {
                break;
            }
            if code == 0 {
                /* padding */
                at += 1;
                continue;
            }
            if at + 1 >= options.len() {
                break;
            }
            let len = options[at + 1] as usize;
            if at + 2 + len > options.len() {
                break;
            }
            let data = &options[at + 2..at + 2 + len];

            match code {
                OPT_MESSAGE_TYPE if len >= 1 => kind = data[0],
                OPT_SUBNET_MASK if len >= 4 => found.mask = be32(data),
                OPT_ROUTER if len >= 4 => found.router = be32(data),
                OPT_DNS if len >= 4 => found.dns = be32(data),
                OPT_SERVER_ID if len >= 4 => found.server_ip = be32(data),
                OPT_LEASE_TIME if len >= 4 => found.lease_secs = be32(data),
                _ => {}
            }

            at += 2 + len;
        }

        /* The server refused: the waiter starts again from DISCOVER. */
        if kind == NAK {
            self.naked.store(true, Ordering::Release);
            return false;
        }
        if kind != want {
            return false;
        }
        /* An address no host may have is no lease, whoever offers it: a
         * server that offers one is broken, or is not a server, and taking it
         * would have the machine send from an address no packet may come
         * from. */
        if !crate::wire::host_address(yiaddr, found.mask) {
            trace!(0, "dhcp: {} offered {} mask {}, which no host may have: ignored",
                Ipv4(found.server_ip), Ipv4(yiaddr), Ipv4(found.mask));
            return false;
        }

        let mut state = self.state.lock();
        if kind == OFFER {
            state.offered_ip = yiaddr;
            state.server_id = found.server_ip;
        } else {
            state.lease = found;
        }
        true
    }

    fn next_transaction(&self) {
        let mut state = self.state.lock();
        state.xid = state.xid.wrapping_add(1);
    }

    /* ---- the lease's life ---- */

    fn run(&'static self) {
        while !kcore::task::stopping() {
            match self.acquire() {
                Some(start) => self.keep(start),
                None => {
                    trace!(0, "dhcp: no lease");
                    sleep_until(now_ms() + NO_LEASE_MS);
                }
            }
        }
    }

    /// INIT to BOUND: a DISCOVER, then a REQUEST for what was offered, a few
    /// times over. When the lease it got starts -- when its REQUEST went out
    /// -- or None.
    fn acquire(&'static self) -> Option<u64> {
        self.listen();

        let mut start = None;
        for _ in 0..TRIES {
            if kcore::task::stopping() {
                break;
            }
            self.next_transaction();

            if self.discover() {
                let sent = now_ms();
                if self.request(Ask::Select, EXCHANGE_TIMEOUT_MS) {
                    start = Some(sent);
                    break;
                }
            }
            if !sleep_until(now_ms() + TRY_GAP_MS) {
                break;
            }
        }

        self.unlisten();
        if start.is_some() {
            self.bind("bound");
        }
        start
    }

    /// BOUND, RENEWING and REBINDING (RFC 2131 4.4.5): the lease renewed by
    /// its server from T1 and by any server from T2, each tried again at
    /// half the time left; and let go of when a server refuses it or its time
    /// runs out. Returns when it has been, or the task is asked to stop.
    fn keep(&'static self, mut start: u64) {
        'lease: loop {
            let times = match Times::of(start, self.lease().lease_secs) {
                Some(times) => times,
                None => {
                    /* A lease with no end: nothing to do but be stopped. */
                    while sleep_until(now_ms() + WAIT_STEP_MS) {}
                    return;
                }
            };

            let mut next = times.t1;
            loop {
                if !sleep_until(next) {
                    return;
                }
                let now = now_ms();
                if now >= times.end {
                    self.let_go("the lease ran out");
                    return;
                }

                let ask = if now >= times.t2 { Ask::Rebind } else { Ask::Renew };
                trace!(0, "dhcp: {}", if ask == Ask::Renew { "renewing" } else { "rebinding" });
                self.listen();
                self.next_transaction();
                /* Not waited for past the lease's end, which comes when it
                 * comes, answer or none. */
                let renewed = self.request(ask, EXCHANGE_TIMEOUT_MS.min(times.end - now));
                let refused = self.naked.load(Ordering::Acquire);
                self.unlisten();

                if renewed {
                    self.bind("renewed");
                    start = now;
                    continue 'lease;
                }
                if refused {
                    self.let_go("the renewal was refused");
                    return;
                }
                /* No answer: again at half the time left before T2 -- or,
                 * rebinding, before the end. */
                let limit = if ask == Ask::Renew { times.t2 } else { times.end };
                let now = now_ms();
                next = (now + limit.saturating_sub(now) / 2).max(now + RETRY_MIN_MS).min(limit);
            }
        }
    }

    /// The lease the last ACK gave, onto the device -- address, mask and
    /// gateway, all three every time, since a renewal may bring a new mask
    /// or router -- and the client ready.
    fn bind(&self, what: &str) {
        let lease = self.lease();
        if let Some(nic) = self.nic() {
            nic.set_ip(lease.ip);
            nic.set_mask(lease.mask);
            nic.set_gw(lease.router);
        }
        self.ready.store(true, Ordering::Release);

        trace!(0, "dhcp: {} ip {} mask {} gw {} lease {}", what, Ipv4(lease.ip),
            Ipv4(lease.mask), Ipv4(lease.router), lease.lease_secs);
    }

    /// The lease is gone: the client is not ready, and its address comes off
    /// the device, so that nothing more goes out from an address that is
    /// somebody else's to have now.
    fn let_go(&self, why: &str) {
        self.ready.store(false, Ordering::Release);
        let lease = self.lease();
        if let Some(nic) = self.nic() {
            if nic.ip() == lease.ip {
                nic.set_ip(0);
                nic.set_mask(0);
                nic.set_gw(0);
            }
        }
        trace!(0, "dhcp: {}: {} let go", why, Ipv4(lease.ip));
    }
}

fn be32(data: &[u8]) -> u32 {
    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
}

/// One option: its code, its length and its bytes. Answers where the next
/// one goes.
fn put_option(frame: &mut [u8], at: usize, code: u8, data: &[u8]) -> usize {
    frame[at] = code;
    frame[at + 1] = data.len() as u8;
    frame[at + 2..at + 2 + data.len()].copy_from_slice(data);
    at + 2 + data.len()
}

/// The IP checksum again, over a header whose time to live has been changed
/// since `ip::write` left one.
fn recompute_ip_checksum(packet: &mut [u8]) {
    packet[ip::CHECKSUM] = 0;
    packet[ip::CHECKSUM + 1] = 0;
    let sum = crate::wire::checksum(&packet[..IP_HDR_LEN]);
    packet[ip::CHECKSUM..ip::CHECKSUM + 2].copy_from_slice(&sum.to_be_bytes());
}

/// What the receive path hands every datagram on the client port to.
impl UdpHandler for Dhcp {
    fn on_frame(&'static self, frame: Lent<'_>, _rx: &mut RxContext) {
        self.receive(frame.bytes());
    }
}
