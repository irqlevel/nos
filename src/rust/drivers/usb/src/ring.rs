//! The two ring shapes: what software produces for the controller, and what
//! the controller produces for software.

use kcore::barrier::{dma_rmb, dma_wmb};
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;

use crate::regs::*;

/// One Transfer Request Block: 16 bytes, and the unit of everything the
/// controller is told and tells.
#[derive(Clone, Copy, Default)]
pub struct Trb {
    pub param: u64,
    pub status: u32,
    pub control: u32,
}

pub const TRB_SIZE: usize = 16;
const NUM_TRBS: usize = PAGE_SIZE / TRB_SIZE;

/// A producer ring -- the command ring, or an endpoint's transfer ring. One
/// page of TRBs whose last entry is a Link TRB with Toggle Cycle set, so the
/// ring wraps in hardware and the producer cycle state flips on every lap.
pub struct Ring {
    dma: Option<DmaBuffer>,
    enqueue: usize,
    cycle: u8,
}

impl Ring {
    pub const fn new() -> Self {
        Self { dma: None, enqueue: 0, cycle: 1 }
    }

    pub fn init(&mut self) -> bool {
        if self.dma.is_some() {
            return true;
        }

        let mut dma = match DmaBuffer::new(1) {
            Some(dma) => dma,
            None => return false,
        };
        dma.as_mut_slice().fill(0);

        let base = dma.phys();
        self.dma = Some(dma);
        self.enqueue = 0;
        self.cycle = 1;

        /* The trailing Link TRB points back at the head and toggles the
         * producer cycle state. Its own cycle bit stays clear until the
         * producer first wraps onto it, so the controller stops at the head
         * instead. */
        self.write(NUM_TRBS - 1, Trb {
            param: base,
            status: 0,
            control: trb_type_field(TRB_LINK) | TRB_TOGGLE_CYCLE,
        });
        true
    }

    pub fn deinit(&mut self) {
        self.dma = None;
        self.enqueue = 0;
        self.cycle = 1;
    }

    pub fn is_ready(&self) -> bool {
        self.dma.is_some()
    }

    pub fn phys(&self) -> u64 {
        self.dma.as_ref().map_or(0, |dma| dma.phys())
    }

    /// Where the next TRB will land -- what Set TR Dequeue Pointer takes when
    /// recovering an endpoint, so the controller resumes where the producer
    /// actually is.
    pub fn enqueue_phys(&self) -> u64 {
        self.phys() + (self.enqueue * TRB_SIZE) as u64
    }

    /// The Dequeue Cycle State to publish in an endpoint or command ring
    /// pointer.
    pub fn cycle(&self) -> u8 {
        self.cycle
    }

    fn write(&mut self, index: usize, trb: Trb) {
        let dma = match self.dma.as_mut() {
            Some(dma) => dma,
            None => return,
        };
        let at = index * TRB_SIZE;
        let slot = &mut dma.as_mut_slice()[at..at + TRB_SIZE];
        slot[0..8].copy_from_slice(&trb.param.to_le_bytes());
        slot[8..12].copy_from_slice(&trb.status.to_le_bytes());
        slot[12..16].copy_from_slice(&trb.control.to_le_bytes());
    }

    fn write_control(&mut self, index: usize, control: u32) {
        let dma = match self.dma.as_mut() {
            Some(dma) => dma,
            None => return,
        };
        let at = index * TRB_SIZE + 12;
        dma.as_mut_slice()[at..at + 4].copy_from_slice(&control.to_le_bytes());
    }

    /// Append one TRB, supplying the cycle bit. Answers with the physical
    /// address of the slot written -- the completion event reports it back --
    /// or 0 when the ring is not there.
    pub fn push(&mut self, param: u64, status: u32, control: u32) -> u64 {
        if self.dma.is_none() {
            return 0;
        }

        /* No producer/consumer distance check: this driver keeps at most a
         * handful of TRBs in flight against a 255-entry ring. */
        let at = self.enqueue;
        let slot_phys = self.phys() + (at * TRB_SIZE) as u64;

        let mut control = control & !TRB_CYCLE;
        if self.cycle != 0 {
            control |= TRB_CYCLE;
        }

        /* The payload is published before the cycle bit hands the TRB over. */
        self.write(at, Trb { param, status, control: 0 });
        dma_wmb();
        self.write_control(at, control);

        self.enqueue += 1;
        if self.enqueue == NUM_TRBS - 1 {
            let mut link = trb_type_field(TRB_LINK) | TRB_TOGGLE_CYCLE;
            if self.cycle != 0 {
                link |= TRB_CYCLE;
            }

            dma_wmb();
            self.write_control(NUM_TRBS - 1, link);

            self.cycle ^= 1;
            self.enqueue = 0;
        }

        slot_phys
    }
}

/// The consumer ring. A single segment described by a one-entry Event Ring
/// Segment Table; software wraps by hand and toggles its consumer cycle
/// state.
pub struct EventRing {
    segment: Option<DmaBuffer>,
    erst: Option<DmaBuffer>,
    dequeue: usize,
    cycle: u8,
}

impl EventRing {
    pub const fn new() -> Self {
        Self { segment: None, erst: None, dequeue: 0, cycle: 1 }
    }

    pub fn init(&mut self) -> bool {
        if self.segment.is_some() {
            return true;
        }

        let (mut segment, mut erst) = match (DmaBuffer::new(1), DmaBuffer::new(1)) {
            (Some(segment), Some(erst)) => (segment, erst),
            _ => return false,
        };
        segment.as_mut_slice().fill(0);
        erst.as_mut_slice().fill(0);

        /* The one segment table entry: where the ring is and how long. */
        let base = segment.phys();
        let entry = erst.as_mut_slice();
        entry[0..8].copy_from_slice(&base.to_le_bytes());
        entry[8..12].copy_from_slice(&(NUM_TRBS as u32).to_le_bytes());
        entry[12..16].copy_from_slice(&0u32.to_le_bytes());

        self.segment = Some(segment);
        self.erst = Some(erst);
        self.dequeue = 0;
        self.cycle = 1;
        true
    }

    pub fn deinit(&mut self) {
        self.segment = None;
        self.erst = None;
        self.dequeue = 0;
        self.cycle = 1;
    }

    pub fn is_ready(&self) -> bool {
        self.segment.is_some()
    }

    pub fn erst_phys(&self) -> u64 {
        self.erst.as_ref().map_or(0, |erst| erst.phys())
    }

    pub fn dequeue_phys(&self) -> u64 {
        self.segment.as_ref().map_or(0, |seg| seg.phys()) + (self.dequeue * TRB_SIZE) as u64
    }

    /// Consume one event. None when the ring is empty.
    pub fn pop(&mut self) -> Option<Trb> {
        let segment = self.segment.as_ref()?;
        let at = self.dequeue * TRB_SIZE;
        let slot = &segment.as_slice()[at..at + TRB_SIZE];

        let control = u32::from_le_bytes(slot[12..16].try_into().unwrap());
        if control & TRB_CYCLE != self.cycle as u32 {
            return None;
        }

        /* The cycle bit is the ownership handshake; the payload reads below
         * must not be hoisted above it. A control dependency is not
         * ordering. */
        dma_rmb();

        let trb = Trb {
            param: u64::from_le_bytes(slot[0..8].try_into().unwrap()),
            status: u32::from_le_bytes(slot[8..12].try_into().unwrap()),
            control,
        };

        self.dequeue += 1;
        if self.dequeue == NUM_TRBS {
            self.dequeue = 0;
            self.cycle ^= 1;
        }

        Some(trb)
    }
}
