//! NBS on the host.
//!
//!     nbs-fuzz gate
//!
//! everything the gate holds: the meta target over seeds 1 to 300, the cs
//! target over 1 to 200, the wire target over 1 to 100, the cluster
//! target over 1 to 1000 and the join target over 1 to 800 -- the races of
//! some of their mutants come once in a few hundred runs -- every allocation
//! held to what the kernel's heap
//! gives at once (fuzz/raft's `heap`). Exit 0 when everything held, 1 with
//! what did not -- a finding with its seed, to run again with
//! `<target> --seed N --trace`.
//!
//!     nbs-fuzz meta|cs|wire|cluster|join [--seed N] [--seeds K] [--steps S] [--seconds T] [--trace]
//!
//! one target; with `--seconds`, seeds on from the first until the time is
//! up -- a campaign.

#[path = "../../raft/src/heap.rs"]
mod heap;
mod cluster;
mod cs;
mod join;
mod meta;
mod wire;
/* Shared with fuzz/raft, of which this crate takes what it needs */
#[allow(dead_code)]
#[path = "../../raft/src/rng.rs"]
mod rng;

#[global_allocator]
static HEAP: heap::Heap = heap::Heap;

use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const GATE_META_SEEDS: u64 = 300;
const META_STEPS: u64 = 400;
const GATE_CS_SEEDS: u64 = 200;
const CS_STEPS: u64 = 300;
const GATE_WIRE_SEEDS: u64 = 100;
const WIRE_STEPS: u64 = 500;
const GATE_CLUSTER_SEEDS: u64 = 1000;
const CLUSTER_STEPS: u64 = 2000;
const GATE_JOIN_SEEDS: u64 = 800;
const JOIN_STEPS: u64 = 3000;

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

fn arg(args: &[String], name: &str) -> Option<u64> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok())
}

/// Seeds `first..first + seeds` of target `name`, run by `run`, or on until
/// `seconds` are up: how many failed, each said.
fn target(name: &str, run: fn(u64, u64, bool) -> Result<(), String>, first: u64, seeds: u64, steps: u64,
          seconds: Option<u64>, trace: bool) -> u32 {
    let start = Instant::now();
    let mut failed = 0;
    let mut seed = first;
    let mut ran = 0;
    loop {
        match seconds {
            Some(s) if start.elapsed() >= Duration::from_secs(s) => break,
            None if ran >= seeds => break,
            _ => {}
        }
        if let Err(e) = guarded(|| run(seed, steps, trace)) {
            println!("FAIL: {} seed {} -- {}", name, seed, e);
            failed += 1;
            if failed >= 5 {
                break;
            }
        }
        seed += 1;
        ran += 1;
    }
    println!("{}: {} seeds {}..{} ({} x {} steps, {} failed, {:.1} s)", if failed == 0 { "PASS" } else { "FAIL" },
             name, first, seed, ran, steps, failed, start.elapsed().as_secs_f64());
    failed
}

fn main() -> ExitCode {
    /* A panic is a finding said once, by `guarded`: not its hook's backtrace
       too */
    panic::set_hook(Box::new(|_| {}));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let trace = args.iter().any(|a| a == "--trace");
    let one = |name: &str, run: fn(u64, u64, bool) -> Result<(), String>, steps: u64| {
        target(name, run, arg(&args, "--seed").unwrap_or(1), arg(&args, "--seeds").unwrap_or(1),
               arg(&args, "--steps").unwrap_or(steps), arg(&args, "--seconds"), trace)
    };
    let failed = match args.first().map(String::as_str) {
        Some("gate") | None => {
            /* `--first`: stopped at the first target that fails -- the
               gate's verdict, sooner (mutants.py's) */
            let first = args.iter().any(|a| a == "--first");
            let targets: [(&str, fn(u64, u64, bool) -> Result<(), String>, u64, u64); 5] = [
                ("meta", meta::run, GATE_META_SEEDS, META_STEPS),
                ("cs", cs::run, GATE_CS_SEEDS, CS_STEPS),
                ("wire", wire::run, GATE_WIRE_SEEDS, WIRE_STEPS),
                ("cluster", cluster::run, GATE_CLUSTER_SEEDS, CLUSTER_STEPS),
                ("join", join::run, GATE_JOIN_SEEDS, JOIN_STEPS),
            ];
            let mut failed = 0;
            for (name, run, seeds, steps) in targets {
                failed += target(name, run, 1, seeds, steps, None, false);
                if first && failed > 0 {
                    break;
                }
            }
            failed
        }
        Some("meta") => one("meta", meta::run, META_STEPS),
        Some("cs") => one("cs", cs::run, CS_STEPS),
        Some("wire") => one("wire", wire::run, WIRE_STEPS),
        Some("cluster") => one("cluster", cluster::run, CLUSTER_STEPS),
        Some("join") => one("join", join::run, JOIN_STEPS),
        Some(other) => {
            eprintln!("nbs-fuzz: no target {} -- gate, meta, cs, wire, cluster, join", other);
            return ExitCode::from(2);
        }
    };
    if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
