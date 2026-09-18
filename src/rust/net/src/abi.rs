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
use crate::wire::Mac;

static ARP: AtomicPtr<ArpTable> = AtomicPtr::new(core::ptr::null_mut());
static ICMP: AtomicPtr<Icmp> = AtomicPtr::new(core::ptr::null_mut());
static DNS: AtomicPtr<Dns> = AtomicPtr::new(core::ptr::null_mut());
static DHCP: AtomicPtr<Dhcp> = AtomicPtr::new(core::ptr::null_mut());

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
