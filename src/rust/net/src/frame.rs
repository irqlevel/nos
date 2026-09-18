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

use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

use kcore::consts::MAX_CPUS;
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

pub const DIRECTION_TX: u8 = 0;
pub const DIRECTION_RX: u8 = 1;

/// The kernel's intrusive list link (Stdlib::ListEntry).
#[repr(C)]
pub struct ListEntry {
    pub flink: *mut ListEntry,
    pub blink: *mut ListEntry,
}

/// A frame: a buffer with its physical address, a length, and a reference
/// count that says when it goes back where it came from.
///
/// Nothing outside this crate sees one any more -- a driver holds a handle
/// and reaches the bytes through `kcore::net` -- so the layout is this
/// crate's own.
#[repr(C)]
pub struct NetFrame {
    pub link: ListEntry,
    pub data: *mut u8,
    pub data_phys: usize,
    /// Bytes in the frame now
    pub len: usize,
    /// Bytes it has room for, which is what its release frees
    pub capacity: usize,
    pub refcount: AtomicIsize,
    pub direction: u8,
    pub release: Option<extern "C" fn(*mut NetFrame, *mut u8)>,
    pub release_ctx: *mut u8,
}

impl NetFrame {
    /// # Safety
    /// `frame` is a live frame.
    unsafe fn link_init(frame: *mut NetFrame) {
        unsafe {
            let link = core::ptr::addr_of_mut!((*frame).link);
            (*link).flink = link;
            (*link).blink = link;
        }
    }
}

/// One CPU's cache. Own cache lines: two CPUs must never share one, and the
/// alignment has to be on the type -- padding the size alone leaves the whole
/// array free to start mid-line.
#[repr(align(64))]
struct PerCpuCache {
    frame: [*mut NetFrame; CACHE_SIZE],
    count: usize,
    /// Counted here rather than in a shared atomic: one increment per packet
    /// on a line every CPU writes would put back exactly the contention this
    /// pool exists to remove. Summed only by the shell.
    hits: usize,
}

const EMPTY_CACHE: PerCpuCache = PerCpuCache {
    frame: [core::ptr::null_mut(); CACHE_SIZE],
    count: 0,
    hits: 0,
};

pub struct FramePool {
    ready: core::sync::atomic::AtomicBool,
    frame_count: AtomicUsize,
    ring: core::cell::UnsafeCell<Option<LocklessRing>>,
    cache: core::cell::UnsafeCell<[PerCpuCache; MAX_CPUS]>,

    alloc_misses: AtomicUsize,
    oversized: AtomicUsize,
    ring_refills: AtomicUsize,
    ring_flushes: AtomicUsize,
}

/* Each cache belongs to one CPU and is touched with interrupts off; the ring
 * is built to be used from every CPU at once. */
unsafe impl Sync for FramePool {}
unsafe impl Send for FramePool {}

pub static POOL: FramePool = FramePool {
    ready: core::sync::atomic::AtomicBool::new(false),
    frame_count: AtomicUsize::new(0),
    ring: core::cell::UnsafeCell::new(None),
    cache: core::cell::UnsafeCell::new([EMPTY_CACHE; MAX_CPUS]),
    alloc_misses: AtomicUsize::new(0),
    oversized: AtomicUsize::new(0),
    ring_refills: AtomicUsize::new(0),
    ring_flushes: AtomicUsize::new(0),
};

/// What a pooled frame's release does: back to the cache it came from.
extern "C" fn release_to_pool(frame: *mut NetFrame, _ctx: *mut u8) {
    POOL.release(frame);
}

impl FramePool {
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn ring(&self) -> Option<&LocklessRing> {
        unsafe { (*self.ring.get()).as_ref() }
    }

    /// Build `count` frames, once, at boot; 0 asks for the default, which is
    /// what boot passes unless `netframes=N` was given.
    pub fn setup(&'static self, count: usize) -> bool {
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
            let frame = match build_frame() {
                Some(frame) => frame,
                None => break,
            };
            if !ring.push(frame as usize) {
                free_frame(frame);
                break;
            }
            built += 1;
        }

        if built == 0 {
            trace!(0, "netpool: not one frame could be built");
            return false;
        }

        unsafe { *self.ring.get() = Some(ring) };
        self.frame_count.store(built, Ordering::Release);
        self.ready.store(true, Ordering::Release);

        trace!(0, "netpool: {} frames of {} bytes, ring capacity {}, {} KiB",
            built, FRAME_CAPACITY, capacity,
            built * (core::mem::size_of::<NetFrame>() + FRAME_CAPACITY) / 1024);
        true
    }

    /// A frame with room for `len` bytes, or null when the request is too
    /// large for a pooled one or the pool is empty -- the caller then falls
    /// back to the allocator.
    pub fn alloc(&'static self, len: usize) -> *mut NetFrame {
        if !self.is_ready() {
            return core::ptr::null_mut();
        }
        if len > FRAME_CAPACITY {
            self.oversized.fetch_add(1, Ordering::Relaxed);
            return core::ptr::null_mut();
        }

        let mut frame = core::ptr::null_mut();
        {
            /* Interrupts off rather than a lock: the cache belongs to this
             * CPU and nothing else touches it, so there is nothing to
             * contend for and no atomic to pay for.
             *
             * The order matters. Reading the CPU id first and disabling
             * after leaves a window in which this task is preempted onto
             * another CPU, and then two CPUs are inside one per-CPU cache --
             * which is not a per-CPU cache at all. */
            let flags = kcore::cpu::irq_save();
            let index = kcore::cpu::id() as usize;
            if index >= MAX_CPUS {
                unsafe { kcore::cpu::irq_restore(flags) };
                return core::ptr::null_mut();
            }

            let cache = unsafe { &mut (*self.cache.get())[index] };

            if cache.count == 0 {
                if let Some(ring) = self.ring() {
                    for _ in 0..BATCH {
                        match ring.pop() {
                            Some(value) => {
                                cache.frame[cache.count] = value as *mut NetFrame;
                                cache.count += 1;
                            }
                            None => break,
                        }
                    }
                }
                if cache.count != 0 {
                    self.ring_refills.fetch_add(1, Ordering::Relaxed);
                }
            }

            if cache.count != 0 {
                cache.count -= 1;
                frame = cache.frame[cache.count];
                cache.hits += 1;
            }

            unsafe { kcore::cpu::irq_restore(flags) };
        }

        if frame.is_null() {
            self.alloc_misses.fetch_add(1, Ordering::Relaxed);
            return core::ptr::null_mut();
        }

        unsafe {
            NetFrame::link_init(frame);
            (*frame).len = 0;
            (*frame).refcount.store(1, Ordering::Release);
        }
        frame
    }

    /// A frame whose last reference is gone, back to this CPU's cache.
    fn release(&'static self, frame: *mut NetFrame) {
        /* A release only happens after the count reached zero, so anything
         * else here means the frame was released twice -- and a frame in the
         * cache twice is handed to two owners. Catch it where it happens
         * rather than where it corrupts. */
        debug_assert!(unsafe { (*frame).refcount.load(Ordering::Acquire) } == 0);

        /* Interrupts off before the CPU id, for the reason in `alloc` */
        let flags = kcore::cpu::irq_save();
        let index = kcore::cpu::id() as usize;

        if index >= MAX_CPUS {
            /* No cache to put it in; the ring always has room for every
             * frame. */
            unsafe { kcore::cpu::irq_restore(flags) };
            if let Some(ring) = self.ring() {
                ring.push(frame as usize);
            }
            return;
        }

        let cache = unsafe { &mut (*self.cache.get())[index] };

        if cache.count == CACHE_SIZE {
            if let Some(ring) = self.ring() {
                for _ in 0..BATCH {
                    cache.count -= 1;
                    if !ring.push(cache.frame[cache.count] as usize) {
                        /* Sized so this cannot happen; put it back rather
                         * than lose the frame if it somehow does. */
                        cache.count += 1;
                        break;
                    }
                }
            }
            self.ring_flushes.fetch_add(1, Ordering::Relaxed);
        }

        if cache.count < CACHE_SIZE {
            cache.frame[cache.count] = frame;
            cache.count += 1;
        } else if let Some(ring) = self.ring() {
            ring.push(frame as usize);
        }

        unsafe { kcore::cpu::irq_restore(flags) };
    }

    /* ---- what the shell reports ---- */

    fn cached(&'static self) -> (usize, usize) {
        let cache = unsafe { &*self.cache.get() };
        let mut count = 0;
        let mut hits = 0;
        for slot in cache.iter() {
            count += slot.count;
            hits += slot.hits;
        }
        (count, hits)
    }

    pub fn alloc_misses(&'static self) -> usize {
        self.alloc_misses.load(Ordering::Relaxed)
    }

    /// Frames a driver is holding: what the pool built, less what it can
    /// account for.
    pub fn in_flight(&'static self) -> usize {
        if !self.is_ready() {
            return 0;
        }
        let (cached, _) = self.cached();
        let held = self.ring().map_or(0, |ring| ring.len()) + cached;
        self.frame_count.load(Ordering::Acquire).saturating_sub(held)
    }

    pub fn stats(&'static self) -> Stats {
        let (cached, hits) = self.cached();
        /* The ring's count is a snapshot of two independently moving
         * positions, so what it accounts for can read a little high; clamp
         * rather than report a huge number that is really a negative one. */
        let ring = self.ring().map_or(0, |r| r.len());
        let total = self.frame_count.load(Ordering::Acquire);
        let held = ring + cached;

        Stats {
            ready: self.is_ready() as u32,
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

/// What `netpool` prints. The C++ side declares the same struct.
#[repr(C)]
pub struct Stats {
    pub ready: u32,
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

/// One frame, built once and never handed back to the allocator: its
/// physical address is resolved here and never again.
fn build_frame() -> Option<*mut NetFrame> {
    let size = core::mem::size_of::<NetFrame>() + FRAME_CAPACITY;
    let layout = unsafe { core::alloc::Layout::from_size_align_unchecked(size, 8) };

    let raw = unsafe { alloc::alloc::alloc_zeroed(layout) };
    if raw.is_null() {
        return None;
    }
    let frame = raw as *mut NetFrame;

    unsafe {
        NetFrame::link_init(frame);
        let data = raw.add(core::mem::size_of::<NetFrame>());
        (*frame).data = data;
        (*frame).len = 0;
        (*frame).capacity = FRAME_CAPACITY;
        (*frame).refcount.store(0, Ordering::Release);
        (*frame).direction = DIRECTION_TX;
        (*frame).release = Some(release_to_pool);
        (*frame).release_ctx = core::ptr::null_mut();

        let phys = kcore::dma::virt_to_phys(data) as usize;
        if phys == 0 {
            alloc::alloc::dealloc(raw, layout);
            return None;
        }
        (*frame).data_phys = phys;
    }

    Some(frame)
}

/// Only for a frame the pool could not take: nothing gives one back once it
/// is in.
fn free_frame(frame: *mut NetFrame) {
    let size = core::mem::size_of::<NetFrame>() + FRAME_CAPACITY;
    let layout = unsafe { core::alloc::Layout::from_size_align_unchecked(size, 8) };
    unsafe { alloc::alloc::dealloc(frame as *mut u8, layout) };
}

/* ---- what the kernel calls ---- */

/// Build the pool: 0 built, -1 not.
#[no_mangle]
pub extern "C" fn rust_netframe_pool_setup(count: usize) -> i32 {
    if POOL.setup(count) { 0 } else { -1 }
}

/// A frame with room for `len` bytes, or null -- the caller then falls back
/// to the allocator.
#[no_mangle]
pub extern "C" fn rust_netframe_pool_alloc(len: usize) -> *mut NetFrame {
    POOL.alloc(len)
}

/// What `netpool` prints.
///
/// # Safety
/// `out` points at a Stats.
#[no_mangle]
pub unsafe extern "C" fn rust_netframe_pool_stats(out: *mut Stats) {
    if out.is_null() {
        return;
    }
    unsafe { *out = POOL.stats() };
}

#[no_mangle]
pub extern "C" fn rust_netframe_pool_misses() -> usize {
    POOL.alloc_misses()
}

#[no_mangle]
pub extern "C" fn rust_netframe_pool_in_flight() -> usize {
    POOL.in_flight()
}

/* ---- a frame's references ---- */

/// One more reference: what a listener takes to keep a frame past its call.
///
/// # Safety
/// `frame` is a live frame.
pub unsafe fn get(frame: *mut NetFrame) {
    unsafe { (*frame).refcount.fetch_add(1, Ordering::AcqRel) };
}

/// One fewer. The last one releases the frame -- to the pool it came from,
/// or to the allocator for one the pool could not serve.
///
/// # Safety
/// `frame` is a live frame and this caller's reference is not used again.
pub unsafe fn put(frame: *mut NetFrame) {
    if unsafe { (*frame).refcount.fetch_sub(1, Ordering::AcqRel) } != 1 {
        return;
    }

    let release = unsafe { (*frame).release };
    if let Some(release) = release {
        release(frame, unsafe { (*frame).release_ctx });
    }
}

/// What a frame from outside the pool is released by: straight back to the
/// allocator, TLB shootdown and all. Which is why the pool exists.
extern "C" fn release_to_allocator(frame: *mut NetFrame, _ctx: *mut u8) {
    let size = core::mem::size_of::<NetFrame>() + unsafe { (*frame).capacity };
    let layout = unsafe { core::alloc::Layout::from_size_align_unchecked(size, 8) };
    unsafe { alloc::alloc::dealloc(frame as *mut u8, layout) };
}

/// A frame to transmit, with room for `len` bytes.
///
/// From the pool whenever it fits one -- a per-CPU cache, no allocator at
/// all. What the pool cannot serve falls through to the allocator, which is
/// what this used to be for every frame.
pub fn alloc_tx(len: usize) -> *mut NetFrame {
    let pooled = POOL.alloc(len);
    if !pooled.is_null() {
        return pooled;
    }

    let size = core::mem::size_of::<NetFrame>() + len;
    let layout = unsafe { core::alloc::Layout::from_size_align_unchecked(size, 8) };
    let raw = unsafe { alloc::alloc::alloc_zeroed(layout) };
    if raw.is_null() {
        return core::ptr::null_mut();
    }

    let frame = raw as *mut NetFrame;
    unsafe {
        NetFrame::link_init(frame);
        let data = raw.add(core::mem::size_of::<NetFrame>());
        (*frame).data = data;
        (*frame).len = 0;
        (*frame).capacity = len;
        (*frame).refcount.store(1, Ordering::Release);
        (*frame).direction = DIRECTION_TX;
        (*frame).release = Some(release_to_allocator);
        (*frame).release_ctx = core::ptr::null_mut();

        let phys = kcore::dma::virt_to_phys(data) as usize;
        if phys == 0 {
            alloc::alloc::dealloc(raw, layout);
            return core::ptr::null_mut();
        }
        (*frame).data_phys = phys;
    }
    frame
}

/// A frame to receive into, with room for `len` bytes.
pub fn alloc_rx(len: usize) -> *mut NetFrame {
    let frame = alloc_tx(len);
    if !frame.is_null() {
        unsafe { (*frame).direction = DIRECTION_RX };
    }
    frame
}

/// Allocations the pool could not serve, and frames a driver is holding.
///
/// # Safety
/// Both are writable, or null.
#[no_mangle]
pub unsafe extern "C" fn kernel_netframe_pool_stats(
    misses: *mut usize, in_flight: *mut usize,
) {
    unsafe {
        if !misses.is_null() {
            *misses = POOL.alloc_misses();
        }
        if !in_flight.is_null() {
            *in_flight = POOL.in_flight();
        }
    }
}
