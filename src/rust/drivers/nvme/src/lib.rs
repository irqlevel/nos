#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use kcore::{trace, dma, io, msix, pci, block, sync};
use kcore::bitmap::BitMap;
use kcore::consts::PAGE_SIZE;
use kcore::time::poll_until_busy;

mod spec;
mod queue;

use spec::*;
use queue::Queue;

use core::sync::atomic::{AtomicPtr, AtomicU16, AtomicU32, AtomicUsize, Ordering};

const MAX_DEVICES: usize = 8;
static DEVICES: [AtomicPtr<NvmeDevice>; MAX_DEVICES] = {
    const NULL: AtomicPtr<NvmeDevice> = AtomicPtr::new(core::ptr::null_mut());
    [NULL; MAX_DEVICES]
};
static DEVICE_COUNT: AtomicU32 = AtomicU32::new(0);

/* A note on inflight_status[]:
 * The ISR writes inflight_status[cid] BEFORE calling wg_done, so by the
 * time completion.wait() returns the write is visible (wg_done/wg_wait
 * provide the happens-before guarantee).  CID reuse while a command is
 * in flight is impossible because alloc_cid() uses a BitMap — the slot
 * stays allocated until the submitter calls free_cid() after wait(). */

/* NVMe PCI class/subclass/prog_if */
const PCI_CLASS_STORAGE:   u8 = 0x01;
const PCI_SUBCLASS_NVME:   u8 = 0x08;
const PCI_PROGIF_NVME:     u8 = 0x02;

/* Number of pages to map for BAR0 (covers regs + doorbells for 2 queues) */
const BAR0_MAP_PAGES: usize = 32;

/* ------------------------------------------------------------------ */

#[allow(dead_code)]
struct NvmeDevice {
    /* BAR0 mapping — must outlive `regs` */
    _bar_mapping: dma::PhysMapping,
    regs:         io::MmioRegion,

    /* Admin queues kept alive to retain DMA buffers */
    admin_sq:  Queue,
    admin_cq:  Queue,
    io_sq:     Queue,
    io_cq:     Queue,

    io_lock: sync::SpinLock,

    _msix_table: msix::MsixTable,
    _msix_irq:   msix::MsixInterrupt,

    db_stride:   usize,
    /* Bitmask of in-use CID slots (1 = in use).  Replaces the linear
     * next_cid counter — avoids CID reuse while a previous command is still
     * in flight.  W=1 → 64 slots, matches IO_QUEUE_DEPTH=64. */
    cid_map: BitMap<1>,

    /* capacity and geometry */
    capacity:    u64,    /* total LBA count */
    sector_size: u32,    /* bytes per LBA */
    max_transfer: u32,   /* max sectors per command (2-page limit) */

    /* In-flight WaitGroup handles indexed by CID.  0 = empty slot.
     * Accessed from both the I/O submission path and the ISR. */
    inflight: [AtomicUsize; IO_QUEUE_DEPTH],

    /* Completion status written by the ISR before calling wg_done.
     * 0 = success; non-zero = NVMe status field value (SC | SCT<<8).
     * Accessed from both the I/O submission path and the ISR. */
    inflight_status: [AtomicU16; IO_QUEUE_DEPTH],

    /* device name for block registration */
    name_buf: [u8; 16],
}

impl Drop for NvmeDevice {
    fn drop(&mut self) {
        /* Tear down in reverse dependency order:
         * 1. Unregister MSI-X ISR — stops new interrupt delivery
         * 2. Destroy MSI-X table — releases PCI vectors
         * 3. Destroy spinlock — after no more concurrent access
         * Remaining fields (queues, BAR mapping) drop implicitly after. */
        self._msix_irq.disarm();
        self._msix_table.disarm();
        /* io_lock is dropped implicitly after this (SpinLock::drop calls destroy) */
    }
}

struct BufWriter<'a> { buf: &'a mut [u8], pos: usize }

impl<'a> core::fmt::Write for BufWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buf.len().saturating_sub(self.pos + 1);
        let n = bytes.len().min(remaining);
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
        Ok(())
    }
}

fn write_name(buf: &mut [u8; 16], idx: u32) -> core::fmt::Result {
    use core::fmt::Write;
    let mut w = BufWriter { buf, pos: 0 };
    write!(w, "nvme{}", idx)?;
    w.buf[w.pos] = 0;
    Ok(())
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
    let admin_sq = match Queue::new(admin_depth, 0, db_stride as usize) {
        Some(q) => q,
        None => { trace!(0, "NVMe: admin SQ alloc failed"); return; }
    };
    let admin_cq = match Queue::new(admin_depth, 0, db_stride as usize) {
        Some(q) => q,
        None => { trace!(0, "NVMe: admin CQ alloc failed"); return; }
    };

    /* --- Program AQA, ASQ, ACQ --- */
    let aqa = ((admin_depth as u32 - 1) << 16) | (admin_depth as u32 - 1);
    regs.write32(REG_AQA, aqa);
    regs.write64(REG_ASQ, admin_sq.sq_phys());
    regs.write64(REG_ACQ, admin_cq.cq_phys());

    /* --- Enable controller --- */
    let new_cc = CC_EN | CC_CSS_NVM | CC_MPS_4K | CC_AMS_RR | CC_IOSQES | CC_IOCQES;
    regs.write32(REG_CC, new_cc);
    if !wait_csts_set(&regs, CSTS_RDY, to_ms) {
        trace!(0, "NVMe: timeout waiting for controller ready");
        return;
    }
    if regs.read32(REG_CSTS) & CSTS_FATAL != 0 {
        trace!(0, "NVMe: controller fatal status");
        return;
    }

    trace!(0, "NVMe: controller ready");

    /* Wrap admin queues into a local structure for polling commands. */
    let mut admin = AdminCtx { sq: admin_sq, cq: admin_cq, cmd_id: 0 };

    /* --- Identify Controller --- */
    let id_buf = match dma::DmaBuffer::new(1) {
        Some(b) => b,
        None => { trace!(0, "NVMe: identify DMA alloc failed"); return; }
    };
    let id_phys = id_buf.phys();

    let mut cmd = SubmissionEntry::new(OPC_IDENTIFY, admin.next_cid());
    cmd.cdw10 = CNS_CONTROLLER;
    cmd.prp1  = id_phys;
    if !admin_exec(&mut admin, &regs, cmd) {
        trace!(0, "NVMe: Identify Controller failed");
        return;
    }

    let id_ctrl = unsafe { &*(id_buf.as_slice().as_ptr() as *const IdentifyController) };
    let mdts = id_ctrl.mdts;
    let max_transfer = if mdts == 0 {
        (2 * PAGE_SIZE / 512) as u32  /* 2-page default */
    } else {
        let pages = 1usize << (mdts as usize);
        ((pages * PAGE_SIZE) / 512).min((2 * PAGE_SIZE) / 512) as u32
    };

    let sn = core::str::from_utf8(&id_ctrl.sn).unwrap_or("?").trim();
    let mn = core::str::from_utf8(&id_ctrl.mn).unwrap_or("?").trim();
    trace!(0, "NVMe: ctrl sn={} mn={} mdts={}", sn, mn, mdts);

    /* --- Identify Namespace 1 --- */
    let mut cmd = SubmissionEntry::new(OPC_IDENTIFY, admin.next_cid());
    cmd.nsid  = 1;
    cmd.cdw10 = CNS_NAMESPACE;
    cmd.prp1  = id_phys;
    if !admin_exec(&mut admin, &regs, cmd) {
        trace!(0, "NVMe: Identify Namespace 1 failed");
        return;
    }

    let id_ns = unsafe { &*(id_buf.as_slice().as_ptr() as *const IdentifyNamespace) };
    let capacity = id_ns.nsze;
    let lbaf_idx = (id_ns.flbas & 0x0F) as usize;
    /* LBA format entries start at offset 128 in IdentifyNamespace. */
    let lbaf_ptr = unsafe {
        (id_ns as *const IdentifyNamespace as *const u8).add(128) as *const LbaFormat
    };
    let lbaf = unsafe { &*lbaf_ptr.add(lbaf_idx) };
    let sector_size: u32 = 1 << lbaf.lbads;
    trace!(0, "NVMe: ns1 capacity={} sectors sector_size={}", capacity, sector_size);

    /* --- Allocate I/O queues --- */
    let io_depth = IO_QUEUE_DEPTH.min(mqes);
    let io_sq = match Queue::new(io_depth, 1, db_stride as usize) {
        Some(q) => q,
        None => { trace!(0, "NVMe: I/O SQ alloc failed"); return; }
    };
    let io_cq = match Queue::new(io_depth, 1, db_stride as usize) {
        Some(q) => q,
        None => { trace!(0, "NVMe: I/O CQ alloc failed"); return; }
    };

    let io_lock = match sync::SpinLock::new() {
        Some(l) => l,
        None => { trace!(0, "NVMe: spinlock alloc failed"); return; }
    };

    /* --- Setup MSI-X --- */
    let msix_table = match msix::MsixTable::new(&dev) {
        Some(t) => t,
        None => { trace!(0, "NVMe: MSI-X setup failed"); return; }
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
    cmd.prp1   = io_cq.cq_phys();
    cmd.cdw10  = ((io_depth as u32 - 1) << 16) | 1; /* QSIZE | QID=1 */
    cmd.cdw11  = (0u32 << 16) | (CQ_IEN as u32) | (CQ_PC as u32); /* IV=0, IEN, PC */
    if !admin_exec(&mut admin, &regs, cmd) {
        trace!(0, "NVMe: Create I/O CQ failed");
        return;
    }

    /* --- Create I/O SQ (admin command) --- */
    let mut cmd = SubmissionEntry::new(OPC_CREATE_IO_SQ, admin.next_cid());
    cmd.nsid   = 0;
    cmd.prp1   = io_sq.sq_phys();
    cmd.cdw10  = ((io_depth as u32 - 1) << 16) | 1; /* QSIZE | QID=1 */
    cmd.cdw11  = (1u32 << 16) | (SQ_PC as u32);      /* CQID=1, PC */
    if !admin_exec(&mut admin, &regs, cmd) {
        trace!(0, "NVMe: Create I/O SQ failed");
        return;
    }

    /* Build the NvmeDevice.  We need the pointer *before* registering MSI-X
     * because the interrupt handler closure needs it.  We box it, get a raw
     * pointer, then store the Box inside the struct after the fact. */
    let mut dev_box = Box::new(NvmeDevice {
        _bar_mapping: bar_mapping,
        regs,
        admin_sq: admin.sq,
        admin_cq: admin.cq,
        io_sq,
        io_cq,
        io_lock,
        _msix_table: msix_table,
        _msix_irq: msix::MsixInterrupt::empty(),
        db_stride: db_stride as usize,
        cid_map: BitMap::new(),
        capacity,
        sector_size,
        max_transfer,
        inflight: {
            const ZERO: AtomicUsize = AtomicUsize::new(0);
            [ZERO; IO_QUEUE_DEPTH]
        },
        inflight_status: {
            const ZERO: AtomicU16 = AtomicU16::new(0);
            [ZERO; IO_QUEUE_DEPTH]
        },
        name_buf: [0u8; 16],
    });

    /* --- Register MSI-X interrupt handler --- */
    let ctx_ptr = dev_box.as_mut() as *mut NvmeDevice as *mut u8;
    let msix_irq = match msix::MsixInterrupt::register(
        &dev_box._msix_table, 0, nvme_msix_handler, ctx_ptr,
    ) {
        Some(irq) => irq,
        None => {
            trace!(0, "NVMe: MSI-X vector registration failed");
            return;
        }
    };
    trace!(0, "NVMe: MSI-X vector={} registered", msix_irq.vector());
    dev_box._msix_irq = msix_irq;

    let idx = DEVICE_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx as usize >= MAX_DEVICES {
        trace!(0, "NVMe: too many devices (max {})", MAX_DEVICES);
        return;
    }

    /* Build device name: "nvme0\0" .. "nvme7\0" */
    let _ = write_name(&mut dev_box.name_buf, idx);

    let raw = Box::into_raw(dev_box);

    /* --- Register as block device --- */
    let ops = block::BlockDeviceOps {
        name:         unsafe { (*raw).name_buf.as_ptr() },
        capacity:     unsafe { (*raw).capacity },
        sector_size:  unsafe { (*raw).sector_size as u64 },
        read_sectors:  nvme_read_sectors,
        write_sectors: nvme_write_sectors,
        flush:         Some(nvme_flush),
        ctx:           raw as *mut u8,
    };

    match block::register(&ops) {
        Some(_reg) => {
            core::mem::forget(_reg);
            DEVICES[idx as usize].store(raw, Ordering::Release);
            trace!(0, "NVMe: registered as block device, capacity={} sectors", unsafe { (*raw).capacity });
        }
        None => {
            trace!(0, "NVMe: block device registration failed");
            unsafe { drop(Box::from_raw(raw)) };
        }
    }
}

pub fn shutdown() {
    let count = (DEVICE_COUNT.load(Ordering::Relaxed) as usize).min(MAX_DEVICES);
    trace!(0, "NVMe: shutdown count={}", count);
    for i in 0..count {
        let raw = DEVICES[i].swap(core::ptr::null_mut(), Ordering::AcqRel);
        trace!(0, "NVMe: shutdown slot={} raw={:#x}", i, raw as usize);
        if !raw.is_null() {
            unsafe { drop(Box::from_raw(raw)) };
        }
    }
    trace!(0, "NVMe: shutdown complete");
}

/* ------------------------------------------------------------------ */
/* Admin command polling helpers                                        */
/* ------------------------------------------------------------------ */

struct AdminCtx {
    sq: Queue,
    cq: Queue,
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
    ctx.sq.ring_sq_doorbell(regs);

    /* Spin-poll the admin CQ.  Global interrupts are disabled during init. */
    let mut result: Option<bool> = None;
    let completed = poll_until_busy(1_000_000, || {
        let _ = regs.read32(REG_VS); /* cheap delay */
        if let Some(cqe) = ctx.cq.poll_completion() {
            ctx.cq.ring_cq_doorbell(regs);
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

/* ------------------------------------------------------------------ */
/* MSI-X interrupt handler                                             */
/* ------------------------------------------------------------------ */

extern "C" fn nvme_msix_handler(ctx: *mut u8) {
    let dev = ctx as *mut NvmeDevice;

    let mut completed = 0u32;
    loop {
        let cqe = match unsafe { (*dev).io_cq.poll_completion() } {
            Some(c) => c,
            None => break,
        };
        unsafe { (*dev).io_cq.ring_cq_doorbell(&(*dev).regs) };

        let cid = cqe.cid as usize % IO_QUEUE_DEPTH;
        let wg_handle = unsafe { (*dev).inflight[cid].load(Ordering::Relaxed) };
        completed = completed + 1;
        if wg_handle != 0 {
            unsafe { (*dev).inflight_status[cid].store(cqe.status_code(), Ordering::Relaxed) };
            unsafe { (*dev).inflight[cid].store(0, Ordering::Release) };
            unsafe { ffi::sync::kernel_waitgroup_done(wg_handle) };
        }
    }
    if completed == 0 {
        trace!(0, "NVMe: IRQ spurious (no CQEs)");
    }
}

/* ------------------------------------------------------------------ */
/* I/O path: read_sectors, write_sectors, flush                        */
/* ------------------------------------------------------------------ */

extern "C" fn nvme_read_sectors(
    ctx: *mut u8, sector: u64, buf: *mut u8, count: u32,
) -> i32 {
    submit_io(ctx, sector, buf as *const u8, count, false, false)
}

extern "C" fn nvme_write_sectors(
    ctx: *mut u8, sector: u64, buf: *const u8, count: u32, _fua: i32,
) -> i32 {
    submit_io(ctx, sector, buf, count, true, _fua != 0)
}

extern "C" fn nvme_flush(ctx: *mut u8) -> i32 {
    let dev = ctx as *mut NvmeDevice;

    let completion = match sync::Completion::new() {
        Some(c) => c,
        None => return -1,
    };

    let cid = {
        let _guard = unsafe { (*dev).io_lock.lock() };
        let cid = match alloc_cid(dev) {
            Some(c) => c,
            None => {
                trace!(0, "NVMe: flush: all CID slots busy");
                return -1;
            }
        };

        unsafe { (*dev).inflight_status[cid as usize].store(0, Ordering::Relaxed) };
        unsafe { (*dev).inflight[cid as usize].store(completion.raw_handle(), Ordering::Relaxed) };
        let mut cmd = SubmissionEntry::new(OPC_FLUSH, cid);
        cmd.nsid = 1;
        unsafe { (*dev).io_sq.submit(&cmd) };
        unsafe { (*dev).io_sq.ring_sq_doorbell(&(*dev).regs) };
        cid
    };

    completion.wait();

    let status = unsafe { (*dev).inflight_status[cid as usize].load(Ordering::Acquire) };
    { let _guard = unsafe { (*dev).io_lock.lock() }; free_cid(dev, cid); }

    if status != 0 {
        trace!(0, "NVMe: flush status={:#x}", status);
        return -1;
    }
    0
}

fn submit_io(
    ctx: *mut u8,
    sector: u64,
    buf: *const u8,
    count: u32,
    is_write: bool,
    _fua: bool,
) -> i32 {
    let dev = ctx as *mut NvmeDevice;

    if count == 0 || count > unsafe { (*dev).max_transfer } {
        return -1;
    }

    let completion = match sync::Completion::new() {
        Some(c) => c,
        None => return -1,
    };

    /* Build PRP entries before taking the lock. */
    let prp1 = dma::virt_to_phys(buf as *const u8);
    let offset_in_page = prp1 as usize & (PAGE_SIZE - 1);
    let bytes_needed = count as usize * unsafe { (*dev).sector_size } as usize;
    let prp2 = if offset_in_page + bytes_needed > PAGE_SIZE {
        let next_page_virt = unsafe { buf.add(PAGE_SIZE - offset_in_page) };
        dma::virt_to_phys(next_page_virt as *const u8)
    } else {
        0
    };

    let opcode = if is_write { OPC_WRITE } else { OPC_READ };

    let cid = {
        let _guard = unsafe { (*dev).io_lock.lock() };
        let cid = match alloc_cid(dev) {
            Some(c) => c,
            None => {
                trace!(0, "NVMe: submit_io: all CID slots busy");
                return -1;
            }
        };

        unsafe { (*dev).inflight_status[cid as usize].store(0, Ordering::Relaxed) };
        unsafe { (*dev).inflight[cid as usize].store(completion.raw_handle(), Ordering::Relaxed) };

        let mut cmd = SubmissionEntry::new(opcode, cid);
        cmd.nsid  = 1;
        cmd.prp1  = prp1;
        cmd.prp2  = prp2;
        cmd.cdw10 = sector as u32;
        cmd.cdw11 = (sector >> 32) as u32;
        let fua_bit: u32 = if _fua { 1 << 30 } else { 0 };
        cmd.cdw12 = fua_bit | (count as u32 - 1);

        unsafe { (*dev).io_sq.submit(&cmd) };
        unsafe { (*dev).io_sq.ring_sq_doorbell(&(*dev).regs) };
        cid
    };

    completion.wait();

    let status = unsafe { (*dev).inflight_status[cid as usize].load(Ordering::Acquire) };
    { let _guard = unsafe { (*dev).io_lock.lock() }; free_cid(dev, cid); }

    if status != 0 {
        trace!(0, "NVMe: I/O status={:#x} sector={} count={}", status, sector, count);
        return -1;
    }
    0
}

/* Allocate a free command ID slot.  Returns None when all IO_QUEUE_DEPTH
 * slots are in use.  Caller must hold io_lock. */
fn alloc_cid(dev: *mut NvmeDevice) -> Option<u16> {
    unsafe { (*dev).cid_map.alloc().map(|c| c as u16) }
}

/* Free a command ID slot after its completion has been consumed.
 * Caller must hold io_lock. */
fn free_cid(dev: *mut NvmeDevice, cid: u16) {
    unsafe { (*dev).cid_map.free(cid as usize) }
}


/* ------------------------------------------------------------------ */
/* FFI glue                                                             */
/* ------------------------------------------------------------------ */
mod ffi {
    pub mod sync {
        extern "C" {
            pub fn kernel_waitgroup_done(handle: usize);
        }
    }
}
