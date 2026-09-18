//! What the C++ side calls the network layer by.
//!
//! The receive path, the shell and the protocols still in C++ reach ARP and
//! ICMP through the names below. Each one finds the single instance of what
//! it is about, made on first use the way the VFS is.

use core::sync::atomic::{AtomicPtr, Ordering};

use alloc::boxed::Box;
use kcore::net::Nic;
use kcore::trace;

use crate::arp::ArpTable;
use crate::dhcp::{Dhcp, Lease};
use crate::dns::{Dns, MAX_DOMAIN_LEN};
use crate::icmp::{Icmp, Stats};
use crate::net_load::{NetLoad, Stats as NetLoadStats};
use crate::netconsole::{Stats as NetconsoleStats, NETCONSOLE};
use crate::udp_shell::UdpShell;
use crate::wire::Mac;

static ARP: AtomicPtr<ArpTable> = AtomicPtr::new(core::ptr::null_mut());
static ICMP: AtomicPtr<Icmp> = AtomicPtr::new(core::ptr::null_mut());
static DNS: AtomicPtr<Dns> = AtomicPtr::new(core::ptr::null_mut());
static DHCP: AtomicPtr<Dhcp> = AtomicPtr::new(core::ptr::null_mut());
static UDP_SHELL: AtomicPtr<UdpShell> = AtomicPtr::new(core::ptr::null_mut());

/// The one load target: a static, because its per-CPU counters are its bulk
/// and it is reached from the receive path, where a pointer chase is the
/// kind of thing it exists to measure.
static NET_LOAD: NetLoad = NetLoad::new_const();

/// The one of something, made on first use. Two callers racing here both get
/// the same one, and the loser's is dropped.
fn once<T>(slot: &AtomicPtr<T>, make: impl FnOnce() -> Option<Box<T>>) -> Option<&'static T> {
    let existing = slot.load(Ordering::Acquire);
    if !existing.is_null() {
        return Some(unsafe { &*existing });
    }

    let made = Box::into_raw(make()?);
    match slot.compare_exchange(
        core::ptr::null_mut(), made, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => Some(unsafe { &*made }),
        Err(winner) => {
            unsafe { drop(Box::from_raw(made)) };
            Some(unsafe { &*winner })
        }
    }
}

/// The one load target. A static rather than something made on first use:
/// its per-CPU counters are its bulk, and it is reached from the receive
/// path, where a pointer chase is the kind of thing it exists to measure.
pub(crate) fn net_load() -> &'static NetLoad {
    &NET_LOAD
}

pub(crate) fn arp_table() -> Option<&'static ArpTable> {
    match once(&ARP, || ArpTable::new().map(Box::new)) {
        Some(arp) => Some(arp),
        None => {
            trace!(0, "arp: no memory for the table");
            None
        }
    }
}

pub(crate) fn icmp() -> Option<&'static Icmp> {
    match once(&ICMP, || Icmp::new().map(Box::new)) {
        Some(icmp) => Some(icmp),
        None => {
            trace!(0, "icmp: no memory");
            None
        }
    }
}

pub(crate) fn dns() -> Option<&'static Dns> {
    match once(&DNS, || Dns::new().map(Box::new)) {
        Some(dns) => Some(dns),
        None => {
            trace!(0, "dns: no memory for the resolver");
            None
        }
    }
}

pub(crate) fn dhcp() -> Option<&'static Dhcp> {
    match once(&DHCP, || Dhcp::new().map(Box::new)) {
        Some(dhcp) => Some(dhcp),
        None => {
            trace!(0, "dhcp: no memory for the client");
            None
        }
    }
}

/// # Safety
/// `frame` points at `len` readable bytes, and `dev` is a device handle.
unsafe fn frame<'a>(frame: *const u8, len: usize) -> Option<&'a [u8]> {
    if frame.is_null() || len == 0 {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(frame, len) })
}

/* ---- ARP ---- */

/// An ARP frame off the wire: a request for this machine is answered, and
/// either kind teaches the cache.
///
/// # Safety
/// `data` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_arp_process(dev: usize, data: *const u8, len: usize) {
    let (arp, nic, bytes) = match (
        arp_table(), unsafe { Nic::from_handle(dev) }, unsafe { frame(data, len) })
    {
        (Some(arp), Some(nic), Some(bytes)) => (arp, nic, bytes),
        _ => return,
    };

    arp.process(&nic, bytes);
}

/// The Ethernet address of `ip`, asking for it if it is not cached: 0 found,
/// -1 not. Task context -- it may wait a second at a time.
///
/// # Safety
/// `mac` points at six writable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_arp_resolve(dev: usize, ip: u32, mac: *mut u8) -> i32 {
    let (arp, nic) = match (arp_table(), unsafe { Nic::from_handle(dev) }) {
        (Some(arp), Some(nic)) => (arp, nic),
        _ => return -1,
    };
    if mac.is_null() {
        return -1;
    }

    match arp.resolve(&nic, ip) {
        Some(found) => {
            unsafe { core::ptr::copy_nonoverlapping(found.as_ptr(), mac, 6) };
            0
        }
        None => -1,
    }
}

/// The Ethernet address of `ip` if it is cached and unexpired, without
/// asking for it: 0 found, -1 not. Any context.
///
/// # Safety
/// `mac` points at six writable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_arp_lookup(ip: u32, mac: *mut u8) -> i32 {
    let arp = match arp_table() {
        Some(arp) => arp,
        None => return -1,
    };
    if mac.is_null() {
        return -1;
    }

    match arp.lookup(ip) {
        Some(found) => {
            unsafe { core::ptr::copy_nonoverlapping(found.as_ptr(), mac, 6) };
            0
        }
        None => -1,
    }
}

/// The cache, for the `arp` command: how many entries there are, with the
/// first `max` of them written as (ip, mac) pairs.
///
/// # Safety
/// `ips` takes `max` addresses and `macs` `max` times six bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_arp_snapshot(ips: *mut u32, macs: *mut u8, max: usize) -> usize {
    let arp = match arp_table() {
        Some(arp) => arp,
        None => return 0,
    };
    if ips.is_null() || macs.is_null() || max == 0 {
        return 0;
    }

    let mut entries = [(0u32, [0u8; 6]); 16];
    let take = max.min(entries.len());
    let count = arp.snapshot(&mut entries[..take]);

    for i in 0..count {
        unsafe {
            *ips.add(i) = entries[i].0;
            core::ptr::copy_nonoverlapping(entries[i].1.as_ptr(), macs.add(i * 6), 6);
        }
    }
    count
}

/* ---- ICMP ---- */

/// An ICMP packet off the wire.
///
/// # Safety
/// `data` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_icmp_process(dev: usize, data: *const u8, len: usize) {
    let (icmp, nic, bytes) = match (
        icmp(), unsafe { Nic::from_handle(dev) }, unsafe { frame(data, len) })
    {
        (Some(icmp), Some(nic), Some(bytes)) => (icmp, nic, bytes),
        _ => return,
    };

    icmp.process(&nic, bytes);
}

/// An echo request to `dst`: 0 sent, -1 not.
#[no_mangle]
pub extern "C" fn rust_icmp_send_echo(dev: usize, dst: u32, id: u16, seq: u16) -> i32 {
    let (icmp, arp, nic) = match (icmp(), arp_table(), unsafe { Nic::from_handle(dev) }) {
        (Some(icmp), Some(arp), Some(nic)) => (icmp, arp, nic),
        _ => return -1,
    };

    if icmp.send_echo_request(&nic, arp, dst, id, seq) { 0 } else { -1 }
}

/// Waits for the reply to (id, seq): 0 with the round trip in `rtt_ns`, or
/// -1 once the timeout passes with none.
///
/// # Safety
/// `rtt_ns` is writable.
#[no_mangle]
pub unsafe extern "C" fn rust_icmp_wait_reply(
    id: u16, seq: u16, timeout_ms: u64, rtt_ns: *mut u64,
) -> i32 {
    let icmp = match icmp() {
        Some(icmp) => icmp,
        None => return -1,
    };

    match icmp.wait_reply(id, seq, timeout_ms) {
        Some(rtt) => {
            if !rtt_ns.is_null() {
                unsafe { *rtt_ns = rtt };
            }
            0
        }
        None => -1,
    }
}

/// What `icmp` reports.
///
/// # Safety
/// `out` points at a Stats.
#[no_mangle]
pub unsafe extern "C" fn rust_icmp_stats(out: *mut Stats) {
    let icmp = match icmp() {
        Some(icmp) => icmp,
        None => return,
    };
    if out.is_null() {
        return;
    }

    unsafe { *out = icmp.stats() };
}

/* Keeps the type in the crate's public surface for the ABI above */
pub type ArpMac = Mac;

/* ---- DNS ---- */

/// Start resolving through `server_ip` on the device: 0 started, -1 not.
#[no_mangle]
pub extern "C" fn rust_dns_start(dev: usize, server_ip: u32) -> i32 {
    let (dns, nic) = match (dns(), unsafe { Nic::from_handle(dev) }) {
        (Some(dns), Some(nic)) => (dns, nic),
        _ => return -1,
    };

    if dns.start(nic, server_ip) { 0 } else { -1 }
}

/// Whether there is a resolver to ask.
#[no_mangle]
pub extern "C" fn rust_dns_ready() -> i32 {
    match dns() {
        Some(dns) if dns.is_ready() => 1,
        _ => 0,
    }
}

/// The address of the name: 0 with it in `ip`, or -1.
///
/// # Safety
/// `name` points at `len` readable bytes and `ip` is writable.
#[no_mangle]
pub unsafe extern "C" fn rust_dns_resolve(
    name: *const u8, len: usize, timeout_ms: u64, ip: *mut u32,
) -> i32 {
    let dns = match dns() {
        Some(dns) => dns,
        None => return -1,
    };
    if name.is_null() || ip.is_null() || len == 0 || len > MAX_DOMAIN_LEN {
        return -1;
    }

    let name = unsafe { core::slice::from_raw_parts(name, len) };
    match dns.resolve(name, timeout_ms) {
        Some(found) => {
            unsafe { *ip = found };
            0
        }
        None => -1,
    }
}

#[no_mangle]
pub extern "C" fn rust_dns_flush() {
    if let Some(dns) = dns() {
        dns.flush();
    }
}

/// The index'th cached name, into `name` (NUL-terminated) with its address
/// into `ip`: 0 when there is one, -1 past the end.
///
/// # Safety
/// `name` takes `cap` bytes and `ip` one address.
#[no_mangle]
pub unsafe extern "C" fn rust_dns_entry(
    index: usize, name: *mut u8, cap: usize, ip: *mut u32,
) -> i32 {
    let dns = match dns() {
        Some(dns) => dns,
        None => return -1,
    };
    if name.is_null() || ip.is_null() || cap == 0 {
        return -1;
    }

    /* One entry at a time, because the caller prints as it goes */
    let mut all = [([0u8; MAX_DOMAIN_LEN + 1], 0usize, 0u32); 32];
    let count = dns.snapshot(&mut all);
    if index >= count {
        return -1;
    }
    let (bytes, len, addr) = all[index];
    if len + 1 > cap {
        return -1;
    }

    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), name, len);
        *name.add(len) = 0;
        *ip = addr;
    }
    0
}

/* ---- DHCP ---- */

/// Start the client on the device: 0 started, -1 not (one is running
/// already, or there was no task to run it in).
#[no_mangle]
pub extern "C" fn rust_dhcp_start(dev: usize) -> i32 {
    let (dhcp, nic) = match (dhcp(), unsafe { Nic::from_handle(dev) }) {
        (Some(dhcp), Some(nic)) => (dhcp, nic),
        _ => return -1,
    };

    if dhcp.start(nic) { 0 } else { -1 }
}

/// Stop the client and give up the port. Returns once its task has left.
#[no_mangle]
pub extern "C" fn rust_dhcp_stop() {
    if let Some(dhcp) = dhcp() {
        dhcp.stop();
    }
}

/// Whether a lease is held.
#[no_mangle]
pub extern "C" fn rust_dhcp_ready() -> i32 {
    match dhcp() {
        Some(dhcp) if dhcp.is_ready() => 1,
        _ => 0,
    }
}

/// What the lease turned out to be.
///
/// # Safety
/// `out` points at a Lease.
#[no_mangle]
pub unsafe extern "C" fn rust_dhcp_lease(out: *mut Lease) {
    let dhcp = match dhcp() {
        Some(dhcp) => dhcp,
        None => return,
    };
    if out.is_null() {
        return;
    }

    unsafe { *out = dhcp.lease() };
}

/* ---- the shell over UDP ---- */

fn udp_shell() -> Option<&'static UdpShell> {
    match once(&UDP_SHELL, || UdpShell::new().map(Box::new)) {
        Some(shell) => Some(shell),
        None => {
            trace!(0, "udpshell: no memory");
            None
        }
    }
}

/// Start the shell on the device's `port`: 0 started, -1 not.
#[no_mangle]
pub extern "C" fn rust_udp_shell_start(dev: usize, port: u16) -> i32 {
    match unsafe { Nic::from_handle(dev) } {
        Some(nic) if udp_shell_start(nic, port) => 0,
        _ => -1,
    }
}

/// The same, for the boot path in this crate.
pub(crate) fn udp_shell_start(nic: Nic, port: u16) -> bool {
    match udp_shell() {
        Some(shell) => shell.start(nic, port),
        None => false,
    }
}

pub(crate) fn udp_shell_stop() {
    if let Some(shell) = udp_shell() {
        shell.stop();
    }
}

/// Stop it and give up the port. Returns once its task has left.
#[no_mangle]
pub extern "C" fn rust_udp_shell_stop() {
    if let Some(shell) = udp_shell() {
        shell.stop();
    }
}

/* ---- the kernel log over UDP ---- */

/// Arm capture from the kernel command line: 1 armed, 0 not asked for.
#[no_mangle]
pub extern "C" fn rust_netconsole_setup() -> i32 {
    if NETCONSOLE.setup() { 1 } else { 0 }
}

/// Attach the device and start the drain task: 0 started, -1 not.
#[no_mangle]
pub extern "C" fn rust_netconsole_start(dev: usize) -> i32 {
    let nic = match unsafe { Nic::from_handle(dev) } {
        Some(nic) => nic,
        None => return -1,
    };

    if NETCONSOLE.start(nic) { 0 } else { -1 }
}

#[no_mangle]
pub extern "C" fn rust_netconsole_stop() {
    NETCONSOLE.stop();
}

#[no_mangle]
pub extern "C" fn rust_netconsole_enabled() -> i32 {
    if NETCONSOLE.is_enabled() { 1 } else { 0 }
}

/// One message into the ring. Called for every line the tracer produces and
/// from the panic printer, so from any context.
///
/// # Safety
/// `s` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_netconsole_log(s: *const u8, len: usize) {
    if s.is_null() || len == 0 {
        return;
    }

    NETCONSOLE.log(unsafe { core::slice::from_raw_parts(s, len) });
}

#[no_mangle]
pub extern "C" fn rust_netconsole_panic_mark() {
    NETCONSOLE.panic_mark();
}

#[no_mangle]
pub extern "C" fn rust_netconsole_panic_flush() {
    NETCONSOLE.panic_flush();
}

/// What the `netconsole` command reports.
///
/// # Safety
/// `out` points at a Stats.
#[no_mangle]
pub unsafe extern "C" fn rust_netconsole_stats(out: *mut NetconsoleStats) {
    if out.is_null() {
        return;
    }

    unsafe { *out = NETCONSOLE.stats() };
}

/* ---- the load target ---- */

/// Start it on the device's `port`, echoing or sinking: 0 started, -1 not.
#[no_mangle]
pub extern "C" fn rust_netload_start(dev: usize, port: u16, echo: i32) -> i32 {
    let nic = match unsafe { Nic::from_handle(dev) } {
        Some(nic) => nic,
        None => return -1,
    };

    if NET_LOAD.start(nic, port, echo != 0) { 0 } else { -1 }
}

#[no_mangle]
pub extern "C" fn rust_netload_stop() {
    NET_LOAD.stop();
}

#[no_mangle]
pub extern "C" fn rust_netload_running() -> i32 {
    if NET_LOAD.is_running() { 1 } else { 0 }
}

#[no_mangle]
pub extern "C" fn rust_netload_reset() {
    NET_LOAD.reset_counters();
}

/// What `netload` reports.
///
/// # Safety
/// `out` points at a Stats.
#[no_mangle]
pub unsafe extern "C" fn rust_netload_stats(out: *mut NetLoadStats) {
    if out.is_null() {
        return;
    }
    unsafe { *out = NET_LOAD.stats() };
}

/// What the index'th CPU received, for the per-CPU line.
#[no_mangle]
pub extern "C" fn rust_netload_cpu_rx(index: usize) -> usize {
    NET_LOAD.cpu_rx(index)
}

/* ---- TCP ---- */

use crate::tcp::{self, Conn, ConnInfo, Stats as TcpStats, TCP};

/// What the C++ side sees a connection as.
type ConnPtr = *mut u8;

/// # Safety
/// `conn` came from connect, listen or accept and has not been closed.
unsafe fn conn_of<'a>(conn: ConnPtr) -> Option<&'a Conn> {
    if conn.is_null() {
        None
    } else {
        Some(unsafe { &*(conn as *const Conn) })
    }
}

/// Start the connection pool's timer: 0 started, -1 not.
#[no_mangle]
pub extern "C" fn rust_tcp_init() -> i32 {
    if TCP.init() { 0 } else { -1 }
}

/// A frame the receive path says carries TCP.
///
/// # Safety
/// `data` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_tcp_process(dev: usize, data: *const u8, len: usize) {
    let (nic, bytes) = match (unsafe { Nic::from_handle(dev) }, unsafe { frame(data, len) }) {
        (Some(nic), Some(bytes)) => (nic, bytes),
        _ => return,
    };

    TCP.process(&nic, bytes);
}

/// An active open, blocking until it is up or the timeout passes. A source
/// port of 0 takes an ephemeral one. Null when it did not connect.
#[no_mangle]
pub extern "C" fn rust_tcp_connect(dev: usize, dst_ip: u32, dst_port: u16, src_port: u16)
    -> ConnPtr
{
    let nic = match unsafe { Nic::from_handle(dev) } {
        Some(nic) => nic,
        None => return core::ptr::null_mut(),
    };

    match TCP.connect(&nic, dst_ip, dst_port, src_port) {
        Some(conn) => conn as *const Conn as ConnPtr,
        None => core::ptr::null_mut(),
    }
}

/// A passive open on the port, at every address this machine has.
#[no_mangle]
pub extern "C" fn rust_tcp_listen(dev: usize, port: u16) -> ConnPtr {
    let nic = match unsafe { Nic::from_handle(dev) } {
        Some(nic) => nic,
        None => return core::ptr::null_mut(),
    };

    match TCP.listen(&nic, port) {
        Some(conn) => conn as *const Conn as ConnPtr,
        None => core::ptr::null_mut(),
    }
}

/// The next connection on a listener's port; null once the timeout passes
/// with none, or once the listener is closed.
///
/// # Safety
/// `listener` came from `rust_tcp_listen`.
#[no_mangle]
pub unsafe extern "C" fn rust_tcp_accept(listener: ConnPtr, timeout_ms: u64) -> ConnPtr {
    let listener = match unsafe { conn_of(listener) } {
        Some(listener) => listener,
        None => return core::ptr::null_mut(),
    };

    /* The pool is a static, so a connection lives as long as the kernel */
    let listener: &'static Conn = unsafe { core::mem::transmute(listener) };
    match TCP.accept(listener, timeout_ms) {
        Some(conn) => conn as *const Conn as ConnPtr,
        None => core::ptr::null_mut(),
    }
}

/// Queue and send: the bytes taken, or -1 when the connection went before
/// any were.
///
/// # Safety
/// `data` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_tcp_send(
    conn: ConnPtr, data: *const u8, len: usize, timeout_ms: u64,
) -> isize {
    let conn = match unsafe { conn_of(conn) } {
        Some(conn) => conn,
        None => return -1,
    };
    if data.is_null() {
        return -1;
    }
    if len == 0 {
        return 0;
    }

    let conn: &'static Conn = unsafe { core::mem::transmute(conn) };
    TCP.send(conn, unsafe { core::slice::from_raw_parts(data, len) }, timeout_ms)
}

/// What has arrived: the byte count, 0 at the end of the stream, or one of
/// the negative answers.
///
/// # Safety
/// `buf` takes `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_tcp_recv(
    conn: ConnPtr, buf: *mut u8, len: usize, timeout_ms: u64,
) -> isize {
    let conn = match unsafe { conn_of(conn) } {
        Some(conn) => conn,
        None => return tcp::RECV_ERROR,
    };
    if buf.is_null() {
        return tcp::RECV_ERROR;
    }
    if len == 0 {
        return 0;
    }

    let conn: &'static Conn = unsafe { core::mem::transmute(conn) };
    TCP.recv(conn, unsafe { core::slice::from_raw_parts_mut(buf, len) }, timeout_ms)
}

/// # Safety
/// `conn` came from connect, listen or accept, and is not used again.
#[no_mangle]
pub unsafe extern "C" fn rust_tcp_close(conn: ConnPtr) {
    if let Some(conn) = unsafe { conn_of(conn) } {
        let conn: &'static Conn = unsafe { core::mem::transmute(conn) };
        TCP.close(conn);
    }
}

/// # Safety
/// As for `rust_tcp_close`.
#[no_mangle]
pub unsafe extern "C" fn rust_tcp_abort(conn: ConnPtr) {
    if let Some(conn) = unsafe { conn_of(conn) } {
        let conn: &'static Conn = unsafe { core::mem::transmute(conn) };
        TCP.abort(conn);
    }
}

/// Who the connection is with; host byte order.
///
/// # Safety
/// `ip` and `port` are writable, and `conn` is live.
#[no_mangle]
pub unsafe extern "C" fn rust_tcp_peer(conn: ConnPtr, ip: *mut u32, port: *mut u16) {
    let conn = match unsafe { conn_of(conn) } {
        Some(conn) => conn,
        None => return,
    };

    let conn: &'static Conn = unsafe { core::mem::transmute(conn) };
    let (peer_ip, peer_port) = TCP.peer(conn);
    unsafe {
        if !ip.is_null() {
            *ip = peer_ip;
        }
        if !port.is_null() {
            *port = peer_port;
        }
    }
}

/// A quoted segment came back unreachable.
#[no_mangle]
pub extern "C" fn rust_tcp_on_icmp_unreachable(
    local_ip: u32, local_port: u16, remote_ip: u32, remote_port: u16, quoted_seq: u32,
) {
    TCP.on_icmp_unreachable(local_ip, local_port, remote_ip, remote_port, quoted_seq);
}

/// What `tcpstat` reports.
///
/// # Safety
/// `out` points at a Stats.
#[no_mangle]
pub unsafe extern "C" fn rust_tcp_stats(out: *mut TcpStats) {
    if out.is_null() {
        return;
    }
    unsafe { *out = TCP.stats() };
}

/// What `tcpstat` prints per connection. The C++ side declares the same
/// struct.
#[repr(C)]
pub struct TcpConnLine {
    pub state: *const u8,
    pub local_ip: u32,
    pub local_port: u16,
    pub remote_port: u16,
    pub remote_ip: u32,
    pub send_used: usize,
    pub in_flight: usize,
    pub recv_used: usize,
}

/// The index'th connection, or -1 when that slot is free.
///
/// # Safety
/// `out` points at a TcpConnLine.
#[no_mangle]
pub unsafe extern "C" fn rust_tcp_conn_at(index: usize, out: *mut TcpConnLine) -> i32 {
    if out.is_null() {
        return -1;
    }

    let info: ConnInfo = match TCP.snapshot(index) {
        Some(info) => info,
        None => return -1,
    };

    /* The names are static strings with a NUL the C side reads to */
    let name: &'static [u8] = match info.state {
        tcp::State::Listen => b"LISTEN\0",
        tcp::State::SynSent => b"SYN_SENT\0",
        tcp::State::SynReceived => b"SYN_RCVD\0",
        tcp::State::Established => b"ESTABLISHED\0",
        tcp::State::FinWait1 => b"FIN_WAIT_1\0",
        tcp::State::FinWait2 => b"FIN_WAIT_2\0",
        tcp::State::CloseWait => b"CLOSE_WAIT\0",
        tcp::State::LastAck => b"LAST_ACK\0",
        tcp::State::Closing => b"CLOSING\0",
        tcp::State::TimeWait => b"TIME_WAIT\0",
        tcp::State::Closed => b"CLOSED\0",
        tcp::State::Free => b"FREE\0",
    };

    unsafe {
        *out = TcpConnLine {
            state: name.as_ptr(),
            local_ip: info.local_ip,
            local_port: info.local_port,
            remote_port: info.remote_port,
            remote_ip: info.remote_ip,
            send_used: info.send_used,
            in_flight: info.in_flight,
            recv_used: info.recv_used,
        };
    }
    0
}

/// How many slots `rust_tcp_conn_at` will answer for.
#[no_mangle]
pub extern "C" fn rust_tcp_max_connections() -> usize {
    tcp::MAX_CONNECTIONS
}

/* ---- TCP, as a module and the TLS client call it ---- */

/* These were defined in `rust_ffi.cpp`, over the C++ `Tcp` view, which called
 * straight back into this crate: Rust to C++ to Rust for every byte the SSH
 * server sent. They are the same names and the same contract, one hop now. */

/// The bytes queued, or -1 when the connection is gone before any were.
///
/// # Safety
/// `conn` came from a listen, accept or connect, and `buf` holds `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_tcp_send(
    conn: ConnPtr, buf: *const u8, len: usize,
) -> isize {
    unsafe { kernel_tcp_send_timeout(conn, buf, len, 0) }
}

/// `kernel_tcp_send` with a bound on the wait for room: the bytes queued, 0
/// when `timeout_ms` found room for none, -1 once the connection is gone.
///
/// # Safety
/// `conn` came from a listen, accept or connect, and `buf` holds `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_tcp_send_timeout(
    conn: ConnPtr, buf: *const u8, len: usize, timeout_ms: u64,
) -> isize {
    let conn = match unsafe { conn_of(conn) } { Some(conn) => conn, None => return -1 };
    if buf.is_null() {
        return -1;
    }
    TCP.send(conn, unsafe { core::slice::from_raw_parts(buf, len) }, timeout_ms)
}

/// The byte count, 0 at end of stream, -1 on a bad argument, -2 on a timeout.
///
/// # Safety
/// `conn` came from a listen, accept or connect, and `buf` takes `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_tcp_recv(
    conn: ConnPtr, buf: *mut u8, len: usize, timeout_ms: u64,
) -> isize {
    let conn = match unsafe { conn_of(conn) } { Some(conn) => conn, None => return -1 };
    if buf.is_null() {
        return -1;
    }
    TCP.recv(conn, unsafe { core::slice::from_raw_parts_mut(buf, len) }, timeout_ms)
}

/// A server of Rust's -- sshd's -- owns its connections rather than borrowing
/// one: it listens on a device's port, accepts, and closes both what it
/// accepted and the listener. `dev` is a `kernel_net_find` handle.
#[no_mangle]
pub extern "C" fn kernel_tcp_listen(dev: usize, port: u16) -> ConnPtr {
    if dev == 0 || port == 0 {
        return core::ptr::null_mut();
    }
    let nic = match unsafe { Nic::from_handle(dev) } {
        Some(nic) => nic,
        None => return core::ptr::null_mut(),
    };
    match TCP.listen(&nic, port) {
        Some(conn) => conn as *const Conn as ConnPtr,
        None => core::ptr::null_mut(),
    }
}

/// The next connection on the listener's port: null once `timeout_ms` passes
/// with none, or once the listener is closed.
///
/// # Safety
/// `listener` came from `kernel_tcp_listen`.
#[no_mangle]
pub unsafe extern "C" fn kernel_tcp_accept(
    listener: ConnPtr, timeout_ms: u64,
) -> ConnPtr {
    let listener = match unsafe { conn_of(listener) } {
        Some(listener) => listener,
        None => return core::ptr::null_mut(),
    };
    match TCP.accept(listener, timeout_ms) {
        Some(conn) => conn as *const Conn as ConnPtr,
        None => core::ptr::null_mut(),
    }
}

/// # Safety
/// `conn` came from a listen, accept or connect and is not used again.
#[no_mangle]
pub unsafe extern "C" fn kernel_tcp_close(conn: ConnPtr) {
    if let Some(conn) = unsafe { conn_of(conn) } {
        TCP.close(conn);
    }
}

/// A connection a server refuses or drops: reset, so that its slot does not
/// sit out TIME-WAIT.
///
/// # Safety
/// `conn` came from a listen, accept or connect and is not used again.
#[no_mangle]
pub unsafe extern "C" fn kernel_tcp_abort(conn: ConnPtr) {
    if let Some(conn) = unsafe { conn_of(conn) } {
        TCP.abort(conn);
    }
}

/// Who is at the other end: the address in host byte order, and the port.
///
/// # Safety
/// `conn` came from a listen, accept or connect; `ip` and `port` are
/// writable or null.
#[no_mangle]
pub unsafe extern "C" fn kernel_tcp_peer(
    conn: ConnPtr, ip: *mut u32, port: *mut u16,
) {
    let conn = match unsafe { conn_of(conn) } { Some(conn) => conn, None => return };
    let (addr, remote) = TCP.peer(conn);
    if !ip.is_null() {
        unsafe { *ip = addr };
    }
    if !port.is_null() {
        unsafe { *port = remote };
    }
}
