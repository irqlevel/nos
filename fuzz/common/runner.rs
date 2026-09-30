//! The runner: random bytes from a seed, each input in a process of its own
//! forked from one booted machine -- a layer's statics are the kernel's, and
//! one input's must not leak into the next -- and a panic, a broken
//! invariant, a spin, a crash and a hang caught, each reported with the seed
//! and iteration that make it again, and the input in a file to replay.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use super::input::Input;
use super::machine::{self, sched};

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

/// A fuzzer: its targets, and what boots its machine.
pub struct Fuzzer {
    /// What it is called in what it prints, and in its inputs' file names.
    pub name: &'static str,
    pub targets: &'static [Target],
    /// Boots the machine, once, in the parent every input's process is
    /// forked from -- which must never make a thread (`sched::boot`).
    pub boot: fn(),
    /// The variable that adds each target's slowest input to the report.
    pub stats_env: &'static str,
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

/* ---- each input ---- */

/// How much of an input is the machine's own: the seed of its chaos --
/// which task goes first, where one is preempted -- and of the entropy
/// pool.
const HEADER: usize = 8;

/// The machine as each input finds it: the clock where it starts, the pool
/// seeded from the input, the chaos chosen.
fn begin(data: &[u8]) {
    let mut seed = [0u8; HEADER];
    for (i, b) in data.iter().take(HEADER).enumerate() {
        seed[i] = *b;
    }
    let word = u64::from_le_bytes(seed);
    /* Preemption at a lock let go of: never, for half the inputs, and for
     * the rest one time in 64, 16 or 4. */
    let preempt = match seed[0] {
        0..=127 => 0,
        128..=191 => 64,
        192..=239 => 16,
        _ => 4,
    };
    sched::begin_input(preempt, word);
    machine::seed_random(word);
}

/// Runs one input, in this process: what the forked child does, and what a
/// replay does.
fn run_input(t: &Target, data: &[u8]) {
    begin(data);
    let mut input = Input::new(data.get(HEADER..).unwrap_or(&[]));
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

/* ---- what the inputs reached ---- */

/// How many inputs reached each state a target counts (`reached`): what
/// the stats report, so that a target that stops reaching somewhere says
/// so. Each input's process counts its own and sends them up at its end.
static REACHED: std::sync::Mutex<std::collections::BTreeMap<&'static str, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Whether the stats are asked for: counting costs nothing otherwise.
static COUNTING: AtomicBool = AtomicBool::new(false);

/// That this input reached `what` -- counted once an input, however often.
pub fn reached(what: &'static str) {
    if COUNTING.load(Ordering::Relaxed) {
        REACHED.lock().unwrap_or_else(|e| e.into_inner()).insert(what, 1);
    }
}

/// What an input's process sends up at its end when it found nothing.
const STATS_MARK: &str = "\u{0}stats";
/// A replay: the panic's own report and backtrace, on the terminal.
static REPLAYING: AtomicBool = AtomicBool::new(false);

/// What became of an input.
enum Outcome {
    /// With what it reached, when that was counted.
    Pass(Vec<String>),
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
        if COUNTING.load(Ordering::Relaxed) {
            let mut up = String::from(STATS_MARK);
            for what in REACHED.lock().unwrap_or_else(|e| e.into_inner()).keys() {
                up.push('\n');
                up.push_str(what);
            }
            // SAFETY: the pipe's write end, and the report's bytes.
            unsafe { os::write(fds[1], up.as_ptr().cast(), up.len()) };
        }
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
        Outcome::Pass(message.strip_prefix(STATS_MARK).map_or(Vec::new(), |m| {
            m.lines().filter(|l| !l.is_empty()).map(str::to_string).collect()
        }))
    }
}

fn install_hook() {
    std::panic::set_hook(Box::new(|info| {
        machine::heap::PANICKING.store(true, Ordering::Relaxed);
        let place = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let what = info.payload().downcast_ref::<&str>().map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let report = format!("{} at {} [task {}, t={} ns]", what, place, sched::me(),
                             sched::now() - sched::START.min(sched::now()));
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

fn usage(f: &Fuzzer) -> ! {
    eprintln!("usage: {} [--seed N] [--iterations N | --seconds S] [--target NAME]... [--keep-going]", f.name);
    eprintln!("       (with neither, each target's own number of inputs: the gate)");
    eprintln!("       {} --replay NAME HEXFILE [--trace]", f.name);
    eprintln!("targets: {}", f.targets.iter().map(|t| t.name).collect::<Vec<_>>().join(", "));
    std::process::exit(2);
}

/// The fuzzer's `main`: the arguments read, the machine booted, the
/// targets run -- or one input replayed. Exit code 0 when nothing was
/// found, 1 for a finding, 3 for a hang.
pub fn main(f: &Fuzzer) {
    let args: Vec<String> = std::env::args().collect();
    let mut seed = 1u64;
    let mut iterations = None;
    let mut seconds = None;
    let mut only: Vec<String> = Vec::new();
    let mut keep_going = false;
    let mut replay = None;
    let mut i = 1;
    while i < args.len() {
        let value = |i: usize| args.get(i + 1).cloned().unwrap_or_else(|| usage(f));
        match args[i].as_str() {
            "--seed" => { seed = value(i).parse().unwrap_or_else(|_| usage(f)); i += 1; }
            "--iterations" => { iterations = Some(value(i).parse().unwrap_or_else(|_| usage(f))); i += 1; }
            "--seconds" => { seconds = Some(value(i).parse::<u64>().unwrap_or_else(|_| usage(f))); i += 1; }
            "--target" => { only.push(value(i)); i += 1; }
            "--keep-going" => keep_going = true,
            "--trace" => machine::ECHO_TRACE.store(true, Ordering::Relaxed),
            "--replay" => {
                let file = args.get(i + 2).cloned().unwrap_or_else(|| usage(f));
                replay = Some((value(i), file));
                i += 2;
            }
            _ => usage(f),
        }
        i += 1;
    }
    for name in &only {
        if !f.targets.iter().any(|t| t.name == name) {
            usage(f);
        }
    }

    install_hook();
    COUNTING.store(std::env::var_os(f.stats_env).is_some(), Ordering::Relaxed);
    (f.boot)();

    if let Some((name, file)) = replay {
        /* One input again, here, with the panic's own report and backtrace. */
        REPLAYING.store(true, Ordering::Relaxed);
        let t = f.targets.iter().find(|t| t.name == name).unwrap_or_else(|| usage(f));
        let data = unhex(&std::fs::read_to_string(&file).expect("the input's file"));
        run_input(t, &data);
        println!("{}: ran to its end, {} turns handed on, {:.1} s simulated", name, sched::handoffs(),
                 (sched::now() - sched::START) as f64 / 1e9);
        return;
    }

    let mut findings: Vec<(String, String)> = Vec::new();
    let mut hung = false;
    let mut total = 0u64;
    for t in f.targets.iter().filter(|t| only.is_empty() || only.iter().any(|n| n == t.name)) {
        let start = Instant::now();
        let mut iter = 0u64;
        let mut failed = 0u64;
        let mut slowest = (0u64, Duration::ZERO);
        let mut reached: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
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
                Outcome::Pass(states) => {
                    for state in states {
                        *reached.entry(state).or_insert(0) += 1;
                    }
                    None
                }
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
                /* One report a place: the same check failing again is the
                 * same bug, whatever the names and numbers it says this
                 * time. A crash or a hang has no place, and is one kind. */
                let place = match what.rsplit_once(" at ") {
                    Some((_, at)) => at.split(" [").next().unwrap_or("").to_string(),
                    None => what.split(" -- ").next().unwrap_or("").chars().filter(|c| !c.is_ascii_digit()).collect(),
                };
                if !findings.iter().any(|(n, p)| n == t.name && *p == place) {
                    let file = format!("{}-{}-{}-{}.hex", f.name, t.name, seed, iter);
                    let _ = std::fs::write(&file, hex(&data));
                    println!("FAIL {}: seed {} iteration {}: {}", t.name, seed, iter, what);
                    println!("     input in {} -- {} --replay {} {}", file, f.name, t.name, file);
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
        if std::env::var_os(f.stats_env).is_some() {
            println!("           slowest: iteration {}, {:.3} s", slowest.0, slowest.1.as_secs_f64());
            for (state, n) in &reached {
                println!("           {:>6.2}%  {}", 100.0 * *n as f64 / iter.max(1) as f64, state);
            }
        }
        let _ = std::io::stdout().flush();
    }
    if findings.is_empty() {
        println!("{}: {} inputs, no finding", f.name, total);
    } else {
        println!("{}: {} inputs, {} findings", f.name, total, findings.len());
        std::process::exit(if hung { 3 } else { 1 });
    }
}
