//! A split virtqueue: the descriptor table, the available ring the driver
//! publishes into, and the used ring the device publishes back.
//!
//! The three live in one run of DMA memory the device reads and writes while
//! the driver does, so every access here is volatile and the ordering is
//! spelled out with device barriers (`dma_wmb`/`dma_rmb`) rather than left to
//! the compiler. Forming a `&mut [u8]` over that memory would be unsound,
//! which is why nothing below does.

use kcore::barrier::{dma_rmb, dma_wmb};
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::trace;

/// The most descriptors a queue here has. A device that asks for more is
/// negotiated down (modern) or refused (legacy), because a driver's
/// per-descriptor bookkeeping is sized to this.
pub const MAX_DESCRIPTORS: u16 = 256;

const DESC_SIZE: usize = 16;
const USED_ELEM_SIZE: usize = 8;

/// The descriptor is device-writable: the device fills it, the driver reads
/// it. Without the flag it is the other way round.
const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

/// Ask the device not to interrupt on completion: what a driver that polls
/// sets, so a level-triggered line is not left asserted for good.
const AVAIL_F_NO_INTERRUPT: u16 = 1;

/// One buffer of a chain handed to the device.
#[derive(Clone, Copy)]
pub struct Buf {
    /// Physical, and the memory stays put until the device is done with it
    pub addr: u64,
    pub len: u32,
    /// The device writes it (a read's payload, a status byte)
    pub writable: bool,
}

impl Buf {
    pub fn read(addr: u64, len: u32) -> Self {
        Self { addr, len, writable: false }
    }

    pub fn write(addr: u64, len: u32) -> Self {
        Self { addr, len, writable: true }
    }
}

pub struct Queue {
    mem: DmaBuffer,
    size: u16,

    /// Byte offsets into mem
    desc: usize,
    avail: usize,
    used: usize,

    /// Head of the free descriptor chain, and how long it is
    free_head: u16,
    num_free: u16,
    /// The used index the driver has consumed up to
    last_used: u16,
}

impl Queue {
    /// Lay a queue of `size` descriptors out in DMA memory, the way the
    /// virtio spec places the three rings: the descriptor table, the
    /// available ring after it, and the used ring on the next page.
    pub fn new(size: u16) -> Option<Self> {
        if size == 0 || size > MAX_DESCRIPTORS || !size.is_power_of_two() {
            trace!(0, "virtio: queue size {} is not one this supports", size);
            return None;
        }

        let desc_bytes = size as usize * DESC_SIZE;
        /* flags, idx, the ring, and the used_event the driver never reads */
        let avail_bytes = 2 + 2 + size as usize * 2 + 2;
        let used_off = (desc_bytes + avail_bytes + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let used_bytes = 2 + 2 + size as usize * USED_ELEM_SIZE + 2;
        let pages = (used_off + used_bytes + PAGE_SIZE - 1) / PAGE_SIZE;

        let mem = DmaBuffer::new(pages)?;

        let mut queue = Self {
            mem,
            size,
            desc: 0,
            avail: desc_bytes,
            used: used_off,
            free_head: 0,
            num_free: size,
            last_used: 0,
        };

        /* The pages come zeroed; all that is left is to thread the free
         * descriptors together. */
        for i in 0..size - 1 {
            queue.write_desc_next(i, i + 1);
        }
        queue.write_desc_next(size - 1, 0xFFFF);

        Some(queue)
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    pub fn desc_phys(&self) -> u64 {
        self.mem.phys()
    }

    pub fn avail_phys(&self) -> u64 {
        self.mem.phys() + self.avail as u64
    }

    pub fn used_phys(&self) -> u64 {
        self.mem.phys() + self.used as u64
    }

    /// How many descriptors are free to be handed out.
    pub fn free(&self) -> u16 {
        self.num_free
    }

    /// Put a chain of buffers on the available ring. The head descriptor's
    /// index comes back, which is what the device names in the used ring.
    /// None when the chain is empty or longer than the free list.
    pub fn add(&mut self, bufs: &[Buf]) -> Option<u16> {
        if bufs.is_empty() || bufs.len() > self.num_free as usize {
            return None;
        }

        let head = self.free_head;
        let mut index = head;

        for (i, buf) in bufs.iter().enumerate() {
            let next = self.read_desc_next(index);
            let mut flags = if buf.writable { DESC_F_WRITE } else { 0 };

            if i + 1 < bufs.len() {
                flags |= DESC_F_NEXT;
            }

            self.write_desc(index, buf.addr, buf.len, flags);

            if i + 1 < bufs.len() {
                index = next;
            } else {
                self.free_head = next;
                self.write_desc_next(index, 0);
            }
        }

        self.num_free -= bufs.len() as u16;

        /* The ring entry has to be visible to the device before the index
         * that publishes it. */
        let avail_idx = self.read_u16(self.avail + 2);
        self.write_u16(self.avail + 4 + (avail_idx % self.size) as usize * 2, head);
        dma_wmb();
        self.write_u16(self.avail + 2, avail_idx.wrapping_add(1));
        dma_wmb();

        Some(head)
    }

    /// Ask the device not to interrupt on completion. Advisory per the spec,
    /// honoured by QEMU, and what keeps a polled device from holding a
    /// level-triggered line asserted after its first completion.
    pub fn disable_interrupts(&mut self) {
        let flags = self.read_u16(self.avail);
        self.write_u16(self.avail, flags | AVAIL_F_NO_INTERRUPT);
        dma_wmb();
    }

    /// Whether the device has published a completion the driver has not
    /// taken yet.
    pub fn has_used(&self) -> bool {
        dma_rmb();
        self.last_used != self.read_u16(self.used + 2)
    }

    /// The next completion: the head descriptor's index and the number of
    /// bytes the device wrote. The chain goes back on the free list.
    pub fn take_used(&mut self) -> Option<(u32, u32)> {
        dma_rmb();
        if self.last_used == self.read_u16(self.used + 2) {
            return None;
        }

        /* Order the index load above before the ring and payload loads
         * below: the ring offset derives from last_used, not from the index
         * that was read, so there is no address dependency, and a control
         * dependency does not order load->load on arm64. Without this a
         * fresh index can pair with a stale ring entry -- and it also fences
         * the caller's reads of what the device wrote into the buffers. */
        dma_rmb();

        let at = self.used + 4 + (self.last_used % self.size) as usize * USED_ELEM_SIZE;
        let id = self.read_u32(at);
        let len = self.read_u32(at + 4);
        self.last_used = self.last_used.wrapping_add(1);

        /* id and len are the device's to say. A head outside the ring would
         * index past the descriptor table and poison the free list, so the
         * completion is reported -- the caller checks the id against its own
         * bookkeeping -- but no chain is walked for it. */
        if id >= self.size as u32 {
            return Some((id, len));
        }

        /* Bound the walk by the ring size, so a cyclic Next cannot spin. */
        let mut index = id as u16;
        for _ in 0..self.size {
            let next = self.read_desc_next(index);
            let has_next = self.read_desc_flags(index) & DESC_F_NEXT != 0;

            self.write_desc_flags(index, 0);
            self.write_desc_next(index, self.free_head);
            self.free_head = index;
            self.num_free += 1;

            if !has_next || next >= self.size {
                break;
            }
            index = next;
        }

        Some((id, len))
    }

    /* ---- the rings, read and written as the device sees them ---- */

    fn desc_at(&self, index: u16) -> usize {
        self.desc + index as usize * DESC_SIZE
    }

    fn write_desc(&mut self, index: u16, addr: u64, len: u32, flags: u16) {
        let at = self.desc_at(index);
        self.write_u64(at, addr);
        self.write_u32(at + 8, len);
        self.write_u16(at + 12, flags);
    }

    fn read_desc_flags(&self, index: u16) -> u16 {
        self.read_u16(self.desc_at(index) + 12)
    }

    fn write_desc_flags(&mut self, index: u16, flags: u16) {
        self.write_u16(self.desc_at(index) + 12, flags);
    }

    fn read_desc_next(&self, index: u16) -> u16 {
        self.read_u16(self.desc_at(index) + 14)
    }

    fn write_desc_next(&mut self, index: u16, next: u16) {
        self.write_u16(self.desc_at(index) + 14, next);
    }

    /* The rings are written through a pointer cast back to mut from the one
     * the allocation handed out, because the device writes them too: a
     * &mut over memory that changes underneath is not a reference this may
     * hold. Every access is volatile for the same reason. */
    fn base(&self) -> *mut u8 {
        self.mem.as_ptr() as *mut u8
    }

    fn read_u16(&self, at: usize) -> u16 {
        unsafe { (self.base().add(at) as *const u16).read_volatile() }
    }

    fn write_u16(&self, at: usize, value: u16) {
        unsafe { (self.base().add(at) as *mut u16).write_volatile(value) }
    }

    fn read_u32(&self, at: usize) -> u32 {
        unsafe { (self.base().add(at) as *const u32).read_volatile() }
    }

    fn write_u32(&self, at: usize, value: u32) {
        unsafe { (self.base().add(at) as *mut u32).write_volatile(value) }
    }

    fn write_u64(&self, at: usize, value: u64) {
        unsafe { (self.base().add(at) as *mut u64).write_volatile(value) }
    }
}
