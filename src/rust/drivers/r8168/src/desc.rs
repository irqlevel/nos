/* RTL8168 DMA descriptor rings.
 *
 * Each ring consists of `RING_SIZE` 16-byte descriptors stored in a
 * contiguous DMA buffer.  The hardware identifies the end of the ring by
 * the EOR (End Of Ring) bit in the last descriptor's opts1 field.
 *
 * The descriptor memory is written by the NIC concurrently with the CPU,
 * so every word of it is a `Volatile` cell: shared, and each load and store
 * is one the compiler leaves where it was written.
 *
 * TX ring ownership protocol:
 *   - Software fills descriptor, sets TX_OWN to hand off to hardware.
 *   - Hardware clears TX_OWN after transmission.
 *   - The shadow array holds the NetFrame of every descriptor the hardware
 *     owns, which is what keeps its DMA buffer alive until the hardware
 *     signals completion.
 *
 * RX ring ownership protocol:
 *   - Software allocates a NetFrame, writes its physical address into the
 *     descriptor, and sets RX_OWN to give the buffer to hardware.
 *   - Hardware writes received data, clears RX_OWN, updates opts1 length.
 *   - Software harvests the descriptor, reclaims the frame, and reposts a
 *     new one via post().
 */

use alloc::vec::Vec;
use kcore::dma::{Descriptor, DmaBuffer, Volatile};
use kcore::net::{NetFrame, TxQueue};
use crate::regs::*;

/* Number of descriptors in each ring.
 * 256 * 16 bytes = 4096 bytes = exactly one DMA page. */
/* Descriptors per ring.
 *
 * 256, which is one page of them, and measured to be the better number.
 *
 * RDU -- the chip finding no descriptor it owns -- starts at around two
 * thousand packets a second here, so the obvious move was a bigger ring, and
 * it was tried: 1024, four pages. It made throughput worse, not better. The
 * ceiling on packets actually received fell from about 5000 a second to about
 * 2000, consistently, across a rate ladder and a two-minute sustained run.
 *
 * The reason a ring cannot fix this is that it does not change how fast the
 * receive softirq drains, and the drain is the ceiling: one queue, one vector,
 * one CPU (`netload` reports every packet arriving on cpu 9). A ring only
 * decides how long a burst can be absorbed before RDU, and paying for that in
 * cache footprint -- 1024 buffers of 2 KiB is 2 MiB cycled through, against
 * 512 KiB here -- costs more than the RDU did. Going past this needs multiple
 * receive queues spread over CPUs, not more descriptors on one. */
pub const RING_SIZE: usize = 256;

/* Pages the ring needs, rounded up. Must stay a power of two: the DMA
   allocator hands back `1 << log2(requested)` pages, so asking for three
   would quietly get two. */
pub const RING_PAGES: usize = (RING_SIZE * 16 + 4095) / 4096;
const _: () = assert!(RING_PAGES.is_power_of_two());
const _: () = assert!(RING_SIZE * core::mem::size_of::<TxDesc>() <= RING_PAGES * 4096);
const _: () = assert!(RING_SIZE * core::mem::size_of::<RxDesc>() <= RING_PAGES * 4096);

/* ================================================================== */
/* Descriptor layout (must match RTL8168 hardware register layout) */

#[repr(C)]
pub struct TxDesc {
    opts1:   Volatile<u32>, /* OWN | EOR | FS | LS | frame_len */
    opts2:   Volatile<u32>, /* checksum offload / VLAN (0 for basic driver) */
    addr_lo: Volatile<u32>, /* low 32 bits of buffer physical address */
    addr_hi: Volatile<u32>, /* high 32 bits of buffer physical address */
}
const _: () = assert!(core::mem::size_of::<TxDesc>() == 16);

#[repr(C)]
pub struct RxDesc {
    opts1:   Volatile<u32>, /* OWN | EOR | buffer_len (written with buf capacity; hardware fills rx_len) */
    opts2:   Volatile<u32>, /* checksum / VLAN status (read-only for basic driver) */
    addr_lo: Volatile<u32>,
    addr_hi: Volatile<u32>,
}
const _: () = assert!(core::mem::size_of::<RxDesc>() == 16);

/* Four volatile words each. */
unsafe impl Descriptor for TxDesc {}
unsafe impl Descriptor for RxDesc {}

/// A ring's descriptors, where the chip is about to be told they are: shared
/// with it, zeroed, and for good. None when the memory is too small for them.
fn ring<D: Descriptor>(dma: DmaBuffer) -> Option<(&'static [D], u64)> {
    let (descs, phys) = dma.leak_ring::<D>();
    if descs.len() < RING_SIZE {
        return None;
    }
    Some((&descs[..RING_SIZE], phys))
}

/// A ring's shadow of what is posted in it: the frame in each slot, none
/// where there is none.
fn shadow() -> Option<Vec<Option<NetFrame>>> {
    let mut frames = Vec::new();
    frames.try_reserve_exact(RING_SIZE).ok()?;
    frames.resize_with(RING_SIZE, || None);
    Some(frames)
}

/* ================================================================== */
/* TX ring */

pub struct TxRing {
    /* RING_SIZE TxDesc structs in DMA memory */
    descs: &'static [TxDesc],
    /* Where the chip is told the ring is */
    pub phys: u64,
    /* The frame of each descriptor the hardware owns */
    frames: Vec<Option<NetFrame>>,
    /* Next free descriptor slot (written by flush_tx) */
    tail: usize,
    /* Next descriptor to check for completion (advanced by reap_completed) */
    head: usize,
}

impl TxRing {
    pub fn new(dma: DmaBuffer) -> Option<Self> {
        /* The whole ring comes zeroed, so no leftover TX_OWN bits can cause
         * the hardware to DMA from garbage addresses at TX_POLL time. */
        let (descs, phys) = ring::<TxDesc>(dma)?;

        /* Mark the last descriptor as EOR so the hardware wraps to index 0 */
        descs[RING_SIZE - 1].opts1.write(TX_EOR);

        Some(Self {
            descs,
            phys,
            frames: shadow()?,
            tail: 0,
            head: 0,
        })
    }

    /* True if there is at least one free descriptor slot */
    pub fn has_space(&self) -> bool {
        ((self.tail + 1) % RING_SIZE) != self.head
    }

    /* Submit a frame into the next TX descriptor slot.
     * Caller must check has_space() first.  The frame is consumed and kept
     * in the shadow array, its DMA buffer alive, until reap_completed() sees
     * that hardware cleared TX_OWN. */
    pub fn submit(&mut self, frame: NetFrame) {
        let idx  = self.tail;
        let phys = frame.data_phys();
        let len  = frame.len() as u32;

        let is_last = idx == RING_SIZE - 1;
        let eor: u32 = if is_last { TX_EOR } else { 0 };

        let d = &self.descs[idx];
        d.addr_lo.write(phys as u32);
        d.addr_hi.write((phys >> 32) as u32);
        d.opts2.write(0);
        /* Write opts1 (with TX_OWN) last; dma_wmb orders the descriptor
         * stores against the OWN store as seen by the NIC (dmb oshst on
         * arm64 — an atomic fence is only dmb ish, whose domain excludes
         * a PCIe master; free on x86). Hardware must see a valid address
         * before it sees OWN=1. */
        kcore::barrier::dma_wmb();
        d.opts1.write(TX_OWN | TX_FS | TX_LS | eor | (len & TX_LEN_MASK));

        self.frames[idx] = Some(frame);
        self.tail = (idx + 1) % RING_SIZE;
    }

    /* Walk the ring from head and free all descriptors that hardware has
     * finished with (TX_OWN cleared).  Called only from flush_tx under
     * tx_lock; never from the ISR. */
    pub fn reap_completed(&mut self, stack: &mut TxQueue<'_>) {
        loop {
            if self.head == self.tail {
                break; /* ring empty */
            }
            let idx = self.head;
            /* Volatile read: hardware clears TX_OWN asynchronously via DMA. */
            let opts1 = self.descs[idx].opts1.read();
            if opts1 & TX_OWN != 0 {
                break; /* hardware still owns */
            }
            if let Some(frame) = self.frames[idx].take() {
                /* Handed back, not dropped: this runs under the device's
                 * transmit lock with interrupts off, and dropping reaches
                 * Mm::Free -> a TLB shootdown that waits for every other CPU.
                 * A CPU spinning on that lock cannot answer it. */
                stack.done(frame);
            }
            self.head = (idx + 1) % RING_SIZE;
        }
    }
}

/* ================================================================== */
/* RX ring */

pub struct RxRing {
    descs: &'static [RxDesc],
    pub phys: u64,
    /* Per-slot shadow frame; there while the descriptor is owned by hardware */
    frames: Vec<Option<NetFrame>>,
    /* Next descriptor to check for received data */
    head: usize,
}

impl RxRing {
    pub fn new(dma: DmaBuffer) -> Option<Self> {
        let (descs, phys) = ring::<RxDesc>(dma)?;
        Some(Self {
            descs,
            phys,
            frames: shadow()?,
            head: 0,
        })
    }

    /* The slot the next received frame will be found in. */
    pub fn head(&self) -> usize {
        self.head
    }

    /* Whether slot `idx` has no buffer posted: a refill that failed. */
    pub fn is_empty_slot(&self, idx: usize) -> bool {
        self.frames[idx].is_none()
    }

    /* Post a NetFrame at descriptor slot `idx`, giving ownership to hardware.
     * The frame is retained in `frames[idx]` for later harvest. */
    pub fn post(&mut self, idx: usize, frame: NetFrame) {
        let phys = frame.data_phys();
        let is_last = idx == RING_SIZE - 1;
        let eor: u32 = if is_last { RX_EOR } else { 0 };

        let d = &self.descs[idx];
        d.addr_lo.write(phys as u32);
        d.addr_hi.write((phys >> 32) as u32);
        d.opts2.write(0);
        kcore::barrier::dma_wmb(); /* see TxRing::submit */
        /* Program buffer capacity into len field; hardware replaces it with
         * the actual received frame length when it clears RX_OWN. */
        d.opts1.write(RX_OWN | eor | (RX_BUF_SIZE as u32 & RX_LEN_MASK));

        self.frames[idx] = Some(frame);
    }

    /* Attempt to harvest a received frame from the current head slot.
     * Returns Some((frame, opts1)) if hardware has filled it, or None if
     * the hardware still owns the descriptor or the slot is empty (a slot
     * is left empty when a refill allocation failed; the caller is
     * responsible for reposting it before harvesting again).
     * The caller extracts the length from opts1 and must check its error
     * bits (RX_ERR_MASK, RX_FF/RX_LF) before passing the frame on. */
    pub fn harvest(&mut self) -> Option<(NetFrame, u32)> {
        let idx = self.head;
        if self.frames[idx].is_none() {
            return None;
        }
        /* Volatile read: hardware clears RX_OWN and writes the received
         * length via DMA. */
        let opts1 = self.descs[idx].opts1.read();
        if opts1 & RX_OWN != 0 {
            return None; /* hardware still owns */
        }
        /* The frame payload pointer comes from the shadow array, not the
         * descriptor — no address dependency — so the caller's reads of the
         * packet bytes must be fenced after the OWN-bit load (dma_rmb; a
         * control dependency does not order load->load on arm64). */
        kcore::barrier::dma_rmb();
        let frame = self.frames[idx].take()?;
        self.head = (idx + 1) % RING_SIZE;
        Some((frame, opts1))
    }
}
