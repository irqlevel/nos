//! A bounded multi-producer multi-consumer queue of words in storage of its
//! own -- no allocation anywhere, so it can be a `static` and be used from
//! the first line of the boot, before there is a page allocator.
//!
//! The structure is Vyukov's, the same one `kernel/lockless_ring.h` uses:
//! every cell carries a sequence number, and a producer or consumer claims a
//! slot by advancing a shared position with a single compare-and-swap.
//! Sequence numbers only ever increase, so there is no ABA to have and no
//! double-width compare-and-swap to write. Enqueue fails when full and
//! dequeue when empty; neither ever blocks, which is what makes it safe from
//! the places that must not -- a hard IRQ handler, code under a spinlock, the
//! panic path.
//!
//! One difference from the kernel's, and the reason this can be a `static`
//! with no initialiser to run: Vyukov's cells start with `seq[i] = i`, which
//! no constant can express for an array. Each cell here holds that number
//! **less its own index**, which starts every cell at zero. The index is a
//! whole `pos & mask`, so what is left is `pos & !mask` -- the round -- and
//! the two comparisons below are against that instead.

use core::sync::atomic::{AtomicUsize, Ordering};

pub struct Cell {
    /// The cell's sequence number less its index; zero when never used.
    seq: AtomicUsize,
    data: AtomicUsize,
}

impl Cell {
    const fn new() -> Self {
        Self { seq: AtomicUsize::new(0), data: AtomicUsize::new(0) }
    }
}

/// `N` must be a power of two, which `new` checks at compile time.
pub struct StaticRing<const N: usize> {
    cells: [Cell; N],
    enqueue_pos: AtomicUsize,
    dequeue_pos: AtomicUsize,
}

/* Built to be used from every CPU at once, which is the whole point. */
unsafe impl<const N: usize> Send for StaticRing<N> {}
unsafe impl<const N: usize> Sync for StaticRing<N> {}

impl<const N: usize> StaticRing<N> {
    pub const fn new() -> Self {
        assert!(N.is_power_of_two(), "a ring's capacity must be a power of two");
        Self {
            cells: [const { Cell::new() }; N],
            enqueue_pos: AtomicUsize::new(0),
            dequeue_pos: AtomicUsize::new(0),
        }
    }

    pub const fn capacity(&self) -> usize {
        N
    }

    /// Queues `value`; false when the ring is full.
    pub fn push(&self, value: usize) -> bool {
        let mut pos = self.enqueue_pos.load(Ordering::Relaxed);
        loop {
            let cell = &self.cells[pos & (N - 1)];
            let round = pos & !(N - 1);
            let seq = cell.seq.load(Ordering::Acquire);

            /* The cell is free for this round exactly when its sequence has
             * caught up with the round; behind means the ring is full, and
             * ahead means another producer took this slot first. */
            if seq == round {
                if self
                    .enqueue_pos
                    .compare_exchange_weak(pos, pos + 1, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    cell.data.store(value, Ordering::Relaxed);
                    cell.seq.store(round + 1, Ordering::Release);
                    return true;
                }
            } else if seq.wrapping_sub(round) > usize::MAX / 2 {
                return false;
            } else {
                pos = self.enqueue_pos.load(Ordering::Relaxed);
                continue;
            }

            pos = self.enqueue_pos.load(Ordering::Relaxed);
        }
    }

    /// The oldest queued value, or None when there is none.
    pub fn pop(&self) -> Option<usize> {
        let mut pos = self.dequeue_pos.load(Ordering::Relaxed);
        loop {
            let cell = &self.cells[pos & (N - 1)];
            let round = pos & !(N - 1);
            let seq = cell.seq.load(Ordering::Acquire);

            /* Filled for this round when the sequence is one past it. */
            if seq == round + 1 {
                if self
                    .dequeue_pos
                    .compare_exchange_weak(pos, pos + 1, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    let value = cell.data.load(Ordering::Relaxed);
                    /* Free again, for the round after this one. */
                    cell.seq.store(round + N, Ordering::Release);
                    return Some(value);
                }
            } else if seq.wrapping_sub(round + 1) > usize::MAX / 2 {
                return None;
            } else {
                pos = self.dequeue_pos.load(Ordering::Relaxed);
                continue;
            }

            pos = self.dequeue_pos.load(Ordering::Relaxed);
        }
    }

    /// A snapshot, immediately stale by construction: for reporting, never
    /// for a decision.
    pub fn count(&self) -> usize {
        let enqueued = self.enqueue_pos.load(Ordering::Relaxed);
        let dequeued = self.dequeue_pos.load(Ordering::Relaxed);
        enqueued.wrapping_sub(dequeued).min(N)
    }
}
