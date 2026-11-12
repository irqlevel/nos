//! The heap: `arena.rs`'s first fit over a few MiB of the BSS. The loader
//! runs once, briefly, on one CPU with interrupts off; what it allocates --
//! a configuration file and its parse, a filesystem's blocks a few at a
//! time, the virtio rings -- comes back to it as it is dropped, in whatever
//! order. Out of it, an allocation fails, and `alloc` makes that a panic,
//! which says so -- with what the heap held -- and turns the guest off.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arena::{Arena, HEAP_BYTES};

#[repr(C, align(4096))]
struct Space(UnsafeCell<[u8; HEAP_BYTES]>);

// SAFETY: the loader is one thread on one CPU with interrupts off; the
// arena hands each byte out to one block at a time.
unsafe impl Sync for Space {}

static SPACE: Space = Space(UnsafeCell::new([0; HEAP_BYTES]));

struct Heap(UnsafeCell<Arena>);

// SAFETY: as `Space`: one thread, and nothing the arena does calls back
// into the heap, so its state is never reached twice at once.
unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap(UnsafeCell::new(Arena::EMPTY));

impl Heap {
    /// The arena, given its stretch the first time.
    ///
    /// # Safety
    /// No other reference to the arena is live: the loader's one thread is
    /// in the allocator, which does not call itself.
    #[allow(clippy::mut_from_ref)]
    unsafe fn arena(&self) -> &mut Arena {
        // SAFETY: the caller's.
        let arena = unsafe { &mut *self.0.get() };
        if !arena.ready() {
            // SAFETY: SPACE is the heap's alone, 4096-aligned, and this runs
            // once, before any block is handed out.
            unsafe { arena.init(SPACE.0.get().cast::<u8>(), HEAP_BYTES) };
        }
        arena
    }
}

/* An allocation the arena had no room for: the bytes asked, and what the
 * heap held then -- kept apart from the arena, so that the panic the
 * failure becomes can say it without reaching into the allocator. */
static FAILED_ASK: AtomicUsize = AtomicUsize::new(0);
static FAILED_USED: AtomicUsize = AtomicUsize::new(0);

fn note(p: *mut u8, asked: usize, arena: &Arena) -> *mut u8 {
    if p.is_null() {
        FAILED_ASK.store(asked, Ordering::Relaxed);
        FAILED_USED.store(arena.usage().0, Ordering::Relaxed);
    }
    p
}

// SAFETY: every block is the arena's, handed out once until it is given
// back; the arena keeps its headers outside the bytes it hands out.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: one thread, and the arena calls nothing back.
        let arena = unsafe { self.arena() };
        note(arena.alloc(layout), layout.size(), arena)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: `ptr` is a block this heap handed out (GlobalAlloc's
        // contract), and as `alloc`.
        unsafe { self.arena().dealloc(ptr) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as `dealloc`, `layout` the block's own.
        let arena = unsafe { self.arena() };
        // SAFETY: as above.
        let p = unsafe { arena.realloc(ptr, layout, new_size) };
        note(p, new_size, arena)
    }
}

/// The last allocation the heap had no room for -- the bytes asked, and
/// those in use then -- and the heap's size: what a panic says of it.
pub fn failure() -> Option<(usize, usize, usize)> {
    let asked = FAILED_ASK.load(Ordering::Relaxed);
    (asked != 0).then(|| (asked, FAILED_USED.load(Ordering::Relaxed), HEAP_BYTES))
}
