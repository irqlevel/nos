//! The layer's single instances, and the few names it is called by from
//! outside Rust.
//!
//! There used to be fifty more of those names here: one per call a C++ view
//! of the layer made. The views are gone and so are they. What is left is
//! what C++ genuinely still calls -- the tracer and the panic path into
//! netconsole, boot into TCP -- and the TCP calls a loadable module binds by
//! name, which have to be a C ABI because a module is linked on its own.

use alloc::boxed::Box;
use kcore::net::Nic;
use kcore::once::OnceBox;
use kcore::trace;

use crate::arp::ArpTable;
use crate::dhcp::Dhcp;
use crate::dns::Dns;
use crate::icmp::Icmp;
use crate::net_load::NetLoad;
use crate::netconsole::NETCONSOLE;
use crate::udp_shell::UdpShell;

static ARP: OnceBox<ArpTable> = OnceBox::new();
static ICMP: OnceBox<Icmp> = OnceBox::new();
static DNS: OnceBox<Dns> = OnceBox::new();
static DHCP: OnceBox<Dhcp> = OnceBox::new();
static UDP_SHELL: OnceBox<UdpShell> = OnceBox::new();

/// The one load target: a static, because its per-CPU counters are its bulk
/// and it is reached from the receive path, where a pointer chase is the
/// kind of thing it exists to measure.
static NET_LOAD: NetLoad = NetLoad::new_const();

/// The one load target. A static rather than something made on first use:
/// its per-CPU counters are its bulk, and it is reached from the receive
/// path, where a pointer chase is the kind of thing it exists to measure.
pub(crate) fn net_load() -> &'static NetLoad {
    &NET_LOAD
}

pub(crate) fn arp_table() -> Option<&'static ArpTable> {
    match ARP.get_or_try_init(|| ArpTable::new().map(Box::new)) {
        Some(arp) => Some(arp),
        None => {
            trace!(0, "arp: no memory for the table");
            None
        }
    }
}

pub(crate) fn icmp() -> Option<&'static Icmp> {
    match ICMP.get_or_try_init(|| Icmp::new().map(Box::new)) {
        Some(icmp) => Some(icmp),
        None => {
            trace!(0, "icmp: no memory");
            None
        }
    }
}

pub(crate) fn dns() -> Option<&'static Dns> {
    match DNS.get_or_try_init(|| Dns::new().map(Box::new)) {
        Some(dns) => Some(dns),
        None => {
            trace!(0, "dns: no memory for the resolver");
            None
        }
    }
}

pub(crate) fn dhcp() -> Option<&'static Dhcp> {
    match DHCP.get_or_try_init(|| Dhcp::new().map(Box::new)) {
        Some(dhcp) => Some(dhcp),
        None => {
            trace!(0, "dhcp: no memory for the client");
            None
        }
    }
}

/* ---- the shell over UDP ---- */
fn udp_shell() -> Option<&'static UdpShell> {
    match UDP_SHELL.get_or_try_init(|| UdpShell::new().map(Box::new)) {
        Some(shell) => Some(shell),
        None => {
            trace!(0, "udpshell: no memory");
            None
        }
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

/* ---- the kernel log over UDP ---- */

/// Arm capture from the kernel command line: 1 armed, 0 not asked for.
#[no_mangle]
pub extern "C" fn rust_netconsole_setup() -> i32 {
    if NETCONSOLE.setup() { 1 } else { 0 }
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

/* ---- TCP ---- */

use crate::tcp::{Conn, TCP};

/// What the C++ side sees a connection as.
type ConnPtr = *mut u8;

/// The connection a word from outside names: one of the pool's, or none.
/// The pool is an array that lives as long as the kernel, so a word that is
/// not the address of one of its slots is simply not a connection -- and one
/// that is, is safe to look at whatever state it is in.
fn conn_of(conn: ConnPtr) -> Option<&'static Conn> {
    TCP.by_handle(conn as usize)
}

/// Start the connection pool's timer: 0 started, -1 not.
#[no_mangle]
pub extern "C" fn rust_tcp_init() -> i32 {
    if TCP.init() { 0 } else { -1 }
}

/* ---- TCP, as a module calls it ---- */

/* A module is linked on its own and binds these by name, which is why they
 * are a C ABI; nothing inside the kernel image comes through here -- the
 * HTTP client and, through the transport it is handed, the TLS client call
 * `TCP` itself. */

/// The bytes queued within `timeout_ms` (0: wait for room as long as it
/// takes): 0 when the time found room for none, -1 once the connection is
/// gone.
///
/// # Safety
/// `conn` came from a listen, accept or connect, and `buf` holds `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_tcp_send_timeout(
    conn: ConnPtr, buf: *const u8, len: usize, timeout_ms: u64,
) -> isize {
    let conn = match conn_of(conn) { Some(conn) => conn, None => return -1 };
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
    let conn = match conn_of(conn) { Some(conn) => conn, None => return -1 };
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
    let nic = match Nic::from_handle(dev) {
        Some(nic) => nic,
        None => return core::ptr::null_mut(),
    };
    match TCP.listen(&nic, port) {
        Some(conn) => conn as *const Conn as ConnPtr,
        None => core::ptr::null_mut(),
    }
}

/// The next connection on the listener's port: null once `timeout_ms` passes
/// with none, or once the listener is closed -- or for a `listener` that is
/// not a connection at all.
#[no_mangle]
pub extern "C" fn kernel_tcp_accept(
    listener: ConnPtr, timeout_ms: u64,
) -> ConnPtr {
    let listener = match conn_of(listener) {
        Some(listener) => listener,
        None => return core::ptr::null_mut(),
    };
    match TCP.accept(listener, timeout_ms) {
        Some(conn) => conn as *const Conn as ConnPtr,
        None => core::ptr::null_mut(),
    }
}

/// Closes what a listen, accept or connect gave out. The caller does not use
/// it again: the slot is another connection's before long.
#[no_mangle]
pub extern "C" fn kernel_tcp_close(conn: ConnPtr) {
    if let Some(conn) = conn_of(conn) {
        TCP.close(conn);
    }
}

/// A connection a server refuses or drops: reset, so that its slot does not
/// sit out TIME-WAIT. As with `kernel_tcp_close`, not used again.
#[no_mangle]
pub extern "C" fn kernel_tcp_abort(conn: ConnPtr) {
    if let Some(conn) = conn_of(conn) {
        TCP.abort(conn);
    }
}

/// Who is at the other end: the address in host byte order, and the port.
///
/// # Safety
/// `ip` and `port` are writable or null.
#[no_mangle]
pub unsafe extern "C" fn kernel_tcp_peer(
    conn: ConnPtr, ip: *mut u32, port: *mut u16,
) {
    let conn = match conn_of(conn) { Some(conn) => conn, None => return };
    let (addr, remote) = TCP.peer(conn);
    unsafe {
        if let Some(ip) = ip.as_mut() {
            *ip = addr;
        }
        if let Some(port) = port.as_mut() {
            *port = remote;
        }
    }
}
