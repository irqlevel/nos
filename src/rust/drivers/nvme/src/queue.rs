#![allow(dead_code)]

/* NVMe submission/completion queue pair backed by DMA buffers.
 *
 * A `Queue` holds one Submission Queue (SQ) and one Completion Queue (CQ).
 * Both buffers are physically contiguous DMA allocations. */

use kcore::dma::DmaBuffer;
use kcore::io::MmioRegion;
use kcore::consts::PAGE_SIZE;
use crate::spec::{SubmissionEntry, CompletionEntry, SQE_SIZE, CQE_SIZE, DB_BASE};

pub struct Queue {
    sq_dma: DmaBuffer,
    cq_dma: DmaBuffer,
    sq_tail: usize,
    cq_head: usize,
    cq_phase: bool,   /* expected phase bit for next valid CQE */
    depth:    usize,
    qid:      u16,    /* 0 = admin, 1+ = I/O */
    db_stride: usize, /* CAP.DSTRD in bytes (4 << DSTRD) */
}

impl Queue {
    /* Allocate a new queue with `depth` entries.
     * Returns None if DMA allocation fails. */
    pub fn new(depth: usize, qid: u16, db_stride: usize) -> Option<Self> {
        let sq_bytes = depth * SQE_SIZE;
        let cq_bytes = depth * CQE_SIZE;
        let sq_pages = (sq_bytes + PAGE_SIZE - 1) / PAGE_SIZE;
        let cq_pages = (cq_bytes + PAGE_SIZE - 1) / PAGE_SIZE;

        let sq_dma = DmaBuffer::new(sq_pages)?;
        let cq_dma = DmaBuffer::new(cq_pages)?;

        Some(Self {
            sq_dma,
            cq_dma,
            sq_tail: 0,
            cq_head: 0,
            cq_phase: true,
            depth,
            qid,
            db_stride,
        })
    }

    pub fn sq_phys(&self) -> u64 {
        self.sq_dma.phys()
    }

    pub fn cq_phys(&self) -> u64 {
        self.cq_dma.phys()
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    pub fn sq_tail(&self) -> usize {
        self.sq_tail
    }

    /* Write `cmd` into the next SQ tail slot and advance tail.
     * Does NOT ring the doorbell -- caller must call `ring_sq_doorbell`. */
    pub fn submit(&mut self, cmd: &SubmissionEntry) {
        let slot = self.sq_tail % self.depth;
        let dst = unsafe {
            (self.sq_dma.as_mut_slice().as_mut_ptr() as *mut SubmissionEntry).add(slot)
        };
        unsafe { core::ptr::write_volatile(dst, *cmd) };
        self.sq_tail = (self.sq_tail + 1) % self.depth;
    }

    /* Poll for one completed entry.
     * Returns Some(cqe) if a completion is available, None if CQ is empty. */
    pub fn poll_completion(&mut self) -> Option<CompletionEntry> {
        let slot = self.cq_head;
        let src = unsafe {
            (self.cq_dma.as_slice().as_ptr() as *const CompletionEntry).add(slot)
        };
        let cqe = unsafe { core::ptr::read_volatile(src) };
        if cqe.phase() != self.cq_phase {
            return None;
        }
        /* Consume the entry: advance head and flip phase on wrap-around. */
        self.cq_head = self.cq_head + 1;
        if self.cq_head >= self.depth {
            self.cq_head = 0;
            self.cq_phase = !self.cq_phase;
        }
        Some(cqe)
    }

    /* Ring the SQ tail doorbell to notify the controller. */
    pub fn ring_sq_doorbell(&self, regs: &MmioRegion) {
        let off = sq_doorbell_offset(self.qid, self.db_stride);
        regs.write32(off, self.sq_tail as u32);
    }

    /* Ring the CQ head doorbell to return consumed entries to the controller. */
    pub fn ring_cq_doorbell(&self, regs: &MmioRegion) {
        let off = cq_doorbell_offset(self.qid, self.db_stride);
        regs.write32(off, self.cq_head as u32);
    }

    /* Peek at the next CQ entry without consuming it (diagnostic). */
    pub fn peek_cq(&self) -> Option<CompletionEntry> {
        let src = unsafe {
            (self.cq_dma.as_slice().as_ptr() as *const CompletionEntry).add(self.cq_head)
        };
        let cqe = unsafe { core::ptr::read_volatile(src) };
        if cqe.phase() != self.cq_phase {
            None
        } else {
            Some(cqe)
        }
    }
}

/* NVMe doorbell offsets:
 *   SQ tail doorbell: DB_BASE + (2*qid)   * (4 << DSTRD)
 *   CQ head doorbell: DB_BASE + (2*qid+1) * (4 << DSTRD) */
fn sq_doorbell_offset(qid: u16, stride: usize) -> usize {
    DB_BASE + (2 * qid as usize) * stride
}

fn cq_doorbell_offset(qid: u16, stride: usize) -> usize {
    DB_BASE + (2 * qid as usize + 1) * stride
}
