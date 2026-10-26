//! The heap: a bump allocator over a few MiB of the BSS. The loader runs
//! once, briefly, on one CPU; what it allocates -- a configuration file, its
//! parse, a few paths -- is small, and is never given back but for the last
//! allocation, which is enough for a vector that grows. Out of it, an
//! allocation fails, and `alloc` makes that a panic, which says so and
//! turns the guest off.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

const HEAP_BYTES: usize = 4 << 20;

#[repr(C, align(4096))]
struct Space(UnsafeCell<[u8; HEAP_BYTES]>);

// SAFETY: the loader is one thread on one CPU with interrupts off; the bump
// pointer below hands each byte out once.
unsafe impl Sync for Space {}

static SPACE: Space = Space(UnsafeCell::new([0; HEAP_BYTES]));

/// The offset of the first free byte, and of the last allocation's start.
struct Bump {
    next: AtomicUsize,
    last: AtomicUsize,
}

// SAFETY: every block is `size` bytes of `SPACE` at the alignment asked for,
// handed out once: `next` only grows past it, but for a dealloc or realloc of
// the last block, which no one else holds.
unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = SPACE.0.get() as usize;
        let next = self.next.load(Ordering::Relaxed);
        let Some(start) = (base + next).checked_add(layout.align() - 1).map(|v| v & !(layout.align() - 1)) else {
            return core::ptr::null_mut();
        };
        let offset = start - base;
        let Some(end) = offset.checked_add(layout.size()) else {
            return core::ptr::null_mut();
        };
        if end > HEAP_BYTES {
            return core::ptr::null_mut();
        }
        self.last.store(offset, Ordering::Relaxed);
        self.next.store(end, Ordering::Relaxed);
        start as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let offset = ptr as usize - SPACE.0.get() as usize;
        if offset == self.last.load(Ordering::Relaxed) {
            self.next.store(offset, Ordering::Relaxed);
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let offset = ptr as usize - SPACE.0.get() as usize;
        if offset == self.last.load(Ordering::Relaxed) {
            /* The last block grows, or shrinks, in place. */
            if let Some(end) = offset.checked_add(new_size).filter(|&e| e <= HEAP_BYTES) {
                self.next.store(end, Ordering::Relaxed);
                return ptr;
            }
            return core::ptr::null_mut();
        }
        // SAFETY: as `GlobalAlloc::realloc`'s own default: a new block,
        // the old one's bytes copied into it.
        let new = unsafe { self.alloc(Layout::from_size_align_unchecked(new_size, layout.align())) };
        if !new.is_null() {
            unsafe { core::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size)) };
        }
        new
    }
}

#[global_allocator]
static HEAP: Bump = Bump { next: AtomicUsize::new(0), last: AtomicUsize::new(usize::MAX) };
