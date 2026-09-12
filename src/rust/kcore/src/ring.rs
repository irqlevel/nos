//! The kernel's lockless ring (kernel/lockless_ring.h), for words: a bounded
//! multi-producer multi-consumer queue, one compare-and-swap an operation and
//! no lock anywhere -- so safe from any context, a hard IRQ handler's
//! included. What the zero-copy block server passes requests with, between
//! the receive path, its worker and the disk's interrupt handler.

use ffi::ring;

pub struct LocklessRing {
    handle: usize,
}

/* The kernel's ring is built to be used from every CPU at once */
unsafe impl Send for LocklessRing {}
unsafe impl Sync for LocklessRing {}

impl LocklessRing {
    /// A ring of `capacity` words -- a power of two -- or None.
    pub fn new(capacity: usize) -> Option<Self> {
        if capacity == 0 || !capacity.is_power_of_two() {
            return None;
        }
        let handle = unsafe { ring::kernel_ring_create(capacity) };
        if handle == 0 { None } else { Some(Self { handle }) }
    }

    /// Queues `value`; false when the ring is full.
    #[inline]
    pub fn push(&self, value: usize) -> bool {
        unsafe { ring::kernel_ring_push(self.handle, value) != 0 }
    }

    #[inline]
    pub fn pop(&self) -> Option<usize> {
        let mut value = 0usize;
        if unsafe { ring::kernel_ring_pop(self.handle, &mut value) } != 0 {
            Some(value)
        } else {
            None
        }
    }

    /// A snapshot, stale at once: for reporting, never for a decision.
    pub fn len(&self) -> usize {
        unsafe { ring::kernel_ring_count(self.handle) }
    }
}

impl Drop for LocklessRing {
    fn drop(&mut self) {
        unsafe { ring::kernel_ring_destroy(self.handle) }
    }
}
