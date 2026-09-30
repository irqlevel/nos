//! The fuzzer's allocator: the system's, holding the kernel to its rule --
//! nothing is allocated or freed with interrupts off. In the kernel a free
//! can reach the page allocator, which shoots the TLB down on every other
//! CPU and waits for each to answer; a CPU spinning with interrupts off on
//! a lock the allocating one holds never answers, and the two wait for each
//! other for good, with nothing to say so (CLAUDE.md: "Never allocate or
//! free with a spinlock held"). Here it is a finding, where it happens.
//!
//! The fuzzer's own code -- the kernel's C++ half, the NIC -- runs with
//! interrupts off at times, inside the kernel's; what it allocates is not
//! the kernel's, and is let be (`sched::harness`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use super::sched;

pub struct Heap;

/// A panic has begun: its message is allocated too, and must be let be.
pub static PANICKING: AtomicBool = AtomicBool::new(false);

/// Bytes the kernel's code allocated and has not freed -- not the fuzzer's
/// own, its log and its bookkeeping (`sched::harness`): what a target holds
/// to staying the same across work that should leave nothing behind.
static LIVE: AtomicIsize = AtomicIsize::new(0);

pub fn live() -> isize {
    LIVE.load(Ordering::Relaxed)
}

fn count(bytes: isize) {
    if !sched::exempt() {
        LIVE.fetch_add(bytes, Ordering::Relaxed);
    }
}

pub fn check(what: &str, size: usize) {
    if sched::irq_off() == 0 || sched::exempt() || PANICKING.load(Ordering::Relaxed) {
        return;
    }
    PANICKING.store(true, Ordering::Relaxed);
    panic!("invariant: task {} {} {} bytes with interrupts off -- a spin lock held: a free can wait on every other \
            CPU, and one spinning on this lock never answers", sched::me(), what, size);
}

// SAFETY: the system's allocator, every call passed through unchanged; the
// check before each only reads this thread's own state and panics.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        check("allocates", layout.size());
        // SAFETY: the caller's contract is the system's.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            count(layout.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        check("frees", layout.size());
        count(-(layout.size() as isize));
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        check("allocates", layout.size());
        // SAFETY: as above.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            count(layout.size() as isize);
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        check("reallocates", new_size);
        // SAFETY: as above.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            count(new_size as isize - layout.size() as isize);
        }
        p
    }
}
