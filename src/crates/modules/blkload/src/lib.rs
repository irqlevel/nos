#![no_std]

//! blkload: a short load test on a block device -- a disk, or a partition
//! of one -- run from the shell, which reports what it got: IOPS,
//! bandwidth, and latency down to the percentiles.
//!
//!     insmod /blkload.ko
//!     blkload nvme01 randread qd=8
//!     blkload vdb1 randwrite bs=8k qd=4 secs=10
//!
//! qd tasks each keep one I/O in flight, synchronously: the queue depth
//! asked of the driver, which passes on as many as it has room for -- 8 at
//! a time for virtio-blk, 63 for NVMe. What a driver takes in one I/O caps
//! bs -- a page for virtio-blk, two for NVMe -- and past it the first I/O
//! fails; blkload then finds the largest size that does go through and says
//! so. Latency runs from submission to the task running again: the kernel's
//! waits yield rather than sleep, so it includes waiting for a CPU.
//!
//! Reads are always allowed. A write test destroys
//! what is on the device, so it has to be asked for by name, and it claims
//! the device first: refused while a mounted filesystem, the disk log or
//! another writer holds it -- or the disk it is a partition of, or a
//! partition of it -- whatever else is said, and holding all of those off
//! until it is done. Unless `force`, it is refused too on a disk with
//! partitions and on anything that starts with an ext2 or nanofs
//! filesystem, a partition table, a boot sector or a prepared disk log area.

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kcore::block::Disk;
use kcore::cmd::{Command, Output};
use kcore::dma::DmaBuffer;
use kcore::time::boot_time_ns;

const HELP: &str = "blkload <dev> [randread|randwrite|read|write] [bs=4k] [qd=1] [secs=5] [force] - disk load test";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const PAGE_SIZE: u64 = 4096;
const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const NS_PER_SEC: u64 = 1_000_000_000;
const NS_PER_MS: u64 = 1_000_000;
const NS_PER_US: u64 = 1_000;

const DEFAULT_BS: u64 = 4 * KIB;
/* A worker's buffer is one physically contiguous DMA run, and the page
   allocator's largest is 512 KiB */
const MAX_BS: u64 = 512 * KIB;
const MAX_QD: u64 = 64;
const DEFAULT_SECS: u64 = 5;
const MAX_SECS: u64 = 60;

/* ext2's superblock is 1024 bytes in, and its magic 56 bytes into that */
const EXT2_MAGIC_AT: usize = 1024 + 56;
const EXT2_MAGIC: u16 = 0xEF53;
/* An MBR -- GPT's protective one included -- and a boot sector end their
   first sector with this */
const MBR_SIGNATURE_AT: usize = 510;
const MBR_SIGNATURE: u16 = 0xAA55;
/* A disk log area prepared for kernel/disklog.cpp begins with "NOSLOG1" */
const DISKLOG_MAGIC: u64 = 0x0031_474F_4C53_4F4E;
/* nanofs, `format nanofs`'s, begins its superblock -- the device's first
   block -- with this (fs/nanofs.h) */
const NANOFS_MAGIC: u32 = 0x4E41_4E4F;
/* Enough of a device's start to hold every one of them */
const PEEK_BYTES: u64 = 4 * KIB;

/* Latency histogram: 2^SUB_BITS buckets to each power of two, so a
   percentile comes out within 1/16 of the truth */
const SUB_BITS: u32 = 4;
const SUB: usize = 1 << SUB_BITS;
const BUCKETS: usize = 64 * SUB;

/* The ten-thousandths the report gives the latency at */
const P50: u64 = 5_000;
const P90: u64 = 9_000;
const P99: u64 = 9_900;
const P999: u64 = 9_990;

struct BlkLoad {
    _cmd: Command,
}

impl kmod::Module for BlkLoad {}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let cmd = Command::register("blkload", HELP, |args, out| {
        if let Err(problem) = run(args, out) {
            let _ = writeln!(out, "blkload: {}", problem);
        }
    })?;

    Ok(Box::new(BlkLoad { _cmd: cmd }))
}

kmod::module!(name: "blkload", init: init);

#[derive(Clone, Copy, PartialEq)]
enum Pattern {
    Random,
    Sequential,
}

struct Options<'a> {
    device: &'a str,
    pattern: Pattern,
    write: bool,
    bs: u64,
    qd: u64,
    secs: u64,
    force: bool,
}

impl Options<'_> {
    fn mode(&self) -> &'static str {
        match (self.pattern, self.write) {
            (Pattern::Random, false) => "randread",
            (Pattern::Random, true) => "randwrite",
            (Pattern::Sequential, false) => "read",
            (Pattern::Sequential, true) => "write",
        }
    }
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
    let device = words
        .next()
        .ok_or_else(|| format!("which device? `disks` lists them\nusage: {}", HELP))?;

    let mut opts = Options {
        device,
        pattern: Pattern::Random,
        write: false,
        bs: DEFAULT_BS,
        qd: 1,
        secs: DEFAULT_SECS,
        force: false,
    };

    for word in words {
        match word {
            "randread" => (opts.pattern, opts.write) = (Pattern::Random, false),
            "randwrite" => (opts.pattern, opts.write) = (Pattern::Random, true),
            "read" => (opts.pattern, opts.write) = (Pattern::Sequential, false),
            "write" => (opts.pattern, opts.write) = (Pattern::Sequential, true),
            "force" => opts.force = true,
            _ => {
                let value = |v: Option<u64>| v.ok_or_else(|| format!("{}: not a number", word));
                match word.split_once('=') {
                    Some(("bs", v)) => opts.bs = value(parse_size(v))?,
                    Some(("qd", v)) => opts.qd = value(v.parse().ok())?,
                    Some(("secs", v)) => opts.secs = value(v.parse().ok())?,
                    _ => return Err(format!("what is '{}'?\nusage: {}", word, HELP)),
                }
            }
        }
    }

    Ok(opts)
}

/* A byte count the way a person would say it */
struct Size(u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (unit, name) = match self.0 {
            n if n >= GIB => (GIB, "GiB"),
            n if n >= MIB => (MIB, "MiB"),
            n if n >= KIB => (KIB, "KiB"),
            _ => (1, "bytes"),
        };
        write!(f, "{} {}", self.0 / unit, name)
    }
}

/* A size as bs= takes it back */
struct BsArg(u64);

impl fmt::Display for BsArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 % KIB == 0 {
            write!(f, "{}k", self.0 / KIB)
        } else {
            write!(f, "{}", self.0)
        }
    }
}

/* Nanoseconds, shown in microseconds to a tenth */
struct Micros(u64);

impl fmt::Display for Micros {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0 / NS_PER_US, (self.0 % NS_PER_US) / 100)
    }
}

fn bucket(ns: u64) -> usize {
    if ns < SUB as u64 {
        return ns as usize;
    }
    let msb = 63 - ns.leading_zeros();
    let shift = msb - SUB_BITS;
    let sub = ((ns >> shift) as usize) & (SUB - 1);
    (shift as usize + 1) * SUB + sub
}

/* [low, high) of a bucket, in nanoseconds */
fn bucket_range(b: usize) -> (u64, u64) {
    if b < SUB {
        return (b as u64, b as u64 + 1);
    }
    let shift = (b / SUB - 1) as u32;
    let sub = (b % SUB) as u64;
    ((SUB as u64 + sub) << shift, (SUB as u64 + sub + 1) << shift)
}

struct Stats {
    ios: u64,
    min: u64,
    max: u64,
    sum: u64,
    error_at: Option<u64>,
    hist: [u32; BUCKETS],
}

impl Stats {
    fn new() -> Self {
        Self { ios: 0, min: u64::MAX, max: 0, sum: 0, error_at: None, hist: [0; BUCKETS] }
    }

    fn record(&mut self, ns: u64) {
        self.ios += 1;
        self.sum += ns;
        self.min = self.min.min(ns);
        self.max = self.max.max(ns);
        self.hist[bucket(ns)] += 1;
    }

    fn merge(&mut self, other: &Stats) {
        self.ios += other.ios;
        self.sum += other.sum;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.error_at = self.error_at.or(other.error_at);
        for (mine, theirs) in self.hist.iter_mut().zip(other.hist.iter()) {
            *mine += *theirs;
        }
    }

    /* The latency per10k ten-thousandths of the ios came in under: the
       middle of the bucket that holds it */
    fn percentile(&self, per10k: u64) -> u64 {
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
}

/* xorshift64*: cheap enough to pick a block per I/O without the kernel's
   entropy pool in the way */
struct Rng(u64);

impl Rng {
    fn new(seed: u64, stream: u64) -> Self {
        let mixed = seed ^ (stream + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        Self(if mixed == 0 { 1 } else { mixed })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/* What every worker reads; only the atomics change while they run */
struct Shared {
    disk: Disk,
    write: bool,
    random: bool,
    bs: usize,
    block_sectors: u64,
    blocks: u64,
    deadline: u64,
    stop: AtomicBool,
    cursor: AtomicU64,
}

struct Worker {
    shared: *const Shared,
    rng: Rng,
    buf: DmaBuffer,
    stats: Stats,
}

extern "C" fn worker_main(ctx: *mut u8) {
    let worker = unsafe { &mut *(ctx as *mut Worker) };
    let shared = unsafe { &*worker.shared };
    let buf = &mut worker.buf.as_mut_slice()[..shared.bs];

    while !shared.stop.load(Ordering::Relaxed) && boot_time_ns() < shared.deadline {
        let block = match shared.random {
            true => worker.rng.next() % shared.blocks,
            false => shared.cursor.fetch_add(1, Ordering::Relaxed) % shared.blocks,
        };
        let sector = block * shared.block_sectors;

        let start = boot_time_ns();
        let done = match shared.write {
            true => shared.disk.write(sector, buf, false),
            false => shared.disk.read(sector, buf),
        };
        let end = boot_time_ns();

        if done.is_err() {
            worker.stats.error_at = Some(sector);
            shared.stop.store(true, Ordering::Relaxed);
            break;
        }
        worker.stats.record(end.saturating_sub(start));
    }
}

/* The largest transfer the device takes in one I/O, below bs: the powers of
   two under it, largest first, until a read at the start goes through. A
   driver's limit is a whole number of pages, so bs=12k finds 8k where halving
   would have stopped at 6k. For when the very first I/O failed, which is
   what asking a driver for more than it takes at once looks like. */
fn largest_transfer(disk: &Disk, bs: u64) -> Option<u64> {
    let sector_size = disk.sector_size();
    let mut buf = dma_buffer(bs).ok()?;
    /* bs is a whole number of sectors, so at least 512 */
    let mut size = 1u64 << (63 - (bs - 1).leading_zeros());
    while size >= sector_size {
        if size % sector_size == 0 && disk.read(0, &mut buf.as_mut_slice()[..size as usize]).is_ok() {
            return Some(size);
        }
        size /= 2;
    }
    None
}

fn dma_buffer(bytes: u64) -> Result<DmaBuffer, String> {
    let pages = ((bytes + PAGE_SIZE - 1) / PAGE_SIZE) as usize;
    DmaBuffer::new(pages).ok_or_else(|| format!("no memory for a {} buffer", Size(bytes)))
}

/* With the device claimed: unless told to go ahead anyway, no write test on
   a disk with partitions, nor on a device that starts with something worth
   keeping */
fn check_writable(disk: &Disk, opts: &Options) -> Result<(), String> {
    if opts.force {
        return Ok(());
    }

    if disk.partitions() != 0 {
        return Err(format!(
            "{} has partitions -- write to one of them, or add force to write over them",
            opts.device
        ));
    }

    let sector_size = disk.sector_size();
    let bytes = PEEK_BYTES.max(sector_size);
    if bytes > disk.sectors() * sector_size {
        return Ok(());
    }

    let mut buf = dma_buffer(bytes)?;
    let head = &mut buf.as_mut_slice()[..bytes as usize];
    disk.read(0, head).map_err(|_| format!("cannot read the start of {}", opts.device))?;

    let word = |at: usize| u16::from_le_bytes([head[at], head[at + 1]]);
    let mut first = [0u8; 8];
    first.copy_from_slice(&head[..8]);
    if u64::from_le_bytes(first) == DISKLOG_MAGIC {
        return Err(format!(
            "{} holds a prepared disk log area -- add force to write over it",
            opts.device
        ));
    }
    if word(EXT2_MAGIC_AT) == EXT2_MAGIC {
        return Err(format!("{} holds an ext2 filesystem -- add force to write over it", opts.device));
    }
    if u32::from_le_bytes([head[0], head[1], head[2], head[3]]) == NANOFS_MAGIC {
        return Err(format!("{} holds a nanofs filesystem -- add force to write over it", opts.device));
    }
    if word(MBR_SIGNATURE_AT) == MBR_SIGNATURE {
        return Err(format!(
            "{} starts with a partition table or a boot sector -- add force to write over it",
            opts.device
        ));
    }

    Ok(())
}

fn run(args: &str, out: &mut Output) -> Result<(), String> {
    let opts = parse(args)?;
    let disk = Disk::open(opts.device)
        .ok_or_else(|| format!("no block device {} -- `disks` lists them", opts.device))?;

    let sector_size = disk.sector_size();
    let sectors = disk.sectors();
    if sector_size == 0 || sectors == 0 {
        return Err(format!("{} is empty", opts.device));
    }
    if opts.bs < sector_size || opts.bs % sector_size != 0 || opts.bs > MAX_BS {
        return Err(format!(
            "bs {} does not suit {}: a whole number of its {}-byte sectors, at most {}",
            opts.bs, opts.device, sector_size, Size(MAX_BS)
        ));
    }
    if opts.qd == 0 || opts.qd > MAX_QD {
        return Err(format!("qd from 1 to {}", MAX_QD));
    }
    if opts.secs == 0 || opts.secs > MAX_SECS {
        return Err(format!("secs from 1 to {}", MAX_SECS));
    }

    let block_sectors = opts.bs / sector_size;
    let blocks = sectors / block_sectors;
    if blocks == 0 {
        return Err(format!("{} is smaller than one block of {}", opts.device, Size(opts.bs)));
    }

    /* Held to the end of the test: no mount, disk log or other writer can take
       the device -- or a disk or partition overlapping it -- meanwhile */
    let _claim = if opts.write {
        let claim = disk
            .claim()
            .map_err(|who| format!("{} is in use by {} -- not writing to it", opts.device, who))?;
        check_writable(&disk, &opts)?;
        Some(claim)
    } else {
        None
    };

    let mut bufs = Vec::new();
    for i in 0..opts.qd {
        let mut buf = dma_buffer(opts.bs)?;
        for (n, byte) in buf.as_mut_slice().iter_mut().enumerate() {
            *byte = (n as u64 ^ i) as u8;
        }
        bufs.push(buf);
    }

    let _ = writeln!(
        out,
        "blkload {}: {}, bs {}, qd {}, {} s over {}",
        opts.device,
        opts.mode(),
        Size(opts.bs),
        opts.qd,
        opts.secs,
        Size(sectors * sector_size)
    );

    let seed = kcore::random::random_u64().unwrap_or(0x9E37_79B9_7F4A_7C15);
    let start = boot_time_ns();
    let shared = Shared {
        disk,
        write: opts.write,
        random: opts.pattern == Pattern::Random,
        bs: opts.bs as usize,
        block_sectors,
        blocks,
        deadline: start + opts.secs * NS_PER_SEC,
        stop: AtomicBool::new(false),
        cursor: AtomicU64::new(0),
    };

    let mut workers: Vec<Box<Worker>> = bufs
        .into_iter()
        .enumerate()
        .map(|(i, buf)| {
            Box::new(Worker { shared: &shared, rng: Rng::new(seed, i as u64), buf, stats: Stats::new() })
        })
        .collect();

    /* Dropping a TaskHandle waits for its task, so every worker is done
       with `shared` and its buffer before this block ends */
    let started = {
        let mut tasks = Vec::new();
        for (i, worker) in workers.iter_mut().enumerate() {
            let ctx = &mut **worker as *mut Worker as *mut u8;
            match kcore::task::spawn_with_ctx(&format!("blkload/{}", i), worker_main, ctx) {
                Some(task) => tasks.push(task),
                None => {
                    shared.stop.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
        tasks.len() as u64
    };
    let elapsed = boot_time_ns().saturating_sub(start).max(1);

    if started != opts.qd {
        return Err(format!("could start only {} of {} tasks", started, opts.qd));
    }

    let mut total = Stats::new();
    for worker in workers.iter() {
        total.merge(&worker.stats);
    }
    report(out, &opts, &disk, &total, elapsed);

    if opts.write {
        let begin = boot_time_ns();
        let flushed = disk.flush();
        let took = boot_time_ns().saturating_sub(begin);
        let _ = writeln!(
            out,
            "  flush {} in {}.{:03} ms",
            if flushed.is_ok() { "done" } else { "FAILED" },
            took / NS_PER_MS,
            (took % NS_PER_MS) / NS_PER_US
        );
    }

    Ok(())
}

fn report(out: &mut Output, opts: &Options, disk: &Disk, total: &Stats, elapsed: u64) {
    let iops = (total.ios as u128 * NS_PER_SEC as u128 / elapsed as u128) as u64;
    let bytes = total.ios as u128 * opts.bs as u128;
    let tenths_mib = (bytes * 10 * NS_PER_SEC as u128 / elapsed as u128 / MIB as u128) as u64;

    let _ = writeln!(
        out,
        "  {} ios in {}.{:02} s: {} IOPS, {}.{} MiB/s",
        total.ios,
        elapsed / NS_PER_SEC,
        (elapsed % NS_PER_SEC) / (NS_PER_SEC / 100),
        iops,
        tenths_mib / 10,
        tenths_mib % 10
    );

    if total.ios != 0 {
        let _ = writeln!(
            out,
            "  latency us: min {}, avg {}, p50 {}, p90 {}, p99 {}, p99.9 {}, max {}",
            Micros(total.min),
            Micros(total.sum / total.ios),
            Micros(total.percentile(P50)),
            Micros(total.percentile(P90)),
            Micros(total.percentile(P99)),
            Micros(total.percentile(P999)),
            Micros(total.max)
        );
    }

    if let Some(sector) = total.error_at {
        let _ = writeln!(out, "  stopped by an I/O error at sector {}", sector);

        /* Nothing went through at all: most likely more in one I/O than the
           driver takes -- say what it does take */
        if total.ios == 0 {
            if let Some(size) = largest_transfer(disk, opts.bs) {
                let _ = writeln!(
                    out,
                    "  {} takes at most {} in one I/O -- try bs={}",
                    opts.device,
                    Size(size),
                    BsArg(size)
                );
            }
        }
    }
}
