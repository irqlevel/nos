//! The machine's disks: `block::BlockDriver`s whose medium is the fuzzer's.
//!
//! A disk here is what a real one is to the kernel, and what each input
//! makes of it: its geometry -- a sector of any size the device may claim
//! -- and its medium, whose every byte is the input's to choose. And it is
//! a device with a volatile write cache: a plain write is taken into the
//! cache and reaches the medium in no order the kernel can see, until a
//! flush -- or a forced write (FUA), which is on the medium when it
//! completes. What a power cut would leave behind is the medium, and of the
//! cache whatever the device happened to have put down: `crash_at` takes
//! that picture at a chosen request, for a target to hold the filesystem on
//! it to what its commit order promises.
//!
//! A request can fail -- one of them, or every one from some point on, as a
//! device does that has gone -- for the error paths to be walked. And the
//! kernel's rules for a block device are checked where they are broken,
//! each a finding: a request past the end of the device, one that is not
//! whole sectors, synchronous I/O where the caller may not sleep, and a
//! write to a disk nothing should be writing to -- a filesystem mounted
//! read-only, a partition table being read.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use ffi::block::BlockIo;
use kcore::block::{SubmitError, IO_FLUSH, IO_READ, IO_WRITE};

use super::sched;

/// The disks there are.
pub const DISKS: usize = 6;

/// What a medium is kept in: a page of it at a time, a page never written
/// reading as zeros -- so a disk is as big as its geometry says, and costs
/// what is written to it.
const CHUNK: usize = 4096;

/// How long a request takes: a task waiting on one lets the others run, as
/// it does on a machine.
const IO_NS: u64 = 50_000;

/// Requests that have waited on their disk's latency: begun, and woken. One
/// begun and not woken has its caller asleep in the middle of its call --
/// with whatever lock that call holds -- which the clock alone ends.
pub static ASLEEP: AtomicU64 = AtomicU64::new(0);
pub static WOKEN: AtomicU64 = AtomicU64::new(0);

/// Asynchronous I/O handed to the disks, finished, and found full.
pub static SUBMITTED: AtomicU64 = AtomicU64::new(0);
pub static FINISHED: AtomicU64 = AtomicU64::new(0);
pub static FULL: AtomicU64 = AtomicU64::new(0);

/// How often a disk's interrupt looks for asynchronous I/O to finish while
/// it has some, and how long one takes at most past its doorbell.
const ASYNC_TICK_NS: u64 = 10_000;
const ASYNC_NS: u64 = 100_000;

/// One asynchronous I/O the device has: what it was handed, whether its
/// doorbell has been rung -- a device fetches nothing before that -- and
/// when it is done. Carried out when it is done, not before: so a caller
/// that touched its memory before the completion -- a write's data changed,
/// a read's taken -- has what a device would have left it.
struct Flying {
    op: u8,
    sector: u64,
    len: usize,
    phys: usize,
    done: extern "C" fn(*mut u8, i32),
    ctx: usize,
    due: Option<u64>,
}

#[derive(Clone, Default)]
pub struct Media {
    chunks: BTreeMap<u64, Box<[u8; CHUNK]>>,
}

impl Media {
    pub fn new() -> Media {
        Media::default()
    }

    /// A medium holding `data` from byte 0.
    pub fn from_bytes(data: &[u8]) -> Media {
        let mut m = Media::new();
        for (i, chunk) in data.chunks(CHUNK).enumerate() {
            if chunk.iter().any(|b| *b != 0) {
                m.write(i as u64 * CHUNK as u64, chunk);
            }
        }
        m
    }

    /* Offsets wrap: a device may claim 2^64 sectors, and the medium is as
     * big as the numbers go. */
    pub fn read(&self, at: u64, buf: &mut [u8]) {
        let mut done = 0;
        while done < buf.len() {
            let pos = at.wrapping_add(done as u64);
            let (index, off) = (pos / CHUNK as u64, (pos % CHUNK as u64) as usize);
            let take = (CHUNK - off).min(buf.len() - done);
            match self.chunks.get(&index) {
                Some(chunk) => buf[done..done + take].copy_from_slice(&chunk[off..off + take]),
                None => buf[done..done + take].fill(0),
            }
            done += take;
        }
    }

    pub fn write(&mut self, at: u64, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let pos = at.wrapping_add(done as u64);
            let (index, off) = (pos / CHUNK as u64, (pos % CHUNK as u64) as usize);
            let take = (CHUNK - off).min(data.len() - done);
            let chunk = self.chunks.entry(index).or_insert_with(|| Box::new([0u8; CHUNK]));
            chunk[off..off + take].copy_from_slice(&data[done..done + take]);
            done += take;
        }
    }

    pub fn bytes(&self, at: u64, len: usize) -> Vec<u8> {
        let mut v = sched::harness(|| vec![0u8; len]);
        self.read(at, &mut v);
        v
    }

    /// The first `len` bytes, whole: for a tool that wants an image file.
    pub fn to_vec(&self, len: u64) -> Vec<u8> {
        self.bytes(0, len as usize)
    }

    /// Everything written of this medium, onto `dst` from byte `at`: an
    /// image put on a partition.
    pub fn copy_into(&self, dst: &mut Media, at: u64) {
        for (&i, chunk) in &self.chunks {
            dst.write(at + i * CHUNK as u64, &chunk[..]);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
    /// A write forced to the medium
    WriteFua,
    Flush,
}

/// One request, as the device saw it.
#[derive(Clone, Copy, Debug)]
pub struct Io {
    pub op: Op,
    pub sector: u64,
    pub count: u64,
    pub ok: bool,
}

pub struct Disk {
    pub name: String,
    pub sector_size: u64,
    pub sectors: u64,
    /// What a read sees: the medium, with the cache's writes over it.
    pub current: Media,
    /// What is on the medium: what the power going leaves of it.
    pub durable: Media,
    /// Writes taken and not yet on the medium, a sector each, oldest first.
    pub cache: Vec<(u64, Vec<u8>)>,
    /// Whether the device has a write cache at all: without one a write is
    /// on the medium when it completes.
    pub volatile: bool,
    /// Every request, in order.
    pub log: Vec<Io>,
    /// Requests, by their number in `log`, that fail.
    pub fail: BTreeSet<u64>,
    /// From this request on every one fails: the device has gone.
    pub dead_from: Option<u64>,
    /// Power cuts: at these request numbers, what the medium would hold is
    /// kept in `crashes` -- before the request.
    pub crash_at: BTreeSet<u64>,
    pub crashes: Vec<(u64, Media)>,
    /// Why nothing may write to it now, if nothing may.
    pub no_writes: Option<String>,
    /// Whether a request is a scheduling point.
    pub latency: bool,
    /// The table's handle, once registered.
    pub handle: usize,
    /// Which of the cache's writes a power cut finds on the medium.
    chaos: u64,
    /// Whether it takes asynchronous I/O (`BlockDriver::submit`) -- the
    /// most bytes one moves -- and how many at once, past which it is busy.
    pub asynchronous: Option<usize>,
    pub depth: usize,
    /// Its asynchronous I/O not yet done, and the timer that does it.
    flying: Vec<Flying>,
    timer: usize,
}

impl Disk {
    /// A disk of `sectors` sectors of `sector_size` bytes, holding `media`.
    pub fn new(name: &str, sector_size: u64, sectors: u64, media: Media) -> Disk {
        Disk {
            name: name.to_string(),
            sector_size,
            sectors,
            current: media.clone(),
            durable: media,
            cache: Vec::new(),
            volatile: false,
            log: Vec::new(),
            fail: BTreeSet::new(),
            dead_from: None,
            crash_at: BTreeSet::new(),
            crashes: Vec::new(),
            no_writes: None,
            latency: false,
            handle: 0,
            chaos: 0x2545_F491_4F6C_DD1D,
            asynchronous: None,
            depth: 0,
            flying: Vec::new(),
            timer: 0,
        }
    }

    /// The doorbell: what was handed over before it is the device's to do,
    /// each done a while after.
    fn ring(&mut self) {
        let now = sched::now();
        for k in 0..self.flying.len() {
            if self.flying[k].due.is_none() {
                let after = 1 + self.chaos_next() % ASYNC_NS;
                self.flying[k].due = Some(now + after);
            }
        }
    }

    /// What a power cut keeps of the cache.
    pub fn seed_chaos(&mut self, seed: u64) {
        self.chaos = seed | 1;
    }

    /// The asynchronous path, as an NVMe disk's: commands of up to `max`
    /// bytes, `depth` of them at once.
    pub fn take_async(&mut self, max: usize, depth: usize) {
        self.asynchronous = Some(max);
        self.depth = depth;
    }

    /// `data` written at byte `at`: on the medium where the device has no
    /// cache or `fua` says so, else a sector at a time into the cache.
    fn put(&mut self, at: u64, data: &[u8], fua: bool) {
        self.current.write(at, data);
        if !self.volatile {
            self.durable.write(at, data);
            return;
        }
        let ss = self.sector_size as usize;
        let first = at / self.sector_size;
        if fua {
            /* On the medium now, and whatever the cache held of these
             * sectors superseded. */
            self.durable.write(at, data);
            let end = first + (data.len() / ss) as u64;
            self.cache.retain(|(s, _)| *s < first || *s >= end);
        } else {
            for (i, piece) in data.chunks(ss).enumerate() {
                self.cache.push((first + i as u64, piece.to_vec()));
            }
        }
    }

    fn chaos_next(&mut self) -> u64 {
        let mut x = self.chaos;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.chaos = x;
        x
    }

    /// What the medium would hold were the power to go now: what is on it,
    /// and of the cache's writes each one the device may have put down --
    /// in the order they came, a later one over an earlier.
    pub fn power_cut(&mut self) -> Media {
        let mut m = self.durable.clone();
        let cache = std::mem::take(&mut self.cache);
        for (sector, data) in &cache {
            if self.chaos_next() % 2 == 0 {
                m.write(sector.wrapping_mul(self.sector_size), data);
            }
        }
        self.cache = cache;
        m
    }

    /// Everything in the cache onto the medium.
    fn flush_cache(&mut self) {
        for (sector, data) in std::mem::take(&mut self.cache) {
            self.durable.write(sector.wrapping_mul(self.sector_size), &data);
        }
    }

    /// Whether request `n` fails.
    fn fails(&self, n: u64) -> bool {
        self.fail.contains(&n) || self.dead_from.is_some_and(|d| n >= d)
    }

}

static DISK: Mutex<[Option<Disk>; DISKS]> = Mutex::new([const { None }; DISKS]);

/// `f` over disk `index`, as the fuzzer's own code.
pub fn with<R>(index: usize, f: impl FnOnce(&mut Disk) -> R) -> R {
    sched::harness(|| {
        let mut g = DISK.lock().unwrap_or_else(|e| e.into_inner());
        f(g[index].as_mut().expect("a disk the input made"))
    })
}

/// Puts `disk` in slot `index` -- its driver registered with the block layer
/// by `register`.
pub fn insert(index: usize, disk: Disk) {
    sched::harness(|| DISK.lock().unwrap_or_else(|e| e.into_inner())[index] = Some(disk));
}

/// Registers disk `index` with the block layer, as its driver's probe
/// does: the device, or None when the table is full.
pub fn register(index: usize) -> Option<block::Disk> {
    let name = with(index, |d| d.name.clone());
    let dev = block::register_driver(&name, &DRIVERS[index])?;
    with(index, |d| d.handle = dev.handle());
    Some(dev)
}

/// How many requests have failed on every disk there is, in all: what a
/// target tells a call refused for a failed request apart from one refused
/// for no reason by.
pub fn failures() -> u64 {
    sched::harness(|| {
        let g = DISK.lock().unwrap_or_else(|e| e.into_inner());
        g.iter().flatten().map(|d| d.log.iter().filter(|io| !io.ok).count() as u64).sum()
    })
}

/// Which slot's disk the block layer's handle `handle` is, if one is.
pub fn index_of(handle: usize) -> Option<usize> {
    sched::harness(|| {
        let g = DISK.lock().unwrap_or_else(|e| e.into_inner());
        g.iter().position(|d| d.as_ref().is_some_and(|d| d.handle == handle && handle != 0))
    })
}

pub struct RamDisk {
    index: usize,
}

pub static DRIVERS: [RamDisk; DISKS] = [
    RamDisk { index: 0 },
    RamDisk { index: 1 },
    RamDisk { index: 2 },
    RamDisk { index: 3 },
    RamDisk { index: 4 },
    RamDisk { index: 5 },
];

impl RamDisk {
    /// One request: checked as the block layer's contract with a driver
    /// says it is, logged, failed if the input says so, and done -- `op`
    /// over the disk and the byte offset it starts at.
    fn request(&self, op: Op, sector: u64, len: usize, run: impl FnOnce(&mut Disk, u64)) -> bool {
        /* A synchronous request waits for the device: never where the
         * caller may not sleep. */
        sched::check_may_wait("does synchronous block I/O");
        let ok = with(self.index, |d| {
            /* Its command goes in after any asynchronous one still owed a
             * doorbell, and the one doorbell covers them all, as one queue
             * of a device's does. */
            d.ring();
            if op != Op::Flush {
                let ss = d.sector_size;
                if ss == 0 || len == 0 || len as u64 % ss != 0 {
                    panic!("invariant: disk {} handed a request of {} bytes, which is not whole sectors of {}",
                           d.name, len, ss);
                }
                let count = len as u64 / ss;
                if sector.checked_add(count).is_none_or(|end| end > d.sectors) {
                    panic!("invariant: disk {} handed a request for {} sectors at {}, past its end at {}: the \
                            block layer lets nothing outside a device reach its driver", d.name, count, sector,
                           d.sectors);
                }
            }
            if matches!(op, Op::Write | Op::WriteFua) {
                if let Some(why) = &d.no_writes {
                    panic!("invariant: disk {} written at sector {} while {}", d.name, sector, why);
                }
            }
            let n = d.log.len() as u64;
            if d.crash_at.contains(&n) {
                let m = d.power_cut();
                d.crashes.push((n, m));
            }
            let count = if op == Op::Flush { 0 } else { len as u64 / d.sector_size };
            let ok = !d.fails(n);
            if super::ECHO_TRACE.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!("[{:>12}] {} #{} {:?} {}+{}{}", sched::now() / 1000, d.name, n, op, sector, count,
                          if ok { "" } else { " FAILS" });
            }
            d.log.push(Io { op, sector, count, ok });
            if ok {
                run(d, sector.wrapping_mul(d.sector_size));
            }
            ok
        });
        if with(self.index, |d| d.latency) {
            ASLEEP.fetch_add(1, Ordering::SeqCst);
            sched::sleep_ns(IO_NS);
            WOKEN.fetch_add(1, Ordering::SeqCst);
        }
        ok
    }
}

/// A disk's interrupt, a tick at a time while it has asynchronous I/O: what
/// is due done -- logged, failed or put on the medium as a synchronous
/// request is -- and its caller told, in interrupt context; the tick
/// stopped once nothing is left.
extern "C" fn finish_async(ctx: *mut u8) {
    let index = ctx as usize;
    let now = sched::now();
    let (calls, stop) = with(index, |d| {
        let mut due = Vec::new();
        let mut k = 0;
        while k < d.flying.len() {
            if d.flying[k].due.is_some_and(|t| t <= now) {
                due.push(d.flying.swap_remove(k));
            } else {
                k += 1;
            }
        }
        /* In an order of the input's: a device finishes what it has as it
         * likes. */
        for k in (1..due.len()).rev() {
            let j = (d.chaos_next() % (k as u64 + 1)) as usize;
            due.swap(k, j);
        }
        if due.len() > 1 {
            crate::reached("a disk's asynchronous I/O finished several at once");
        }
        let mut calls = Vec::new();
        for io in due {
            let status = if carry_out(d, &io) { 0 } else { -1 };
            calls.push((io.done, io.ctx, status));
        }
        let stop = (d.flying.is_empty() && d.timer != 0).then(|| std::mem::replace(&mut d.timer, 0));
        (calls, stop)
    });
    if let Some(timer) = stop {
        sched::timer_stop(timer);
    }
    for &(done, ctx, status) in &calls {
        FINISHED.fetch_add(1, Ordering::Relaxed);
        done(ctx as *mut u8, status);
    }
    sched::harness(|| drop(calls));
}

/// An asynchronous I/O done: its memory still the caller's -- DMA pages
/// given out and not given back, or the device writes into what the kernel
/// freed under it -- and the request logged, failed and carried out as a
/// synchronous one is. Whether it went well.
fn carry_out(d: &mut Disk, io: &Flying) -> bool {
    if io.op != IO_FLUSH && !super::ffi::dma_live(io.phys, io.len) {
        panic!("invariant: disk {} finished an asynchronous I/O of {} bytes into memory at {:#x} that is no longer \
                DMA pages the kernel holds: freed with I/O in flight", d.name, io.len, io.phys);
    }
    let op = match io.op {
        IO_READ => Op::Read,
        IO_WRITE => Op::Write,
        _ => Op::Flush,
    };
    if op == Op::Write {
        if let Some(why) = &d.no_writes {
            panic!("invariant: disk {} written at sector {} while {}", d.name, io.sector, why);
        }
    }
    let n = d.log.len() as u64;
    if d.crash_at.contains(&n) {
        let m = d.power_cut();
        d.crashes.push((n, m));
    }
    let count = if op == Op::Flush { 0 } else { io.len as u64 / d.sector_size };
    let ok = !d.fails(n);
    if super::ECHO_TRACE.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("[{:>12}] {} #{} {:?} {}+{} async{}", sched::now() / 1000, d.name, n, op, io.sector, count,
                  if ok { "" } else { " FAILS" });
    }
    d.log.push(Io { op, sector: io.sector, count, ok });
    if ok {
        let at = io.sector.wrapping_mul(d.sector_size);
        match op {
            /* Pages the kernel holds, checked just now. */
            Op::Read => d.current.read(at, unsafe { std::slice::from_raw_parts_mut(io.phys as *mut u8, io.len) }),
            Op::Write => {
                let data = unsafe { std::slice::from_raw_parts(io.phys as *const u8, io.len) }.to_vec();
                d.put(at, &data, false);
            }
            _ => d.flush_cache(),
        }
    }
    ok
}

impl block::BlockDriver for RamDisk {
    fn capacity(&self) -> u64 {
        with(self.index, |d| d.sectors)
    }

    fn is_async(&self) -> bool {
        with(self.index, |d| d.asynchronous.is_some())
    }

    fn submit_max(&self) -> usize {
        with(self.index, |d| d.asynchronous.unwrap_or(0))
    }

    /// Checked as the kernel's contract with an asynchronous driver has it
    /// -- inside the device, whole sectors, no more than it moves at once,
    /// memory the caller holds as DMA pages -- and kept until its doorbell
    /// and its time: busy past `depth` of them, as a device's queue is.
    fn submit(&'static self, io: &BlockIo, kick: bool) -> Result<(), SubmitError> {
        let index = self.index;
        let started = with(index, |d| {
            let Some(max) = d.asynchronous else { return Err(SubmitError::Unsupported) };
            let len = io.count as usize * d.sector_size as usize;
            if io.op != IO_FLUSH {
                if io.op != IO_READ && io.op != IO_WRITE {
                    return Err(SubmitError::Invalid);
                }
                if io.count == 0 || len > max || io.phys % 4 != 0 {
                    panic!("invariant: disk {} handed an asynchronous I/O of {} sectors at {:#x}, which it does not \
                            take: at most {} bytes, dword aligned", d.name, io.count, io.phys, max);
                }
                if io.sector.checked_add(u64::from(io.count)).is_none_or(|end| end > d.sectors) {
                    panic!("invariant: disk {} handed an asynchronous I/O for {} sectors at {}, past its end at {}",
                           d.name, io.count, io.sector, d.sectors);
                }
                if !super::ffi::dma_live(io.phys as usize, len) {
                    panic!("invariant: disk {} handed an asynchronous I/O of {} bytes at {:#x}, which is no DMA \
                            pages' the kernel holds", d.name, len, io.phys);
                }
            }
            if d.flying.len() >= d.depth {
                FULL.fetch_add(1, Ordering::Relaxed);
                crate::reached("a disk busy with asynchronous I/O");
                return Err(SubmitError::Busy);
            }
            d.flying.push(Flying { op: io.op, sector: io.sector, len, phys: io.phys as usize, done: io.done,
                                   ctx: io.ctx as usize, due: None });
            if kick {
                d.ring();
            }
            SUBMITTED.fetch_add(1, Ordering::Relaxed);
            Ok(d.timer == 0)
        });
        let Ok(start_timer) = started else { return started.map(|_| ()) };
        if start_timer {
            let timer = sched::timer_start(finish_async, index, ASYNC_TICK_NS);
            with(index, |d| d.timer = timer);
        }
        Ok(())
    }

    fn kick(&'static self) {
        with(self.index, |d| d.ring());
    }

    fn sector_size(&self) -> u64 {
        with(self.index, |d| d.sector_size)
    }

    fn read(&'static self, sector: u64, buf: &mut [u8]) -> bool {
        let len = buf.len();
        self.request(Op::Read, sector, len, |d, at| d.current.read(at, buf))
    }

    fn write(&'static self, sector: u64, data: &[u8], fua: bool) -> bool {
        let op = if fua { Op::WriteFua } else { Op::Write };
        self.request(op, sector, data.len(), |d, at| d.put(at, data, fua))
    }

    fn flush(&'static self) -> bool {
        self.request(Op::Flush, 0, 0, |d, _| d.flush_cache())
    }

}
