//! The fuzzer's allocator: the system's, but for large blocks, which are
//! kept when freed and handed out again for the same layout.
//!
//! A virtio disk takes its request buffers when it is made -- eight of
//! 256 KiB -- as the kernel wants it to: one allocation per guest, none on
//! the data path. The fuzzer makes a disk or two for every input, and handed
//! back to the system and taken again, each block was pages faulted in
//! afresh, zeroed, and advised away on the free: most of what a run of the
//! whole machine spent, and more of it the more processes ran at once, all
//! of them in the kernel's VM. Kept here, a block is memory already mapped.
//!
//! The one `unsafe` of the harness, and all it does is hand a block back
//! out: a block is kept with the layout it was allocated for and given out
//! again only for that same layout, so what a caller gets is exactly what
//! the system would have given it -- memory of that size and alignment,
//! contents undefined (`alloc_zeroed`'s default zeroes it, as it must).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Blocks this large and larger are kept: a disk's buffers, the NAT table,
/// a switch port's inbox.
const KEPT_FROM: usize = 64 * 1024;
/// How many are kept at most: four disks' buffers, and room besides.
const KEEP: usize = 48;

pub struct Recycling;

/// The kept blocks: an address (0 for none), its size and its alignment --
/// under `LOCK`, a spin lock, since a lock that allocates cannot be taken
/// inside the allocator.
static PTRS: [AtomicUsize; KEEP] = [const { AtomicUsize::new(0) }; KEEP];
static SIZES: [AtomicUsize; KEEP] = [const { AtomicUsize::new(0) }; KEEP];
static ALIGNS: [AtomicUsize; KEEP] = [const { AtomicUsize::new(0) }; KEEP];
static LOCK: AtomicBool = AtomicBool::new(false);

fn locked<R>(f: impl FnOnce() -> R) -> R {
    while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        std::hint::spin_loop();
    }
    let r = f();
    LOCK.store(false, Ordering::Release);
    r
}

/// A kept block of exactly `layout`, taken out of the store.
fn take(layout: Layout) -> Option<usize> {
    locked(|| {
        (0..KEEP).find_map(|i| {
            let p = PTRS[i].load(Ordering::Relaxed);
            (p != 0 && SIZES[i].load(Ordering::Relaxed) == layout.size()
                && ALIGNS[i].load(Ordering::Relaxed) == layout.align())
            .then(|| {
                PTRS[i].store(0, Ordering::Relaxed);
                p
            })
        })
    })
}

/// `ptr`, a block of `layout`, into the store: false when it is full.
fn keep(ptr: usize, layout: Layout) -> bool {
    locked(|| {
        let Some(i) = (0..KEEP).find(|&i| PTRS[i].load(Ordering::Relaxed) == 0) else { return false };
        SIZES[i].store(layout.size(), Ordering::Relaxed);
        ALIGNS[i].store(layout.align(), Ordering::Relaxed);
        PTRS[i].store(ptr, Ordering::Relaxed);
        true
    })
}

// SAFETY: every block handed out is either the system's, fresh, or one the
// system gave for this very layout and that was freed since -- no one else
// holds it, and it is the size and alignment asked for. Every block freed
// goes back to the system or into the store, never both.
unsafe impl GlobalAlloc for Recycling {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() >= KEPT_FROM {
            if let Some(p) = take(layout) {
                return p as *mut u8;
            }
        }
        // SAFETY: the caller's contract is the system's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() >= KEPT_FROM && keep(ptr as usize, layout) {
            return;
        }
        // SAFETY: `ptr` came from `alloc` with `layout`: the system's own,
        // or a kept block, which is the system's too.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() >= KEPT_FROM {
            if let Some(p) = take(layout) {
                let p = p as *mut u8;
                // SAFETY: a kept block of `layout.size()` bytes, now the
                // caller's alone.
                unsafe { p.write_bytes(0, layout.size()) };
                return p;
            }
        }
        // SAFETY: the caller's contract is the system's.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.size() < KEPT_FROM && new_size < KEPT_FROM {
            // SAFETY: a small block is the system's own, never a kept one.
            return unsafe { System.realloc(ptr, layout, new_size) };
        }
        // SAFETY: the caller's contract: `new_size`, with the old alignment,
        // is a layout; `ptr` holds `layout.size()` bytes.
        unsafe {
            let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
            let new = self.alloc(new_layout);
            if !new.is_null() {
                core::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
            new
        }
    }
}
