//! The program's allocator: the system's, holding everything allocated to
//! what the kernel's heap gives at once -- 128 pages, 512 KiB
//! (`PageTable::MAX_CONTIGUOUS_PAGES`). The system gives far more, so a list
//! that grows with the log, the network or a member's backlog would pass
//! here and panic the kernel the day it got that long; here it is a
//! finding. A larger block is still given -- an allocator may not unwind --
//! and counted, and the run that asked for it fails (`check`), the first
//! with the backtrace of what asked. The fuzzer's own allocations are held
//! to it too: none of them needs more.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The kernel heap's page, and the most of them one allocation is.
const PAGE: usize = 4096;
const MAX_PAGES: usize = 128;
pub const CEILING: usize = PAGE * MAX_PAGES;

pub struct Heap;

/// Blocks asked for past the ceiling, and the largest of them.
static OVER: AtomicU64 = AtomicU64::new(0);
static LARGEST: AtomicU64 = AtomicU64::new(0);
/// The first refusal's backtrace has been said.
static SAID: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Inside the fuzzer's own work, or saying a refusal (which allocates).
    static EXEMPT: Cell<u32> = const { Cell::new(0) };
}

/// The block the kernel's heap would give for `size` bytes aligned to
/// `align`: an over-aligned one is allocated `align` and a pointer bigger.
fn kernel_gives(size: usize, align: usize) -> bool {
    let whole = if align <= 8 { size } else { size.saturating_add(align + 8) };
    whole.div_ceil(PAGE) <= MAX_PAGES
}

fn note(size: usize, align: usize) {
    if kernel_gives(size, align) || EXEMPT.with(|e| e.get()) > 0 {
        return;
    }
    OVER.fetch_add(1, Ordering::Relaxed);
    LARGEST.fetch_max(size as u64, Ordering::Relaxed);
    if !SAID.swap(true, Ordering::Relaxed) {
        let _own = Exempt::new();
        let trace = std::backtrace::Backtrace::force_capture();
        let _ = writeln!(std::io::stderr(), "raft heap: {} bytes asked for at once, past the {} KiB the kernel's heap \
                                             gives:\n{}", size, CEILING / 1024, trace);
    }
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size(), layout.align());
        // SAFETY: the caller's contract, handed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size(), layout.align());
        // SAFETY: the caller's contract, handed on.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, handed on.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size, layout.align());
        // SAFETY: the caller's contract, handed on.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Held while a refusal is said, which allocates.
struct Exempt;

impl Exempt {
    fn new() -> Exempt {
        EXEMPT.with(|e| e.set(e.get() + 1));
        Exempt
    }
}

impl Drop for Exempt {
    fn drop(&mut self) {
        EXEMPT.with(|e| e.set(e.get() - 1));
    }
}

/// Blocks past the ceiling asked for so far.
pub fn over() -> u64 {
    OVER.load(Ordering::Relaxed)
}

/// `Err` if a block past the ceiling was asked for since `before`.
pub fn check(before: u64) -> Result<(), String> {
    let now = over();
    if now == before {
        return Ok(());
    }
    Err(format!("{} allocations past the {} KiB the kernel's heap gives at once, the largest {} bytes", now - before,
                CEILING / 1024, LARGEST.load(Ordering::Relaxed)))
}
