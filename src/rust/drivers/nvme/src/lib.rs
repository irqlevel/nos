#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use kcore::{trace, dma, io, msix, pci, block, sync};
use kcore::bitmap::BitMap;
use kcore::block::{BlockDriver, BlockIo, SubmitError};
use kcore::consts::PAGE_SIZE;
use kcore::once::Once;
use kcore::time::poll_until_busy;

mod spec;
mod queue;

use spec::*;
use queue::{CompletionQueue, SubmissionQueue};

use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU8, Ordering};

const MAX_DEVICES: usize = 8;
static DEVICES: [Once<&'static NvmeDevice>; MAX_DEVICES] = [const { Once::new() }; MAX_DEVICES];
static DEVICE_COUNT: AtomicU32 = AtomicU32::new(0);

/* A note on status[]:
 * The ISR writes status[cid] BEFORE it wakes the waiter, so by the time
 * done[cid].wait() returns the write is visible (the wait group provides
 * the happens-before guarantee).  CID reuse while a command is in flight is
 * impossible because alloc_cid() uses a BitMap — the slot stays allocated
 * until the submitter frees it after its wait. */

/* NVMe PCI class/subclass/prog_if */
const PCI_CLASS_STORAGE:   u8 = 0x01;
const PCI_SUBCLASS_NVME:   u8 = 0x08;
const PCI_PROGIF_NVME:     u8 = 0x02;

/* Number of pages to map for BAR0 (covers regs + doorbells for 2 queues) */
const BAR0_MAP_PAGES: usize = 32;

/* What a command ID in flight is, which is what tells the ISR whom to tell */
const CID_IDLE:  u8 = 0;
const CID_SYNC:  u8 = 1;   /* a task waits on done[cid] */
const CID_ASYNC: u8 = 2;   /* submission.targets[cid] is to be called */

/* ------------------------------------------------------------------ */

/* A controller, for the life of the kernel: the block table is pointed at
 * it, and so is its interrupt handler. Everything in it is shared between
 * the tasks that submit, on any CPU, and the ISR that completes -- so it is
 * registers, atomics, and two locks that own what they guard. */
struct NvmeDevice {
    regs:         io::MmioRegion,
    /* BAR0 mapping, which `regs` is a window of */
    _bar_mapping: dma::PhysMapping,

    /* Admin queues kept to retain their DMA buffers: the controller still
     * knows where they are. */
    _admin: AdminCtx,

    /* The submission side, and what each command in flight is waiting to
     * tell.  Taken with interrupts off: the ISR takes it too. */
    submission: sync::SpinLock<Submission>,

    /* The completion queue: the ISR's, but an interrupt can arrive on
     * another CPU while one is still being handled, so it is taken. */
    completion: sync::SpinLock<CompletionQueue>,

    /* The interrupt, and the table its vector is an entry of.  Kept:
     * dropped, they would take the handler away -- which is what shutdown
     * does with them. */
    irq: sync::SpinLock<Option<Irq>>,

    /* CAP.TO-derived controller enable/disable timeout (ms), as used during
     * init.  Shutdown honours it too. */
    disable_timeout_ms: u64,

    /* capacity and geometry */
    capacity:    u64,    /* total LBA count */
    sector_size: u32,    /* bytes per LBA */
    max_transfer: u32,   /* max sectors per command (2-PRP-entry limit) */

    /* Asynchronous commands that may be in flight at once: the synchronous
     * path keeps SYNC_RESERVED_CIDS of the IDs. */
    async_limit: usize,

    /* What the command with this ID is -- CID_*.  The submitter stores it
     * under the submission lock, before the doorbell (Release); the ISR
     * loads it (Acquire) and clears it. */
    kind: [AtomicU8; IO_QUEUE_DEPTH],

    /* What a synchronous command's submitter waits on, by CID: taken up with
     * add(1) before the doorbell, given back by the ISR. */
    done: Vec<sync::WaitGroup>,

    /* Completion status written by the ISR before it wakes the waiter.
     * 0 = success; non-zero = NVMe status field value (SC | SCT<<8). */
    status: [AtomicU16; IO_QUEUE_DEPTH],
}

/* Dropped in this order: the handler goes before the table its vector is in. */
struct Irq {
    _handler: msix::MsixInterrupt,
    _table:   msix::MsixTable,
}

/* Whom an asynchronous command's completion is for. */
#[derive(Clone, Copy)]
struct AsyncTarget {
    done: extern "C" fn(ctx: *mut u8, status: i32),
    ctx:  usize,
}

struct Submission {
    queue: SubmissionQueue,

    /* Bitmask of in-use CID slots (1 = in use).  Avoids CID reuse while a
     * previous command is still in flight.  W=1 → 64 slots, matches
     * IO_QUEUE_DEPTH=64.  Slots beyond the actual queue depth are permanently
     * reserved at init so in-flight commands never exceed what the SQ can
     * hold (spec full condition is depth-1 entries). */
    cid_map: BitMap<1>,

    /* Commands in the SQ whose doorbell a kick still owes the controller:
     * an asynchronous batch rings it once, at the end. */
    doorbell_owed: bool,

    /* Asynchronous commands in flight. */
    async_in_flight: usize,

    /* The asynchronous path's completion target per CID. */
    targets: [Option<AsyncTarget>; IO_QUEUE_DEPTH],
}

impl Submission {
    /* Allocate a free command ID slot.  None when all are in use. */
    fn alloc_cid(&mut self) -> Option<u16> {
        self.cid_map.alloc().map(|c| c as u16)
    }

    /* Free a command ID slot after its completion has been consumed. */
    fn free_cid(&mut self, cid: u16) {
        self.cid_map.free(cid as usize)
    }
}

/* ------------------------------------------------------------------ */

pub fn init() {
    let count = pci::device_count();
    for i in 0..count {
        if let Some(dev) = pci::get_device(i) {
            if dev.class == PCI_CLASS_STORAGE
                && dev.subclass == PCI_SUBCLASS_NVME
                && dev.prog_if == PCI_PROGIF_NVME
            {
                trace!(0, "found NVMe device {:04x}:{:04x} at {:02x}:{:02x}.{:x}",
                    dev.vendor, dev.device, dev.bus, dev.slot, dev.func);
                init_device(dev);
            }
        }
    }
}

fn init_device(dev: pci::PciDevice) {
    /* Enable bus mastering so the controller can do DMA. */
    dev.enable_bus_mastering();

    /* --- Map BAR0 --- */
    let bar_phys = dev.get_bar64(0);
    if bar_phys == 0 {
        trace!(0, "NVMe: BAR0 is 0, skipping");
        return;
    }
    let bar_mapping = match dma::PhysMapping::map(bar_phys, BAR0_MAP_PAGES) {
        Some(m) => m,
        None => {
            trace!(0, "NVMe: failed to map BAR0 phys={:#x}", bar_phys);
            return;
        }
    };
    let regs = io::MmioRegion::new(bar_mapping.as_mut_ptr(), BAR0_MAP_PAGES * PAGE_SIZE);

    /* --- Read CAP --- */
    let cap = regs.read64(REG_CAP);
    let db_stride = 4 << ((cap >> CAP_DSTRD_SHIFT) & CAP_DSTRD_MASK);
    let mqes = ((cap & CAP_MQES_MASK) + 1) as usize;
    /* CAP.TO = 0 means "no timeout specified" (not "instant") — use minimum. */
    let to_raw = ((cap >> CAP_TO_SHIFT) & CAP_TO_MASK) as u64;
    let to_ms = if to_raw == 0 { MIN_TIMEOUT_MS } else { to_raw * 500 };

    trace!(0, "NVMe CAP: mqes={} dstrd_bytes={} to={}ms",
        mqes, db_stride, to_ms);

    /* The fixed-size BAR0 mapping must cover the doorbells we use (qid 0
     * and 1, SQ+CQ each).  An unusually large CAP.DSTRD would otherwise
     * trip MmioRegion's bounds assert at doorbell-ring time. */
    let max_db_end = DB_BASE + 3 * db_stride as usize + 4;
    if max_db_end > BAR0_MAP_PAGES * PAGE_SIZE {
        trace!(0, "NVMe: doorbell stride {} exceeds BAR0 mapping, skipping", db_stride);
        return;
    }

    /* --- Disable controller --- */
    let cc = regs.read32(REG_CC);
    if cc & CC_EN != 0 {
        regs.write32(REG_CC, cc & !CC_EN);
        if !wait_csts_clear(&regs, CSTS_RDY, to_ms) {
            trace!(0, "NVMe: timeout waiting for controller disable");
            return;
        }
    }

    /* --- Allocate admin queues --- */
    let admin_depth = ADMIN_QUEUE_DEPTH.min(mqes);
    let admin_sq = match SubmissionQueue::new(admin_depth, 0, db_stride as usize) {
        Some(q) => q,
        None => { trace!(0, "NVMe: admin SQ alloc failed"); return; }
    };
    let admin_cq = match CompletionQueue::new(admin_depth, 0, db_stride as usize) {
        Some(q) => q,
        None => { trace!(0, "NVMe: admin CQ alloc failed"); return; }
    };

    /* --- Program AQA, ASQ, ACQ --- */
    let aqa = ((admin_depth as u32 - 1) << 16) | (admin_depth as u32 - 1);
    regs.write32(REG_AQA, aqa);
    regs.write64(REG_ASQ, admin_sq.phys());
    regs.write64(REG_ACQ, admin_cq.phys());

    /* --- Enable controller --- */
    let new_cc = CC_EN | CC_CSS_NVM | CC_MPS_4K | CC_AMS_RR | CC_IOSQES | CC_IOCQES;
    regs.write32(REG_CC, new_cc);
    if !wait_csts_set(&regs, CSTS_RDY, to_ms) {
        trace!(0, "NVMe: timeout waiting for controller ready");
        disable_controller_on_error(&regs, to_ms);
        return;
    }
    if regs.read32(REG_CSTS) & CSTS_FATAL != 0 {
        trace!(0, "NVMe: controller fatal status");
        disable_controller_on_error(&regs, to_ms);
        return;
    }

    trace!(0, "NVMe: controller ready");

    /* Wrap admin queues into a local structure for polling commands. */
    let mut admin = AdminCtx { sq: admin_sq, cq: admin_cq, cmd_id: 0 };

    /* --- Identify Controller --- */
    let id_buf = match dma::DmaBuffer::new(1) {
        Some(b) => b,
        None => { trace!(0, "NVMe: identify DMA alloc failed"); disable_controller_on_error(&regs, to_ms); return; }
    };
    let id_phys = id_buf.phys();

    let mut cmd = SubmissionEntry::new(OPC_IDENTIFY, admin.next_cid());
    cmd.cdw10 = CNS_CONTROLLER;
    cmd.prp1  = id_phys;
    if !admin_exec(&mut admin, &regs, cmd) {
        trace!(0, "NVMe: Identify Controller failed");
        disable_controller_on_error(&regs, to_ms);
        return;
    }

    {
        let id_ctrl = id_buf.as_slice();
        let text = |at: usize, len: usize| {
            core::str::from_utf8(&id_ctrl[at..at + len]).unwrap_or("?").trim()
        };
        trace!(0, "NVMe: ctrl sn={} mn={} mdts={}",
            text(ID_CTRL_SN_AT, ID_CTRL_SN_LEN), text(ID_CTRL_MN_AT, ID_CTRL_MN_LEN),
            id_ctrl[ID_CTRL_MDTS_AT]);
    }

    /* --- Identify Namespace 1 --- */
    let mut cmd = SubmissionEntry::new(OPC_IDENTIFY, admin.next_cid());
    cmd.nsid  = 1;
    cmd.cdw10 = CNS_NAMESPACE;
    cmd.prp1  = id_phys;
    if !admin_exec(&mut admin, &regs, cmd) {
        trace!(0, "NVMe: Identify Namespace 1 failed");
        disable_controller_on_error(&regs, to_ms);
        return;
    }

    let (capacity, lbads) = {
        let id_ns = id_buf.as_slice();
        let mut nsze = [0u8; 8];
        nsze.copy_from_slice(&id_ns[ID_NS_NSZE_AT..ID_NS_NSZE_AT + 8]);

        /* The format in use, of the sixteen the namespace may describe */
        let lbaf_idx = (id_ns[ID_NS_FLBAS_AT] & 0x0F) as usize;
        let lbaf_at = ID_NS_LBAF_AT + lbaf_idx * ID_NS_LBAF_SIZE;
        (u64::from_le_bytes(nsze), id_ns[lbaf_at + LBAF_LBADS_AT])
    };
    /* lbads comes from the device; a value >= 32 would overflow the shift
     * (masked in release builds -> plausible-but-wrong sector size). */
    if lbads >= 32 {
        trace!(0, "NVMe: invalid lbads {}, skipping", lbads);
        disable_controller_on_error(&regs, to_ms);
        return;
    }
    let sector_size: u32 = 1 << lbads;
    trace!(0, "NVMe: ns1 capacity={} sectors sector_size={}", capacity, sector_size);
    if sector_size < 512 || sector_size as usize > PAGE_SIZE {
        trace!(0, "NVMe: unsupported sector size {}, skipping", sector_size);
        disable_controller_on_error(&regs, to_ms);
        return;
    }

    /* Max sectors per command.  The I/O path supplies at most two PRP
     * entries (no PRP lists), so a command may span at most two memory
     * pages.  MDTS is either 0 (unlimited) or at least one page (2^mdts
     * pages, mdts >= 1 -> >= 2 pages), so the 2-page cap is always the
     * binding limit.  Computed in device sectors, not 512-byte units. */
    let max_transfer = (2 * PAGE_SIZE / sector_size as usize) as u32;

    /* --- Allocate I/O queues --- */
    let io_depth = IO_QUEUE_DEPTH.min(mqes);
    let io_sq = match SubmissionQueue::new(io_depth, 1, db_stride as usize) {
        Some(q) => q,
        None => { trace!(0, "NVMe: I/O SQ alloc failed"); disable_controller_on_error(&regs, to_ms); return; }
    };
    let io_cq = match CompletionQueue::new(io_depth, 1, db_stride as usize) {
        Some(q) => q,
        None => { trace!(0, "NVMe: I/O CQ alloc failed"); disable_controller_on_error(&regs, to_ms); return; }
    };

    /* --- Setup MSI-X --- */
    let msix_table = match msix::MsixTable::new(&dev) {
        Some(t) => t,
        None => { trace!(0, "NVMe: MSI-X setup failed"); disable_controller_on_error(&regs, to_ms); return; }
    };

    /* Enable MSI-X in PCI config space BEFORE creating the I/O CQ.
     * QEMU's nvme_create_cq() only calls msix_vector_use() when MSI-X
     * is already enabled; without this, msix_notify() silently drops
     * the interrupt because the vector is not marked as "used". */
    if let Some(cap) = dev.find_capability(0x11) {
        let mc = dev.read_config16(cap as u16 + 2);
        dev.write_config16(cap as u16 + 2, mc | 0x8000);
        trace!(0, "NVMe: pre-enabled MSI-X in PCI config (cap={:#x})", cap);
    }

    /* --- Create I/O CQ (admin command) --- */
    let mut cmd = SubmissionEntry::new(OPC_CREATE_IO_CQ, admin.next_cid());
    cmd.nsid   = 0;
    cmd.prp1   = io_cq.phys();
    cmd.cdw10  = ((io_depth as u32 - 1) << 16) | 1; /* QSIZE | QID=1 */
    cmd.cdw11  = (0u32 << 16) | (CQ_IEN as u32) | (CQ_PC as u32); /* IV=0, IEN, PC */
    if !admin_exec(&mut admin, &regs, cmd) {
        trace!(0, "NVMe: Create I/O CQ failed");
        disable_controller_on_error(&regs, to_ms);
        return;
    }

    /* --- Create I/O SQ (admin command) --- */
    let mut cmd = SubmissionEntry::new(OPC_CREATE_IO_SQ, admin.next_cid());
    cmd.nsid   = 0;
    cmd.prp1   = io_sq.phys();
    cmd.cdw10  = ((io_depth as u32 - 1) << 16) | 1; /* QSIZE | QID=1 */
    cmd.cdw11  = (1u32 << 16) | (SQ_PC as u32);      /* CQID=1, PC */
    if !admin_exec(&mut admin, &regs, cmd) {
        trace!(0, "NVMe: Create I/O SQ failed");
        disable_controller_on_error(&regs, to_ms);
        return;
    }

    let idx = DEVICE_COUNT.load(Ordering::Relaxed);
    if idx as usize >= MAX_DEVICES {
        trace!(0, "NVMe: too many devices (max {})", MAX_DEVICES);
        disable_controller_on_error(&regs, to_ms);
        return;
    }

    let mut done = Vec::new();
    if done.try_reserve_exact(IO_QUEUE_DEPTH).is_err() {
        trace!(0, "NVMe: no memory for the waiters");
        disable_controller_on_error(&regs, to_ms);
        return;
    }
    for _ in 0..IO_QUEUE_DEPTH {
        match sync::WaitGroup::new() {
            Some(wg) => done.push(wg),
            None => {
                trace!(0, "NVMe: wait group alloc failed");
                disable_controller_on_error(&regs, to_ms);
                return;
            }
        }
    }

    let submission = sync::SpinLock::new(Submission {
        queue: io_sq,
        cid_map: {
            let mut m = BitMap::new();
            /* Permanently reserve slots the SQ cannot hold: at most
             * depth-1 commands may be outstanding (NVMe full condition). */
            for b in io_depth.saturating_sub(1)..IO_QUEUE_DEPTH {
                m.set(b);
            }
            m
        },
        doorbell_owed: false,
        async_in_flight: 0,
        targets: [None; IO_QUEUE_DEPTH],
    });
    let (submission, completion, irq) = match (
        submission, sync::SpinLock::new(io_cq), sync::SpinLock::new(None),
    ) {
        (Some(submission), Some(completion), Some(irq)) => (submission, completion, irq),
        _ => {
            trace!(0, "NVMe: spinlock alloc failed");
            disable_controller_on_error(&regs, to_ms);
            return;
        }
    };

    /* For good from here: the interrupt handler is pointed at it, and then
     * the block table is. */
    let device: &'static NvmeDevice = Box::leak(Box::new(NvmeDevice {
        regs,
        _bar_mapping: bar_mapping,
        _admin: admin,
        submission,
        completion,
        irq,
        disable_timeout_ms: to_ms,
        capacity,
        sector_size,
        max_transfer,
        async_limit: (io_depth - 1).saturating_sub(SYNC_RESERVED_CIDS).max(1),
        kind: [const { AtomicU8::new(CID_IDLE) }; IO_QUEUE_DEPTH],
        done,
        status: [const { AtomicU16::new(0) }; IO_QUEUE_DEPTH],
    }));

    /* --- Register MSI-X interrupt handler --- */
    let handler = match msix::MsixInterrupt::register_for(&msix_table, 0, device, NvmeDevice::interrupt) {
        Some(irq) => irq,
        None => {
            trace!(0, "NVMe: MSI-X vector registration failed");
            device.quiesce();
            return;
        }
    };
    trace!(0, "NVMe: MSI-X vector={} registered", handler.vector());
    *device.irq.lock() = Some(Irq { _handler: handler, _table: msix_table });

    /* "nvme0" .. "nvme7".  Init runs single-threaded, so the provisional
     * index is stable; DEVICE_COUNT itself is only incremented after
     * registration succeeds, so it is never inflated by failed
     * initialisations. */
    let mut name = [0u8; 5];
    name[..4].copy_from_slice(b"nvme");
    name[4] = b'0' + idx as u8;
    let name = core::str::from_utf8(&name).unwrap_or("nvme?");

    /* --- Register as block device: a whole disk --- */
    match block::register_driver(name, 0, device) {
        Some(_reg) => {
            /* Registration is permanent; the handle has no Drop. */
            let _ = DEVICES[idx as usize].set(device);
            DEVICE_COUNT.store(idx + 1, Ordering::Release);
            trace!(0, "NVMe: registered as block device, capacity={} sectors", capacity);
        }
        None => {
            trace!(0, "NVMe: block device registration failed");
            device.shutdown();
        }
    }
}

pub fn shutdown() {
    let count = (DEVICE_COUNT.load(Ordering::Relaxed) as usize).min(MAX_DEVICES);
    trace!(0, "NVMe: shutdown count={}", count);
    for slot in DEVICES[..count].iter() {
        if let Some(device) = slot.get() {
            device.shutdown();
        }
    }
    trace!(0, "NVMe: shutdown complete");
}

impl NvmeDevice {
    /* Stop the controller fetching commands and DMAing into the queues. */
    fn quiesce(&self) {
        let cc = self.regs.read32(REG_CC);
        self.regs.write32(REG_CC, cc & !CC_EN);
        if !wait_csts_clear(&self.regs, CSTS_RDY, self.disable_timeout_ms) {
            trace!(0, "NVMe: controller did not go not-ready on shutdown");
        }
    }

    /* Tear down in dependency order:
     * 1. Disable the controller — stops command fetching and DMA into the
     *    queue buffers
     * 2. Unregister the MSI-X handler — stops new interrupt delivery
     * 3. Destroy the MSI-X table — releases PCI vectors
     * The queues and the BAR mapping stay where they are: the device is for
     * good, and nothing is looking at them any more. */
    fn shutdown(&self) {
        self.quiesce();

        /* Taken out under the lock and dropped after it: the drop waits for
         * a handler that may be running. */
        let irq = self.irq.lock().take();
        drop(irq);
    }
}

/* ------------------------------------------------------------------ */
/* Admin command polling helpers                                        */
/* ------------------------------------------------------------------ */

struct AdminCtx {
    sq: SubmissionQueue,
    cq: CompletionQueue,
    cmd_id: u16,
}

impl AdminCtx {
    fn next_cid(&mut self) -> u16 {
        let cid = self.cmd_id;
        self.cmd_id = self.cmd_id.wrapping_add(1);
        cid
    }
}

/* Submit one admin command and poll for its completion.
 * Returns true on success. */
fn admin_exec(ctx: &mut AdminCtx, regs: &io::MmioRegion, cmd: SubmissionEntry) -> bool {
    let cid = (cmd.cdw0 >> 16) as u16;
    ctx.sq.submit(&cmd);
    ctx.sq.ring_doorbell(regs);

    /* Spin-poll the admin CQ.  Global interrupts are disabled during init. */
    let mut result: Option<bool> = None;
    let completed = poll_until_busy(1_000_000, || {
        let _ = regs.read32(REG_VS); /* cheap delay */
        if let Some(cqe) = ctx.cq.poll() {
            ctx.cq.ring_doorbell(regs);
            if cqe.cid != cid {
                trace!(0, "NVMe: admin CQE cid mismatch: got {} expected {}", cqe.cid, cid);
                result = Some(false);
            } else if cqe.status_code() != 0 {
                trace!(0, "NVMe: admin cmd {} status={:#x}", cid, cqe.status_code());
                result = Some(false);
            } else {
                result = Some(true);
            }
            return true; /* stop polling */
        }
        false
    });
    if !completed {
        trace!(0, "NVMe: admin command {} timed out", cid);
        return false;
    }
    result.unwrap_or(false)
}

/* Poll until CSTS bit is set, or timeout.
 * Each iteration is ~1 µs of MMIO read; use 1000 * timeout_ms iterations. */
fn wait_csts_set(regs: &io::MmioRegion, bit: u32, timeout_ms: u64) -> bool {
    poll_until_busy((timeout_ms * 1000) as usize, || regs.read32(REG_CSTS) & bit != 0)
}

/* Poll until CSTS bit is clear, or timeout. */
fn wait_csts_clear(regs: &io::MmioRegion, bit: u32, timeout_ms: u64) -> bool {
    poll_until_busy((timeout_ms * 1000) as usize, || regs.read32(REG_CSTS) & bit == 0)
}

/* Disable the controller and wait for it to go not-ready. Called on init error
 * paths after CC.EN was set: it must run before the admin/IO queue and Identify
 * DMA buffers are freed, otherwise a late/timed-out completion would DMA into
 * memory the page allocator has already reused. */
fn disable_controller_on_error(regs: &io::MmioRegion, to_ms: u64) {
    let cc = regs.read32(REG_CC);
    regs.write32(REG_CC, cc & !CC_EN);
    if !wait_csts_clear(regs, CSTS_RDY, to_ms) {
        trace!(0, "NVMe: controller did not go not-ready on init error");
    }
}

/* ------------------------------------------------------------------ */
/* MSI-X interrupt handler                                             */
/* ------------------------------------------------------------------ */

/* Completions one pass of the handler takes before it acknowledges them; a
 * pass that fills it goes round again. */
const CQE_BATCH: usize = 32;

impl NvmeDevice {
    fn interrupt(&'static self) {
        let mut completed = 0u32;
        loop {
            /* Take what the controller has posted, then acknowledge the lot
             * with one CQ head doorbell. A doorbell is a write across the bus
             * -- under a hypervisor, an exit -- and this paid one per
             * completion. It still has to come before any of these command
             * IDs can go back out: a CID reused while its old entry is
             * unacknowledged is one more entry the controller may need room
             * for, and the queue has room for one per CID and no more. */
            let mut cqes = [(0u16, 0u16); CQE_BATCH];
            let mut n = 0usize;
            {
                let mut cq = self.completion.lock();
                while n < CQE_BATCH {
                    match cq.poll() {
                        Some(cqe) => {
                            cqes[n] = (cqe.cid, cqe.status_code());
                            n += 1;
                        }
                        None => break,
                    }
                }
                if n != 0 {
                    cq.ring_doorbell(&self.regs);
                }
            }
            if n == 0 {
                break;
            }
            completed += n as u32;

            /* Asynchronous completions are collected and their CIDs freed
             * under one lock before their callbacks run -- a callback's owner
             * may want to submit again the moment it hears -- and the
             * callbacks after. */
            let mut finished = [(0usize, 0u16); CQE_BATCH];
            let mut nfinished = 0usize;

            for &(cid, status) in &cqes[..n] {
                let cid = cid as usize % IO_QUEUE_DEPTH;

                /* Acquire pairs with the submitter's Release store */
                match self.kind[cid].swap(CID_IDLE, Ordering::AcqRel) {
                    CID_ASYNC => {
                        finished[nfinished] = (cid, status);
                        nfinished += 1;
                    }
                    CID_SYNC => {
                        /* The status before the wake: the waiter reads it
                         * once its wait returns. */
                        self.status[cid].store(status, Ordering::Relaxed);
                        self.done[cid].done();
                    }
                    _ => {}
                }
            }

            if nfinished == 0 {
                continue;
            }

            let mut calls = [(None::<AsyncTarget>, 0u16); CQE_BATCH];
            {
                let mut submission = self.submission.lock();
                submission.async_in_flight -= nfinished;
                for (call, &(cid, status)) in calls.iter_mut().zip(&finished[..nfinished]) {
                    *call = (submission.targets[cid].take(), status);
                    submission.free_cid(cid as u16);
                }
            }

            for &(target, status) in &calls[..nfinished] {
                if let Some(target) = target {
                    (target.done)(target.ctx as *mut u8, status as i32);
                }
            }
        }
        if completed == 0 {
            /* Not level 0: a shared/stray vector would otherwise spam the log */
            trace!(3, "NVMe: IRQ spurious (no CQEs)");
        }
    }
}

/* ------------------------------------------------------------------ */
/* Asynchronous path: submit, kick                                     */
/* ------------------------------------------------------------------ */

impl NvmeDevice {
    /* The command into the SQ, its doorbell left owed. Never blocks and
     * never waits for a command ID: a full queue is Busy, for the caller to
     * try again after a completion. The completion calls io.done from the
     * ISR above. */
    fn queue_async(&self, io: &BlockIo) -> Result<(), SubmitError> {
        let (opcode, prp1, prp2, cdw12) = match io.op {
            block::IO_READ | block::IO_WRITE => {
                let count = io.count;
                if count == 0
                    || count > self.max_transfer
                    || io.sector >= self.capacity
                    || count as u64 > self.capacity - io.sector
                {
                    return Err(SubmitError::Invalid);
                }

                /* Two PRP entries, no lists: the first may start anywhere
                 * dword aligned, the second is the page after it -- the range
                 * is physically contiguous, the caller's promise. */
                let bytes = count as usize * self.sector_size as usize;
                let offset = io.phys as usize & (PAGE_SIZE - 1);
                if io.phys & 3 != 0 || offset + bytes > 2 * PAGE_SIZE {
                    return Err(SubmitError::Invalid);
                }
                let prp2 = if offset + bytes > PAGE_SIZE {
                    (io.phys & !(PAGE_SIZE as u64 - 1)) + PAGE_SIZE as u64
                } else {
                    0
                };

                let write = io.op == block::IO_WRITE;
                let fua: u32 = if write && io.fua != 0 { 1 << 30 } else { 0 };
                (if write { OPC_WRITE } else { OPC_READ }, io.phys, prp2, fua | (count - 1))
            }
            block::IO_FLUSH => (OPC_FLUSH, 0, 0, 0),
            _ => return Err(SubmitError::Invalid),
        };

        let mut submission = self.submission.lock();
        if submission.async_in_flight >= self.async_limit {
            return Err(SubmitError::Busy);
        }
        let cid = match submission.alloc_cid() {
            Some(c) => c as usize,
            None => return Err(SubmitError::Busy),
        };
        submission.async_in_flight += 1;

        submission.targets[cid] = Some(AsyncTarget { done: io.done, ctx: io.ctx as usize });
        /* Release: the ISR has to see what the command is before the
         * doorbell can bring its completion */
        self.kind[cid].store(CID_ASYNC, Ordering::Release);

        let mut cmd = SubmissionEntry::new(opcode, cid as u16);
        cmd.nsid = 1;
        cmd.prp1 = prp1;
        cmd.prp2 = prp2;
        if opcode != OPC_FLUSH {
            cmd.cdw10 = io.sector as u32;
            cmd.cdw11 = (io.sector >> 32) as u32;
            cmd.cdw12 = cdw12;
        }

        submission.queue.submit(&cmd);
        submission.doorbell_owed = true;
        Ok(())
    }

    /* The doorbell for what queue_async queued without ringing it: the tail
     * as it stands, which covers every one of them. Under the submission
     * lock, as every doorbell write is -- the tail must never be written
     * going backwards. */
    fn ring_owed(&self) {
        let mut submission = self.submission.lock();
        if submission.doorbell_owed {
            submission.queue.ring_doorbell(&self.regs);
            submission.doorbell_owed = false;
        }
    }
}

/* ------------------------------------------------------------------ */
/* I/O path: read, write, flush                                        */
/* ------------------------------------------------------------------ */

/* When every command ID is in flight: how long to wait for one, a little at
 * a time. A second is far past any completion; past it the I/O fails as it
 * always did -- which the panic path, where nothing completes, needs. */
const CID_WAIT_NS: u64 = 50_000;

/* Command IDs the asynchronous path leaves to the synchronous one. A server
 * keeping the queue full -- netblk at a high window -- would otherwise have
 * every ID back the moment it came free, and a filesystem's synchronous I/O
 * on the same disk would wait out lock_with_cid's retries and fail. */
const SYNC_RESERVED_CIDS: usize = 8;
const CID_WAIT_TRIES: u32 = 20_000;

impl NvmeDevice {
    /* The submission lock, held, and a free command ID under it. Every ID
     * in flight at once -- several tasks' I/O together, a load test's -- is
     * no reason to fail a synchronous read or write: one comes back with the
     * next completion, so wait for it. None only once CID_WAIT_TRIES have
     * gone by. */
    fn lock_with_cid(&self) -> Option<(sync::SpinLockGuard<'_, Submission>, u16)> {
        for _ in 0..CID_WAIT_TRIES {
            let mut submission = self.submission.lock();
            if let Some(cid) = submission.alloc_cid() {
                return Some((submission, cid));
            }
            drop(submission);
            kcore::task::sleep(kcore::time::Duration::from_nanos(CID_WAIT_NS));
        }
        None
    }

    /* One synchronous command: into the queue with its doorbell, and the
     * wait for the ISR to say how it went. `fill` writes what the command
     * is, given its ID. */
    fn execute(&self, what: &str, fill: impl FnOnce(u16) -> SubmissionEntry) -> Option<u16> {
        let cid = {
            let (mut submission, cid) = match self.lock_with_cid() {
                Some(held) => held,
                None => {
                    trace!(0, "NVMe: {}: all CID slots busy", what);
                    return None;
                }
            };

            self.status[cid as usize].store(0, Ordering::Relaxed);
            /* Taken up before the doorbell can bring the completion that
             * gives it back, and the kind with Release for the same reason:
             * the ISR has to find a waiter to wake. */
            self.done[cid as usize].add(1);
            self.kind[cid as usize].store(CID_SYNC, Ordering::Release);

            submission.queue.submit(&fill(cid));
            /* The tail covers any asynchronous command still owed a doorbell */
            submission.queue.ring_doorbell(&self.regs);
            submission.doorbell_owed = false;
            cid
        };

        self.done[cid as usize].wait();

        let status = self.status[cid as usize].load(Ordering::Acquire);
        self.submission.lock().free_cid(cid);
        Some(status)
    }

    fn transfer(&self, sector: u64, buf: *const u8, len: usize, is_write: bool, fua: bool) -> bool {
        let count = len / self.sector_size as usize;
        if count == 0 || count > self.max_transfer as usize {
            return false;
        }

        /* Build PRP entries before taking the lock. */
        let prp1 = dma::virt_to_phys(buf);
        let offset_in_page = prp1 as usize & (PAGE_SIZE - 1);
        if offset_in_page + len > 2 * PAGE_SIZE {
            /* Would span 3+ pages: per spec the controller then treats PRP2 as
             * a PRP *list* pointer and would DMA through garbage addresses. */
            trace!(0, "NVMe: transfer spans >2 pages (offset={} bytes={}), rejecting",
                offset_in_page, len);
            return false;
        }
        let prp2 = if offset_in_page + len > PAGE_SIZE {
            /* The page after: an address, which is all a PRP is */
            dma::virt_to_phys(buf.wrapping_add(PAGE_SIZE - offset_in_page))
        } else {
            0
        };

        let opcode = if is_write { OPC_WRITE } else { OPC_READ };
        let status = self.execute("submit_io", |cid| {
            let mut cmd = SubmissionEntry::new(opcode, cid);
            cmd.nsid  = 1;
            cmd.prp1  = prp1;
            cmd.prp2  = prp2;
            cmd.cdw10 = sector as u32;
            cmd.cdw11 = (sector >> 32) as u32;
            let fua_bit: u32 = if fua { 1 << 30 } else { 0 };
            cmd.cdw12 = fua_bit | (count as u32 - 1);
            cmd
        });

        match status {
            Some(0) => true,
            Some(status) => {
                trace!(0, "NVMe: I/O status={:#x} sector={} count={}", status, sector, count);
                false
            }
            None => false,
        }
    }
}

impl BlockDriver for NvmeDevice {
    const ASYNC: bool = true;

    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn sector_size(&self) -> u64 {
        self.sector_size as u64
    }

    fn read(&'static self, sector: u64, buf: &mut [u8]) -> bool {
        self.transfer(sector, buf.as_ptr(), buf.len(), false, false)
    }

    fn write(&'static self, sector: u64, data: &[u8], fua: bool) -> bool {
        self.transfer(sector, data.as_ptr(), data.len(), true, fua)
    }

    fn flush(&'static self) -> bool {
        let status = self.execute("flush", |cid| {
            let mut cmd = SubmissionEntry::new(OPC_FLUSH, cid);
            cmd.nsid = 1;
            cmd
        });

        match status {
            Some(0) => true,
            Some(status) => {
                trace!(0, "NVMe: flush status={:#x}", status);
                false
            }
            None => false,
        }
    }

    /* A kick rings the doorbell whatever became of this command -- the ones
     * queued before it without one are owed it. */
    fn submit(&'static self, io: &BlockIo, kick: bool) -> Result<(), SubmitError> {
        let result = self.queue_async(io);
        if kick {
            self.ring_owed();
        }
        result
    }

    fn kick(&'static self) {
        self.ring_owed();
    }
}
