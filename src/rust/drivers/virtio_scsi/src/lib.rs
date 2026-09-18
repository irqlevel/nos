//! virtio-scsi: a SCSI host adapter, and the disks behind it.
//!
//! One adapter serves many logical units, and this driver registers each one
//! that says it is a disk as a block device of its own -- sda, sdb and so
//! on. They share the adapter's request queue and its pool of slots, because
//! that is what the hardware shares.
//!
//! Only the request queue is set up. The control queue is for task
//! management -- aborts and resets, which nothing here asks for -- and the
//! event queue for hotplug and live resize, which nothing here listens to.
//!
//! Every caller blocks until its own command is answered, so the shape is
//! the one virtio-blk has: a slot carries the request header, the response
//! and something to wait on, and both the probe (INQUIRY, READ CAPACITY) and
//! the block layer's reads and writes go through the same one call.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use kcore::block;
use kcore::consts::PAGE_SIZE;
use kcore::dma::{self, DmaBuffer};
use kcore::interrupt::LegacyInterrupt;
use kcore::msix::MsixInterrupt;
use kcore::pci;
use kcore::sync::{SpinLock, WaitGroup};
use kcore::trace;
use virtio::mmio::{MmioTransport, Slot};
use virtio::{Buf, Queue, Transport};

/// Adapters, and disks across all of them: sda … sdh.
const MAX_ADAPTERS: usize = 4;
const MAX_DISKS: usize = 8;

/// Commands in flight on one adapter.
const MAX_SLOTS: usize = 16;

/// The queue the commands go on. 0 is task management and 1 is events;
/// neither is set up.
const REQUEST_QUEUE: u16 = 2;

/* The device's configuration, at these offsets (virtio spec 5.6.4) */
const CFG_SENSE_SIZE: usize = 20;
const CFG_CDB_SIZE: usize = 24;
const CFG_MAX_TARGET: usize = 30;

/* The request header is 19 bytes plus the CDB, the response 12 plus the
 * sense data; both sizes are the device's to say. */
const REQ_FIXED: usize = 19;
const RESP_FIXED: usize = 12;
/// Where the CDB sits in the request header
const REQ_CDB_AT: usize = 19;
const REQ_LUN_AT: usize = 0;

/* What a response says: the device's own verdict, then the SCSI one. */
const RESP_STATUS_AT: usize = 10;
const RESP_RESPONSE_AT: usize = 11;
const RESPONSE_OK: u8 = 0;
const STATUS_GOOD: u8 = 0;

/* The commands this driver sends */
const OP_TEST_UNIT_READY: u8 = 0x00;
const OP_INQUIRY: u8 = 0x12;
const OP_READ_CAPACITY: u8 = 0x25;
const OP_READ10: u8 = 0x28;
const OP_WRITE10: u8 = 0x2A;
const OP_SYNC_CACHE: u8 = 0x35;

/// INQUIRY: a direct-access block device, connected.
const INQUIRY_LEN: usize = 36;
const TYPE_DIRECT_ACCESS: u8 = 0;
const READ_CAPACITY_LEN: usize = 8;

/// The fallback where READ CAPACITY says nothing sensible.
const DEFAULT_SECTOR_SIZE: u64 = 512;

const NO_SLOT: u8 = 0xFF;

static ADAPTERS: AtomicUsize = AtomicUsize::new(0);
static DISKS: AtomicUsize = AtomicUsize::new(0);

/// Nothing to set up: the driver is called from the boot path by the names
/// at the bottom, and this is what keeps them in the archive.
pub fn init() {}

/// Which way the data goes, if there is any.
#[derive(Clone, Copy, PartialEq)]
enum Data {
    None,
    /// The device writes it: a read, an INQUIRY
    In,
    /// The device reads it: a write
    Out,
}

struct Hba {
    transport: Box<dyn Transport>,
    /// 19 + cdb_size, and 12 + sense_size: what the device asks for
    req_size: usize,
    resp_size: usize,
    cdb_size: usize,
    max_target: u16,
    msix: bool,

    /// One page of request headers and responses, a slot's worth apiece
    dma: DmaBuffer,
    free: AtomicU32,
    done: Vec<WaitGroup>,
    complete: Vec<AtomicBool>,

    /// The queue and what is on it. Taken with interrupts off: completions
    /// arrive in interrupt context.
    lock: SpinLock,
    inner: UnsafeCell<Inner>,

    _irq: Irq,
}

struct Inner {
    queue: Queue,
    slot_of_head: [u8; virtio::MAX_DESCRIPTORS as usize],
}

enum Irq {
    Msix(MsixInterrupt),
    Legacy(LegacyInterrupt),
    None,
}

/* Everything inside is atomic or taken under the lock, and an adapter is
 * registered for the life of the kernel. */
unsafe impl Sync for Hba {}
unsafe impl Send for Hba {}

/// One logical unit: a disk the block layer knows by name.
struct Disk {
    hba: *const Hba,
    target: u8,
    lun: u16,
    capacity: u64,
    sector_size: u64,
    name: [u8; 8],
}

unsafe impl Sync for Disk {}
unsafe impl Send for Disk {}

impl Hba {
    fn slot_stride(&self) -> usize {
        /* The request and the response of one slot, kept eight-aligned so a
         * header never straddles what the device reads. */
        (self.req_size + self.resp_size + 7) & !7
    }

    fn req_offset(&self, slot: usize) -> usize {
        slot * self.slot_stride()
    }

    fn resp_offset(&self, slot: usize) -> usize {
        self.req_offset(slot) + self.req_size
    }

    fn take_slot(&self) -> Option<usize> {
        loop {
            let free = self.free.load(Ordering::Acquire);
            if free == 0 {
                return None;
            }
            let slot = free.trailing_zeros() as usize;
            if self
                .free
                .compare_exchange(free, free & !(1 << slot), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(slot);
            }
        }
    }

    fn wait_for_slot(&self) -> usize {
        loop {
            if let Some(slot) = self.take_slot() {
                return slot;
            }
            if block::interrupts_started() {
                kcore::task::yield_to_runnable();
            } else {
                self.collect();
            }
        }
    }

    /// Send one SCSI command to a logical unit and wait for its answer.
    /// `data` is physical, and whichever way it goes the device's rules are
    /// kept: everything it reads comes before anything it writes.
    fn command(
        &self, target: u8, lun: u16, cdb: &[u8], data: Option<(u64, u32)>, dir: Data,
    ) -> bool {
        let slot = self.wait_for_slot();
        let base = self.dma.as_ptr() as *mut u8;
        let req = self.req_offset(slot);
        let resp = self.resp_offset(slot);

        unsafe {
            core::ptr::write_bytes(base.add(req), 0, self.req_size + self.resp_size);

            /* The SAM single-level LUN the spec asks for */
            let lun_at = base.add(req + REQ_LUN_AT);
            lun_at.write_volatile(0x01);
            lun_at.add(1).write_volatile(target);
            lun_at.add(2).write_volatile(((lun >> 8) as u8) | 0x40);
            lun_at.add(3).write_volatile(lun as u8);

            let cdb_at = base.add(req + REQ_CDB_AT);
            for (i, byte) in cdb.iter().take(self.cdb_size).enumerate() {
                cdb_at.add(i).write_volatile(*byte);
            }
        }

        self.complete[slot].store(false, Ordering::Release);
        self.done[slot].add(1);

        let req_buf = Buf::read(self.dma.phys() + req as u64, self.req_size as u32);
        let resp_buf = Buf::write(self.dma.phys() + resp as u64, self.resp_size as u32);

        let queued = {
            let _guard = self.lock.lock();
            let inner = unsafe { &mut *self.inner.get() };

            let head = match (dir, data) {
                /* Data the device reads goes with the request, before the
                 * response it writes. */
                (Data::Out, Some((phys, len))) => {
                    inner.queue.add(&[req_buf, Buf::read(phys, len), resp_buf])
                }
                /* Data the device writes goes after the response. */
                (Data::In, Some((phys, len))) => {
                    inner.queue.add(&[req_buf, resp_buf, Buf::write(phys, len)])
                }
                _ => inner.queue.add(&[req_buf, resp_buf]),
            };

            match head {
                Some(head) if (head as usize) < inner.slot_of_head.len() => {
                    inner.slot_of_head[head as usize] = slot as u8;
                    true
                }
                _ => false,
            }
        };

        if !queued {
            trace!(0, "virtio-scsi: the request queue would not take a command");
            self.done[slot].done();
            self.done[slot].wait();
            self.give_slot(slot);
            return false;
        }

        self.transport.notify(REQUEST_QUEUE);
        self.wait_done(slot);

        let (response, status) = unsafe {
            (
                base.add(resp + RESP_RESPONSE_AT).read_volatile(),
                base.add(resp + RESP_STATUS_AT).read_volatile(),
            )
        };
        self.give_slot(slot);

        response == RESPONSE_OK && status == STATUS_GOOD
    }

    fn give_slot(&self, slot: usize) {
        self.free.fetch_or(1 << slot, Ordering::AcqRel);
    }

    fn wait_done(&self, slot: usize) {
        if block::interrupts_started() {
            self.done[slot].wait();
            return;
        }

        while !self.complete[slot].load(Ordering::Acquire) {
            self.collect();
            core::hint::spin_loop();
        }
        self.done[slot].wait();
    }

    /// Take every answer the adapter has published and wake whoever waits.
    fn collect(&self) {
        loop {
            let slot = {
                let _guard = self.lock.lock();
                let inner = unsafe { &mut *self.inner.get() };

                match inner.queue.take_used() {
                    None => break,
                    Some((head, _len)) => {
                        let head = head as usize;
                        if head >= inner.slot_of_head.len() {
                            continue;
                        }
                        let slot = inner.slot_of_head[head];
                        inner.slot_of_head[head] = NO_SLOT;
                        slot
                    }
                }
            };

            if slot == NO_SLOT || slot as usize >= MAX_SLOTS {
                continue;
            }

            self.complete[slot as usize].store(true, Ordering::Release);
            self.done[slot as usize].done();
        }
    }
}

/* ---- what the block table calls ---- */

extern "C" fn read_sectors(ctx: *mut u8, sector: u64, buf: *mut u8, count: u32) -> i32 {
    transfer(ctx, sector, buf, count, false, false)
}

extern "C" fn write_sectors(ctx: *mut u8, sector: u64, buf: *const u8, count: u32, fua: i32) -> i32 {
    transfer(ctx, sector, buf as *mut u8, count, true, fua != 0)
}

fn transfer(ctx: *mut u8, sector: u64, buf: *mut u8, count: u32, write: bool, fua: bool) -> i32 {
    if ctx.is_null() || buf.is_null() {
        return -1;
    }
    if count == 0 {
        return 0;
    }

    let disk = unsafe { &*(ctx as *const Disk) };
    let hba = unsafe { &*disk.hba };
    let len = count as u64 * disk.sector_size;

    /* One physically contiguous run, which is what the block API asks its
     * callers for: a page, page-aligned. */
    if len > PAGE_SIZE as u64 || (buf as usize) & (PAGE_SIZE - 1) != 0 {
        trace!(0, "virtio-scsi: {} sectors at {:p} is not one page-aligned run", count, buf);
        return -1;
    }

    let phys = dma::virt_to_phys(buf);
    if phys == 0 {
        return -1;
    }

    /* READ(10) / WRITE(10): the block address and the count, big-endian, and
     * the force-unit-access bit for a write that must reach the medium. */
    let mut cdb = [0u8; 32];
    cdb[0] = if write { OP_WRITE10 } else { OP_READ10 };
    if write && fua {
        cdb[1] = 0x08;
    }
    let lba = sector as u32;
    cdb[2..6].copy_from_slice(&lba.to_be_bytes());
    cdb[7..9].copy_from_slice(&(count as u16).to_be_bytes());

    let dir = if write { Data::Out } else { Data::In };
    if hba.command(disk.target, disk.lun, &cdb, Some((phys, len as u32)), dir) {
        0
    } else {
        -1
    }
}

extern "C" fn flush(ctx: *mut u8) -> i32 {
    if ctx.is_null() {
        return -1;
    }

    let disk = unsafe { &*(ctx as *const Disk) };
    let hba = unsafe { &*disk.hba };

    let mut cdb = [0u8; 32];
    cdb[0] = OP_SYNC_CACHE;

    if hba.command(disk.target, disk.lun, &cdb, None, Data::None) {
        0
    } else {
        -1
    }
}

extern "C" fn interrupt(ctx: *mut u8) {
    let hba = unsafe { &*(ctx as *const Hba) };

    /* The ISR byte says whether this device raised the line, and reading it
     * acknowledges; under MSI-X it means nothing (virtio 1.x 4.1.4.5). */
    if !hba.msix && hba.transport.read_isr() == 0 {
        return;
    }

    hba.collect();
}

/* ---- bring-up ---- */

enum IrqSource {
    Pci(pci::PciDevice),
    Line(u8),
}

/// Bring an adapter up and register every disk behind it.
fn start(transport: Box<dyn Transport>, source: IrqSource) -> bool {
    if ADAPTERS.load(Ordering::Relaxed) >= MAX_ADAPTERS {
        return false;
    }

    if virtio::negotiate(transport.as_ref(), 0).is_none() {
        trace!(0, "virtio-scsi: the adapter would not agree on features");
        return false;
    }

    let sense_size = transport.config_read32(CFG_SENSE_SIZE) as usize;
    let cdb_size = transport.config_read32(CFG_CDB_SIZE) as usize;
    let max_target = transport.config_read8(CFG_MAX_TARGET) as u16
        | (transport.config_read8(CFG_MAX_TARGET + 1) as u16) << 8;

    /* The header this driver writes has room for a 32-byte CDB and 96 bytes
     * of sense data; an adapter that wants more is not one it can serve. */
    if cdb_size == 0 || cdb_size > 32 || sense_size == 0 || sense_size > 96 {
        trace!(0, "virtio-scsi: cdb {} sense {} is not a shape this speaks", cdb_size, sense_size);
        virtio::failed(transport.as_ref());
        return false;
    }

    let req_size = REQ_FIXED + cdb_size;
    let resp_size = RESP_FIXED + sense_size;

    /* Every slot's header and its response share one page. */
    if MAX_SLOTS * ((req_size + resp_size + 7) & !7) > PAGE_SIZE {
        trace!(0, "virtio-scsi: {} slots of {}+{} bytes do not fit a page",
            MAX_SLOTS, req_size, resp_size);
        virtio::failed(transport.as_ref());
        return false;
    }

    let size = transport.select_queue(REQUEST_QUEUE);
    if size == 0 {
        trace!(0, "virtio-scsi: the adapter has no request queue");
        virtio::failed(transport.as_ref());
        return false;
    }

    let queue = match Queue::new(size) {
        Some(queue) => queue,
        None => {
            virtio::failed(transport.as_ref());
            return false;
        }
    };

    let dma = match DmaBuffer::new(1) {
        Some(dma) => dma,
        None => {
            virtio::failed(transport.as_ref());
            return false;
        }
    };

    let lock = match SpinLock::new() {
        Some(lock) => lock,
        None => {
            virtio::failed(transport.as_ref());
            return false;
        }
    };

    let mut done = Vec::with_capacity(MAX_SLOTS);
    let mut complete = Vec::with_capacity(MAX_SLOTS);
    for _ in 0..MAX_SLOTS {
        match WaitGroup::new() {
            Some(wg) => done.push(wg),
            None => {
                virtio::failed(transport.as_ref());
                return false;
            }
        }
        complete.push(AtomicBool::new(false));
    }

    let hba = Box::new(Hba {
        transport,
        req_size,
        resp_size,
        cdb_size,
        max_target: core::cmp::min(max_target, 255),
        msix: false,
        dma,
        free: AtomicU32::new(if MAX_SLOTS >= 32 { u32::MAX } else { (1 << MAX_SLOTS) - 1 }),
        done,
        complete,
        lock,
        inner: UnsafeCell::new(Inner {
            queue,
            slot_of_head: [NO_SLOT; virtio::MAX_DESCRIPTORS as usize],
        }),
        _irq: Irq::None,
    });

    let raw = Box::into_raw(hba);
    let transport = unsafe { (*raw).transport.as_ref() };

    let (irq, msix_entry) = arm_interrupt(transport, source, raw);
    unsafe {
        (*raw)._irq = irq;
        (*raw).msix = msix_entry.is_some();
    }

    let layout = {
        let inner = unsafe { &*(*raw).inner.get() };
        virtio::QueueLayout {
            size: inner.queue.size(),
            desc: inner.queue.desc_phys(),
            driver: inner.queue.avail_phys(),
            device: inner.queue.used_phys(),
            msix: msix_entry,
        }
    };
    transport.setup_queue(REQUEST_QUEUE, &layout);
    virtio::driver_ok(transport);

    ADAPTERS.fetch_add(1, Ordering::AcqRel);

    let hba = unsafe { &*raw };
    trace!(0, "virtio-scsi: an adapter with cdb {} sense {}, targets up to {}",
        cdb_size, sense_size, hba.max_target);

    let mut found = 0;
    for target in 0..=hba.max_target {
        if DISKS.load(Ordering::Relaxed) >= MAX_DISKS {
            break;
        }
        if probe_lun(hba, target as u8, 0) {
            found += 1;
        }
    }

    trace!(0, "virtio-scsi: {} disks behind the adapter", found);
    true
}

/// Ask one logical unit what it is, and register it if it is a disk.
fn probe_lun(hba: &'static Hba, target: u8, lun: u16) -> bool {
    let mut buf = match DmaBuffer::new(1) {
        Some(buf) => buf,
        None => return false,
    };

    /* INQUIRY: what kind of device this is, and whether anything is there */
    let mut cdb = [0u8; 32];
    cdb[0] = OP_INQUIRY;
    cdb[4] = INQUIRY_LEN as u8;

    if !hba.command(target, lun, &cdb, Some((buf.phys(), INQUIRY_LEN as u32)), Data::In) {
        return false;
    }

    let inquiry = buf.as_slice()[0];
    let device_type = inquiry & 0x1F;
    let qualifier = (inquiry >> 5) & 0x07;
    if device_type != TYPE_DIRECT_ACCESS || qualifier != 0 {
        return false;
    }

    /* TEST UNIT READY, until the unit stops answering with the attention it
     * owes whoever first spoke to it after a reset. */
    for _ in 0..5 {
        let mut cdb = [0u8; 32];
        cdb[0] = OP_TEST_UNIT_READY;
        if hba.command(target, lun, &cdb, None, Data::None) {
            break;
        }
    }

    /* READ CAPACITY(10): the last block's address and how big a block is,
     * both big-endian. */
    let mut cdb = [0u8; 32];
    cdb[0] = OP_READ_CAPACITY;
    buf.as_mut_slice()[..READ_CAPACITY_LEN].fill(0);

    if !hba.command(target, lun, &cdb, Some((buf.phys(), READ_CAPACITY_LEN as u32)), Data::In) {
        return false;
    }

    let answer = &buf.as_slice()[..READ_CAPACITY_LEN];
    let last_lba = u32::from_be_bytes([answer[0], answer[1], answer[2], answer[3]]) as u64;
    let block_size = u32::from_be_bytes([answer[4], answer[5], answer[6], answer[7]]) as u64;

    let capacity = last_lba + 1;
    let sector_size = if block_size == 0 || block_size > PAGE_SIZE as u64 {
        DEFAULT_SECTOR_SIZE
    } else {
        block_size
    };

    let index = DISKS.load(Ordering::Relaxed);
    let mut name = [0u8; 8];
    name[..2].copy_from_slice(b"sd");
    name[2] = b'a' + index as u8;

    let disk = Box::into_raw(Box::new(Disk {
        hba: hba as *const Hba,
        target,
        lun,
        capacity,
        sector_size,
        name,
    }));

    let ops = block::BlockDeviceOps {
        name: unsafe { core::ptr::addr_of!((*disk).name) as *const u8 },
        capacity,
        sector_size,
        read_sectors,
        write_sectors,
        flush: Some(flush),
        /* The asynchronous path is NVMe's. */
        submit: None,
        kick: None,
        ctx: disk as *mut u8,
        parent: 0,
    };

    match block::register(&ops) {
        Some(registration) => {
            core::mem::forget(registration);
            DISKS.store(index + 1, Ordering::Release);
            trace!(0, "virtio-scsi: {} is target {} lun {}, {} sectors of {} bytes",
                core::str::from_utf8(&name[..3]).unwrap_or("?"), target, lun,
                capacity, sector_size);
            true
        }
        None => {
            unsafe { drop(Box::from_raw(disk)) };
            false
        }
    }
}

fn arm_interrupt(
    transport: &dyn Transport, source: IrqSource, hba: *mut Hba,
) -> (Irq, Option<u16>) {
    if let Some(table) = transport.msix_table() {
        match MsixInterrupt::register(table, 0, interrupt, hba as *mut u8) {
            Some(irq) => {
                transport.use_msix(0);
                return (Irq::Msix(irq), Some(0));
            }
            None => trace!(0, "virtio-scsi: no MSI-X slot left, falling back on the line"),
        }
    }

    let registered = match source {
        IrqSource::Pci(dev) => LegacyInterrupt::register_level(&dev, interrupt, hba as *mut u8),
        IrqSource::Line(line) => LegacyInterrupt::register_irq(line, interrupt, hba as *mut u8),
    };

    match registered {
        Some(irq) => (Irq::Legacy(irq), None),
        None => {
            trace!(0, "virtio-scsi: the adapter has no interrupt -- I/O will be polled");
            (Irq::None, None)
        }
    }
}

/// The virtio-scsi adapters on the PCI bus. Called from the boot path.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub extern "C" fn rust_virtio_scsi_init() {
    use virtio::pci::PciTransport;

    for device in [pci::device::VIRTIO_SCSI, pci::device::VIRTIO_SCSI_MODERN] {
        let mut start_at = 0;
        while ADAPTERS.load(Ordering::Relaxed) < MAX_ADAPTERS {
            let (index, dev) = match pci::find_device_from(pci::vendor::VIRTIO, device, start_at) {
                Some(found) => found,
                None => break,
            };
            start_at = index + 1;

            dev.enable_bus_mastering();
            let transport = match PciTransport::probe(&dev) {
                Some(transport) => transport,
                None => continue,
            };

            trace!(0, "virtio-scsi: {} virtio-pci at {:02x}:{:02x}.{}",
                if transport.is_legacy() { "legacy" } else { "modern" },
                dev.bus, dev.slot, dev.func);

            start(Box::new(transport), IrqSource::Pci(dev));
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[no_mangle]
pub extern "C" fn rust_virtio_scsi_init() {}

/// The same for the virtio-mmio windows the device tree described.
///
/// # Safety
/// `slots` points at `count` slots, each naming a mapped register window.
#[no_mangle]
pub unsafe extern "C" fn rust_virtio_scsi_init_mmio(slots: *const Slot, count: usize) {
    if slots.is_null() {
        return;
    }

    let slots = unsafe { core::slice::from_raw_parts(slots, count) };
    for slot in slots {
        if ADAPTERS.load(Ordering::Relaxed) >= MAX_ADAPTERS {
            break;
        }
        if MmioTransport::device_id(slot) != virtio::device::SCSI {
            continue;
        }

        let transport = match MmioTransport::probe(slot) {
            Some(transport) => transport,
            None => continue,
        };

        trace!(0, "virtio-scsi: virtio-mmio at {:#x}, irq {}", slot.base, slot.int_id);
        start(Box::new(transport), IrqSource::Line(slot.int_id as u8));
    }
}
