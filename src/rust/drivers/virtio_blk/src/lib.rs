//! virtio-blk: the disk QEMU hands a guest, and the root filesystem's home
//! on both architectures.
//!
//! A request is three buffers on the one queue -- a header the driver writes,
//! the data, and a status byte the device writes -- or two for a flush, which
//! has no data. Every caller of this driver blocks until its own request is
//! done, so the driver holds a small pool of slots instead of a queue of
//! requests: a slot is a header, a status byte and something to wait on, and
//! a caller that finds none free gives the CPU up until one is.
//!
//! Early in boot there is nothing to wake a waiter with -- the partition
//! probe reads sectors before the scheduler is running -- so until the block
//! layer says interrupts have started, a request is polled to completion
//! instead of waited for.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

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

/// vda … vdh: as many as the kernel's naming leaves room for.
const MAX_DEVICES: usize = 8;

/// Requests in flight at once. Each takes three descriptors of the 256 the
/// queue has, so 64 of them is 192 -- the ring is never the thing that runs
/// out, and the slots are what callers queue for.
const MAX_SLOTS: usize = 64;

const SECTOR_SIZE: usize = 512;
/// The request queue, which is the only one a virtio-blk has.
const REQUEST_QUEUE: u16 = 0;

/* What a request header asks for */
const TYPE_IN: u32 = 0;
const TYPE_OUT: u32 = 1;
const TYPE_FLUSH: u32 = 4;

/// The device has a write cache, and a flush is worth sending.
const FEATURE_FLUSH: u64 = 1 << 9;

const HEADER_SIZE: usize = 16;
/// The status byte the device writes: 0 is success.
const STATUS_UNSET: u8 = 0xFF;
/// No slot owns this descriptor head.
const NO_SLOT: u8 = 0xFF;

static DEVICES: AtomicUsize = AtomicUsize::new(0);

/// Nothing to set up: the driver is called from the boot path by the names
/// below, and this is what keeps them in the archive.
pub fn init() {}

struct Blk {
    transport: Box<dyn Transport>,
    /// NUL-terminated, handed to the block table, which keeps it
    name: [u8; 8],
    capacity: u64,
    has_flush: bool,
    /// Whether completions arrive as MSI-X, where the ISR byte means nothing
    msix: bool,

    /// One page holding every slot's request header and status byte
    dma: DmaBuffer,
    /// Free slots, one bit each
    free: AtomicU64,
    /// Admission, in the order callers arrive: a caller takes the next
    /// ticket and waits until the slots in flight have come down far enough
    /// for it to be its turn. A bitmap alone would let a caller that is
    /// already looping take the slot another has been waiting for, and under
    /// a load test with more tasks than slots that starves half of them.
    next_ticket: AtomicU64,
    served: AtomicU64,
    /// Taken by the submitter, given back by the completion
    done: Vec<WaitGroup>,
    /// Set by the completion before it wakes the waiter, so the polled path
    /// has something to watch
    complete: Vec<AtomicBool>,

    /// The queue and what is on it. Taken with interrupts off: the
    /// completion path runs in interrupt context.
    lock: SpinLock<()>,
    inner: UnsafeCell<Inner>,

    /// Kept so the registration outlives the device
    _irq: Irq,
}

struct Inner {
    queue: Queue,
    /// Which slot a descriptor head belongs to, NO_SLOT for none
    slot_of_head: [u8; virtio::MAX_DESCRIPTORS as usize],
}

enum Irq {
    Msix(MsixInterrupt),
    Legacy(LegacyInterrupt),
    None,
}

/* Everything inside is either atomic or taken under the lock, and the device
 * is registered for the life of the kernel. */
unsafe impl Sync for Blk {}
unsafe impl Send for Blk {}

impl Blk {
    fn header_offset(slot: usize) -> usize {
        slot * HEADER_SIZE
    }

    fn status_offset(slot: usize) -> usize {
        MAX_SLOTS * HEADER_SIZE + slot
    }

    /// A free slot, or None. Lock-free: a bit is claimed with a compare and
    /// exchange, so two callers never take the same one.
    fn take_slot(&self) -> Option<usize> {
        loop {
            let free = self.free.load(Ordering::Acquire);
            if free == 0 {
                return None;
            }
            let slot = free.trailing_zeros() as usize;
            let taken = free & !(1 << slot);
            if self
                .free
                .compare_exchange(free, taken, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(slot);
            }
        }
    }

    fn give_slot(&self, slot: usize) {
        self.free.fetch_or(1 << slot, Ordering::AcqRel);
        /* The slot is back before the ticket is: whoever this lets in finds
         * one free. */
        self.served.fetch_add(1, Ordering::AcqRel);
    }

    /// Wait for a slot in the order callers asked for one, giving the CPU up
    /// while the device works through what it has. Before the scheduler runs
    /// there is nothing to yield to, so completions are collected here
    /// instead -- which is also what makes the early-boot path work at all.
    fn wait_for_slot(&self) -> usize {
        let ticket = self.next_ticket.fetch_add(1, Ordering::AcqRel);

        loop {
            if self.served.load(Ordering::Acquire) + MAX_SLOTS as u64 > ticket {
                if let Some(slot) = self.take_slot() {
                    return slot;
                }
            }

            if block::interrupts_started() {
                kcore::task::yield_to_runnable();
            } else {
                self.collect();
            }
        }
    }

    /// Write a request header into a slot and hand the chain to the device.
    /// The status byte is the device's answer, read once the wait is over.
    fn request(&self, kind: u32, sector: u64, data: Option<(u64, u32, bool)>) -> bool {
        let slot = self.wait_for_slot();

        let header = Self::header_offset(slot);
        let status = Self::status_offset(slot);
        let base = self.dma.as_ptr() as *mut u8;

        unsafe {
            (base.add(header) as *mut u32).write_volatile(kind);
            (base.add(header + 4) as *mut u32).write_volatile(0);
            (base.add(header + 8) as *mut u64).write_volatile(sector);
            base.add(status).write_volatile(STATUS_UNSET);
        }

        self.complete[slot].store(false, Ordering::Release);
        self.done[slot].add(1);

        let header_buf = Buf::read(self.dma.phys() + header as u64, HEADER_SIZE as u32);
        let status_buf = Buf::write(self.dma.phys() + status as u64, 1);

        let queued = {
            let _guard = self.lock.lock();
            /* The lock is held for the whole of this: a completion on
             * another CPU may claim the head the moment it drops, so the
             * mapping from head to slot is published here and not after. */
            let inner = unsafe { &mut *self.inner.get() };

            let head = match data {
                Some((phys, len, writable)) => {
                    let data_buf = if writable {
                        Buf::write(phys, len)
                    } else {
                        Buf::read(phys, len)
                    };
                    inner.queue.add(&[header_buf, data_buf, status_buf])
                }
                None => inner.queue.add(&[header_buf, status_buf]),
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
            trace!(0, "virtio-blk: the queue would not take a request");
            self.done[slot].done();
            self.done[slot].wait();
            self.give_slot(slot);
            return false;
        }

        self.transport.notify(REQUEST_QUEUE);
        self.wait_done(slot);

        let answer = unsafe { base.add(status).read_volatile() };
        self.give_slot(slot);
        answer == 0
    }

    /// Wait for one slot's completion: blocked if there is a scheduler to
    /// block under, polled if the boot has not got there yet.
    fn wait_done(&self, slot: usize) {
        if block::interrupts_started() {
            self.done[slot].wait();
            return;
        }

        while !self.complete[slot].load(Ordering::Acquire) {
            self.collect();
            core::hint::spin_loop();
        }
        /* The count the submitter added, taken back: the completion has
         * already given it, so this returns at once. */
        self.done[slot].wait();
    }

    /// Take every completion the device has published and wake whoever is
    /// waiting for it. Runs in interrupt context, and on the polled path in
    /// the submitter's.
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
                            trace!(0, "virtio-blk: the device named descriptor {}, which is not one", head);
                            continue;
                        }
                        let slot = inner.slot_of_head[head];
                        inner.slot_of_head[head] = NO_SLOT;
                        slot
                    }
                }
            };

            if slot == NO_SLOT || slot as usize >= MAX_SLOTS {
                trace!(0, "virtio-blk: a completion for a request nobody is waiting for");
                continue;
            }

            /* The flag before the wake: the polled path watches the flag,
             * and the blocked one reads the status once wait() returns. */
            self.complete[slot as usize].store(true, Ordering::Release);
            self.done[slot as usize].done();
        }
    }
}

/* ---- what the block table calls ---- */

extern "C" fn read_sectors(ctx: *mut u8, sector: u64, buf: *mut u8, count: u32) -> i32 {
    transfer(ctx, sector, buf, count, TYPE_IN)
}

extern "C" fn write_sectors(ctx: *mut u8, sector: u64, buf: *const u8, count: u32, fua: i32) -> i32 {
    let rc = transfer(ctx, sector, buf as *mut u8, count, TYPE_OUT);
    if rc != 0 || fua == 0 {
        return rc;
    }
    flush(ctx)
}

fn transfer(ctx: *mut u8, sector: u64, buf: *mut u8, count: u32, kind: u32) -> i32 {
    if ctx.is_null() || buf.is_null() {
        return -1;
    }
    if count == 0 {
        return 0;
    }

    let blk = unsafe { &*(ctx as *const Blk) };
    let len = count as usize * SECTOR_SIZE;

    /* The driver hands the device one physically contiguous run, so the
     * buffer is a page at most and page-aligned -- what the block API asks
     * of its callers. */
    if len > PAGE_SIZE || (buf as usize) & (PAGE_SIZE - 1) != 0 {
        trace!(0, "virtio-blk: {} sectors at {:p} is not one page-aligned run", count, buf);
        return -1;
    }

    let phys = dma::virt_to_phys(buf);
    if phys == 0 {
        trace!(0, "virtio-blk: no physical address for {:p}", buf);
        return -1;
    }

    if blk.request(kind, sector, Some((phys, len as u32, kind == TYPE_IN))) {
        0
    } else {
        -1
    }
}

extern "C" fn flush(ctx: *mut u8) -> i32 {
    if ctx.is_null() {
        return -1;
    }

    let blk = unsafe { &*(ctx as *const Blk) };
    if !blk.has_flush {
        /* No write cache: there is nothing to push. */
        return 0;
    }

    if blk.request(TYPE_FLUSH, 0, None) {
        0
    } else {
        -1
    }
}

extern "C" fn interrupt(ctx: *mut u8) {
    let blk = unsafe { &*(ctx as *const Blk) };

    /* On the line-interrupt path the ISR byte says whether this device
     * raised it, and reading it acknowledges. Under MSI-X it means nothing
     * (virtio 1.x 4.1.4.5): a device that keeps the spec leaves it zero, and
     * gating on it there would drop every completion. */
    if !blk.msix && blk.transport.read_isr() == 0 {
        return;
    }

    blk.collect();
}

/* ---- bring-up ---- */

/// Take a device on whatever bus found it: negotiate, lay the queue out,
/// put its interrupt in place and register it as a disk.
///
/// The order matters at one point: a modern virtio-pci device is told which
/// MSI-X vector serves its queue as the queue is enabled, so the handler has
/// to be registered first -- and the handler needs a device to be handed, so
/// the device is built before its queue is started.
fn start(transport: Box<dyn Transport>, source: IrqSource) -> bool {
    let index = DEVICES.load(Ordering::Relaxed);
    if index >= MAX_DEVICES {
        return false;
    }

    let features = match virtio::negotiate(transport.as_ref(), FEATURE_FLUSH) {
        Some(features) => features,
        None => {
            trace!(0, "virtio-blk: the device would not agree on features");
            return false;
        }
    };
    let has_flush = features & FEATURE_FLUSH != 0;

    let size = transport.select_queue(REQUEST_QUEUE);
    if size == 0 {
        trace!(0, "virtio-blk: the device has no request queue");
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

    let lock = match SpinLock::new(()) {
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

    let capacity = transport.config_read64(0);
    let mut name = [0u8; 8];
    name[..2].copy_from_slice(b"vd");
    name[2] = b'a' + index as u8;

    let blk = Box::new(Blk {
        transport,
        name,
        capacity,
        has_flush,
        msix: false,
        dma,
        free: AtomicU64::new(if MAX_SLOTS >= 64 { u64::MAX } else { (1 << MAX_SLOTS) - 1 }),
        next_ticket: AtomicU64::new(0),
        /* The first MAX_SLOTS tickets are admitted straight away. */
        served: AtomicU64::new(0),
        done,
        complete,
        lock,
        inner: UnsafeCell::new(Inner {
            queue,
            slot_of_head: [NO_SLOT; virtio::MAX_DESCRIPTORS as usize],
        }),
        _irq: Irq::None,
    });

    /* From here the device is a pointer: the interrupt handler takes it as
     * its context, and it is never freed once registered. */
    let raw = Box::into_raw(blk);
    let transport = unsafe { (*raw).transport.as_ref() };

    let (irq, msix_entry) = arm_interrupt(transport, source, raw);
    unsafe {
        (*raw)._irq = irq;
        (*raw).msix = msix_entry.is_some();
    }

    /* Now the device can be pointed at the rings -- and told, if it has
     * MSI-X, which vector the queue raises. */
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

    let ops = block::BlockDeviceOps {
        name: unsafe { core::ptr::addr_of!((*raw).name) as *const u8 },
        capacity,
        sector_size: SECTOR_SIZE as u64,
        read_sectors,
        write_sectors,
        flush: Some(flush),
        /* The asynchronous path is NVMe's; this driver is a queue and a
         * caller that waits on it. */
        submit: None,
        kick: None,
        ctx: raw as *mut u8,
        parent: 0,
    };

    match block::register(&ops) {
        Some(registration) => {
            core::mem::forget(registration);
            DEVICES.store(index + 1, Ordering::Release);
            trace!(0, "virtio-blk: {} is a disk of {} sectors{}, {} slots",
                core::str::from_utf8(&name[..3]).unwrap_or("?"), capacity,
                if has_flush { " with a write cache" } else { "" }, MAX_SLOTS);
            true
        }
        None => {
            trace!(0, "virtio-blk: the device table would not take another disk");
            virtio::failed(transport);
            unsafe { drop(Box::from_raw(raw)) };
            false
        }
    }
}

enum IrqSource {
    /// A PCI device, whose line is used where MSI-X cannot be
    Pci(pci::PciDevice),
    /// A bare interrupt number: the device tree's, for a virtio-mmio window
    Line(u8),
}

/// Put the completion interrupt in place: MSI-X where the bus has it, the
/// device's line otherwise. Returns the entry the queue should raise.
fn arm_interrupt(
    transport: &dyn Transport, source: IrqSource, blk: *mut Blk,
) -> (Irq, Option<u16>) {
    if let Some(table) = transport.msix_table() {
        match MsixInterrupt::register(table, 0, interrupt, blk as *mut u8) {
            Some(irq) => {
                transport.use_msix(0);
                return (Irq::Msix(irq), Some(0));
            }
            None => trace!(0, "virtio-blk: no MSI-X slot left, falling back on the line"),
        }
    }

    let registered = match source {
        IrqSource::Pci(dev) => LegacyInterrupt::register_level(&dev, interrupt, blk as *mut u8),
        IrqSource::Line(line) => LegacyInterrupt::register_irq(line, interrupt, blk as *mut u8),
    };

    match registered {
        Some(irq) => (Irq::Legacy(irq), None),
        None => {
            trace!(0, "virtio-blk: the device has no interrupt -- I/O will be polled");
            (Irq::None, None)
        }
    }
}

/// The virtio-blk disks on the PCI bus. Called from the boot path, before
/// the partitions are looked at.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub extern "C" fn rust_virtio_blk_init() {
    use virtio::pci::PciTransport;

    for device in [pci::device::VIRTIO_BLK, pci::device::VIRTIO_BLK_MODERN] {
        let mut start_at = 0;
        while DEVICES.load(Ordering::Relaxed) < MAX_DEVICES {
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

            trace!(0, "virtio-blk: {} virtio-pci at {:02x}:{:02x}.{}",
                if transport.is_legacy() { "legacy" } else { "modern" },
                dev.bus, dev.slot, dev.func);

            start(Box::new(transport), IrqSource::Pci(dev));
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[no_mangle]
pub extern "C" fn rust_virtio_blk_init() {}

/// The same for the virtio-mmio windows the device tree described.
///
/// # Safety
/// `slots` points at `count` slots, each naming a mapped register window.
#[no_mangle]
pub unsafe extern "C" fn rust_virtio_blk_init_mmio(slots: *const Slot, count: usize) {
    if slots.is_null() {
        return;
    }

    let slots = unsafe { core::slice::from_raw_parts(slots, count) };
    for slot in slots {
        if DEVICES.load(Ordering::Relaxed) >= MAX_DEVICES {
            break;
        }
        if MmioTransport::device_id(slot) != virtio::device::BLK {
            continue;
        }

        let transport = match MmioTransport::probe(slot) {
            Some(transport) => transport,
            None => continue,
        };

        trace!(0, "virtio-blk: virtio-mmio at {:#x}, irq {}", slot.base, slot.int_id);
        start(Box::new(transport), IrqSource::Line(slot.int_id as u8));
    }
}
