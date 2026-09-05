/* RTL8125 DMA descriptor rings.
 *
 * The 8125 keeps the RTL8168's legacy 16-byte descriptor -- provided the
 * chip's "new TX descriptor format" bit stays clear, which hw_start() takes
 * care of.  Each ring is RING_SIZE descriptors in one contiguous DMA page;
 * the hardware finds the end of the ring by the EOR bit in the last one.
 *
 * Descriptor memory is written by the NIC concurrently with the CPU, so it
 * is never touched through a reference: every load and store goes through a
 * volatile access on a raw pointer (see desc_ptr()).
 *
 * TX ownership protocol:
 *   - software fills the descriptor and sets TX_OWN to hand it over
 *   - hardware clears TX_OWN once the frame is on the wire
 *   - the shadow array holds the raw NetFrame handle (0 = empty) so the
 *     buffer stays alive until that happens
 *
 * RX ownership protocol:
 *   - software posts a NetFrame's physical address and sets RX_OWN
 *   - hardware writes the frame, clears RX_OWN and puts the length in opts1
 *   - software harvests it, hands the frame up, and posts a fresh one
 */

use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use kcore::dma::DmaBuffer;
use kcore::net;
use kcore::net::NetFrame;

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
    pub opts1: u32,   /* OWN | EOR | FS | LS | frame_len */
    pub opts2: u32,   /* checksum offload / VLAN -- unused */
    pub addr_lo: u32, /* low 32 bits of the buffer's physical address */
    pub addr_hi: u32,
}
const _: () = assert!(core::mem::size_of::<TxDesc>() == 16);

#[repr(C)]
pub struct RxDesc {
    pub opts1: u32, /* OWN | EOR | buffer capacity; hardware writes rx_len */
    pub opts2: u32, /* checksum / VLAN status -- unused */
    pub addr_lo: u32,
    pub addr_hi: u32,
}
const _: () = assert!(core::mem::size_of::<RxDesc>() == 16);

/* ================================================================== */
/* TX ring */

pub struct TxRing {
    pub dma: DmaBuffer,
    /* Shadow handles, non-zero while the descriptor is owned by hardware.
     * Raw usize rather than Option<NetFrame> so the array can be built by
     * value without the non-Copy initialisation restriction. */
    pub frames: [usize; RING_SIZE],
    /* Next free slot (written by flush_tx) */
    pub tail: usize,
    /* Next slot to check for completion (advanced by reap_completed) */
    pub head: usize,
}

impl TxRing {
    pub fn new(mut dma: DmaBuffer) -> Self {
        /* Zero the page so no stale TX_OWN bit can make the chip DMA from a
         * garbage address the first time the doorbell is rung. */
        unsafe { core::ptr::write_bytes(dma.as_mut_ptr(), 0, dma.len()) };

        let mut ring = Self {
            dma,
            frames: [0usize; RING_SIZE],
            tail: 0,
            head: 0,
        };
        unsafe {
            write_volatile(addr_of_mut!((*ring.desc_ptr(RING_SIZE - 1)).opts1), TX_EOR);
        }
        ring
    }

    fn desc_ptr(&mut self, idx: usize) -> *mut TxDesc {
        unsafe { (self.dma.as_mut_ptr() as *mut TxDesc).add(idx) }
    }

    pub fn has_space(&self) -> bool {
        ((self.tail + 1) % RING_SIZE) != self.head
    }

    /// Hand one frame to the hardware.  Caller must have checked has_space().
    /// The frame is consumed; its handle lives in the shadow array until
    /// reap_completed() sees the chip clear TX_OWN.
    pub fn submit(&mut self, frame: NetFrame) {
        let idx = self.tail;
        let phys = frame.data_phys();
        let len = frame.len() as u32;

        let eor: u32 = if idx == RING_SIZE - 1 { TX_EOR } else { 0 };

        let handle = frame.into_raw();

        let d = self.desc_ptr(idx);
        unsafe {
            write_volatile(addr_of_mut!((*d).addr_lo), phys as u32);
            write_volatile(addr_of_mut!((*d).addr_hi), (phys >> 32) as u32);
            write_volatile(addr_of_mut!((*d).opts2), 0);
            /* opts1 (carrying TX_OWN) goes last, after a device-ordering
             * barrier: the NIC must see a valid address before it sees the
             * ownership handover.  dma_wmb is dmb oshst on arm64 -- an
             * atomic fence would only be dmb ish, which does not order
             * against a PCIe master; free on x86. */
            kcore::barrier::dma_wmb();
            write_volatile(
                addr_of_mut!((*d).opts1),
                TX_OWN | TX_FS | TX_LS | eor | (len & TX_LEN_MASK),
            );
        }

        self.frames[idx] = handle;
        self.tail = (idx + 1) % RING_SIZE;
    }

    /// Release every descriptor the chip has finished with.  Called from
    /// flush_tx only (under the C++ TxQueueLock), never from the ISR.
    pub fn reap_completed(&mut self, net: net::NetDeviceHandle) {
        loop {
            if self.head == self.tail {
                break; /* ring empty */
            }
            let idx = self.head;
            /* Volatile: the chip clears TX_OWN by DMA. */
            let opts1 = unsafe { read_volatile(addr_of!((*self.desc_ptr(idx)).opts1)) };
            if opts1 & TX_OWN != 0 {
                break; /* still owned by hardware */
            }
            let h = self.frames[idx];
            self.frames[idx] = 0;
            if h != 0 {
                /* Handed back, not dropped: this runs under the C++
                 * TxQueueLock with interrupts off, and dropping reaches
                 * Mm::Free -> a TLB shootdown that waits for every other CPU.
                 * A CPU spinning on that lock cannot answer it. */
                net.tx_done(unsafe { NetFrame::from_raw(h) });
            }
            self.head = (idx + 1) % RING_SIZE;
        }
    }
}

/* ================================================================== */
/* RX ring */

pub struct RxRing {
    pub dma: DmaBuffer,
    pub frames: [usize; RING_SIZE],
    /* Next slot to check for received data */
    pub head: usize,
}

impl RxRing {
    pub fn new(mut dma: DmaBuffer) -> Self {
        unsafe { core::ptr::write_bytes(dma.as_mut_ptr(), 0, dma.len()) };

        Self {
            dma,
            frames: [0usize; RING_SIZE],
            head: 0,
        }
    }

    fn desc_ptr(&mut self, idx: usize) -> *mut RxDesc {
        unsafe { (self.dma.as_mut_ptr() as *mut RxDesc).add(idx) }
    }

    /// Give slot `idx` a buffer and hand it to the hardware.
    pub fn post(&mut self, idx: usize, frame: NetFrame) {
        let phys = frame.data_phys();
        let eor: u32 = if idx == RING_SIZE - 1 { RX_EOR } else { 0 };

        let handle = frame.into_raw();

        let d = self.desc_ptr(idx);
        unsafe {
            write_volatile(addr_of_mut!((*d).addr_lo), phys as u32);
            write_volatile(addr_of_mut!((*d).addr_hi), (phys >> 32) as u32);
            write_volatile(addr_of_mut!((*d).opts2), 0);
            kcore::barrier::dma_wmb(); /* see TxRing::submit */
            /* The length field carries the buffer capacity on the way in;
             * the chip overwrites it with the received length. */
            write_volatile(
                addr_of_mut!((*d).opts1),
                RX_OWN | eor | (RX_BUF_SIZE as u32 & RX_LEN_MASK),
            );
        }

        self.frames[idx] = handle;
    }

    /// Take the frame at the head slot if the chip has filled it.
    /// Returns None when hardware still owns the descriptor, or when the slot
    /// is empty because an earlier refill failed -- the caller reposts it.
    /// The caller reads the length and the error bits out of opts1.
    pub fn harvest(&mut self) -> Option<(NetFrame, u32)> {
        let idx = self.head;
        let h = self.frames[idx];
        if h == 0 {
            return None;
        }
        /* Volatile: the chip clears RX_OWN and writes the length by DMA. */
        let opts1 = unsafe { read_volatile(addr_of!((*self.desc_ptr(idx)).opts1)) };
        if opts1 & RX_OWN != 0 {
            return None;
        }
        /* The payload pointer comes from the shadow array, not from the
         * descriptor, so there is no address dependency to order the packet
         * reads after the OWN load -- and a control dependency does not order
         * load->load on arm64.  Fence explicitly. */
        kcore::barrier::dma_rmb();
        self.frames[idx] = 0;
        self.head = (idx + 1) % RING_SIZE;
        Some((unsafe { NetFrame::from_raw(h) }, opts1))
    }
}
