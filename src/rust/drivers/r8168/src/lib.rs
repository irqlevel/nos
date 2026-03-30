/* RTL8111/8168 Gigabit Ethernet driver for NOS.
 *
 * Supports Realtek RTL8168 (PCI 10EC:8168) as found in Hetzner EX44/AX41.
 *
 * Architecture:
 *  - PCI probe scans for 10EC:8168 devices; each match calls init_device().
 *  - MMIO BAR 2 (32-bit memory) is mapped for register access.
 *  - TX: flush_tx() is called by the C++ net stack under TxQueueLock.
 *    It drains the TX queue, fills TX descriptors, and kicks the hardware.
 *    The ISR reaps completed TX descriptors and releases shadow frames.
 *  - RX: the ISR raises softirq TYPE_NET_RX on ROK/RxOverflow.
 *    The C++ net layer calls process_rx() on every registered RustNetDevice
 *    from the softirq task.  process_rx() harvests received descriptors,
 *    calls enqueue_rx(), and reposts fresh RX frames.
 *  - Interrupts: legacy INTx (RTL8168 rarely exposes MSI-X; use LegacyInterrupt).
 *
 * Locking:
 *  - tx_ring: accessed only from flush_tx (under C++ TxQueueLock).  The ISR
 *    does NOT touch tx_ring; TX reaping is done at the start of flush_tx.
 *  - rx_ring: accessed only from process_rx (single softirq task), no lock needed.
 */

#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use core::fmt::Write;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use kcore::{trace, dma, io, interrupt, net, pci, softirq};

mod desc;
mod regs;

use desc::{RxRing, TxRing, RING_SIZE};
use regs::*;

/* ================================================================== */
/* Module-level device registry (same pattern as nvme driver) */

const MAX_DEVICES: usize = 4;
static DEVICES: [AtomicPtr<R8168Device>; MAX_DEVICES] = {
    const NULL: AtomicPtr<R8168Device> = AtomicPtr::new(core::ptr::null_mut());
    [NULL; MAX_DEVICES]
};
static DEVICE_COUNT: AtomicU32 = AtomicU32::new(0);

/* Number of pages to map for MMIO BAR (RTL8168 register space is 256 bytes;
 * 1 page is sufficient and avoids over-mapping). */
const BAR_MAP_PAGES: usize = 1;

/* ================================================================== */
/* Device structure */

/* Field declaration order matters for Drop: Rust drops fields in order.
 * We must unregister the interrupt (_irq) before freeing the DMA rings,
 * and free the DMA rings before unmapping MMIO (_bar_mapping).
 * The Drop impl additionally masks hardware interrupts via INTR_MASK=0
 * BEFORE any fields are dropped, preventing any last in-flight ISR from
 * accessing freed memory. */
struct R8168Device {
    /* 1st dropped: unregister ISR (no new ISR callbacks after this) */
    _irq:         interrupt::LegacyInterrupt,
    /* 2nd/3rd dropped: free DMA descriptor rings (safe; no ISR running) */
    tx_ring:      TxRing,
    rx_ring:      RxRing,
    net_handle:   net::NetDeviceHandle, /* no Drop; just a usize */
    mac:          [u8; 6],
    name_buf:     [u8; 16],
    /* Statistics (atomic so ISR can update without the tx_lock) */
    tx_packets:   AtomicU64,
    rx_packets:   AtomicU64,
    rx_dropped:   AtomicU64,
    /* Last dropped: unmap MMIO (safe; no rings or ISR left) */
    _bar_mapping: dma::PhysMapping,
    regs:         io::MmioRegion, /* raw ptr; no Drop -- must be last */
}

impl Drop for R8168Device {
    fn drop(&mut self) {
        /* Mask all interrupts at the hardware level before any fields are
         * dropped.  This prevents a last in-flight interrupt (already
         * delivered to the CPU but not yet handled) from calling the ISR
         * after we start freeing resources.  The interrupt gate ensures
         * interrupts are disabled in the ISR, so there is no race with
         * an ISR that is currently executing when Drop is called from
         * kernel shutdown context (interrupts enabled). */
        self.regs.write16(INTR_MASK, 0);
    }
}

/* ================================================================== */
/* Public entry points called from kernel/src/lib.rs */

pub fn init() {
    let mut start: usize = 0;
    loop {
        match pci::find_device_from(PCI_VENDOR_REALTEK, PCI_DEVICE_RTL8168, start) {
            None => break,
            Some((idx, dev)) => {
                trace!(0, "r8168: found RTL8168 at {:02x}:{:02x}.{} irq={}",
                    dev.bus, dev.slot, dev.func, dev.irq_line);
                init_device(&dev);
                start = idx + 1;
            }
        }
    }
}

pub fn shutdown() {
    let count = (DEVICE_COUNT.load(Ordering::Relaxed) as usize).min(MAX_DEVICES);
    for i in 0..count {
        let raw = DEVICES[i].swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !raw.is_null() {
            unsafe { drop(Box::from_raw(raw)) };
        }
    }
    trace!(0, "r8168: shutdown complete, count={}", count);
}

/* ================================================================== */
/* Device initialisation */

fn init_device(pci_dev: &pci::PciDevice) {
    /* --- Enable bus mastering for DMA --- */
    pci_dev.enable_bus_mastering();

    /* --- Find the MMIO BAR ---
     * RTL8168 provides I/O ports at BAR 0 (type bit = 1) and 32-bit MMIO
     * at BAR 2 (type bit = 0).  We only need the MMIO BAR. */
    let bar_phys = find_mmio_bar(pci_dev);
    if bar_phys == 0 {
        trace!(0, "r8168: no MMIO BAR found, skipping device");
        return;
    }
    trace!(0, "r8168: MMIO BAR at {:#x}", bar_phys);

    /* --- Map MMIO BAR --- */
    let bar_mapping = match dma::PhysMapping::map(bar_phys, BAR_MAP_PAGES) {
        Some(m) => m,
        None => {
            trace!(0, "r8168: failed to map MMIO BAR");
            return;
        }
    };
    let regs = io::MmioRegion::new(bar_mapping.as_mut_ptr(), BAR_MAP_PAGES * 4096);

    /* --- Chip reset --- */
    if !chip_reset(&regs) {
        trace!(0, "r8168: chip reset timed out");
        return;
    }

    /* --- Read MAC address (stable across reset) --- */
    let mac = read_mac(&regs);
    trace!(0, "r8168: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

    /* --- Allocate TX and RX descriptor rings (1 page each = 256 descs) --- */
    let tx_dma = match dma::DmaBuffer::new(1) {
        Some(d) => d,
        None => {
            trace!(0, "r8168: TX DMA alloc failed");
            return;
        }
    };
    let rx_dma = match dma::DmaBuffer::new(1) {
        Some(d) => d,
        None => {
            trace!(0, "r8168: RX DMA alloc failed");
            return;
        }
    };

    let tx_ring = TxRing::new(tx_dma);
    let mut rx_ring = RxRing::new(rx_dma);

    /* --- Fill RX ring with pre-allocated NetFrame buffers --- */
    for i in 0..RING_SIZE {
        match net::NetFrame::alloc_rx(RX_BUF_SIZE) {
            Some(frame) => rx_ring.post(i, frame),
            None => {
                trace!(0, "r8168: RX frame alloc failed at slot {}", i);
                /* Ring is partially filled; stop here.  process_rx will not
                 * advance past unfilled slots (frames[i] == 0). */
                break;
            }
        }
    }

    /* --- Unlock config registers before programming descriptor addresses --- */
    regs.write8(CFG9346, CFG9346_UNLOCK);

    /* --- Program TX descriptor ring base address --- */
    let tx_phys = tx_ring.dma.phys();
    regs.write32(TNPDS_LO, tx_phys as u32);
    regs.write32(TNPDS_HI, (tx_phys >> 32) as u32);

    /* --- Program RX descriptor ring base address --- */
    let rx_phys = rx_ring.dma.phys();
    regs.write32(RDSAR_LO, rx_phys as u32);
    regs.write32(RDSAR_HI, (rx_phys >> 32) as u32);

    /* --- Configure TX --- */
    regs.write32(TX_CONFIG, TX_CONFIG_VAL);
    regs.write8(MAX_TX_PKT_SIZE, MAX_TX_PKT_VAL);

    /* --- Configure RX (accept broadcast + multicast + unicast) --- */
    regs.write32(RX_CONFIG, RX_CONFIG_VAL);

    /* --- Set max RX packet size (1518 bytes = standard Ethernet frame) --- */
    regs.write16(RX_MAX_SIZE, 0x05F6);

    /* --- Enable C+ mode: 64-bit DMA + multiple R/W --- */
    regs.write16(CPCR, CPCR_VAL);

    /* --- Re-lock config registers --- */
    regs.write8(CFG9346, CFG9346_LOCK);

    /* --- Accept all multicast (MAR0-7 = 0xFF..FF) --- */
    for i in 0..8 {
        regs.write8(MAR0 + i, 0xFF);
    }

    /* Device index is allocated later, after successful registration.
     * Use a placeholder for the name; it will be overwritten once we
     * know the real index. */
    let name_buf = [0u8; 16];

    /* Box the device so it has a stable address for ISR callbacks.
     * _irq and net_handle are placeholders (handle=0, no-op Drop); they are
     * replaced with real handles before the device is visible to the kernel. */
    let mut dev_box = Box::new(R8168Device {
        _irq:       interrupt::LegacyInterrupt::empty(),
        tx_ring,
        rx_ring,
        net_handle: net::NetDeviceHandle::placeholder(),
        mac,
        name_buf,
        tx_packets: AtomicU64::new(0),
        rx_packets: AtomicU64::new(0),
        rx_dropped: AtomicU64::new(0),
        _bar_mapping: bar_mapping,
        regs,
    });

    let ctx_ptr = dev_box.as_mut() as *mut R8168Device as *mut u8;

    /* --- Register legacy interrupt --- */
    let irq = match interrupt::LegacyInterrupt::register_level(pci_dev, r8168_isr, ctx_ptr) {
        Some(i) => i,
        None => {
            trace!(0, "r8168: failed to register interrupt");
            return;
        }
    };
    trace!(0, "r8168: IRQ vector={}", irq.vector());
    dev_box._irq = irq;

    /* --- Enable TX + RX --- */
    dev_box.regs.write8(CMD_REG, CMD_TX_EN | CMD_RX_EN);

    /* --- Set interrupt mask --- */
    dev_box.regs.write16(INTR_MASK, INTR_MASK_BITS);

    /* --- Build device name as null-terminated C string --- */
    let raw = Box::into_raw(dev_box);

    /* --- Register as NetDevice --- */
    let ops = net::NetDeviceOps {
        name:       unsafe { (*raw).name_buf.as_ptr() },
        mac:        unsafe { (*raw).mac },
        flush_tx:   r8168_flush_tx,
        process_rx: r8168_process_rx,
        ctx:        raw as *mut u8,
    };
    let handle = match net::register(&ops) {
        Some(h) => h,
        None => {
            trace!(0, "r8168: NetDevice registration failed");
            unsafe { drop(Box::from_raw(raw)) };
            return;
        }
    };
    unsafe { (*raw).net_handle = handle };

    /* Allocate device slot only after everything has succeeded so that
     * DEVICE_COUNT is never inflated by failed initialisations. */
    let idx = DEVICE_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx as usize >= MAX_DEVICES {
        trace!(0, "r8168: too many devices (max {})", MAX_DEVICES);
        unsafe { drop(Box::from_raw(raw)) };
        return;
    }
    write_device_name(unsafe { &mut (*raw).name_buf }, idx);

    DEVICES[idx as usize].store(raw, Ordering::Release);
    trace!(0, "r8168: registered as {} (irq={})",
        core::str::from_utf8(unsafe { &(*raw).name_buf }).unwrap_or("?"),
        pci_dev.irq_line);
}

/* ================================================================== */
/* Helpers */

/* Probe PCI BARs to find the 32-bit MMIO BAR.
 * RTL8168: BAR 0 is I/O (bit 0 = 1), BAR 2 is 32-bit MMIO (bit 0 = 0). */
fn find_mmio_bar(dev: &pci::PciDevice) -> u64 {
    for bar in [2u8, 1, 0] {
        let raw = dev.get_bar(bar);
        if raw & 1 == 0 && raw > 0xFFF {
            /* Memory BAR with non-trivial address */
            let bits21 = (raw >> 1) & 0x3;
            if bits21 == 2 {
                /* 64-bit BAR */
                return dev.get_bar64(bar);
            } else {
                /* 32-bit BAR */
                return (raw & !0xF) as u64;
            }
        }
    }
    0
}

/* Perform a chip software reset.  Returns true if the chip came out of reset
 * within the timeout period. */
fn chip_reset(regs: &io::MmioRegion) -> bool {
    regs.write8(CMD_REG, CMD_RESET);
    /* Poll until Reset bit self-clears; typical latency is < 10 µs */
    for _ in 0..10_000 {
        if regs.read8(CMD_REG) & CMD_RESET == 0 {
            return true;
        }
        /* Busy-wait; we can't sleep here (called before interrupt init) */
        core::hint::spin_loop();
    }
    false
}

/* Read the 6-byte MAC address from IDR0..IDR5 */
fn read_mac(regs: &io::MmioRegion) -> [u8; 6] {
    [
        regs.read8(IDR0),
        regs.read8(IDR1),
        regs.read8(IDR2),
        regs.read8(IDR3),
        regs.read8(IDR4),
        regs.read8(IDR5),
    ]
}

/* Write device name "eth0\0" .. "eth3\0" into `buf` */
fn write_device_name(buf: &mut [u8; 16], idx: u32) {
    struct BufWriter<'a> { buf: &'a mut [u8; 16], pos: usize }
    impl<'a> Write for BufWriter<'a> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for &b in s.as_bytes() {
                if self.pos + 1 >= self.buf.len() { break; }
                self.buf[self.pos] = b;
                self.pos = self.pos + 1;
            }
            Ok(())
        }
    }
    let mut w = BufWriter { buf, pos: 0 };
    let _ = write!(w, "eth{}", idx);
    /* null-terminate */
    w.buf[w.pos] = 0;
}

/* ================================================================== */
/* Interrupt service routine */

extern "C" fn r8168_isr(ctx: *mut u8) {
    let dev = unsafe { &mut *(ctx as *mut R8168Device) };

    /* Read and clear (write-1-to-clear) interrupt status */
    let status = dev.regs.read16(INTR_STATUS);
    if status == 0 {
        return; /* spurious */
    }
    dev.regs.write16(INTR_STATUS, status);

    if status & ISR_SYS_ERR != 0 {
        trace!(0, "r8168: fatal PCI system error in ISR");
    }

    if status & ISR_TER != 0 {
        trace!(0, "r8168: TX error in ISR");
    }
    /* TX completion (TOK/TDU) is handled by reap_completed() at the start
     * of flush_tx — NOT here.  Reaping in the ISR would race with flush_tx
     * on a different CPU (ISR-disable is per-CPU only). */

    if status & (ISR_ROK | ISR_RER | ISR_RX_OVFLOW | ISR_RX_FIFO_OV) != 0 {
        /* Defer frame processing to softirq task.  The C++ net layer calls
         * process_rx() on all registered RustNetDevices from the softirq. */
        softirq::raise(softirq::TYPE_NET_RX);
    }
}

/* ================================================================== */
/* TX path: called by C++ net stack under TxQueueLock */

extern "C" fn r8168_flush_tx(ctx: *mut u8) {
    let dev = unsafe { &mut *(ctx as *mut R8168Device) };

    /* Reap any already-completed TX slots to make room.
     * Safe without a lock: flush_tx is the only code path that accesses
     * tx_ring, and the C++ TxQueueLock serialises callers. */
    dev.tx_ring.reap_completed();

    let mut submitted: u32 = 0;
    loop {
        if !dev.tx_ring.has_space() {
            break;
        }
        match dev.net_handle.tx_dequeue() {
            None => break,
            Some(frame) => {
                dev.tx_ring.submit(frame);
                submitted = submitted + 1;
            }
        }
    }

    if submitted > 0 {
        dev.tx_packets.fetch_add(submitted as u64, Ordering::Relaxed);
        /* Kick the TX DMA engine.  Must be written after the descriptor
         * stores (already guaranteed by tx_ring.submit's compiler_fence). */
        dev.regs.write8(TX_POLL, TX_POLL_NPQ);
    }
}

/* ================================================================== */
/* RX path: called from softirq task by the C++ net layer */

extern "C" fn r8168_process_rx(ctx: *mut u8) {
    let dev = unsafe { &mut *(ctx as *mut R8168Device) };

    /* Walk the RX ring until we hit a hardware-owned descriptor */
    loop {
        let (mut frame, rx_len) = match dev.rx_ring.harvest() {
            None => break,
            Some(pair) => pair,
        };

        if rx_len < 4 {
            /* Discard runt frames (hardware glitch or error) */
            dev.rx_dropped.fetch_add(1, Ordering::Relaxed);
            /* Re-post the frame to hardware */
            let idx = (dev.rx_ring.head + RING_SIZE - 1) % RING_SIZE; /* prev slot */
            match net::NetFrame::alloc_rx(RX_BUF_SIZE) {
                Some(new_frame) => dev.rx_ring.post(idx, new_frame),
                None => {
                    /* No memory: repost the original frame with reset length */
                    frame.set_len(0);
                    dev.rx_ring.post(idx, frame);
                }
            }
            continue;
        }

        /* The length field includes the 4-byte CRC; strip it */
        let data_len = (rx_len - 4) as usize;
        frame.set_len(data_len);
        dev.rx_packets.fetch_add(1, Ordering::Relaxed);

        /* Enqueue to kernel net stack (transfers ownership) */
        dev.net_handle.enqueue_rx(frame);

        /* Refill this slot: compute the descriptor index we just harvested
         * (head has already advanced in harvest()). */
        let prev = (dev.rx_ring.head + RING_SIZE - 1) % RING_SIZE;
        match net::NetFrame::alloc_rx(RX_BUF_SIZE) {
            Some(new_frame) => dev.rx_ring.post(prev, new_frame),
            None => {
                /* Memory pressure: leave slot empty; process_rx will skip it
                 * next time (frames[prev] == 0). */
                dev.rx_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
