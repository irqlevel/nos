//! The hypervisor's guest-facing code, fuzzed on the host.
//!
//! Everything a guest drives -- the serial port, the 8259, the PIT, the RTC,
//! ACPI's fixed hardware, PCI configuration space, the virtio disk and NIC,
//! the local APIC in both its modes, the IO-APIC, the MMIO decoder, page
//! walker and emulator, the run loop that dispatches every exit, the guests'
//! DHCP server -- and what a guest's image drives -- the Linux loader's
//! header parsing and layout --
//! compiled from the hypervisor's own sources, as they are, against
//! stand-ins for the kernel (`kernel`: a clock the fuzzer moves, the locks,
//! a vCPU's wait) and for the CPU (`cpu`: hvarch's VMCB layout from its own
//! source, and a CPU whose guest is a script), and guest memory as sparse
//! pages (`memory`). A guest decides every value these functions are
//! handed, so nothing any of them does with one may bring down the host
//! that runs it: no panic -- an index out of range, an overflow, which this
//! build checks as a RUSTUB=1 kernel does (Cargo.toml) -- no loop that does
//! not end, and no loop that spins rather than sleeps. Each target turns
//! bytes into a sequence of what a guest and the host do to one device, or
//! to the whole machine (`platform`: a Linux guest's, built, loaded and run
//! by the real run loop on each of its CPUs); the runner feeds it random
//! bytes from a seed, catching a panic, a broken invariant, a spin and a
//! hang, each reported with the seed and iteration that make it again.
#![allow(dead_code)]

extern crate alloc;
extern crate self as kcore;
extern crate self as hvarch;

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A finding that is not a panic: something the code did that it must not.
macro_rules! invariant {
    ($cond:expr, $($fmt:tt)*) => {
        if !$cond {
            panic!("invariant: {}", format!($($fmt)*));
        }
    };
}

/* ---- what the sources reach of the kernel and of hvarch ---- */

mod kernel;
pub use kernel::{consts, dma, sync, time};
#[path = "../../../src/rust/kcore/src/pod.rs"]
pub mod pod;
mod cpu;
pub use cpu::{machine, svm, vm, x86, Caps, Error, Ext, Result, Vendor};
pub mod memory;

/* ---- the hypervisor's own sources ---- */

pub mod devices;
#[path = "../../../src/rust/hv/src/acpi.rs"]
pub mod acpi;
/// The guests' DHCP server, from the module: what a guest's DHCP client's
/// frames reach.
#[path = "../../../src/rust/modules/hv/src/dhcp.rs"]
pub mod dhcp;
#[path = "../../../src/rust/hv/src/insn.rs"]
pub mod insn;
#[path = "../../../src/rust/hv/src/lapic.rs"]
pub mod lapic;
#[path = "../../../src/rust/hv/src/linux.rs"]
pub mod linux;
#[path = "../../../src/rust/hv/src/mmio.rs"]
pub mod mmio;
#[path = "../../../src/rust/hv/src/policy.rs"]
pub mod policy;
#[path = "../../../src/rust/hv/src/run.rs"]
pub mod run;
#[path = "../../../src/rust/hv/src/smp.rs"]
pub mod smp;
#[path = "../../../src/rust/hv/src/walk.rs"]
pub mod walk;

mod platform;
mod targets;

/* ---- the runner ---- */

/// A target: bytes in, a sequence of what a guest and the host do to one
/// device out; a panic, or an `invariant!` that fails, is a finding.
pub struct Target {
    pub name: &'static str,
    pub run: fn(&mut targets::Input),
    /// How long a target's input is at most: enough for a few hundred
    /// operations.
    pub max_len: usize,
}

/// What a panic said, kept by the hook for the runner to report.
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);
/// Where the runner is, for the watchdog to report a hang by.
static PROGRESS: AtomicU64 = AtomicU64::new(0);
static CURRENT: Mutex<(&str, u64, u64)> = Mutex::new(("", 0, 0));
static REPLAYING: AtomicBool = AtomicBool::new(false);

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
    /* Short inputs as often as long ones: a device's first few operations
     * are where most of its states are reached from. */
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

/// Run `t` on `data`: None, or what went wrong.
fn run_one(t: &Target, data: &[u8]) -> Option<String> {
    crate::time::reset();
    *LAST_PANIC.lock().unwrap() = None;
    let result = std::panic::catch_unwind(|| {
        let mut input = targets::Input::new(data);
        (t.run)(&mut input);
    });
    match result {
        Ok(()) => None,
        Err(_) => Some(LAST_PANIC.lock().unwrap().take().unwrap_or_else(|| String::from("a panic"))),
    }
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    let s = s.trim();
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap_or(0)).collect()
}

fn usage() -> ! {
    eprintln!("usage: hv-fuzz [--seed N] [--iterations N | --seconds S] [--target NAME]... [--keep-going]");
    eprintln!("       hv-fuzz --replay NAME HEXFILE");
    eprintln!("targets: {}", targets::ALL.iter().map(|t| t.name).collect::<Vec<_>>().join(", "));
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut seed = 1u64;
    let mut iterations = 20_000u64;
    let mut seconds = None;
    let mut only: Vec<String> = Vec::new();
    let mut keep_going = false;
    let mut replay = None;
    let mut i = 1;
    while i < args.len() {
        let value = |i: usize| args.get(i + 1).cloned().unwrap_or_else(|| usage());
        match args[i].as_str() {
            "--seed" => { seed = value(i).parse().unwrap_or_else(|_| usage()); i += 1; }
            "--iterations" => { iterations = value(i).parse().unwrap_or_else(|_| usage()); i += 1; }
            "--seconds" => { seconds = Some(value(i).parse::<u64>().unwrap_or_else(|_| usage())); i += 1; }
            "--target" => { only.push(value(i)); i += 1; }
            "--keep-going" => keep_going = true,
            "--replay" => {
                let file = args.get(i + 2).cloned().unwrap_or_else(|| usage());
                replay = Some((value(i), file));
                i += 2;
            }
            _ => usage(),
        }
        i += 1;
    }

    if let Some((name, file)) = replay {
        /* One input again, with the panic's own report and backtrace. */
        REPLAYING.store(true, Ordering::Relaxed);
        let t = targets::ALL.iter().find(|t| t.name == name).unwrap_or_else(|| usage());
        let data = unhex(&std::fs::read_to_string(&file).expect("the input's file"));
        crate::time::reset();
        let mut input = targets::Input::new(&data);
        (t.run)(&mut input);
        println!("{}: ran to its end", name);
        return;
    }

    std::panic::set_hook(Box::new(|info| {
        let place = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let what = info.payload().downcast_ref::<&str>().map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        /* The first of a run's panics is the finding: a guest's other CPUs
         * then leave their runs by panics of their own. */
        let mut last = LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_none() {
            *last = Some(format!("{} at {}", what, place));
        }
    }));

    /* A target that stops making progress for this long is in a loop: the
     * hang is reported with what reproduces it, and the run fails. */
    std::thread::spawn(|| {
        let mut last = u64::MAX;
        loop {
            std::thread::sleep(Duration::from_secs(10));
            let now = PROGRESS.load(Ordering::Relaxed);
            if now == last {
                let (name, seed, iter) = *CURRENT.lock().unwrap();
                println!("HANG {}: seed {} iteration {} made no progress in 10 s", name, seed, iter);
                if let Some(t) = targets::ALL.iter().find(|t| t.name == name) {
                    let file = format!("hv-fuzz-{}-{}-{}.hex", name, seed, iter);
                    let _ = std::fs::write(&file, hex(&input(name, seed, iter, t.max_len)));
                    println!("     input in {} -- hv-fuzz --replay {} {}", file, name, file);
                }
                let _ = std::io::stdout().flush();
                std::process::exit(3);
            }
            last = now;
        }
    });

    let mut findings: Vec<(String, String)> = Vec::new();
    let mut total = 0u64;
    for t in targets::ALL.iter().filter(|t| only.is_empty() || only.iter().any(|n| n == t.name)) {
        let start = Instant::now();
        let mut iter = 0u64;
        let mut failed = 0u64;
        loop {
            match seconds {
                Some(s) if start.elapsed() >= Duration::from_secs(s) => break,
                None if iter >= iterations => break,
                _ => {}
            }
            *CURRENT.lock().unwrap() = (t.name, seed, iter);
            let data = input(t.name, seed, iter, t.max_len);
            if let Some(what) = run_one(t, &data) {
                failed += 1;
                /* One report a place: the same bug found again is the same. */
                let place = what.rsplit(" at ").next().unwrap_or("").to_string();
                if !findings.iter().any(|(n, p)| n == t.name && *p == place) {
                    let file = format!("hv-fuzz-{}-{}-{}.hex", t.name, seed, iter);
                    let _ = std::fs::write(&file, hex(&data));
                    println!("FAIL {}: seed {} iteration {}: {}", t.name, seed, iter, what);
                    println!("     input in {} -- hv-fuzz --replay {} {}", file, t.name, file);
                    findings.push((t.name.to_string(), place));
                }
                if !keep_going {
                    std::process::exit(1);
                }
            }
            iter += 1;
            PROGRESS.fetch_add(1, Ordering::Relaxed);
        }
        total += iter;
        println!("{:<10} {:>9} inputs, {:>6.1} s{}", t.name, iter, start.elapsed().as_secs_f64(),
                 if failed != 0 { format!(", {} failed", failed) } else { String::new() });
    }
    if std::env::var_os("HV_FUZZ_STATS").is_some() {
        platform::report();
    }
    if findings.is_empty() {
        println!("hv-fuzz: {} inputs, no finding", total);
    } else {
        println!("hv-fuzz: {} inputs, {} findings", total, findings.len());
        std::process::exit(1);
    }
}
