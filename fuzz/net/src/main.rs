//! The network layer, fuzzed on the host.
//!
//! Everything the network hands the kernel is somebody else's to choose --
//! every frame on the wire, every answer a server gives the HTTP client, the
//! DHCP client and the resolver, every byte an SSH client sends the server
//! -- so a panic, an overflow, a lock broken or a loop that never ends
//! anywhere on those paths is one somebody else can cause. This program is
//! the kernel's own crates -- `net`, `tls`, `fs`, `ssh`, the sshd module's
//! source, over `kcore` and `ffi` as they are -- linked with the rest of a kernel
//! written for the purpose: its C++ half (`machine`: the locks, tasks, soft
//! IRQs, timers, the clock, the entropy pool, the command table), a NIC
//! whose wire is the fuzzer's, and the network around the machine
//! (`world`: the hosts on it -- a gateway, a DNS server, a DHCP server, an
//! HTTP server, a TLS server, an SSH client, a stranger -- each speaking its
//! protocol, well and badly). Each target turns random bytes into what the
//! world does to the machine, and what the machine's programs ask of it;
//! the runner feeds it random bytes from a seed, each input in a process of
//! its own forked from one booted machine -- the network layer's statics
//! are the kernel's, and one input's must not leak into the next -- and
//! catches a panic, a broken invariant, a spin, a crash and a hang, each
//! reported with the seed and iteration that make it again.
#![allow(dead_code)]

extern crate alloc;

use std::io::Write as _;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// A finding that is not a panic: something the code did that it must not.
macro_rules! invariant {
    ($cond:expr, $($fmt:tt)*) => {
        if !$cond {
            panic!("invariant: {}", format!($($fmt)*));
        }
    };
}

mod input;
mod machine;
mod targets;
mod world;

pub use input::Input;

#[global_allocator]
static HEAP: machine::heap::Heap = machine::heap::Heap;

/// A target: bytes in, what the world and the machine's programs do out; a
/// panic, or an `invariant!` that fails, is a finding.
pub struct Target {
    pub name: &'static str,
    pub run: fn(&mut Input),
    /// How long a target's input is at most.
    pub max_len: usize,
    /// How many inputs the gate runs of it: as many as reach its deep
    /// states, the whole gate a couple of minutes.
    pub gate: u64,
}

/// SplitMix64: the seed's stream of random words.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The input of iteration `iter` of `target` under `seed`: its length and
/// bytes from a stream of their own, so any one iteration can be made again
/// without the ones before it.
fn input(target: &str, seed: u64, iter: u64, max_len: usize) -> Vec<u8> {
    let mut h = seed ^ 0xCBF2_9CE4_8422_2325;
    for b in target.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01B3);
    }
    let mut rng = Rng(h ^ iter.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let len = match rng.next() % 4 {
        0 => (rng.next() % (max_len as u64 / 32).max(64)) as usize,
        1 => (rng.next() % (max_len as u64 / 4).max(512)) as usize,
        _ => (rng.next() % max_len as u64) as usize,
    };
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        v.extend_from_slice(&rng.next().to_le_bytes());
    }
    v.truncate(len);
    v
}

/// Runs one input, in this process: what the forked child does, and what a
/// replay does.
fn run_input(t: &Target, data: &[u8]) {
    world::begin(data);
    let mut input = Input::new(world::script(data));
    (t.run)(&mut input);
}

/* ---- a process for each input ---- */

mod os {
    #[repr(C)]
    pub struct PollFd {
        pub fd: i32,
        pub events: i16,
        pub revents: i16,
    }

    #[cfg(target_os = "macos")]
    pub type Nfds = u32;
    #[cfg(not(target_os = "macos"))]
    pub type Nfds = u64;

    pub const POLLIN: i16 = 1;
    pub const SIGKILL: i32 = 9;

    extern "C" {
        pub fn fork() -> i32;
        pub fn pipe(fds: *mut i32) -> i32;
        pub fn close(fd: i32) -> i32;
        pub fn read(fd: i32, buf: *mut core::ffi::c_void, n: usize) -> isize;
        pub fn write(fd: i32, buf: *const core::ffi::c_void, n: usize) -> isize;
        pub fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
        pub fn kill(pid: i32, sig: i32) -> i32;
        pub fn poll(fds: *mut PollFd, nfds: Nfds, timeout: i32) -> i32;
        pub fn _exit(code: i32) -> !;
    }
}

/// The child's end of the pipe its finding goes up, -1 in a replay.
static REPORT_FD: AtomicI32 = AtomicI32::new(-1);
/// A replay: the panic's own report and backtrace, on the terminal.
static REPLAYING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What became of an input.
enum Outcome {
    Pass,
    Finding(String),
    /// Killed by a signal: a stack overflow, an abort.
    Crash(i32, String),
    Hang,
}

/// How long an input's process may run before it is a hang.
const HANG: Duration = Duration::from_secs(20);

fn run_forked(t: &Target, data: &[u8]) -> Outcome {
    let mut fds = [0i32; 2];
    // SAFETY: two ints for the pipe's ends.
    if unsafe { os::pipe(fds.as_mut_ptr()) } != 0 {
        panic!("no pipe");
    }
    // SAFETY: the parent has never made a thread, so the child is a copy of
    // the whole of it.
    let pid = unsafe { os::fork() };
    if pid < 0 {
        panic!("no fork");
    }
    if pid == 0 {
        // SAFETY: the read end is the parent's.
        unsafe { os::close(fds[0]) };
        REPORT_FD.store(fds[1], Ordering::Relaxed);
        run_input(t, data);
        // SAFETY: the input ran to its end: the process goes, its threads
        // with it, nothing flushed or dropped.
        unsafe { os::_exit(0) }
    }
    // SAFETY: the write end is the child's.
    unsafe { os::close(fds[1]) };

    let started = Instant::now();
    let mut message = Vec::new();
    let mut hung = false;
    loop {
        let left = HANG.saturating_sub(started.elapsed());
        if left.is_zero() {
            hung = true;
            break;
        }
        let mut pfd = os::PollFd { fd: fds[0], events: os::POLLIN, revents: 0 };
        // SAFETY: one pollfd.
        let ready = unsafe { os::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if ready <= 0 {
            continue;
        }
        let mut buf = [0u8; 4096];
        // SAFETY: the buffer's own length.
        let n = unsafe { os::read(fds[0], buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        if message.len() < 65536 {
            message.extend_from_slice(&buf[..n as usize]);
        }
    }
    if hung {
        // SAFETY: the child, which is ours.
        unsafe { os::kill(pid, os::SIGKILL) };
    }
    let mut status = 0;
    // SAFETY: the child, and a status to write.
    unsafe {
        os::waitpid(pid, &mut status, 0);
        os::close(fds[0]);
    }
    let message = String::from_utf8_lossy(&message).into_owned();
    if hung {
        return Outcome::Hang;
    }
    let signal = status & 0x7F;
    let code = (status >> 8) & 0xFF;
    if signal != 0 {
        Outcome::Crash(signal, message)
    } else if code != 0 {
        Outcome::Finding(if message.is_empty() { format!("exit {}", code) } else { message })
    } else {
        Outcome::Pass
    }
}

fn install_hook() {
    std::panic::set_hook(Box::new(|info| {
        machine::heap::PANICKING.store(true, Ordering::Relaxed);
        let place = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let what = info.payload().downcast_ref::<&str>().map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let report = format!("{} at {} [task {}, t={} ns]", what, place, machine::sched::me(),
                             machine::sched::now() - machine::sched::START.min(machine::sched::now()));
        let fd = REPORT_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            // SAFETY: the pipe's write end, and the report's bytes.
            unsafe {
                os::write(fd, report.as_ptr().cast(), report.len());
                os::_exit(1)
            }
        }
        eprintln!("FINDING: {}", report);
        if REPLAYING.load(Ordering::Relaxed) {
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
        }
        let _ = std::io::stderr().flush();
        // SAFETY: the process ends here, its other threads with it.
        unsafe { os::_exit(1) }
    }));
}

/* ---- the runner ---- */

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    let s = s.trim();
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap_or(0)).collect()
}

fn usage() -> ! {
    eprintln!("usage: net-fuzz [--seed N] [--iterations N | --seconds S] [--target NAME]... [--keep-going]");
    eprintln!("       (with neither, each target's own number of inputs: the gate)");
    eprintln!("       net-fuzz --replay NAME HEXFILE [--trace]");
    eprintln!("targets: {}", targets::ALL.iter().map(|t| t.name).collect::<Vec<_>>().join(", "));
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut seed = 1u64;
    let mut iterations = None;
    let mut seconds = None;
    let mut only: Vec<String> = Vec::new();
    let mut keep_going = false;
    let mut replay = None;
    let mut i = 1;
    while i < args.len() {
        let value = |i: usize| args.get(i + 1).cloned().unwrap_or_else(|| usage());
        match args[i].as_str() {
            "--seed" => { seed = value(i).parse().unwrap_or_else(|_| usage()); i += 1; }
            "--iterations" => { iterations = Some(value(i).parse().unwrap_or_else(|_| usage())); i += 1; }
            "--seconds" => { seconds = Some(value(i).parse::<u64>().unwrap_or_else(|_| usage())); i += 1; }
            "--target" => { only.push(value(i)); i += 1; }
            "--keep-going" => keep_going = true,
            "--trace" => machine::ECHO_TRACE.store(true, Ordering::Relaxed),
            "--replay" => {
                let file = args.get(i + 2).cloned().unwrap_or_else(|| usage());
                replay = Some((value(i), file));
                i += 2;
            }
            _ => usage(),
        }
        i += 1;
    }
    for name in &only {
        if !targets::ALL.iter().any(|t| t.name == name) {
            usage();
        }
    }

    install_hook();
    machine::boot();

    if let Some((name, file)) = replay {
        /* One input again, here, with the panic's own report and backtrace. */
        REPLAYING.store(true, Ordering::Relaxed);
        let t = targets::ALL.iter().find(|t| t.name == name).unwrap_or_else(|| usage());
        let data = unhex(&std::fs::read_to_string(&file).expect("the input's file"));
        run_input(t, &data);
        println!("{}: ran to its end, {} turns handed on, {:.1} s simulated", name, machine::sched::handoffs(),
                 (machine::sched::now() - machine::sched::START) as f64 / 1e9);
        return;
    }

    let mut findings: Vec<(String, String)> = Vec::new();
    let mut hung = false;
    let mut total = 0u64;
    for t in targets::ALL.iter().filter(|t| only.is_empty() || only.iter().any(|n| n == t.name)) {
        let start = Instant::now();
        let mut iter = 0u64;
        let mut failed = 0u64;
        let mut slowest = (0u64, Duration::ZERO);
        loop {
            match seconds {
                Some(s) if start.elapsed() >= Duration::from_secs(s) => break,
                None if iter >= iterations.unwrap_or(t.gate) => break,
                _ => {}
            }
            let data = input(t.name, seed, iter, t.max_len);
            let began = Instant::now();
            let outcome = run_forked(t, &data);
            let took = began.elapsed();
            if took > slowest.1 {
                slowest = (iter, took);
            }
            let what = match outcome {
                Outcome::Pass => None,
                Outcome::Finding(m) => Some(m),
                Outcome::Crash(sig, m) => Some(format!("killed by signal {} -- a stack overflow, an abort{}{}", sig,
                                                       if m.is_empty() { "" } else { ": " }, m)),
                Outcome::Hang => {
                    hung = true;
                    Some(format!("a hang: no end in {} s", HANG.as_secs()))
                }
            };
            if let Some(what) = what {
                failed += 1;
                /* One report a place and a kind: the same bug found again
                 * is the same -- what it says, its numbers aside, and where. */
                let place = what.rsplit(" at ").next().unwrap_or("").split(" [").next().unwrap_or("").to_string();
                let kind: String = what.split(" -- ").next().unwrap_or("").chars()
                    .filter(|c| !c.is_ascii_digit()).collect();
                let place = format!("{} {}", kind, place);
                if !findings.iter().any(|(n, p)| n == t.name && *p == place) {
                    let file = format!("net-fuzz-{}-{}-{}.hex", t.name, seed, iter);
                    let _ = std::fs::write(&file, hex(&data));
                    println!("FAIL {}: seed {} iteration {}: {}", t.name, seed, iter, what);
                    println!("     input in {} -- net-fuzz --replay {} {}", file, t.name, file);
                    let _ = std::io::stdout().flush();
                    findings.push((t.name.to_string(), place));
                }
                if !keep_going {
                    std::process::exit(if hung { 3 } else { 1 });
                }
            }
            iter += 1;
        }
        total += iter;
        println!("{:<10} {:>7} inputs, {:>6.1} s{}", t.name, iter, start.elapsed().as_secs_f64(),
                 if failed != 0 { format!(", {} failed", failed) } else { String::new() });
        if std::env::var_os("NET_FUZZ_STATS").is_some() {
            println!("           slowest: iteration {}, {:.3} s", slowest.0, slowest.1.as_secs_f64());
        }
        let _ = std::io::stdout().flush();
    }
    if findings.is_empty() {
        println!("net-fuzz: {} inputs, no finding", total);
    } else {
        println!("net-fuzz: {} inputs, {} findings", total, findings.len());
        std::process::exit(if hung { 3 } else { 1 });
    }
}
