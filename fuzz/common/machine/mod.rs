//! The machine the kernel's crates run on in a fuzzer: the kernel's C++
//! half, as the `ffi` crate declares it (`ffi`: locks, tasks, soft IRQs,
//! timers, the clock, the entropy pool, the log and the command table), the
//! CPUs that run its tasks one at a time (`sched`) and its allocator
//! (`heap`). What a fuzzer's layer has of its own -- a NIC, a disk, a
//! command line -- the fuzzer adds beside it.

pub mod cmd;
pub mod ffi;
pub mod heap;
pub mod sched;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

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

/// What the kernel log holds: the lines traced, the newest kept, each
/// with its number.
static DMESG: Mutex<VecDeque<(u64, Vec<u8>)>> = Mutex::new(VecDeque::new());
const DMESG_LINES: usize = 256;
/// The lines traced so far: the next one's number.
static TRACED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Print every line as it is traced: a replay's `--trace`.
pub static ECHO_TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The level the kernel traces at unless told otherwise
/// (`Parameters::DefaultLogLevel`): what reaches the log and whatever the
/// C++ tracer hands it on to.
const LOG_LEVEL: u32 = 1;
/// A line of the log at most: `Tracer::Output`'s buffer, its NUL aside. A
/// longer one is cut, "..." where it was.
const LINE_MAX: usize = 255;

/// Where the C++ tracer hands every line on to besides the log: netconsole,
/// the disk log -- whichever of the kernel's the fuzzer links. Set once, at
/// boot; it is kernel code, and runs as such (`sched::kernel`).
static TRACE_SINK: OnceLock<fn(&[u8])> = OnceLock::new();

pub fn set_trace_sink(sink: fn(&[u8])) {
    if TRACE_SINK.set(sink).is_err() {
        panic!("the trace sink set twice");
    }
}

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
        d.push_back((TRACED.fetch_add(1, Ordering::Relaxed), line.clone()));
    }
    /* Every line on, as the C++ tracer hands it: from whatever context
     * traced it, with whatever lock is held. */
    if let Some(sink) = TRACE_SINK.get() {
        sink(&line);
    }
}

pub fn dmesg() -> Vec<Vec<u8>> {
    DMESG.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(_, l)| l.clone()).collect()
}

/// The number the next line traced will have: a mark to ask
/// `traced_since` from.
pub fn trace_mark() -> u64 {
    TRACED.load(Ordering::Relaxed)
}

/// The lines traced since `mark` that the log still holds.
pub fn traced_since(mark: u64) -> Vec<Vec<u8>> {
    DMESG.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|(n, _)| *n >= mark).map(|(_, l)| l.clone()).collect()
}

/* ---- the CPUs ---- */

static NEXT_CPU: AtomicU32 = AtomicU32::new(0);

/// The CPU a new task runs on: round the four, so that per-CPU state --
/// the frame pool's caches, the counters -- is more than one CPU's.
pub fn next_cpu() -> u32 {
    NEXT_CPU.fetch_add(1, Ordering::Relaxed) % 4
}
