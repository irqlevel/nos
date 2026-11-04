//! A disk's image: a file of nos's that a guest's disk is, held for as long
//! as the disk is open and read and written off the VFS's lock.
//!
//! The VFS holds a mount's lock across every call on its filesystem, down to
//! the device and back (`vfs`): a guest's disk read and written through it
//! waits behind every other guest's disk on that filesystem and behind nos's
//! own files there, a request at a time. An image takes the file's map
//! from the filesystem once, as it is opened (`Vfs::map_runs`), and holds
//! it (`Map`): every read and write after it is resolved there, no lock
//! taken, and goes to the device straight through a buffer of its own, in
//! batches the driver has in flight together.
//!
//! What makes that safe is the hold (`vfs::OPEN_HOLD`): while an image is
//! open nothing else may write the file, cut it, grow it, rename it or
//! remove it, so the blocks the map names stay the file's -- and nobody else
//! writes them -- until the image is closed, and nothing but the image
//! changes the map. The image's own task growing it ahead (`Ahead`) gives
//! the file blocks only where it has none: until the image takes them into
//! its map they are holes there, which read as the zeros they hold, and a
//! write into them waits for the task rather than growing the image over
//! what it gives. What only the filesystem can do is left to it: a write
//! into a hole needs blocks allocated and pointed at in order, so a batch
//! with a hole in it goes through the VFS as every write did -- the blocks
//! it touched taken into the map again after it, as a punch's are -- and the
//! next flush is then the filesystem's sync and not only the device's. A filesystem with no blocks of its own on a device
//! (`FileSystem::data_block_size` 0 -- ramfs) has its images read and
//! written through the VFS whole.
//!
//! A partial block -- a sector of a guest's in a filesystem block of 4 KiB
//! -- is read, changed and written back whole. That is safe because an
//! image is one writer's: its calls come one at a time (`&mut self`, and the
//! slot's lock across the C ABI), each waiting for its queue's I/O first,
//! and the hold keeps everyone else off the file.
//!
//! A write into a hole grows the file by the chunk around it first
//! (`Vfs::allocate`, `CHUNK_BYTES`) -- by twice as many as the last time
//! where the hole begins where that growth ended, to `GROW_MAX_CHUNKS` --
//! its blocks given to the file in one commit as unwritten extents, which
//! read as zeros and have nothing written to them, and the write then goes
//! to the device as an overwrite: a commit a chunk, where a write through
//! the VFS was a commit a batch. A guest writing its disk on from one end --
//! each write beginning where the last ended -- has the stretch ahead of it
//! given its blocks by a task of the image's own (`Ahead`), every hole in
//! it, so that its writes meet none and wait for no commit: on the EX44 the
//! disk's task of a guest's 2 GiB first write committed 28 times, 5-7 ms
//! each, 18 of them in its first 150 ms. What it wrote there is made written
//! at the guest's next flush (`Vfs::mark_written`), whose commit's own flush puts
//! the data on the medium first -- noted as written once it is done, not
//! when it is started. A guest's discard gives whole chunks back
//! to the filesystem (`discard`, `Vfs::punch`): holes from then on, as a
//! write of zeros with its unmap flag does with the chunks it covers whole
//! (`write_zeroes`), the rest of it zeros written. Blocks given
//! back may be the next file's the moment the punch is done -- which is safe
//! because, the image's calls coming one at a time and each waiting for its
//! queue's I/O first, no read or write of its own is under way off the VFS's
//! lock on a block it mapped before them.
//!
//! A guest's disk is served through the image's queue (`Flights`, `start`,
//! `reap`): buffers of its own, a request each, which the device reads and
//! writes as they are -- a read or a write of whole mapped blocks with the
//! device asynchronously, as many at once as the guest has out, each back
//! when it is done; anything else after everything in flight is done, by
//! the synchronous path above.
//!
//! A loadable module reaches one through the C ABI at the bottom -- an image
//! is a slot of this file's table and the generation of what is in it, looked
//! up on every call (`kcore::fs::Image`, `kcore::fs::ImageQueue`).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use block::queue::Queue;
use block::{Disk, Piece};
use kcore::block::SubmitError;
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::once::OnceBox;
use kcore::consts::NS_PER_SEC;
use kcore::sync::{Event, Mutex};
use kcore::task::{self, TaskHandle};
use kcore::time::{boot_time_ns, Duration};
use kcore::trace::{self, ImageNote, ImageSync};
use kcore::klog;

use crate::vfs::{Handle, MapRun, Vfs, OPEN_HOLD, OPEN_READ, OPEN_WRITE};
use ffi::fs::{ImageDone, IMAGE_QUEUE_MAX};

/// The most blocks a batch moves: a page of the buffer each -- 2 MiB, so
/// that every request a guest's disk has out at once (eight, of up to 256
/// KiB) goes to the device in one batch (`read_vec`, `write_vec`).
const BATCH_PAGES: usize = 512;

/// The most blocks `extent` looks at in one call: 16 MiB of 4 KiB blocks, a
/// map of a batch's at a time under the VFS's lock -- let go of between
/// them, and the caller's to stop between calls.
const EXTENT_BLOCKS: u64 = 4096;

/// The most blocks the map asks the filesystem about in one call: 16 GiB of
/// 4 KiB blocks -- a hole is one run however long, so a sparse image of
/// terabytes is a few calls, each under the mount's lock for no longer than
/// the runs it has take.
const MAP_WINDOW: u64 = 1 << 22;

/// What a write into a hole grows an image by (`Vfs::allocate`): the 16
/// MiB around it, given to the file as unwritten extents in one commit
/// rather than a commit for every batch the guest writes into it -- and
/// what a guest's discard gives back whole, the rest of its range left as
/// it is.
pub const CHUNK_BYTES: u64 = 16 << 20;

/// The most chunks a write into a hole grows an image by at once: one, then
/// twice the last for every hole that begins where the last growth ended --
/// a guest writing its disk from one end on, whose next write is into the
/// next hole -- up to 256 MiB of 16 MiB chunks. A commit and its flushes for
/// every 256 MiB the guest writes, not every 16; and no more than 256 MiB
/// given that it may never write.
pub const GROW_MAX_CHUNKS: u64 = 16;

/// What a growth gives ahead of the chunks its write needs, at most: an
/// eighth of the filesystem's free blocks, so that the last of the room is
/// not given to writes that may never come -- and a refusal of what is
/// ahead never sends a write that fits through the VFS.
const GROW_SHARE: u64 = 8;

/// How long a write into a hole waits for the growth the image's task is
/// making over it before the wait is said in the log -- and again every
/// time as long goes by (`Direct::wait_ahead`).
const AHEAD_SAY_NS: u64 = 10 * NS_PER_SEC;

/// The step of that wait.
const AHEAD_STEP_MS: u64 = 100;

/// How many writes in a row, each beginning where the last ended, make a
/// guest's stream from one end of its disk -- past the first -- before the
/// image grows ahead of it (`Direct::ask_ahead`).
const STREAM_MIN: u64 = 2;

/// The chunks of a stream's first stretch grown ahead, and the most of one
/// after it, each twice the last's: a gigabyte of 16 MiB chunks, so that a
/// guest's 2 GiB first write is a handful of commits made while it writes,
/// each well ahead of it -- where a stretch of one chunk, 3.5 ms of a 1 MiB
/// QD8 stream's writing on the EX44, was waited for by the writes that came
/// to it, 8.8 ms each (round 24) -- and within the same share of the room
/// as a growth (`GROW_SHARE`).
const AHEAD_FIRST: u64 = 4;
const AHEAD_MAX: u64 = 64;

/// What zeros are written from, a piece at a time (`write_zeroes`).
const ZERO_CHUNK: usize = 16 * 1024;
static ZEROS: [u8; ZERO_CHUNK] = [0; ZERO_CHUNK];

/// Blocks of the file's that follow one another on the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    /// Its first block, in the file
    first: u64,
    len: u64,
    /// Where its first block begins on the device
    sector: u64,
    /// An unwritten extent's: the file's, reading as zeros until made
    /// written -- a chunk given ahead of the guest's writes (`grow`).
    unwritten: bool,
}

/// Where the file's blocks are on the device, as the image holds it: the
/// runs of blocks the file has, sorted and apart -- a hole is what no run
/// covers. Taken from the filesystem when the image is opened, a window at a
/// time (`Vfs::map_runs`), and changed by nothing but the image's own writes
/// into holes and its punches, the blocks each touched taken again from the
/// filesystem after it: the hold keeps everyone else off the file, and a
/// filesystem moves no file's blocks. So a batch is resolved here, with no
/// lock taken, where it was asked of the VFS under its mount's -- and a
/// guest's disk waits for no other guest's on the same filesystem, nor for
/// nos's own files there. A map that cannot be taken, or taken again, is
/// dropped, and the image asks the VFS for every batch as it did.
struct Map {
    runs: Vec<Run>,
    /// The device's sectors in a block
    per_block: u64,
}

impl Map {
    /// The map of the `blocks` blocks of the file `handle` holds, or None:
    /// an I/O error, or no memory for it.
    fn load(vfs: &Vfs, handle: Handle, blocks: u64, per_block: u64) -> Option<Map> {
        let mut map = Map { runs: Vec::new(), per_block };
        map.retake(vfs, handle, 0, blocks).then_some(map)
    }

    /// The first run that ends past block `b`.
    fn index(&self, b: u64) -> usize {
        self.runs.partition_point(|r| r.first + r.len <= b)
    }

    /// The first block of `from..to` that no run covers -- a hole -- or None
    /// where the file has every one of them.
    fn first_hole(&self, from: u64, to: u64) -> Option<u64> {
        let mut b = from;
        let mut i = self.index(b);
        while b < to {
            match self.runs.get(i) {
                Some(r) if r.first <= b => {
                    b = r.first + r.len;
                    i += 1;
                }
                _ => return Some(b),
            }
        }
        None
    }

    /// Where each of `out.len()` blocks from `first` begins on the device,
    /// 0 for a hole, and whether it is an unwritten extent's, into
    /// `unwritten` beside it.
    fn fill(&self, first: u64, out: &mut [u64], unwritten: &mut [bool]) {
        let mut i = self.index(first);
        for (k, (slot, flag)) in out.iter_mut().zip(unwritten.iter_mut()).enumerate() {
            let b = first + k as u64;
            while self.runs.get(i).is_some_and(|r| r.first + r.len <= b) {
                i += 1;
            }
            (*slot, *flag) = match self.runs.get(i) {
                Some(r) if r.first <= b => (r.sector + (b - r.first) * self.per_block, r.unwritten),
                _ => (0, false),
            };
        }
    }

    /// Block `b` the first of a run, where a run has it in its middle:
    /// false when there is no memory for the second half.
    fn split(&mut self, b: u64) -> bool {
        let i = self.index(b);
        let Some(r) = self.runs.get(i).copied() else { return true };
        if r.first >= b {
            return true;
        }
        if self.runs.try_reserve(1).is_err() {
            return false;
        }
        let left = b - r.first;
        self.runs[i].len = left;
        self.runs.insert(i + 1, Run { first: b, len: r.len - left, sector: r.sector + left * self.per_block, ..r });
        true
    }

    /// The runs at `i - 1` and `i` made one, where the second carries on
    /// where the first ends, in the file and on the device.
    fn join(&mut self, i: usize) {
        if i == 0 || i >= self.runs.len() {
            return;
        }
        let (a, b) = (self.runs[i - 1], self.runs[i]);
        if a.first + a.len == b.first && a.sector + a.len * self.per_block == b.sector && a.unwritten == b.unwritten {
            self.runs[i - 1].len += b.len;
            self.runs.remove(i);
        }
    }

    /// Blocks `first..first + count` taken again from the filesystem: what
    /// the map had of them dropped, and what the filesystem says of them now
    /// put in their place, a window at a time. false -- and the map no
    /// longer to be trusted -- on an I/O error, runs that do not add up to
    /// what was asked, or no memory.
    fn retake(&mut self, vfs: &Vfs, handle: Handle, first: u64, count: u64) -> bool {
        let Some(end) = first.checked_add(count) else { return false };
        if !self.split(first) || !self.split(end) {
            return false;
        }
        let (from, to) = (self.index(first), self.index(end));
        self.runs.drain(from..to);
        let mut at_run = from;
        let mut got = Vec::new();
        let mut at = first;
        while at < end {
            let n = (end - at).min(MAP_WINDOW);
            got.clear();
            if !vfs.map_runs(handle, at, n, &mut got) {
                return false;
            }
            let mut b = at;
            for &MapRun { sector, len, unwritten } in &got {
                if len == 0 || len > at + n - b {
                    klog!(0, "image: the filesystem's map of {} blocks from {} does not add up", n, at);
                    return false;
                }
                if sector != 0 {
                    let carries_on = at_run > 0 && {
                        let prev = self.runs[at_run - 1];
                        prev.first + prev.len == b && prev.sector + prev.len * self.per_block == sector
                            && prev.unwritten == unwritten
                    };
                    if carries_on {
                        self.runs[at_run - 1].len += len;
                    } else {
                        if self.runs.try_reserve(1).is_err() {
                            return false;
                        }
                        self.runs.insert(at_run, Run { first: b, len, sector, unwritten });
                        at_run += 1;
                    }
                }
                b += len;
            }
            if b != at + n {
                klog!(0, "image: the filesystem's map of {} blocks from {} does not add up", n, at);
                return false;
            }
            at += n;
        }
        self.join(at_run);
        true
    }
}

/// A request's part in a batch: which request, where in its bytes and how
/// many, the batch's first page it takes -- a page a block -- where in that
/// block it starts, how many blocks it takes, and the first of them in the
/// file.
#[derive(Clone, Copy)]
struct Stretch {
    req: usize,
    from: usize,
    len: usize,
    page: usize,
    skip: usize,
    blocks: usize,
    first: u64,
}

/// File blocks an image wrote over unwritten extents since its filesystem
/// gave them (`grow`), as runs `start..end`, sorted, apart and not touching:
/// what reads read from the device until they are made written -- rather
/// than as zeros, as an unwritten extent's are -- and what its next sync
/// makes written (`Vfs::mark_written`), after flushing the device. A punch
/// takes its blocks out: one of them given again as an unwritten extent,
/// and found here, would read whatever the device held there.
#[derive(Default)]
struct Pending {
    runs: Vec<(u64, u64)>,
}

impl Pending {
    fn contains(&self, b: u64) -> bool {
        let i = self.runs.partition_point(|&(_, end)| end <= b);
        self.runs.get(i).is_some_and(|&(start, _)| start <= b)
    }

    /// `start..end` in, run on into the runs it meets or touches: false,
    /// and nothing changed, when there is no memory for one more run.
    fn insert(&mut self, start: u64, end: u64) -> bool {
        if start >= end {
            return true;
        }
        let i = self.runs.partition_point(|&(_, e)| e < start);
        let mut j = i;
        let (mut s, mut e) = (start, end);
        while let Some(&(rs, re)) = self.runs.get(j) {
            if rs > e {
                break;
            }
            s = s.min(rs);
            e = e.max(re);
            j += 1;
        }
        if j > i {
            self.runs[i] = (s, e);
            self.runs.drain(i + 1..j);
            return true;
        }
        if self.runs.try_reserve(1).is_err() {
            return false;
        }
        self.runs.insert(i, (s, e));
        true
    }

    /// `start..end` out: false, and nothing changed, when a run it splits
    /// in two finds no memory for the second half.
    fn remove(&mut self, start: u64, end: u64) -> bool {
        if start >= end {
            return true;
        }
        let i = self.runs.partition_point(|&(_, e)| e <= start);
        let mut j = i;
        while self.runs.get(j).is_some_and(|&(rs, _)| rs < end) {
            j += 1;
        }
        if i == j {
            return true;
        }
        let (first, last) = (self.runs[i].0, self.runs[j - 1].1);
        let mut kept = [(0u64, 0u64); 2];
        let mut n = 0;
        if first < start {
            kept[n] = (first, start);
            n += 1;
        }
        if last > end {
            kept[n] = (end, last);
            n += 1;
        }
        if n > j - i && self.runs.try_reserve(n - (j - i)).is_err() {
            return false;
        }
        self.runs.splice(i..j, kept[..n].iter().copied());
        true
    }

    /// The runs as first blocks and counts: what `Vfs::mark_written` takes.
    fn as_counts(&self, out: &mut Vec<(u64, u64)>) -> bool {
        out.clear();
        if out.try_reserve_exact(self.runs.len()).is_err() {
            return false;
        }
        out.extend(self.runs.iter().map(|&(s, e)| (s, e - s)));
        true
    }

    fn blocks(&self) -> u64 {
        self.runs.iter().map(|&(s, e)| e - s).sum()
    }
}

/// The most runs an image keeps written and not yet made written: past
/// them, it makes them written there and then, as a sync would -- a guest
/// writing all over an unwritten disk for long without a flush costs the
/// host no more memory than this.
const PENDING_MAX: usize = 1024;

/// What an image reads and writes off the VFS's lock with: the device its
/// file's blocks are on, and a page of the buffer for each block of a batch.
struct Direct {
    disk: Disk,
    /// A block of the file's, in bytes: a page or a whole fraction of one.
    block: usize,
    batch: DmaBuffer,
    /// Where each block of the batch is on the device, 0 for a hole; and
    /// beside it whether it is an unwritten extent's, and whether that has
    /// not been written since (`fresh`): a block that reads as zeros, its
    /// device's bytes no one's to see.
    sectors: Vec<u64>,
    unwritten: Vec<bool>,
    fresh: Vec<bool>,
    pieces: Vec<Piece>,
    /// The requests' parts the batch is made of.
    stretches: Vec<Stretch>,
    /// The file's map as the image holds it; None once it could not be taken.
    map: Option<Map>,
    /// What a write into a hole grows the file by, and a discard gives back
    /// whole, in blocks: `CHUNK_BYTES`, unless told otherwise.
    chunk: u64,
    /// Where the last growth ended, in blocks, and how many chunks it was:
    /// a hole beginning there is given twice as many (`grow`).
    grown_to: u64,
    span: u64,
    /// The guest's stream of writes from one end (`ask_ahead`): the block
    /// the next write of it begins at, and how many in a row went on so;
    /// and where the stretch last given ahead of it ends, and its chunks.
    stream_at: u64,
    stream_run: u64,
    ahead_to: u64,
    ahead_span: u64,
    /// The growth under way ahead of its guest's writes, by a task of its
    /// own (`ask_ahead`): one at a time, None between them.
    ahead: Option<Grower>,
    /// No growth ahead from here on -- no task could be had for one, or its
    /// filesystem refused what one asked -- and each write into a hole grows
    /// the image itself, as before.
    ahead_off: bool,
    /// Growths made ahead, the blocks they gave, and the writes that met one
    /// still under way and waited for it.
    aheads: u64,
    ahead_blocks: u64,
    ahead_waits: Timed,
    /// A write went through the VFS since the last sync -- blocks were
    /// allocated, the filesystem's own metadata changed -- so the next sync
    /// is the filesystem's, not only the device's.
    through_vfs: bool,
    /// Batches read and written off the VFS's lock, and written through it:
    /// what the image says as it goes.
    read_off: u64,
    written_off: u64,
    written_through: u64,
    /// Blocks given back by discards, and given to the file ahead of writes.
    punched: u64,
    allocated: u64,
    /// Whether its filesystem gives blocks ahead of writes at all
    /// (`Vfs::allocates`): where it does not, a write into a hole goes
    /// through the VFS without asking it for a chunk first.
    grows: bool,
    /// What it wrote over unwritten extents and has not made written yet,
    /// and the blocks it has made so: the next sync's work, and what it did.
    pending: Pending,
    made_written: u64,
    /// Chunks its filesystem refused, though it gives blocks ahead of writes
    /// -- each such write going through the VFS, as every one into a hole did
    /// before chunks: said at the first, and counted.
    refused: u64,
    /// Its growths and its conversions of what it wrote over unwritten
    /// extents (`make_written`) -- each a commit of the filesystem's, which a
    /// guest's write or flush waits for -- and how long they took, in all
    /// and at most: what a guest's I/O that met one waited.
    grows_timed: Timed,
    conversions: Timed,
    /// The guest's flushes (`Image::sync`), each the whole call.
    syncs: Spread,
}

/// How many of something there were and how long they took, in all and at
/// most, in nanoseconds.
#[derive(Default, Clone, Copy)]
struct Timed {
    count: u64,
    ns: u64,
    max_ns: u64,
}

impl Timed {
    fn add(&mut self, from: u64) {
        let ns = boot_time_ns().saturating_sub(from);
        self.count += 1;
        self.ns += ns;
        self.max_ns = self.max_ns.max(ns);
    }

    fn average_us(&self) -> u64 {
        self.ns / self.count.max(1) / 1000
    }

    fn max_us(&self) -> u64 {
        self.max_ns / 1000
    }
}

/// How long something took, a log2 bucket of microseconds each -- 0 for
/// under 1 us, k for 2^(k-1) up to 2^k -- and at most: where a tail is, which
/// an average does not say (the image's let-go lines).
#[derive(Clone, Copy)]
struct Spread {
    buckets: [u64; SPREAD_BUCKETS],
    max_ns: u64,
}

/// Buckets up to 2^24 us -- 16 s -- the last of them everything past.
const SPREAD_BUCKETS: usize = 26;

impl Spread {
    const fn new() -> Spread {
        Spread { buckets: [0; SPREAD_BUCKETS], max_ns: 0 }
    }

    fn add(&mut self, from: u64) {
        let ns = boot_time_ns().saturating_sub(from);
        let us = ns / 1000;
        let k = (u64::BITS - us.leading_zeros()) as usize;
        self.buckets[k.min(SPREAD_BUCKETS - 1)] += 1;
        self.max_ns = self.max_ns.max(ns);
    }

    fn count(&self) -> u64 {
        self.buckets.iter().sum()
    }

    /// The top of the bucket the `per_mille`'th of them falls in, in us.
    fn bound(&self, per_mille: u64) -> u64 {
        let want = (self.count() * per_mille).div_ceil(1000).max(1);
        let mut seen = 0;
        for (k, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= want {
                return 1u64 << k;
            }
        }
        1 << (SPREAD_BUCKETS - 1)
    }
}

impl core::fmt::Display for Spread {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.count() == 0 {
            return f.write_str("none");
        }
        write!(f, "{}, half within {} us, 99% {}, 99.9% {}, the longest {}", self.count(), self.bound(500),
               self.bound(990), self.bound(999), self.max_ns / 1000)
    }
}

/// What an image serves a guest's disk's requests in (`start`, `reap`):
/// buffers of its own, one a request, and the I/O of those it has with the
/// device -- asynchronous, where its disk takes that and the request's
/// blocks are all mapped already: a read or a write of whole blocks the map
/// names, a read's holes and unwritten blocks zeros. Those go to the device
/// straight from the buffer, with no copy and no lock, as many at once as
/// the guest has out, and each comes back as it is done. Anything else -- a
/// write into a hole, a partial block, a flush, a discard, any of the
/// image's other calls -- waits for everything in flight first (`drain`),
/// and is served by the synchronous path as before: so nothing the image
/// changes of its map, its file or its pending conversions moves under I/O
/// the device has, and a punched block goes to nobody else while a write of
/// the guest's is on its way to it.
struct Flights {
    queue: Queue,
    slots: Vec<Flight>,
    /// Requests done that `reap` has not given back yet, in the order they
    /// finished: room for every buffer.
    done: VecDeque<(usize, bool)>,
    /// A request's commands as `start_fast` makes them: where in its
    /// buffer, how long, the sector. Room for one a block.
    commands: Vec<(usize, usize, u64)>,
    /// Requests served by the device's asynchronous path, and the others.
    fast: u64,
    slow: u64,
    /// How long they took: an asynchronous one from its start until its
    /// last command was done, the others the call; and how long the image
    /// waited for its I/O before anything else (`drain`).
    fast_took: Spread,
    slow_took: Spread,
    drained: Spread,
}

/// A buffer's request while the image has it.
#[derive(Default)]
struct Flight {
    /// Whether the image has it: from `start` until `reap` gives it back.
    busy: bool,
    write: bool,
    /// Its commands not yet done, and whether every one went well.
    left: usize,
    ok: bool,
    /// When it was started.
    began: u64,
    /// For a write, the runs of blocks it writes over unwritten extents --
    /// noted as written (`note_written`) once it is done, and not before:
    /// until then a read of one finds the zeros it held.
    unwritten: Vec<(u64, u64)>,
}

/// How long a drain waits for a completion before it looks again, and
/// before it says it is still waiting.
const DRAIN_STEP_MS: u64 = 100;
const DRAIN_SAY_NS: u64 = 10_000_000_000;

/// What `start` did with a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Started {
    /// With the device: `reap` gives it back.
    InFlight,
    /// Done already, well or not.
    Done(bool),
}

pub struct Image {
    vfs: &'static Vfs,
    handle: Handle,
    /// The file's path, as it was given -- any bytes the VFS takes in a
    /// name -- for what the image says as it goes.
    path: Vec<u8>,
    size: u64,
    writable: bool,
    /// The file's mount and which file it is: what a claim on it is keyed by.
    identity: (u64, u64),
    /// None: its filesystem keeps no blocks of its own on a device, and
    /// everything goes through the VFS.
    direct: Option<Direct>,
    /// Its queue, once it has one (`make_queue`).
    flights: Option<Flights>,
}

impl Image {
    /// The regular file at `path`, held as a disk's image, for reading -- and
    /// with `write` for writing too: None when there is none, it cannot be
    /// held so (someone else writes it, or holds it for writing; a read-only
    /// mount), or there is no memory for its buffer.
    pub fn open(vfs: &'static Vfs, path: &[u8], write: bool) -> Option<Image> {
        let flags = OPEN_READ | OPEN_HOLD | if write { OPEN_WRITE } else { 0 };
        let mut name = Vec::new();
        name.try_reserve_exact(path.len()).ok()?;
        name.extend_from_slice(path);
        let handle = vfs.open(path, flags)?;
        /* Closed by its drop from here on, whatever is refused below. */
        let mut image =
            Image { vfs, handle, path: name, size: 0, writable: write, identity: (0, 0), direct: None, flights: None };
        image.size = vfs.length(handle)? as u64;
        image.identity = vfs.identity(handle)?;
        if let Some((device, block)) = vfs.geometry(handle) {
            /* Blocks no page holds whole, or no whole number of the device's
             * sectors: through the VFS, as a filesystem with none. */
            if let Some(disk) = Direct::fits(device, block) {
                let mut direct = Direct::new(disk, block)?;
                direct.grows = write && vfs.allocates(handle);
                let per_block = block as u64 / direct.disk.sector_size();
                direct.map = Map::load(vfs, handle, image.size.div_ceil(block as u64), per_block);
                if direct.map.is_none() {
                    klog!(1, "image: {}: no map of its own -- every batch asks the VFS",
                           core::str::from_utf8(path).unwrap_or("(a path that is not UTF-8)"));
                }
                image.direct = Some(direct);
            }
        }
        Some(image)
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// The VFS's handle it holds the file by: what tells it apart from the
    /// file's other opens, to whoever looks at them all (the fuzzer).
    pub fn handle(&self) -> Handle {
        self.handle
    }

    /// The file's mount and which file on it this is (`Vfs::identity`).
    pub fn identity(&self) -> (u64, u64) {
        self.identity
    }

    /// Whether a discard gives blocks back -- its filesystem keeps blocks of
    /// its own on a device -- or changes nothing.
    pub fn gives_back(&self) -> bool {
        self.direct.is_some()
    }

    /// Where the image has data from `offset` on, as `(start, end)`: holes
    /// from `offset` to `start`, and from `start` to `end` blocks that are
    /// on the device -- `start` is `offset` unless a hole was there, `end`
    /// where the next hole begins, at most `EXTENT_BLOCKS` blocks on from
    /// `offset`'s and never past the image's end. `start == end` where none
    /// of the blocks looked at holds data: holes up to there, and the next
    /// call goes on from it. So a copy that leaves the holes out -- a VM's
    /// disk cloned -- reads only what is there, and walks a sparse disk of
    /// gigabytes a call at a time, free to stop between them. What a hole
    /// is, is the filesystem's map (`Vfs::map`): a block it reads as zeros
    /// without one. Where its filesystem keeps no blocks of its own, every
    /// byte is data. `(size, size)` from the end on; None on an I/O error.
    pub fn extent(&mut self, offset: u64) -> Option<(u64, u64)> {
        let size = self.size;
        if offset >= size {
            return Some((size, size));
        }
        let (vfs, handle) = (self.vfs, self.handle);
        let Some(direct) = self.direct.as_mut() else { return Some((offset, size)) };
        let block = direct.block as u64;
        let first = offset / block;
        let last = size.div_ceil(block).min(first.saturating_add(EXTENT_BLOCKS));
        /* Byte offsets of blocks inside the image: at most `size`, however
         * near the top of a u64 a damaged length puts it. */
        let at_block = |b: u64| b.checked_mul(block).map_or(size, |at| at.min(size));
        let mut data_from = None;
        let mut at = first;
        while at < last {
            let n = usize::try_from(last - at).map_or(direct.sectors.len(), |n| n.min(direct.sectors.len()));
            if !direct.resolve(vfs, handle, at, 0, n) {
                return None;
            }
            for (k, &sector) in direct.sectors[..n].iter().enumerate() {
                let b = at + k as u64;
                match (data_from, sector != 0 && !direct.fresh[k]) {
                    (None, true) => data_from = Some(b),
                    (Some(from), false) => return Some((at_block(from).max(offset), at_block(b))),
                    _ => {}
                }
            }
            at += n as u64;
        }
        let end = at_block(last);
        Some(match data_from {
            Some(from) => (at_block(from).max(offset), end),
            None => (end, end),
        })
    }

    /// Whether the map the image holds is what the filesystem says of every
    /// one of its blocks now -- taken afresh, as runs, and held to it -- or
    /// it holds none, and asks. For whoever holds the image to its
    /// filesystem (the fuzzer).
    /// Growths made ahead of its writes by its own task, and the writes into
    /// a hole that waited for one: for whoever holds the image to what it
    /// reached (the fuzzer).
    pub fn grown_ahead(&self) -> (u64, u64) {
        self.direct.as_ref().map_or((0, 0), |d| (d.aheads, d.ahead_waits.count))
    }

    pub fn map_is_the_filesystems(&mut self) -> bool {
        let (vfs, handle, size) = (self.vfs, self.handle, self.size);
        /* A growth under way ahead is the file's before it is the map's:
         * waited for and taken, so that the two are held to each other as
         * they settle. */
        if let Some(direct) = self.direct.as_mut() {
            direct.settle_ahead(vfs, handle);
        }
        let Some(Direct { map: Some(map), block, .. }) = self.direct.as_ref() else { return true };
        match Map::load(vfs, handle, size.div_ceil(*block as u64), map.per_block) {
            Some(now) if now.runs == map.runs => true,
            Some(_) => {
                klog!(0, "image: its map is not its filesystem's");
                false
            }
            None => false,
        }
    }

    /// Whether `len` bytes from `offset` are all inside the image.
    fn inside(&self, offset: u64, len: usize) -> bool {
        self.inside_len(offset, len as u64)
    }

    fn inside_len(&self, offset: u64, len: u64) -> bool {
        offset.checked_add(len).is_some_and(|end| end <= self.size)
    }

    /// Fills `buf` from `offset`, all of it inside the image: false on an
    /// I/O error, or a range that is not.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> bool {
        self.read_vec(&mut [(offset, buf)])
    }

    /// Writes `data` at `offset`, all of it inside the image -- which never
    /// grows: false on an I/O error, a range that is not, or an image held
    /// for reading. Not synced: `sync` is.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> bool {
        self.write_vec(&[(offset, data)])
    }

    /// Fills each buffer of `ios` from its offset, all of them inside the
    /// image: the blocks of as many as the batch holds read in one go, so
    /// that the requests a guest's disk has out at once are with the device
    /// at once -- a request's blocks that are holes zeros. False on an I/O
    /// error, or a range that is not inside, by when some may be filled.
    pub fn read_vec(&mut self, ios: &mut [(u64, &mut [u8])]) -> bool {
        if ios.iter().any(|(at, buf)| !self.inside(*at, buf.len())) {
            return false;
        }
        self.drain();
        let (vfs, handle) = (self.vfs, self.handle);
        let Some(direct) = self.direct.as_mut() else {
            return ios.iter_mut().all(|(at, buf)| {
                usize::try_from(*at).ok().and_then(|at| vfs.read_at(handle, at, buf)) == Some(buf.len())
            });
        };

        let block = direct.block;
        let pages = direct.sectors.len();
        let (mut req, mut done) = (0usize, 0usize);
        while req < ios.len() {
            /* A batch: the next parts of as many requests as its pages hold */
            direct.stretches.clear();
            let mut used = 0;
            while req < ios.len() && used < pages {
                let (offset, len) = (ios[req].0, ios[req].1.len());
                if done == len {
                    req += 1;
                    done = 0;
                    continue;
                }
                let at = offset + done as u64;
                let skip = (at % block as u64) as usize;
                let blocks = (skip + len - done).div_ceil(block).min(pages - used);
                let take = (blocks * block - skip).min(len - done);
                let first = at / block as u64;
                if !direct.resolve(vfs, handle, first, used, blocks) {
                    return false;
                }
                direct.stretches.push(Stretch { req, from: done, len: take, page: used, skip, blocks, first });
                used += blocks;
                done += take;
            }

            /* The blocks that are there, a page of the buffer each, read in
             * one go -- with no lock held. An unwritten extent's that nothing
             * has written reads as zeros: its bytes on the device are what it
             * held before it was the file's, no one's to see. */
            direct.pieces.clear();
            for (k, &sector) in direct.sectors[..used].iter().enumerate() {
                if sector != 0 && !direct.fresh[k] {
                    direct.pieces.push(Piece { sector, at: k * PAGE_SIZE, len: block });
                }
            }
            if direct.disk.read_pieces(&mut direct.batch, &direct.pieces).is_err() {
                klog!(0, "image: a read of {} blocks failed", direct.pieces.len());
                return false;
            }
            direct.read_off += 1;

            for st in &direct.stretches {
                let buf = &mut ios[st.req].1[st.from..st.from + st.len];
                let (mut off, mut skip, mut k) = (0, st.skip, st.page);
                while off < st.len {
                    let chunk = (block - skip).min(st.len - off);
                    let to = &mut buf[off..off + chunk];
                    if direct.sectors[k] != 0 && !direct.fresh[k] {
                        let from = k * PAGE_SIZE + skip;
                        to.copy_from_slice(&direct.batch.as_slice()[from..from + chunk]);
                    } else {
                        /* A hole, or an unwritten extent's block nothing has
                         * written, reads as zeros */
                        to.fill(0);
                    }
                    off += chunk;
                    skip = 0;
                    k += 1;
                }
            }
        }
        true
    }

    /// Writes each of `ios` at its offset, all of them inside the image --
    /// which never grows: as many as the batch holds in one go, as
    /// `read_vec` reads them, a hole's chunk given its blocks first (`grow`).
    /// A block two of them write -- two sectors of one block, which the
    /// first and the last block of each are read for and written back whole
    /// -- is never in one batch twice: the second waits for the next batch,
    /// so both land. A part of one into a hole that cannot be given blocks
    /// goes through the VFS, after the batch before it. False on an I/O
    /// error, a range that is not inside, or an image held for reading, by
    /// when some may be written. Not synced: `sync` is.
    pub fn write_vec(&mut self, ios: &[(u64, &[u8])]) -> bool {
        if !self.writable || ios.iter().any(|(at, data)| !self.inside(*at, data.len())) {
            return false;
        }
        self.drain();
        let (vfs, handle, size) = (self.vfs, self.handle, self.size);
        let Some(direct) = self.direct.as_mut() else {
            return ios.iter().all(|(at, data)| usize::try_from(*at).is_ok_and(|at| vfs.write_within(handle, at, data)));
        };

        let block = direct.block;
        let pages = direct.sectors.len();
        let file_blocks = size.div_ceil(block as u64);
        direct.take_ahead(vfs, handle);
        let (mut req, mut done) = (0usize, 0usize);
        while req < ios.len() {
            direct.stretches.clear();
            let mut used = 0;
            while req < ios.len() && used < pages {
                let (offset, data) = ios[req];
                if done == data.len() {
                    req += 1;
                    done = 0;
                    continue;
                }
                let at = offset + done as u64;
                let skip = (at % block as u64) as usize;
                let blocks = (skip + data.len() - done).div_ceil(block).min(pages - used);
                /* What of the request this part takes: to the end of its blocks */
                let take = (blocks * block - skip).min(data.len() - done);
                let first = at / block as u64;
                if !direct.resolve(vfs, handle, first, used, blocks) {
                    return false;
                }
                if direct.grows && direct.sectors[used..used + blocks].contains(&0) {
                    /* Being given already, by the image's task: waited for,
                     * and not given a second time over it */
                    if direct.wait_ahead(vfs, handle, first, blocks as u64)
                        && !direct.resolve(vfs, handle, first, used, blocks)
                    {
                        return false;
                    }
                    if direct.sectors[used..used + blocks].contains(&0)
                        && direct.grow(vfs, handle, first, blocks as u64, file_blocks)
                        && !direct.resolve(vfs, handle, first, used, blocks)
                    {
                        return false;
                    }
                }
                let (batch, part) = direct.sectors.split_at(used);
                let part = &part[..blocks];
                if used > 0 && (part.contains(&0) || part.iter().any(|s| batch.contains(s))) {
                    /* The batch so far first: this part writes a block it
                     * has, or goes through the VFS after it */
                    break;
                }
                if part.contains(&0) {
                    /* A hole still -- a filesystem that gives no blocks ahead
                     * of a write, or none left to give: blocks only it can
                     * allocate and point at, in order, so the part through
                     * it, as every write went before */
                    direct.through_vfs = true;
                    let Ok(to) = usize::try_from(at) else { return false };
                    /* What it wrote over unwritten extents made written first:
                     * the VFS's write reads an unwritten block as zeros, and
                     * would put them back around its own bytes in one */
                    if !direct.make_written(vfs, handle, None) {
                        return false;
                    }
                    let written = vfs.write_within(handle, to, &data[done..done + take]);
                    /* Blocks given, as far as the write got: the map's from now */
                    direct.retake(vfs, handle, first, blocks as u64);
                    if !written {
                        return false;
                    }
                    direct.written_through += 1;
                    done += take;
                    continue;
                }
                direct.stretches.push(Stretch { req, from: done, len: take, page: used, skip, blocks, first });
                used += blocks;
                done += take;
            }
            if used == 0 {
                continue;
            }

            /* The first block and the last of each part, where it takes only
             * part of them: what the rest of each holds, read first -- zeros,
             * for an unwritten extent's block nothing has written */
            direct.pieces.clear();
            for st in &direct.stretches {
                let end = st.skip + st.len;
                let mut ends = [None; 2];
                if st.skip != 0 {
                    ends[0] = Some(st.page);
                }
                if end % block != 0 && (st.blocks > 1 || st.skip == 0) {
                    ends[1] = Some(st.page + st.blocks - 1);
                }
                for k in ends.into_iter().flatten() {
                    if direct.fresh[k] {
                        direct.batch.as_mut_slice()[k * PAGE_SIZE..k * PAGE_SIZE + block].fill(0);
                    } else {
                        direct.pieces.push(Piece { sector: direct.sectors[k], at: k * PAGE_SIZE, len: block });
                    }
                }
            }
            if direct.disk.read_pieces(&mut direct.batch, &direct.pieces).is_err() {
                klog!(0, "image: a read of the ends of a write failed");
                return false;
            }

            for st in &direct.stretches {
                let data = &ios[st.req].1[st.from..st.from + st.len];
                let (mut off, mut skip) = (0, st.skip);
                for k in st.page..st.page + st.blocks {
                    let chunk = (block - skip).min(st.len - off);
                    let to = k * PAGE_SIZE + skip;
                    direct.batch.as_mut_slice()[to..to + chunk].copy_from_slice(&data[off..off + chunk]);
                    off += chunk;
                    skip = 0;
                }
            }

            direct.pieces.clear();
            for (k, &sector) in direct.sectors[..used].iter().enumerate() {
                direct.pieces.push(Piece { sector, at: k * PAGE_SIZE, len: block });
            }
            if direct.disk.write_pieces(&direct.batch, &direct.pieces).is_err() {
                klog!(0, "image: a write of {} blocks failed", used);
                return false;
            }
            direct.written_off += 1;

            /* What went over an unwritten extent's blocks, kept to be made
             * written: read from the device from now, not as zeros */
            for k in 0..direct.stretches.len() {
                let st = direct.stretches[k];
                let mut b = 0;
                while b < st.blocks {
                    if !direct.unwritten[st.page + b] {
                        b += 1;
                        continue;
                    }
                    let from = b;
                    while b < st.blocks && direct.unwritten[st.page + b] {
                        b += 1;
                    }
                    if !direct.note_written(vfs, handle, st.first + from as u64, st.first + b as u64) {
                        return false;
                    }
                }
            }
        }
        /* Its last write going on from the last growth: the next asked
         * ahead of it, as a queue's write has it (`ask_ahead`) */
        if let Some(&(at, data)) = ios.last() {
            let first = at / block as u64;
            let n = (at % block as u64 + data.len() as u64).div_ceil(block as u64);
            direct.ask_ahead(vfs, handle, first, n, file_blocks);
        }
        true
    }

    /// How many chunks the image's filesystem refused ahead of a write into
    /// a hole, though it gives blocks so: what its fuzzer holds to its
    /// having had a reason -- no room, a request that failed.
    pub fn chunks_refused(&self) -> u64 {
        self.direct.as_ref().map_or(0, |d| d.refused)
    }

    /// What a discard gives back whole, in bytes -- a chunk -- or 0 where
    /// it gives back nothing (`gives_back`).
    pub fn discard_unit(&self) -> u64 {
        self.direct.as_ref().map_or(0, |d| d.chunk * d.block as u64)
    }

    /// The image's chunk -- what a write into a hole grows it by, and what a
    /// discard gives back whole -- made `blocks` blocks, at least one: for
    /// whoever wants another than `CHUNK_BYTES` (the fuzzer).
    pub fn set_chunk(&mut self, blocks: u64) {
        if let Some(direct) = self.direct.as_mut() {
            direct.chunk = blocks.max(1);
        }
    }

    /// The whole chunks inside `len` bytes from `offset` given back to the
    /// filesystem -- holes from here on, reading as zeros, the image's size
    /// as it was: a guest's discard. The rest of the range is left as it is,
    /// as a disk may leave what it is told it may forget: its blocks stay
    /// the file's, holding the guest's own data, and the next write there
    /// needs nothing allocated -- so a guest that discards as it frees, as
    /// ext4 mounted with `discard` does, costs the filesystem a commit for
    /// every chunk it gives back whole rather than for every range. False
    /// for a range not inside the image, an image held for reading, or an
    /// I/O error. On a filesystem with no blocks of its own there is
    /// nothing to give back, and that is done. What the filesystem changed
    /// of its own metadata is its own to put on the medium -- a journal's
    /// commit may come later -- so the next sync is the filesystem's, as
    /// after a write through it.
    pub fn discard(&mut self, offset: u64, len: u64) -> bool {
        if !self.writable || !self.inside_len(offset, len) {
            return false;
        }
        self.drain();
        let unit = self.discard_unit();
        if unit == 0 {
            return true;
        }
        let (from, to) = (offset.div_ceil(unit).saturating_mul(unit), (offset + len) / unit * unit);
        from >= to || self.punch(from, to - from)
    }

    /// The whole blocks inside `len` bytes from `offset` given back to the
    /// filesystem (`Vfs::punch`), and holes in the map from then on.
    fn punch(&mut self, offset: u64, len: u64) -> bool {
        /* Blocks given back may be another file's the moment the punch is
         * done: never with a write of the guest's on its way to them. */
        assert!(self.flights.as_ref().is_none_or(|f| f.queue.in_flight() == 0),
                "image: a punch with I/O of its queue's in flight");
        let (vfs, handle) = (self.vfs, self.handle);
        let Some(direct) = self.direct.as_mut() else { return true };
        let given = vfs.punch(handle, offset, len);
        /* The whole blocks inside the range, as far as the punch got: holes
         * in the map from now */
        let block = direct.block as u64;
        let (first, end) = (offset.div_ceil(block), (offset + len) / block);
        if first < end {
            direct.retake(vfs, handle, first, end - first);
            /* Out of what is kept to be made written: given again, as an
             * unwritten extent, a block found there would read whatever the
             * device holds. With no memory to take them out, all of it made
             * written now -- the punched blocks are holes, and left -- and
             * nothing kept. */
            if !direct.pending.remove(first, end) {
                let _ = direct.make_written(vfs, handle, None);
                direct.pending.runs.clear();
            }
        }
        match given {
            Some(given) => {
                direct.punched += given;
                if given != 0 {
                    direct.through_vfs = true;
                }
                true
            }
            None => false,
        }
    }

    /// `len` bytes from `offset` made zeros, all of them inside the image:
    /// with `unmap` the whole chunks among them given back -- holes read as
    /// zeros -- and the rest written over with zeros. False for a range not
    /// inside, an image held for reading, or an I/O error.
    pub fn write_zeroes(&mut self, offset: u64, len: u64, unmap: bool) -> bool {
        if !self.writable || !self.inside_len(offset, len) {
            return false;
        }
        self.drain();
        let end = offset + len;
        let unit = self.discard_unit();
        if unmap && unit != 0 {
            /* The whole chunks inside: none, where the first one past
             * `offset` would lie past a u64 -- never a discard from 0. */
            if let Some(from) = offset.div_ceil(unit).checked_mul(unit) {
                let to = end / unit * unit;
                if from < to {
                    return self.punch(from, to - from) && self.zero(offset, from) && self.zero(to, end);
                }
            }
        }
        self.zero(offset, end)
    }

    /// Zeros written over `[from, to)`, a piece at a time: each piece ending
    /// at a multiple of `ZERO_CHUNK` in the image -- which every block size
    /// divides -- so that no block is written by two of them, and a power
    /// cut between two leaves every sector what it held or zeros. Pieces
    /// counted from `from` itself would split a sector between two writes
    /// wherever `from` is not a multiple of one.
    fn zero(&mut self, from: u64, to: u64) -> bool {
        let chunk = ZERO_CHUNK as u64;
        let mut at = from;
        while at < to {
            let piece_end = (at / chunk + 1).checked_mul(chunk).map_or(to, |e| e.min(to));
            let n = (piece_end - at) as usize;
            if !self.write_at(at, &ZEROS[..n]) {
                return false;
            }
            at += n as u64;
        }
        true
    }

    /// Everything written to the image so far on its disk's medium: what it
    /// wrote over unwritten extents made written (`make_written`), whose
    /// commit pushes the device's cache out before it says so, or the cache
    /// pushed out where there were none -- or, after a write the VFS took,
    /// the filesystem synced, which pushes it out after its own metadata.
    pub fn sync(&mut self) -> bool {
        let began = boot_time_ns();
        let ok = self.sync_now();
        if let Some(direct) = self.direct.as_mut() {
            direct.syncs.add(began);
        }
        ok
    }

    fn sync_now(&mut self) -> bool {
        /* What the guest was told is done is on the device: everything in
         * flight done first, so that nothing of it is left out. */
        self.drain();
        let (vfs, handle) = (self.vfs, self.handle);
        let flushed = match self.direct.as_mut() {
            Some(direct) if !direct.pending.runs.is_empty() => {
                if !direct.make_written(vfs, handle, None) {
                    return false;
                }
                true
            }
            _ => false,
        };
        match self.direct.as_mut() {
            Some(direct) if !direct.through_vfs => flushed || direct.disk.flush().is_ok(),
            Some(direct) => {
                if !self.vfs.sync_file(self.handle) {
                    return false;
                }
                direct.through_vfs = false;
                true
            }
            None => self.vfs.sync_file(self.handle),
        }
    }

    /* ---- the queue: a guest's disk's requests, as many at once as it has ---- */

    /// The image's queue (`Flights`): `buffers` buffers of `pages` pages
    /// each, a request's at a time -- false when there is no memory for
    /// them, or it has one already.
    pub fn make_queue(&mut self, buffers: usize, pages: usize) -> bool {
        if self.flights.is_some() || buffers == 0 || pages == 0 {
            return false;
        }
        let disk = self.direct.as_ref().map(|d| d.disk);
        let Some(queue) = Queue::new(disk, buffers, pages) else { return false };
        let mut slots = Vec::new();
        let mut done = VecDeque::new();
        let mut commands = Vec::new();
        /* The most runs a request's blocks make, every other one unwritten. */
        let runs = pages / 2 + 1;
        if slots.try_reserve_exact(buffers).is_err() || done.try_reserve_exact(buffers).is_err()
            || commands.try_reserve_exact(pages).is_err()
        {
            return false;
        }
        for _ in 0..buffers {
            let mut unwritten = Vec::new();
            if unwritten.try_reserve_exact(runs).is_err() {
                return false;
            }
            slots.push(Flight { unwritten, ..Flight::default() });
        }
        self.flights = Some(Flights { queue, slots, done, commands, fast: 0, slow: 0, fast_took: Spread::new(),
                                      slow_took: Spread::new(), drained: Spread::new() });
        true
    }

    /// The number of its queue's entry, which `block::queue::wake` and
    /// `block::queue::wait` take: None while it has none.
    pub fn queue_entry(&self) -> Option<usize> {
        self.flights.as_ref().map(|f| f.queue.entry())
    }

    /// Where buffer `k` of its queue is, for whoever fills and empties it
    /// between its requests (`kernel_image_queue`).
    pub fn buffer_address(&mut self, k: usize) -> Option<*mut u8> {
        self.flights.as_mut()?.queue.address(k)
    }

    /// Buffer `k`'s bytes, while the image has no request of it: what a
    /// request's data is put in before `start`, and taken from after.
    pub fn buffer(&mut self, k: usize) -> Option<&mut [u8]> {
        let fl = self.flights.as_mut()?;
        if fl.slots.get(k)?.busy {
            return None;
        }
        fl.queue.bytes(k)
    }

    /// Buffer `k`'s request -- `len` bytes from `offset` read into it, or
    /// with `write` written from it, all of them inside the image -- taken:
    /// with the device and given back by `reap` (`InFlight`), or done now.
    /// A buffer the image has a request of already, one past the queue, a
    /// request longer than a buffer or not inside the image, and a write to
    /// one held for reading are each done, and failed. What the device is
    /// handed waits for `kick`.
    pub fn start(&mut self, k: usize, write: bool, offset: u64, len: usize) -> Started {
        let Some(fl) = self.flights.as_ref() else { return Started::Done(false) };
        if fl.slots.get(k).is_none_or(|f| f.busy) || len == 0 || len > fl.queue.buffer_len()
            || !self.inside(offset, len) || (write && !self.writable)
        {
            return Started::Done(false);
        }
        self.take_ahead();
        if let Some(started) = self.start_fast(k, write, offset, len) {
            if write {
                self.ask_ahead(offset, len);
            }
            return started;
        }
        /* A write into a hole: its chunks given first, and then to the
         * device as any other write -- with what is in flight left to go on
         * meanwhile, none of it a write into a hole (each went this way or
         * the synchronous one), where the synchronous path below waits for
         * all of it, then for the growth's commit, then writes this one: on
         * the EX44 a guest's sequential first write stood 12 ms at every
         * growth so, its device idle. */
        if write && self.grow_for(offset, len) {
            if let Some(started) = self.start_fast(k, write, offset, len) {
                self.ask_ahead(offset, len);
                return started;
            }
        }
        /* The synchronous path, after everything in flight: the batch's,
         * through the image's own buffer -- with the queue out of the way
         * meanwhile, whose buffer this request's data is in. */
        let began = boot_time_ns();
        self.drain();
        let Some(mut fl) = self.flights.take() else { return Started::Done(false) };
        let ok = match fl.queue.bytes(k) {
            Some(bytes) if write => self.write_vec(&[(offset, &bytes[..len])]),
            Some(bytes) => self.read_vec(&mut [(offset, &mut bytes[..len])]),
            None => false,
        };
        fl.slow += 1;
        fl.slow_took.add(began);
        self.flights = Some(fl);
        if trace::on::<ImageSync>() {
            trace::emit(&ImageSync {
                offset,
                ns: ns_field(boot_time_ns().saturating_sub(began)),
                len: u32::try_from(len).unwrap_or(u32::MAX),
                pid: trace::pid(),
                write: u8::from(write),
                ok: u8::from(ok),
                _pad: [0; 2],
            });
        }
        Started::Done(ok)
    }

    /// The holes among the blocks of a write of `len` bytes at `offset`
    /// given their chunks (`Direct::grow`), the queue's requests in flight
    /// as they are: whether there were holes and the filesystem gave them --
    /// a request whose blocks the image's map has not, or not whole, none.
    fn grow_for(&mut self, offset: u64, len: usize) -> bool {
        let (vfs, handle, size) = (self.vfs, self.handle, self.size);
        let Some(direct) = self.direct.as_mut() else { return false };
        let block = direct.block;
        if !direct.grows || direct.map.is_none() || offset % block as u64 != 0 || len % block != 0 {
            return false;
        }
        let (first, n) = (offset / block as u64, len / block);
        if n == 0 || n > direct.sectors.len() || !direct.resolve(vfs, handle, first, 0, n)
            || !direct.sectors[..n].contains(&0)
        {
            return false;
        }
        /* Being given already, by the image's task: waited for, and not
         * given a second time over it. */
        if direct.wait_ahead(vfs, handle, first, n as u64)
            && direct.resolve(vfs, handle, first, 0, n) && !direct.sectors[..n].contains(&0)
        {
            return true;
        }
        direct.grow(vfs, handle, first, n as u64, size.div_ceil(block as u64))
    }

    /// What the image's task grew ahead since the last look, into the map
    /// (`Direct::take_ahead`).
    fn take_ahead(&mut self) {
        let (vfs, handle) = (self.vfs, self.handle);
        if let Some(direct) = self.direct.as_mut() {
            direct.take_ahead(vfs, handle);
        }
    }

    /// A write of `len` bytes at `offset` started: the next growth asked
    /// ahead of it where it goes on from the last (`Direct::ask_ahead`).
    fn ask_ahead(&mut self, offset: u64, len: usize) {
        let (vfs, handle, size) = (self.vfs, self.handle, self.size);
        let Some(direct) = self.direct.as_mut() else { return };
        let block = direct.block as u64;
        direct.ask_ahead(vfs, handle, offset / block, (len as u64).div_ceil(block), size.div_ceil(block));
    }

    /// `start`'s asynchronous path (`Flights`): the request as commands to
    /// the device, runs of blocks one after another there -- or None, and
    /// nothing done, for one it does not take.
    fn start_fast(&mut self, k: usize, write: bool, offset: u64, len: usize) -> Option<Started> {
        let (vfs, handle) = (self.vfs, self.handle);
        let direct = self.direct.as_mut()?;
        let fl = self.flights.as_mut()?;
        let max = fl.queue.asynchronous()?;
        let block = direct.block;
        if direct.map.is_none() || offset % block as u64 != 0 || len % block != 0 {
            return None;
        }
        let (first, n) = (offset / block as u64, len / block);
        if n > direct.sectors.len() || !direct.resolve(vfs, handle, first, 0, n) {
            return None;
        }
        if write && direct.sectors[..n].contains(&0) {
            /* A hole: blocks only its filesystem can give it, in order. */
            return None;
        }

        /* Runs of blocks one after another on the device, up to what one
         * command moves; a read's blocks that are holes, or unwritten and
         * not written since, zeros in the buffer and no command at all. */
        let per_block = block as u64 / direct.disk.sector_size();
        fl.commands.clear();
        {
            let bytes = fl.queue.bytes(k)?;
            let mut b = 0;
            while b < n {
                let sector = direct.sectors[b];
                if !write && (sector == 0 || direct.fresh[b]) {
                    bytes[b * block..(b + 1) * block].fill(0);
                    b += 1;
                    continue;
                }
                let from = b;
                b += 1;
                while b < n && (b - from + 1) * block <= max
                    && direct.sectors[b] == sector + (b - from) as u64 * per_block
                    && (write || !direct.fresh[b])
                {
                    b += 1;
                }
                fl.commands.push((from * block, (b - from) * block, sector));
            }
        }
        let slot = &mut fl.slots[k];
        slot.unwritten.clear();
        if write {
            let mut b = 0;
            while b < n {
                if !direct.unwritten[b] {
                    b += 1;
                    continue;
                }
                let from = b;
                while b < n && direct.unwritten[b] {
                    b += 1;
                }
                /* Into the room taken at `make_queue`: a request's runs. */
                slot.unwritten.push((first + from as u64, first + b as u64));
            }
        }
        if fl.commands.is_empty() {
            /* Zeros, every block: nothing for the device to do. */
            fl.fast += 1;
            return Some(Started::Done(true));
        }

        let count = fl.commands.len();
        let began = boot_time_ns();
        let slot = &mut fl.slots[k];
        (slot.busy, slot.write, slot.left, slot.ok, slot.began) = (true, write, count, true, began);
        for i in 0..count {
            let (at, len, sector) = fl.commands[i];
            loop {
                match fl.queue.start(k, at, len, sector, write) {
                    Ok(()) => break,
                    Err(SubmitError::Busy) if i == 0 && fl.queue.in_flight() == 0 => {
                        /* Full of someone else's commands, none of the
                         * image's own to wait for: this one by the
                         * synchronous path, which waits for a command ID. */
                        fl.slots[k].busy = false;
                        return None;
                    }
                    Err(SubmitError::Busy) if boot_time_ns().saturating_sub(began) < DRAIN_SAY_NS => {
                        /* The device's queue is full: one of the image's own
                         * commands done makes room, or -- none of them out --
                         * someone else's, looked for again shortly. */
                        fl.queue.kick();
                        let step = if fl.queue.in_flight() > 0 { DRAIN_STEP_MS } else { 1 };
                        fl.queue.wait(Duration::from_millis(step));
                        while let Some((buf, ok)) = fl.queue.reap() {
                            command_done(direct, fl, vfs, handle, buf, ok);
                        }
                    }
                    Err(_) => {
                        /* Never, for a request checked as this one was --
                         * but said, and failed: what was handed over is
                         * still the device's, and the request is done when
                         * that is. */
                        klog!(0, "image: a command of {} bytes at sector {} was refused, or found no room on its \
                                   disk for {} s", len, sector, DRAIN_SAY_NS / 1_000_000_000);
                        let slot = &mut fl.slots[k];
                        slot.ok = false;
                        slot.left -= count - i;
                        if slot.left == 0 {
                            slot.busy = false;
                            return Some(Started::Done(false));
                        }
                        return Some(Started::InFlight);
                    }
                }
            }
        }
        fl.fast += 1;
        Some(Started::InFlight)
    }

    /// The doorbell for what `start` handed the device.
    pub fn kick(&mut self) {
        if let Some(fl) = self.flights.as_ref() {
            fl.queue.kick();
        }
    }

    /// A request done, if one is -- its buffer, and whether it went well --
    /// in the order they finished: never one `start` said was done.
    pub fn reap(&mut self) -> Option<(usize, bool)> {
        let (vfs, handle) = (self.vfs, self.handle);
        let fl = self.flights.as_mut()?;
        if fl.done.is_empty() {
            while let Some((buf, ok)) = fl.queue.reap() {
                match self.direct.as_mut() {
                    Some(direct) => command_done(direct, fl, vfs, handle, buf, ok),
                    None => klog!(0, "image: a completion for buffer {}, with no device to have come from", buf),
                }
            }
        }
        fl.done.pop_front()
    }

    /// Sleeps until one of its commands is done, or `block::queue::wake`
    /// wakes its queue, or `timeout` is up: whether one came. At once, with
    /// nothing to wait on, for an image with no queue.
    pub fn wait(&self, timeout: Duration) -> bool {
        self.flights.as_ref().is_some_and(|fl| fl.queue.wait(timeout))
    }

    /// Everything the queue has with the device done: its requests kept for
    /// `reap`. Said, while it waits long -- a disk that has stopped
    /// answering, whose guest's disk stops with it.
    fn drain(&mut self) {
        let (vfs, handle) = (self.vfs, self.handle);
        let Some(fl) = self.flights.as_mut() else { return };
        if fl.queue.in_flight() == 0 {
            return;
        }
        fl.queue.kick();
        let (began, mut said) = (boot_time_ns(), false);
        loop {
            while let Some((buf, ok)) = fl.queue.reap() {
                if let Some(direct) = self.direct.as_mut() {
                    command_done(direct, fl, vfs, handle, buf, ok);
                }
            }
            if fl.queue.in_flight() == 0 {
                fl.drained.add(began);
                return;
            }
            fl.queue.wait(Duration::from_millis(DRAIN_STEP_MS));
            let waited = boot_time_ns().saturating_sub(began);
            if waited >= DRAIN_SAY_NS && !said {
                said = true;
                klog!(0, "image: {}: has waited {} s for {} commands of its disk's", show(&self.path),
                       waited / 1_000_000_000, fl.queue.in_flight());
            }
        }
    }
}

/// One of buffer `buf`'s commands done: its request too, with the last --
/// what it wrote over unwritten extents noted as written now, and not
/// before (`Flight::unwritten`) -- and kept for `reap`.
fn command_done(direct: &mut Direct, fl: &mut Flights, vfs: &Vfs, handle: Handle, buf: usize, ok: bool) {
    let Some(slot) = fl.slots.get_mut(buf).filter(|s| s.busy && s.left > 0) else {
        klog!(0, "image: a completion for buffer {}, which has no command out", buf);
        return;
    };
    slot.ok &= ok;
    slot.left -= 1;
    if slot.left != 0 {
        return;
    }
    let mut ok = slot.ok;
    if slot.write && ok {
        for &(from, to) in &slot.unwritten {
            ok &= direct.note_written(vfs, handle, from, to);
        }
    }
    slot.busy = false;
    fl.fast_took.add(slot.began);
    /* Into the room taken at `make_queue`: a request a buffer. */
    fl.done.push_back((buf, ok));
}

/// Nanoseconds as a trace record's field holds them: `u32::MAX` for that or
/// more.
fn ns_field(ns: u64) -> u32 {
    u32::try_from(ns).unwrap_or(u32::MAX)
}

/// A path the image was given, as what it says shows it.
fn show(path: &[u8]) -> &str {
    core::str::from_utf8(path).unwrap_or("(a path that is not UTF-8)")
}

impl Drop for Image {
    fn drop(&mut self) {
        /* The task growing it ahead stopped first: it gives blocks by the
         * handle let go of below. */
        if let Some(direct) = self.direct.as_mut() {
            direct.stop_ahead();
        }
        /* Its buffers are the device's until then, and what it wrote over
         * unwritten extents is noted when it is done. */
        self.drain();
        let path = core::str::from_utf8(&self.path).unwrap_or("(a path that is not UTF-8)");
        let (vfs, handle) = (self.vfs, self.handle);
        if let Some(direct) = self.direct.as_mut() {
            /* What it wrote over unwritten extents made written as it goes,
             * as a sync would: let go of with no sync, a guest's last writes
             * are the file's all the same, as a disk's are. */
            if !direct.make_written(vfs, handle, None) {
                klog!(0, "image: {}: {} blocks it wrote over unwritten extents were not made written: they read as \
                           zeros", path, direct.pending.blocks());
            }
        }
        match &self.direct {
            /* Two lines, each short of what a line of the log holds whole. */
            Some(d) => {
                klog!(0, "image: {} let go of -- {} batches read and {} written off the VFS's lock, {} written \
                           through it; {}", path, d.read_off, d.written_off, d.written_through, MapSays(d.map.as_ref()));
                klog!(0, "image: {}: {} blocks given ahead of writes, {} chunks refused, {} blocks given back, {} \
                           made written", path, d.allocated, d.refused, d.punched, d.made_written);
                klog!(0, "image: {}: {} growths, {} us on average and {} us at most; {} conversions, {} us on \
                           average and {} us at most", path, d.grows_timed.count, d.grows_timed.average_us(),
                       d.grows_timed.max_us(), d.conversions.count, d.conversions.average_us(),
                       d.conversions.max_us());
                klog!(0, "image: {}: {} growths made ahead of its writes, {} blocks; {} writes waited for one, {} us \
                           on average and {} us at most", path, d.aheads, d.ahead_blocks, d.ahead_waits.count,
                       d.ahead_waits.average_us(), d.ahead_waits.max_us());
                if let Some(fl) = self.flights.as_ref() {
                    klog!(0, "image: {}: {} requests served with the device asynchronously, {} by the batch",
                           path, fl.fast, fl.slow);
                    klog!(0, "image: {}: with the device {}", path, fl.fast_took);
                    klog!(0, "image: {}: by the batch {}", path, fl.slow_took);
                    klog!(0, "image: {}: waited for {}", path, fl.drained);
                }
                klog!(0, "image: {}: syncs {}", path, d.syncs);
            }
            None => klog!(0, "image: {} let go of -- read and written through the VFS, its filesystem keeping no \
                               blocks of its own on a device", path),
        }
        self.vfs.release(self.handle);
    }
}

/// What the image says of its map as it is let go of.
struct MapSays<'a>(Option<&'a Map>);

impl core::fmt::Display for MapSays<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(map) => write!(f, "its map {} runs", map.runs.len()),
            None => f.write_str("no map of its own"),
        }
    }
}

impl Direct {
    /// The device `device`, when blocks of `block` bytes on it are what a
    /// batch moves: a page holds a whole number of them, and each is whole
    /// sectors of the device's.
    fn fits(device: usize, block: usize) -> Option<Disk> {
        let disk = Disk::from_handle(device)?;
        let sector = disk.sector_size() as usize;
        if block == 0 || block > PAGE_SIZE || PAGE_SIZE % block != 0 || sector == 0 || block % sector != 0 {
            klog!(0, "image: blocks of {} bytes on sectors of {}: read and written through the VFS", block, sector);
            return None;
        }
        Some(disk)
    }

    /// For blocks of `block` bytes on `disk` (`fits`): None when there is
    /// no memory for the buffer.
    fn new(disk: Disk, block: usize) -> Option<Direct> {
        let batch = batch_buffer()?;
        let mut sectors = Vec::new();
        let mut unwritten = Vec::new();
        let mut fresh = Vec::new();
        let mut pieces = Vec::new();
        let mut stretches = Vec::new();
        sectors.try_reserve_exact(batch.pages()).ok()?;
        unwritten.try_reserve_exact(batch.pages()).ok()?;
        fresh.try_reserve_exact(batch.pages()).ok()?;
        pieces.try_reserve_exact(batch.pages()).ok()?;
        stretches.try_reserve_exact(batch.pages()).ok()?;
        sectors.resize(batch.pages(), 0);
        unwritten.resize(batch.pages(), false);
        fresh.resize(batch.pages(), false);
        let chunk = (CHUNK_BYTES / block as u64).max(1);
        Some(Direct { disk, block, batch, sectors, unwritten, fresh, pieces, stretches, map: None, chunk,
                      grown_to: u64::MAX, span: 0, stream_at: u64::MAX, stream_run: 0,
                      ahead_to: 0, ahead_span: 0, ahead: None, ahead_off: false, aheads: 0,
                      ahead_blocks: 0, ahead_waits: Timed::default(), through_vfs: false, read_off: 0, written_off: 0,
                      written_through: 0, punched: 0, allocated: 0, grows: false, refused: 0,
                      pending: Pending::default(), made_written: 0, grows_timed: Timed::default(),
                      conversions: Timed::default(), syncs: Spread::new() })
    }

    /// Where each of `n` blocks from `first` is on the device, into
    /// `sectors` from `slot` on -- and whether it is an unwritten extent's,
    /// and fresh -- from the map the image holds, or asked of the VFS where
    /// it holds none, which says an unwritten extent's blocks are holes.
    fn resolve(&mut self, vfs: &Vfs, handle: Handle, first: u64, slot: usize, n: usize) -> bool {
        let out = &mut self.sectors[slot..slot + n];
        let unwritten = &mut self.unwritten[slot..slot + n];
        let fresh = &mut self.fresh[slot..slot + n];
        match &self.map {
            Some(map) => {
                map.fill(first, out, unwritten);
                for (k, (f, &u)) in fresh.iter_mut().zip(unwritten.iter()).enumerate() {
                    *f = u && !self.pending.contains(first + k as u64);
                }
                true
            }
            None => {
                unwritten.fill(false);
                fresh.fill(false);
                vfs.map(handle, first, out)
            }
        }
    }

    /// File blocks `start..end`, written over unwritten extents, kept to be
    /// made written by the next sync -- or made written now, with what was
    /// kept, where there are as many runs kept as `PENDING_MAX` or no memory
    /// for one more: never a block written and not kept, which would read as
    /// zeros. false when they could not be made written.
    fn note_written(&mut self, vfs: &Vfs, handle: Handle, start: u64, end: u64) -> bool {
        if self.pending.insert(start, end) {
            return self.pending.runs.len() < PENDING_MAX || self.make_written(vfs, handle, None);
        }
        self.make_written(vfs, handle, Some((start, end - start)))
    }

    /// What it wrote over unwritten extents -- and `also`, a run it could
    /// not keep -- made written: the filesystem told (`Vfs::mark_written`),
    /// whose commit flushes the device before its commit block, so that the
    /// data is on the medium before the file says it is there -- and on true
    /// all of it on the medium, a flush of the device's own -- and the map
    /// taken again where it changed. What is kept is kept until it is done.
    /// false on a failure, by when some may be.
    fn make_written(&mut self, vfs: &Vfs, handle: Handle, also: Option<(u64, u64)>) -> bool {
        if self.pending.runs.is_empty() && also.is_none() {
            return true;
        }
        let mut runs = Vec::new();
        let kept = self.pending.as_counts(&mut runs);
        let from = boot_time_ns();
        let done = kept && vfs.mark_written(handle, &runs)
            && also.is_none_or(|run| vfs.mark_written(handle, &[run]));
        self.conversions.add(from);
        if trace::on::<ImageNote>() {
            let blocks = runs.iter().chain(also.iter()).fold(0u64, |sum, &(_, n)| sum.saturating_add(n));
            trace::emit(&ImageNote {
                from: runs.first().or(also.as_ref()).map_or(0, |&(first, _)| first),
                ns: ns_field(boot_time_ns().saturating_sub(from)),
                blocks: u32::try_from(blocks).unwrap_or(u32::MAX),
                pid: trace::pid(),
                ok: u8::from(done),
                _pad: [0; 3],
            });
        }
        /* Taken again whatever came of it: what the filesystem did make
         * written is so in the map from now. */
        for &(first, count) in runs.iter().chain(also.iter()) {
            self.retake(vfs, handle, first, count);
        }
        if !done {
            klog!(0, "image: {} blocks written over unwritten extents were not made written", self.pending.blocks());
            return false;
        }
        self.made_written += self.pending.blocks() + also.map_or(0, |(_, n)| n);
        self.pending.runs.clear();
        true
    }

    /// Blocks `first..first + count`, which the filesystem may have changed
    /// -- given blocks for holes, taken them back -- taken again into the map;
    /// the map dropped where they cannot be.
    fn retake(&mut self, vfs: &Vfs, handle: Handle, first: u64, count: u64) {
        if let Some(map) = self.map.as_mut() {
            if !map.retake(vfs, handle, first, count) {
                klog!(0, "image: its map of blocks {}..+{} cannot be taken again: every batch asks the VFS from \
                           here on", first, count);
                self.map = None;
            }
        }
    }

    /// The chunks blocks `first..first + n` are in, of a file of `blocks`
    /// blocks -- and as many after them as make twice the last growth's,
    /// where they begin where it ended (`GROW_MAX_CHUNKS`, `GROW_SHARE`) --
    /// given every block they lack, as unwritten extents, and taken into the
    /// map: whether the filesystem gave them.
    fn grow(&mut self, vfs: &Vfs, handle: Handle, first: u64, n: u64, blocks: u64) -> bool {
        let chunk = self.chunk;
        let from = first / chunk * chunk;
        let need = (first + n).div_ceil(chunk).saturating_mul(chunk).min(blocks);
        if from >= need {
            return false;
        }
        let span = if from == self.grown_to { self.span.saturating_mul(2).clamp(1, GROW_MAX_CHUNKS) } else { 1 };
        let started = boot_time_ns();
        let room = vfs.space_of_file(handle).map_or(0, |s| s.free_bytes / self.block as u64 / GROW_SHARE);
        let ahead = from.saturating_add(span.saturating_mul(chunk)).min(blocks).saturating_sub(need).min(room);
        let ahead = ahead / chunk * chunk;
        /* What is ahead with what the write needs, and where that is
         * refused -- the room went meanwhile -- what it needs alone. */
        let to = if ahead > 0 && self.give(vfs, handle, from, need + ahead) {
            need + ahead
        } else if self.give(vfs, handle, from, need) {
            need
        } else {
            0
        };
        self.grows_timed.add(started);
        if to > 0 {
            (self.grown_to, self.span) = (to, (to - from).div_ceil(chunk));
            return true;
        }
        /* Not wrong -- the write goes through the VFS, as every one into a
         * hole did before chunks -- but a commit for each such write, which
         * is what chunks are for: said at the first, and counted. */
        if self.refused == 0 {
            klog!(0, "image: its filesystem refused blocks {}..+{} ahead of a write into a hole: such writes go \
                       through the VFS", from, need - from);
        }
        self.refused += 1;
        false
    }

    /// Blocks `from..to` given every block they lack (`Vfs::allocate`) and
    /// taken into the map again, whatever came of it: what the filesystem
    /// gave before a refusal is the file's all the same. Whether it gave
    /// them all.
    fn give(&mut self, vfs: &Vfs, handle: Handle, from: u64, to: u64) -> bool {
        let given = vfs.allocate(handle, from, to - from);
        self.retake(vfs, handle, from, to - from);
        let Some(given) = given else { return false };
        self.allocated += given;
        true
    }

    /// The image's growth of blocks `from..to` made ahead of its writes by a
    /// task of its own (`ahead_task`), started now: whether it was. Where
    /// none can be had -- no memory, no task -- that is said once and the
    /// image grows itself from here on.
    fn start_ahead(&mut self, vfs: &'static Vfs, handle: Handle, from: u64, to: u64) -> bool {
        let made = match (Mutex::new(None), Event::new()) {
            (Some(given), Some(done)) => {
                let shared = Arc::new(Ahead {
                    vfs, handle, block: self.block as u64, from, to, given, done, ready: AtomicBool::new(false),
                });
                task::spawn_with("image-ahead", shared.clone(), ahead_task).map(|t| Grower { shared, task: Some(t) })
            }
            _ => None,
        };
        match made {
            Some(grower) => {
                self.ahead = Some(grower);
                true
            }
            None => {
                klog!(0, "image: no task to grow it ahead of its writes: each write into a hole grows it itself");
                self.ahead_off = true;
                false
            }
        }
    }

    /// The growth made ahead, once its task is done: taken into the map,
    /// and the task let go of. Before every request, a load when there is
    /// nothing. A refusal ends growing ahead.
    fn take_ahead(&mut self, vfs: &Vfs, handle: Handle) {
        if !self.ahead.as_ref().is_some_and(|g| g.shared.ready.load(Ordering::Acquire)) {
            return;
        }
        let Some(mut g) = self.ahead.take() else { return };
        /* Done with its file: its drop has nothing left to wait for. */
        drop(g.task.take());
        let (from, to) = (g.shared.from, g.shared.to);
        let Some((end, given)) = *g.shared.given.lock() else { return };
        /* Taken again whatever came of it: what the filesystem gave before a
         * refusal is the file's all the same. */
        if end > from {
            self.retake(vfs, handle, from, end - from);
        }
        match given {
            Some(n) if end > from => {
                self.aheads += 1;
                self.ahead_blocks += n;
                self.allocated += n;
            }
            _ => {
                klog!(0, "image: its filesystem refused blocks {}..+{} grown ahead of its writes, or had no room for \
                           them: each write into a hole grows it itself from here on", from, to - from);
                self.ahead_off = true;
            }
        }
    }

    /// A write of blocks `first..first + n` started, in a file of `blocks`.
    /// One that begins where the last ended goes on with a guest's stream of
    /// writes from one end of its disk; once it has gone on for
    /// `STREAM_MIN` of them, the stretch ahead of it -- from where the last
    /// one given ahead ends, or the chunk after the write: `AHEAD_FIRST`
    /// chunks, then twice the last's, to `AHEAD_MAX` -- is given every block
    /// it lacks by the image's task, and the next as soon as the stream is
    /// into the last. What is there already is left as it is, so that a disk whose
    /// free space is holes among blocks written once has its holes given
    /// ahead of the guest as a disk of holes does. A write that begins
    /// elsewhere ends the stream: a guest writing here and there has none
    /// given ahead of it.
    fn ask_ahead(&mut self, vfs: &'static Vfs, handle: Handle, first: u64, n: u64, blocks: u64) {
        let end = first.saturating_add(n);
        if first == self.stream_at {
            self.stream_run = self.stream_run.saturating_add(1);
        } else {
            (self.stream_run, self.ahead_to, self.ahead_span) = (0, 0, 0);
        }
        self.stream_at = end;
        if !self.grows || self.ahead_off || self.ahead.is_some() || self.map.is_none() || self.stream_run < STREAM_MIN
        {
            return;
        }
        let chunk = self.chunk;
        /* The stream not into the last stretch yet: not yet */
        if self.ahead_to > end && end <= self.ahead_to.saturating_sub(self.ahead_span.saturating_mul(chunk)) {
            return;
        }
        let from = self.ahead_to.max(end.div_ceil(chunk).saturating_mul(chunk));
        if from >= blocks {
            return;
        }
        let span = if self.ahead_span == 0 { AHEAD_FIRST } else { self.ahead_span.saturating_mul(2).min(AHEAD_MAX) };
        let to = from.saturating_add(span.saturating_mul(chunk)).min(blocks);
        /* A stretch the file has every block of -- a guest writing over its
         * disk, its journal going round -- is passed over, as the image's
         * own map says, and the next looked at as the stream goes on: only
         * one with a hole is worth a growth, from the chunk the hole is in.
         * On the EX44, growths asked over blocks the file had took the
         * guest's 1 MiB writes over its file from 4.6 GB/s to 1.5 and its
         * fdatasync from 736 a second to 371 (round 23). */
        let Some(hole) = self.map.as_ref().and_then(|map| map.first_hole(from, to)) else {
            (self.ahead_to, self.ahead_span) = (to, span);
            return;
        };
        let from = (hole / chunk * chunk).max(from);
        if self.start_ahead(vfs, handle, from, to) {
            (self.ahead_to, self.ahead_span) = (to, span);
        }
    }

    /// Whether blocks `first..first + n` are among what the growth under way
    /// ahead is giving: then waited for -- said in the log every
    /// `AHEAD_SAY_NS` it goes on -- and taken into the map, so that the
    /// write finds them rather than growing the image over what the task
    /// gives.
    fn wait_ahead(&mut self, vfs: &Vfs, handle: Handle, first: u64, n: u64) -> bool {
        let Some(g) = self.ahead.as_ref() else { return false };
        let shared = g.shared.clone();
        if first >= shared.to || shared.from >= first.saturating_add(n) {
            return false;
        }
        let began = boot_time_ns();
        let mut said = began;
        while !shared.ready.load(Ordering::Acquire) {
            if !shared.done.wait_for(Duration::from_millis(AHEAD_STEP_MS)) {
                let now = boot_time_ns();
                if now.saturating_sub(said) >= AHEAD_SAY_NS {
                    klog!(0, "image: a write into blocks {}..+{} has waited {} s for the growth ahead of it", first,
                           n, now.saturating_sub(began) / NS_PER_SEC);
                    said = now;
                }
            }
        }
        self.ahead_waits.add(began);
        self.take_ahead(vfs, handle);
        true
    }

    /// The growth under way ahead, if one is, waited for and taken into the
    /// map (`map_is_the_filesystems`) -- not counted as a write's wait.
    fn settle_ahead(&mut self, vfs: &Vfs, handle: Handle) {
        if let Some((from, to)) = self.ahead.as_ref().map(|g| (g.shared.from, g.shared.to)) {
            let waits = self.ahead_waits;
            self.wait_ahead(vfs, handle, from, to - from);
            self.ahead_waits = waits;
        }
    }

    /// The growth under way ahead waited for: before the image lets go of
    /// its file, which the task gives blocks of by its handle.
    fn stop_ahead(&mut self) {
        if let Some(mut g) = self.ahead.take() {
            /* Its drop waits for the task to end. */
            drop(g.task.take());
        }
    }
}

/// A growth of an image made ahead of its guest's writes, shared by the
/// image and the task of its own that makes it (`ahead_task`): the file,
/// what to give, what came of it, and the image's wait for it where a write
/// of its cannot go on without it (`Direct::wait_ahead`).
struct Ahead {
    vfs: &'static Vfs,
    handle: Handle,
    /// A block of the file's, in bytes: what the room it may take is counted
    /// in.
    block: u64,
    /// Blocks `from..to` to give.
    from: u64,
    to: u64,
    /// What it gave: where what it asked the filesystem for ends -- `to`,
    /// or short of it for want of room -- and how many blocks it gave, None
    /// where its filesystem refused them. None until it is done.
    given: Mutex<Option<(u64, Option<u64>)>>,
    /// The image's, when it waits: the growth done.
    done: Event,
    /// Set once `given` is: what the image's requests look at without the
    /// lock.
    ready: AtomicBool,
}

/// The image's side of a growth made ahead: what it shares with the task,
/// and the task, let go of -- waited for -- once it is done or the image is.
struct Grower {
    shared: Arc<Ahead>,
    task: Option<TaskHandle>,
}

/// A task of an image's own that gives it what is asked (`Vfs::allocate`)
/// ahead of its guest's writes -- no more than the image's share of the
/// room left (`GROW_SHARE`), whole chunks of it where that is less -- says
/// so, and ends: one growth, one task.
fn ahead_task(a: Arc<Ahead>) {
    /* What was asked where the image's share of the room holds it -- the
     * file's last blocks too, short of a chunk -- and whole chunks of that
     * share where it does not: none, and a refusal, where not one is left. */
    let block = a.block.max(1);
    let chunk = (CHUNK_BYTES / block).max(1);
    let room = a.vfs.space_of_file(a.handle).map_or(0, |s| s.free_bytes / block / GROW_SHARE);
    let want = a.to.saturating_sub(a.from);
    let len = if want <= room { want } else { room / chunk * chunk };
    let given = if len > 0 { a.vfs.allocate(a.handle, a.from, len) } else { None };
    *a.given.lock() = Some((a.from + len, given));
    a.ready.store(true, Ordering::Release);
    a.done.signal();
}

/// The data path's buffer: `BATCH_PAGES`, or the largest run of pages that
/// can be had short of it, down to one.
fn batch_buffer() -> Option<DmaBuffer> {
    let mut pages = BATCH_PAGES;
    loop {
        if let Some(buf) = DmaBuffer::new(pages) {
            return Some(buf);
        }
        if pages == 1 {
            klog!(0, "image: no memory for a buffer");
            return None;
        }
        pages /= 2;
    }
}

/* ---- the images a module holds (`kcore::fs::Image`) ----
 *
 * An image crosses as a word: a slot of the table and the generation of
 * what is in it, looked up on every call -- a word that is no image's reads
 * as none, which is why the calls that take nothing else are safe for any
 * word. A call holds its slot's lock for as long as it reads or writes:
 * one image's calls come one at a time, and nobody else's wait on them. */

/// Images held at once: every disk of every guest the hv module runs, and
/// as many again.
const MAX_IMAGES: usize = 128;

struct Slot {
    generation: u32,
    image: Option<Image>,
}

static IMAGES: OnceBox<Vec<Mutex<Slot>>> = OnceBox::new();

fn images() -> Option<&'static [Mutex<Slot>]> {
    let table = IMAGES.get_or_try_init(|| {
        let mut slots = Vec::new();
        slots.try_reserve_exact(MAX_IMAGES).ok()?;
        for _ in 0..MAX_IMAGES {
            slots.push(Mutex::new(Slot { generation: 0, image: None })?);
        }
        Some(Box::new(slots))
    });
    if table.is_none() {
        klog!(0, "image: no memory for the table of images");
    }
    table.map(|slots| slots.as_slice())
}

/* The slot, plus one, below; the generation above -- as a VFS handle. */
const SLOT_BITS: u32 = 32;
const SLOT_MASK: usize = (1 << SLOT_BITS) - 1;
const _: () = assert!(usize::BITS == 2 * SLOT_BITS, "an image is a slot and a generation");

fn word(slot: usize, generation: u32) -> usize {
    ((generation as usize) << SLOT_BITS) | (slot + 1)
}

/// Per slot, the queue of the image in it (`kernel_image_queue`): the
/// image's generation above, its queue's entry plus one below -- 0 for none.
/// What `kernel_image_wait` and `kernel_image_wake` find it by without the
/// slot's lock, which the queue's owner may hold for as long as a growth of
/// the image's file takes: the vCPU that wakes it is never held up by it.
static WAITS: [AtomicU64; MAX_IMAGES] = [const { AtomicU64::new(0) }; MAX_IMAGES];

/// The queue entry of the image `image` names, from `WAITS`: None for a
/// word that names no image with a queue.
fn queue_of(image: usize) -> Option<usize> {
    let at = (image & SLOT_MASK).checked_sub(1)?;
    let generation = (image >> SLOT_BITS) as u32;
    let word = WAITS.get(at)?.load(Ordering::Acquire);
    let entry = ((word & 0xFFFF_FFFF) as usize).checked_sub(1)?;
    ((word >> 32) as u32 == generation).then_some(entry)
}

/// The image `image` names, locked, for `work` -- or None for a word that
/// names none.
fn with<R>(image: usize, work: impl FnOnce(&mut Image) -> R) -> Option<R> {
    let at = (image & SLOT_MASK).checked_sub(1)?;
    let generation = (image >> SLOT_BITS) as u32;
    let mut slot = images()?.get(at)?.lock();
    if slot.generation != generation {
        return None;
    }
    slot.image.as_mut().map(work)
}

/// Holds the regular file at `path` -- or at `<path>.new`, where `locate`
/// finds it -- as a disk's image, for reading, and with `write` not 0 for
/// writing too: the image, or 0 -- no such file, one held already or
/// written by someone else, a read-only mount, no memory, every slot taken.
///
/// # Safety
/// `path` points at `path_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_open(path: *const u8, path_len: usize, write: i32) -> usize {
    let (Some(vfs), Some(path), Some(table)) =
        (crate::vfs_instance(), unsafe { crate::path(path, path_len) }, images())
    else {
        return 0;
    };
    let Some(at) = core::str::from_utf8(path).ok().and_then(crate::files::locate) else { return 0 };
    let Some(image) = Image::open(vfs, at.as_bytes(), write != 0) else { return 0 };

    let mut image = Some(image);
    for (index, slot) in table.iter().enumerate() {
        let mut slot = slot.lock();
        if slot.image.is_none() {
            slot.image = image.take();
            return word(index, slot.generation);
        }
    }
    klog!(0, "image: all {} images are held", MAX_IMAGES);
    /* Closed as it goes, with no slot's lock held. */
    drop(image);
    0
}

/// Lets go of the image; a word that is no image's is nothing to close.
#[no_mangle]
pub extern "C" fn kernel_image_close(image: usize) {
    let Some(at) = (image & SLOT_MASK).checked_sub(1) else { return };
    let Some(slot) = images().and_then(|table| table.get(at)) else { return };
    let taken = {
        let mut slot = slot.lock();
        if slot.generation != (image >> SLOT_BITS) as u32 {
            return;
        }
        let taken = slot.image.take();
        WAITS[at].store(0, Ordering::Release);
        /* The word names nothing from here on, whatever takes the slot. */
        slot.generation = slot.generation.wrapping_add(1);
        taken
    };
    /* Freed, and its file closed, off the slot's lock. */
    drop(taken);
}

/// The image's size in bytes, or -1 for a word that is no image's.
#[no_mangle]
pub extern "C" fn kernel_image_length(image: usize) -> i64 {
    with(image, |image| i64::try_from(image.size()).unwrap_or(-1)).unwrap_or(-1)
}

/// What a discard of the image gives back whole, in bytes -- its chunk --
/// or 0 where a discard gives back nothing, and for a word that is no
/// image's (`Image::discard_unit`).
#[no_mangle]
pub extern "C" fn kernel_image_discard_unit(image: usize) -> u64 {
    with(image, |image| image.discard_unit()).unwrap_or(0)
}

/// The mount the image's file is on, never 0 -- or 0 for a word that is no
/// image's. With `kernel_image_key`, which file it is.
#[no_mangle]
pub extern "C" fn kernel_image_mount(image: usize) -> u64 {
    with(image, |image| image.identity().0).unwrap_or(0)
}

/// Which file on its mount the image is (`kernel_file_key`): never 0 -- or
/// 0 for a word that is no image's.
#[no_mangle]
pub extern "C" fn kernel_image_key(image: usize) -> u64 {
    with(image, |image| image.identity().1).unwrap_or(0)
}

/// Fills `buf` -- `len` bytes -- from `offset` of the image, all of it
/// inside: 0, or -1.
///
/// # Safety
/// `buf` takes `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_read(image: usize, offset: u64, buf: *mut u8, len: usize) -> i32 {
    if buf.is_null() || isize::try_from(len).is_err() {
        return -1;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    match with(image, |image| image.read_at(offset, buf)) {
        Some(true) => 0,
        _ => -1,
    }
}

/// Writes `len` bytes at `offset` of the image, all of them inside -- the
/// image never grows: 0, or -1. Not synced: `kernel_image_sync` is.
///
/// # Safety
/// `data` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_write(image: usize, offset: u64, data: *const u8, len: usize) -> i32 {
    if data.is_null() || isize::try_from(len).is_err() {
        return -1;
    }
    let data = unsafe { core::slice::from_raw_parts(data, len) };
    match with(image, |image| image.write_at(offset, data)) {
        Some(true) => 0,
        _ => -1,
    }
}

/// Where the image has data from `offset` on (`Image::extent`): the first
/// byte of it into `out[0]`, holes before it, and the end of its run into
/// `out[1]` -- the two the same where none of the blocks looked at holds
/// any: 0, or -1.
///
/// # Safety
/// `out` takes two `u64`s.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_extent(image: usize, offset: u64, out: *mut u64) -> i32 {
    if out.is_null() || !out.is_aligned() {
        return -1;
    }
    match with(image, |image| image.extent(offset)) {
        Some(Some((start, end))) => {
            let out = unsafe { core::slice::from_raw_parts_mut(out, 2) };
            out[0] = start;
            out[1] = end;
            0
        }
        _ => -1,
    }
}

/// The whole blocks inside `len` bytes from `offset` given back -- holes,
/// reading as zeros -- a guest's discard: 0, or -1.
#[no_mangle]
pub extern "C" fn kernel_image_discard(image: usize, offset: u64, len: u64) -> i32 {
    match with(image, |image| image.discard(offset, len)) {
        Some(true) => 0,
        _ => -1,
    }
}

/// `len` bytes from `offset` made zeros, the whole blocks among them given
/// back with `unmap` not 0: 0, or -1.
#[no_mangle]
pub extern "C" fn kernel_image_write_zeroes(image: usize, offset: u64, len: u64, unmap: i32) -> i32 {
    match with(image, |image| image.write_zeroes(offset, len, unmap != 0)) {
        Some(true) => 0,
        _ => -1,
    }
}

/// Everything written to the image so far on its disk's medium -- a
/// guest's flush: 0, or -1.
#[no_mangle]
pub extern "C" fn kernel_image_sync(image: usize) -> i32 {
    match with(image, Image::sync) {
        Some(true) => 0,
        _ => -1,
    }
}

/* ---- an image's queue: a guest's disk's requests, as many at once as it
 * has, in buffers of the image's own (`Image::start`) ----
 *
 * The buffers are the image's, freed when it is let go of and not before --
 * once every request of them is done. Whoever holds the image fills a
 * buffer with a write's data before it starts the request, and takes a
 * read's out once it is reaped, and keeps off it in between: the image's,
 * and the device's, until `kernel_image_reap` gives it back. Every call but
 * `kernel_image_wake` from one task. */

/// Its queue (`Image::make_queue`): `buffers` buffers of `bytes` each, a
/// whole number of pages, their addresses into `out` -- 0, or -1: no
/// memory, a queue already, a word that is no image's.
///
/// # Safety
/// `out` takes `buffers` pointers.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_queue(image: usize, buffers: usize, bytes: usize, out: *mut *mut u8) -> i32 {
    if out.is_null() || !out.is_aligned() || buffers == 0 || buffers > IMAGE_QUEUE_MAX || bytes == 0
        || bytes % PAGE_SIZE != 0
    {
        return -1;
    }
    let out = unsafe { core::slice::from_raw_parts_mut(out, buffers) };
    let Some(at) = (image & SLOT_MASK).checked_sub(1) else { return -1 };
    let made = with(image, |img| {
        if !img.make_queue(buffers, bytes / PAGE_SIZE) {
            return None;
        }
        for (k, o) in out.iter_mut().enumerate() {
            *o = img.buffer_address(k)?;
        }
        img.queue_entry()
    });
    match made {
        Some(Some(entry)) => {
            /* Under no lock: a word stale by now is the next image's, whose
             * wake is a spurious one -- never a wait on what is gone. */
            WAITS[at].store(((image >> SLOT_BITS) as u64) << 32 | (entry as u64 + 1), Ordering::Release);
            0
        }
        _ => -1,
    }
}

/// Buffer `buffer`'s request -- `len` bytes from `offset` read into it, or
/// with `write` not 0 written from it -- to the image (`Image::start`): 1
/// while it is with the device, until `kernel_image_reap` gives it back; 0
/// done now; -1 done, and failed. Its doorbell waits for
/// `kernel_image_kick`.
#[no_mangle]
pub extern "C" fn kernel_image_start(image: usize, buffer: usize, write: i32, offset: u64, len: usize) -> i32 {
    match with(image, |img| img.start(buffer, write != 0, offset, len)) {
        Some(Started::InFlight) => 1,
        Some(Started::Done(true)) => 0,
        _ => -1,
    }
}

/// The doorbell for the requests started.
#[no_mangle]
pub extern "C" fn kernel_image_kick(image: usize) {
    with(image, Image::kick);
}

/// Requests done, as many as there are and `max` at most, into `out` --
/// each its buffer and how it went: how many, or -1 for a word that is no
/// image's with a queue.
///
/// # Safety
/// `out` takes `max` of them.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_reap(image: usize, out: *mut ImageDone, max: usize) -> isize {
    if out.is_null() || !out.is_aligned() || max == 0 || isize::try_from(max).is_err() {
        return -1;
    }
    let out = unsafe { core::slice::from_raw_parts_mut(out, max) };
    let reaped = with(image, |img| {
        img.flights.as_ref()?;
        let mut n = 0;
        while n < out.len() {
            let Some((buffer, ok)) = img.reap() else { break };
            out[n] = ImageDone { buffer: buffer as u32, ok: u32::from(ok) };
            n += 1;
        }
        Some(n)
    });
    match reaped {
        Some(Some(n)) => n as isize,
        _ => -1,
    }
}

/// Spins up to `poll_ns` -- with no lock of the image's held -- for what
/// `kernel_image_wait` sleeps for: 1 when one came, 0 when the time ran
/// out, -1 at once for a word that is no image's with a queue.
#[no_mangle]
pub extern "C" fn kernel_image_poll(image: usize, poll_ns: u64) -> i32 {
    match queue_of(image) {
        Some(entry) => i32::from(block::queue::poll(entry, Duration::from_nanos(poll_ns))),
        None => -1,
    }
}

/// Sleeps until one of the image's commands is done, or `kernel_image_wake`
/// wakes it, or `timeout_ns` is up -- with no lock of the image's held: 1
/// when one came, 0 when the time ran out, -1 at once for a word that is no
/// image's with a queue.
#[no_mangle]
pub extern "C" fn kernel_image_wait(image: usize, timeout_ns: u64) -> i32 {
    match queue_of(image) {
        Some(entry) => i32::from(block::queue::wait(entry, Duration::from_nanos(timeout_ns))),
        None => -1,
    }
}

/// Wakes the image's queue's waiter, from anywhere: what a request handed
/// to its owner wakes it with. Nothing, for a word that is no image's with
/// a queue.
#[no_mangle]
pub extern "C" fn kernel_image_wake(image: usize) {
    if let Some(entry) = queue_of(image) {
        block::queue::wake(entry);
    }
}
