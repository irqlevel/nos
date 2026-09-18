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

use alloc::vec::Vec;
use kcore::dma::{Descriptor, DmaBuffer, Volatile};
use kcore::net::{NetFrame, TxQueue};

use crate::regs::*;

/// Descriptors per ring.
///
/// 1024 * 16 bytes is four pages, which the allocator hands out contiguously
/// because it rounds to a power of two. Raised from 256 to find out whether
/// ring depth has anything to do with the chip dropping packets while it owns
/// every descriptor in the ring -- at 256 it dropped 408,000 a second with
/// all 255 available, which is not the shape of a ring that is too short, but
/// measuring beats arguing about it.
pub const RING_SIZE: usize = 1024;
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
pub struct Desc {
    d0: Volatile<u32>,
    d1: Volatile<u32>,
    d2: Volatile<u32>,
    d3: Volatile<u32>,
}

const _: () = assert!(core::mem::size_of::<Desc>() == DESC_BYTES);

/* Four volatile words. */
unsafe impl Descriptor for Desc {}

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

/// A ring's descriptors, where the chip has been told they are: shared with
/// it, and for good. None when there is no memory for them, or not enough.
fn ring(dma: DmaBuffer) -> Option<(&'static [Desc], u64)> {
    let (descs, phys) = dma.leak_ring::<Desc>();
    if descs.len() < RING_SIZE {
        return None;
    }
    Some((&descs[..RING_SIZE], phys))
}

/// A ring's shadow of what is posted in it: the frame in each slot, none
/// where there is none. On the heap, not inline -- see `RxRing::frames`.
fn shadow() -> Option<Vec<Option<NetFrame>>> {
    let mut frames = Vec::new();
    frames.try_reserve_exact(RING_SIZE).ok()?;
    frames.resize_with(RING_SIZE, || None);
    Some(frames)
}

/// The receive ring as anything but the poll may see it: the descriptors
/// themselves, which are the chip's as much as the driver's, and where the
/// poll last said it was. What the state dump reads.
pub struct RxView {
    descs: &'static [Desc],
}

impl RxView {
    /// The status word of descriptor `idx`, as it is in memory this instant.
    pub fn status(&self, idx: usize) -> u32 {
        self.descs.get(idx).map_or(0, |d| d.d2.read())
    }
}

pub struct RxRing {
    descs: &'static [Desc],
    pub phys: u64,
    /// The frame in each slot; None where a refill has not happened.
    ///
    /// Heap, not an inline array. Two of these inline is 16 KiB inside a
    /// struct that Box::new builds on the stack before moving, and a kernel
    /// stack is 32 KiB in total -- at RING_SIZE 1024 that is a double fault
    /// during device init, which is how this was found.
    frames: Vec<Option<NetFrame>>,
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
    pub fn new(dma: DmaBuffer) -> Option<Self> {
        let (descs, phys) = ring(dma)?;
        Some(Self {
            descs,
            phys,
            frames: shadow()?,
            next_to_clean: 0,
            next_to_use: 0,
            rdt_written: 0,
        })
    }

    /// The same descriptors, for whoever dumps the chip's state while the
    /// poll owns this ring on another CPU.
    pub fn view(&self) -> RxView {
        RxView { descs: self.descs }
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

        /* Writing the software format also clears the status word, so the
         * stale Descriptor Done from the previous round cannot be read as a
         * fresh completion. */
        let d = &self.descs[idx];
        d.d0.write(phys as u32);
        d.d1.write((phys >> 32) as u32);
        d.d2.write(0);
        d.d3.write(0);

        self.frames[idx] = Some(frame);
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

    /// Software's two pointers, and whether the slot it is waiting on has a
    /// buffer in it: what the poll leaves out for the state dump.
    pub fn pointers(&self) -> (u32, u32, bool) {
        (
            self.next_to_clean as u32,
            self.next_to_use as u32,
            self.frames[self.next_to_clean].is_some(),
        )
    }

    /// Whether the next slot has been completed by the chip. Cheap: one read
    /// of DMA memory, no register access -- which is what lets the poll loop
    /// go round again without touching the device.
    pub fn has_work(&mut self) -> bool {
        let idx = self.next_to_clean;
        if self.frames[idx].is_none() {
            /* An earlier refill failed and left this slot empty; the chip
             * cannot pass it, so there is nothing to wait for. Reported as
             * work so the caller reposts it. */
            return true;
        }
        self.descs[idx].d2.read() & RXD_STAT_DD != 0
    }

    /// Take the completed frame at the clean pointer.
    /// Returns the frame, the status/error word and the length in bytes.
    pub fn harvest(&mut self) -> Option<(NetFrame, u32, usize)> {
        let idx = self.next_to_clean;
        if self.frames[idx].is_none() {
            return None;
        }

        let d = &self.descs[idx];
        let status = d.d2.read();
        if status & RXD_STAT_DD == 0 {
            return None;
        }

        /* The payload address comes from the shadow array, not from the
         * descriptor, so nothing ties the packet reads to the load of the
         * status word. A control dependency does not order load->load on
         * arm64; fence explicitly. */
        kcore::barrier::dma_rmb();

        let len = (d.d3.read() & 0xFFFF) as usize;

        let frame = self.frames[idx].take()?;
        self.next_to_clean = (idx + 1) % RING_SIZE;
        Some((frame, status, len))
    }
}

pub struct TxRing {
    descs: &'static [Desc],
    pub phys: u64,
    frames: Vec<Option<NetFrame>>,
    /// The slots that asked the chip for a write-back (RS). Not every one
    /// does: see `submit` and `report_last`.
    rs: Vec<bool>,
    next_to_use: usize,
    next_to_clean: usize,
}

impl TxRing {
    pub fn new(dma: DmaBuffer) -> Option<Self> {
        let (descs, phys) = ring(dma)?;

        let mut rs = Vec::new();
        rs.try_reserve_exact(RING_SIZE).ok()?;
        rs.resize(RING_SIZE, false);

        Some(Self { descs, phys, frames: shadow()?, rs, next_to_use: 0, next_to_clean: 0 })
    }

    /// Room for one more, under the same gap rule the receive ring follows.
    pub fn can_submit(&self) -> bool {
        (self.next_to_use + 1) % RING_SIZE != self.next_to_clean
    }

    /// Place a frame in the next slot, asking the chip to report it done when
    /// `rs`. Does not ring the doorbell: the caller submits a run, marks its
    /// last descriptor with `report_last` and writes TDT once. A frame there
    /// is no room for comes back.
    pub fn submit(&mut self, frame: NetFrame, rs: bool) -> Result<(), NetFrame> {
        if !self.can_submit() {
            return Err(frame);
        }

        let idx = self.next_to_use;
        let phys = frame.data_phys();
        let len = frame.len();

        /* RS asks for the write-back this driver reaps on -- not on every
         * descriptor: each is a descriptor write the chip makes and, with
         * TXDW enabled, an interrupt, and under a flood of echoes, one a
         * packet was a tenth of the receive CPU in the interrupt handler's
         * register reads alone. */
        let rs_bit = if rs { TXD_DCMD_RS } else { 0 };

        let d = &self.descs[idx];
        d.d0.write(phys as u32);
        d.d1.write((phys >> 32) as u32);

        /* Every frame is a whole packet in one buffer, so every descriptor is
         * EOP. IFCS has the chip append the CRC, DEXT selects the advanced
         * layout these offsets describe. */
        d.d2.write(
            (len as u32 & 0xFFFF)
                | TXD_DTYP_DATA
                | TXD_DCMD_EOP
                | TXD_DCMD_IFCS
                | rs_bit
                | TXD_DCMD_DEXT,
        );

        /* Payload length for a packet with no offloads is just the frame, and
         * the status half starts clear so the DD we reap on is the chip's. */
        d.d3.write((len as u32) << TXD_PAYLEN_SHIFT);

        self.frames[idx] = Some(frame);
        self.rs[idx] = rs;
        self.next_to_use = (idx + 1) % RING_SIZE;
        Ok(())
    }

    /// Ask for a write-back on the descriptor submitted last, if it did not
    /// already. Before the doorbell that hands it over, while the chip cannot
    /// be reading it.
    pub fn report_last(&mut self) {
        if self.next_to_use == self.next_to_clean {
            return;
        }

        let idx = (self.next_to_use + RING_SIZE - 1) % RING_SIZE;
        if self.rs[idx] {
            return;
        }

        let d = &self.descs[idx];
        d.d2.write(d.d2.read() | TXD_DCMD_RS);
        self.rs[idx] = true;
    }

    pub fn tail(&self) -> u32 {
        self.next_to_use as u32
    }

    /// Give back every frame the chip has finished with. Called at the head of
    /// flush_tx, never from the ISR. Only a descriptor that asked for a
    /// write-back gets one, and the chip works through the ring in order: the
    /// next such descriptor done means everything up to it is.
    pub fn reap_completed(&mut self, stack: &mut TxQueue<'_>) -> usize {
        let mut reaped = 0;

        while self.next_to_clean != self.next_to_use {
            let mut watch = self.next_to_clean;
            while watch != self.next_to_use && !self.rs[watch] {
                watch = (watch + 1) % RING_SIZE;
            }

            /* Every run ends with a descriptor that reports (report_last), so
             * one is always ahead of whatever is unreaped. */
            if watch == self.next_to_use {
                break;
            }

            if self.descs[watch].d3.read() & TXD_STAT_DD == 0 {
                break;
            }

            kcore::barrier::dma_rmb();

            loop {
                let idx = self.next_to_clean;
                let frame = self.frames[idx].take();
                self.rs[idx] = false;
                self.next_to_clean = (idx + 1) % RING_SIZE;

                /* Back to the pool through the net layer, which is what keeps
                 * the frame off any allocator on this path. */
                if let Some(frame) = frame {
                    stack.done(frame);
                    reaped += 1;
                }

                if idx == watch {
                    break;
                }
            }
        }

        reaped
    }
}
