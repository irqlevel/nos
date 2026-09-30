//! The machine the network layer runs on: the kernel's C++ half, as the
//! `ffi` crate declares it (`ffi`: locks, tasks, soft IRQs, timers, the
//! clock, the entropy pool, the log, the command table, the lockless ring),
//! the CPUs that run its tasks one at a time (`sched`), its NIC (`nic`) and
//! its allocator (`heap`) -- everything `net`, `tls`, `kcore` and the
//! modules' sources reach, stood in for.

pub mod cmd;
pub mod ffi;
pub mod heap;
pub mod nic;
pub mod sched;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
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

/* ---- the entropy pool ---- */

/// The pool's stream: the input's, so that a run is the same every time.
/// `dry` has it give nothing, as a pool not yet seeded does.
struct Pool {
    state: u64,
    dry: bool,
    /// Values to hand out first, in place of the stream's: where sequence
    /// numbers wrap, all ones, zero.
    queued: VecDeque<u8>,
}

static POOL: Mutex<Pool> = Mutex::new(Pool { state: 0x9E37_79B9_7F4A_7C15, dry: false, queued: VecDeque::new() });

pub fn seed_random(seed: u64) {
    let mut p = POOL.lock().unwrap_or_else(|e| e.into_inner());
    p.state = seed ^ 0xD1B5_4A32_D192_ED03;
    p.dry = false;
    p.queued.clear();
}

/// The pool gives nothing from now on, or gives again.
pub fn random_dry(dry: bool) {
    POOL.lock().unwrap_or_else(|e| e.into_inner()).dry = dry;
}

/// The next random bytes the kernel asks for are these.
pub fn random_queue(bytes: &[u8]) {
    POOL.lock().unwrap_or_else(|e| e.into_inner()).queued.extend(bytes.iter().copied());
}

pub fn random(buf: &mut [u8]) -> bool {
    let mut p = POOL.lock().unwrap_or_else(|e| e.into_inner());
    if p.dry {
        return false;
    }
    for b in buf.iter_mut() {
        if let Some(q) = p.queued.pop_front() {
            *b = q;
            continue;
        }
        /* SplitMix64, a byte at a time */
        p.state = p.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = p.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *b = (z ^ (z >> 31)) as u8;
    }
    true
}

/* ---- the log ---- */

/// What the kernel log holds: the lines traced, the newest kept.
static DMESG: Mutex<VecDeque<Vec<u8>>> = Mutex::new(VecDeque::new());
const DMESG_LINES: usize = 256;
/// Print every line as it is traced: a replay's `--trace`.
pub static ECHO_TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The level the kernel traces at unless told otherwise
/// (`Parameters::DefaultLogLevel`): what reaches the log and netconsole.
const LOG_LEVEL: u32 = 1;
/// A line of the log at most: `Tracer::Output`'s buffer, its NUL aside. A
/// longer one is cut, "..." where it was.
const LINE_MAX: usize = 255;

pub fn trace(level: u32, msg: &[u8]) {
    if ECHO_TRACE.load(Ordering::Relaxed) {
        eprintln!("[{:>12}] {}", sched::now() / 1000, String::from_utf8_lossy(msg));
    }
    if level > LOG_LEVEL {
        return;
    }
    /* The line as rust_ffi.cpp's kernel_trace prints it: the CPU, the time
     * since boot, the message. */
    let us = sched::now().saturating_sub(sched::START) / 1000;
    let mut line = format!("{}:{}.{:06}:", sched::cpu(), us / 1_000_000, us % 1_000_000).into_bytes();
    line.extend_from_slice(msg);
    line.push(b'\n');
    if line.len() > LINE_MAX {
        line.truncate(LINE_MAX - 3);
        line.extend_from_slice(b"...");
    }
    {
        let mut d = DMESG.lock().unwrap_or_else(|e| e.into_inner());
        if d.len() == DMESG_LINES {
            d.pop_front();
        }
        d.push_back(line.clone());
    }
    /* Every line to netconsole, as the C++ tracer hands it: from whatever
     * context traced it, with whatever lock is held. */
    // SAFETY: the line is `line.len()` readable bytes.
    sched::kernel(|| unsafe { net::abi::rust_netconsole_log(line.as_ptr(), line.len()) });
}

pub fn dmesg() -> Vec<Vec<u8>> {
    DMESG.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
}

/* ---- the lockless ring ---- */

struct Ring {
    words: VecDeque<usize>,
    capacity: usize,
    alive: bool,
}

static RINGS: Mutex<Vec<Ring>> = Mutex::new(Vec::new());

pub fn ring_create(capacity: usize) -> usize {
    if capacity == 0 || !capacity.is_power_of_two() {
        return 0;
    }
    let mut r = RINGS.lock().unwrap_or_else(|e| e.into_inner());
    r.push(Ring { words: VecDeque::with_capacity(capacity), capacity, alive: true });
    r.len()
}

fn with_ring<R>(ring: usize, f: impl FnOnce(&mut Ring) -> R) -> R {
    let mut r = RINGS.lock().unwrap_or_else(|e| e.into_inner());
    match ring.checked_sub(1).and_then(|i| r.get_mut(i)) {
        Some(ring) if ring.alive => f(ring),
        _ => panic!("invariant: ring {} is no ring", ring),
    }
}

pub fn ring_destroy(ring: usize) {
    with_ring(ring, |r| r.alive = false)
}

pub fn ring_push(ring: usize, value: usize) -> bool {
    with_ring(ring, |r| {
        if r.words.len() == r.capacity {
            return false;
        }
        r.words.push_back(value);
        true
    })
}

pub fn ring_pop(ring: usize) -> Option<usize> {
    with_ring(ring, |r| r.words.pop_front())
}

pub fn ring_count(ring: usize) -> usize {
    with_ring(ring, |r| r.words.len())
}

/* ---- the CPUs ---- */

static NEXT_CPU: AtomicU32 = AtomicU32::new(0);

/// The CPU a new task runs on: round the four, so that per-CPU state --
/// the frame pool's caches, the counters -- is more than one CPU's.
pub fn next_cpu() -> u32 {
    NEXT_CPU.fetch_add(1, Ordering::Relaxed) % 4
}
