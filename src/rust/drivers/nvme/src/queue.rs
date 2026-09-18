/* NVMe queues, backed by DMA buffers: a submission queue the driver writes
 * commands into and the controller reads, and a completion queue the
 * controller writes and the driver reads. Both buffers are physically
 * contiguous DMA allocations, and every entry crosses through
 * `DmaBuffer::store` / `load` -- volatile, and checked to lie in the buffer. */

use kcore::dma::DmaBuffer;
use kcore::io::MmioRegion;
use kcore::consts::PAGE_SIZE;
use crate::spec::{SubmissionEntry, CompletionEntry, SQE_SIZE, CQE_SIZE, CQE_STATUS_AT, DB_BASE};

fn pages_for(bytes: usize) -> usize {
    (bytes + PAGE_SIZE - 1) / PAGE_SIZE
}

pub struct SubmissionQueue {
    dma: DmaBuffer,
    tail: usize,
    depth: usize,
    qid: u16,          /* 0 = admin, 1+ = I/O */
    db_stride: usize,  /* CAP.DSTRD in bytes (4 << DSTRD) */
}

impl SubmissionQueue {
    /* Allocate a new queue with `depth` entries.
     * Returns None if DMA allocation fails. */
    pub fn new(depth: usize, qid: u16, db_stride: usize) -> Option<Self> {
        if depth == 0 {
            return None;
        }
        Some(Self { dma: DmaBuffer::new(pages_for(depth * SQE_SIZE))?, tail: 0, depth, qid, db_stride })
    }

    pub fn phys(&self) -> u64 {
        self.dma.phys()
    }

    /* Write `cmd` into the next tail slot and advance the tail.
     * Does NOT ring the doorbell -- the caller does, with `ring_doorbell`. */
    pub fn submit(&mut self, cmd: &SubmissionEntry) {
        let slot = self.tail % self.depth;
        self.dma.store(slot * SQE_SIZE, *cmd);
        self.tail = (self.tail + 1) % self.depth;
    }

    /* Ring the tail doorbell to notify the controller. */
    pub fn ring_doorbell(&self, regs: &MmioRegion) {
        /* The SQE stores must be visible to the DEVICE before the doorbell
           write: dma_wmb (dmb oshst on arm64 — an atomic Release fence is
           only dmb ish, whose domain excludes a PCIe master; free on x86). */
        kcore::barrier::dma_wmb();
        regs.write32(DB_BASE + (2 * self.qid as usize) * self.db_stride, self.tail as u32);
    }
}

pub struct CompletionQueue {
    dma: DmaBuffer,
    head: usize,
    phase: bool,       /* expected phase bit for next valid CQE */
    depth: usize,
    qid: u16,
    db_stride: usize,
}

impl CompletionQueue {
    pub fn new(depth: usize, qid: u16, db_stride: usize) -> Option<Self> {
        if depth == 0 {
            return None;
        }
        Some(Self {
            dma: DmaBuffer::new(pages_for(depth * CQE_SIZE))?,
            head: 0,
            phase: true,
            depth,
            qid,
            db_stride,
        })
    }

    pub fn phys(&self) -> u64 {
        self.dma.phys()
    }

    /* Read the CQE at `slot` if the controller has posted it.
     *
     * The status word (which holds the phase bit) is read first, on its
     * own: a single volatile read of the whole 16-byte entry lets the
     * compiler load the dwords in any order, which could pair a valid
     * phase bit with stale cid/status from before the controller's write.
     * The dma_rmb closes the CPU half of the same hazard: a control
     * dependency does not order load->load on arm64, so without it the
     * full-entry read (and any later read of DMA'd data buffers) could be
     * satisfied before the phase-bit load. */
    fn read_cqe(&self, slot: usize) -> Option<CompletionEntry> {
        let at = slot * CQE_SIZE;
        let status: u16 = self.dma.load(at + CQE_STATUS_AT)?;
        if (status & 1 != 0) != self.phase {
            return None;
        }
        kcore::barrier::dma_rmb();
        self.dma.load(at)
    }

    /* Poll for one completed entry.
     * Returns Some(cqe) if a completion is available, None if CQ is empty. */
    pub fn poll(&mut self) -> Option<CompletionEntry> {
        let cqe = self.read_cqe(self.head)?;
        /* Consume the entry: advance head and flip phase on wrap-around. */
        self.head += 1;
        if self.head >= self.depth {
            self.head = 0;
            self.phase = !self.phase;
        }
        Some(cqe)
    }

    /* Ring the head doorbell to return consumed entries to the controller. */
    pub fn ring_doorbell(&self, regs: &MmioRegion) {
        regs.write32(DB_BASE + (2 * self.qid as usize + 1) * self.db_stride, self.head as u32);
    }
}
