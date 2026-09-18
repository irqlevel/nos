/* RTL8125 DMA descriptor rings.
 *
 * The 8125 keeps the RTL8168's legacy 16-byte descriptor -- provided the
 * chip's "new TX descriptor format" bit stays clear, which hw_start() takes
 * care of.  Each ring is RING_SIZE descriptors in one contiguous DMA page;
 * the hardware finds the end of the ring by the EOR bit in the last one.
 *
 * Descriptor memory is written by the NIC concurrently with the CPU, so
 * every word of it is a `Volatile` cell: shared -- with the chip, and with
 * whoever dumps the chip's state while the poll is running -- and each load
 * and store is one the compiler leaves where it was written.
 *
 * TX ownership protocol:
 *   - software fills the descriptor and sets TX_OWN to hand it over
 *   - hardware clears TX_OWN once the frame is on the wire
 *   - the shadow array holds the NetFrame itself, so the buffer stays alive
 *     until that happens
 *
 * RX ownership protocol:
 *   - software posts a NetFrame's physical address and sets RX_OWN
 *   - hardware writes the frame, clears RX_OWN and puts the length in opts1
 *   - software harvests it, hands the frame up, and posts a fresh one
 */

use alloc::vec::Vec;
use kcore::dma::{Descriptor, DmaBuffer, Volatile};
use kcore::net::{NetFrame, TxQueue};

use crate::regs::*;

/* 256 * 16 bytes = 4096 bytes = exactly one DMA page. */
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
/* Descriptor layout (must match the chip's legacy format) */

#[repr(C)]
pub struct TxDesc {
    opts1: Volatile<u32>,   /* OWN | EOR | FS | LS | frame_len */
    opts2: Volatile<u32>,   /* checksum offload / VLAN -- unused */
    addr_lo: Volatile<u32>, /* low 32 bits of the buffer's physical address */
    addr_hi: Volatile<u32>,
}
const _: () = assert!(core::mem::size_of::<TxDesc>() == 16);

#[repr(C)]
pub struct RxDesc {
    opts1: Volatile<u32>, /* OWN | EOR | buffer capacity; hardware writes rx_len */
    opts2: Volatile<u32>, /* checksum / VLAN status -- unused */
    addr_lo: Volatile<u32>,
    addr_hi: Volatile<u32>,
}
const _: () = assert!(core::mem::size_of::<RxDesc>() == 16);

/* Four volatile words each. */
unsafe impl Descriptor for TxDesc {}
unsafe impl Descriptor for RxDesc {}

/// A ring's descriptors, where the chip is about to be told they are: shared
/// with it, zeroed, and for good. None when the memory is too small for them.
///
/// Zeroed so that no stale OWN bit can make the chip DMA from a garbage
/// address the first time the doorbell is rung.
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
    descs: &'static [TxDesc],
    /* Where the chip is told the ring is */
    pub phys: u64,
    /* The frame of each descriptor the hardware owns. */
    frames: Vec<Option<NetFrame>>,
    /* Next free slot (written by flush_tx) */
    tail: usize,
    /* Next slot to check for completion (advanced by reap_completed) */
    head: usize,
}

impl TxRing {
    pub fn new(dma: DmaBuffer) -> Option<Self> {
        let (descs, phys) = ring::<TxDesc>(dma)?;

        descs[RING_SIZE - 1].opts1.write(TX_EOR);

        Some(Self {
            descs,
            phys,
            frames: shadow()?,
            tail: 0,
            head: 0,
        })
    }

    pub fn has_space(&self) -> bool {
        ((self.tail + 1) % RING_SIZE) != self.head
    }

    /// Hand one frame to the hardware.  Caller must have checked has_space().
    /// The frame is consumed; it lives in the shadow array until
    /// reap_completed() sees the chip clear TX_OWN.
    pub fn submit(&mut self, frame: NetFrame) {
        let idx = self.tail;
        let phys = frame.data_phys();
        let len = frame.len() as u32;

        let eor: u32 = if idx == RING_SIZE - 1 { TX_EOR } else { 0 };

        let d = &self.descs[idx];
        d.addr_lo.write(phys as u32);
        d.addr_hi.write((phys >> 32) as u32);
        d.opts2.write(0);
        /* opts1 (carrying TX_OWN) goes last, after a device-ordering
         * barrier: the NIC must see a valid address before it sees the
         * ownership handover.  dma_wmb is dmb oshst on arm64 -- an
         * atomic fence would only be dmb ish, which does not order
         * against a PCIe master; free on x86. */
        kcore::barrier::dma_wmb();
        d.opts1.write(TX_OWN | TX_FS | TX_LS | eor | (len & TX_LEN_MASK));

        self.frames[idx] = Some(frame);
        self.tail = (idx + 1) % RING_SIZE;
    }

    /// Release every descriptor the chip has finished with.  Called from
    /// flush_tx only (under the device's transmit lock), never from the ISR.
    pub fn reap_completed(&mut self, stack: &mut TxQueue<'_>) {
        loop {
            if self.head == self.tail {
                break; /* ring empty */
            }
            let idx = self.head;
            /* Volatile: the chip clears TX_OWN by DMA. */
            let opts1 = self.descs[idx].opts1.read();
            if opts1 & TX_OWN != 0 {
                break; /* still owned by hardware */
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

/// The receive descriptors as anything but the poll may see them: they are
/// the chip's as much as the driver's. What the state dump reads.
pub struct RxView {
    descs: &'static [RxDesc],
}

impl RxView {
    /// `opts1` of descriptor `idx`, as it is in memory this instant.
    pub fn opts1(&self, idx: usize) -> u32 {
        self.descs.get(idx).map_or(0, |d| d.opts1.read())
    }
}

pub struct RxRing {
    descs: &'static [RxDesc],
    pub phys: u64,
    frames: Vec<Option<NetFrame>>,
    /* Next slot to check for received data */
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

    /// The same descriptors, for whoever dumps the chip's state while the
    /// poll owns this ring on another CPU.
    pub fn view(&self) -> RxView {
        RxView { descs: self.descs }
    }

    /// The slot the next received frame will be found in.
    pub fn head(&self) -> usize {
        self.head
    }

    /// Whether slot `idx` has no buffer posted: a refill that failed.
    pub fn is_empty_slot(&self, idx: usize) -> bool {
        self.frames[idx].is_none()
    }

    /// Give slot `idx` a buffer and hand it to the hardware.
    pub fn post(&mut self, idx: usize, frame: NetFrame) {
        let phys = frame.data_phys();
        let eor: u32 = if idx == RING_SIZE - 1 { RX_EOR } else { 0 };

        let d = &self.descs[idx];
        d.addr_lo.write(phys as u32);
        d.addr_hi.write((phys >> 32) as u32);
        d.opts2.write(0);
        kcore::barrier::dma_wmb(); /* see TxRing::submit */
        /* The length field carries the buffer capacity on the way in;
         * the chip overwrites it with the received length. */
        d.opts1.write(RX_OWN | eor | (RX_BUF_SIZE as u32 & RX_LEN_MASK));

        self.frames[idx] = Some(frame);
    }

    /// Whether the head slot needs attention: either the chip has completed
    /// it, or an earlier refill left it empty and it must be reposted. Used
    /// to close the poll: unmasking and then finding work is the race NAPI
    /// has to re-check for.
    pub fn has_work(&self) -> bool {
        let idx = self.head;
        if self.frames[idx].is_none() {
            return true;
        }
        let opts1 = self.descs[idx].opts1.read();
        opts1 & RX_OWN == 0
    }

    /// Take the frame at the head slot if the chip has filled it.
    /// Returns None when hardware still owns the descriptor, or when the slot
    /// is empty because an earlier refill failed -- the caller reposts it.
    /// The caller reads the length and the error bits out of opts1.
    pub fn harvest(&mut self) -> Option<(NetFrame, u32)> {
        let idx = self.head;
        if self.frames[idx].is_none() {
            return None;
        }
        /* Volatile: the chip clears RX_OWN and writes the length by DMA. */
        let opts1 = self.descs[idx].opts1.read();
        if opts1 & RX_OWN != 0 {
            return None;
        }
        /* The payload pointer comes from the shadow array, not from the
         * descriptor, so there is no address dependency to order the packet
         * reads after the OWN load -- and a control dependency does not order
         * load->load on arm64.  Fence explicitly. */
        kcore::barrier::dma_rmb();
        let frame = self.frames[idx].take()?;
        self.head = (idx + 1) % RING_SIZE;
        Some((frame, opts1))
    }
}
