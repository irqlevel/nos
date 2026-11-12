//! A device's queue for a loadable module (`kcore::block::DiskQueue`): what
//! a guest's disk on a partition or a whole disk of nos's is served by --
//! `fs::image`'s queue (`kernel_image_*`) with no file under it.
//!
//! A queue is a `queue::Queue`'s buffers, which the module fills and empties
//! between its requests, and its claim on its device, named as the module
//! asked, for as long as it is open: no mount, disk log or other writer has
//! the device meanwhile, nor anything overlapping it. A request is up to a
//! buffer, from the buffer's start: started on the device as commands of
//! what it moves at once and given back whole when the last is done -- or,
//! on a device with no asynchronous path, done there and then a page at a
//! time.
//!
//! A queue crosses as a word, a slot of the table and the generation of
//! what is in it, as an image does: looked up on every call, so a word that
//! is no queue's names nothing. A call holds its slot's lock for as long as
//! it works: a queue's calls come one at a time -- its owner's -- and a
//! wake (`kernel_blockq_wake`) takes no lock at all.
//!
//! A queue may be opened on a task's event (`events`, a module's handle of
//! one): its completions and wakes signal it too, and the queue holds it
//! until it is closed (`queue::Queue`).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use events::SharedEvent;
use ffi::block::QueueDone;
use kcore::block::SubmitError;
use kcore::consts::PAGE_SIZE;
use kcore::klog;
use kcore::once::OnceBox;
use kcore::sync::Mutex;
use kcore::time::{boot_time_ns, Duration};

use crate::disk::Disk;
use crate::queue::{self, Queue};
use crate::table::{self, Holder, Piece};

/// Queues open at once: every disk of every guest the hv module runs, and
/// as many again.
const MAX_QUEUES: usize = 128;

/// The most buffers a queue has -- as many I/O as the queue takes at once
/// -- and the most bytes each has.
const BUFFERS_MAX: usize = queue::IN_FLIGHT_MAX;
const BYTES_MAX: usize = 1 << 20;

/// How long a request waits for room on a device whose queue is full of
/// other people's commands before it is failed and said; and how long it
/// waits between looks.
const BUSY_GIVE_UP_NS: u64 = 10_000_000_000;
const BUSY_STEP_MS: u64 = 100;

/// A buffer's request while the device has it.
#[derive(Default, Clone, Copy)]
struct Flight {
    busy: bool,
    /// Its commands not yet done, and whether every one went well.
    left: usize,
    ok: bool,
}

/// A claim on the device, given back when it goes: after the queue, which
/// waits for its I/O (field order).
struct Claim(usize);

impl Drop for Claim {
    fn drop(&mut self) {
        table::release(self.0);
    }
}

struct DevQueue {
    disk: Disk,
    writable: bool,
    /// Its buffers and their I/O: dropped first, which waits for what the
    /// device still has.
    queue: Queue,
    flights: Vec<Flight>,
    /// Requests done that `reap` has not given back yet, in the order they
    /// finished: room for every buffer.
    done: VecDeque<(usize, bool)>,
    /// A synchronous request's pieces: room for a buffer's pages.
    pieces: Vec<Piece>,
    _claim: Claim,
}

/// What `start` did with a request.
enum Started {
    InFlight,
    Done(bool),
}

impl DevQueue {
    /// `disk`'s queue -- `buffers` buffers of `bytes` each -- for writing
    /// to it with `writable`, its claim held by `holder` whether it writes
    /// or not: a VM that reads its disk has it to itself as well; made on
    /// `event`, a task's, when one is given. Who holds what overlaps it,
    /// when that is in the way; None for memory.
    fn open(disk: Disk, writable: bool, holder: Holder, buffers: usize, bytes: usize,
            event: Option<Arc<SharedEvent>>) -> Result<Option<DevQueue>, Holder>
    {
        let claim = Claim(table::claim(disk.handle(), holder)?);
        let pages = bytes / PAGE_SIZE;
        let Some(queue) = Queue::new(Some(disk), buffers, pages, event) else { return Ok(None) };
        let (mut flights, mut done, mut pieces) = (Vec::new(), VecDeque::new(), Vec::new());
        if flights.try_reserve_exact(buffers).is_err() || done.try_reserve_exact(buffers).is_err()
            || pieces.try_reserve_exact(pages).is_err()
        {
            return Ok(None);
        }
        flights.resize(buffers, Flight::default());
        Ok(Some(DevQueue { disk, writable, queue, flights, done, pieces, _claim: claim }))
    }

    /// The device's size, in bytes.
    fn bytes(&self) -> u64 {
        self.disk.sectors().saturating_mul(self.disk.sector_size())
    }

    /// Buffer `k`'s request -- `len` bytes from `offset` on the device read
    /// into it, or with `write` written from it, from its start -- taken:
    /// with the device until `reap` gives it back, or done now. A buffer
    /// with a request already, one past the queue, one longer than a
    /// buffer, not whole sectors or not inside the device, and a write to a
    /// queue opened for reading are each done, and failed. What the device
    /// is handed waits for `kick`.
    fn start(&mut self, k: usize, write: bool, offset: u64, len: usize) -> Started {
        let ss = self.disk.sector_size() as usize;
        if self.flights.get(k).is_none_or(|f| f.busy) || len == 0 || len > self.queue.buffer_len()
            || ss == 0 || offset % ss as u64 != 0 || len % ss != 0 || (write && !self.writable)
            || offset.checked_add(len as u64).is_none_or(|end| end > self.bytes())
        {
            return Started::Done(false);
        }
        let sector = offset / ss as u64;
        let Some(max) = self.queue.asynchronous() else { return Started::Done(self.now(k, write, sector, len)) };

        /* Commands of what the device moves at once, from the buffer's
         * start on: one, for a guest's request on an NVMe. */
        let count = len.div_ceil(max);
        let began = boot_time_ns();
        self.flights[k] = Flight { busy: true, left: count, ok: true };
        for i in 0..count {
            let at = i * max;
            let n = (len - at).min(max);
            loop {
                match self.queue.start(k, at, n, sector + (at / ss) as u64, write) {
                    Ok(()) => break,
                    Err(SubmitError::Busy) if i == 0 && self.queue.in_flight() == 0 => {
                        /* Full of someone else's commands and none of this
                         * queue's to wait for: by the synchronous path,
                         * which waits for a command ID. */
                        self.flights[k].busy = false;
                        return Started::Done(self.now(k, write, sector, len));
                    }
                    Err(SubmitError::Busy) if boot_time_ns().saturating_sub(began) < BUSY_GIVE_UP_NS => {
                        /* One of its own commands done makes room -- or,
                         * none out, someone else's, looked for shortly. */
                        self.queue.kick();
                        let step = if self.queue.in_flight() > 0 { BUSY_STEP_MS } else { 1 };
                        self.queue.wait(Duration::from_millis(step));
                        self.collect();
                    }
                    Err(_) => {
                        /* Never, for a request checked as this one was --
                         * but said, and failed: what was handed over is
                         * still the device's, and the request done when
                         * that is. */
                        klog!(0, "block: {}: a command of {} bytes at sector {} was refused, or found no room for {} s",
                               self.disk.name(), n, sector, BUSY_GIVE_UP_NS / 1_000_000_000);
                        let f = &mut self.flights[k];
                        f.ok = false;
                        f.left -= count - i;
                        if f.left == 0 {
                            f.busy = false;
                            return Started::Done(false);
                        }
                        return Started::InFlight;
                    }
                }
            }
        }
        Started::InFlight
    }

    /// The synchronous path: `len` bytes of buffer `k` from its start, a
    /// page at a time, from `sector` on.
    fn now(&mut self, k: usize, write: bool, sector: u64, len: usize) -> bool {
        let ss = self.disk.sector_size() as usize;
        self.pieces.clear();
        let mut at = 0;
        while at < len {
            let n = (len - at).min(PAGE_SIZE);
            /* Into the room taken at `open`: a buffer's pages. */
            self.pieces.push(Piece { sector: sector + (at / ss) as u64, at, len: n });
            at += n;
        }
        self.queue.now(k, &self.pieces, write)
    }

    /// The commands the device has finished, each to its request: a request
    /// whose last it was onto `done`.
    fn collect(&mut self) {
        while let Some((k, ok)) = self.queue.reap() {
            let Some(f) = self.flights.get_mut(k).filter(|f| f.busy && f.left > 0) else {
                klog!(0, "block: {}: a command done for buffer {}, which has no request out", self.disk.name(), k);
                continue;
            };
            f.left -= 1;
            f.ok &= ok;
            if f.left == 0 {
                f.busy = false;
                /* Into the room taken at `open`: a request a buffer. */
                self.done.push_back((k, f.ok));
            }
        }
    }

    /// A request done, if one is: its buffer, and whether it went well.
    fn reap(&mut self) -> Option<(usize, bool)> {
        if self.done.is_empty() {
            self.collect();
        }
        self.done.pop_front()
    }
}

/* ---- the table ---- */

struct Slot {
    generation: u32,
    queue: Option<DevQueue>,
}

static QUEUES: OnceBox<Vec<Mutex<Slot>>> = OnceBox::new();

/// Per slot, its queue's generation above and its `queue::Queue`'s entry
/// plus one below -- 0 for none: what a wait, a poll and a wake find it by
/// without the slot's lock, which the owner may hold for as long as a
/// synchronous request takes.
static WAITS: [AtomicU64; MAX_QUEUES] = [const { AtomicU64::new(0) }; MAX_QUEUES];

fn slots() -> Option<&'static [Mutex<Slot>]> {
    let table = QUEUES.get_or_try_init(|| {
        let mut slots = Vec::new();
        slots.try_reserve_exact(MAX_QUEUES).ok()?;
        for _ in 0..MAX_QUEUES {
            slots.push(Mutex::new(Slot { generation: 0, queue: None })?);
        }
        Some(Box::new(slots))
    });
    if table.is_none() {
        klog!(0, "block: no memory for the table of device queues");
    }
    table.map(|slots| slots.as_slice())
}

/* The slot, plus one, below; the generation above. */
const SLOT_BITS: u32 = 32;
const SLOT_MASK: usize = (1 << SLOT_BITS) - 1;
const _: () = assert!(usize::BITS == 2 * SLOT_BITS, "a queue is a slot and a generation");

fn word(slot: usize, generation: u32) -> usize {
    ((generation as usize) << SLOT_BITS) | (slot + 1)
}

/// The queue `q` names, locked, for `work` -- or None for a word that names
/// none.
fn with<R>(q: usize, work: impl FnOnce(&mut DevQueue) -> R) -> Option<R> {
    let at = (q & SLOT_MASK).checked_sub(1)?;
    let generation = (q >> SLOT_BITS) as u32;
    let mut slot = slots()?.get(at)?.lock();
    if slot.generation != generation {
        return None;
    }
    slot.queue.as_mut().map(work)
}

/// The `queue::Queue` entry of the queue `q` names, from `WAITS`.
fn entry_of(q: usize) -> Option<usize> {
    let at = (q & SLOT_MASK).checked_sub(1)?;
    let generation = (q >> SLOT_BITS) as u32;
    let word = WAITS.get(at)?.load(Ordering::Acquire);
    let entry = ((word & 0xFFFF_FFFF) as usize).checked_sub(1)?;
    ((word >> 32) as u32 == generation).then_some(entry)
}

/* ---- what a module calls ---- */

/// A queue on device `device`: `buffers` buffers of `bytes` each -- a whole
/// number of pages -- their addresses into `out`, the device claimed for
/// the holder the `name_len` bytes at `name` name -- "a module writing to
/// it" for none, as `kernel_blockdev_claim` has it -- for writing with
/// `write` not 0, made on the task's event `event` names unless it is 0:
/// the queue's word. 0 for memory, a word that names no device or no event,
/// or arguments past what a queue takes -- or with who holds what is in the
/// way copied into `refused` (`cap` bytes) and its length, `cap` at most,
/// into `*refused_len`, which is 0 otherwise.
///
/// # Safety
/// `out` takes `buffers` pointers; `name`, unless `name_len` is 0, points
/// at `name_len` readable bytes; `refused`, unless null, takes `cap` bytes;
/// `refused_len`, unless null, is writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockq_open(
    device: usize, write: i32, buffers: usize, bytes: usize, name: *const u8, name_len: usize, out: *mut *mut u8,
    refused: *mut u8, cap: usize, refused_len: *mut usize, event: u64,
) -> usize {
    if let Some(len) = unsafe { refused_len.as_mut() } {
        *len = 0;
    }
    if out.is_null() || !out.is_aligned() || buffers == 0 || buffers > BUFFERS_MAX || bytes == 0
        || bytes > BYTES_MAX || bytes % PAGE_SIZE != 0
    {
        return 0;
    }
    let Some(disk) = Disk::from_handle(device) else { return 0 };
    /* The task's event, looked up: the queue holds it from here on, so the
     * module may let go of its handle first */
    let event = match event {
        0 => None,
        handle => match events::lookup(handle) {
            Some(e) => Some(e),
            None => return 0,
        },
    };
    let out = unsafe { core::slice::from_raw_parts_mut(out, buffers) };
    let holder = if name.is_null() || name_len == 0 {
        Holder::of(table::MODULE_HOLDER)
    } else {
        Holder::named(unsafe { core::slice::from_raw_parts(name, name_len) })
    };
    let Some(slots) = slots() else { return 0 };

    /* Opened before a slot is looked for: a claim refused, or memory
     * short, takes no slot -- and a slot's lock is never held across the
     * allocation. */
    let mut queue = match DevQueue::open(disk, write != 0, holder, buffers, bytes, event) {
        Ok(Some(queue)) => queue,
        Ok(None) => return 0,
        Err(in_the_way) => {
            let n = unsafe { table::copy_out(in_the_way.as_bytes(), refused, cap) };
            if let Some(len) = unsafe { refused_len.as_mut() } {
                *len = n.min(cap);
            }
            return 0;
        }
    };
    for (k, o) in out.iter_mut().enumerate() {
        match queue.queue.address(k) {
            Some(at) => *o = at,
            None => return 0,
        }
    }
    let entry = queue.queue.entry();
    for (at, slot) in slots.iter().enumerate() {
        let mut slot = slot.lock();
        if slot.queue.is_some() {
            continue;
        }
        slot.generation = slot.generation.wrapping_add(1);
        slot.queue = Some(queue);
        let generation = slot.generation;
        WAITS[at].store((generation as u64) << 32 | (entry as u64 + 1), Ordering::Release);
        return word(at, generation);
    }
    klog!(0, "block: all {} device queues are open", MAX_QUEUES);
    /* Dropped here, off every slot's lock: it waits for nothing, having
     * started nothing. */
    drop(queue);
    0
}

/// Buffer `buffer`'s request -- `len` bytes from byte `offset` of the
/// device read into it, or with `write` not 0 written from it -- started: 1
/// while the device has it, until `kernel_blockq_reap` gives it back; 0
/// done now; -1 done, and failed. Its doorbell waits for
/// `kernel_blockq_kick`.
#[no_mangle]
pub extern "C" fn kernel_blockq_start(q: usize, buffer: usize, write: i32, offset: u64, len: usize) -> i32 {
    match with(q, |dq| dq.start(buffer, write != 0, offset, len)) {
        Some(Started::InFlight) => ffi::block::QUEUE_IN_FLIGHT,
        Some(Started::Done(true)) => ffi::block::QUEUE_DONE,
        _ => ffi::block::QUEUE_FAILED,
    }
}

/// The doorbell for the requests started.
#[no_mangle]
pub extern "C" fn kernel_blockq_kick(q: usize) {
    with(q, |dq| dq.queue.kick());
}

/// Requests done, as many as there are and `max` at most, into `out` --
/// each its buffer and how it went: how many, or -1 for a word that is no
/// queue's.
///
/// # Safety
/// `out` takes `max` of them.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockq_reap(q: usize, out: *mut QueueDone, max: usize) -> isize {
    if out.is_null() || !out.is_aligned() || max == 0 || isize::try_from(max).is_err() {
        return -1;
    }
    let out = unsafe { core::slice::from_raw_parts_mut(out, max) };
    let reaped = with(q, |dq| {
        let mut n = 0;
        while n < out.len() {
            let Some((buffer, ok)) = dq.reap() else { break };
            out[n] = QueueDone { buffer: buffer as u32, ok: u32::from(ok) };
            n += 1;
        }
        n
    });
    match reaped {
        Some(n) => n as isize,
        None => -1,
    }
}

/// Sleeps up to `timeout_ns` -- with no lock of the queue's held -- until
/// one of its commands is done, or a `kernel_blockq_wake`: 1 when one came,
/// 0 when the time ran out, -1 at once for a word that is no queue's.
#[no_mangle]
pub extern "C" fn kernel_blockq_wait(q: usize, timeout_ns: u64) -> i32 {
    match entry_of(q) {
        Some(entry) => i32::from(queue::wait(entry, Duration::from_nanos(timeout_ns))),
        None => -1,
    }
}

/// What `kernel_blockq_wait` sleeps for, spun for for up to `poll_ns`.
#[no_mangle]
pub extern "C" fn kernel_blockq_poll(q: usize, poll_ns: u64) -> i32 {
    match entry_of(q) {
        Some(entry) => i32::from(queue::poll(entry, Duration::from_nanos(poll_ns))),
        None => -1,
    }
}

/// Wakes whoever waits on the queue, from any context; nothing for a word
/// that is no queue's.
#[no_mangle]
pub extern "C" fn kernel_blockq_wake(q: usize) {
    if let Some(entry) = entry_of(q) {
        queue::wake(entry);
    }
}

/// The device's write cache pushed out: 0 once it is, -1 when it failed.
#[no_mangle]
pub extern "C" fn kernel_blockq_flush(q: usize) -> i32 {
    match with(q, |dq| dq.disk.flush().is_ok()) {
        Some(true) => 0,
        _ => -1,
    }
}

/// `len` bytes from byte `offset` of the device -- whole sectors, inside
/// it -- given back to it: 0 once it has been told, -1 when it takes no
/// such thing, failed, or the queue was opened for reading.
#[no_mangle]
pub extern "C" fn kernel_blockq_discard(q: usize, offset: u64, len: u64) -> i32 {
    let ok = with(q, |dq| {
        let ss = dq.disk.sector_size();
        dq.writable && ss != 0 && offset % ss == 0 && len % ss == 0
            && offset.checked_add(len).is_some_and(|end| end <= dq.bytes())
            && dq.disk.discard(offset / ss, len / ss).is_ok()
    });
    if ok == Some(true) { 0 } else { -1 }
}

/// The queue closed: what the device still has waited for, its buffers
/// freed and its claim given back. Its word names nothing after.
#[no_mangle]
pub extern "C" fn kernel_blockq_close(q: usize) {
    let Some(at) = (q & SLOT_MASK).checked_sub(1) else { return };
    let generation = (q >> SLOT_BITS) as u32;
    let Some(slot) = slots().and_then(|s| s.get(at)) else { return };
    let taken = {
        let mut slot = slot.lock();
        if slot.generation != generation {
            return;
        }
        WAITS[at].store(0, Ordering::Release);
        slot.queue.take()
    };
    /* Off the slot's lock: the drop waits for the device. */
    drop(taken);
}
