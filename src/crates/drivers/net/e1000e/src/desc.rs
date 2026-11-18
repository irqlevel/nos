/* Descriptor rings for the 82574, in the legacy layout.
 *
 * The 82574 has three receive layouts -- legacy, extended (RFCTL.EXTEN) and
 * packet split -- and two transmit ones. This driver uses the legacy layout
 * both ways, the one every part of the e1000 family has: one buffer a
 * descriptor, and a write-back that fills in length and status beside the
 * buffer address rather than overwriting it.
 *
 * Ownership works as on igb (desc.rs there): no OWN bit, but a pair of ring
 * pointers -- the head the chip exposes (RDH/TDH), the tail software moves
 * (RDT/TDT) -- and a Descriptor Done bit the chip writes when it is done
 * with one. The tail may never catch software's own clean pointer, since
 * tail == head would read as an empty ring, so one descriptor of RING_SIZE
 * is always kept back: `desc_unused` enforces it. */

use alloc::vec::Vec;
use kcore::dma::{Descriptor, DmaBuffer, Volatile};
use net::{Frame, TxQueue};

/// Descriptors per ring: Linux's default for the part (E1000_DEFAULT_RXD and
/// E1000_DEFAULT_TXD). 256 * 16 bytes is one page, a multiple of the 128
/// bytes RDLEN and TDLEN must be. At a gigabit, with the receive side polled
/// rather than interrupted while busy, 255 buffers are some 3 ms of
/// small-packet line rate.
pub const RING_SIZE: usize = 256;
pub const DESC_BYTES: usize = 16;
pub const RING_PAGES: usize = (RING_SIZE * DESC_BYTES).div_ceil(4096);

/// Receive buffer size: RCTL.BSIZE 2048 with BSEX clear, which is every byte
/// a frame of the net layer's pool has. With RCTL.LPE off no frame the MAC
/// takes is longer than 1522 bytes, so none spans two descriptors.
pub const RX_BUF_SIZE: usize = 2048;

const _: () = assert!(RING_SIZE * DESC_BYTES <= RING_PAGES * 4096);
const _: () = assert!((RING_SIZE * DESC_BYTES) % 128 == 0);

/* Receive, as software writes it:   d0/d1 = buffer address (lo/hi)
 *                                   d2/d3 = zero
 * Receive, as the chip writes back: d2    = length (15:0), checksum (31:16)
 *                                   d3    = status (7:0), errors (15:8),
 *                                           VLAN (31:16)
 * Transmit, as software writes it:  d0/d1 = buffer address (lo/hi)
 *                                   d2    = length (15:0), CSO (23:16),
 *                                           command (31:24)
 *                                   d3    = status (3:0), CSS, VLAN
 * Transmit, as the chip writes back: d3   = status, of which only DD matters
 *
 * Four words rather than a struct a field a byte, so that every access is a
 * whole volatile word, as on igb. */
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

/* Receive status and errors, as they sit in d3. */
pub const RXD_STAT_DD: u32 = 1 << 0; /* descriptor done */
pub const RXD_STAT_EOP: u32 = 1 << 1; /* end of packet */
const RXD_ERR_SHIFT: u32 = 8;
/* E1000_RXD_ERR_FRAME_ERR_MASK: CRC, symbol, sequence, carrier extension
 * and data errors. The checksum errors (TCPE, IPE) are not among them: the
 * frame is whole, and the stack checks its own checksums. */
pub const RXD_ERR_FRAME_MASK: u32 = ((1 << 0) | (1 << 1) | (1 << 2) | (1 << 4) | (1 << 7)) << RXD_ERR_SHIFT;
const RXD_LEN_MASK: u32 = 0xFFFF;

/* Transmit command bits, as they sit in d2, and the status bit in d3. */
pub const TXD_CMD_EOP: u32 = 1 << 24; /* end of packet */
pub const TXD_CMD_IFCS: u32 = 1 << 25; /* insert the CRC */
pub const TXD_CMD_RS: u32 = 1 << 27; /* report status: write DD back */
pub const TXD_STAT_DD: u32 = 1 << 0;
const TXD_LEN_MASK: u32 = 0xFFFF;

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
/// where there is none. On the heap, for the reason `RxRing::frames` gives.
fn shadow() -> Option<Vec<Option<Frame>>> {
    let mut frames = Vec::new();
    frames.try_reserve_exact(RING_SIZE).ok()?;
    frames.resize_with(RING_SIZE, || None);
    Some(frames)
}

/// The receive ring as anything but the poll may see it: what the chip has
/// written into it, which the state dump reads.
pub struct RxView {
    descs: &'static [Desc],
}

impl RxView {
    /// The status and error word of descriptor `idx`, as it is this instant.
    pub fn status(&self, idx: usize) -> u32 {
        self.descs.get(idx).map_or(0, |d| d.d3.read())
    }
}

pub struct RxRing {
    descs: &'static [Desc],
    pub phys: u64,
    /// The frame in each slot; None where a refill has not happened. On the
    /// heap: inline in the device struct it would be built on the stack
    /// before the move into its box, and a kernel stack is small.
    frames: Vec<Option<Frame>>,
    /// The slot the chip completes next, from software's point of view.
    next_to_clean: usize,
    /// The slot to hand over next.
    next_to_use: usize,
    /// What was last written to RDT: the tail can move without a refill --
    /// an errored frame goes straight back into the ring -- and the chip has
    /// to be told even then.
    rdt_written: u32,
}

impl RxRing {
    pub fn new(dma: DmaBuffer) -> Option<Self> {
        let (descs, phys) = ring(dma)?;
        Some(Self { descs, phys, frames: shadow()?, next_to_clean: 0, next_to_use: 0, rdt_written: 0 })
    }

    pub fn view(&self) -> RxView {
        RxView { descs: self.descs }
    }

    /// Slots that could still be handed to the chip, keeping the gap that
    /// stops the tail from meeting the head.
    pub fn desc_unused(&self) -> usize {
        if self.next_to_clean > self.next_to_use {
            self.next_to_clean - self.next_to_use - 1
        } else {
            RING_SIZE + self.next_to_clean - self.next_to_use - 1
        }
    }

    /// Put a buffer in the next free slot, without moving the tail: the
    /// caller posts a run and publishes it once. None, and the frame back,
    /// when the gap rule leaves no room.
    pub fn post_next(&mut self, frame: Frame) -> Result<usize, Frame> {
        if self.desc_unused() == 0 {
            return Err(frame);
        }

        let idx = self.next_to_use;
        let phys = frame.data_phys();

        /* The status word cleared with the rest, so the Descriptor Done of
         * the slot's last round is not read as a new completion. */
        let d = &self.descs[idx];
        d.d0.write(phys as u32);
        d.d1.write((phys >> 32) as u32);
        d.d2.write(0);
        d.d3.write(0);

        self.frames[idx] = Some(frame);
        self.next_to_use = (idx + 1) % RING_SIZE;
        Ok(idx)
    }

    /// The value for RDT: everything up to, not including, this is the
    /// chip's.
    pub fn tail(&self) -> u32 {
        self.next_to_use as u32
    }

    pub fn needs_tail_write(&self) -> bool {
        self.next_to_use as u32 != self.rdt_written
    }

    pub fn mark_tail_written(&mut self) {
        self.rdt_written = self.next_to_use as u32;
    }

    /// Software's two pointers, and whether the slot it waits on holds a
    /// buffer: what the poll leaves out for the state dump.
    pub fn pointers(&self) -> (u32, u32, bool) {
        (self.next_to_clean as u32, self.next_to_use as u32, self.frames[self.next_to_clean].is_some())
    }

    /// Whether the next slot is done: one read of DMA memory, no register.
    pub fn has_work(&mut self) -> bool {
        let idx = self.next_to_clean;
        if self.frames[idx].is_none() {
            /* A refill that failed left the slot empty: the chip cannot pass
             * it, so it is work -- to repost -- not something to wait for. */
            return true;
        }
        self.descs[idx].d3.read() & RXD_STAT_DD != 0
    }

    /// Take the completed frame at the clean pointer: the frame, its status
    /// and error word, and its length in bytes.
    pub fn harvest(&mut self) -> Option<(Frame, u32, usize)> {
        let idx = self.next_to_clean;
        if self.frames[idx].is_none() {
            return None;
        }

        let d = &self.descs[idx];
        let status = d.d3.read();
        if status & RXD_STAT_DD == 0 {
            return None;
        }

        /* The length and the payload after the status that says they are
         * there: nothing orders the loads on arm64 but a barrier. */
        kcore::barrier::dma_rmb();

        let len = (d.d2.read() & RXD_LEN_MASK) as usize;
        let frame = self.frames[idx].take()?;
        self.next_to_clean = (idx + 1) % RING_SIZE;
        Some((frame, status, len))
    }
}

pub struct TxRing {
    descs: &'static [Desc],
    pub phys: u64,
    frames: Vec<Option<Frame>>,
    /// The slots that asked for a write-back (RS): not every one does.
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

    pub fn can_submit(&self) -> bool {
        (self.next_to_use + 1) % RING_SIZE != self.next_to_clean
    }

    /// Place a frame in the next slot, asking for a write-back when `rs`.
    /// The doorbell is the caller's, once a run is in. A frame there is no
    /// room for, or one longer than a descriptor can say, comes back.
    pub fn submit(&mut self, frame: Frame, rs: bool) -> Result<(), Frame> {
        let len = frame.len();
        if !self.can_submit() || len == 0 || len > TXD_LEN_MASK as usize {
            return Err(frame);
        }

        let idx = self.next_to_use;
        let phys = frame.data_phys();
        let rs_bit = if rs { TXD_CMD_RS } else { 0 };

        let d = &self.descs[idx];
        d.d0.write(phys as u32);
        d.d1.write((phys >> 32) as u32);
        /* One buffer, one whole frame: always the end of the packet, and the
         * chip appends the CRC. No offload, so no checksum fields. */
        d.d2.write(len as u32 | TXD_CMD_EOP | TXD_CMD_IFCS | rs_bit);
        /* Status clear, so the DD this is reaped on is the chip's. */
        d.d3.write(0);

        self.frames[idx] = Some(frame);
        self.rs[idx] = rs;
        self.next_to_use = (idx + 1) % RING_SIZE;
        Ok(())
    }

    /// Ask for a write-back on the descriptor submitted last, if it did not
    /// already: before the doorbell that hands it over.
    pub fn report_last(&mut self) {
        if self.next_to_use == self.next_to_clean {
            return;
        }

        let idx = (self.next_to_use + RING_SIZE - 1) % RING_SIZE;
        if self.rs[idx] {
            return;
        }

        let d = &self.descs[idx];
        d.d2.write(d.d2.read() | TXD_CMD_RS);
        self.rs[idx] = true;
    }

    pub fn tail(&self) -> u32 {
        self.next_to_use as u32
    }

    /// Give back every frame the chip has finished with: called at the head
    /// of flush_tx, never from an interrupt. The chip works through the ring
    /// in order and writes back only where RS asked, so the next such
    /// descriptor done means everything up to it is.
    pub fn reap_completed(&mut self, stack: &mut TxQueue<'_>) -> usize {
        let mut reaped = 0;

        while self.next_to_clean != self.next_to_use {
            let mut watch = self.next_to_clean;
            while watch != self.next_to_use && !self.rs[watch] {
                watch = (watch + 1) % RING_SIZE;
            }

            /* Every run ends with a descriptor that reports (report_last),
             * so one is always ahead of whatever is unreaped. */
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

                /* Back to the pool through the net layer: nothing on this
                 * path frees. */
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
