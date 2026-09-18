/* RTL8111/8168 Gigabit Ethernet driver for NOS.
 *
 * Supports Realtek RTL8168 (PCI 10EC:8168) as found in Hetzner EX44/AX41.
 *
 * Architecture:
 *  - PCI probe scans for 10EC:8168 devices; each match calls init_device().
 *  - MMIO BAR 2 (32-bit memory) is mapped for register access.
 *  - TX: flush_tx() is called by the net stack under the device's transmit
 *    lock. It reaps completed TX descriptors, drains the TX queue, fills TX
 *    descriptors, and kicks the hardware.
 *  - RX: the ISR raises softirq TYPE_NET_RX on every interrupt.
 *    The net layer calls process_rx() on every registered device from the
 *    softirq task.  process_rx() harvests received descriptors, hands the
 *    frames up, and reposts fresh RX frames.
 *  - Interrupts: legacy INTx (RTL8168 rarely exposes MSI-X; use LegacyInterrupt).
 *
 * Locking:
 *  - The TX ring is the driver's `NetDriver::Tx`: flush_tx is handed it, under
 *    the device's transmit lock, and nothing else can reach it.  The ISR does
 *    NOT touch it; TX reaping is done at the start of flush_tx.
 *  - The RX ring is its `NetDriver::Rx`: process_rx is handed it, from the one
 *    softirq task, and nothing else can reach it.  No lock needed.
 *  - What is left in the device itself -- registers and counters -- is what
 *    the ISR shares with both, and is `&self`.
 */

#![no_std]
extern crate alloc;

use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use kcore::net::{NetBinding, NetDriver, RxQueue, TxQueue};
use kcore::once::Once;
use kcore::sync::IrqSpinLock;
use kcore::{trace, dma, io, interrupt, net, pci, softirq};

mod desc;
mod regs;

use desc::{RxRing, TxRing, RING_PAGES, RING_SIZE};
use regs::*;

/* ================================================================== */
/* Module-level device registry (same pattern as nvme driver) */

const MAX_DEVICES: usize = 4;
static DEVICES: [Once<&'static R8168Device>; MAX_DEVICES] = {
    const NONE: Once<&'static R8168Device> = Once::new();
    [NONE; MAX_DEVICES]
};
static DEVICE_COUNT: AtomicU32 = AtomicU32::new(0);

/* Number of pages to map for MMIO BAR (RTL8168 register space is 256 bytes;
 * 1 page is sufficient and avoids over-mapping). */
const BAR_MAP_PAGES: usize = 1;

/* ================================================================== */
/* Device structure */

/* What the interrupt handler, the transmit path and the receive poll share:
 * registers and counters.  The rings are not here -- each belongs to the one
 * path that is handed it (see `NetDriver` below).
 *
 * A device lives for good: the net layer never gives one back, and the chip
 * has been told where its rings are.  Shutting down is `quiesce`. */
struct R8168Device {
    regs:         io::MmioRegion,
    _bar_mapping: dma::PhysMapping,
    /* The interrupt, until shutdown takes it out to unregister it. */
    irq:          IrqSpinLock<Option<interrupt::LegacyInterrupt>>,
    /* Statistics (atomic so every path can update them without a lock) */
    tx_packets:   AtomicU64,
    rx_packets:   AtomicU64,
    rx_dropped:   AtomicU64,
}

impl R8168Device {
    fn quiesce(&self) {
        /* Stop the TX/RX DMA engines, then mask all interrupts at the
         * hardware level, before the handler goes.  This stops the NIC
         * DMAing into memory the kernel is about to stop looking after and
         * keeps a last in-flight interrupt (already delivered to the CPU but
         * not yet handled) from finding nothing behind its vector. */
        self.regs.write8(CMD_REG, 0);
        self.regs.write16(INTR_MASK, 0);

        /* Unregistered outside the lock: that is a call into the kernel's
         * interrupt table, which is nothing to make under a spinlock. */
        let irq = self.irq.lock().take();
        drop(irq);
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
    for slot in &DEVICES[..count] {
        if let Some(dev) = slot.get() {
            dev.quiesce();
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
    let tx_dma = match dma::DmaBuffer::new(RING_PAGES) {
        Some(d) => d,
        None => {
            trace!(0, "r8168: TX DMA alloc failed");
            return;
        }
    };
    let rx_dma = match dma::DmaBuffer::new(RING_PAGES) {
        Some(d) => d,
        None => {
            trace!(0, "r8168: RX DMA alloc failed");
            return;
        }
    };

    let (tx_ring, mut rx_ring) = match (TxRing::new(tx_dma), RxRing::new(rx_dma)) {
        (Some(tx), Some(rx)) => (tx, rx),
        _ => {
            trace!(0, "r8168: no memory for the rings' bookkeeping");
            return;
        }
    };

    /* --- Fill RX ring with pre-allocated NetFrame buffers --- */
    for i in 0..RING_SIZE {
        match net::NetFrame::alloc_rx(RX_BUF_SIZE) {
            Some(frame) => rx_ring.post(i, frame),
            None => {
                trace!(0, "r8168: RX frame alloc failed at slot {}", i);
                /* Ring is partially filled; stop here.  process_rx refills
                 * empty slots at its loop head once memory is available. */
                break;
            }
        }
    }

    /* --- Unlock config registers before programming descriptor addresses --- */
    regs.write8(CFG9346, CFG9346_UNLOCK);

    /* --- Program TX descriptor ring base address --- */
    let tx_phys = tx_ring.phys;
    regs.write32(TNPDS_LO, tx_phys as u32);
    regs.write32(TNPDS_HI, (tx_phys >> 32) as u32);

    /* --- Program RX descriptor ring base address --- */
    let rx_phys = rx_ring.phys;
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

    /* Name the device with the next free index before registering (the
     * net layer traces the name at registration time).  Init runs
     * single-threaded, so the provisional index is stable; DEVICE_COUNT
     * itself is only incremented after registration succeeds, so it is
     * never inflated by failed initialisations. */
    let idx = DEVICE_COUNT.load(Ordering::Relaxed);
    if idx as usize >= MAX_DEVICES {
        trace!(0, "r8168: too many devices (max {})", MAX_DEVICES);
        return;
    }
    let mut name_buf = [0u8; 16];
    let name = write_device_name(&mut name_buf, idx);

    /* The device goes where it will stay, so that the interrupt handler has
     * somewhere to be pointed at; the rings go with it, each to the one path
     * that will be handed it.  Nothing calls into any of it until the
     * interrupt is registered and, last of all, the net layer is told. */
    let binding = NetBinding::new(R8168Device {
        regs,
        _bar_mapping: bar_mapping,
        irq:        IrqSpinLock::new(None),
        tx_packets: AtomicU64::new(0),
        rx_packets: AtomicU64::new(0),
        rx_dropped: AtomicU64::new(0),
    }, tx_ring, rx_ring);
    let dev = binding.driver();

    /* --- Register legacy interrupt --- */
    let irq = match interrupt::LegacyInterrupt::register_level_for(pci_dev, dev, isr) {
        Some(i) => i,
        None => {
            trace!(0, "r8168: failed to register interrupt");
            dev.quiesce();
            return;
        }
    };
    trace!(0, "r8168: IRQ vector={}", irq.vector());
    *dev.irq.lock() = Some(irq);

    /* --- Enable TX + RX --- */
    dev.regs.write8(CMD_REG, CMD_TX_EN | CMD_RX_EN);

    /* --- Set interrupt mask --- */
    dev.regs.write16(INTR_MASK, INTR_MASK_BITS);

    /* --- Register as NetDevice --- */
    if binding.register(name, mac).is_none() {
        trace!(0, "r8168: NetDevice registration failed");
        dev.quiesce();
        return;
    }

    /* Commit the device slot only after everything has succeeded. */
    DEVICE_COUNT.store(idx + 1, Ordering::Relaxed);
    /* The slot is this device's own: init is single-threaded. */
    let _ = DEVICES[idx as usize].set(dev);
    trace!(0, "r8168: registered as {} (irq={})", name, pci_dev.irq_line);
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

/* Write device name "eth0" .. "eth3" into `buf` */
fn write_device_name(buf: &mut [u8; 16], idx: u32) -> &str {
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
    let BufWriter { buf, pos } = w;
    core::str::from_utf8(&buf[..pos]).unwrap_or("eth?")
}

/* ================================================================== */
/* Interrupt service routine */

fn isr(dev: &'static R8168Device) {
    /* flush_tx and process_rx may be running on other CPUs at this moment
     * (per-CPU IRQ disable does not exclude them): what is shared with them
     * is the device, not the rings. The ISR only touches MMIO registers. */
    let regs = &dev.regs;

    /* Loop until the status reads back as zero. The register is
     * write-1-to-clear and the chip signals MSI on the 0->1 transition of
     * the aggregate (status & mask), so a bit the chip sets between one read
     * and its acknowledge is neither cleared nor signalled -- and every later
     * event only adds to a status that is already non-zero. That is a lost
     * interrupt with nothing left to recover it. Diagnosed on the 8125, which
     * has the same register with the same semantics; see r8125_isr. */
    const MAX_ROUNDS: u32 = 32;

    let mut round = 0;
    let mut status = regs.read16(INTR_STATUS);
    if status == 0 {
        return; /* spurious */
    }

    loop {
        regs.write16(INTR_STATUS, status);

        if status & ISR_SYS_ERR != 0 {
            trace!(0, "r8168: fatal PCI system error in ISR");
        }

        if status & ISR_TER != 0 {
            trace!(0, "r8168: TX error in ISR");
        }

        /* TX completion: reaping stays in flush_tx (reaping here would race
         * it on another CPU), but schedule a drain so frames left in the
         * stack's transmit queue while the ring was full are flushed now
         * that slots have freed -- without this they stall until an
         * unrelated future transmit. */
        if status & (ISR_TOK | ISR_TDU | ISR_TER) != 0 {
            softirq::raise(softirq::TYPE_NET_TX);
        }

        /* On every pass. Deferred, not done here: the net layer calls
         * process_rx() on every registered device from the softirq task. */
        softirq::raise(softirq::TYPE_NET_RX);

        status = regs.read16(INTR_STATUS);
        if status == 0 {
            return;
        }

        round += 1;
        if round >= MAX_ROUNDS {
            regs.write16(INTR_STATUS, u16::MAX);
            return;
        }
    }
}

/* ================================================================== */
/* TX path: called by the net stack under the device's transmit lock;
 * RX path: called from the softirq task by the net layer */

impl NetDriver for R8168Device {
    type Tx = TxRing;
    type Rx = RxRing;

    fn flush_tx(&'static self, ring: &mut TxRing, stack: &mut TxQueue<'_>) {
        /* Reap any already-completed TX slots to make room. */
        ring.reap_completed(stack);

        let mut submitted: u32 = 0;
        loop {
            if !ring.has_space() {
                break;
            }
            match stack.dequeue() {
                None => break,
                Some(frame) => {
                    ring.submit(frame);
                    submitted = submitted + 1;
                }
            }
        }

        if submitted > 0 {
            self.tx_packets.fetch_add(submitted as u64, Ordering::Relaxed);
            /* Kick the TX DMA engine.  Must be written after the descriptor
             * stores (already guaranteed by TxRing::submit's barrier). */
            self.regs.write8(TX_POLL, TX_POLL_NPQ);
        }
    }

    fn process_rx(&'static self, ring: &mut RxRing, up: &mut RxQueue<'_>) {
        /* Walk the RX ring until we hit a hardware-owned descriptor */
        loop {
            let idx = ring.head();

            /* Refill a slot left empty by an earlier NetFrame allocation
             * failure.  The hardware stalls on a descriptor it does not own,
             * so RX cannot make progress until the slot is reposted. */
            if ring.is_empty_slot(idx) {
                match net::NetFrame::alloc_rx(RX_BUF_SIZE) {
                    Some(frame) => ring.post(idx, frame),
                    None => break, /* still no memory; retry on next softirq */
                }
            }

            let (mut frame, opts1) = match ring.harvest() {
                None => break,
                Some(pair) => pair,
            };

            let rx_len = opts1 & RX_LEN_MASK;
            let whole_frame = opts1 & RX_FF != 0 && opts1 & RX_LF != 0;
            if opts1 & RX_ERR_MASK != 0 || !whole_frame || rx_len < 4 {
                /* Error frame, multi-descriptor fragment (cannot happen while
                 * RX_MAX_SIZE < RX_BUF_SIZE, but check anyway), or runt:
                 * drop it and give the buffer straight back to hardware. */
                self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                frame.set_len(0);
                ring.post(idx, frame);
                continue;
            }

            /* The length field includes the 4-byte CRC; strip it */
            let data_len = (rx_len - 4) as usize;
            frame.set_len(data_len);
            self.rx_packets.fetch_add(1, Ordering::Relaxed);

            /* Enqueue to kernel net stack (transfers ownership) */
            up.enqueue(frame);

            /* Refill the slot we just harvested */
            match net::NetFrame::alloc_rx(RX_BUF_SIZE) {
                Some(new_frame) => ring.post(idx, new_frame),
                None => {
                    /* Memory pressure: leave the slot empty; the refill at the
                     * top of this loop reposts it once allocation succeeds. */
                    self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}
