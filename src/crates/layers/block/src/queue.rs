//! A disk's asynchronous I/O as the kernel image's own code has it: to and
//! from buffers the queue owns, several requests with the device at once,
//! each one's completion left by the device's interrupt where the queue's
//! owner takes it in task context (`reap`) -- what a guest's disk is served
//! with (`fs::image`), so that the device has every request the guest has
//! out, and each comes back as it is done rather than with the slowest of a
//! batch.
//!
//! What makes it safe is who owns what. The buffers are the queue's, made
//! when it is and freed only when it goes -- and it goes only once every I/O
//! it handed the device is done (`Drop`), so the device never reads or
//! writes memory that is someone else's by then. An I/O names its buffer by
//! index and a range inside it, which the queue checks against the buffer;
//! and a buffer with I/O on it is not to be had (`bytes`), so nothing here
//! touches memory the device may be writing. The completion's context is
//! not a pointer: it is the number of the queue's entry in a table that
//! lives for good (`DONE`) and the buffer's, looked up in the interrupt --
//! a completion that comes late finds an entry that is still there.
//!
//! The device's doorbell is left for `kick`, so that the I/O started
//! together goes to it in one write. A disk without the asynchronous path
//! (`Disk::can_submit`) still has its queue and buffers -- its owner reads
//! and writes them synchronously -- and `start` answers `Unsupported`.
//!
//! A queue may be made on the event of a task that waits for this and more
//! (`events::SharedEvent`, plans/ipc.md I2): its completions and wakes then
//! signal that event as well as its own. The entry holds the event under a
//! lock of its own, interrupt-safe, which a completion signals it under and
//! the queue's drop takes it out under -- so that once the drop has it, no
//! completion is still in its signal -- and lets go of it after, off the
//! lock: nothing signals the task's event once it may be freed, and nothing
//! in the interrupt path takes a count of it, or drops one.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use events::SharedEvent;
use ffi::block::BlockIo;
use kcore::block::{SubmitError, IO_FLUSH, IO_READ, IO_WRITE};
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::once::OnceBox;
use kcore::ring::LocklessRing;
use kcore::sync::{Event, IrqSpinLock};
use kcore::time::{boot_time_ns, Duration};
use kcore::trace::{self, BioDone, BioSubmit};
use kcore::klog;

use crate::Disk;

/// Queues there may be at once: every disk of every guest, and more.
pub const QUEUES: usize = 128;

/// The most I/O a queue has with the device at once -- and so the room its
/// completions have (`Done::ring`), which can never fill.
pub const IN_FLIGHT_MAX: usize = 256;

/// How long `Drop` waits for its I/O before it says so, and before it gives
/// up waiting: its buffers are then never freed -- the device may still
/// write them -- and its entry never used again.
const DRAIN_SAY_NS: u64 = 10_000_000_000;
const DRAIN_GIVE_UP_NS: u64 = 120_000_000_000;

/// Where a queue's completions are left -- and its owner woken, by them or
/// by whoever has more for it (`wake`). An entry of `DONE`: made the first
/// time a queue takes it, and kept for good.
struct Done {
    /// A completion: its buffer, shifted up one, and a 1 below for a failure.
    ring: LocklessRing,
    /// The queue's own: what its `wait` sleeps on, and every wait inside its
    /// calls -- so that a drop from any task never waits on a task's event.
    event: Event,
    /// The event of the task the queue was made on, while it is open:
    /// signalled under this lock with `event`, and taken out under it by
    /// the queue's drop, which lets go of it after (`let_go_of_task_event`).
    task_event: IrqSpinLock<Option<Arc<SharedEvent>>>,
    /// Whether `task_event` may hold one: what a completion looks at before
    /// it takes the lock, so that a queue on no task's event pays a load.
    on_task_event: AtomicBool,
}

impl Done {
    /// The queue's owner woken: its own event, and the task's it was made on.
    /// From any context.
    #[inline]
    fn signal(&self) {
        self.event.signal();
        if self.on_task_event.load(Ordering::Acquire) {
            /* Under the lock: the drop takes it out under the same one, and
             * so cannot let go of it while this signal is made */
            if let Some(e) = self.task_event.lock().as_ref() {
                e.signal();
            }
        }
    }
}

static DONE: [OnceBox<Done>; QUEUES] = [const { OnceBox::new() }; QUEUES];
/// Which entries a queue has.
static TAKEN: [AtomicBool; QUEUES] = [const { AtomicBool::new(false) }; QUEUES];

/// The completion of one I/O, from the device's interrupt: onto its queue's
/// ring, and its owner woken. The context is a number (`context`).
extern "C" fn on_done(ctx: *mut u8, status: i32) {
    let word = ctx as usize;
    let (entry, buffer) = ((word >> 32).wrapping_sub(1), word & 0xFFFF_FFFF);
    let Some(done) = DONE.get(entry).and_then(OnceBox::get) else {
        klog!(0, "block: a completion for queue {}, which there is not", entry);
        return;
    };
    /* Before it is handed on: whoever reaps it records after this. */
    bio_done(entry, buffer, status == 0, false);
    if !done.ring.push(buffer << 1 | usize::from(status != 0)) {
        /* Never: the ring has room for every I/O the queue can have out. */
        klog!(0, "block: queue {} has no room for a completion: its owner waits for it for good", entry);
    }
    done.signal();
}

/// `bio_done`: queue `entry`'s command on `buffer` done -- `ok`, or not --
/// or, `refused`, never handed to the device after all.
#[inline]
fn bio_done(entry: usize, buffer: usize, ok: bool, refused: bool) {
    if trace::on::<BioDone>() {
        trace::emit(&BioDone {
            entry: u16::try_from(entry).unwrap_or(u16::MAX),
            buffer: u16::try_from(buffer).unwrap_or(u16::MAX),
            ok: u8::from(ok),
            refused: u8::from(refused),
            _pad: [0; 2],
        });
    }
}

/// An I/O's context: its queue's entry, plus one, above; its buffer below.
fn context(entry: usize, buffer: usize) -> *mut u8 {
    (((entry + 1) << 32) | buffer) as *mut u8
}

/// Wakes whoever waits on queue `entry` (`Queue::wait`) -- and on the event
/// of the task it was made on: from anywhere, with no lock of the queue's
/// -- the entry is there for good. An entry no queue has is woken for
/// nothing.
pub fn wake(entry: usize) {
    if let Some(done) = DONE.get(entry).and_then(OnceBox::get) {
        done.signal();
    }
}

/// Sleeps until queue `entry` has a completion or a `wake`, or `timeout` is
/// up: whether one came -- false at once for an entry that was never made.
/// One task waits on an entry at a time: its queue's owner, whether here or
/// in `Queue::wait`.
pub fn wait(entry: usize, timeout: Duration) -> bool {
    match DONE.get(entry).and_then(OnceBox::get) {
        Some(done) => done.event.wait_for(timeout),
        None => false,
    }
}

/// `wait`'s signal looked for for up to `poll`, spinning rather than
/// sleeping -- the owner's call, before its `wait`: whether one came.
pub fn poll(entry: usize, poll: Duration) -> bool {
    match DONE.get(entry).and_then(OnceBox::get) {
        Some(done) => done.event.poll(poll),
        None => false,
    }
}

pub struct Queue {
    /// The disk its I/O goes to; None for a queue that has only buffers.
    disk: Option<Disk>,
    /// Whether the disk takes asynchronous I/O, and the most bytes one moves.
    max: usize,
    entry: usize,
    bufs: Vec<DmaBuffer>,
    /// A buffer's bytes: what was asked for, whatever the allocator gave.
    len: usize,
    /// The I/O in flight on each buffer, and in all.
    busy: Vec<u32>,
    in_flight: usize,
}

impl Queue {
    /// `buffers` buffers of `pages` pages each, physically contiguous, and
    /// asynchronous I/O to `disk` from them where it takes any -- made on
    /// `event`, a task's, when one is given: its completions and wakes then
    /// signal that event too, which it holds until it is dropped. None when
    /// there is no memory for them, or every queue there may be is taken.
    pub fn new(disk: Option<Disk>, buffers: usize, pages: usize, event: Option<Arc<SharedEvent>>) -> Option<Queue> {
        let len = pages.checked_mul(PAGE_SIZE)?;
        let entry = take_entry()?;
        let mut queue = Queue { disk, max: 0, entry, bufs: Vec::new(), len, busy: Vec::new(), in_flight: 0 };
        /* From here the drop gives the entry back. */
        queue.bufs.try_reserve_exact(buffers).ok()?;
        queue.busy.try_reserve_exact(buffers).ok()?;
        for _ in 0..buffers {
            queue.bufs.push(DmaBuffer::new(pages).filter(|b| b.pages() >= pages)?);
            queue.busy.push(0);
        }
        if let Some(disk) = queue.disk.filter(|d| d.can_submit()) {
            let sector = disk.sector_size() as usize;
            /* What the device moves from a page's start, less a page: an
             * I/O from inside one reaches a page more than its length --
             * and whole pages, which are whole sectors. */
            if sector != 0 && PAGE_SIZE % sector == 0 {
                queue.max = disk.submit_max().saturating_sub(PAGE_SIZE) / PAGE_SIZE * PAGE_SIZE;
            }
        }
        if let (Some(event), Some(done)) = (event, DONE[entry].get()) {
            /* Last, once nothing above can fail: none of its I/O is out yet.
             * The entry's last owner let go of its own (`Drop`), so what is
             * taken out here is nothing -- and is dropped off the lock. */
            let before = core::mem::replace(&mut *done.task_event.lock(), Some(event));
            done.on_task_event.store(true, Ordering::Release);
            drop(before);
        }
        Some(queue)
    }

    /// The number of its entry in the table: what `wake` takes.
    pub fn entry(&self) -> usize {
        self.entry
    }

    /// Whether `start` hands its disk I/O, and the most bytes one moves.
    pub fn asynchronous(&self) -> Option<usize> {
        (self.max != 0).then_some(self.max)
    }

    /// How many buffers it has, and how long each is.
    pub fn buffers(&self) -> usize {
        self.bufs.len()
    }

    pub fn buffer_len(&self) -> usize {
        self.len
    }

    /// The bytes of buffer `buffer`, when it has no I/O in flight.
    pub fn bytes(&mut self, buffer: usize) -> Option<&mut [u8]> {
        if *self.busy.get(buffer)? != 0 {
            return None;
        }
        Some(&mut self.bufs[buffer].as_mut_slice()[..self.len])
    }

    /// Where buffer `buffer` is in the kernel's address space, for whoever
    /// fills and empties it between its I/O (`kernel_image_queue`): an
    /// address, which is nothing to have -- to use it is the holder's
    /// promise to keep off the buffer while it has I/O in flight.
    pub fn address(&mut self, buffer: usize) -> Option<*mut u8> {
        self.bufs.get_mut(buffer).map(DmaBuffer::as_mut_ptr)
    }

    /// `len` bytes of buffer `buffer` from `at` -- whole sectors, `at` a
    /// whole number of them in -- read from (or with `write`, written to)
    /// the disk from `sector`, handed to the device with its doorbell left
    /// for `kick`. Done when `reap` says so.
    pub fn start(&mut self, buffer: usize, at: usize, len: usize, sector: u64, write: bool)
        -> Result<(), SubmitError>
    {
        let disk = self.disk.ok_or(SubmitError::Unsupported)?;
        if self.max == 0 {
            return Err(SubmitError::Unsupported);
        }
        let ss = disk.sector_size() as usize;
        let Some(buf) = self.bufs.get(buffer) else { return Err(SubmitError::Invalid) };
        if len == 0 || len > self.max || len % ss != 0 || at % ss != 0
            || at.checked_add(len).is_none_or(|end| end > self.len)
        {
            return Err(SubmitError::Invalid);
        }
        if self.in_flight >= IN_FLIGHT_MAX {
            return Err(SubmitError::Busy);
        }
        let count = u32::try_from(len / ss).map_err(|_| SubmitError::Invalid)?;
        let io = BlockIo {
            op: if write { IO_WRITE } else { IO_READ },
            fua: 0,
            reserved: 0,
            count,
            sector,
            phys: buf.phys() + at as u64,
            done: on_done,
            ctx: context(self.entry, buffer),
        };
        /* Recorded before the device can have it -- its doorbell may be
         * someone else's to ring -- so that its completion comes after; a
         * refusal is the completion, said so. */
        if trace::on::<BioSubmit>() {
            trace::emit(&BioSubmit {
                sector,
                count,
                pid: trace::pid(),
                entry: u16::try_from(self.entry).unwrap_or(u16::MAX),
                buffer: u16::try_from(buffer).unwrap_or(u16::MAX),
                in_flight: u16::try_from(self.in_flight + 1).unwrap_or(u16::MAX),
                write: u8::from(write),
                _pad: 0,
            });
        }
        /* The range is inside a buffer the queue owns, contiguous and on a
         * page; the queue frees no buffer while it has I/O on it, and goes
         * only once all of it is done (`Drop`); nothing here hands out a
         * buffer with I/O on it (`bytes`). The context is a number. */
        if let Err(e) = unsafe { disk.submit(&io, false) } {
            bio_done(self.entry, buffer, false, true);
            return Err(e);
        }
        self.busy[buffer] += 1;
        self.in_flight += 1;
        Ok(())
    }

    /// A flush of the disk's write cache, as buffer `buffer`'s I/O: what
    /// the device had done when it began made durable, its completion
    /// reaped as a read's or a write's is. The buffer's bytes are not the
    /// device's to touch; the buffer is how its owner tells this one from
    /// the others. Unsupported where the disk has no asynchronous flush --
    /// no write cache, or no asynchronous path -- for the owner to flush by
    /// the synchronous path.
    pub fn start_flush(&mut self, buffer: usize) -> Result<(), SubmitError> {
        let disk = self.disk.ok_or(SubmitError::Unsupported)?;
        if self.max == 0 {
            return Err(SubmitError::Unsupported);
        }
        if buffer >= self.bufs.len() {
            return Err(SubmitError::Invalid);
        }
        if self.in_flight >= IN_FLIGHT_MAX {
            return Err(SubmitError::Busy);
        }
        let io = BlockIo {
            op: IO_FLUSH,
            fua: 0,
            reserved: 0,
            count: 0,
            sector: 0,
            phys: 0,
            done: on_done,
            ctx: context(self.entry, buffer),
        };
        /* A flush names no memory: nothing of the buffer's is handed over.
         * The context is a number. */
        if let Err(e) = unsafe { disk.submit(&io, false) } {
            return Err(e);
        }
        self.busy[buffer] += 1;
        self.in_flight += 1;
        Ok(())
    }

    /// `pieces` of buffer `buffer` read from the disk -- or with `write`
    /// written to it -- by the synchronous path, and returned once all of
    /// them are done (`Disk::read_pieces`): whether every one went well.
    /// Refused while the buffer has I/O in flight, and for a queue that has
    /// no disk.
    pub fn now(&mut self, buffer: usize, pieces: &[crate::Piece], write: bool) -> bool {
        let Some(disk) = self.disk else { return false };
        if self.busy.get(buffer).is_none_or(|&n| n != 0) {
            return false;
        }
        let buf = &mut self.bufs[buffer];
        if write { disk.write_pieces(buf, pieces).is_ok() } else { disk.read_pieces(buf, pieces).is_ok() }
    }

    /// The doorbell for what `start` handed the device.
    pub fn kick(&self) {
        if let Some(disk) = self.disk.filter(|_| self.max != 0) {
            disk.kick();
        }
    }

    /// How much I/O it has with the device.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// An I/O the device is done with, if one is: its buffer, and whether it
    /// went well. A completion for a buffer with nothing in flight is a
    /// device's -- or this code's -- mistake, said and left out.
    pub fn reap(&mut self) -> Option<(usize, bool)> {
        let done = DONE[self.entry].get()?;
        loop {
            let word = done.ring.pop()?;
            let (buffer, ok) = (word >> 1, word & 1 == 0);
            match self.busy.get_mut(buffer) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    self.in_flight -= 1;
                    return Some((buffer, ok));
                }
                _ => klog!(0, "block: queue {} has a completion for buffer {}, which has no I/O in flight",
                            self.entry, buffer),
            }
        }
    }

    /// Sleeps until a completion comes, or a `wake`, or `timeout`: whether
    /// one did. The queue's own event, whatever task's it was made on.
    pub fn wait(&self, timeout: Duration) -> bool {
        match DONE[self.entry].get() {
            Some(done) => done.event.wait_for(timeout),
            None => false,
        }
    }

    /// The event of the task it was made on let go of, from here on not
    /// signalled: taken out under its lock, so that no completion is in its
    /// signal once this has it, and dropped after -- off the lock, in task
    /// context, where the last count of it may free it.
    fn let_go_of_task_event(&self) {
        let Some(done) = DONE[self.entry].get() else { return };
        /* Never set, nothing to let go of: the flag goes up only once the
         * event is in (`new`), and a queue on none takes no lock here */
        if !done.on_task_event.swap(false, Ordering::AcqRel) {
            return;
        }
        let taken = done.task_event.lock().take();
        drop(taken);
    }

    /// A command the boot's test of a queue on a task's event finishes as a
    /// device would (kernel/src/one_event.rs): buffer `buffer` taken as a
    /// started command's is, in flight until `reap` gives it back. None for
    /// a buffer past the queue, or one with I/O in flight.
    pub fn test_command(&mut self, buffer: usize) -> Option<TestCommand> {
        if *self.busy.get(buffer)? != 0 || self.in_flight >= IN_FLIGHT_MAX {
            return None;
        }
        self.busy[buffer] += 1;
        self.in_flight += 1;
        Some(TestCommand { context: context(self.entry, buffer) as usize })
    }
}

/// A command of `Queue::test_command`'s, which `finish` ends as its
/// device's interrupt would end one: through the queue's own completion.
pub struct TestCommand {
    context: usize,
}

impl TestCommand {
    /// From any context, an interrupt's included -- once: a second is a
    /// completion of a buffer with nothing in flight, which `reap` says.
    pub fn finish(&self, ok: bool) {
        on_done(self.context as *mut u8, if ok { 0 } else { -1 });
    }
}

impl Drop for Queue {
    /// Waits for every I/O it handed the device -- what its buffers are
    /// kept for -- saying so when that is long; past `DRAIN_GIVE_UP_NS` the
    /// buffers and the entry are given up on for good instead.
    fn drop(&mut self) {
        let began = boot_time_ns();
        let mut said = false;
        while self.in_flight > 0 {
            while self.reap().is_some() {}
            if self.in_flight == 0 {
                break;
            }
            let waited = boot_time_ns().saturating_sub(began);
            if waited >= DRAIN_GIVE_UP_NS {
                klog!(0, "block: queue {} gives up on {} I/O its disk never finished: its buffers are kept for \
                           good", self.entry, self.in_flight);
                /* Kept: the device may write them yet, and the entry may be
                 * left a completion -- which signals nobody's event but the
                 * entry's own from here on. */
                for buf in self.bufs.drain(..) {
                    core::mem::forget(buf);
                }
                self.let_go_of_task_event();
                return;
            }
            if waited >= DRAIN_SAY_NS && !said {
                said = true;
                klog!(0, "block: queue {} has waited {} s for {} I/O its disk has not finished", self.entry,
                       waited / 1_000_000_000, self.in_flight);
            }
            self.wait(Duration::from_millis(100));
        }
        /* Whatever was signalled for it read, for the next owner. */
        if let Some(done) = DONE[self.entry].get() {
            while done.ring.pop().is_some() {}
        }
        /* Before the entry is given back: the next owner's event is its own */
        self.let_go_of_task_event();
        TAKEN[self.entry].store(false, Ordering::Release);
    }
}

/// A free entry of the table, made where it was not yet.
fn take_entry() -> Option<usize> {
    for (i, taken) in TAKEN.iter().enumerate() {
        if taken.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            continue;
        }
        let made = DONE[i].get_or_try_init(|| {
            Some(alloc::boxed::Box::new(Done {
                ring: LocklessRing::new(IN_FLIGHT_MAX)?,
                event: Event::new()?,
                task_event: IrqSpinLock::new(None),
                on_task_event: AtomicBool::new(false),
            }))
        });
        if made.is_none() {
            TAKEN[i].store(false, Ordering::Release);
            klog!(0, "block: no memory for a queue's completions");
            return None;
        }
        return Some(i);
    }
    klog!(0, "block: all {} queues are taken", QUEUES);
    None
}
