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
 *  - TX: the C++ net stack calls flush_tx() under TxQueueLock.  It drains
 *    the software queue into TX descriptors and rings the doorbell.  TX
 *    reaping happens at the head of flush_tx, never in the ISR.
 *  - RX: the ISR raises softirq TYPE_NET_RX; the net layer then calls
 *    process_rx() from the softirq task, which harvests descriptors, hands
 *    frames up and reposts fresh buffers.
 *  - Interrupts: MSI-X vector 0 when the device offers a table (this chip
 *    has 32 vectors), otherwise legacy INTx.  A single vector carries every
 *    event, as it does in the vendor driver.
 *
 * Locking:
 *  - tx_ring is touched only by flush_tx, which the C++ TxQueueLock
 *    serialises.  The ISR never touches it.
 *  - rx_ring is touched only by process_rx, which runs in one softirq task.
 */

#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use core::fmt::Write;
use core::ptr::{addr_of, read_volatile};
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use kcore::{dma, interrupt, io, msix, net, pci, softirq, trace};

mod desc;
mod hw;
mod regs;

use desc::{RxRing, TxRing, RING_PAGES, RING_SIZE};
use hw::Chip;
use regs::*;

/* ================================================================== */
/* Module-level device registry (same pattern as the nvme and r8168 drivers) */

const MAX_DEVICES: usize = 4;
static DEVICES: [AtomicPtr<R8125Device>; MAX_DEVICES] = {
    const NULL: AtomicPtr<R8125Device> = AtomicPtr::new(core::ptr::null_mut());
    [NULL; MAX_DEVICES]
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

/* Field order is the drop order, and it matters: stop interrupt delivery
 * before freeing the DMA rings, and free the rings before unmapping the
 * registers.  Drop additionally stops the DMA engines and masks interrupts
 * in hardware before any field is dropped, so an interrupt already on its
 * way to the CPU cannot reach memory that is about to be freed. */
struct R8125Device {
    /* 1st: unregister the ISR (no further callbacks after this) */
    _msix_irq: msix::MsixInterrupt,
    /* 2nd: tear down the MSI-X table, masking its entries */
    _msix_table: Option<msix::MsixTable>,
    /* 3rd: the legacy INTx slot, if that is the path in use */
    _intx: interrupt::LegacyInterrupt,
    /* then the DMA rings -- safe now that no ISR can run */
    tx_ring: TxRing,
    rx_ring: RxRing,
    net_handle: net::NetDeviceHandle, /* no Drop; just a usize */
    mac: [u8; 6],
    name_buf: [u8; 16],
    /* Statistics, atomic so any context can update them */
    tx_packets: AtomicU64,
    rx_packets: AtomicU64,
    rx_dropped: AtomicU64,
    /* last: unmap MMIO, once nothing can reference the registers */
    _bar_mapping: dma::PhysMapping,
    regs: io::MmioRegion, /* raw pointer; no Drop -- must come last */
}

impl Drop for R8125Device {
    fn drop(&mut self) {
        /* Stop the TX/RX DMA engines and mask every interrupt source before
         * any field is dropped.  The chip must not DMA into rings we are
         * about to free, and a last in-flight interrupt must not find a
         * half-torn-down device. */
        self.regs.write8(CMD_REG, 0);
        self.regs.write32(INTR_MASK, 0);
        self.regs.write32(INTR_STATUS, u32::MAX);
    }
}

/* ================================================================== */
/* Public entry points called from kernel/src/lib.rs */

pub fn init() {
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
    for i in 0..count {
        let raw = DEVICES[i].swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !raw.is_null() {
            unsafe { drop(Box::from_raw(raw)) };
        }
    }
    trace!(0, "r8125: shutdown complete, count={}", count);
}

/* ================================================================== */
/* Device initialisation */

fn init_device(pci_dev: &pci::PciDevice) {
    /* Claim a slot before touching the hardware.  Every later failure path
     * either happens before the DMA engines are started or unwinds through
     * the device's Drop, which stops them first; bailing out after the
     * engines are running would free rings the chip is still writing to.
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

    let tx_ring = TxRing::new(tx_dma);
    let mut rx_ring = RxRing::new(rx_dma);

    for i in 0..RING_SIZE {
        match net::NetFrame::alloc_rx(RX_BUF_SIZE) {
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
    let tx_phys = tx_ring.dma.phys();
    regs.write32(TNPDS_HI, (tx_phys >> 32) as u32);
    regs.write32(TNPDS_LO, tx_phys as u32);

    let rx_phys = rx_ring.dma.phys();
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

    /* Name the device with the slot claimed at entry: the C++ side traces
     * the name at registration time. */
    let mut name_buf = [0u8; 16];
    write_device_name(&mut name_buf, idx);

    /* Box the device so ISR callbacks have a stable address.  The interrupt
     * and net handles are placeholders until the real ones are installed. */
    let mut dev_box = Box::new(R8125Device {
        _msix_irq: msix::MsixInterrupt::empty(),
        _msix_table: None,
        _intx: interrupt::LegacyInterrupt::empty(),
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

    let ctx_ptr = dev_box.as_mut() as *mut R8125Device as *mut u8;

    if !attach_interrupt(pci_dev, &mut dev_box, ctx_ptr) {
        trace!(0, "r8125: no interrupt could be registered");
        return;
    }

    /* Arm the sources we handle. */
    dev_box.regs.write32(INTR_MASK, INTR_MASK_BITS);

    trace_link(&dev_box.regs);

    let raw = Box::into_raw(dev_box);

    let ops = net::NetDeviceOps {
        name: unsafe { (*raw).name_buf.as_ptr() },
        mac: unsafe { (*raw).mac },
        flush_tx: r8125_flush_tx,
        process_rx: r8125_process_rx,
        ctx: raw as *mut u8,
    };
    let handle = match net::register(&ops) {
        Some(h) => h,
        None => {
            trace!(0, "r8125: NetDevice registration failed");
            unsafe { drop(Box::from_raw(raw)) };
            return;
        }
    };
    unsafe { (*raw).net_handle = handle };

    /* Commit the slot only once everything has succeeded. */
    DEVICE_COUNT.store(idx + 1, Ordering::Relaxed);
    DEVICES[idx as usize].store(raw, Ordering::Release);
    trace!(
        0,
        "r8125: registered as {}",
        core::str::from_utf8(unsafe { &(*raw).name_buf }).unwrap_or("?")
    );
}

/* ================================================================== */
/* Helpers */

/// Attach an interrupt to the device: MSI-X vector 0 if the chip offers a
/// table, otherwise legacy INTx.  Returns false if neither worked.
fn attach_interrupt(
    pci_dev: &pci::PciDevice,
    dev_box: &mut Box<R8125Device>,
    ctx_ptr: *mut u8,
) -> bool {
    /* One vector carries every event on this chip, exactly as in the vendor
     * driver: the 32 MSI-X entries only become useful with RSS and multiple
     * queues, which this driver does not use. */
    if let Some(table) = msix::MsixTable::new(pci_dev) {
        match msix::MsixInterrupt::register(&table, 0, r8125_isr, ctx_ptr) {
            Some(irq) => {
                trace!(
                    0,
                    "r8125: MSI-X vector={} ({} entries available)",
                    irq.vector(),
                    table.table_size()
                );
                dev_box._msix_irq = irq;
                dev_box._msix_table = Some(table);
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

    match interrupt::LegacyInterrupt::register_level(pci_dev, r8125_isr, ctx_ptr) {
        Some(irq) => {
            trace!(0, "r8125: INTx vector={} (irq {})", irq.vector(), pci_dev.irq_line);
            dev_box._intx = irq;
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

/* Write "eth0\0".."eth3\0" into `buf` */
fn write_device_name(buf: &mut [u8; 16], idx: u32) {
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
    w.buf[w.pos] = 0;
}

/* ================================================================== */
/* Interrupt service routine */

/* Receive-side error events seen by the ISR. Static rather than per-device:
   there is one of these cards in the machine that matters, and a counter that
   needs no device pointer can be read from anywhere. */
static RX_ERR_EVENTS: AtomicU64 = AtomicU64::new(0);

pub fn rx_err_events() -> u64 {
    RX_ERR_EVENTS.load(Ordering::Relaxed)
}

extern "C" fn r8125_isr(ctx: *mut u8) {
    /* Raw dereference, not `&mut`: flush_tx or process_rx may hold their own
     * reference to this device on another CPU right now (a per-CPU interrupt
     * disable does not exclude them), and two live `&mut` to one object are
     * UB.  The ISR only touches MMIO registers. */
    let dev = ctx as *mut R8125Device;
    let regs = unsafe { &(*dev).regs };

    /* The status register is write-1-to-clear, and the chip signals MSI-X on
     * the 0->1 transition of (status & mask) -- of the aggregate, not per
     * event. That combination makes a single read-then-acknowledge lose
     * interrupts for good:
     *
     *   read status            -> ROK
     *   [chip sets TOK]           aggregate already non-zero: no message
     *   write status (ROK)     -> clears ROK, leaves TOK set
     *   return                    TOK set, mask open, no message pending,
     *                             and none ever coming: the aggregate can
     *                             not go 0->1 again because it never went
     *                             back to 0.
     *
     * Every later event only adds a bit to a status that is already
     * non-zero. The bare metal machine was caught in exactly this state:
     * isr 0x40D5 with the mask open and the handler not running -- and the
     * race is hit at random, which is why the stall arrived after anything
     * from 276 packets to 44000 of the same load.
     *
     * So: loop until the status reads back as zero. Only then is the next
     * event a 0->1 transition the chip will signal. Bounded, and if the bound
     * is hit the whole register is acknowledged at once -- both softirqs are
     * raised on every pass, so no work is lost by that, and a status that
     * will not go quiet after this many rounds is a chip that is going to
     * storm regardless. */
    const MAX_ROUNDS: u32 = 32;

    let mut round = 0;
    let mut status = regs.read32(INTR_STATUS);

    if status == 0 {
        return; /* shared line, not us */
    }

    loop {
        if status == u32::MAX {
            /* All-ones is what a vanished device reads back, not a status
             * with every event set at once.  Acknowledging it would be
             * pointless and raising both softirqs would spin the net layer
             * on dead hardware. */
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

        /* Receive-side error bits, counted, the first few traced. */
        if status & (ISR_RX_OVERFLOW | ISR_RX_FIFO_OVER | ISR_RER) != 0 {
            let n = RX_ERR_EVENTS.fetch_add(1, Ordering::Relaxed);
            if n < 10 {
                trace!(0, "r8125: rx error in ISR, status 0x{:x} (event {})",
                    status, n + 1);
            }
        }

        /* TX reaping stays in flush_tx -- doing it here would race a
         * flush_tx running on another CPU.  Raising the softirq drains
         * frames that piled up in the C++ TxQueue while the ring was full;
         * without it they would wait for an unrelated future SubmitTx. */
        if status & (ISR_TOK | ISR_TDU | ISR_TER) != 0 {
            softirq::raise(softirq::TYPE_NET_TX);
        }

        /* On every pass, not only when the receive bits are set: a harvest
         * that finds nothing is cheap, and a receive wakeup that depends on
         * a particular bit being seen is one more thing this register can
         * lose. */
        softirq::raise(softirq::TYPE_NET_RX);

        status = regs.read32(INTR_STATUS);
        if status == 0 {
            return;
        }

        round += 1;
        if round >= MAX_ROUNDS {
            regs.write32(INTR_STATUS, u32::MAX);
            return;
        }
    }
}

/* ================================================================== */
/* TX path: called by the C++ net stack under TxQueueLock */

extern "C" fn r8125_flush_tx(ctx: *mut u8) {
    /* Raw pointer, not `&mut`: process_rx may hold its own reference on
     * another CPU (softirq exclusivity is per-type, and TxQueueLock does not
     * cover process_rx).  Only fields disjoint from process_rx's are touched
     * here; tx_ring is safe without a lock because flush_tx is its only
     * mutator and the C++ TxQueueLock serialises flush_tx callers. */
    let dev = ctx as *mut R8125Device;

    unsafe {
        let net = (*dev).net_handle;
        (*dev).tx_ring.reap_completed(net);

        let mut submitted: u32 = 0;
        loop {
            if !(*dev).tx_ring.has_space() {
                break;
            }
            match (*dev).net_handle.tx_dequeue() {
                None => break,
                Some(frame) => {
                    (*dev).tx_ring.submit(frame);
                    submitted = submitted + 1;
                }
            }
        }

        if submitted > 0 {
            (*dev).tx_packets.fetch_add(submitted as u64, Ordering::Relaxed);
            /* Doorbell.  On the 8125 this is a 16-bit write of bit 0 to
             * 0x90 -- the 8168's 8-bit NPQ at 0x38 does nothing here.
             * Ordered after the descriptor stores by submit()'s dma_wmb. */
            (*dev).regs.write16(TX_POLL, TX_POLL_KICK);
        }
    }
}

/* ================================================================== */
/* RX path: called from the softirq task by the C++ net layer */

/* A window into the chip, for a machine that has stopped receiving and can
 * still be typed at. Six explanations for that stall were built by reasoning
 * about what the hardware must be doing; every one was wrong. This reads it.
 *
 * CMD_RX_EN is the first thing to look at: if the chip has cleared it, the
 * receiver is off and no amount of draining or reposting will bring it back.
 * head_posted and head_opts1 say whose the head descriptor is -- ours and
 * unposted, or the chip's and never written. */
#[repr(C)]
pub struct R8125State {
    pub present: u32,
    pub cmd: u32,
    pub intr_status: u32,
    pub intr_mask: u32,
    pub rx_config: u32,
    pub rx_head: u32,
    pub head_posted: u32,
    pub head_opts1: u32,
    pub rx_err_events: u64,
    pub rx_packets: u64,
    pub rx_dropped: u64,
}

#[no_mangle]
pub extern "C" fn r8125_get_state(out: *mut R8125State) -> i32 {
    if out.is_null() {
        return -1;
    }

    let dev = DEVICES[0].load(Ordering::Acquire);
    if dev.is_null() {
        unsafe { (*out).present = 0 };
        return -1;
    }

    unsafe {
        let regs = &(*dev).regs;
        let head = (*dev).rx_ring.head;
        let posted = (*dev).rx_ring.frames[head] != 0;

        /* Read straight out of the descriptor the chip would fill next. */
        let opts1 = {
            let d = ((*dev).rx_ring.dma.as_ptr() as *const desc::RxDesc).add(head);
            read_volatile(addr_of!((*d).opts1))
        };

        (*out).present = 1;
        (*out).cmd = regs.read8(CMD_REG) as u32;
        (*out).intr_status = regs.read32(INTR_STATUS);
        (*out).intr_mask = regs.read32(INTR_MASK);
        (*out).rx_config = regs.read32(RX_CONFIG);
        (*out).rx_head = head as u32;
        (*out).head_posted = if posted { 1 } else { 0 };
        (*out).head_opts1 = opts1;
        (*out).rx_err_events = RX_ERR_EVENTS.load(Ordering::Relaxed);
        (*out).rx_packets = (*dev).rx_packets.load(Ordering::Relaxed);
        (*out).rx_dropped = (*dev).rx_dropped.load(Ordering::Relaxed);
    }

    0
}

extern "C" fn r8125_process_rx(ctx: *mut u8) {
    /* Raw pointer for the same reason as flush_tx; process_rx touches only
     * rx_ring / rx_* / net_handle, all disjoint from flush_tx's tx_ring. */
    let dev = ctx as *mut R8125Device;

    unsafe {
        loop {
            let idx = (*dev).rx_ring.head;

            /* Refill a slot an earlier allocation failure left empty: the
             * chip stalls on a descriptor it does not own, so RX makes no
             * progress until the slot is posted again. */
            if (*dev).rx_ring.frames[idx] == 0 {
                match net::NetFrame::alloc_rx(RX_BUF_SIZE) {
                    Some(frame) => (*dev).rx_ring.post(idx, frame),
                    None => break, /* still no memory; retry next softirq */
                }
            }

            let (mut frame, opts1) = match (*dev).rx_ring.harvest() {
                None => break,
                Some(pair) => pair,
            };

            let rx_len = opts1 & RX_LEN_MASK;
            let whole_frame = opts1 & RX_FF != 0 && opts1 & RX_LF != 0;
            if opts1 & RX_ERR_MASK != 0 || !whole_frame || rx_len < 4 {
                /* Error frame, a fragment of a multi-descriptor frame (the
                 * RX_MAX_SIZE filter should prevent those), or a runt: drop
                 * it and give the buffer straight back to the chip. */
                (*dev).rx_dropped.fetch_add(1, Ordering::Relaxed);
                frame.set_len(0);
                (*dev).rx_ring.post(idx, frame);
                continue;
            }

            /* The reported length includes the 4-byte CRC. */
            let data_len = (rx_len - 4) as usize;
            frame.set_len(data_len);
            (*dev).rx_packets.fetch_add(1, Ordering::Relaxed);

            (*dev).net_handle.enqueue_rx(frame);

            match net::NetFrame::alloc_rx(RX_BUF_SIZE) {
                Some(new_frame) => (*dev).rx_ring.post(idx, new_frame),
                None => {
                    /* Under memory pressure leave the slot empty; the refill
                     * at the top of this loop posts it once allocation works. */
                    (*dev).rx_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}
