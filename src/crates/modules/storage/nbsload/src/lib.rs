#![no_std]
#![forbid(unsafe_code)]

//! nbsload: a load test on an NBS volume through NBS's client (nbsdrive),
//! run from the shell on any host of the NBS cluster -- IOPS, bandwidth and
//! latency down to the percentiles, as blkload says them of a device.
//!
//!     insmod /nbsload.ko
//!     nbsload disk1 randread qd=8
//!     nbsload disk1 randwrite bs=64k qd=16 secs=10
//!     nbsload disk1 verify secs=20 force
//!
//! The command's own task keeps qd operations out at once through the
//! volume's session: no task of its own, nothing shared. A write test
//! attaches the volume for writing here -- refused while another host
//! writes, unless `force`, which takes it -- and destroys what is on it; a
//! read test attaches read-only, which a host writing refuses. `verify`
//! writes blocks each of which says which block it is and which of its
//! writes, reads blocks back as it goes and holds each to the last write
//! done to it, and at the end, after a flush, reads every block it wrote
//! and holds them all: a block that does not hold what was last written to
//! it is said with where, and fails the test. Its writes and reads never
//! overlap one another, so that what a block holds is one write's. The
//! module's unload ends a test under way at once.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use kcore::cmd::{Command, Output};
use kcore::time::boot_time_ns;
use latency::Histogram;
use nbsdrive::{Done, Drive, Op, TURN_NS};

const HELP: &str = "nbsload <volume> [randread|randwrite|read|write|verify] [bs= qd= secs= span=] [force]";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const NS_PER_SEC: u64 = 1_000_000_000;
const NS_PER_MS: u64 = 1_000_000;
const NS_PER_US: u64 = 1_000;

/// What a block is a whole number of: the client's.
const BLOCK: u64 = nbsclient::BLOCK;
const DEFAULT_BS: u64 = 4 * KIB;
/// An operation's most: a request's of the session's, which its buffer is
/// -- well under the most the kernel's heap gives at once (512 KiB).
const MAX_BS: u64 = 256 * KIB;
const DEFAULT_QD: u64 = 8;
const MAX_QD: u64 = 128;
const DEFAULT_SECS: u64 = 5;
const MAX_SECS: u64 = 600;
/// What `verify` writes over unless `span=` says: what it remembers of each
/// block is a few words, and a whole volume's would be many.
const VERIFY_SPAN: u64 = 64 * MIB;
/// A verify block's first word: "NBSLOAD1".
const MAGIC: u64 = 0x3144_414F_4C53_424E;
/// How many blocks gone wrong are said by where, at most.
const SAID_MAX: usize = 8;

/// Set as the module's unload begins: a test under way ends.
static STOPPING: AtomicBool = AtomicBool::new(false);

struct NbsLoad {
    _cmd: Command,
}

impl kmod::Module for NbsLoad {
    fn stop(&mut self) {
        STOPPING.store(true, Ordering::Relaxed);
    }
}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let cmd = Command::register("nbsload", HELP, |args, out| {
        if let Err(problem) = run(args, out) {
            let _ = writeln!(out, "nbsload: {}", problem);
        }
    })?;
    Ok(Box::new(NbsLoad { _cmd: cmd }))
}

kmod::module!(name: "nbsload", init: init);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    RandRead,
    RandWrite,
    Read,
    Write,
    Verify,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::RandRead => "randread",
            Mode::RandWrite => "randwrite",
            Mode::Read => "read",
            Mode::Write => "write",
            Mode::Verify => "verify",
        }
    }

    fn writes(self) -> bool {
        matches!(self, Mode::RandWrite | Mode::Write | Mode::Verify)
    }

    fn random(self) -> bool {
        matches!(self, Mode::RandRead | Mode::RandWrite | Mode::Verify)
    }
}

struct Options<'a> {
    volume: &'a str,
    mode: Mode,
    bs: u64,
    qd: u64,
    secs: u64,
    span: Option<u64>,
    force: bool,
}

/* 4096, 4k, 1m */
fn parse_size(text: &str) -> Option<u64> {
    let (digits, unit) = match text.as_bytes().last()? {
        b'k' | b'K' => (&text[..text.len() - 1], KIB),
        b'm' | b'M' => (&text[..text.len() - 1], MIB),
        _ => (text, 1),
    };
    digits.parse::<u64>().ok()?.checked_mul(unit)
}

fn parse(args: &str) -> Result<Options<'_>, String> {
    let mut words = args.split_whitespace();
    let volume = words.next().ok_or_else(|| format!("which volume? `nbs volumes` lists them\nusage: {}", HELP))?;
    let mut o = Options { volume, mode: Mode::RandRead, bs: DEFAULT_BS, qd: DEFAULT_QD, secs: DEFAULT_SECS,
                          span: None, force: false };
    for word in words {
        match word {
            "randread" => o.mode = Mode::RandRead,
            "randwrite" => o.mode = Mode::RandWrite,
            "read" => o.mode = Mode::Read,
            "write" => o.mode = Mode::Write,
            "verify" => o.mode = Mode::Verify,
            "force" => o.force = true,
            _ => {
                let value = |v: Option<u64>| v.ok_or_else(|| format!("{}: not a number", word));
                match word.split_once('=') {
                    Some(("bs", v)) => o.bs = value(parse_size(v))?,
                    Some(("qd", v)) => o.qd = value(v.parse().ok())?,
                    Some(("secs", v)) => o.secs = value(v.parse().ok())?,
                    Some(("span", v)) => o.span = Some(value(v.parse::<u64>().ok().and_then(|m| m.checked_mul(MIB)))?),
                    _ => return Err(format!("what is '{}'?\nusage: {}", word, HELP)),
                }
            }
        }
    }
    if o.bs < BLOCK || o.bs % BLOCK != 0 || o.bs > MAX_BS {
        return Err(format!("bs a whole number of {} KiB, at most {} KiB", BLOCK / KIB, MAX_BS / KIB));
    }
    if o.qd == 0 || o.qd > MAX_QD {
        return Err(format!("qd from 1 to {}", MAX_QD));
    }
    if o.secs == 0 || o.secs > MAX_SECS {
        return Err(format!("secs from 1 to {}", MAX_SECS));
    }
    Ok(o)
}

/* xorshift64*: a block per operation without the entropy pool in the way */
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// splitmix64's finish: a word of a verify block, from where and which.
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Verify block `block`'s bytes as its write `stamp` makes them: the magic,
/// the block, the stamp, then words of both -- a block torn, or another's,
/// reads as neither.
fn pattern(block: u64, stamp: u64, bs: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(bs);
    for i in 0..(bs / 8) as u64 {
        let word = match i {
            0 => MAGIC,
            1 => block,
            2 => stamp,
            _ => mix(block.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ stamp.rotate_left(32) ^ i),
        };
        data.extend_from_slice(&word.to_le_bytes());
    }
    data
}

/// What a verify block read holds, said: which block's write it is, or
/// that it is none.
fn reads_as(data: &[u8]) -> String {
    let word = |i: usize| data.get(i * 8..i * 8 + 8).map(|b| {
        let mut w = [0u8; 8];
        w.copy_from_slice(b);
        u64::from_le_bytes(w)
    });
    match (word(0), word(1), word(2)) {
        (Some(MAGIC), Some(b), Some(s)) if data == pattern(b, s, data.len()).as_slice() => {
            format!("block {}'s write {}", b, s)
        }
        (Some(MAGIC), Some(b), Some(s)) => format!("block {}'s write {}, torn", b, s),
        _ if data.iter().all(|&x| x == 0) => String::from("zeros"),
        _ => String::from("no write of nbsload's"),
    }
}

/// An operation out.
struct Flight {
    start: u64,
    write: bool,
    block: u64,
    /// A verify write's stamp.
    stamp: u64,
}

struct Run<'a> {
    o: &'a Options<'a>,
    d: Drive,
    blocks: u64,
    rng: Rng,
    cursor: u64,
    out: BTreeMap<u64, Flight>,
    /// The blocks an operation of `verify`'s is out on: one at a time each.
    busy: BTreeSet<u64>,
    /// `verify`'s: each block's last write done, by its stamp.
    written: BTreeMap<u64, u64>,
    stamp: u64,
    reads: Histogram,
    writes: Histogram,
    checked: u64,
    /// What went wrong: an operation failed, or a block holds what it
    /// should not.
    failed: Option<String>,
    wrong: Vec<String>,
    wrong_count: u64,
}

impl Run<'_> {
    /// The next block: at random, or the next in turn -- for `verify` one
    /// no operation is out on.
    fn pick(&mut self) -> Option<u64> {
        for _ in 0..64 {
            let b = if self.o.mode.random() {
                self.rng.next() % self.blocks
            } else {
                let b = self.cursor % self.blocks;
                self.cursor += 1;
                b
            };
            if !self.busy.contains(&b) {
                return Some(b);
            }
        }
        None
    }

    /// One operation started.
    fn start(&mut self) -> Result<(), String> {
        let Some(block) = self.pick() else { return Ok(()) };
        let offset = block * self.o.bs;
        let write = match self.o.mode {
            Mode::Verify => self.rng.next() % 10 < 6 || self.written.is_empty(),
            m => m.writes(),
        };
        let (op, data, stamp) = if write {
            self.stamp += 1;
            let data = if self.o.mode == Mode::Verify {
                pattern(block, self.stamp, self.o.bs as usize)
            } else {
                let mut d = Vec::new();
                d.try_reserve_exact(self.o.bs as usize).map_err(|_| String::from("no memory for a write"))?;
                d.resize(self.o.bs as usize, (block ^ self.stamp) as u8);
                d
            };
            (Op::Write { offset }, data, self.stamp)
        } else {
            (Op::Read { offset, len: self.o.bs as u32 }, Vec::new(), 0)
        };
        let tag = self.d.submit(op, data)?;
        if self.o.mode == Mode::Verify {
            self.busy.insert(block);
        }
        self.out.insert(tag, Flight { start: boot_time_ns(), write, block, stamp });
        Ok(())
    }

    /// An operation done: its time, and for `verify` what it wrote or read.
    fn finish(&mut self, done: Done) {
        let Some(f) = self.out.remove(&done.tag) else { return };
        self.busy.remove(&f.block);
        let took = boot_time_ns().saturating_sub(f.start);
        if !done.ok {
            if self.failed.is_none() {
                let what = if f.write { "write" } else { "read" };
                self.failed = Some(format!("a {} at block {} failed: {}", what, f.block,
                                           self.why().unwrap_or_else(|| String::from("said in the log"))));
            }
            /* A write that failed may be there or not */
            if f.write {
                self.written.remove(&f.block);
            }
            return;
        }
        if f.write {
            self.writes.record(took);
            if self.o.mode == Mode::Verify {
                self.written.insert(f.block, f.stamp);
            }
        } else {
            self.reads.record(took);
            if self.o.mode == Mode::Verify {
                self.check(f.block, &done.data, "as it went");
            }
        }
    }

    /// Block `block` read as `data`: held to its last write done.
    fn check(&mut self, block: u64, data: &[u8], when: &str) {
        let Some(&stamp) = self.written.get(&block) else { return };
        self.checked += 1;
        if data == pattern(block, stamp, data.len()).as_slice() {
            return;
        }
        self.wrong_count += 1;
        if self.wrong.len() < SAID_MAX {
            self.wrong.push(format!("block {} (byte {}), read {}, holds {} -- its last write done is {}", block,
                                    block * self.o.bs, when, reads_as(data), stamp));
        }
    }

    fn why(&self) -> Option<String> {
        match self.d.session().phase() {
            nbsclient::Phase::Failed(why) => Some(why.clone()),
            _ => None,
        }
    }

    /// Operations kept out until the deadline -- or the first failure, or
    /// the unload -- and those out then waited for.
    fn load(&mut self, deadline: u64) -> Result<(), String> {
        loop {
            let open = boot_time_ns() < deadline && self.failed.is_none() && !STOPPING.load(Ordering::Relaxed)
                && !matches!(self.d.session().phase(), nbsclient::Phase::Failed(_));
            if open {
                while (self.out.len() as u64) < self.o.qd {
                    let before = self.out.len();
                    self.start()?;
                    if self.out.len() == before {
                        break;
                    }
                }
            }
            if self.out.is_empty() {
                return Ok(());
            }
            self.d.turn(TURN_NS);
            while let Some(done) = self.d.take_done() {
                self.finish(done);
            }
        }
    }

    /// `verify`'s end: every block written read back after the flush, qd
    /// at a time.
    fn read_back(&mut self) -> Result<u64, String> {
        let mut left: Vec<u64> = self.written.keys().copied().collect();
        let total = left.len() as u64;
        let mut out: BTreeMap<u64, u64> = BTreeMap::new();
        while !left.is_empty() || !out.is_empty() {
            if STOPPING.load(Ordering::Relaxed) {
                return Err(String::from("ended by the module's unload"));
            }
            while (out.len() as u64) < self.o.qd {
                let Some(b) = left.pop() else { break };
                let tag = self.d.submit(Op::Read { offset: b * self.o.bs, len: self.o.bs as u32 }, Vec::new())?;
                out.insert(tag, b);
            }
            self.d.turn(TURN_NS);
            while let Some(done) = self.d.take_done() {
                let Some(b) = out.remove(&done.tag) else { continue };
                if !done.ok {
                    return Err(format!("the read back of block {} failed: {}", b,
                                       self.why().unwrap_or_else(|| String::from("said in the log"))));
                }
                self.check(b, &done.data, "back after the flush");
            }
        }
        Ok(total)
    }
}

/// `n` operations of `bs` bytes in `elapsed` ns: a line of the report.
fn rate(out: &mut Output, what: &str, h: &Histogram, bs: u64, elapsed: u64) {
    if h.ios == 0 {
        return;
    }
    let iops = (h.ios as u128 * NS_PER_SEC as u128 / elapsed as u128) as u64;
    let tenths_mib = (h.ios as u128 * bs as u128 * 10 * NS_PER_SEC as u128 / elapsed as u128 / MIB as u128) as u64;
    let _ = writeln!(out, "  {} {}: {} IOPS, {}.{} MiB/s", h.ios, what, iops, tenths_mib / 10, tenths_mib % 10);
    let _ = writeln!(out, "  {} latency us: {}", what, h.line());
}

fn run(args: &str, out: &mut Output) -> Result<(), String> {
    STOPPING.store(false, Ordering::Relaxed);
    let o = parse(args)?;
    let me = nbsdrive::this_host()?;
    let d = Drive::open(o.volume, &me, o.mode.writes(), o.force, Some("nbsload"))?;
    let size = d.session().size();
    let span = match (o.span, o.mode) {
        (Some(s), _) => s.min(size),
        (None, Mode::Verify) => VERIFY_SPAN.min(size),
        (None, _) => size,
    };
    let blocks = span / o.bs;
    if blocks == 0 {
        let _ = d.stop();
        return Err(format!("{} has no block of {} KiB in {} MiB", o.volume, o.bs / KIB, span / MIB));
    }
    let _ = writeln!(out, "nbsload {}: {}, bs {} KiB, qd {}, {} s over {} MiB of its {} MiB, generation {}",
                     o.volume, o.mode.name(), o.bs / KIB, o.qd, o.secs, span / MIB, size / MIB,
                     d.session().generation());
    let seed = kcore::random::random_u64().unwrap_or(0x9E37_79B9_7F4A_7C15) | 1;
    let mut r = Run { o: &o, d, blocks, rng: Rng(seed), cursor: 0, out: BTreeMap::new(), busy: BTreeSet::new(),
                      written: BTreeMap::new(), stamp: 0, reads: Histogram::new(), writes: Histogram::new(),
                      checked: 0, failed: None, wrong: Vec::new(), wrong_count: 0 };
    let start = boot_time_ns();
    let loaded = r.load(start + o.secs * NS_PER_SEC);
    let elapsed = boot_time_ns().saturating_sub(start).max(1);
    let _ = writeln!(out, "  {}.{:02} s{}", elapsed / NS_PER_SEC, (elapsed % NS_PER_SEC) / (NS_PER_SEC / 100),
                     if STOPPING.load(Ordering::Relaxed) { ", ended by the module's unload" } else { "" });
    rate(out, "reads", &r.reads, o.bs, elapsed);
    rate(out, "writes", &r.writes, o.bs, elapsed);
    let mut result = loaded;
    if o.mode.writes() && r.failed.is_none() && result.is_ok() {
        let begin = boot_time_ns();
        result = r.d.flush();
        let took = boot_time_ns().saturating_sub(begin);
        let _ = writeln!(out, "  flush {} in {}.{:03} ms", if result.is_ok() { "done" } else { "FAILED" },
                         took / NS_PER_MS, (took % NS_PER_MS) / NS_PER_US);
    }
    if o.mode == Mode::Verify && r.failed.is_none() && result.is_ok() {
        let during = r.checked;
        match r.read_back() {
            Ok(n) => {
                let _ = writeln!(out, "  verify: {} reads held to their blocks' last writes as it went, {} blocks \
                                       read back after the flush", during, n);
            }
            Err(e) => result = Err(e),
        }
    }
    for line in &r.wrong {
        let _ = writeln!(out, "  WRONG: {}", line);
    }
    let stopped = r.d.stop();
    if let Some(why) = r.failed.take() {
        return Err(why);
    }
    if r.wrong_count > 0 {
        return Err(format!("{} blocks did not hold their last write", r.wrong_count));
    }
    result.and(stopped)
}
