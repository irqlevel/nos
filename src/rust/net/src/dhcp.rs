//! DHCP: how the machine gets its address, and keeps it.
//!
//! A task does DISCOVER, REQUEST, and then sleeps until half the lease is
//! gone and renews. A renewal is unicast to the server that granted the
//! lease, as RFC 2131 asks, and falls back to broadcast when the server's
//! Ethernet address cannot be resolved -- which is what rebinding is.

use core::sync::atomic::{AtomicBool, Ordering};

use kcore::net::{Nic, UdpListener};
use kcore::sync::SpinLock;
use kcore::task::TaskHandle;
use kcore::trace;

use crate::abi;
use crate::wire::{eth, ip, udp, Mac, ETH_HDR_LEN, ETH_TYPE_IP, IP_HDR_LEN,
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

struct Received {
    buf: [u8; RX_MAX],
    len: usize,
    ready: bool,
}

/// What the task, the listener and the shell all reach: one lock over all
/// of it. None of it is on a path where the lock could matter.
struct State {
    rx: Received,
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
    pub fn stop(&self) {
        let task = self.task.lock().take();
        if let Some(task) = task {
            task.request_stop();
            /* Dropping the handle waits for the task and releases it */
            drop(task);
        }

        self.unlisten();
        self.state.lock().nic = None;
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
    fn receive(&self, frame: &[u8]) {
        let mut state = self.state.lock();
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

    /// Waits for a message of `want`; false once the timeout passes, or at
    /// once when the server says no.
    fn wait_for(&self, want: u8, timeout_ms: u64) -> bool {
        let mut left = timeout_ms;

        while left > 0 {
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

    fn request(&self, renewing: bool) -> bool {
        let nic = match self.nic() {
            Some(nic) => nic,
            None => return false,
        };

        let mut frame = [0u8; MAX_FRAME];
        let len = self.build_request(&nic, &mut frame, renewing);

        self.arm();
        nic.send_raw(&frame[..len]);
        self.wait_for(ACK, EXCHANGE_TIMEOUT_MS)
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

    fn build_request(&self, nic: &Nic, frame: &mut [u8], renewing: bool) -> usize {
        let (lease, offered, server) = {
            let state = self.state.lock();
            (state.lease, state.offered_ip, state.server_id)
        };

        /* Selecting (after an offer): the address asked for and the server
         * it came from. Renewing: neither, and the address carried in ciaddr
         * instead -- some servers refuse or ignore a renewal that still
         * carries options 50 and 54 (RFC 2131). */
        let options_len = if renewing { 3 + 1 } else { 3 + 6 + 6 + 1 };

        /* RFC 2131 4.3.2: a renewal is unicast to the server that granted
         * the lease; with no answer for its address this becomes a
         * rebinding broadcast. */
        let mut dst_mac = MAC_BROADCAST;
        let mut unicast = false;
        if renewing && lease.server_ip != 0 {
            if let Some(arp) = abi::arp_table() {
                if let Some(mac) = arp.resolve(nic, nic.route_ip(lease.server_ip)) {
                    dst_mac = mac;
                    unicast = true;
                }
            }
        }

        let mut at = self.build_head(
            nic, frame, &dst_mac,
            if renewing { lease.ip } else { 0 },
            if unicast { lease.server_ip } else { 0xFFFF_FFFF },
            options_len,
            /* Renewing, this machine holds a routable address, so the server
             * can answer it directly: no broadcast flag. */
            if renewing { 0 } else { FLAG_BROADCAST },
            if renewing { lease.ip } else { 0 });

        at = put_option(frame, at, OPT_MESSAGE_TYPE, &[REQUEST]);
        if !renewing {
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

        let datagram = match crate::udp::parse(frame) {
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

        let mut state = self.state.lock();
        state.offered_ip = yiaddr;
        state.server_id = found.server_ip;
        state.lease = found;
        true
    }

    fn next_transaction(&self) {
        let mut state = self.state.lock();
        state.xid = state.xid.wrapping_add(1);
    }

    /* ---- the lease's life ---- */

    fn run(&'static self) {
        while !kcore::task::stopping() {
            self.listen();

            let mut bound = false;
            for _ in 0..TRIES {
                if kcore::task::stopping() {
                    break;
                }
                self.next_transaction();

                if self.discover() && self.request(false) {
                    bound = true;
                    break;
                }
                kcore::task::sleep_ms(2000);
            }

            self.unlisten();

            if !bound {
                trace!(0, "dhcp: no lease");
                kcore::task::sleep_ms(5000);
                continue;
            }

            let lease = self.lease();
            if let Some(nic) = self.nic() {
                nic.set_ip(lease.ip);
                nic.set_mask(lease.mask);
                nic.set_gw(lease.router);
            }
            self.ready.store(true, Ordering::Release);

            trace!(0, "dhcp: bound ip {}.{}.{}.{} mask {}.{}.{}.{} gw {}.{}.{}.{} lease {}",
                (lease.ip >> 24) & 0xFF, (lease.ip >> 16) & 0xFF,
                (lease.ip >> 8) & 0xFF, lease.ip & 0xFF,
                (lease.mask >> 24) & 0xFF, (lease.mask >> 16) & 0xFF,
                (lease.mask >> 8) & 0xFF, lease.mask & 0xFF,
                (lease.router >> 24) & 0xFF, (lease.router >> 16) & 0xFF,
                (lease.router >> 8) & 0xFF, lease.router & 0xFF,
                lease.lease_secs);

            /* T1: half the lease, and never less than ten seconds */
            let t1 = (lease.lease_secs as u64 / 2).max(10);
            for _ in 0..t1 {
                if kcore::task::stopping() {
                    return;
                }
                kcore::task::sleep_ms(1000);
            }

            trace!(0, "dhcp: renewing");
            self.listen();
            self.next_transaction();
            let renewed = self.request(true);
            self.unlisten();

            if renewed {
                let lease = self.lease();
                if let Some(nic) = self.nic() {
                    nic.set_ip(lease.ip);
                }
                trace!(0, "dhcp: renewed, lease {}", lease.lease_secs);
            } else {
                trace!(0, "dhcp: the renewal was refused, starting again");
                self.ready.store(false, Ordering::Release);
            }
        }
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
impl kcore::net::UdpHandler for Dhcp {
    fn on_frame(&'static self, frame: kcore::net::Lent<'_>, _rx: &mut kcore::net::RxContext) {
        self.receive(frame.bytes());
    }
}
