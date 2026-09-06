/* Intel igb-family gigabit Ethernet driver for NOS.
 *
 * Claims 8086:10C9 (82576) and 8086:1533 (I210). The first is what QEMU
 * emulates, which is where this was written and is meant to be debugged; the
 * second is the part on real hardware. They share a register map Intel
 * publishes -- the reason this driver reads like a transcription of a
 * datasheet where the Realtek ones next door read like archaeology.
 *
 * Architecture, matching the r8125 driver:
 *  - PCI probe walks an ID table; each match calls init_device().
 *  - The register BAR (BAR 0) is mapped for MMIO access.
 *  - TX: the C++ net stack calls flush_tx() under TxQueueLock. It reaps
 *    finished descriptors, drains the software queue into the ring and
 *    writes the tail once. Reaping never happens in the ISR.
 *  - RX: the ISR masks the receive sources and raises softirq TYPE_NET_RX;
 *    process_rx() then polls to a budget, hands frames up in one batch,
 *    refills, and re-arms only on its way out.
 *  - One RX and one TX queue. The per-queue register blocks are strided, so
 *    more queues are a later change of arithmetic, not of structure.
 *
 * How the rings differ from the Realtek ones is described in desc.rs: there
 * is no OWN bit, ownership is a pair of ring pointers, and the tail is a
 * register write.
 *
 * Interrupts are legacy INTx. This part offers MSI-X, but using it means
 * routing queues to vectors through the IVAR registers and driving the
 * extended EICR block, and none of that buys anything until there is more
 * than one queue to spread.
 *
 * Locking:
 *  - tx_ring is touched only by flush_tx, which the C++ TxQueueLock
 *    serialises. The ISR never touches it.
 *  - rx_ring is touched only by process_rx, which the softirq layer runs on
 *    one CPU at a time.
 */

#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use kcore::{dma, interrupt, io, msix, net, pci, softirq, trace};

mod desc;
mod regs;

use desc::{RxRing, TxRing, RING_PAGES, RING_SIZE, RX_BUF_SIZE};
use regs::*;

/* ================================================================== */
/* Module-level device registry (same pattern as the other drivers) */

const MAX_DEVICES: usize = 4;
static DEVICES: [AtomicPtr<IgbDevice>; MAX_DEVICES] = {
    const NULL: AtomicPtr<IgbDevice> = AtomicPtr::new(core::ptr::null_mut());
    [NULL; MAX_DEVICES]
};
static DEVICE_COUNT: AtomicU32 = AtomicU32::new(0);

/* Counters worth having when something is wrong on a machine whose only
 * console is a UDP socket. */
static RX_POLLS: AtomicU64 = AtomicU64::new(0);
static RX_BUDGET_HITS: AtomicU64 = AtomicU64::new(0);
static RX_ERR_EVENTS: AtomicU64 = AtomicU64::new(0);

/* The register window. 128 KiB, which reaches the NVM block at 0x12010 as
 * well as the queue and interrupt blocks far below it. The card's BAR is
 * larger still -- 512 KiB on the I210 -- but nothing above here is touched. */
const BAR_MAP_PAGES: usize = 32;
const PAGE_SIZE: usize = 4096;
const _: () = assert!(BAR_MAP_PAGES * PAGE_SIZE >= REG_SPACE_USED);

/* Which part this is. The two share a register map; they differ in who else
 * is allowed to touch the PHY, which is enough to need naming. */
#[derive(Clone, Copy, PartialEq)]
enum Generation {
    /// 82576 and relatives. What QEMU emulates.
    I82576,
    /// I210/I211. Has manageability firmware sharing the MDIO bus.
    I210,
}

impl Generation {
    fn as_str(self) -> &'static str {
        match self {
            Generation::I82576 => "82576",
            Generation::I210 => "I210",
        }
    }
}

const SUPPORTED: [(u16, Generation); 10] = [
    (PCI_DEVICE_82576, Generation::I82576),
    (PCI_DEVICE_82576_QUAD, Generation::I82576),
    (PCI_DEVICE_82576_NS, Generation::I82576),
    (PCI_DEVICE_I210_COPPER, Generation::I210),
    (PCI_DEVICE_I210_FIBER, Generation::I210),
    (PCI_DEVICE_I210_SERDES, Generation::I210),
    (PCI_DEVICE_I210_SGMII, Generation::I210),
    (PCI_DEVICE_I210_COPPER_FLASHLESS, Generation::I210),
    (PCI_DEVICE_I210_SERDES_FLASHLESS, Generation::I210),
    (PCI_DEVICE_I211_COPPER, Generation::I210),
];

/* Descriptors taken in one pass before the poll yields, and passes before it
 * gives the CPU back through a softirq. The product bounds how long one
 * entry into process_rx can hold the CPU. */
const RX_BUDGET: u32 = 64;
const MAX_POLLS: u32 = 8;

struct IgbDevice {
    _msix_irq: msix::MsixInterrupt,
    _msix_table: Option<msix::MsixTable>,
    _intx: interrupt::LegacyInterrupt,
    /// Whether causes arrive through the extended block rather than ICR/IMS.
    /// It decides which pair of registers masks and arms the receive side.
    msix: bool,
    generation: Generation,
    phy_addr: u32,
    tx_ring: TxRing,
    rx_ring: RxRing,
    net_handle: net::NetDeviceHandle,
    mac: [u8; 6],
    name_buf: [u8; 16],
    tx_packets: AtomicU64,
    rx_packets: AtomicU64,
    rx_dropped: AtomicU64,
    _bar_mapping: dma::PhysMapping,
    regs: io::MmioRegion,
}

impl IgbDevice {
    /// Go quiet on receive for the duration of a poll.
    fn mask_rx(&self) {
        if self.msix {
            self.regs.write32(EIMC, EICR_VECTOR0);
        } else {
            self.regs.write32(IMC, RX_INTR_BITS);
        }
    }

    /// Hand the ring back to the interrupt.
    fn arm_rx(&self) {
        if self.msix {
            self.regs.write32(EIMS, EICR_VECTOR0);
        } else {
            self.regs.write32(IMS, RX_INTR_BITS);
        }
    }
}

impl Drop for IgbDevice {
    fn drop(&mut self) {
        /* Stop both engines before the rings go: the chip must not be left
         * writing into freed pages. */
        self.regs.write32(IMC, u32::MAX);
        self.regs.write32(EIMC, u32::MAX);
        let rctl = self.regs.read32(RCTL);
        self.regs.write32(RCTL, rctl & !RCTL_EN);
        let tctl = self.regs.read32(TCTL);
        self.regs.write32(TCTL, tctl & !TCTL_EN);
        self.regs.write32(RXDCTL0, 0);
        self.regs.write32(TXDCTL0, 0);
        let _ = self.regs.read32(STATUS); /* flush the posted writes */
    }
}

/* ================================================================== */
/* Small timing helpers.
 *
 * Spin rather than sleep: all of this runs during device init, some of it
 * before there is a task to sleep in. The spin budget is a backstop so a
 * counter that never advances cannot hang the boot. */

const NS_PER_US: u64 = 1000;
const SPIN_PER_US: u64 = 200;
const SPIN_MIN: u64 = 1000;

fn udelay(us: u64) {
    let budget = us.saturating_mul(SPIN_PER_US).max(SPIN_MIN);

    if kcore::hpet::is_available() {
        let start = kcore::hpet::read_ns();
        let want = us.saturating_mul(NS_PER_US);
        let mut left = budget;
        while kcore::hpet::read_ns().wrapping_sub(start) < want {
            core::hint::spin_loop();
            left = left - 1;
            if left == 0 {
                return;
            }
        }
        return;
    }

    for _ in 0..budget {
        core::hint::spin_loop();
    }
}

fn wait_for<F: FnMut() -> bool>(us: u64, tries: u32, mut cond: F) -> bool {
    for _ in 0..tries {
        if cond() {
            return true;
        }
        udelay(us);
    }
    cond()
}

/* ================================================================== */
/* Public entry points called from kernel/src/lib.rs */

pub fn init() {
    for (device_id, generation) in SUPPORTED {
        let mut start: usize = 0;
        loop {
            match pci::find_device_from(PCI_VENDOR_INTEL, device_id, start) {
                None => break,
                Some((idx, dev)) => {
                    trace!(
                        0,
                        "igb: found {} {:04x}:{:04x} at {:02x}:{:02x}.{} rev {:02x} irq={}",
                        generation.as_str(),
                        PCI_VENDOR_INTEL,
                        device_id,
                        dev.bus,
                        dev.slot,
                        dev.func,
                        dev.revision,
                        dev.irq_line
                    );
                    init_device(&dev, generation);
                    start = idx + 1;
                }
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
    trace!(0, "igb: shutdown complete, count={}", count);
}

/* ================================================================== */
/* Hardware bring-up */

/// Take the device back from whatever the firmware left running, then reset.
fn reset(regs: &io::MmioRegion) -> bool {
    /* Nothing may reach a handler that does not exist yet. */
    regs.write32(IMC, u32::MAX);
    let _ = regs.read32(ICR);

    let rctl = regs.read32(RCTL);
    regs.write32(RCTL, rctl & !RCTL_EN);
    let tctl = regs.read32(TCTL);
    regs.write32(TCTL, tctl & !TCTL_EN);
    let _ = regs.read32(STATUS);
    udelay(10_000);

    /* Let outstanding DMA finish before the reset, or the chip can come back
     * with a transaction still in flight. Not fatal if it times out -- the
     * reset is next either way -- but worth saying. */
    let ctrl = regs.read32(CTRL);
    regs.write32(CTRL, ctrl | CTRL_GIO_MASTER_DISABLE);
    if !wait_for(100, 100, || {
        regs.read32(STATUS) & STATUS_GIO_MASTER_ENABLE == 0
    }) {
        trace!(0, "igb: bus master did not go idle before reset");
    }

    let ctrl = regs.read32(CTRL);
    regs.write32(CTRL, ctrl | CTRL_RST);

    /* The datasheet asks for 1 ms of quiet before the register file is
     * readable again; CTRL_RST clears itself when the reset completes. */
    udelay(1000);
    if !wait_for(1000, 100, || regs.read32(CTRL) & CTRL_RST == 0) {
        return false;
    }

    /* The reset re-enables some sources; silence them again. */
    regs.write32(IMC, u32::MAX);
    let _ = regs.read32(ICR);

    /* The station address and a handful of other registers are reloaded from
     * the NVM as part of coming out of reset, and that takes longer than the
     * reset itself. Reading RAL0 before it finishes gets whatever was there
     * before -- which on a cold boot is zero, and a zero station address is
     * rejected further down as "no valid address", so the card would simply
     * not appear. Not fatal if it times out: the read below decides. */
    if !wait_for(1000, 100, || regs.read32(EEC) & EEC_AUTO_RD != 0) {
        trace!(0, "igb: NVM auto-read did not complete; the MAC may not be loaded");
    }

    true
}

/// Stop both DMA engines and silence the device.
///
/// Needed on every failure path taken after the receive queue is enabled.
/// The rings are local variables until the device is boxed, so returning
/// frees the pages they live in -- and a chip still holding those addresses
/// goes on writing received frames into memory the page allocator has handed
/// to somebody else. Nothing reports that; it corrupts whatever comes next.
fn stop_engines(regs: &io::MmioRegion) {
    regs.write32(IMC, u32::MAX);
    regs.write32(EIMC, u32::MAX);

    let rctl = regs.read32(RCTL);
    regs.write32(RCTL, rctl & !RCTL_EN);
    let tctl = regs.read32(TCTL);
    regs.write32(TCTL, tctl & !TCTL_EN);

    regs.write32(RXDCTL0, 0);
    regs.write32(TXDCTL0, 0);

    /* Wait for the queues to say they have stopped, rather than assuming the
     * write took effect the moment it was posted. The enable bit reads back
     * clear only once the engine is idle, and the whole point of this
     * function is that the rings are about to be freed. */
    if !wait_for(100, 100, || {
        regs.read32(RXDCTL0) & XDCTL_QUEUE_ENABLE == 0
            && regs.read32(TXDCTL0) & XDCTL_QUEUE_ENABLE == 0
    }) {
        trace!(0, "igb: queues did not report idle after being disabled");
    }
}

/// The station address, which hardware has already loaded into the first
/// receive-address register from the NVM.
fn read_mac(regs: &io::MmioRegion) -> Option<[u8; 6]> {
    let ral = regs.read32(RAL0);
    let rah = regs.read32(RAH0);

    let mac = [
        ral as u8,
        (ral >> 8) as u8,
        (ral >> 16) as u8,
        (ral >> 24) as u8,
        rah as u8,
        (rah >> 8) as u8,
    ];

    /* All-zero and all-ones are the two ways "there is nothing here" reads
     * back; a multicast bit in the first octet is not a station address. */
    let all_zero = mac.iter().all(|b| *b == 0);
    let all_ones = mac.iter().all(|b| *b == 0xFF);
    if all_zero || all_ones || mac[0] & 1 != 0 {
        return None;
    }
    Some(mac)
}

fn write_mac(regs: &io::MmioRegion, mac: &[u8; 6]) {
    let ral = (mac[0] as u32)
        | ((mac[1] as u32) << 8)
        | ((mac[2] as u32) << 16)
        | ((mac[3] as u32) << 24);
    let rah = (mac[4] as u32) | ((mac[5] as u32) << 8) | RAH_AV;

    /* Low half first: the address becomes valid on the write that sets AV. */
    regs.write32(RAL0, ral);
    regs.write32(RAH0, rah);
}

/* ================================================================== */
/* Software/firmware semaphore, I210 only.
 *
 * On this part the manageability firmware drives the same MDIO bus, so the
 * PHY is a shared resource and both sides claim it through SW_FW_SYNC. That
 * register is itself guarded by a hardware mutex in SWSM, so every claim is
 * two acquisitions deep: take the mutex, set the bit, drop the mutex.
 *
 * Everything here is bounded. A firmware that never releases its claim must
 * cost a failed bring-up and a line in the log, not a boot that stops. */

/// Take the hardware mutex guarding SW_FW_SYNC.
fn hw_semaphore_get(regs: &io::MmioRegion) -> bool {
    /* SMBI is the mutex itself: clear means nobody holds it. */
    if !wait_for(50, 200, || regs.read32(SWSM) & SWSM_SMBI == 0) {
        return false;
    }

    /* Then software's own bit, which only latches if the claim took. */
    let ok = wait_for(50, 200, || {
        let swsm = regs.read32(SWSM);
        regs.write32(SWSM, swsm | SWSM_SWESMBI);
        regs.read32(SWSM) & SWSM_SWESMBI != 0
    });

    if !ok {
        hw_semaphore_put(regs);
        return false;
    }
    true
}

fn hw_semaphore_put(regs: &io::MmioRegion) {
    let swsm = regs.read32(SWSM);
    regs.write32(SWSM, swsm & !(SWSM_SMBI | SWSM_SWESMBI));
}

/// Claim a resource in SW_FW_SYNC, waiting for firmware to let go of it.
fn swfw_acquire(regs: &io::MmioRegion, mask: u32) -> bool {
    let fwmask = mask << SWFW_FW_SHIFT;

    for _ in 0..200 {
        if !hw_semaphore_get(regs) {
            return false;
        }

        let sync = regs.read32(SW_FW_SYNC);
        if sync & (fwmask | mask) == 0 {
            regs.write32(SW_FW_SYNC, sync | mask);
            hw_semaphore_put(regs);
            return true;
        }

        /* Held by the other side: drop the mutex so it can make progress. */
        hw_semaphore_put(regs);
        udelay(5000);
    }

    false
}

fn swfw_release(regs: &io::MmioRegion, mask: u32) {
    if !hw_semaphore_get(regs) {
        /* Nothing better to do than let the claim go anyway: leaving it set
         * would lock the PHY out for good. */
        let sync = regs.read32(SW_FW_SYNC);
        regs.write32(SW_FW_SYNC, sync & !mask);
        return;
    }

    let sync = regs.read32(SW_FW_SYNC);
    regs.write32(SW_FW_SYNC, sync & !mask);
    hw_semaphore_put(regs);
}

/* ================================================================== */
/* PHY, through the MDI control register.
 *
 * One register in, one register out, and a poll for the ready bit: the MAC
 * does the MDIO bit-banging. Every access is bounded -- a PHY that never
 * answers must not hang the boot. */

fn mdic_wait(regs: &io::MmioRegion) -> Option<u32> {
    let mut val = 0u32;
    let ok = wait_for(50, 100, || {
        val = regs.read32(MDIC);
        val & MDIC_READY != 0
    });

    if !ok || val & MDIC_ERROR != 0 {
        return None;
    }
    Some(val)
}

/// Where the PHY answers on the MDIO bus.
///
/// On a copper part this selects nothing: MDICNFG.destination is clear, every
/// MDIC access goes to the integrated PHY, and the address field is ignored.
/// MDICNFG.PHYADD is for an *external* PHY on an SGMII or SerDes board, which
/// this driver does not handle. The value is reported rather than derived, so
/// a log from a board that turns out to be wired differently says so.
fn phy_address(regs: &io::MmioRegion, generation: Generation) -> u32 {
    if generation != Generation::I210 {
        return PHY_ADDR_INTERNAL;
    }

    let external = (regs.read32(MDICNFG) & MDICNFG_DESTINATION) != 0;
    if !external {
        return PHY_ADDR_INTERNAL;
    }

    let addr = (regs.read32(MDICNFG) & MDICNFG_PHY_MASK) >> MDICNFG_PHY_SHIFT;
    trace!(0, "igb: MDICNFG selects an external PHY at address {}", addr);
    addr
}

fn phy_read(regs: &io::MmioRegion, phy_addr: u32, reg: u32) -> Option<u16> {
    regs.write32(
        MDIC,
        (reg << MDIC_REG_SHIFT) | (phy_addr << MDIC_PHY_SHIFT) | MDIC_OP_READ,
    );
    mdic_wait(regs).map(|v| (v & MDIC_DATA_MASK) as u16)
}

fn phy_write(regs: &io::MmioRegion, phy_addr: u32, reg: u32, data: u16) -> bool {
    regs.write32(
        MDIC,
        (data as u32)
            | (reg << MDIC_REG_SHIFT)
            | (phy_addr << MDIC_PHY_SHIFT)
            | MDIC_OP_WRITE,
    );
    mdic_wait(regs).is_some()
}

/// Bring the link up: take the PHY out of power-down and restart
/// auto-negotiation.
///
/// This is not optional and not cosmetic. Until negotiation completes the MAC
/// leaves STATUS.LU clear, and with the link reading down the receiver drops
/// every frame before it ever looks at a descriptor -- the ring stays
/// untouched, the head pointer never moves, and nothing in the receive path
/// gives any hint why. Transmit, meanwhile, works: frames go out and replies
/// come back to a card that will not take them.
fn phy_start_link(regs: &io::MmioRegion, generation: Generation, phy_addr: u32) -> bool {
    /* On the I210 the PHY is shared with the manageability firmware. Talking
     * to it without the claim is not a race that shows up as a clean failure:
     * two masters on one MDIO bus produce reads that look like data. */
    if generation == Generation::I210 && !swfw_acquire(regs, SWFW_PHY0_SM) {
        trace!(0, "igb: could not take the PHY semaphore from firmware");
        return false;
    }

    let up = phy_start_link_locked(regs, phy_addr);

    if generation == Generation::I210 {
        swfw_release(regs, SWFW_PHY0_SM);
    }

    up
}

fn phy_start_link_locked(regs: &io::MmioRegion, phy_addr: u32) -> bool {
    let bmcr = match phy_read(regs, phy_addr, PHY_BMCR) {
        Some(v) => v,
        None => {
            trace!(0, "igb: PHY did not answer on MDIC");
            return false;
        }
    };

    /* Say what to offer before asking for a round of negotiation. With
     * auto-negotiation enabled the speed and duplex bits in BMCR are ignored
     * -- what the partner sees comes from these two registers -- and leaving
     * them at whatever the PHY powered up with is how a gigabit port and a
     * gigabit PHY settled on 10BASE-T full duplex on the first machine this
     * ran on. */
    if !phy_write(regs, phy_addr, PHY_ANAR, ANAR_ADVERTISE_ALL) {
        trace!(0, "igb: PHY advertisement write failed");
        return false;
    }

    if !phy_write(regs, phy_addr, PHY_GCTL, GCTL_1000_FULL | GCTL_1000_HALF) {
        trace!(0, "igb: PHY gigabit advertisement write failed");
        return false;
    }

    /* Speed and duplex here are what the link falls back to if the partner
     * cannot negotiate at all; the restart is what makes the advertisement
     * above take effect. */
    let want = (bmcr & !BMCR_PDOWN) | BMCR_ANENABLE | BMCR_ANRESTART
        | BMCR_FULLDPLX
        | BMCR_SPEED1000;

    if !phy_write(regs, phy_addr, PHY_BMCR, want) {
        trace!(0, "igb: PHY control write failed");
        return false;
    }

    /* Negotiation takes as long as it takes; this only waits long enough to
     * be able to say in the log whether it finished, and boots either way.
     * A link that comes up later announces itself through LSC. */
    let up = wait_for(10_000, 100, || {
        match phy_read(regs, phy_addr, PHY_BMSR) {
            /* Read twice in effect: the link bit is latching-low, so the
             * first read after a change reports the old state. */
            Some(v) => v & (BMSR_LSTATUS | BMSR_ANEGCOMPLETE) == (BMSR_LSTATUS | BMSR_ANEGCOMPLETE),
            None => false,
        }
    });

    if !up {
        trace!(0, "igb: auto-negotiation has not completed yet");
    }
    up
}

fn trace_link(regs: &io::MmioRegion) {
    let status = regs.read32(STATUS);
    if status & STATUS_LU == 0 {
        trace!(0, "igb: link down");
        return;
    }

    let speed = match status & STATUS_SPEED_MASK {
        STATUS_SPEED_10 => 10,
        STATUS_SPEED_100 => 100,
        STATUS_SPEED_1000 => 1000,
        _ => 0,
    };
    let duplex = if status & STATUS_FD != 0 { "full" } else { "half" };
    trace!(0, "igb: link up, {} Mbit/s {} duplex", speed, duplex);
}

fn find_mmio_bar(pci_dev: &pci::PciDevice) -> u64 {
    /* BAR 0 is the register window on every part in this family. Memory
     * BARs have bit 0 clear; the type field says whether it is a 64-bit
     * pair. */
    let bar0 = pci_dev.get_bar(0);
    if bar0 & 1 != 0 {
        return 0; /* an I/O BAR is not what we want */
    }
    let is_64bit = (bar0 >> 1) & 0x3 == 0x2;
    if is_64bit {
        pci_dev.get_bar64(0) & !0xFu64
    } else {
        (bar0 & !0xFu32) as u64
    }
}

/* "ethN", which is the name every NIC driver here uses and the one the
 * shell's DHCP autostart looks for by name. */
fn write_device_name(buf: &mut [u8; 16], idx: u32) {
    let name = b"eth";
    let mut i = 0;
    while i < name.len() {
        buf[i] = name[i];
        i += 1;
    }
    /* Single digit is enough: MAX_DEVICES is 4. */
    buf[i] = b'0' + (idx as u8 % 10);
    buf[i + 1] = 0;
}

/* ================================================================== */
/* Device initialisation */

fn init_device(pci_dev: &pci::PciDevice, generation: Generation) {
    /* Claim a slot before touching the hardware, as the r8125 driver does:
     * every failure below either happens before the DMA engines start or
     * unwinds through Drop, which stops them first. */
    let idx = DEVICE_COUNT.load(Ordering::Relaxed);
    if idx as usize >= MAX_DEVICES {
        trace!(0, "igb: too many devices (max {})", MAX_DEVICES);
        return;
    }

    pci_dev.enable_bus_mastering();

    let bar_phys = find_mmio_bar(pci_dev);
    if bar_phys == 0 {
        trace!(0, "igb: no MMIO BAR found, skipping device");
        return;
    }

    let bar_mapping = match dma::PhysMapping::map(bar_phys, BAR_MAP_PAGES) {
        Some(m) => m,
        None => {
            trace!(0, "igb: failed to map MMIO BAR at {:#x}", bar_phys);
            return;
        }
    };
    let regs = io::MmioRegion::new(bar_mapping.as_mut_ptr(), BAR_MAP_PAGES * PAGE_SIZE);
    trace!(0, "igb: MMIO BAR at {:#x}", bar_phys);

    if !reset(&regs) {
        trace!(0, "igb: chip reset timed out");
        return;
    }

    /* Which interface the MAC is wired to, from the NVM. Reported rather than
     * forced: the copper, SGMII and SERDES variants of this family differ
     * only here, and writing "internal PHY" onto a SERDES part would break a
     * card that was working. If this ever reads as anything but internal, the
     * PHY bring-up below is talking to the wrong thing and the log will say
     * so instead of leaving a silent link-down. */
    let link_mode = regs.read32(CTRL_EXT) & CTRL_EXT_LINK_MODE_MASK;
    if link_mode != CTRL_EXT_LINK_MODE_INTERNAL {
        trace!(
            0,
            "igb: link mode {:#x} is not the internal PHY; this driver drives copper only",
            link_mode >> 22
        );
    }

    let mac = match read_mac(&regs) {
        Some(m) => m,
        None => {
            trace!(0, "igb: no valid station address in RAL0/RAH0");
            return;
        }
    };
    trace!(
        0,
        "igb: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );

    /* --- Rings: one page each, 256 descriptors --- */
    let tx_dma = match dma::DmaBuffer::new(RING_PAGES) {
        Some(d) => d,
        None => {
            trace!(0, "igb: TX ring alloc failed");
            return;
        }
    };
    let rx_dma = match dma::DmaBuffer::new(RING_PAGES) {
        Some(d) => d,
        None => {
            trace!(0, "igb: RX ring alloc failed");
            return;
        }
    };

    let tx_ring = TxRing::new(tx_dma);
    let mut rx_ring = RxRing::new(rx_dma);

    /* Fill the ring before the engine is switched on. desc_unused enforces
     * the one-descriptor gap, so this posts RING_SIZE - 1 buffers. */
    let mut posted = 0;
    while rx_ring.desc_unused() > 0 {
        match net::NetFrame::alloc_rx(RX_BUF_SIZE) {
            Some(frame) => {
                if rx_ring.post_next(frame).is_none() {
                    break;
                }
                posted += 1;
            }
            None => {
                /* A partially filled ring still works: process_rx refills
                 * what it can each pass. */
                trace!(0, "igb: RX frame alloc failed after {} slots", posted);
                break;
            }
        }
    }

    /* --- Link. Ask the PHY to bring it up and auto-negotiate.
     *
     * The master-disable bit set during reset is cleared here explicitly.
     * A device reset is supposed to clear it, but leaving that to the reset
     * means trusting it: the bit stops the chip mastering the bus at all, so
     * getting it wrong costs every descriptor fetch and every packet, with
     * nothing in the registers to say why. --- */
    let ctrl = regs.read32(CTRL);
    regs.write32(
        CTRL,
        (ctrl & !CTRL_GIO_MASTER_DISABLE) | CTRL_SLU | CTRL_ASDE,
    );

    let phy_addr = phy_address(&regs, generation);
    trace!(0, "igb: PHY at MDIO address {}", phy_addr);
    phy_start_link(&regs, generation, phy_addr);

    /* --- Multicast table: nothing accepted by hash, broadcast handled by
     * RCTL.BAM below. --- */
    for i in 0..MTA_ENTRIES {
        regs.write32(MTA + i * 4, 0);
    }

    /* --- Receive queue 0 --- */
    let rx_phys = rx_ring.dma.phys();
    regs.write32(RDBAL0, rx_phys as u32);
    regs.write32(RDBAH0, (rx_phys >> 32) as u32);
    regs.write32(RDLEN0, (RING_SIZE * desc::DESC_BYTES) as u32);
    regs.write32(RDH0, 0);
    regs.write32(RDT0, 0);

    /* 2 KiB buffers, advanced one-buffer descriptors -- the layout desc.rs
     * decodes -- and drop rather than back up when the ring runs dry. */
    regs.write32(
        SRRCTL0,
        ((RX_BUF_SIZE as u32) >> SRRCTL_BSIZEPKT_SHIFT)
            | SRRCTL_DESCTYPE_ADV_ONEBUF
            | SRRCTL_DROP_EN,
    );

    /* Descriptors must be visible before the engine that will read them. */
    kcore::barrier::dma_wmb();

    regs.write32(RXDCTL0, XDCTL_QUEUE_ENABLE);
    if !wait_for(100, 100, || regs.read32(RXDCTL0) & XDCTL_QUEUE_ENABLE != 0) {
        trace!(0, "igb: RX queue did not enable");
        stop_engines(&regs);
        return;
    }

    /* Only now hand the buffers over: the tail must not point past
     * descriptors the engine was not yet allowed to fetch. */
    regs.write32(RDT0, rx_ring.tail());
    rx_ring.mark_tail_written();

    regs.write32(
        RCTL,
        RCTL_EN | RCTL_BAM | RCTL_SZ_2048 | RCTL_SECRC,
    );

    /* --- Transmit queue 0 --- */
    let tx_phys = tx_ring.dma.phys();
    regs.write32(TDBAL0, tx_phys as u32);
    regs.write32(TDBAH0, (tx_phys >> 32) as u32);
    regs.write32(TDLEN0, (RING_SIZE * desc::DESC_BYTES) as u32);
    regs.write32(TDH0, 0);
    regs.write32(TDT0, 0);

    regs.write32(TXDCTL0, XDCTL_QUEUE_ENABLE);
    if !wait_for(100, 100, || regs.read32(TXDCTL0) & XDCTL_QUEUE_ENABLE != 0) {
        trace!(0, "igb: TX queue did not enable");
        stop_engines(&regs);
        return;
    }

    regs.write32(
        TCTL,
        TCTL_EN | TCTL_PSP | TCTL_CT_DEFAULT | TCTL_COLD_FULL_DUPLEX | TCTL_RTLC,
    );

    /* The address filter, after the receiver is configured and before it can
     * match anything. */
    write_mac(&regs, &mac);

    /* Still silent: the handler does not exist yet. */
    regs.write32(IMC, u32::MAX);
    let _ = regs.read32(ICR);

    let mut name_buf = [0u8; 16];
    write_device_name(&mut name_buf, idx);

    let mut dev_box = Box::new(IgbDevice {
        _msix_irq: msix::MsixInterrupt::empty(),
        _msix_table: None,
        _intx: interrupt::LegacyInterrupt::empty(),
        msix: false,
        generation,
        phy_addr,
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

    let ctx_ptr = dev_box.as_mut() as *mut IgbDevice as *mut u8;

    if !attach_interrupt(pci_dev, &mut dev_box, ctx_ptr) {
        trace!(0, "igb: no interrupt could be registered");
        return;
    }

    if dev_box.msix {
        arm_msix(&dev_box.regs);
    } else {
        dev_box.regs.write32(IMS, INTR_MASK_BITS);
    }

    trace_link(&dev_box.regs);

    let raw = Box::into_raw(dev_box);

    let ops = net::NetDeviceOps {
        name: unsafe { (*raw).name_buf.as_ptr() },
        mac: unsafe { (*raw).mac },
        flush_tx: igb_flush_tx,
        process_rx: igb_process_rx,
        ctx: raw as *mut u8,
    };
    let handle = match net::register(&ops) {
        Some(h) => h,
        None => {
            trace!(0, "igb: NetDevice registration failed");
            unsafe { drop(Box::from_raw(raw)) };
            return;
        }
    };
    unsafe { (*raw).net_handle = handle };

    DEVICES[idx as usize].store(raw, Ordering::Release);
    DEVICE_COUNT.store(idx + 1, Ordering::Release);

    trace!(0, "igb: device {} ready ({})", idx, generation.as_str());
}

fn attach_interrupt(
    pci_dev: &pci::PciDevice,
    dev_box: &mut Box<IgbDevice>,
    ctx_ptr: *mut u8,
) -> bool {
    /* One vector for everything. The point of this part's 25 vectors is to
     * give each queue its own, and there is one queue. */
    if let Some(table) = msix::MsixTable::new(pci_dev) {
        match msix::MsixInterrupt::register(&table, 0, igb_isr, ctx_ptr) {
            Some(irq) => {
                trace!(
                    0,
                    "igb: MSI-X vector={} ({} entries available)",
                    irq.vector(),
                    table.table_size()
                );
                dev_box._msix_irq = irq;
                dev_box._msix_table = Some(table);
                dev_box.msix = true;
                return true;
            }
            None => {
                trace!(0, "igb: MSI-X entry 0 unavailable, falling back to INTx");
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

    match interrupt::LegacyInterrupt::register_level(pci_dev, igb_isr, ctx_ptr) {
        Some(irq) => {
            trace!(0, "igb: INTx vector={} (irq {})", irq.vector(), pci_dev.irq_line);
            dev_box._intx = irq;
            true
        }
        None => false,
    }
}

/// Route every cause to MSI-X vector 0 and arm it.
///
/// IVAR maps queues to vectors a byte apiece -- receive queue 0 in the low
/// byte of IVAR0, transmit queue 0 in the next, everything that is not a
/// queue in the second byte of IVAR_MISC -- and the top bit of each byte is
/// what makes the entry mean anything.
fn arm_msix(regs: &io::MmioRegion) {
    regs.write32(
        GPIE,
        GPIE_MSIX_MODE | GPIE_PBA | GPIE_EIAME | GPIE_NSICR,
    );

    regs.write32(IVAR0, IVAR_VALID | (IVAR_VALID << 8));
    regs.write32(IVAR_MISC, IVAR_VALID << 8);

    /* No auto-clear: the handler reads EICR, and having the hardware clear
     * causes behind its back is how a driver loses an event it never saw. */
    regs.write32(EIAC, 0);
    regs.write32(EIAM, 0);

    /* Link changes still arrive through the legacy mask even in this mode. */
    regs.write32(IMS, ICR_LSC);
    regs.write32(EIMS, EICR_VECTOR0);
}

/* ================================================================== */
/* Interrupt */

extern "C" fn igb_isr(ctx: *mut u8) {
    let dev = ctx as *mut IgbDevice;
    if dev.is_null() {
        return;
    }

    unsafe {
        let regs = &(*dev).regs;

        /* In MSI-X mode the vector's own cause register says whether this
         * interrupt is ours; the per-event detail still arrives in ICR. */
        if (*dev).msix {
            let eicr = regs.read32(EICR);
            if eicr & EICR_VECTOR0 == 0 {
                return;
            }

            /* Clear it by writing the bits back. EICR is documented as
             * cleared on read only when GPIE.Multiple_MSIX is zero, and this
             * driver sets that bit -- so a read alone leaves the cause
             * standing and the interrupt re-asserts the moment it is armed
             * again. Measured on an I210 before this line existed: 434
             * million interrupts and 144 million poll rounds, with not one
             * frame received. (333016 rev 3.7, section 8.8.3.) */
            regs.write32(EICR, eicr);
        }

        /* Reading the cause register clears it, so this is the only chance to
         * see these bits: everything they ask for has to be started here.
         * A zero read means the line belongs to somebody else -- INTx is
         * shared. */
        let icr = regs.read32(ICR);
        if icr == 0 && !(*dev).msix {
            return;
        }

        if icr & ICR_LSC != 0 {
            trace_link(regs);
        }

        if icr & ICR_RXO != 0 {
            let n = RX_ERR_EVENTS.fetch_add(1, Ordering::Relaxed);
            if n < 10 {
                trace!(0, "igb: receiver overrun, icr {:#x} (event {})", icr, n + 1);
            }
        }

        /* TX reaping stays in flush_tx -- doing it here would race a
         * flush_tx on another CPU. The softirq drains frames that piled up
         * in the C++ TxQueue while the ring was full. */
        if icr & ICR_TXDW != 0 {
            softirq::raise(softirq::TYPE_NET_TX);
        }

        /* Go quiet on receive and hand the ring to the poll. Unlike the
         * Realtek parts there is no edge to lose here: the causes are cleared
         * by the reads above, so re-arming later cannot land on a stale one.
         *
         * On the single MSI-X vector every cause shares the interrupt, so the
         * poll is entered whenever it fires; a harvest that finds nothing is
         * cheap, and it is one fewer thing this register can lose. */
        if (*dev).msix || icr & RX_INTR_BITS != 0 {
            (*dev).mask_rx();
            softirq::raise(softirq::TYPE_NET_RX);
        }
    }
}

/* ================================================================== */
/* Receive */

/// Put buffers back into every slot the chip has given up, and publish them.
/// Returns whether the tail moved.
unsafe fn refill_rx(dev: *mut IgbDevice) {
    while (*dev).rx_ring.desc_unused() > 0 {
        match net::NetFrame::alloc_rx(RX_BUF_SIZE) {
            Some(frame) => {
                if (*dev).rx_ring.post_next(frame).is_none() {
                    break;
                }
            }
            None => {
                /* Out of frames: leave the slots empty and try again next
                 * pass. The ring keeps running on what it still holds. */
                (*dev).rx_dropped.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }

    /* Publish on the tail having moved, not on this function having posted
     * something: an error frame reposted in the harvest loop moves it too,
     * and if it took the last free slot the loop above adds nothing. Keying
     * off `posted` there would leave that descriptor sitting in the ring
     * with the chip never told it was available. */
    if (*dev).rx_ring.needs_tail_write() {
        /* Descriptors visible before the tail that points past them. */
        kcore::barrier::dma_wmb();
        (*dev).regs.write32(RDT0, (*dev).rx_ring.tail());
        (*dev).rx_ring.mark_tail_written();
    }
}

extern "C" fn igb_process_rx(ctx: *mut u8) {
    let dev = ctx as *mut IgbDevice;
    if dev.is_null() {
        return;
    }

    let mut polls: u32 = 0;

    unsafe {
        /* Harvested frames wait here until the batch is complete, so the
         * receive queue's lock is taken once rather than once per frame. */
        let mut batch: [usize; RX_BUDGET as usize] = [0; RX_BUDGET as usize];

        loop {
            RX_POLLS.fetch_add(1, Ordering::Relaxed);

            let mut taken = 0u32;
            let mut budget_hit = false;
            let mut batched = 0usize;

            loop {
                if taken >= RX_BUDGET {
                    budget_hit = true;
                    break;
                }

                let (mut frame, status, len) = match (*dev).rx_ring.harvest() {
                    None => break,
                    Some(triple) => triple,
                };

                taken += 1;

                /* A frame that is not end-of-packet would be one piece of a
                 * multi-descriptor packet. RCTL.LPE is off and the buffer is
                 * 2 KiB, so the chip has no way to make one -- but a frame
                 * that claims to be a fragment is not one to pass up. */
                if status & RXD_ERR_MASK != 0 || status & RXD_STAT_EOP == 0 || len == 0 {
                    (*dev).rx_dropped.fetch_add(1, Ordering::Relaxed);
                    frame.set_len(0);

                    /* Straight back into the ring rather than out through the
                     * pool and in again: the buffer is still perfectly good,
                     * and the receive path has no business allocating. The
                     * slot is there because the harvest above just freed one,
                     * so this cannot fail. */
                    let _ = (*dev).rx_ring.post_next(frame);
                    continue;
                }

                frame.set_len(len);
                (*dev).rx_packets.fetch_add(1, Ordering::Relaxed);

                batch[batched] = frame.into_raw();
                batched += 1;
            }

            /* Hand the harvest over in one piece, then give the chip its
             * buffers back. Refilling after the batch keeps the ring supplied
             * from the pool the dispatch below will replenish. */
            (*dev).net_handle.enqueue_rx_batch(&batch[..batched]);
            refill_rx(dev);

            if budget_hit {
                RX_BUDGET_HITS.fetch_add(1, Ordering::Relaxed);

                /* Still ours to finish. Stay silent and come back through the
                 * softirq, letting the dispatch that follows -- and everything
                 * else on this CPU -- have its turn. */
                softirq::raise(softirq::TYPE_NET_RX);
                return;
            }

            /* Round again while the ring still has frames, receive sources
             * left masked: the repeating path touches no device register at
             * all, since has_work reads the descriptor out of DMA memory. */
            if !(*dev).rx_ring.has_work() {
                /* Empty: hand the ring back to the interrupt. */
                (*dev).arm_rx();

                /* Re-checked after arming, for a frame that landed between
                 * the last harvest and the write above. */
                if !(*dev).rx_ring.has_work() {
                    return;
                }

                (*dev).mask_rx();
            }

            polls += 1;
            if polls >= MAX_POLLS {
                /* Work left and out of rounds -- which is also how a run of
                 * failed frame allocations looks, since an empty slot reads
                 * as work to do. Back through the softirq, silent. */
                softirq::raise(softirq::TYPE_NET_RX);
                return;
            }
        }
    }
}

/* ================================================================== */
/* Transmit: called by the C++ net stack under TxQueueLock */

extern "C" fn igb_flush_tx(ctx: *mut u8) {
    let dev = ctx as *mut IgbDevice;
    if dev.is_null() {
        return;
    }

    unsafe {
        let net = (*dev).net_handle;

        /* Give back what the chip has finished before asking for room. */
        (*dev).tx_ring.reap_completed(net);

        let mut submitted: u32 = 0;
        while (*dev).tx_ring.can_submit() {
            let frame = match net.tx_dequeue() {
                None => break,
                Some(f) => f,
            };

            if !(*dev).tx_ring.submit(frame) {
                break;
            }
            submitted += 1;
        }

        if submitted != 0 {
            (*dev).tx_packets.fetch_add(submitted as u64, Ordering::Relaxed);

            /* Descriptors visible before the doorbell that points past them. */
            kcore::barrier::dma_wmb();
            (*dev).regs.write32(TDT0, (*dev).tx_ring.tail());
        }
    }
}

/* ================================================================== */
/* State dump, for the `igbdump` shell command.
 *
 * Reading the chip rather than the driver's idea of it: the receive stall on
 * the other card in this tree was found that way and not by any amount of
 * reading driver code. ICR is deliberately absent -- it is read-to-clear, so
 * a dump that showed it would also consume it. */

#[repr(C)]
pub struct IgbState {
    pub present: u32,
    /// 0 = 82576, 1 = I210.
    pub generation: u32,
    pub phy_bmcr: u32,
    pub phy_bmsr: u32,
    pub ctrl: u32,
    pub status: u32,
    pub rctl: u32,
    pub tctl: u32,
    pub ims: u32,
    pub rxdctl: u32,
    pub srrctl: u32,
    pub rdh: u32,
    pub rdt: u32,
    pub tdh: u32,
    pub tdt: u32,
    pub next_to_clean: u32,
    pub next_to_use: u32,
    pub head_status: u32,
    pub head_posted: u32,
    pub rx_polls: u64,
    pub rx_budget_hits: u64,
    pub rx_err_events: u64,
    pub rx_packets: u64,
    pub rx_dropped: u64,
    pub tx_packets: u64,
}

#[no_mangle]
pub extern "C" fn igb_get_state(out: *mut IgbState) -> i32 {
    if out.is_null() {
        return -1;
    }

    let raw = DEVICES[0].load(Ordering::Acquire);
    if raw.is_null() {
        unsafe { (*out).present = 0 };
        return 0;
    }

    unsafe {
        let regs = &(*raw).regs;
        (*out).present = 1;
        (*out).generation = if (*raw).generation == Generation::I210 { 1 } else { 0 };

        /* What the PHY itself says, which on a machine with no console but
         * this NIC is the difference between "the cable is out" and "the
         * driver never brought the link up". Read under the same claim the
         * bring-up takes; if firmware will not give it up, report zeroes
         * rather than whatever a contended MDIO bus hands back. */
        let phy_locked = (*raw).generation != Generation::I210
            || swfw_acquire(regs, SWFW_PHY0_SM);
        if phy_locked {
            let addr = (*raw).phy_addr;
            (*out).phy_bmcr = phy_read(regs, addr, PHY_BMCR).unwrap_or(0) as u32;
            (*out).phy_bmsr = phy_read(regs, addr, PHY_BMSR).unwrap_or(0) as u32;
            if (*raw).generation == Generation::I210 {
                swfw_release(regs, SWFW_PHY0_SM);
            }
        }

        (*out).ctrl = regs.read32(CTRL);
        (*out).status = regs.read32(STATUS);
        (*out).rctl = regs.read32(RCTL);
        (*out).tctl = regs.read32(TCTL);
        (*out).ims = regs.read32(IMS);
        (*out).rxdctl = regs.read32(RXDCTL0);
        (*out).srrctl = regs.read32(SRRCTL0);
        (*out).rdh = regs.read32(RDH0);
        (*out).rdt = regs.read32(RDT0);
        (*out).tdh = regs.read32(TDH0);
        (*out).tdt = regs.read32(TDT0);

        let (ntc, ntu, status, posted) = (*raw).rx_ring.debug_state();
        (*out).next_to_clean = ntc;
        (*out).next_to_use = ntu;
        (*out).head_status = status;
        (*out).head_posted = posted;

        (*out).rx_polls = RX_POLLS.load(Ordering::Relaxed);
        (*out).rx_budget_hits = RX_BUDGET_HITS.load(Ordering::Relaxed);
        (*out).rx_err_events = RX_ERR_EVENTS.load(Ordering::Relaxed);
        (*out).rx_packets = (*raw).rx_packets.load(Ordering::Relaxed);
        (*out).rx_dropped = (*raw).rx_dropped.load(Ordering::Relaxed);
        (*out).tx_packets = (*raw).tx_packets.load(Ordering::Relaxed);
    }

    0
}
