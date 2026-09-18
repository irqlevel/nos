//! Network frames, and the pool they are recycled through.
//!
//! Without the pool every frame costs three things that have nothing to do
//! with networking: an allocation, a virtual-to-physical walk (which
//! temporarily maps a page per level and invalidates the TLB for each), and
//! on release a free whose page allocator shoots down the TLB on every other
//! CPU and waits for all of them to answer. The last is not merely slow --
//! it is what deadlocked this kernel when a driver freed a frame under its
//! transmit lock. A datapath that never calls the allocator cannot have that
//! bug.
//!
//! So frames are built once at boot: a fixed size, a physical address
//! resolved then and never again. A per-CPU cache serves the common case
//! with interrupts off and no atomics, so a frame released on a CPU is handed
//! back out on the same CPU while it is still in that core's cache. Behind
//! the caches is one lockless ring, touched only in batches when a cache runs
//! dry or fills.

use core::alloc::Layout;
use core::cell::Cell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

use kcore::once::Once;
use kcore::percpu::{ConstInit, CpuLocal, LocalCounter, PerCpu};
use kcore::ring::LocklessRing;
use kcore::trace;

/// Data bytes per frame. Two kilobytes covers a 1500-byte MTU with room for
/// the headers a driver prepends, and keeps the whole frame inside one page.
pub const FRAME_CAPACITY: usize = 2048;

/// Headroom, not a measured need. On virtio at fourteen thousand packets a
/// second the pool never once ran dry -- two frames in flight out of a
/// thousand, because transmits complete as fast as they are posted and the
/// frame comes straight back. What that does not say is how many a different
/// driver holds, and the cost of being wrong upward is 8 MiB on a machine
/// with sixty-four gigabytes, while the cost of being wrong downward is drops
/// on a datapath. `netframes=N` overrides it, which is the part that helps:
/// the number can be raised on the machine in question and checked against
/// `netpool` without a rebuild.
pub const DEFAULT_FRAME_COUNT: usize = 4096;

/// How many frames a cache holds, and how many move between a cache and the
/// ring at once. The batching is the whole point: the ring is touched once
/// per batch instead of once per packet.
const CACHE_SIZE: usize = 64;
const BATCH: usize = 32;

/* ---- a frame ---- */

/// The frame itself: a few words, and its buffer straight after them in the
/// same allocation. Nothing outside this file sees one -- the layer holds a
/// `Frame`, a driver holds a handle and reaches the bytes through
/// `kcore::net` -- so every way to one is below.
struct RawFrame {
    /// Where the queue this frame is on goes next. The queue's alone: a
    /// frame is on one queue at a time, and only what holds that queue looks
    /// here.
    next: Cell<*mut RawFrame>,
    data: NonNull<u8>,
    data_phys: usize,
    /// Bytes it has room for, which is what its release frees
    capacity: usize,
    /// Bytes in the frame now
    len: AtomicUsize,
    /// When this reaches zero the frame goes back where it came from
    refcount: AtomicIsize,
    /// Back to the pool, that is, rather than to the allocator
    pooled: bool,
}

impl RawFrame {
    fn layout(capacity: usize) -> Option<Layout> {
        let size = core::mem::size_of::<RawFrame>().checked_add(capacity)?;
        Layout::from_size_align(size, core::mem::align_of::<RawFrame>()).ok()
    }

    /// One frame from the allocator, its physical address resolved here and
    /// never again.
    fn build(capacity: usize, pooled: bool) -> Option<IdleFrame> {
        let layout = Self::layout(capacity)?;

        /* Never zero-sized: the header alone is not. */
        let raw = unsafe { alloc::alloc::alloc_zeroed(layout) };
        let frame = NonNull::new(raw as *mut RawFrame)?;
        /* One allocation: the buffer starts where the header ends. */
        let data = unsafe { NonNull::new_unchecked(raw.add(core::mem::size_of::<RawFrame>())) };

        let data_phys = kcore::dma::virt_to_phys(data.as_ptr()) as usize;
        if data_phys == 0 {
            unsafe { alloc::alloc::dealloc(raw, layout) };
            return None;
        }

        /* Fresh memory of the header's size and alignment. */
        unsafe {
            frame.as_ptr().write(RawFrame {
                next: Cell::new(core::ptr::null_mut()),
                data,
                data_phys,
                capacity,
                len: AtomicUsize::new(0),
                refcount: AtomicIsize::new(0),
                pooled,
            });
        }
        Some(IdleFrame(frame))
    }

    fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// Never past the buffer, whoever asks: `bytes` trusts this.
    fn set_len(&self, len: usize) {
        self.len.store(len.min(self.capacity), Ordering::Relaxed);
    }

    fn bytes(&self) -> &[u8] {
        /* The buffer is `capacity` bytes for as long as the frame is, and
         * `len` is never more. */
        unsafe { core::slice::from_raw_parts(self.data.as_ptr(), self.len()) }
    }
}

/// A frame nobody holds: what the pool keeps, and what the allocator has
/// just built.
struct IdleFrame(NonNull<RawFrame>);

/* A pointer, but to something that is nobody else's. */
unsafe impl Send for IdleFrame {}

impl IdleFrame {
    fn header(&self) -> &RawFrame {
        /* Alive until `free`, which takes `self`. */
        unsafe { self.0.as_ref() }
    }

    /// Into somebody's hands, with the one reference that makes it theirs.
    fn activate(self) -> Frame {
        let header = self.header();
        header.next.set(core::ptr::null_mut());
        header.len.store(0, Ordering::Relaxed);
        header.refcount.store(1, Ordering::Release);
        Frame(self.0)
    }

    /// As the word the lockless ring carries.
    fn word(&self) -> usize {
        self.0.as_ptr() as usize
    }

    /// # Safety
    /// `word` is one `word()` gave, of a frame handed to the ring with it and
    /// taken from the ring just now.
    unsafe fn from_word(word: usize) -> Option<IdleFrame> {
        NonNull::new(word as *mut RawFrame).map(IdleFrame)
    }

    /// Back to the allocator, TLB shootdown and all. Which is why the pool
    /// exists.
    fn free(self) {
        if let Some(layout) = RawFrame::layout(self.header().capacity) {
            /* Built by `build` with this same layout, and nobody holds it. */
            unsafe { alloc::alloc::dealloc(self.0.as_ptr() as *mut u8, layout) };
        }
    }
}

/// One reference to a frame. Dropping it gives the reference up, and the
/// last one given up sends the frame back where it came from -- to the pool,
/// or to the allocator for one the pool could not serve.
pub struct Frame(NonNull<RawFrame>);

/* A buffer and a few words about it. What is written through a handle is
 * atomic, or is the queue link -- which only the one queue a frame is on
 * touches -- or is the buffer of a frame so new that nobody else has it. */
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

impl Frame {
    fn header(&self) -> &RawFrame {
        /* A handle is a reference, and the frame outlives its references. */
        unsafe { self.0.as_ref() }
    }

    /// A frame to transmit, with room for `len` bytes.
    ///
    /// From the pool whenever it fits one -- a per-CPU cache, no allocator at
    /// all. What the pool cannot serve falls through to the allocator, which
    /// is what this used to be for every frame.
    pub fn alloc_tx(len: usize) -> Option<Frame> {
        POOL.alloc(len)
            .or_else(|| RawFrame::build(len, false).map(IdleFrame::activate))
    }

    /// A frame to receive into, with room for `len` bytes.
    pub fn alloc_rx(len: usize) -> Option<Frame> {
        Self::alloc_tx(len)
    }

    pub fn len(&self) -> usize {
        self.header().len()
    }

    pub fn bytes(&self) -> &[u8] {
        self.header().bytes()
    }

    /// The whole of `data` as the frame's contents. False when it has no
    /// room for that much.
    pub fn fill(&mut self, data: &[u8]) -> bool {
        let header = self.header();
        if data.len() > header.capacity {
            return false;
        }
        /* Within the buffer, by the check above. */
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), header.data.as_ptr(), data.len());
        }
        header.set_len(data.len());
        true
    }

    /// The word a listener is lent the frame by, for the length of a call:
    /// no reference goes with it -- `kernel_netframe_get` is how the listener
    /// takes one.
    pub fn as_lent(&self) -> usize {
        self.0.as_ptr() as usize
    }

    /// The reference as the word a driver or a listener knows it by. Not
    /// given up: `from_handle` takes it back.
    pub fn into_handle(self) -> usize {
        let handle = self.0.as_ptr() as usize;
        core::mem::forget(self);
        handle
    }

    /// # Safety
    /// `handle` is a reference `into_handle` gave out -- or 0 -- that its
    /// holder gives up here, of a frame that is on no queue.
    pub unsafe fn from_handle(handle: usize) -> Option<Frame> {
        NonNull::new(handle as *mut RawFrame).map(Frame)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        let header = self.header();
        if header.refcount.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }

        /* That was the last reference: nobody holds it now. */
        let pooled = header.pooled;
        let idle = IdleFrame(self.0);
        if pooled {
            POOL.release(idle);
        } else {
            idle.free();
        }
    }
}

/* ---- a queue of frames ---- */

/// Frames threaded through their own links, so that queueing one allocates
/// nothing. A device only ever adds at the end, takes from the front, or
/// moves the lot, so a queue with a tail pointer does everything a doubly
/// linked list would -- and can live in a `static`, which a self-referential
/// circular list cannot.
///
/// The queue holds the reference of every frame on it: `push` takes a
/// `Frame` and `pop` gives one back.
pub struct FrameQueue {
    head: Option<NonNull<RawFrame>>,
    tail: Option<NonNull<RawFrame>>,
    count: usize,
}

/* As a `Frame` is: the queue is the handles of what is on it. */
unsafe impl Send for FrameQueue {}

impl FrameQueue {
    pub const fn new() -> FrameQueue {
        FrameQueue { head: None, tail: None, count: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn push(&mut self, frame: Frame) {
        let raw = frame.0;
        /* The reference is the queue's from here. */
        core::mem::forget(frame);

        /* A frame in somebody's hands is on no queue, so its link is this
         * queue's to write -- and the tail's is, the tail being on this
         * one. */
        unsafe { raw.as_ref() }.next.set(core::ptr::null_mut());
        match self.tail {
            Some(tail) => unsafe { tail.as_ref() }.next.set(raw.as_ptr()),
            None => self.head = Some(raw),
        }
        self.tail = Some(raw);
        self.count += 1;
    }

    pub fn pop(&mut self) -> Option<Frame> {
        let raw = self.head?;

        /* On this queue, so alive and its link this queue's. */
        let next = unsafe { raw.as_ref() }.next.replace(core::ptr::null_mut());
        self.head = NonNull::new(next);
        if self.head.is_none() {
            self.tail = None;
        }
        self.count -= 1;

        /* The queue's reference, handed on. */
        Some(Frame(raw))
    }

    /// Everything, in one move, leaving this one empty.
    pub fn take(&mut self) -> FrameQueue {
        core::mem::replace(self, FrameQueue::new())
    }
}

impl Drop for FrameQueue {
    fn drop(&mut self) {
        while let Some(frame) = self.pop() {
            drop(frame);
        }
    }
}

/* ---- the pool ---- */

/// One CPU's cache: frames released here, waiting to be handed out here.
struct Cache {
    frames: [Option<IdleFrame>; CACHE_SIZE],
    count: usize,
}

impl ConstInit for Cache {
    const INIT: Self = Cache { frames: [const { None }; CACHE_SIZE], count: 0 };
}

/// What the shell may know about a cache it cannot look into: the cache is
/// its CPU's alone, so its CPU leaves the numbers out here.
///
/// Counted per CPU rather than in a shared atomic: one increment per packet
/// on a line every CPU writes would put back exactly the contention this
/// pool exists to remove.
#[repr(align(64))]
struct CacheSeen {
    held: LocalCounter,
    hits: LocalCounter,
}

impl ConstInit for CacheSeen {
    const INIT: Self = CacheSeen { held: LocalCounter::new(), hits: LocalCounter::new() };
}

pub struct FramePool {
    /// Behind the caches, and what says the pool is set up
    ring: Once<LocklessRing>,
    frame_count: AtomicUsize,
    cache: CpuLocal<Cache>,
    seen: PerCpu<CacheSeen>,

    alloc_misses: AtomicUsize,
    oversized: AtomicUsize,
    ring_refills: AtomicUsize,
    ring_flushes: AtomicUsize,
}

pub static POOL: FramePool = FramePool {
    ring: Once::new(),
    frame_count: AtomicUsize::new(0),
    cache: CpuLocal::new(),
    seen: PerCpu::new(),
    alloc_misses: AtomicUsize::new(0),
    oversized: AtomicUsize::new(0),
    ring_refills: AtomicUsize::new(0),
    ring_flushes: AtomicUsize::new(0),
};

impl FramePool {
    pub fn is_ready(&self) -> bool {
        self.ring.get().is_some()
    }

    /// Build `count` frames, once, at boot; 0 asks for the default, which is
    /// what boot passes unless `netframes=N` was given.
    pub fn setup(&self, count: usize) -> bool {
        if self.is_ready() {
            return false;
        }
        let count = if count == 0 { DEFAULT_FRAME_COUNT } else { count };

        /* The ring has to hold every frame at once -- a flush from a full
         * cache must never fail, or a frame would have to go back to the
         * allocator, which is the thing this exists to avoid. */
        let mut capacity = 1;
        while capacity < count {
            capacity *= 2;
        }

        let ring = match LocklessRing::new(capacity) {
            Some(ring) => ring,
            None => {
                trace!(0, "netpool: no ring of {} cells", capacity);
                return false;
            }
        };

        let mut built = 0;
        for _ in 0..count {
            let frame = match RawFrame::build(FRAME_CAPACITY, true) {
                Some(frame) => frame,
                None => break,
            };
            if !ring.push(frame.word()) {
                frame.free();
                break;
            }
            built += 1;
        }

        if built == 0 {
            trace!(0, "netpool: not one frame could be built");
            return false;
        }

        self.frame_count.store(built, Ordering::Release);
        if self.ring.set(ring).is_err() {
            /* Two setups at once, and the other one won: its frames are the
             * pool's. These stay in a ring nobody will ever pop. */
            return false;
        }

        trace!(0, "netpool: {} frames of {} bytes, ring capacity {}, {} KiB",
            built, FRAME_CAPACITY, capacity,
            built * (core::mem::size_of::<RawFrame>() + FRAME_CAPACITY) / 1024);
        true
    }

    /// A frame with room for `len` bytes, or None when the request is too
    /// large for a pooled one or the pool is empty -- the caller then falls
    /// back to the allocator.
    fn alloc(&self, len: usize) -> Option<Frame> {
        let ring = self.ring.get()?;
        if len > FRAME_CAPACITY {
            self.oversized.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        /* This CPU's cache, with interrupts off rather than a lock: nothing
         * else touches it, so there is nothing to contend for and no atomic
         * to pay for. */
        let idle = self.cache.with(|cache, cpu| {
            if cache.count == 0 {
                for _ in 0..BATCH {
                    /* What the ring carries is what `setup` and `release`
                     * put in it. */
                    match ring.pop().and_then(|word| unsafe { IdleFrame::from_word(word) }) {
                        Some(idle) => {
                            cache.frames[cache.count] = Some(idle);
                            cache.count += 1;
                        }
                        None => break,
                    }
                }
                if cache.count != 0 {
                    self.ring_refills.fetch_add(1, Ordering::Relaxed);
                }
            }

            let idle = match cache.count {
                0 => None,
                count => {
                    cache.count = count - 1;
                    cache.frames[count - 1].take()
                }
            };

            if let Some(seen) = self.seen.get(cpu) {
                seen.held.set(cache.count);
                if idle.is_some() {
                    seen.hits.add(1);
                }
            }
            idle
        }).flatten();

        match idle {
            Some(idle) => Some(idle.activate()),
            None => {
                self.alloc_misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// A frame whose last reference is gone, back to this CPU's cache.
    fn release(&self, idle: IdleFrame) {
        /* A release only happens after the count reached zero, so anything
         * else here means the frame was released twice -- and a frame in the
         * cache twice is handed to two owners. Catch it where it happens
         * rather than where it corrupts. */
        debug_assert!(idle.header().refcount.load(Ordering::Acquire) == 0);

        let ring = match self.ring.get() {
            Some(ring) => ring,
            /* A pooled frame with no pool: there is no such thing, and the
             * allocator is where it came from if there were. */
            None => return idle.free(),
        };

        let mut idle = Some(idle);
        self.cache.with(|cache, cpu| {
            if cache.count == CACHE_SIZE {
                for _ in 0..BATCH {
                    let top = cache.count - 1;
                    let word = match cache.frames[top].as_ref() {
                        Some(spilled) => spilled.word(),
                        None => break,
                    };
                    if !ring.push(word) {
                        /* Sized so this cannot happen; it stays in the cache
                         * rather than being lost if it somehow does. */
                        break;
                    }
                    /* The ring's now. */
                    cache.frames[top] = None;
                    cache.count = top;
                }
                self.ring_flushes.fetch_add(1, Ordering::Relaxed);
            }

            if cache.count < CACHE_SIZE {
                cache.frames[cache.count] = idle.take();
                cache.count += 1;
            }

            if let Some(seen) = self.seen.get(cpu) {
                seen.held.set(cache.count);
            }
        });

        /* No cache took it -- this CPU has none, or its own would not drain.
         * The ring always has room for every frame. */
        if let Some(idle) = idle {
            ring.push(idle.word());
        }
    }

    /* ---- what the shell reports ---- */

    fn cached(&self) -> (usize, usize) {
        let mut count = 0;
        let mut hits = 0;
        for seen in self.seen.iter() {
            count += seen.held.get();
            hits += seen.hits.get();
        }
        (count, hits)
    }

    pub fn alloc_misses(&self) -> usize {
        self.alloc_misses.load(Ordering::Relaxed)
    }

    /// Frames a driver is holding: what the pool built, less what it can
    /// account for.
    pub fn in_flight(&self) -> usize {
        let ring = match self.ring.get() {
            Some(ring) => ring,
            None => return 0,
        };
        let (cached, _) = self.cached();
        self.frame_count.load(Ordering::Acquire).saturating_sub(ring.len() + cached)
    }

    pub fn stats(&self) -> Stats {
        let (cached, hits) = self.cached();
        /* The ring's count is a snapshot of two independently moving
         * positions, so what it accounts for can read a little high; clamp
         * rather than report a huge number that is really a negative one. */
        let ring = self.ring.get().map_or(0, |r| r.len());
        let total = self.frame_count.load(Ordering::Acquire);
        let held = ring + cached;

        Stats {
            ready: self.is_ready(),
            frames: total,
            capacity: FRAME_CAPACITY,
            in_ring: ring,
            in_caches: cached,
            in_flight: total.saturating_sub(held),
            hits,
            misses: self.alloc_misses.load(Ordering::Relaxed),
            oversized: self.oversized.load(Ordering::Relaxed),
            refills: self.ring_refills.load(Ordering::Relaxed),
            flushes: self.ring_flushes.load(Ordering::Relaxed),
        }
    }
}

/// What `netpool` prints.
pub struct Stats {
    pub ready: bool,
    pub frames: usize,
    pub capacity: usize,
    pub in_ring: usize,
    pub in_caches: usize,
    pub in_flight: usize,
    pub hits: usize,
    pub misses: usize,
    pub oversized: usize,
    pub refills: usize,
    pub flushes: usize,
}

/* ---- what the kernel calls ---- */

/// Build the pool: 0 built, -1 not.
#[no_mangle]
pub extern "C" fn rust_netframe_pool_setup(count: usize) -> i32 {
    if POOL.setup(count) { 0 } else { -1 }
}

/* ---- what a driver, or a listener, calls ----
 *
 * A frame crosses to them as a word, and they work on it through these. A
 * handle is a reference: whoever was given one either hands it on or gives
 * it up with `kernel_netframe_put`. */

/// A frame somebody outside holds, looked at without taking it.
///
/// # Safety
/// `handle` is 0 or a reference its holder has not given up.
unsafe fn peek<'a>(handle: usize) -> Option<&'a RawFrame> {
    unsafe { (handle as *const RawFrame).as_ref() }
}

#[no_mangle]
pub extern "C" fn kernel_netframe_alloc_tx(len: usize) -> usize {
    Frame::alloc_tx(len).map_or(0, Frame::into_handle)
}

#[no_mangle]
pub extern "C" fn kernel_netframe_alloc_rx(len: usize) -> usize {
    Frame::alloc_rx(len).map_or(0, Frame::into_handle)
}

/// # Safety
/// `frame` is 0 or a reference the caller holds.
#[no_mangle]
pub unsafe extern "C" fn kernel_netframe_data(frame: usize) -> *mut u8 {
    unsafe { peek(frame) }.map_or(core::ptr::null_mut(), |frame| frame.data.as_ptr())
}

/// # Safety
/// `frame` is 0 or a reference the caller holds.
#[no_mangle]
pub unsafe extern "C" fn kernel_netframe_data_phys(frame: usize) -> u64 {
    unsafe { peek(frame) }.map_or(0, |frame| frame.data_phys as u64)
}

/// # Safety
/// `frame` is 0 or a reference the caller holds.
#[no_mangle]
pub unsafe extern "C" fn kernel_netframe_len(frame: usize) -> usize {
    unsafe { peek(frame) }.map_or(0, RawFrame::len)
}

/// # Safety
/// `frame` is 0 or a reference the caller holds.
#[no_mangle]
pub unsafe extern "C" fn kernel_netframe_capacity(frame: usize) -> usize {
    unsafe { peek(frame) }.map_or(0, |frame| frame.capacity)
}

/// # Safety
/// `frame` is 0 or a reference the caller holds.
#[no_mangle]
pub unsafe extern "C" fn kernel_netframe_set_len(frame: usize, len: usize) {
    if let Some(frame) = unsafe { peek(frame) } {
        frame.set_len(len);
    }
}

/// One more reference: what a listener takes to keep a frame past its call.
///
/// # Safety
/// `frame` is 0 or a frame alive for the call -- one the caller holds, or
/// was lent.
#[no_mangle]
pub unsafe extern "C" fn kernel_netframe_get(frame: usize) {
    if let Some(frame) = unsafe { peek(frame) } {
        frame.refcount.fetch_add(1, Ordering::AcqRel);
    }
}

/// One fewer. The last one releases the frame.
///
/// # Safety
/// `frame` is 0 or a reference the caller holds, and does not use again.
#[no_mangle]
pub unsafe extern "C" fn kernel_netframe_put(frame: usize) {
    drop(unsafe { Frame::from_handle(frame) });
}
