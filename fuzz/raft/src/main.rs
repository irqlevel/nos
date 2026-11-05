//! The raft crate on the host.
//!
//!     raft-fuzz gate
//!
//! everything the gate holds: the known answers, the scripted traps, the
//! node target over seeds 1 to 2000, the wire target over seeds 1 to 20 and
//! the hostile target over seeds 1 to 2000, the storage target over seeds
//! 1 to 20 -- every allocation of every one
//! held to what the kernel's heap gives at once (`heap`). Exit 0 when
//! everything held, 1 with what did not -- a finding with its seed, to run
//! again with `node --seed N --trace` or `hostile --seed N`.
//!
//!     raft-fuzz unit | scenarios [name]
//!     raft-fuzz node [--seed N] [--seeds K] [--steps S] [--seconds T] [--trace]
//!     raft-fuzz wire|hostile|storage [--seed N] [--seeds K] [--iterations K] [--seconds T]
//!
//! one part at a time; with `--seconds`, seeds on from the first until the
//! time is up -- a campaign.

mod heap;
mod hostile;
mod review;
mod rng;
mod scenarios;
mod sim;
mod storefuzz;
mod unit;
mod wirefuzz;

#[global_allocator]
static HEAP: heap::Heap = heap::Heap;

use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use rng::Rng;
use sim::{Params, Sim};

/// Steps a node run takes before the network is healed and progress asked.
const STEPS: u64 = 5000;
/// Rounds of a tick each that progress may take once healed.
const CONVERGE_ROUNDS: u64 = 600;
const GATE_NODE_SEEDS: u64 = 2000;
const GATE_WIRE_SEEDS: u64 = 20;
const WIRE_ITERATIONS: u64 = 5000;
const GATE_HOSTILE_SEEDS: u64 = 2000;
const GATE_STORAGE_SEEDS: u64 = 20;
const STORAGE_ITERATIONS: u64 = 300;
const HOSTILE_ITERATIONS: u64 = 3000;

/// A run's outcome, a panic included as a finding, and a block asked for
/// that the kernel's heap would not give.
fn guarded(f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    let before = heap::over();
    let r = match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let what = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "a panic".into());
            Err(format!("panicked: {}", what))
        }
    };
    r.and_then(|()| heap::check(before))
}

/// One node run: a group of three to seven, its configuration and its
/// network's faults drawn from `seed`.
pub fn node_run(seed: u64, steps: u64, trace: bool) -> Result<(), String> {
    let mut r = Rng::new(seed.wrapping_mul(31));
    let p = Params::random(&mut r);
    let nvoters = if r.chance(10) { 1 } else { r.range(3, 5) };
    let nlearners = r.range(0, 2);
    let voters: Vec<u64> = (1..=nvoters).collect();
    let learners: Vec<u64> = (nvoters + 1..=nvoters + nlearners).collect();
    let mut s = Sim::new(p, seed, &voters, &learners);
    s.trace = trace;
    for _ in 0..steps {
        s.random_step();
        if !s.failures.is_empty() {
            break;
        }
    }
    if s.failures.is_empty() {
        s.check_logs();
    }
    if s.failures.is_empty() && !s.converge(CONVERGE_ROUNDS) {
        let d = s.describe();
        s.fail(format!("no progress in {} rounds once healed (params {:?}): {}", CONVERGE_ROUNDS, s.p, d));
    }
    if s.failures.is_empty() {
        s.check_logs();
    }
    if std::env::var_os("RAFT_STATS").is_some() {
        eprintln!("seed {}: {}", seed, s.describe());
    }
    if s.failures.is_empty() { Ok(()) } else { Err(s.failures.join("; ")) }
}

fn arg(args: &[String], name: &str) -> Option<u64> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok())
}

fn unit() -> u32 {
    let mut failed = 0;
    for (name, f) in unit::ALL {
        match guarded(*f) {
            Ok(()) => println!("PASS: unit {}", name),
            Err(e) => {
                println!("FAIL: unit {} -- {}", name, e);
                failed += 1;
            }
        }
    }
    failed
}

fn scenarios(only: Option<&str>) -> u32 {
    let mut failed = 0;
    for (name, f) in scenarios::ALL.iter().chain(review::ALL) {
        if only.is_some_and(|o| o != *name) {
            continue;
        }
        match guarded(*f) {
            Ok(()) => println!("PASS: scenario {}", name),
            Err(e) => {
                println!("FAIL: scenario {} -- {}", name, e);
                failed += 1;
            }
        }
    }
    failed
}

fn nodes(first: u64, count: u64, steps: u64, seconds: Option<u64>, trace: bool) -> u32 {
    let start = Instant::now();
    let mut failed = 0;
    let mut seed = first;
    let mut ran = 0;
    loop {
        match seconds {
            Some(s) if start.elapsed() >= Duration::from_secs(s) => break,
            None if ran >= count => break,
            _ => {}
        }
        if let Err(e) = guarded(|| node_run(seed, steps, trace)) {
            println!("FAIL: node seed {} -- {}", seed, e);
            failed += 1;
            if failed >= 5 {
                break;
            }
        }
        seed += 1;
        ran += 1;
    }
    println!("{}: node seeds {}..{} ({} runs, {} failed, {:.1} s)", if failed == 0 { "PASS" } else { "FAIL" },
             first, seed, ran, failed, start.elapsed().as_secs_f64());
    failed
}

/// A target of seeds and iterations -- the wire, a hostile peer -- from
/// `first`, `count` of them or until `seconds` are up.
fn seeds(name: &str, run: fn(u64, u64) -> Result<(), String>, first: u64, count: u64, iterations: u64,
         seconds: Option<u64>) -> u32 {
    let start = Instant::now();
    let mut failed = 0;
    let mut seed = first;
    let mut ran = 0;
    loop {
        match seconds {
            Some(s) if start.elapsed() >= Duration::from_secs(s) => break,
            None if ran >= count => break,
            _ => {}
        }
        if let Err(e) = guarded(|| run(seed, iterations)) {
            println!("FAIL: {} seed {} -- {}", name, seed, e);
            failed += 1;
            if failed >= 5 {
                break;
            }
        }
        seed += 1;
        ran += 1;
    }
    println!("{}: {} seeds {}..{} ({} x {} iterations, {} failed, {:.1} s)", if failed == 0 { "PASS" } else { "FAIL" },
             name, first, seed, ran, iterations, failed, start.elapsed().as_secs_f64());
    failed
}

/// A target's seeds as the command line names them: one with `--seed`,
/// the gate's otherwise.
fn target(args: &[String], name: &str, run: fn(u64, u64) -> Result<(), String>, gate_seeds: u64, iterations: u64,
          seconds: Option<u64>) -> u32 {
    let one = arg(args, "--seed");
    let count = arg(args, "--seeds").unwrap_or(if one.is_some() { 1 } else { gate_seeds });
    seeds(name, run, one.unwrap_or(1), count, arg(args, "--iterations").unwrap_or(iterations), seconds)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("gate");
    let trace = args.iter().any(|a| a == "--trace");
    let seconds = arg(&args, "--seconds");
    /* A panic is reported as a finding with its seed; its own message is
     * printed only when tracing. */
    if !trace {
        panic::set_hook(Box::new(|_| {}));
    }
    let failed = match cmd {
        "gate" => {
            unit()
                + scenarios(None)
                + nodes(1, GATE_NODE_SEEDS, STEPS, None, false)
                + seeds("wire", wirefuzz::run, 1, GATE_WIRE_SEEDS, WIRE_ITERATIONS, None)
                + seeds("hostile", hostile::run, 1, GATE_HOSTILE_SEEDS, HOSTILE_ITERATIONS, None)
                + seeds("storage", storefuzz::run, 1, GATE_STORAGE_SEEDS, STORAGE_ITERATIONS, None)
        }
        "unit" => unit(),
        "scenarios" => scenarios(args.get(2).map(String::as_str).filter(|a| !a.starts_with("--"))),
        "node" => {
            let seed = arg(&args, "--seed").unwrap_or(1);
            let count = arg(&args, "--seeds").unwrap_or(if arg(&args, "--seed").is_some() { 1 } else { 100 });
            nodes(seed, count, arg(&args, "--steps").unwrap_or(STEPS), seconds, trace)
        }
        "wire" => target(&args, "wire", wirefuzz::run, GATE_WIRE_SEEDS, WIRE_ITERATIONS, seconds),
        "hostile" => target(&args, "hostile", hostile::run, GATE_HOSTILE_SEEDS, HOSTILE_ITERATIONS, seconds),
        "storage" => target(&args, "storage", storefuzz::run, GATE_STORAGE_SEEDS, STORAGE_ITERATIONS, seconds),
        _ => {
            eprintln!("usage: raft-fuzz gate | unit | scenarios [name] | node [--seed N] [--seeds K] [--steps S] \
                       [--seconds T] [--trace] | wire|hostile|storage [--seed N] [--seeds K] [--iterations K] [--seconds T]");
            return ExitCode::from(2);
        }
    };
    if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
