/* The kernel's lockless ring (kernel/lockless_ring.h): a bounded
   multi-producer multi-consumer queue of words, one compare-and-swap an
   operation and no lock anywhere -- so safe with interrupts off, in a hard
   IRQ handler too. */
extern "C" {
    /// A ring of `capacity` words, a power of two; 0 when there is no memory.
    pub fn kernel_ring_create(capacity: usize) -> usize;
    pub fn kernel_ring_destroy(ring: usize);
    /// 1 once queued, 0 when the ring is full.
    pub fn kernel_ring_push(ring: usize, value: usize) -> i32;
    /// 1 with *value set, 0 when the ring is empty.
    pub fn kernel_ring_pop(ring: usize, value: *mut usize) -> i32;
    /// A snapshot, stale at once: for reporting only.
    pub fn kernel_ring_count(ring: usize) -> usize;
}
