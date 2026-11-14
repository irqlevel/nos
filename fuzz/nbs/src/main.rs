//! NBS on the host.
//!
//!     nbs-fuzz gate
//!
//! everything the gate holds: the meta target over seeds 1 to 1000, the cs
//! target over 1 to 200, the inflight target over 1 to 600, the wire target
//! over 1 to 100, the scrub target over 1 to 300, the cluster target over 1
//! to 1000, the join target over 1 to 800 and the tail target -- the
//! cluster target's over a volume no whole number of chunks -- over 1 to
//! 200: the races of
//! some of their mutants come once in a few hundred runs -- every allocation
//! held to what the kernel's heap
//! gives at once (fuzz/raft's `heap`). Exit 0 when everything held, 1 with
//! what did not -- a finding with its seed, to run again with
//! `<target> --seed N --trace`.
//!
//!     nbs-fuzz meta|cs|inflight|wire|scrub|cluster|join|tail [--seed N] [--seeds K] [--steps S] [--seconds T] [--trace]
//!
//! one target; with `--seconds`, seeds on from the first until the time is
//! up -- a campaign.

#[path = "../../raft/src/heap.rs"]
mod heap;
mod cluster;
mod cs;
mod inflight;
mod join;
mod meta;
mod scrub;
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

const GATE_META_SEEDS: u64 = 1000;
const META_STEPS: u64 = 400;
const GATE_CS_SEEDS: u64 = 200;
const CS_STEPS: u64 = 300;
const GATE_WIRE_SEEDS: u64 = 100;
const WIRE_STEPS: u64 = 500;
const GATE_SCRUB_SEEDS: u64 = 300;
const SCRUB_STEPS: u64 = 3000;
const GATE_CLUSTER_SEEDS: u64 = 1000;
const CLUSTER_STEPS: u64 = 2000;
/// Cluster seeds past the gate's range run with it: each the place a rule's
/// race comes together that the range lost when the target's course moved
/// -- the client's copy-up ended only once every member active in its view
/// has copied (`client_copy_every_active`: a clone's or a volume's copy-up
/// asked at a view a member had left, NotHere, those that had come since
/// never asked), lost to N13's chains, found again at these; and N13c's
/// campaign's: a copy-up's end named by its chunk's number alone, matched
/// to a copy-up asked anew late given that number again (1219, 3663); a
/// session attached anew for a write that reached no replica, its reconcile
/// held by the stuck join that held the write (1960), and the end that cuts
/// a replica off as its write was never sent (2149); and its lone writes'
/// campaign's: a join's copy of a write out that closed what it copied,
/// its source then taken out and back, an attach anew's reconcile finding
/// it open nowhere (5316); a reconcile's mend of a shared place held for a
/// copy-up a stuck join held back (7225); and a write out by a view of its
/// set that a map read finds changed sent again by the new view
/// (`client_resend_changed`), its race moved past the range by the lone
/// writes' course (2758, 4206: a write done that a replica lacks). And
/// those the review of 2026-11's fixes moved on, found again by a campaign
/// of each mutant: a write of a chain applied before a join of its set
/// began written again as it comes again (`relayed_joins`, 5375); the
/// copy-up ended only once every member active in the view has copied
/// (`client_copy_every_active`, 2962), one asked by a view behind its set
/// asked again from its start (`client_copy_stale_view`, 9122) and one a
/// member refuses as of another epoch or map asked again
/// (`client_copy_refused_again`, 8918); a writer's start reconciling the
/// open chunks past a cs's first page of three (`client_held_pages`, 4710);
/// and a write to a place whose close is out sent to each replica
/// directly, the close sent no more (`client_reopened_direct`, 10774: a
/// close sent again came after the write and closed the chunk over it, the
/// writer taken over before its next close, its replicas left apart). And
/// those of the review's question 5, found by a campaign of its mutants: a
/// join's copy keeping open what its source held open (`copy_keeps_open`,
/// `sums_say_open`: 11883, 16293, 16662 -- closed by the copy, the source taken
/// out took the last mark, and two active replicas were apart with none);
/// and a close refused for its view, to be sent again after the map read,
/// sent no more once its place is written again (`client_reopened_after_map`,
/// 20346: sent after the write, it closed the chunk over it); and those
/// the invariants of that round found the good tree failing at: a close of
/// the driver's waiting on a place a write took as it began, closed by
/// nobody, the flushes after it failed at their deadline
/// (`client_close_after_write`: 17081, 17235), and a close settled by a
/// view that changed -- refused as no replica's, or waiting on a cs taken
/// out -- taken for closed before the map read sent it to the joiner made a
/// member, which held the chunk open (`client_not_here_waits_map`,
/// `client_taken_out_waits_map`: 18153, 18466, 25047, 25064); and three
/// rules of before whose races that round's course moved past the range,
/// found again by a campaign of each mutant: a copy-up by a view behind its
/// set asked again from its start (`client_copy_stale_view`, 61339), a
/// flush answered only once the session serves (`client_flush_serving`,
/// 60065, 61530), and a write of a chain applied before a join of its set
/// began written again as it comes again (`relayed_joins`, 60834, 70881). A
/// stopgap: a race moved on by the next change is lost again, which the
/// mutants say.
const GATE_CLUSTER_ALSO: [u64; 34] = [1232, 2759, 5241, 6647, 7786, 1219, 1960, 2149, 3663, 5316, 7225, 2758, 4206,
                                      5375, 2962, 9122, 8918, 4710, 10774, 11883, 16293, 16662, 20346,
                                      17081, 17235, 18153, 18466, 25047, 25064, 61339, 60065, 61530, 60834, 70881];
const GATE_JOIN_SEEDS: u64 = 800;
/// The `tail` target's: the cluster target's over a volume no whole number of
/// chunks (the review of 2026-11).
const GATE_TAIL_SEEDS: u64 = 200;
const JOIN_STEPS: u64 = 3000;
const GATE_INFLIGHT_SEEDS: u64 = 600;
const INFLIGHT_STEPS: u64 = 400;

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
            let targets: [(&str, fn(u64, u64, bool) -> Result<(), String>, u64, u64); 8] = [
                ("meta", meta::run, GATE_META_SEEDS, META_STEPS),
                ("cs", cs::run, GATE_CS_SEEDS, CS_STEPS),
                ("inflight", inflight::run, GATE_INFLIGHT_SEEDS, INFLIGHT_STEPS),
                ("wire", wire::run, GATE_WIRE_SEEDS, WIRE_STEPS),
                ("scrub", scrub::run, GATE_SCRUB_SEEDS, SCRUB_STEPS),
                ("cluster", cluster::run, GATE_CLUSTER_SEEDS, CLUSTER_STEPS),
                ("join", join::run, GATE_JOIN_SEEDS, JOIN_STEPS),
                ("tail", cluster::run_tail, GATE_TAIL_SEEDS, CLUSTER_STEPS),
            ];
            let mut failed = 0;
            for (name, run, seeds, steps) in targets {
                failed += target(name, run, 1, seeds, steps, None, false);
                if first && failed > 0 {
                    break;
                }
                if name == "cluster" {
                    for seed in GATE_CLUSTER_ALSO {
                        failed += target(name, run, seed, 1, steps, None, false);
                    }
                    if first && failed > 0 {
                        break;
                    }
                }
            }
            failed
        }
        Some("meta") => one("meta", meta::run, META_STEPS),
        Some("cs") => one("cs", cs::run, CS_STEPS),
        Some("inflight") => one("inflight", inflight::run, INFLIGHT_STEPS),
        Some("wire") => one("wire", wire::run, WIRE_STEPS),
        Some("scrub") => one("scrub", scrub::run, SCRUB_STEPS),
        Some("cluster") => one("cluster", cluster::run, CLUSTER_STEPS),
        Some("join") => one("join", join::run, JOIN_STEPS),
        Some("tail") => one("tail", cluster::run_tail, CLUSTER_STEPS),
        Some(other) => {
            eprintln!("nbs-fuzz: no target {} -- gate, meta, cs, inflight, wire, scrub, cluster, join, tail", other);
            return ExitCode::from(2);
        }
    };
    if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
