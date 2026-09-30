//! The machine the network layer runs on: the fuzzers' common one
//! (`common::machine`: the kernel's C++ half, the CPUs that run its tasks
//! one at a time, its allocator), and what the network adds to it -- its
//! NIC (`nic`), the command line's network parameters, and netconsole,
//! which the C++ tracer hands every line to.

pub mod nic;

pub use crate::common::machine::*;

use std::sync::Mutex;

/// eth0's MAC, QEMU's default.
pub const ETH0_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
/// How many frames the pool is built with: fewer than the kernel's 4096,
/// so that an input can run it dry.
pub const POOL_FRAMES: usize = 512;

/// Boots the machine, once, in the parent every input's process is forked
/// from: the pool built, eth0 registered, TCP's timer started, the network
/// layer's commands in the table -- as the kernel's boot does them, in its
/// order. No thread is made and none parked: see `sched::boot`.
pub fn boot() {
    sched::boot();
    set_trace_sink(netconsole_log);
    if !net::frame::POOL.setup(POOL_FRAMES) {
        panic!("the frame pool would not build");
    }
    if net::register("eth0", ETH0_MAC, &nic::DRIVERS[0], (), ()).is_none() {
        panic!("eth0 would not register");
    }
    if !net::tcp::TCP.init() {
        panic!("TCP would not start");
    }
    net::init();
}

/// Every line the tracer makes, to netconsole: from whatever context traced
/// it, with whatever lock is held.
fn netconsole_log(line: &[u8]) {
    // SAFETY: the line is `line.len()` readable bytes.
    sched::kernel(|| unsafe { net::abi::rust_netconsole_log(line.as_ptr(), line.len()) });
}

/* ---- the kernel command line ---- */

#[derive(Clone, Copy, Default)]
pub struct Params {
    pub dhcp_off: bool,
    pub dns_on: bool,
    pub rxpoll_on: bool,
    pub netconsole: Option<(u32, u16, usize)>,
}

static PARAMS: Mutex<Params> = Mutex::new(Params { dhcp_off: false, dns_on: false, rxpoll_on: false, netconsole: None });

pub fn params() -> Params {
    *PARAMS.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn set_params(p: Params) {
    *PARAMS.lock().unwrap_or_else(|e| e.into_inner()) = p;
}

#[no_mangle]
pub extern "C" fn kernel_param_dhcp_off() -> i32 {
    params().dhcp_off as i32
}

#[no_mangle]
pub extern "C" fn kernel_param_dns_on() -> i32 {
    params().dns_on as i32
}

#[no_mangle]
pub extern "C" fn kernel_param_rxpoll_on() -> i32 {
    params().rxpoll_on as i32
}

/// # Safety
/// The three are writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_netconsole_params(ip: *mut u32, port: *mut u16, tail_kb: *mut usize) -> i32 {
    match params().netconsole {
        Some((i, p, t)) => {
            // SAFETY: `kcore::net::netconsole_params`'s locals.
            unsafe {
                *ip = i;
                *port = p;
                *tail_kb = t;
            }
            1
        }
        None => 0,
    }
}
