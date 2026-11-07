//! What a load test says of the times its operations took (blkload,
//! nbsload): a histogram of 16 buckets to each power of two of nanoseconds,
//! so that a percentile comes out within 1/16 of the truth, and those times
//! shown in microseconds to a tenth.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

/* 2^SUB_BITS buckets to each power of two */
const SUB_BITS: u32 = 4;
const SUB: usize = 1 << SUB_BITS;
const BUCKETS: usize = 64 * SUB;
const NS_PER_US: u64 = 1_000;

/// The ten-thousandths a report gives the latency at.
pub const P50: u64 = 5_000;
pub const P90: u64 = 9_000;
pub const P99: u64 = 9_900;
pub const P999: u64 = 9_990;

fn bucket(ns: u64) -> usize {
    if ns < SUB as u64 {
        return ns as usize;
    }
    let msb = 63 - ns.leading_zeros();
    let shift = msb - SUB_BITS;
    let sub = ((ns >> shift) as usize) & (SUB - 1);
    (shift as usize + 1) * SUB + sub
}

/// [low, high) of a bucket, in nanoseconds.
fn bucket_range(b: usize) -> (u64, u64) {
    if b < SUB {
        return (b as u64, b as u64 + 1);
    }
    let shift = (b / SUB - 1) as u32;
    let sub = (b % SUB) as u64;
    ((SUB as u64 + sub) << shift, (SUB as u64 + sub + 1) << shift)
}

/// The times of operations, in nanoseconds: how many, the least, the most,
/// their sum, and how many fell in each bucket.
#[derive(Clone)]
pub struct Histogram {
    pub ios: u64,
    pub min: u64,
    pub max: u64,
    pub sum: u64,
    hist: [u32; BUCKETS],
}

impl Default for Histogram {
    fn default() -> Histogram {
        Histogram::new()
    }
}

impl Histogram {
    pub const fn new() -> Histogram {
        Histogram { ios: 0, min: u64::MAX, max: 0, sum: 0, hist: [0; BUCKETS] }
    }

    pub fn record(&mut self, ns: u64) {
        self.ios += 1;
        self.sum += ns;
        self.min = self.min.min(ns);
        self.max = self.max.max(ns);
        self.hist[bucket(ns)] += 1;
    }

    pub fn merge(&mut self, other: &Histogram) {
        self.ios += other.ios;
        self.sum += other.sum;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        for (mine, theirs) in self.hist.iter_mut().zip(other.hist.iter()) {
            *mine += *theirs;
        }
    }

    /// The latency `per10k` ten-thousandths of the operations came in
    /// under: the middle of the bucket that holds it.
    pub fn percentile(&self, per10k: u64) -> u64 {
        let rank = ((self.ios * per10k + 9_999) / 10_000).max(1);
        let mut seen = 0u64;
        for (b, &n) in self.hist.iter().enumerate() {
            seen += n as u64;
            if seen >= rank {
                let (low, high) = bucket_range(b);
                return low + (high - low) / 2;
            }
        }
        self.max
    }

    /// The report's line of it: min, avg, p50, p90, p99, p99.9 and max, in
    /// microseconds. Nothing recorded, nothing to say: empty.
    pub fn line(&self) -> Line<'_> {
        Line(self)
    }
}

/// `Histogram::line`'s.
pub struct Line<'a>(&'a Histogram);

impl fmt::Display for Line<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let h = self.0;
        if h.ios == 0 {
            return Ok(());
        }
        write!(f, "min {}, avg {}, p50 {}, p90 {}, p99 {}, p99.9 {}, max {}", Micros(h.min), Micros(h.sum / h.ios),
               Micros(h.percentile(P50)), Micros(h.percentile(P90)), Micros(h.percentile(P99)),
               Micros(h.percentile(P999)), Micros(h.max))
    }
}

/// Nanoseconds, shown in microseconds to a tenth.
pub struct Micros(pub u64);

impl fmt::Display for Micros {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0 / NS_PER_US, (self.0 % NS_PER_US) / 100)
    }
}
