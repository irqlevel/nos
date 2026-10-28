//! The host API's wire crates (src/crates/web, src/crates/mgmtwire) on the
//! host, as the API module links them:
//!
//! - `web-test vectors`: known answers -- SHA-1's FIPS 180 vectors, RFC
//!   6455's accept key and frames, and requests, frames, JSON texts,
//!   parameters, networks and certificates each with what it must make or
//!   why it must be refused;
//! - `web-test fuzz [--seed N] [--iters N]`: every parser fed what a
//!   generator makes and then breaks -- held to never panicking, to taking
//!   back what its writer wrote, to answering a prefix of anything whole
//!   with "not yet", and to the limits it is given;
//! - `web-test cert <dir>`: certificates and keys written out for OpenSSL and
//!   Python's ssl to judge (scripts/web-test.py).
//!
//! Exit code 0: every check held; 1: one did not.

mod certs;
mod fuzzing;
mod vectors;

use std::process::ExitCode;

/// Checks that failed, each reported as it fails.
#[derive(Default)]
pub struct Fails(pub usize);

impl Fails {
    pub fn check(&mut self, what: &str, ok: bool, detail: impl std::fmt::Debug) {
        if !ok {
            println!("FAIL: {}: {:?}", what, detail);
            self.0 += 1;
        }
    }
}

/// SplitMix64: a seed's stream of random words.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `n` (0 for none).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    pub fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }

    pub fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fails = match args.first().map(String::as_str) {
        Some("vectors") => vectors::run(),
        Some("fuzz") => {
            let mut seed = 1;
            let mut iters = 20_000;
            let mut i = 1;
            while i + 1 < args.len() {
                match args[i].as_str() {
                    "--seed" => seed = args[i + 1].parse().unwrap_or(1),
                    "--iters" => iters = args[i + 1].parse().unwrap_or(20_000),
                    _ => {}
                }
                i += 2;
            }
            fuzzing::run(seed, iters)
        }
        Some("cert") if args.len() == 2 => certs::write(&args[1]),
        _ => {
            eprintln!("usage: web-test vectors | fuzz [--seed N] [--iters N] | cert <dir>");
            return ExitCode::from(2);
        }
    };
    if fails == 0 {
        println!("web-test: every check held");
        ExitCode::SUCCESS
    } else {
        println!("web-test: {} checks failed", fails);
        ExitCode::FAILURE
    }
}
