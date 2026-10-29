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
//!
//! And to the kernel heap's size: one allocation is at most 128 pages
//! (`PageTable::MAX_CONTIGUOUS_PAGES`), past which `kernel_alloc` returns
//! null -- an `alloc_error` and a panic for what cannot fail, an error for
//! a `try_reserve`. The system's allocator gives far more, so a list that
//! grows with what a disk or the network says reached 512 KiB unseen here
//! and panicked the kernel (a journal of 8193 transactions replayed at a
//! mount): here it is refused as the kernel refuses it -- by a fuzzer that
//! holds itself to it (`hold_to_ceiling`), whose own big allocations -- its
//! models, judges and images -- are marked as its own (`sched::harness`).
//! fs-fuzz does; net-fuzz's world and judges are not yet told apart from
//! the kernel's code, which its ceiling would refuse along with them.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
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

/// The kernel heap's page and the most of them one allocation is.
const PAGE: usize = 4096;
const MAX_PAGES: usize = 128;

/// Whether this fuzzer holds the kernel's code to the kernel heap's
/// ceiling at all (`hold_to_ceiling`).
static HELD: AtomicBool = AtomicBool::new(false);
/// The kernel heap's ceiling lifted, by a target whose model does not yet
/// know it (`lift_ceiling`).
static LIFTED: AtomicBool = AtomicBool::new(false);

/// The kernel's code held to the kernel heap's ceiling from here on: what a
/// fuzzer whose own big allocations are marked as its own calls at boot.
pub fn hold_to_ceiling() {
    HELD.store(true, Ordering::Relaxed);
}
/// A refusal has been said, its backtrace with it: once is enough.
static SAID: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Inside the saying of a refusal: what its backtrace allocates is let be.
    static SAYING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The ceiling lifted for as long as this lives -- by a target whose model
/// holds the kernel to more than its heap gives at once (ramfs keeps a
/// file's data as one allocation, which the kernel's heap refuses past 512
/// KiB, and the `vfs` target's model does not know it yet).
pub struct Lifted(());

pub fn lift_ceiling() -> Lifted {
    LIFTED.store(true, Ordering::Relaxed);
    Lifted(())
}

impl Drop for Lifted {
    fn drop(&mut self) {
        LIFTED.store(false, Ordering::Relaxed);
    }
}

/// Whether the kernel's heap would give `size` bytes aligned to `align` at
/// once (`kernel_alloc`: an over-aligned block is allocated `align` and a
/// pointer bigger). What the fuzzer allocates for itself is let be. A
/// refusal is said -- the first with the backtrace of what asked, since an
/// `alloc_error` aborts with none.
fn kernel_gives(size: usize, align: usize) -> bool {
    let whole = if align <= 8 { size } else { size.saturating_add(align + 8) };
    if whole.div_ceil(PAGE) <= MAX_PAGES || !HELD.load(Ordering::Relaxed) || sched::exempt()
        || PANICKING.load(Ordering::Relaxed) || LIFTED.load(Ordering::Relaxed) || SAYING.with(|s| s.get()) {
        return true;
    }
    SAYING.with(|s| s.set(true));
    let _ = writeln!(std::io::stderr(), "fuzz heap: task {} asks for {} bytes at once, past the {} KiB the kernel's \
                                         heap gives: refused, as kernel_alloc refuses it", sched::me(), size,
                     MAX_PAGES * PAGE / 1024);
    if !SAID.swap(true, Ordering::Relaxed) {
        let trace = std::backtrace::Backtrace::force_capture();
        let _ = writeln!(std::io::stderr(), "{}", trace);
    }
    SAYING.with(|s| s.set(false));
    false
}

// SAFETY: the system's allocator, every call passed through unchanged but
// for what the kernel's heap would refuse, which gets the null the
// contract allows; the checks before each only read this thread's own
// state and panic.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        check("allocates", layout.size());
        if !kernel_gives(layout.size(), layout.align()) {
            return core::ptr::null_mut();
        }
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
        if !kernel_gives(layout.size(), layout.align()) {
            return core::ptr::null_mut();
        }
        // SAFETY: as above.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            count(layout.size() as isize);
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        check("reallocates", new_size);
        if !kernel_gives(new_size, layout.align()) {
            return core::ptr::null_mut();
        }
        // SAFETY: as above.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            count(new_size as isize - layout.size() as isize);
        }
        p
    }
}
