//! NBS on the host.
//!
//!     nbs-fuzz gate
//!
//! everything the gate holds: the meta target over seeds 1 to 300, every
//! allocation held to what the kernel's heap gives at once (fuzz/raft's
//! `heap`). Exit 0 when everything held, 1 with what did not -- a finding
//! with its seed, to run again with `meta --seed N --trace`.
//!
//!     nbs-fuzz meta [--seed N] [--seeds K] [--steps S] [--seconds T] [--trace]
//!
//! one target; with `--seconds`, seeds on from the first until the time is
//! up -- a campaign.

#[path = "../../raft/src/heap.rs"]
mod heap;
mod meta;
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

/// Seeds `first..first + seeds` of the meta target, or on until `seconds`
/// are up: how many failed, each said.
fn meta(first: u64, seeds: u64, steps: u64, seconds: Option<u64>, trace: bool) -> u32 {
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
        if let Err(e) = guarded(|| meta::run(seed, steps, trace)) {
            println!("FAIL: meta seed {} -- {}", seed, e);
            failed += 1;
            if failed >= 5 {
                break;
            }
        }
        seed += 1;
        ran += 1;
    }
    println!("{}: meta seeds {}..{} ({} x {} steps, {} failed, {:.1} s)", if failed == 0 { "PASS" } else { "FAIL" },
             first, seed, ran, steps, failed, start.elapsed().as_secs_f64());
    failed
}

fn main() -> ExitCode {
    /* A panic is a finding said once, by `guarded`: not its hook's backtrace
       too */
    panic::set_hook(Box::new(|_| {}));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let trace = args.iter().any(|a| a == "--trace");
    let failed = match args.first().map(String::as_str) {
        Some("gate") | None => meta(1, GATE_META_SEEDS, META_STEPS, None, false),
        Some("meta") => meta(arg(&args, "--seed").unwrap_or(1), arg(&args, "--seeds").unwrap_or(1),
                             arg(&args, "--steps").unwrap_or(META_STEPS), arg(&args, "--seconds"), trace),
        Some(other) => {
            eprintln!("nbs-fuzz: no target {} -- gate, meta", other);
            return ExitCode::from(2);
        }
    };
    if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
