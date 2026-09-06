/* Descriptor rings for the igb family.
 *
 * These work nothing like the Realtek rings next door. There is no OWN bit
 * the chip clears in place: ownership is expressed by two ring pointers the
 * hardware exposes as registers. The head (RDH/TDH) is where the chip is
 * working; the tail (RDT/TDT) is what software has handed it, and software
 * moves the tail to give descriptors away. A descriptor is finished when the
 * chip writes a Descriptor Done bit into a write-back format that overlays
 * the one software wrote.
 *
 * One consequence worth stating: the tail may never catch the software's own
 * clean pointer, because tail == head would be indistinguishable from an
 * empty ring. So one descriptor of the RING_SIZE is always left in hand --
 * `desc_unused` is what enforces it, and it is why the ring holds
 * RING_SIZE - 1 buffers rather than RING_SIZE. */

use alloc::vec;
use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use kcore::dma::DmaBuffer;
use kcore::net::{NetDeviceHandle, NetFrame};

use crate::regs::*;

/// Descriptors per ring.
///
/// 256, which is what Linux's igb uses for both directions on every part in
/// the family (IGB_DEFAULT_RXD, against a maximum of 4096). It drives an I210
/// at line rate on that, so ring depth is not what stands between this driver
/// and the same -- raising it to 1024 was tried and told us only that the
/// shadow arrays did not belong on the stack.
pub const RING_SIZE: usize = 256;
pub const DESC_BYTES: usize = 16;
pub const RING_PAGES: usize = (RING_SIZE * DESC_BYTES + 4095) / 4096;

/// Receive buffer size. 2048 keeps SRRCTL.BSIZEPKT at its default of 2 KiB
/// and, with RCTL.LPE off, means no frame ever spans two descriptors -- so a
/// harvest is always one whole packet.
pub const RX_BUF_SIZE: usize = 2048;

const _: () = assert!(RING_SIZE * DESC_BYTES <= RING_PAGES * 4096);

/* Both descriptor formats are four 32-bit words, and both directions
 * overwrite what software wrote with a write-back layout. Naming the words
 * rather than declaring two structs and transmuting between them keeps every
 * access plainly volatile. */
#[repr(C)]
struct Desc {
    d0: u32,
    d1: u32,
    d2: u32,
    d3: u32,
}

/* Receive, software format:  d0/d1 = packet buffer address (lo/hi)
 *                            d2/d3 = header buffer address, unused here
 * Receive, write-back:       d0    = RSS type / packet type
 *                            d1    = RSS hash
 *                            d2    = status and error bits
 *                            d3    = length in the low half, VLAN in the high
 *
 * Transmit, software format: d0/d1 = buffer address (lo/hi)
 *                            d2    = command, type and length
 *                            d3    = offload info and payload length
 * Transmit, write-back:      d3    = status, of which only DD matters here */

pub struct RxRing {
    pub dma: DmaBuffer,
    /// Raw NetFrame handles, one per slot; 0 where a refill has not happened.
    ///
    /// Heap, not an inline array. Two of these inline is 16 KiB inside a
    /// struct that Box::new builds on the stack before moving, and a kernel
    /// stack is 32 KiB in total -- at RING_SIZE 1024 that is a double fault
    /// during device init, which is how this was found.
    frames: Vec<usize>,
    /// The slot the chip will complete next, from software's point of view.
    next_to_clean: usize,
    /// The slot to hand over next.
    next_to_use: usize,
    /// What was last written to RDT. Kept because the tail can move without
    /// a refill: an error frame goes straight back into the ring, and if it
    /// takes the last free slot the refill that follows posts nothing and
    /// would leave the chip never told about it.
    rdt_written: u32,
}

impl RxRing {
    pub fn new(mut dma: DmaBuffer) -> Self {
        unsafe { core::ptr::write_bytes(dma.as_mut_ptr(), 0, dma.len()) };
        Self {
            dma,
            frames: vec![0usize; RING_SIZE],
            next_to_clean: 0,
            next_to_use: 0,
            rdt_written: 0,
        }
    }

    fn desc_ptr(&mut self, idx: usize) -> *mut Desc {
        unsafe { (self.dma.as_mut_ptr() as *mut Desc).add(idx) }
    }

    /// The same address without borrowing the ring mutably, for the state
    /// dump: that runs from the shell task while the poll owns this ring on
    /// another CPU, and handing out a second `&mut` to it would be a lie to
    /// the compiler whether or not the reads are harmless.
    fn desc_ptr_shared(&self, idx: usize) -> *const Desc {
        unsafe { (self.dma.as_ptr() as *const Desc).add(idx) }
    }

    /// Slots that could still be handed to the chip, keeping the one-descriptor
    /// gap that stops the tail from meeting the head.
    pub fn desc_unused(&self) -> usize {
        if self.next_to_clean > self.next_to_use {
            self.next_to_clean - self.next_to_use - 1
        } else {
            RING_SIZE + self.next_to_clean - self.next_to_use - 1
        }
    }

    /// Put a buffer in the next free slot. Does not move the tail: the caller
    /// posts a run of them and publishes the whole run once, which is both
    /// fewer register writes and the order the chip wants -- descriptors
    /// visible before the tail that points past them.
    ///
    /// Returns the slot used, or None when the gap rule says there is no room.
    pub fn post_next(&mut self, frame: NetFrame) -> Option<usize> {
        if self.desc_unused() == 0 {
            return None;
        }

        let idx = self.next_to_use;
        let phys = frame.data_phys();
        let handle = frame.into_raw();

        let d = self.desc_ptr(idx);
        unsafe {
            /* Writing the software format also clears the status word, so the
             * stale Descriptor Done from the previous round cannot be read as
             * a fresh completion. */
            write_volatile(addr_of_mut!((*d).d0), phys as u32);
            write_volatile(addr_of_mut!((*d).d1), (phys >> 32) as u32);
            write_volatile(addr_of_mut!((*d).d2), 0);
            write_volatile(addr_of_mut!((*d).d3), 0);
        }

        self.frames[idx] = handle;
        self.next_to_use = (idx + 1) % RING_SIZE;
        Some(idx)
    }

    /// The value to write to RDT, meaning "everything up to but excluding
    /// this is yours".
    pub fn tail(&self) -> u32 {
        self.next_to_use as u32
    }

    /// Whether the tail has moved since it was last published to the chip.
    pub fn needs_tail_write(&self) -> bool {
        self.next_to_use as u32 != self.rdt_written
    }

    /// Record that `tail()` has just been written to RDT.
    pub fn mark_tail_written(&mut self) {
        self.rdt_written = self.next_to_use as u32;
    }

    /// Software's two pointers and the status word of the descriptor it is
    /// waiting on, for the state dump. Takes `&self`: see `desc_ptr_shared`.
    pub fn debug_state(&self) -> (u32, u32, u32, u32) {
        let idx = self.next_to_clean;
        let status = unsafe { read_volatile(addr_of!((*self.desc_ptr_shared(idx)).d2)) };
        let posted = if self.frames[idx] != 0 { 1 } else { 0 };
        (
            self.next_to_clean as u32,
            self.next_to_use as u32,
            status,
            posted,
        )
    }

    /// Whether the next slot has been completed by the chip. Cheap: one read
    /// of DMA memory, no register access -- which is what lets the poll loop
    /// go round again without touching the device.
    pub fn has_work(&mut self) -> bool {
        let idx = self.next_to_clean;
        if self.frames[idx] == 0 {
            /* An earlier refill failed and left this slot empty; the chip
             * cannot pass it, so there is nothing to wait for. Reported as
             * work so the caller reposts it. */
            return true;
        }
        let status = unsafe { read_volatile(addr_of!((*self.desc_ptr(idx)).d2)) };
        status & RXD_STAT_DD != 0
    }

    /// Take the completed frame at the clean pointer.
    /// Returns the frame, the status/error word and the length in bytes.
    pub fn harvest(&mut self) -> Option<(NetFrame, u32, usize)> {
        let idx = self.next_to_clean;
        let h = self.frames[idx];
        if h == 0 {
            return None;
        }

        let status = unsafe { read_volatile(addr_of!((*self.desc_ptr(idx)).d2)) };
        if status & RXD_STAT_DD == 0 {
            return None;
        }

        /* The payload address comes from the shadow array, not from the
         * descriptor, so nothing ties the packet reads to the load of the
         * status word. A control dependency does not order load->load on
         * arm64; fence explicitly. */
        kcore::barrier::dma_rmb();

        let len = (unsafe { read_volatile(addr_of!((*self.desc_ptr(idx)).d3)) } & 0xFFFF) as usize;

        self.frames[idx] = 0;
        self.next_to_clean = (idx + 1) % RING_SIZE;
        Some((unsafe { NetFrame::from_raw(h) }, status, len))
    }
}

impl Drop for RxRing {
    fn drop(&mut self) {
        for i in 0..RING_SIZE {
            let h = self.frames[i];
            if h != 0 {
                self.frames[i] = 0;
                drop(unsafe { NetFrame::from_raw(h) });
            }
        }
    }
}

pub struct TxRing {
    pub dma: DmaBuffer,
    frames: Vec<usize>,
    next_to_use: usize,
    next_to_clean: usize,
}

impl TxRing {
    pub fn new(mut dma: DmaBuffer) -> Self {
        unsafe { core::ptr::write_bytes(dma.as_mut_ptr(), 0, dma.len()) };
        Self {
            dma,
            frames: vec![0usize; RING_SIZE],
            next_to_use: 0,
            next_to_clean: 0,
        }
    }

    fn desc_ptr(&mut self, idx: usize) -> *mut Desc {
        unsafe { (self.dma.as_mut_ptr() as *mut Desc).add(idx) }
    }

    /// Room for one more, under the same gap rule the receive ring follows.
    pub fn can_submit(&self) -> bool {
        (self.next_to_use + 1) % RING_SIZE != self.next_to_clean
    }

    /// Place a frame in the next slot. Does not ring the doorbell; the caller
    /// submits a run and writes TDT once.
    pub fn submit(&mut self, frame: NetFrame) -> bool {
        if !self.can_submit() {
            return false;
        }

        let idx = self.next_to_use;
        let phys = frame.data_phys();
        let len = frame.len();
        let handle = frame.into_raw();

        let d = self.desc_ptr(idx);
        unsafe {
            write_volatile(addr_of_mut!((*d).d0), phys as u32);
            write_volatile(addr_of_mut!((*d).d1), (phys >> 32) as u32);

            /* Every frame is a whole packet in one buffer, so every descriptor
             * is EOP. RS asks for the write-back this driver reaps on, IFCS
             * has the chip append the CRC, DEXT selects the advanced layout
             * these offsets describe. */
            write_volatile(
                addr_of_mut!((*d).d2),
                (len as u32 & 0xFFFF)
                    | TXD_DTYP_DATA
                    | TXD_DCMD_EOP
                    | TXD_DCMD_IFCS
                    | TXD_DCMD_RS
                    | TXD_DCMD_DEXT,
            );

            /* Payload length for a packet with no offloads is just the frame,
             * and the status half starts clear so the DD we reap on is the
             * chip's. */
            write_volatile(addr_of_mut!((*d).d3), (len as u32) << TXD_PAYLEN_SHIFT);
        }

        self.frames[idx] = handle;
        self.next_to_use = (idx + 1) % RING_SIZE;
        true
    }

    pub fn tail(&self) -> u32 {
        self.next_to_use as u32
    }

    /// Give back every frame the chip has finished with. Called at the head of
    /// flush_tx, never from the ISR.
    pub fn reap_completed(&mut self, net: NetDeviceHandle) -> usize {
        let mut reaped = 0;

        while self.next_to_clean != self.next_to_use {
            let idx = self.next_to_clean;
            let h = self.frames[idx];
            if h == 0 {
                /* Nothing here to give back, but the slot is still behind the
                 * use pointer: step over it rather than stalling the reap. */
                self.next_to_clean = (idx + 1) % RING_SIZE;
                continue;
            }

            let status = unsafe { read_volatile(addr_of!((*self.desc_ptr(idx)).d3)) };
            if status & TXD_STAT_DD == 0 {
                break;
            }

            kcore::barrier::dma_rmb();

            self.frames[idx] = 0;
            self.next_to_clean = (idx + 1) % RING_SIZE;

            /* Back to the pool through the net layer, which is what keeps the
             * frame off any allocator on this path. */
            net.tx_done(unsafe { NetFrame::from_raw(h) });
            reaped += 1;
        }

        reaped
    }
}

impl Drop for TxRing {
    fn drop(&mut self) {
        for i in 0..RING_SIZE {
            let h = self.frames[i];
            if h != 0 {
                self.frames[i] = 0;
                drop(unsafe { NetFrame::from_raw(h) });
            }
        }
    }
}
