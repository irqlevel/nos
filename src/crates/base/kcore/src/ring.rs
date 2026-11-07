//! A bounded multi-producer multi-consumer queue of words: one
//! compare-and-swap an operation, no lock anywhere and nothing that waits --
//! a push to a full ring and a pop from an empty one fail at once -- so safe
//! from any context, a hard IRQ handler's included. What the net frame pool
//! recycles its frames through, and what the zero-copy block server passes
//! requests with, between the receive path, its worker and the disk's
//! interrupt handler.
//!
//! The structure is Vyukov's. Every cell carries a sequence number, and a
//! producer or a consumer claims a position by advancing a shared counter
//! with one compare-and-swap. The cell of position `pos` is the producer's
//! to fill when its sequence is `pos`, and the consumer's to empty when it
//! is `pos + 1`; emptied, it is handed to the producer a lap ahead as
//! `pos + capacity`. Sequences only grow, so there is no ABA to have.
//!
//! All of it is atomics -- the value a cell holds as well, which the
//! sequence's release and acquire hand from one side to the other -- so the
//! ring needs no `unsafe`, and is `Send` and `Sync` because its fields are.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

/// The most cells a ring has: they are one allocation.
pub const MAX_CAPACITY: usize = 1 << 20;

struct Cell {
    seq: AtomicUsize,
    value: AtomicUsize,
}

pub struct LocklessRing {
    cells: Box<[Cell]>,
    mask: usize,
    enqueue_pos: AtomicUsize,
    dequeue_pos: AtomicUsize,
}

impl LocklessRing {
    /// A ring of `capacity` words -- a power of two, at most
    /// `MAX_CAPACITY` -- or None, as when there is no memory for it.
    pub fn new(capacity: usize) -> Option<Self> {
        if capacity == 0 || !capacity.is_power_of_two() || capacity > MAX_CAPACITY {
            return None;
        }

        let mut cells = Vec::new();
        cells.try_reserve_exact(capacity).ok()?;
        /* Cell i starts at sequence i: empty, and the producer that claims
         * position i finds the sequence it is looking for. */
        cells.extend((0..capacity).map(|i| Cell { seq: AtomicUsize::new(i), value: AtomicUsize::new(0) }));

        Some(Self {
            cells: cells.into_boxed_slice(),
            mask: capacity - 1,
            enqueue_pos: AtomicUsize::new(0),
            dequeue_pos: AtomicUsize::new(0),
        })
    }

    /// Queues `value`; false when the ring is full.
    #[inline]
    pub fn push(&self, value: usize) -> bool {
        let mut pos = self.enqueue_pos.load(Ordering::Relaxed);
        loop {
            let cell = &self.cells[pos & self.mask];
            /* Acquire: a cell a consumer handed back comes with that
             * consumer's read of what it held done. */
            let seq = cell.seq.load(Ordering::Acquire);
            match seq.wrapping_sub(pos) as isize {
                0 => match self.enqueue_pos.compare_exchange_weak(
                    pos, pos.wrapping_add(1), Ordering::Relaxed, Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        cell.value.store(value, Ordering::Relaxed);
                        /* Release: the value -- and whatever was written
                         * before the push, a frame's contents -- reaches
                         * the consumer that acquires this sequence. */
                        cell.seq.store(pos.wrapping_add(1), Ordering::Release);
                        return true;
                    }
                    /* Another producer took the position: try the next. */
                    Err(now) => pos = now,
                },
                /* The cell is still the lap behind's: the ring is full. */
                d if d < 0 => return false,
                /* Another producer has been here since `pos` was read. */
                _ => pos = self.enqueue_pos.load(Ordering::Relaxed),
            }
        }
    }

    /// The oldest value queued, or None when the ring is empty.
    #[inline]
    pub fn pop(&self) -> Option<usize> {
        let mut pos = self.dequeue_pos.load(Ordering::Relaxed);
        loop {
            let cell = &self.cells[pos & self.mask];
            /* Acquire: pairs with the producer's release of this sequence,
             * so the value read below is the one it stored. */
            let seq = cell.seq.load(Ordering::Acquire);
            match seq.wrapping_sub(pos.wrapping_add(1)) as isize {
                0 => match self.dequeue_pos.compare_exchange_weak(
                    pos, pos.wrapping_add(1), Ordering::Relaxed, Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let value = cell.value.load(Ordering::Relaxed);
                        /* Release: the read above is done before the
                         * producer a lap ahead, which acquires this, fills
                         * the cell again. */
                        cell.seq.store(pos.wrapping_add(self.mask).wrapping_add(1), Ordering::Release);
                        return Some(value);
                    }
                    Err(now) => pos = now,
                },
                /* Nothing has been pushed here yet: the ring is empty. */
                d if d < 0 => return None,
                _ => pos = self.dequeue_pos.load(Ordering::Relaxed),
            }
        }
    }

    /// A snapshot, stale at once: for reporting, never for a decision.
    pub fn len(&self) -> usize {
        let enqueued = self.enqueue_pos.load(Ordering::Relaxed);
        let dequeued = self.dequeue_pos.load(Ordering::Relaxed);
        match enqueued.wrapping_sub(dequeued) as isize {
            n if n > 0 => (n as usize).min(self.cells.len()),
            _ => 0,
        }
    }
}
