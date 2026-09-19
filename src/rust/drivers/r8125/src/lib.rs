/* Realtek RTL8125 2.5GbE driver for NOS.
 *
 * Covers PCI 10EC:8125 -- RTL8125A (XID 0x609) and RTL8125B (XID 0x641), the
 * onboard NIC on most current consumer B-series boards.  The RTL8168 driver
 * next door does not work on this chip: the interrupt registers moved and
 * widened, the TX doorbell moved, and the chip comes up with a multi-queue
 * RX block that has to be switched off (see regs.rs).
 *
 * Architecture, matching the r8168 driver:
 *  - PCI probe scans for 10EC:8125; each match calls init_device().
 *  - The 64 KiB MMIO BAR (BAR 2) is mapped for register access.
 *  - TX: the net stack calls flush_tx() under the device's transmit lock.
 *    It drains the software queue into TX descriptors and rings the doorbell.
 *    TX reaping happens at the head of flush_tx, never in the ISR.
 *  - RX: the ISR raises softirq TYPE_NET_RX; the net layer then calls
 *    process_rx() from the softirq task, which harvests descriptors, hands
 *    frames up and reposts fresh buffers.
 *  - Interrupts: MSI-X vector 0 when the device offers a table (this chip
 *    has 32 vectors), otherwise legacy INTx.  A single vector carries every
 *    event, as it does in the vendor driver.
 *
 * Locking:
 *  - The TX ring is the driver's `NetDriver::Tx`: flush_tx is handed it, under
 *    the device's transmit lock, and nothing else can reach it.  The ISR
 *    never touches it.
 *  - The RX ring is its `NetDriver::Rx`: process_rx is handed it, in the one
 *    softirq task, and nothing else can reach it.  The state dump reads the
 *    descriptors -- which are the chip's as much as ours, so shared cells --
 *    and where the poll last said it was.
 *  - What is left in the device itself -- registers, counters -- is what the
 *    ISR shares with both, and is `&self`.
 */

#![no_std]
extern crate alloc;

use alloc::boxed::Box;

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use kcore::cmd::{Command, Output};
use kcore::once::Once;
use kcore::sync::IrqSpinLock;
use kcore::{dma, interrupt, io, msix, pci, softirq, trace};
use net::{Frame, FrameQueue, NetDriver, RxQueue, TxQueue};

mod desc;
mod hw;
mod regs;

use desc::{RxRing, RxView, TxRing, RING_PAGES, RING_SIZE};
use hw::Chip;
use regs::*;

/* ================================================================== */
/* Module-level device registry (same pattern as the nvme and r8168 drivers) */

const MAX_DEVICES: usize = 4;
static DEVICES: [Once<&'static R8125Device>; MAX_DEVICES] = {
    const NONE: Once<&'static R8125Device> = Once::new();
    [NONE; MAX_DEVICES]
};
static DEVICE_COUNT: AtomicU32 = AtomicU32::new(0);

/* The whole 64 KiB register window.  Unlike the 8168's 256 bytes, this chip
 * keeps the queue, RSS and EEE blocks high in the BAR. */
const BAR_MAP_PAGES: usize = 16;
const PAGE_SIZE: usize = 4096;
const _: () = assert!(BAR_MAP_PAGES * PAGE_SIZE >= REG_SPACE_USED);

/* PCI command register bits used on the INTx path */
const PCI_COMMAND: u16 = 0x04;
const PCI_COMMAND_INTX_DISABLE: u16 = 1 << 10;

/* ================================================================== */
/* Device structure */

/* The interrupts a device has.  Field order is the drop order, and it
 * matters: the handler goes before the table its vector is an entry of. */
struct Irqs {
    /* 1st: unregister the ISR (no further callbacks after this) */
    _msix_irq: Option<msix::MsixInterrupt>,
    /* 2nd: tear down the MSI-X table, masking its entries */
    _msix_table: Option<msix::MsixTable>,
    /* 3rd: the legacy INTx slot, if that is the path in use */
    _intx: Option<interrupt::LegacyInterrupt>,
}

/* What the interrupt handler, the transmit path, the receive poll and the
 * state dump share: registers, counters, and the poll's word on where it is.
 * The rings are not here -- each belongs to the one path that is handed it
 * (see `NetDriver` below).
 *
 * A device lives for good: the net layer never gives one back, and the chip
 * has been told where its rings are.  Shutting down is `quiesce`. */
struct R8125Device {
    regs: io::MmioRegion,
    _bar_mapping: dma::PhysMapping,
    /* The interrupts, until shutdown takes them out to unregister them. */
    irqs: IrqSpinLock<Option<Irqs>>,
    /* The receive descriptors, for the state dump. */
    rx_view: RxView,
    /* Where the poll last left the receive ring, for the same. */
    rx_head: AtomicU32,
    rx_head_posted: AtomicBool,
    /* Statistics, atomic so any context can update them */
    tx_packets: AtomicU64,
    rx_packets: AtomicU64,
    rx_dropped: AtomicU64,
}

impl R8125Device {
    fn quiesce(&self) {
        /* Stop the TX/RX DMA engines and mask every interrupt source before
         * the handler goes.  The chip must not DMA into rings nothing is
         * looking after any more, and a last in-flight interrupt must not
         * find a half-torn-down device. */
        self.regs.write8(CMD_REG, 0);
        self.regs.write32(INTR_MASK, 0);
        self.regs.write32(INTR_STATUS, u32::MAX);

        /* Unregistered outside the lock: that is a call into the kernel's
         * interrupt tables, which is nothing to make under a spinlock. */
        let irqs = self.irqs.lock().take();
        drop(irqs);
    }

    /// Arm exactly `bits` in the mask register.
    ///
    /// The register is the only record of what is armed. An in-memory copy
    /// alongside it would be a second source of truth that the ISR and the
    /// poll update without a common lock, and the two disagreeing is a lost
    /// interrupt: the ISR decides whether a status bit is a signal it will be
    /// given by looking at the mask, and a stale "masked" there makes it
    /// return without acknowledging anything.
    fn arm(&self, bits: u32) {
        self.regs.write32(INTR_MASK, bits)
    }

    /// Where the poll leaves the ring, for the state dump.
    fn publish_rx(&self, ring: &RxRing) {
        let head = ring.head();
        self.rx_head.store(head as u32, Ordering::Relaxed);
        self.rx_head_posted.store(!ring.is_empty_slot(head), Ordering::Relaxed);
    }
}

/* ================================================================== */
/* Public entry points called from kernel/src/lib.rs */

pub fn init() {
    match Command::register("nicdump", "nicdump - r8125 chip and ring state", dump) {
        /* The command is the kernel's own and stays for good. */
        Ok(cmd) => core::mem::forget(cmd),
        Err(_) => trace!(0, "r8125: cannot register the nicdump command"),
    }

    let mut start: usize = 0;
    loop {
        match pci::find_device_from(PCI_VENDOR_REALTEK, PCI_DEVICE_RTL8125, start) {
            None => break,
            Some((idx, dev)) => {
                trace!(
                    0,
                    "r8125: found RTL8125 at {:02x}:{:02x}.{} rev {:02x} irq={}",
                    dev.bus,
                    dev.slot,
                    dev.func,
                    dev.revision,
                    dev.irq_line
                );
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
    trace!(0, "r8125: shutdown complete, count={}", count);
}

/* ================================================================== */
/* Device initialisation */

fn init_device(pci_dev: &pci::PciDevice) {
    /* Claim a slot before touching the hardware.  Every later failure path
     * either happens before the DMA engines are started or goes through the
     * device's `quiesce`, which stops them first.
     * Init is single-threaded, so this index is stable, and DEVICE_COUNT is
     * only advanced once registration has succeeded. */
    let idx = DEVICE_COUNT.load(Ordering::Relaxed);
    if idx as usize >= MAX_DEVICES {
        trace!(0, "r8125: too many devices (max {})", MAX_DEVICES);
        return;
    }

    pci_dev.enable_bus_mastering();

    let bar_phys = find_mmio_bar(pci_dev);
    if bar_phys == 0 {
        trace!(0, "r8125: no MMIO BAR found, skipping device");
        return;
    }

    let bar_mapping = match dma::PhysMapping::map(bar_phys, BAR_MAP_PAGES) {
        Some(m) => m,
        None => {
            trace!(0, "r8125: failed to map MMIO BAR at {:#x}", bar_phys);
            return;
        }
    };
    let regs = io::MmioRegion::new(bar_mapping.as_mut_ptr(), BAR_MAP_PAGES * PAGE_SIZE);

    /* --- Identify the revision: it selects three of the OCP values --- */
    let xid = hw::read_xid(&regs);
    let chip = match hw::identify(&regs) {
        Some(c) => c,
        None => {
            /* A revision newer than we know about is far more likely to be
             * 8125B-shaped than to need a different driver.  Say so and go on. */
            trace!(0, "r8125: unknown XID {:#05x}, driving it as an RTL8125B", xid);
            Chip::B
        }
    };
    trace!(
        0,
        "r8125: {} (xid {:#05x}) MMIO BAR at {:#x}",
        chip.as_str(),
        xid,
        bar_phys
    );

    /* --- Take the MAC away from firmware, then soft-reset it --- */
    hw::hw_init(&regs, chip);
    if !hw::reset(&regs) {
        trace!(0, "r8125: chip reset timed out");
        return;
    }

    let mac = match read_mac(&regs) {
        Some(m) => m,
        None => {
            trace!(0, "r8125: no valid station address in the eFuse copy or the RAR");
            return;
        }
    };
    trace!(
        0,
        "r8125: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );

    /* --- Descriptor rings: one page each = 256 descriptors --- */
    let tx_dma = match dma::DmaBuffer::new(RING_PAGES) {
        Some(d) => d,
        None => {
            trace!(0, "r8125: TX ring alloc failed");
            return;
        }
    };
    let rx_dma = match dma::DmaBuffer::new(RING_PAGES) {
        Some(d) => d,
        None => {
            trace!(0, "r8125: RX ring alloc failed");
            return;
        }
    };

    let (tx_ring, mut rx_ring) = match (TxRing::new(tx_dma), RxRing::new(rx_dma)) {
        (Some(tx), Some(rx)) => (tx, rx),
        _ => {
            trace!(0, "r8125: no memory for the rings' bookkeeping");
            return;
        }
    };

    for i in 0..RING_SIZE {
        match Frame::alloc_rx(RX_BUF_SIZE) {
            Some(frame) => rx_ring.post(i, frame),
            None => {
                trace!(0, "r8125: RX frame alloc failed at slot {}", i);
                /* Partially filled ring is fine: process_rx reposts empty
                 * slots at its loop head once memory frees up. */
                break;
            }
        }
    }

    /* --- Bring-up.  Config registers stay unlocked across the whole
     * chip-specific sequence and the ring programming, as the vendor
     * sequence expects. --- */
    regs.write8(CFG9346, CFG9346_UNLOCK);

    hw::aspm_disable(&regs);
    hw::hw_start(&regs, chip);

    /* Put the address into the receive filter before RX is switched on. */
    write_mac(&regs, &mac);

    /* Frames larger than this are dropped by the chip, so one frame never
     * spans two descriptors and process_rx can treat every harvest as a
     * complete packet. */
    regs.write16(RX_MAX_SIZE, RX_MAX_SIZE_VAL);
    /* No MaxTxPacketSize (0xEC) write: that register belongs to the
     * 8101/8168 generations and the vendor 8125 path never touches it. */

    /* Ring base addresses: high half first.  The chip latches the pair on
     * the write to the low half. */
    let tx_phys = tx_ring.phys;
    regs.write32(TNPDS_HI, (tx_phys >> 32) as u32);
    regs.write32(TNPDS_LO, tx_phys as u32);

    let rx_phys = rx_ring.phys;
    regs.write32(RDSAR_HI, (rx_phys >> 32) as u32);
    regs.write32(RDSAR_LO, rx_phys as u32);

    regs.write8(CFG9346, CFG9346_LOCK);

    /* Descriptors are already in memory; make sure they are visible to the
     * chip before its DMA engines are switched on. */
    kcore::barrier::dma_wmb();

    /* Flush the posted configuration writes by reading one register back,
     * so none of them is still in flight when the engines start. */
    let _ = regs.read8(CMD_REG);

    regs.write8(CMD_REG, CMD_TX_EN | CMD_RX_EN);

    /* RX: default descriptor prefetch depth, unlimited burst, and on the
     * 8125B the pause-slot fix. */
    let mut rx_cfg = RX_CFG_FETCH_DFLT | RX_CFG_DMA_BURST;
    if chip == Chip::B {
        rx_cfg = rx_cfg | RX_CFG_PAUSE_SLOT_ON;
    }
    regs.write32(RX_CONFIG, rx_cfg);

    regs.write32(TX_CONFIG, TX_CONFIG_VAL);

    /* Accept every multicast group, then the address filter itself. */
    for i in 0..8 {
        regs.write8(MAR0 + i, 0xFF);
    }
    let rx_cfg = (regs.read32(RX_CONFIG) & !RX_CFG_ACCEPT_MASK) | RX_ACCEPT_BITS;
    regs.write32(RX_CONFIG, rx_cfg);

    /* Nothing may reach a handler that does not exist yet. */
    regs.write32(INTR_MASK, 0);
    regs.write32(INTR_STATUS, u32::MAX);

    /* Name the device with the slot claimed at entry: the net layer traces
     * the name at registration time. */
    let mut name_buf = [0u8; 16];
    let name = write_device_name(&mut name_buf, idx);

    /* The device goes where it will stay, so that the interrupt handler has
     * somewhere to be pointed at; the rings stay here until the net layer is
     * told, last of all, and become its to lend -- each to the one path that
     * touches it.  Nothing calls into any of it until then. */
    let rx_view = rx_ring.view();
    let head_posted = !rx_ring.is_empty_slot(rx_ring.head());
    let dev: &'static R8125Device = Box::leak(Box::new(R8125Device {
        regs,
        _bar_mapping: bar_mapping,
        irqs: IrqSpinLock::new(None),
        rx_view,
        rx_head: AtomicU32::new(0),
        rx_head_posted: AtomicBool::new(head_posted),
        tx_packets: AtomicU64::new(0),
        rx_packets: AtomicU64::new(0),
        rx_dropped: AtomicU64::new(0),
    }));

    if !attach_interrupt(pci_dev, dev) {
        trace!(0, "r8125: no interrupt could be registered");
        dev.quiesce();
        return;
    }

    /* Arm the sources we handle. */
    dev.regs.write32(INTR_MASK, INTR_MASK_BITS);

    trace_link(&dev.regs);

    if net::register(name, mac, dev, tx_ring, rx_ring).is_none() {
        trace!(0, "r8125: NetDevice registration failed");
        dev.quiesce();
        return;
    }

    /* Commit the slot only once everything has succeeded. */
    DEVICE_COUNT.store(idx + 1, Ordering::Relaxed);
    /* The slot is this device's own: init is single-threaded. */
    let _ = DEVICES[idx as usize].set(dev);
    trace!(0, "r8125: registered as {}", name);
}

/* ================================================================== */
/* Helpers */

/// Attach an interrupt to the device: MSI-X vector 0 if the chip offers a
/// table, otherwise legacy INTx.  Returns false if neither worked.
fn attach_interrupt(pci_dev: &pci::PciDevice, dev: &'static R8125Device) -> bool {
    /* One vector carries every event on this chip, exactly as in the vendor
     * driver: the 32 MSI-X entries only become useful with RSS and multiple
     * queues, which this driver does not use. */
    if let Some(table) = msix::MsixTable::new(pci_dev) {
        match msix::MsixInterrupt::register_for(&table, 0, dev, isr) {
            Some(irq) => {
                trace!(
                    0,
                    "r8125: MSI-X vector={} ({} entries available)",
                    irq.vector(),
                    table.table_size()
                );
                *dev.irqs.lock() = Some(Irqs {
                    _msix_irq: Some(irq),
                    _msix_table: Some(table),
                    _intx: None,
                });
                return true;
            }
            None => {
                trace!(0, "r8125: MSI-X entry 0 unavailable, falling back to INTx");
                /* `table` is dropped here.  EnableVector never ran, so MSI-X
                 * was never enabled in config space and INTx is still live. */
            }
        }
    }

    /* Firmware can hand the device over with INTx masked in the command
     * register; the MSI-X path clears that implicitly, this one must not
     * assume it. */
    let cmd = pci_dev.read_config16(PCI_COMMAND);
    if cmd & PCI_COMMAND_INTX_DISABLE != 0 {
        pci_dev.write_config16(PCI_COMMAND, cmd & !PCI_COMMAND_INTX_DISABLE);
    }

    match interrupt::LegacyInterrupt::register_level_for(pci_dev, dev, isr) {
        Some(irq) => {
            trace!(0, "r8125: INTx vector={} (irq {})", irq.vector(), pci_dev.irq_line);
            *dev.irqs.lock() = Some(Irqs {
                _msix_irq: None,
                _msix_table: None,
                _intx: Some(irq),
            });
            true
        }
        None => false,
    }
}

/// Locate the register BAR.  Every RTL8125 seen so far puts registers in
/// BAR 2 (BAR 0 is I/O, BAR 4 holds the MSI-X table); BAR 0 is checked as a
/// fallback in case a board wires it differently.
fn find_mmio_bar(dev: &pci::PciDevice) -> u64 {
    for bar in [2u8, 0] {
        let raw = dev.get_bar(bar);
        if raw & 1 != 0 {
            continue; /* I/O BAR */
        }
        if raw & !0xF == 0 {
            continue; /* unassigned */
        }
        let is64 = (raw >> 1) & 0x3 == 2;
        return if is64 {
            dev.get_bar64(bar)
        } else {
            (raw & !0xF) as u64
        };
    }
    0
}

/// True for an address that can belong to this station: not a group address
/// (which also rules out the all-ones a dead register reads back) and not
/// all-zero.
fn is_valid_mac(mac: &[u8; 6]) -> bool {
    if mac[0] & 1 != 0 {
        return false;
    }
    mac.iter().any(|&b| b != 0)
}

/// Read the station address.
///
/// The receive-address registers are not the authoritative copy on this
/// chip: the address loaded from the eFuse lives in a backup block at
/// MAC0_BKP, and the RAR can come out of reset holding something else --
/// which is why the vendor driver reads the backup first and programs the
/// RAR from it.  The RAR is still worth trying as a fallback, since
/// firmware may have set it up before handing the chip over.
///
/// Unlike the vendor driver this does not fall back to a random address:
/// on a NIC whose identity cannot be read, a made-up one would silently
/// produce a different DHCP lease on every boot instead of an obvious
/// failure.
fn read_mac(regs: &io::MmioRegion) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];

    for i in 0..6 {
        mac[i] = regs.read8(MAC0_BKP + i);
    }
    if is_valid_mac(&mac) {
        return Some(mac);
    }
    trace!(0, "r8125: eFuse address copy invalid, trying the RAR");

    for i in 0..6 {
        mac[i] = regs.read8(IDR0 + i);
    }
    if is_valid_mac(&mac) {
        return Some(mac);
    }
    None
}

/// Program the receive-address registers, so the hardware unicast filter
/// matches frames addressed to this station.  Without this the chip would
/// accept only broadcast and multicast.  The high half goes first; the chip
/// latches the pair on the write to the low half, and each write is flushed
/// by a read before the next.  Config registers must already be unlocked.
fn write_mac(regs: &io::MmioRegion, mac: &[u8; 6]) {
    regs.write32(IDR4, (mac[4] as u32) | ((mac[5] as u32) << 8));
    let _ = regs.read8(CMD_REG);

    regs.write32(
        IDR0,
        (mac[0] as u32)
            | ((mac[1] as u32) << 8)
            | ((mac[2] as u32) << 16)
            | ((mac[3] as u32) << 24),
    );
    let _ = regs.read8(CMD_REG);
}

/* Report what the PHY says about the link.  Called once at probe and again
 * on every link-change interrupt. */
fn trace_link(regs: &io::MmioRegion) {
    let phy = regs.read8(PHY_STATUS);
    trace!(
        0,
        "r8125: link {} {} (phy {:#04x})",
        if phy & PHY_LINK_UP != 0 { "up" } else { "down" },
        if phy & PHY_FULL_DUPLEX != 0 { "full-duplex" } else { "half-duplex" },
        phy
    );
}

/* Write "eth0".."eth3" into `buf` */
fn write_device_name(buf: &mut [u8; 16], idx: u32) -> &str {
    struct BufWriter<'a> {
        buf: &'a mut [u8; 16],
        pos: usize,
    }
    impl<'a> Write for BufWriter<'a> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for &b in s.as_bytes() {
                if self.pos + 1 >= self.buf.len() {
                    break;
                }
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

/* Receive-side error events seen by the ISR. Static rather than per-device:
   there is one of these cards in the machine that matters, and a counter that
   needs no device pointer can be read from anywhere. */
static RX_ERR_EVENTS: AtomicU64 = AtomicU64::new(0);

/* How the receive poll is being driven, which is the question a throughput
 * ceiling of "polls per second times budget" asks and nothing so far could
 * answer. POLLS counts entries into the poll; BUDGET_HITS counts the ones
 * that ran out of budget and asked for another pass instead of arming the
 * chip. If nearly every poll is a budget hit, the cadence is the softirq
 * loop's; if nearly none are, it is the chip's interrupt rate. */
static RX_POLLS: AtomicU64 = AtomicU64::new(0);
static RX_BUDGET_HITS: AtomicU64 = AtomicU64::new(0);

/* The sources a receive poll silences. Everything else -- transmit, link
 * change -- keeps interrupting, because nothing is polling those. */
const RX_INTR_BITS: u32 = ISR_ROK | ISR_RER | ISR_RDU | ISR_RX_FIFO_OVER;

/// Frames one poll takes before yielding, so that the dispatch which runs
/// after the harvest -- and everything else on this CPU -- gets a turn. The
/// receive queue the harvest feeds holds 256; draining a whole ring into it
/// without ever returning would overflow it and drop the excess.
const RX_BUDGET: u32 = 64;

fn isr(dev: &'static R8125Device) {
    /* flush_tx or process_rx may be running on another CPU right now (a
     * per-CPU interrupt disable does not exclude them): what is shared with
     * them is the device, not the rings. The ISR only touches MMIO registers
     * and atomics. */
    let regs = &dev.regs;

    /* Receive interrupts are silenced here and armed again by the poll, once
     * it has drained the ring. Without that, this card interrupts once per
     * packet -- measured at 0.93 interrupts per packet delivered -- and under
     * overload it interrupts far more often than it delivers: 9.48 per packet
     * at the point where throughput had collapsed from 25000 packets a second
     * to 15000. The work is the same either way; what changes is how much of
     * the CPU is left to do it.
     *
     * The loop exits on `status & mask`, not on `status`. The chip raises
     * MSI-X on the 0->1 transition of that product, so a bit set in the
     * status but not armed is not a signal that is coming -- and looping
     * until a masked receive bit cleared would spin for as long as packets
     * keep arriving. */
    const MAX_ROUNDS: u32 = 32;
    let mut round = 0;

    loop {
        let mask = regs.read32(INTR_MASK);
        let status = regs.read32(INTR_STATUS);

        if status == u32::MAX {
            /* All-ones is what a vanished device reads back, not a status
             * with every event set at once. */
            return;
        }

        if status & mask == 0 {
            return;
        }

        regs.write32(INTR_STATUS, status);

        if status & ISR_LINK_CHG != 0 {
            trace_link(regs);
        }

        /* Not in INTR_MASK_BITS, but the chip still latches it: a set bit
         * here means a bus error the driver should not silently ignore. */
        if status & ISR_SYS_ERR != 0 {
            trace!(0, "r8125: fatal PCI system error in ISR");
        }

        if status & ISR_TER != 0 {
            trace!(0, "r8125: TX error in ISR");
        }

        if status & (ISR_RDU | ISR_RX_FIFO_OVER | ISR_RER) != 0 {
            let n = RX_ERR_EVENTS.fetch_add(1, Ordering::Relaxed);
            if n < 10 {
                trace!(0, "r8125: rx error in ISR, status 0x{:x} (event {})",
                    status, n + 1);
            }
        }

        /* TX reaping stays in flush_tx -- doing it here would race a
         * flush_tx running on another CPU. Raising the softirq drains frames
         * that piled up in the stack's transmit queue while the ring was full. */
        if status & (ISR_TOK | ISR_TDU | ISR_TER) != 0 {
            softirq::raise(softirq::TYPE_NET_TX);
        }

        /* Hand the receive side to the poll and go quiet on it. Raised
         * whenever anything at all arrived, not only on the receive bits: a
         * harvest that finds nothing is cheap, and it is one less thing this
         * register can lose. */
        dev.arm(mask & !RX_INTR_BITS);
        softirq::raise(softirq::TYPE_NET_RX);

        round += 1;
        if round >= MAX_ROUNDS {
            regs.write32(INTR_STATUS, u32::MAX);
            return;
        }
    }
}

/* ================================================================== */
/* State dump */

/* A window into the chip, for a machine that has stopped receiving and can
 * still be typed at. Six explanations for that stall were built by reasoning
 * about what the hardware must be doing; every one was wrong. This reads it.
 *
 * CMD_RX_EN is the first thing to look at: if the chip has cleared it, the
 * receiver is off and no amount of draining or reposting will bring it back
 * -- which is the one thing six guesses about this stall never checked.
 * "posted" and "opts1" say whose the head descriptor is -- ours and unposted,
 * or the chip's and never written. */
fn dump(_args: &str, out: &mut Output) {
    let dev = match DEVICES[0].get() {
        Some(dev) => *dev,
        None => {
            let _ = writeln!(out, "nicdump: no r8125");
            return;
        }
    };

    let regs = &dev.regs;
    let bit = |word: u32, mask: u32| (word & mask != 0) as u32;

    let cmd = regs.read8(CMD_REG) as u32;
    let intr_status = regs.read32(INTR_STATUS);
    let intr_mask = regs.read32(INTR_MASK);
    let rx_config = regs.read32(RX_CONFIG);

    /* Where the poll last said it was, and the descriptor the chip would
     * fill next read straight out of the ring, as it is this instant -- not
     * out of anything the poll remembers. */
    let head = dev.rx_head.load(Ordering::Relaxed);
    let posted = dev.rx_head_posted.load(Ordering::Relaxed);
    let opts1 = dev.rx_view.opts1(head as usize);

    let _ = writeln!(out, "cmd 0x{:X} rx-en {} tx-en {}",
        cmd, bit(cmd, CMD_RX_EN as u32), bit(cmd, CMD_TX_EN as u32));
    let _ = writeln!(out, "isr 0x{:X} imr 0x{:X} rxcfg 0x{:X}", intr_status, intr_mask, rx_config);
    let _ = writeln!(out, "rx head {} posted {} opts1 0x{:X} own {}",
        head, posted as u32, opts1, bit(opts1, RX_OWN));
    let _ = writeln!(out, "rx packets {} dropped {} err events {}",
        dev.rx_packets.load(Ordering::Relaxed), dev.rx_dropped.load(Ordering::Relaxed),
        RX_ERR_EVENTS.load(Ordering::Relaxed));

    /* A ceiling that reads as polls-per-second times budget is either the
     * softirq loop's cadence or the chip's interrupt rate; these tell which. */
    let _ = writeln!(out, "rx polls {}, of them budget-limited {}",
        RX_POLLS.load(Ordering::Relaxed), RX_BUDGET_HITS.load(Ordering::Relaxed));
}

/* ================================================================== */
/* RX path: called from the softirq task by the net layer */

impl R8125Device {
    fn poll_rx(&self, ring: &mut RxRing, up: &mut RxQueue<'_>) {
        /* The second half of what the ISR started when it silenced the receive
         * sources: drain, then arm them again.
         *
         * Three things this has to get right, each of which is a stall if it is
         * got wrong.
         *
         * A budget. The harvest feeds a queue of 256 that is drained and
         * dispatched only after this function returns, so a poll that ran until
         * the ring was empty would overflow that queue under load and drop
         * everything past it -- while holding the CPU that has to do the
         * dispatching. On the budget the poll yields with the sources still
         * silent and raises its own softirq, which is how it stays scheduled
         * without needing an interrupt. Every descriptor taken counts against it,
         * including the ones thrown away as errors: a chip producing those
         * steadily would otherwise keep this loop forever.
         *
         * Acknowledging before arming. The chip signals on the 0->1 transition of
         * `(status & mask)`. Arming while the status still carries the receive
         * bits leaves that product already non-zero, and no later packet can then
         * make it transition -- exactly the lost-interrupt shape that made this
         * machine deaf for a day. Clearing them first means the next packet is a
         * genuine 0->1 whatever the chip does about unmasking.
         *
         * Re-checking after arming. A packet landing between the last empty
         * harvest and the write to the mask register was already counted in the
         * status just cleared, so nothing would come for it. */
        const MAX_POLLS: u32 = 8;
        let mut polls = 0;

        /* Harvested frames wait here until the drain ends, so the receive
         * queue's lock is taken once for the batch instead of once per
         * frame. Sized by the budget, which is what bounds the drain. */
        let mut batch = FrameQueue::new();

        loop {
            RX_POLLS.fetch_add(1, Ordering::Relaxed);

            let mut taken = 0u32;
            let mut budget_hit = false;

            loop {
                if taken >= RX_BUDGET {
                    budget_hit = true;
                    break;
                }

                let idx = ring.head();

                /* Refill a slot an earlier allocation failure left empty: the
                 * chip stalls on a descriptor it does not own, so RX makes no
                 * progress until the slot is posted again. */
                if ring.is_empty_slot(idx) {
                    match Frame::alloc_rx(RX_BUF_SIZE) {
                        Some(frame) => ring.post(idx, frame),
                        None => break, /* still no memory; try again later */
                    }
                }

                let (mut frame, opts1) = match ring.harvest() {
                    None => break,
                    Some(pair) => pair,
                };

                taken += 1;

                let rx_len = opts1 & RX_LEN_MASK;
                let whole_frame = opts1 & RX_FF != 0 && opts1 & RX_LF != 0;
                if opts1 & RX_ERR_MASK != 0 || !whole_frame || rx_len < 4 {
                    /* Error frame, a fragment of a multi-descriptor frame (the
                     * RX_MAX_SIZE filter should prevent those), or a runt:
                     * drop it and give the buffer straight back to the chip. */
                    self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                    frame.set_len(0);
                    ring.post(idx, frame);
                    continue;
                }

                /* The reported length includes the 4-byte CRC. */
                let data_len = (rx_len - 4) as usize;
                frame.set_len(data_len);
                self.rx_packets.fetch_add(1, Ordering::Relaxed);

                batch.push(frame);

                match Frame::alloc_rx(RX_BUF_SIZE) {
                    Some(new_frame) => ring.post(idx, new_frame),
                    None => {
                        /* Under memory pressure leave the slot empty; the
                         * refill at the top of this loop posts it once
                         * allocation works again. */
                        self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            /* Hand the harvest over in one piece. Done before the status is
             * touched so that nothing sits in a local array while the chip is
             * being told it may signal again. */
            up.deliver(&mut batch);

            /* Clear what the chip has reported, so the next packet is a
             * transition rather than an addition to a status already set. */
            self.regs.write32(INTR_STATUS, RX_INTR_BITS);

            if budget_hit {
                RX_BUDGET_HITS.fetch_add(1, Ordering::Relaxed);

                /* Still ours to finish. Stay silent, come back through the
                 * softirq, and let the dispatch that follows this harvest --
                 * and everything else on this CPU -- have its turn. */
                softirq::raise(softirq::TYPE_NET_RX);
                return;
            }

            /* Round again while the ring still has frames, with the receive
             * sources left masked -- the repeating path touches no device
             * register at all (has_work reads the descriptor's OWN bit out of
             * DMA memory). This used to arm here and mask again a handful of
             * instructions later, which reopened the window on every single
             * round; at line rate a packet lands in it every time, and the
             * chip answered with 7.5M interrupts for 38.7M frames -- one per
             * five, each costing an interrupt entry and two register reads on
             * the CPU already busy draining the ring. */
            if !ring.has_work() {
                /* Empty: hand the ring back to the interrupt. */
                self.arm(INTR_MASK_BITS);

                /* Re-checked after arming. A packet landing between the
                 * harvest above and the write to the mask register was
                 * already counted in the status cleared at the top of this
                 * round, so nothing would come for it. */
                if !ring.has_work() {
                    return;
                }

                /* Raced: it is ours to take, so go silent again. */
                self.arm(INTR_MASK_BITS & !RX_INTR_BITS);
            }

            polls += 1;
            if polls >= MAX_POLLS {
                /* Work left and out of rounds -- which is also how a run of
                 * failed frame allocations looks, since an empty head slot
                 * reads as work to do. Come back through the softirq, silent,
                 * exactly as the budget does above: arming with frames
                 * already waiting would be arming onto a status the chip has
                 * set again, and no later packet could make that a 0->1
                 * transition. */
                softirq::raise(softirq::TYPE_NET_RX);
                return;
            }
        }
    }
}

/* ================================================================== */
/* TX path: called by the net stack under the device's transmit lock;
 * RX path: the poll above, and then its word on where it stopped */

impl NetDriver for R8125Device {
    type Tx = TxRing;
    type Rx = RxRing;

    fn flush_tx(&'static self, ring: &mut TxRing, stack: &mut TxQueue<'_>) {
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
            /* Doorbell.  On the 8125 this is a 16-bit write of bit 0 to
             * 0x90 -- the 8168's 8-bit NPQ at 0x38 does nothing here.
             * Ordered after the descriptor stores by submit()'s dma_wmb. */
            self.regs.write16(TX_POLL, TX_POLL_KICK);
        }
    }

    fn process_rx(&'static self, ring: &mut RxRing, up: &mut RxQueue<'_>) {
        self.poll_rx(ring, up);
        self.publish_rx(ring);
    }
}
