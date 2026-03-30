/* RTL8168 DMA descriptor rings.
 *
 * Each ring consists of `RING_SIZE` 16-byte descriptors stored in a
 * contiguous DMA buffer.  The hardware identifies the end of the ring by
 * the EOR (End Of Ring) bit in the last descriptor's opts1 field.
 *
 * TX ring ownership protocol:
 *   - Software fills descriptor, sets TX_OWN to hand off to hardware.
 *   - Hardware clears TX_OWN after transmission.
 *   - Shadow array stores raw NetFrame handles (0 = empty) to keep the
 *     DMA buffer alive until hardware signals completion.
 *
 * RX ring ownership protocol:
 *   - Software allocates a NetFrame, writes its physical address into the
 *     descriptor, and sets RX_OWN to give the buffer to hardware.
 *   - Hardware writes received data, clears RX_OWN, updates opts1 length.
 *   - Software harvests the descriptor, reclaims the frame, and reposts a
 *     new one via post().
 */

use kcore::dma::DmaBuffer;
use kcore::net::NetFrame;
use crate::regs::*;

/* Number of descriptors in each ring.
 * 256 * 16 bytes = 4096 bytes = exactly one DMA page. */
pub const RING_SIZE: usize = 256;
const _: () = assert!(RING_SIZE * core::mem::size_of::<TxDesc>() <= 4096);

/* ================================================================== */
/* Descriptor layout (must match RTL8168 hardware register layout) */

#[repr(C)]
pub struct TxDesc {
    pub opts1:   u32, /* OWN | EOR | FS | LS | frame_len */
    pub opts2:   u32, /* checksum offload / VLAN (0 for basic driver) */
    pub addr_lo: u32, /* low 32 bits of buffer physical address */
    pub addr_hi: u32, /* high 32 bits of buffer physical address */
}
const _: () = assert!(core::mem::size_of::<TxDesc>() == 16);

#[repr(C)]
pub struct RxDesc {
    pub opts1:   u32, /* OWN | EOR | buffer_len (written with buf capacity; hardware fills rx_len) */
    pub opts2:   u32, /* checksum / VLAN status (read-only for basic driver) */
    pub addr_lo: u32,
    pub addr_hi: u32,
}
const _: () = assert!(core::mem::size_of::<RxDesc>() == 16);

/* ================================================================== */
/* TX ring */

pub struct TxRing {
    /* DMA memory holding RING_SIZE TxDesc structs */
    pub dma: DmaBuffer,
    /* Shadow raw frame handles: non-zero while descriptor is owned by hardware.
     * Using raw usize instead of Option<NetFrame> avoids the non-Copy array
     * initialisation restriction. */
    pub frames: [usize; RING_SIZE],
    /* Next free descriptor slot (written by flush_tx) */
    pub tail: usize,
    /* Next descriptor to check for completion (advanced by reap_completed) */
    pub head: usize,
}

impl TxRing {
    pub fn new(mut dma: DmaBuffer) -> Self {
        /* Zero the entire DMA page so no leftover TX_OWN bits can cause
         * the hardware to DMA from garbage addresses at TX_POLL time. */
        for b in dma.as_mut_slice().iter_mut() { *b = 0; }

        let mut ring = Self {
            dma,
            frames: [0usize; RING_SIZE],
            tail: 0,
            head: 0,
        };
        /* Mark the last descriptor as EOR so the hardware wraps to index 0 */
        ring.descs_mut()[RING_SIZE - 1].opts1 = TX_EOR;
        ring
    }

    /* Virtual-address pointer to the descriptor array in DMA memory */
    pub fn descs_mut(&mut self) -> &mut [TxDesc; RING_SIZE] {
        unsafe {
            &mut *(self.dma.as_mut_slice().as_mut_ptr() as *mut [TxDesc; RING_SIZE])
        }
    }

    pub fn descs(&self) -> &[TxDesc; RING_SIZE] {
        unsafe {
            &*(self.dma.as_slice().as_ptr() as *const [TxDesc; RING_SIZE])
        }
    }

    /* True if there is at least one free descriptor slot */
    pub fn has_space(&self) -> bool {
        ((self.tail + 1) % RING_SIZE) != self.head
    }

    /* Submit a frame into the next TX descriptor slot.
     * Caller must check has_space() first.  The frame is consumed and its
     * raw handle is stored in the shadow array to keep the DMA buffer alive
     * until reap_completed() sees that hardware cleared TX_OWN. */
    pub fn submit(&mut self, frame: NetFrame) {
        let idx  = self.tail;
        let phys = frame.data_phys();
        let len  = frame.len() as u32;

        let is_last = idx == RING_SIZE - 1;
        let eor: u32 = if is_last { TX_EOR } else { 0 };

        /* Transfer ownership into shadow array without calling Drop */
        let handle = frame.into_raw();

        let descs = self.descs_mut();
        descs[idx].addr_lo = phys as u32;
        descs[idx].addr_hi = (phys >> 32) as u32;
        descs[idx].opts2   = 0;
        /* Write opts1 (with TX_OWN) last; compiler_fence prevents reorder.
         * This ensures hardware sees a valid address before it sees OWN=1. */
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
        descs[idx].opts1   = TX_OWN | TX_FS | TX_LS | eor | (len & TX_LEN_MASK);

        self.frames[idx] = handle;
        self.tail = (idx + 1) % RING_SIZE;
    }

    /* Walk the ring from head and free all descriptors that hardware has
     * finished with (TX_OWN cleared).  Called only from flush_tx under
     * tx_lock; never from the ISR. */
    pub fn reap_completed(&mut self) {
        loop {
            if self.head == self.tail {
                break; /* ring empty */
            }
            let idx = self.head;
            /* Volatile read: hardware clears TX_OWN asynchronously via DMA.
             * Without read_volatile the compiler may hoist or cache this load. */
            let opts1 = unsafe {
                let p = &self.descs()[idx].opts1 as *const u32;
                core::ptr::read_volatile(p)
            };
            if opts1 & TX_OWN != 0 {
                break; /* hardware still owns */
            }
            let h = self.frames[idx];
            self.frames[idx] = 0;
            if h != 0 {
                /* Release the shadow NetFrame (decrements kernel refcount) */
                drop(unsafe { NetFrame::from_raw(h) });
            }
            self.head = (idx + 1) % RING_SIZE;
        }
    }
}

/* ================================================================== */
/* RX ring */

pub struct RxRing {
    pub dma: DmaBuffer,
    /* Per-slot shadow handle; non-zero while descriptor is owned by hardware */
    pub frames: [usize; RING_SIZE],
    /* Next descriptor to check for received data */
    pub head: usize,
}

impl RxRing {
    pub fn new(dma: DmaBuffer) -> Self {
        Self {
            dma,
            frames: [0usize; RING_SIZE],
            head: 0,
        }
    }

    pub fn descs_mut(&mut self) -> &mut [RxDesc; RING_SIZE] {
        unsafe {
            &mut *(self.dma.as_mut_slice().as_mut_ptr() as *mut [RxDesc; RING_SIZE])
        }
    }

    pub fn descs(&self) -> &[RxDesc; RING_SIZE] {
        unsafe {
            &*(self.dma.as_slice().as_ptr() as *const [RxDesc; RING_SIZE])
        }
    }

    /* Post a NetFrame at descriptor slot `idx`, giving ownership to hardware.
     * The frame handle is retained in `frames[idx]` for later harvest. */
    pub fn post(&mut self, idx: usize, frame: NetFrame) {
        let phys = frame.data_phys();
        let is_last = idx == RING_SIZE - 1;
        let eor: u32 = if is_last { RX_EOR } else { 0 };

        let handle = frame.into_raw();

        let descs = self.descs_mut();
        descs[idx].addr_lo = phys as u32;
        descs[idx].addr_hi = (phys >> 32) as u32;
        descs[idx].opts2   = 0;
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
        /* Program buffer capacity into len field; hardware replaces it with
         * the actual received frame length when it clears RX_OWN. */
        descs[idx].opts1   = RX_OWN | eor | (RX_BUF_SIZE as u32 & RX_LEN_MASK);

        self.frames[idx] = handle;
    }

    /* Attempt to harvest a received frame from the current head slot.
     * Returns Some(frame) with the received data if hardware has filled it,
     * or None if the hardware still owns the descriptor or the slot is empty.
     * The caller is responsible for calling `set_len` on the returned frame
     * before passing it to `enqueue_rx`. */
    pub fn harvest(&mut self) -> Option<(NetFrame, u32)> {
        let idx = self.head;
        let h = self.frames[idx];
        if h == 0 {
            return None;
        }
        /* Volatile read: hardware clears RX_OWN and writes the received
         * length via DMA.  Without read_volatile the compiler may cache
         * a stale value from a previous iteration. */
        let opts1 = unsafe {
            let p = &self.descs()[idx].opts1 as *const u32;
            core::ptr::read_volatile(p)
        };
        if opts1 & RX_OWN != 0 {
            return None; /* hardware still owns */
        }
        /* Extract received length before we clear the slot */
        let rx_len = opts1 & RX_LEN_MASK;
        self.frames[idx] = 0;
        self.head = (idx + 1) % RING_SIZE;
        Some((unsafe { NetFrame::from_raw(h) }, rx_len))
    }
}
