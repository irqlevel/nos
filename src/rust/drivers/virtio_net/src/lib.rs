//! virtio-net: the network card QEMU gives a guest, on both architectures.
//!
//! Two queues, receive and transmit, and a twelve-byte header in front of
//! every packet (ten on a legacy device, which has no `num_buffers` field).
//! The header goes in a descriptor of its own, so the packet starts at the
//! first byte of the frame -- where the rest of the stack expects it, and
//! what lets a received frame be handed up as it is rather than copied.
//!
//! Where the work happens follows the kernel's net contract: the interrupt
//! raises the soft IRQs and does nothing else; the receive harvest runs in
//! the receive soft IRQ, which is one CPU at a time, so the receive side
//! needs no lock of its own; and the transmit side is only ever touched from
//! `flush_tx`, which the net stack calls with its own lock held. A frame
//! finished with there is handed back with `tx_done` and not dropped --
//! dropping it would free under that lock, and freeing shoots down every
//! other CPU's TLB and waits for it.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use kcore::dma::DmaBuffer;
use kcore::interrupt::LegacyInterrupt;
use kcore::msix::MsixInterrupt;
use kcore::net::{self, NetDeviceHandle, NetFrame};
use kcore::pci;
use kcore::softirq;
use kcore::trace;
use virtio::mmio::{MmioTransport, Slot};
use virtio::{Buf, Queue, Transport};

/// eth0 … eth3
const MAX_DEVICES: usize = 4;

const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;

/// Two descriptors to a receive slot, so this many of them need twice as
/// many descriptors in the ring.
const MAX_RX_SLOTS: usize = 128;
/// Transmits in flight at once, two descriptors each.
const MAX_TX_SLOTS: usize = 32;

/// What a frame from the pool holds (NetFramePool::FrameCapacity).
const FRAME_CAPACITY: usize = 2048;

/// How many frames one harvest hands up before it comes back through the
/// soft IRQ, so a flood cannot hold the CPU.
const RX_BUDGET: usize = 64;

/// VIRTIO_NET_F_MAC: the device has an address to tell us.
const FEATURE_MAC: u64 = 1 << 5;

/// The header in front of every packet, and the shorter one a legacy device
/// takes (no num_buffers).
const HDR_SIZE_MODERN: usize = 12;
const HDR_SIZE_LEGACY: usize = 10;

const NO_SLOT: u8 = 0xFF;

/// What QEMU's user-mode networking hands out, and what the UDP shell is
/// reachable on when nothing runs DHCP.
const DEFAULT_IP: u32 = (10 << 24) | (0 << 16) | (2 << 8) | 15;

static DEVICES: AtomicUsize = AtomicUsize::new(0);

/// Nothing to set up: the driver is called from the boot path by the names
/// at the bottom, and this is what keeps them in the archive.
pub fn init() {}

struct Net {
    transport: Box<dyn Transport>,
    name: [u8; 8],
    mac: [u8; 6],
    /// 12, or 10 on a legacy device
    hdr_size: usize,
    handle: NetDeviceHandle,
    /// Whether completions arrive as MSI-X, where the ISR byte means nothing
    msix: bool,

    /// One page each for the receive and transmit headers, a slot's worth
    /// apiece
    rx_hdr: DmaBuffer,
    tx_hdr: DmaBuffer,

    /// Touched only by the receive soft IRQ, which runs on one CPU at a time
    rx: UnsafeCell<RxState>,
    /// Touched only from flush_tx, which the net stack calls under its lock
    tx: UnsafeCell<TxState>,

    rx_packets: AtomicU64,
    tx_packets: AtomicU64,
    rx_dropped: AtomicU64,
    /// Receive slots with no frame in them, waiting on memory
    rx_empty: AtomicU32,

    _irq: Irq,
}

struct RxState {
    queue: Queue,
    slots: usize,
    frames: [Option<NetFrame>; MAX_RX_SLOTS],
    slot_of_head: [u8; virtio::MAX_DESCRIPTORS as usize],
}

struct TxState {
    queue: Queue,
    frames: [Option<NetFrame>; MAX_TX_SLOTS],
    slot_of_head: [u8; virtio::MAX_DESCRIPTORS as usize],
    /// Free slots, one bit each
    free: u32,
}

enum Irq {
    Msix(MsixInterrupt),
    Legacy(LegacyInterrupt),
    None,
}

/* The state inside is reached only from the paths named on each field, and
 * the device is registered for the life of the kernel. */
unsafe impl Sync for Net {}
unsafe impl Send for Net {}

impl Net {
    fn rx_hdr_phys(&self, slot: usize) -> u64 {
        self.rx_hdr.phys() + (slot * self.hdr_size) as u64
    }

    fn tx_hdr_phys(&self, slot: usize) -> u64 {
        self.tx_hdr.phys() + (slot * self.hdr_size) as u64
    }

    /// Put a frame in a receive slot and hand the pair of descriptors -- the
    /// header, then the frame -- to the device.
    fn post_rx(&self, rx: &mut RxState, slot: usize, frame: NetFrame) -> bool {
        let bufs = [
            Buf::write(self.rx_hdr_phys(slot), self.hdr_size as u32),
            Buf::write(frame.data_phys(), FRAME_CAPACITY as u32),
        ];

        match rx.queue.add(&bufs) {
            Some(head) if (head as usize) < rx.slot_of_head.len() => {
                rx.slot_of_head[head as usize] = slot as u8;
                rx.frames[slot] = Some(frame);
                true
            }
            _ => false,
        }
    }

    /// Fill every empty receive slot that memory allows. True if anything
    /// was posted, which is what owes the device a notify.
    fn refill_rx(&self, rx: &mut RxState) -> bool {
        if self.rx_empty.load(Ordering::Relaxed) == 0 {
            return false;
        }

        let mut posted = false;
        for slot in 0..rx.slots {
            if rx.frames[slot].is_some() {
                continue;
            }

            let frame = match NetFrame::alloc_rx(FRAME_CAPACITY) {
                Some(frame) => frame,
                /* Still nothing to post with: the next pass tries again. */
                None => break,
            };

            if !self.post_rx(rx, slot, frame) {
                break;
            }

            self.rx_empty.fetch_sub(1, Ordering::Relaxed);
            posted = true;
        }

        posted
    }

    /// Take what the device has received and hand it up. Runs in the receive
    /// soft IRQ.
    fn harvest(&self) {
        /* The receive soft IRQ is one CPU at a time, and nothing else
         * touches this state. */
        let rx = unsafe { &mut *self.rx.get() };

        let mut batch = [0usize; RX_BUDGET];
        let mut batched = 0;
        let mut taken = 0;
        let mut budget_hit = false;

        let mut refilled = self.refill_rx(rx);

        loop {
            let (head, len) = match rx.queue.take_used() {
                Some(used) => used,
                None => break,
            };

            let head = head as usize;
            if head >= rx.slot_of_head.len() {
                trace!(0, "virtio-net: the device named descriptor {}, which is not one", head);
                continue;
            }

            let slot = rx.slot_of_head[head];
            rx.slot_of_head[head] = NO_SLOT;
            if slot == NO_SLOT || slot as usize >= rx.slots {
                continue;
            }
            let slot = slot as usize;

            let mut frame = match rx.frames[slot].take() {
                Some(frame) => frame,
                None => continue,
            };

            /* The reported length covers the header and the packet behind
             * it; a device that reports less than its own header has said
             * nothing this can use. */
            let data_len = (len as usize).saturating_sub(self.hdr_size);
            if data_len == 0 || data_len > FRAME_CAPACITY {
                self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                frame.set_len(0);
                if !self.post_rx(rx, slot, frame) {
                    self.rx_empty.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }

            frame.set_len(data_len);
            self.rx_packets.fetch_add(1, Ordering::Relaxed);
            batch[batched] = frame.into_raw();
            batched += 1;
            taken += 1;

            /* The slot goes back to the device with a fresh frame: the one
             * just harvested is the stack's now, for as long as it likes. */
            match NetFrame::alloc_rx(FRAME_CAPACITY) {
                Some(fresh) => {
                    if self.post_rx(rx, slot, fresh) {
                        refilled = true;
                    } else {
                        self.rx_empty.fetch_add(1, Ordering::Relaxed);
                    }
                }
                None => {
                    self.rx_empty.fetch_add(1, Ordering::Relaxed);
                    self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }

            if batched == batch.len() {
                self.handle.enqueue_rx_batch(&batch[..batched]);
                batched = 0;
            }

            if taken >= RX_BUDGET {
                budget_hit = true;
                break;
            }
        }

        if batched != 0 {
            self.handle.enqueue_rx_batch(&batch[..batched]);
        }

        if refilled {
            self.transport.notify(RX_QUEUE);
        }

        if budget_hit {
            /* Still ours to finish: come back through the soft IRQ so this
             * CPU can do something else in between. */
            softirq::raise(softirq::TYPE_NET_RX);
        }
    }

    /// Give back every transmit the device has finished with, and send what
    /// the stack has queued. Called under the net stack's transmit lock.
    fn flush(&self) {
        let tx = unsafe { &mut *self.tx.get() };

        /* Completions first: they are what frees the slots the sends below
         * need. */
        loop {
            let (head, _len) = match tx.queue.take_used() {
                Some(used) => used,
                None => break,
            };

            let head = head as usize;
            if head >= tx.slot_of_head.len() {
                continue;
            }

            let slot = tx.slot_of_head[head];
            /* Cleared so a repeated completion for the same head cannot free
             * the slot, and its frame, twice. */
            tx.slot_of_head[head] = NO_SLOT;
            if slot == NO_SLOT || slot as usize >= MAX_TX_SLOTS {
                continue;
            }
            let slot = slot as usize;

            tx.free |= 1 << slot;
            if let Some(frame) = tx.frames[slot].take() {
                /* Handed back, not dropped: this runs under a lock, and a
                 * drop would free -- which shoots down every other CPU's TLB
                 * and waits for CPUs that cannot answer while they spin on
                 * that same lock. */
                self.handle.tx_done(frame);
            }
        }

        let mut submitted = 0;
        while tx.free != 0 {
            let frame = match self.handle.tx_dequeue() {
                Some(frame) => frame,
                None => break,
            };

            let slot = tx.free.trailing_zeros() as usize;

            /* A zeroed header in front of the packet: no checksum offload,
             * no segmentation, nothing to say. */
            unsafe {
                core::ptr::write_bytes(
                    (self.tx_hdr.as_ptr() as *mut u8).add(slot * self.hdr_size),
                    0,
                    self.hdr_size,
                );
            }

            let bufs = [
                Buf::read(self.tx_hdr_phys(slot), self.hdr_size as u32),
                Buf::read(frame.data_phys(), frame.len() as u32),
            ];

            match tx.queue.add(&bufs) {
                Some(head) if (head as usize) < tx.slot_of_head.len() => {
                    tx.free &= !(1 << slot);
                    tx.slot_of_head[head as usize] = slot as u8;
                    tx.frames[slot] = Some(frame);
                    submitted += 1;
                }
                _ => {
                    /* The ring would not take it. The frame goes back for
                     * release rather than being dropped here, for the same
                     * reason a completed one does. */
                    trace!(0, "virtio-net: the transmit ring is full");
                    self.handle.tx_done(frame);
                    break;
                }
            }
        }

        if submitted != 0 {
            self.tx_packets.fetch_add(submitted as u64, Ordering::Relaxed);
            self.transport.notify(TX_QUEUE);
        }
    }
}

/* ---- what the net stack calls ---- */

extern "C" fn flush_tx(ctx: *mut u8) {
    let net = unsafe { &*(ctx as *const Net) };
    net.flush();
}

extern "C" fn process_rx(ctx: *mut u8) {
    let net = unsafe { &*(ctx as *const Net) };
    net.harvest();
}

extern "C" fn interrupt(ctx: *mut u8) {
    let net = unsafe { &*(ctx as *const Net) };

    /* On the line-interrupt path the ISR byte says whether this device
     * raised it, and reading it acknowledges. Under MSI-X it means nothing
     * (virtio 1.x 4.1.4.5). */
    if !net.msix && net.transport.read_isr() == 0 {
        return;
    }

    /* Nothing is harvested here: both sides are soft IRQ work, and the
     * transmit one needs the stack's lock, which an interrupt must not
     * take. */
    softirq::raise(softirq::TYPE_NET_RX);
    softirq::raise(softirq::TYPE_NET_TX);
}

/* ---- bring-up ---- */

enum IrqSource {
    Pci(pci::PciDevice),
    Line(u8),
}

fn start(transport: Box<dyn Transport>, source: IrqSource) -> bool {
    let index = DEVICES.load(Ordering::Relaxed);
    if index >= MAX_DEVICES {
        return false;
    }

    let features = match virtio::negotiate(transport.as_ref(), FEATURE_MAC) {
        Some(features) => features,
        None => {
            trace!(0, "virtio-net: the device would not agree on features");
            return false;
        }
    };

    /* A legacy device's header has no num_buffers field, and every
     * descriptor built below is that much shorter. */
    let hdr_size = if transport.is_legacy() { HDR_SIZE_LEGACY } else { HDR_SIZE_MODERN };

    let rx_size = transport.select_queue(RX_QUEUE);
    let tx_size = transport.select_queue(TX_QUEUE);
    if rx_size == 0 || tx_size == 0 {
        trace!(0, "virtio-net: the device is missing a queue ({} {})", rx_size, tx_size);
        virtio::failed(transport.as_ref());
        return false;
    }

    let rx_queue = match Queue::new(rx_size) {
        Some(queue) => queue,
        None => {
            virtio::failed(transport.as_ref());
            return false;
        }
    };
    let tx_queue = match Queue::new(tx_size) {
        Some(queue) => queue,
        None => {
            virtio::failed(transport.as_ref());
            return false;
        }
    };

    let (rx_hdr, tx_hdr) = match (DmaBuffer::new(1), DmaBuffer::new(1)) {
        (Some(rx), Some(tx)) => (rx, tx),
        _ => {
            virtio::failed(transport.as_ref());
            return false;
        }
    };

    let mut mac = [0u8; 6];
    if features & FEATURE_MAC != 0 {
        for (i, byte) in mac.iter_mut().enumerate() {
            *byte = transport.config_read8(i);
        }
    }

    /* Two descriptors to a slot, so the ring holds half as many slots as it
     * has descriptors. */
    let slots = core::cmp::min(rx_size as usize / 2, MAX_RX_SLOTS);

    let mut name = [0u8; 8];
    name[..3].copy_from_slice(b"eth");
    name[3] = b'0' + index as u8;

    let net = Box::new(Net {
        transport,
        name,
        mac,
        hdr_size,
        handle: NetDeviceHandle::placeholder(),
        msix: false,
        rx_hdr,
        tx_hdr,
        rx: UnsafeCell::new(RxState {
            queue: rx_queue,
            slots,
            frames: [const { None }; MAX_RX_SLOTS],
            slot_of_head: [NO_SLOT; virtio::MAX_DESCRIPTORS as usize],
        }),
        tx: UnsafeCell::new(TxState {
            queue: tx_queue,
            frames: [const { None }; MAX_TX_SLOTS],
            slot_of_head: [NO_SLOT; virtio::MAX_DESCRIPTORS as usize],
            free: if MAX_TX_SLOTS >= 32 { u32::MAX } else { (1 << MAX_TX_SLOTS) - 1 },
        }),
        rx_packets: AtomicU64::new(0),
        tx_packets: AtomicU64::new(0),
        rx_dropped: AtomicU64::new(0),
        rx_empty: AtomicU32::new(slots as u32),
        _irq: Irq::None,
    });

    let raw = Box::into_raw(net);
    let transport = unsafe { (*raw).transport.as_ref() };

    /* The interrupt before the queues are enabled: a modern virtio-pci
     * device is told which vector serves a queue as the queue starts. */
    let (irq, msix_entry) = arm_interrupt(transport, source, raw);
    unsafe {
        (*raw)._irq = irq;
        (*raw).msix = msix_entry.is_some();
    }

    {
        let rx = unsafe { &*(*raw).rx.get() };
        let tx = unsafe { &*(*raw).tx.get() };
        transport.setup_queue(RX_QUEUE, &virtio::QueueLayout {
            size: rx.queue.size(),
            desc: rx.queue.desc_phys(),
            driver: rx.queue.avail_phys(),
            device: rx.queue.used_phys(),
            msix: msix_entry,
        });
        transport.setup_queue(TX_QUEUE, &virtio::QueueLayout {
            size: tx.queue.size(),
            desc: tx.queue.desc_phys(),
            driver: tx.queue.avail_phys(),
            device: tx.queue.used_phys(),
            msix: msix_entry,
        });
    }

    virtio::driver_ok(transport);

    /* Frames for the device to receive into, before it is told anything has
     * arrived to be received. */
    {
        let device = unsafe { &*raw };
        let rx = unsafe { &mut *(*raw).rx.get() };
        if device.refill_rx(rx) {
            transport.notify(RX_QUEUE);
        }
    }

    let ops = net::NetDeviceOps {
        name: unsafe { core::ptr::addr_of!((*raw).name) as *const u8 },
        mac,
        flush_tx,
        process_rx,
        ctx: raw as *mut u8,
    };

    match net::register(&ops) {
        Some(handle) => {
            unsafe { (*raw).handle = handle };
            /* What QEMU's user-mode networking hands out: without it a boot
             * that runs no DHCP has no address at all, and the UDP shell is
             * how such a boot is reached. */
            handle.set_ip(DEFAULT_IP);

            DEVICES.store(index + 1, Ordering::Release);
            trace!(0, "virtio-net: {} is up, mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, {} receive slots",
                core::str::from_utf8(&name[..4]).unwrap_or("?"),
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], slots);
            true
        }
        None => {
            trace!(0, "virtio-net: the device table would not take another card");
            virtio::failed(transport);
            unsafe { drop(Box::from_raw(raw)) };
            false
        }
    }
}

fn arm_interrupt(
    transport: &dyn Transport, source: IrqSource, net: *mut Net,
) -> (Irq, Option<u16>) {
    if let Some(table) = transport.msix_table() {
        match MsixInterrupt::register(table, 0, interrupt, net as *mut u8) {
            Some(irq) => {
                transport.use_msix(0);
                return (Irq::Msix(irq), Some(0));
            }
            None => trace!(0, "virtio-net: no MSI-X slot left, falling back on the line"),
        }
    }

    let registered = match source {
        IrqSource::Pci(dev) => LegacyInterrupt::register_level(&dev, interrupt, net as *mut u8),
        IrqSource::Line(line) => LegacyInterrupt::register_irq(line, interrupt, net as *mut u8),
    };

    match registered {
        Some(irq) => (Irq::Legacy(irq), None),
        None => {
            trace!(0, "virtio-net: the device has no interrupt");
            (Irq::None, None)
        }
    }
}

/// The virtio-net cards on the PCI bus. Called from the boot path.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub extern "C" fn rust_virtio_net_init() {
    use virtio::pci::PciTransport;

    for device in [pci::device::VIRTIO_NET, pci::device::VIRTIO_NET_MODERN] {
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

            trace!(0, "virtio-net: {} virtio-pci at {:02x}:{:02x}.{}",
                if transport.is_legacy() { "legacy" } else { "modern" },
                dev.bus, dev.slot, dev.func);

            start(Box::new(transport), IrqSource::Pci(dev));
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[no_mangle]
pub extern "C" fn rust_virtio_net_init() {}

/// The same for the virtio-mmio windows the device tree described.
///
/// # Safety
/// `slots` points at `count` slots, each naming a mapped register window.
#[no_mangle]
pub unsafe extern "C" fn rust_virtio_net_init_mmio(slots: *const Slot, count: usize) {
    if slots.is_null() {
        return;
    }

    let slots = unsafe { core::slice::from_raw_parts(slots, count) };
    for slot in slots {
        if DEVICES.load(Ordering::Relaxed) >= MAX_DEVICES {
            break;
        }
        if MmioTransport::device_id(slot) != virtio::device::NET {
            continue;
        }

        let transport = match MmioTransport::probe(slot) {
            Some(transport) => transport,
            None => continue,
        };

        trace!(0, "virtio-net: virtio-mmio at {:#x}, irq {}", slot.base, slot.int_id);
        start(Box::new(transport), IrqSource::Line(slot.int_id as u8));
    }
}
