/* Intel 82574L gigabit Ethernet driver for NOS: the e1000e family's 82574.
 *
 * Claims 8086:10D3. The part is on server boards of its age -- two of them
 * on the ASUS Z8NR-D12 this was written for -- and it is what QEMU's
 * `-device e1000e` models, which is where it was developed and is meant to
 * be debugged. The register map and every value written are the 82574
 * datasheet's, as Linux's e1000e programs them (regs.rs says where each
 * comes from).
 *
 * Architecture, matching igb next door:
 *  - PCI probe walks the ID table; each match calls init_device().
 *  - BAR 0 is the register window, mapped for MMIO.
 *  - TX: the net stack calls flush_tx() under the device's transmit lock. It
 *    reaps finished descriptors, drains the software queue into the ring and
 *    writes the tail once. Reaping never happens in an interrupt.
 *  - RX: the interrupt masks the receive side and raises softirq
 *    TYPE_NET_RX; process_rx() polls to a budget, hands frames up in one
 *    batch, refills, and re-arms only on its way out.
 *  - One receive and one transmit queue, legacy descriptors (desc.rs).
 *
 * Interrupts are MSI-X where the part offers it, INTx otherwise. The 82574's
 * MSI-X is its own: causes, not queues, are routed to vectors by IVAR -- the
 * receive and transmit queue causes here to vector 0, everything else (a
 * link change, an overrun) through the Other cause to vector 1 -- and two
 * registers do on delivery what a handler would otherwise read ICR for:
 * EIAC clears the queue causes and IAM, with CTRL_EXT.EIAME, masks them. So
 * the queue vector's handler reads no register at all, as igb's does, and
 * only the rare Other vector reads ICR. See arm_msix.
 *
 * Locking, which is to say who is handed what (net::NetDriver):
 *  - the transmit ring is flush_tx's, under the device's transmit lock;
 *  - the receive ring is process_rx's, one CPU at a time;
 *  - everything else -- E1000eDevice -- is shared by those two, the
 *    interrupt handlers and the state dump: registers and atomics, and the
 *    PHY, which only task context reaches and under a mutex.
 */

#![no_std]
extern crate alloc;

use alloc::boxed::Box;

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use kcore::cmd::{Command, Output};
use kcore::once::Once;
use kcore::sync::{IrqSpinLock, Mutex};
use kcore::time::{boot_time_ns, delay_us};
use kcore::{dma, interrupt, io, msix, pci, softirq, klog};
use net::device::ETH_NAME_MAX;
use net::{Frame, FrameQueue, NetDriver, RxQueue, TxQueue};

mod desc;
mod phy;
mod regs;

use desc::{RxRing, RxView, TxRing, RING_PAGES, RING_SIZE, RXD_ERR_FRAME_MASK, RXD_STAT_DD, RXD_STAT_EOP, RX_BUF_SIZE};
use regs::*;

/* ================================================================== */
/* Module-level device registry: the driver's own list, for shutdown and
 * the dump. The names come from the net layer (`net::free_eth_name`). */

const MAX_DEVICES: usize = 4;
static DEVICES: [Once<&'static E1000eDevice>; MAX_DEVICES] = [const { Once::new() }; MAX_DEVICES];
static DEVICE_COUNT: AtomicU32 = AtomicU32::new(0);

const PAGE_SIZE: usize = 4096;
/* Every register the driver reaches, in whole pages. BAR 0 is 128 KiB. */
const BAR_MAP_PAGES: usize = REG_SPACE_USED.div_ceil(PAGE_SIZE);

/* Descriptors taken in one pass before the poll yields, and passes before it
 * gives the CPU back through a softirq: their product bounds one entry into
 * process_rx. */
const RX_BUDGET: u32 = 64;
const MAX_POLLS: u32 = 8;

/* Under load, an empty ring is looked at again on the next softirq pass
 * rather than handed back to the interrupt, until it has sat empty this
 * long -- as on igb, whose lib.rs says why at length. */
const REPOLL_MIN_PPS: u32 = 200_000;
const REPOLL_NS: u64 = 20_000;
const RATE_SAMPLE_NS: u64 = 1_000_000;

/* A write-back asked for every this many descriptors of a run, besides its
 * last: often enough that a long run gives frames back while it goes out. */
const TX_RS_EVERY: u32 = 32;

/* Bring-up waits, in microseconds and tries. */
const MASTER_DISABLE_WAIT_US: u64 = 100;
const MASTER_DISABLE_TRIES: u32 = 800; /* Linux's MASTER_DISABLE_TIMEOUT */
const QUIET_BEFORE_RESET_US: u64 = 10_000;
const AFTER_RESET_US: u64 = 1000;
const AUTO_READ_WAIT_US: u64 = 1000;
const AUTO_READ_TRIES: u32 = 10; /* Linux's AUTO_READ_DONE_TIMEOUT */
/* After the NVM's auto-read the PHY loads its own configuration from it:
 * Linux waits 25 ms on the 82574 before touching either. */
const PHY_CONFIG_LOAD_US: u64 = 25_000;
/* How long boot waits for the link before going on: long enough to say in
 * the log that it came up; a link slower than that announces itself. */
const LINK_WAIT_US: u64 = 10_000;
const LINK_WAIT_TRIES: u32 = 100;

/// Spin until `cond` holds, polling every `us` microseconds, up to `tries`
/// times; whether it came to hold.
pub(crate) fn wait_for<F: FnMut() -> bool>(us: u64, tries: u32, mut cond: F) -> bool {
    for _ in 0..tries {
        if cond() {
            return true;
        }
        delay_us(us);
    }
    cond()
}

/// ITR and EITR count in 256 ns units; never 0, which would turn the
/// throttle off altogether.
fn itr_units(us: u32) -> u32 {
    (us.saturating_mul(1000) / ITR_UNIT_NS).max(1)
}

/// The interrupts a device has, in the order they are to go: the handlers
/// before the table their vectors are entries of.
struct Irqs {
    _msix_queue: Option<msix::MsixInterrupt>,
    _msix_other: Option<msix::MsixInterrupt>,
    _msix_table: Option<msix::MsixTable>,
    _intx: Option<interrupt::LegacyInterrupt>,
}

struct E1000eDevice {
    /// Kept: dropped, they would take the handlers away -- which is what
    /// shutdown does with them.
    irqs: IrqSpinLock<Option<Irqs>>,
    /// MSI-X rather than INTx: which causes mask the receive side. Settled
    /// before anything is armed.
    msix: AtomicBool,
    /// The queue causes and the Other cause have a vector each; otherwise
    /// one handler, reading ICR, has them all.
    two_vector: AtomicBool,
    /// Set by an interrupt when the link changed, reported by the poll: the
    /// poll runs in task context, and the report is a log line.
    link_event: AtomicBool,
    link_up: AtomicBool,
    name: [u8; ETH_NAME_MAX],
    name_len: usize,
    /// For the bus master bit, which is what stops the device's DMA.
    pci: pci::PciDevice,
    /// The PHY, task context only: two dumps at once would interleave on
    /// MDIC.
    phy: Mutex<()>,
    phy_id: u32,
    /// The firmware has the PHY (MANC.BLK_PHY_RST_ON_IDE) and the link was
    /// left as it found it.
    phy_blocked: bool,
    manc: u32,
    rx_view: RxView,
    rx_next_to_clean: AtomicU32,
    rx_next_to_use: AtomicU32,
    rx_head_posted: AtomicBool,
    tx_packets: AtomicU64,
    rx_packets: AtomicU64,
    rx_dropped: AtomicU64,
    rx_polls: AtomicU64,
    rx_budget_hits: AtomicU64,
    rx_overruns: AtomicU64,
    /// Receive rate, packets a second, resampled by the poll: it sets the
    /// throttle (apply_itr) and whether to repoll an empty ring.
    rx_rate_pps: AtomicU32,
    rate_last_ns: AtomicU64,
    rate_last_pkts: AtomicU64,
    /// The throttle programmed, microseconds.
    itr_us: AtomicU32,
    isr_queue: AtomicU64,
    isr_other: AtomicU64,
    empty_since: AtomicU64,
    rx_repolls: AtomicU64,
    rx_repoll_hits: AtomicU64,
    rx_repoll_ns: AtomicU64,
    /* The chip's statistics, accumulated: the registers are read-clear. */
    stat_tpr: AtomicU64,
    stat_gprc: AtomicU64,
    stat_mpc: AtomicU64,
    stat_rnbc: AtomicU64,
    stat_rxerrc: AtomicU64,
    stat_crcerrs: AtomicU64,
    stat_tpt: AtomicU64,
    stat_gptc: AtomicU64,
    _bar_mapping: dma::PhysMapping,
    regs: io::MmioRegion,
}

impl E1000eDevice {
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("eth?")
    }

    fn is_msix(&self) -> bool {
        self.msix.load(Ordering::Relaxed)
    }

    /// What masks the receive side: the queue causes under MSI-X -- both,
    /// as the queue vector's delivery masks both -- the receive causes on
    /// INTx.
    fn rx_mask_bits(&self) -> u32 {
        if self.is_msix() {
            QUEUE_CAUSES
        } else {
            RX_INTR_BITS
        }
    }

    /// Go quiet on receive for the duration of a poll.
    fn mask_rx(&self) {
        self.regs.write32(IMC, self.rx_mask_bits());
    }

    /// Hand the ring back to the interrupt.
    fn arm_rx(&self) {
        self.regs.write32(IMS, self.rx_mask_bits());
    }

    /// Every interrupt masked, transmit stopped, the device handed back to
    /// the firmware and its DMA stopped: what a shutdown leaves, and a
    /// registration that failed. The rings stay where they are -- the
    /// device is for good -- with nothing reading them.
    ///
    /// Receive is not switched off by RCTL: clearing RCTL.EN with frames
    /// arriving can hang the 82574's receive unit, which is why Linux never
    /// does on this part (FLAG2_NO_DISABLE_RX). The bus master bit is what
    /// stops its DMA, receive included -- as Linux's shutdown stops it.
    fn quiesce(&self) {
        let regs = &self.regs;
        regs.write32(IMC, u32::MAX);
        regs.write32(EIAC, 0);
        regs.write32(TCTL, regs.read32(TCTL) & !TCTL_EN);
        regs.write32(CTRL_EXT, regs.read32(CTRL_EXT) & !CTRL_EXT_DRV_LOAD);
        let _ = regs.read32(STATUS); /* flush the posted writes */

        /* The handlers: taken out under the lock, dropped after it -- the
         * drop waits for one that may be running. */
        let irqs = self.irqs.lock().take();
        drop(irqs);

        let cmd = self.pci.read_config16(PCI_COMMAND);
        self.pci.write_config16(PCI_COMMAND, cmd & !PCI_COMMAND_MASTER);
    }
}

/* ================================================================== */
/* Public entry points called from kernel/src/lib.rs */

pub fn init() {
    match Command::register("e1000edump", "e1000edump [n] - 82574 chip and ring state, of the nth e1000e (0)", dump) {
        /* The command is the kernel's own and stays for good. */
        Ok(cmd) => core::mem::forget(cmd),
        Err(_) => klog!(0, "e1000e: cannot register the e1000edump command"),
    }

    let mut start: usize = 0;
    while let Some((idx, dev)) = pci::find_device_from(PCI_VENDOR_INTEL, PCI_DEVICE_82574L, start) {
        klog!(0, "e1000e: found 82574L {:04x}:{:04x} at {:02x}:{:02x}.{} rev {:02x} irq={}",
            PCI_VENDOR_INTEL, PCI_DEVICE_82574L, dev.bus, dev.slot, dev.func, dev.revision, dev.irq_line);
        init_device(&dev);
        start = idx + 1;
    }
}

pub fn shutdown() {
    let count = (DEVICE_COUNT.load(Ordering::Acquire) as usize).min(MAX_DEVICES);
    for slot in DEVICES[..count].iter() {
        if let Some(dev) = slot.get() {
            dev.quiesce();
        }
    }
    klog!(0, "e1000e: shutdown complete, count={}", count);
}

/* ================================================================== */
/* Hardware bring-up */

/// The bridge whose secondary bus the device is on: what PCI Express calls
/// the other end of its link. None for a device on the root bus.
fn parent_bridge(dev: &pci::PciDevice) -> Option<pci::PciDevice> {
    if dev.bus == 0 {
        return None;
    }
    (0..pci::device_count()).filter_map(pci::get_device).find(|b| {
        b.class == PCI_CLASS_BRIDGE && b.subclass == PCI_SUBCLASS_PCI_BRIDGE
            && u16::from(b.read_config8(PCI_SECONDARY_BUS)) == dev.bus
    })
}

/// Where a function's PCI Express Link Control register is.
fn link_control(dev: &pci::PciDevice) -> Option<u16> {
    dev.find_capability(PCI_CAP_ID_EXP).map(|cap| u16::from(cap) + PCI_EXP_LNKCTL)
}

/// ASPM L0s and L1 off on the device's link, both ends: the 82574's errata
/// have its transmit hang when the link goes into a power state, and Linux
/// turns both off for the part (FLAG2_DISABLE_ASPM_L0S and _L1) the same
/// way, downstream end first (__e1000e_disable_aspm).
fn disable_aspm(dev: &pci::PciDevice) {
    let at = match link_control(dev) {
        Some(at) => at,
        None => return,
    };
    let parent = parent_bridge(dev).and_then(|p| link_control(&p).map(|pat| (p, pat)));

    let ours = dev.read_config16(at);
    let theirs = parent.as_ref().map_or(0, |(p, pat)| p.read_config16(*pat));
    if (ours | theirs) & PCI_EXP_LNKCTL_ASPMC == 0 {
        return;
    }

    dev.write_config16(at, ours & !PCI_EXP_LNKCTL_ASPMC);
    if let Some((p, pat)) = &parent {
        p.write_config16(*pat, p.read_config16(*pat) & !PCI_EXP_LNKCTL_ASPMC);
    }
    klog!(0, "e1000e: ASPM off on {:02x}:{:02x}.{} (was {:#x}) and its bridge (was {:#x}): the 82574's errata",
        dev.bus, dev.slot, dev.func, ours & PCI_EXP_LNKCTL_ASPMC, theirs & PCI_EXP_LNKCTL_ASPMC);
}

/// Take the device back from whatever ran it last, and reset it: Linux's
/// e1000_reset_hw_82571 for the 82574. False when it does not come back.
fn reset(regs: &io::MmioRegion) -> bool {
    /* Nothing may reach a handler that does not exist yet. */
    regs.write32(IMC, u32::MAX);
    regs.write32(RCTL, 0);
    regs.write32(TCTL, regs.read32(TCTL) & !TCTL_EN);
    let _ = regs.read32(STATUS);
    delay_us(QUIET_BEFORE_RESET_US);

    /* Outstanding DMA finished before the reset, or the bus can be left
     * with a transaction the device no longer remembers. Not fatal if it
     * does not go idle -- the reset comes either way -- but worth a line. */
    regs.write32(CTRL, regs.read32(CTRL) | CTRL_GIO_MASTER_DISABLE);
    if !wait_for(MASTER_DISABLE_WAIT_US, MASTER_DISABLE_TRIES, || {
        regs.read32(STATUS) & STATUS_GIO_MASTER_ENABLE == 0
    }) {
        klog!(0, "e1000e: bus master did not go idle before the reset");
    }

    /* The split of the packet buffer takes effect with the reset. */
    regs.write32(PBA, PBA_RX_KB);

    /* The MDIO bus held across the reset, as Linux holds it: ownership goes
     * back to the firmware with every reset, and a reset with the firmware
     * mid-access is one the PHY sees half of. */
    let mdio = phy::Mdio::take(regs);
    if mdio.is_none() {
        klog!(0, "e1000e: the firmware did not give up the MDIO bus before the reset");
    }
    regs.write32(CTRL, regs.read32(CTRL) | CTRL_RST);
    drop(mdio);
    delay_us(AFTER_RESET_US);

    /* The station address and the rest come back from the NVM after the
     * reset; until this bit is set, RAL0 holds whatever it held. Linux
     * stops here when it never comes: nothing after it can be trusted. */
    if !wait_for(AUTO_READ_WAIT_US, AUTO_READ_TRIES, || regs.read32(EECD) & EECD_AUTO_RD != 0) {
        klog!(0, "e1000e: the NVM auto-read did not complete after the reset");
        return false;
    }
    delay_us(PHY_CONFIG_LOAD_US);

    /* The reset re-enables some sources; silence them again. */
    regs.write32(IMC, u32::MAX);
    let _ = regs.read32(ICR);
    true
}

/// The bits the 82574 needs set or cleared before anything else, which the
/// datasheet calls hardware-dependent and Linux sets in
/// e1000_initialize_hw_bits_82571 -- and the two its init_hw adds.
fn init_hw_bits(regs: &io::MmioRegion) {
    regs.write32(TXDCTL0, regs.read32(TXDCTL0) | TXDCTL_COUNT_DESC);
    regs.write32(TXDCTL1, regs.read32(TXDCTL1) | TXDCTL_COUNT_DESC);
    regs.write32(TARC0, (regs.read32(TARC0) & !TARC0_CLEAR) | TARC0_BIT_26);
    regs.write32(CTRL, regs.read32(CTRL) & !CTRL_RESERVED_29);
    regs.write32(CTRL_EXT, (regs.read32(CTRL_EXT) & !CTRL_EXT_BIT_23) | CTRL_EXT_BIT_22);
    regs.write32(GCR, regs.read32(GCR) | GCR_BIT_22 | GCR_L1_ACT_WITHOUT_L0S_RX);
    regs.write32(GCR2, regs.read32(GCR2) | GCR2_BIT_0);
}

/// The station address, which the NVM auto-read put in RAR 0.
fn read_mac(regs: &io::MmioRegion) -> Option<[u8; 6]> {
    let ral = regs.read32(RAL0);
    let rah = regs.read32(RAH0);
    let mac = [ral as u8, (ral >> 8) as u8, (ral >> 16) as u8, (ral >> 24) as u8, rah as u8, (rah >> 8) as u8];

    /* All zeros and all ones are how "nothing here" reads back; a
     * multicast bit in the first octet is no station address. */
    if mac.iter().all(|b| *b == 0) || mac.iter().all(|b| *b == 0xFF) || mac[0] & 1 != 0 {
        return None;
    }
    Some(mac)
}

fn write_mac(regs: &io::MmioRegion, mac: &[u8; 6]) {
    let ral = u32::from(mac[0]) | (u32::from(mac[1]) << 8) | (u32::from(mac[2]) << 16) | (u32::from(mac[3]) << 24);
    let rah = u32::from(mac[4]) | (u32::from(mac[5]) << 8) | RAH_AV;
    /* Low half first: the address becomes valid on the write that sets AV. */
    regs.write32(RAL0, ral);
    regs.write32(RAH0, rah);
}

/// The receive filters as nothing but the station address and broadcast:
/// the other receive addresses, the multicast hash and the VLAN table
/// cleared (e1000e_init_rx_addrs, the MTA and VFTA loops of init_hw).
fn clear_filters(regs: &io::MmioRegion) {
    for i in 1..RAR_ENTRIES {
        regs.write32(RAL0 + i * RAR_STRIDE, 0);
        regs.write32(RAH0 + i * RAR_STRIDE, 0);
    }
    for i in 0..MTA_ENTRIES {
        regs.write32(MTA + i * 4, 0);
    }
    for i in 0..VFTA_ENTRIES {
        regs.write32(VFTA + i * 4, 0);
    }
}

/// The MAC's side of the link, then the PHY's: link up with speed and
/// duplex as the PHY resolves them, no flow control, low-power link-up off
/// (it would settle at 10 Mbit/s), and auto-negotiation started. Returns
/// the PHY's identity, 0 where it was not reached.
fn setup_link(regs: &io::MmioRegion) -> u32 {
    let ctrl = regs.read32(CTRL);
    regs.write32(CTRL, (ctrl | CTRL_SLU)
        & !(CTRL_FRCSPD | CTRL_FRCDPLX | CTRL_RFCE | CTRL_TFCE | CTRL_GIO_MASTER_DISABLE));
    regs.write32(POEMB, regs.read32(POEMB) & !POEMB_D0A_LPLU);

    let mdio = match phy::Mdio::take(regs) {
        Some(mdio) => mdio,
        None => {
            klog!(0, "e1000e: the firmware did not give up the MDIO bus: the link is as it found it");
            return 0;
        }
    };
    let (id, ok) = phy::setup_link(&mdio);
    drop(mdio);
    if !ok {
        klog!(0, "e1000e: PHY {:#010x} set up only in part; the link may not come up", id);
    }
    id
}

/// Read every statistics register this driver accumulates, which zeroes it:
/// what was counted before the reset -- symbol errors pile up wildly with no
/// link -- is not this boot's.
fn clear_stats(regs: &io::MmioRegion) {
    for reg in [TPR, GPRC, MPC, RNBC, RXERRC, CRCERRS, TPT, GPTC] {
        let _ = regs.read32(reg);
    }
}

/// Stop both DMA engines for good: what a bring-up that fails after the
/// rings were handed to the chip does before they go. The frames posted
/// there go back to the pool when the ring is dropped, and a chip still
/// holding their addresses would write the next frames it receives into
/// memory that is someone else's.
fn stop_engines(pci_dev: &pci::PciDevice, regs: &io::MmioRegion) {
    regs.write32(IMC, u32::MAX);
    regs.write32(TCTL, regs.read32(TCTL) & !TCTL_EN);
    regs.write32(CTRL_EXT, regs.read32(CTRL_EXT) & !CTRL_EXT_DRV_LOAD);
    let cmd = pci_dev.read_config16(PCI_COMMAND);
    pci_dev.write_config16(PCI_COMMAND, cmd & !PCI_COMMAND_MASTER);
    /* And a reset, which forgets the rings' addresses. */
    regs.write32(CTRL, regs.read32(CTRL) | CTRL_RST);
    delay_us(AFTER_RESET_US);
}

/* ================================================================== */
/* Device initialisation */

fn init_device(pci_dev: &pci::PciDevice) {
    let idx = DEVICE_COUNT.load(Ordering::Relaxed) as usize;
    if idx >= MAX_DEVICES {
        klog!(0, "e1000e: too many devices (max {})", MAX_DEVICES);
        return;
    }

    /* Before the chip is touched, as Linux does at probe. */
    disable_aspm(pci_dev);

    pci_dev.enable_bus_mastering();

    let bar_phys = pci_dev.bar_address(0);
    if bar_phys == 0 {
        klog!(0, "e1000e: BAR 0 is not a memory BAR, skipping the device");
        return;
    }
    let bar_mapping = match dma::PhysMapping::map(bar_phys, BAR_MAP_PAGES) {
        Some(m) => m,
        None => {
            klog!(0, "e1000e: failed to map BAR 0 at {:#x}", bar_phys);
            return;
        }
    };
    let regs = io::MmioRegion::new(bar_mapping.as_mut_ptr(), BAR_MAP_PAGES * PAGE_SIZE);

    if !reset(&regs) {
        klog!(0, "e1000e: the chip did not come back from its reset");
        let cmd = pci_dev.read_config16(PCI_COMMAND);
        pci_dev.write_config16(PCI_COMMAND, cmd & !PCI_COMMAND_MASTER);
        return;
    }

    /* What the manageability firmware is doing on this port: read, never
     * written. A BMC sharing it keeps working through all of this. */
    let manc = regs.read32(MANC);
    let phy_blocked = manc & MANC_BLK_PHY_RST_ON_IDE != 0;
    klog!(0, "e1000e: manageability {:#x}{}{}{}{}{}", manc,
        if manc & MANC_ASF_EN != 0 { ", ASF" } else { "" },
        if manc & MANC_RCV_TCO_EN != 0 { ", TCO receive" } else { "" },
        if manc & MANC_ARP_EN != 0 { ", ARP to the firmware" } else { "" },
        if manc & MANC_EN_MNG2HOST != 0 { ", to-host filters" } else { "" },
        if phy_blocked { ", the firmware holds the PHY" } else { "" });

    let mac = match read_mac(&regs) {
        Some(m) => m,
        None => {
            klog!(0, "e1000e: no valid station address in RAL0/RAH0");
            return;
        }
    };
    klog!(0, "e1000e: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

    /* The driver has the device from here (e1000e_get_hw_control). */
    regs.write32(CTRL_EXT, regs.read32(CTRL_EXT) | CTRL_EXT_DRV_LOAD);

    init_hw_bits(&regs);
    clear_filters(&regs);

    let phy_id = if phy_blocked {
        klog!(0, "e1000e: a firmware session holds the PHY: its link is left as it is");
        0
    } else {
        setup_link(&regs)
    };

    /* Transmit write-back: one for every descriptor that asks, on both
     * queues alike -- the errata want the unused second queue's TXDCTL the
     * same as the first's (e1000_configure_tx). */
    let txdctl = (regs.read32(TXDCTL0) & !TXDCTL_WTHRESH_MASK) | TXDCTL_FULL_TX_DESC_WB | TXDCTL_COUNT_DESC;
    regs.write32(TXDCTL0, txdctl);
    regs.write32(TXDCTL1, txdctl);

    clear_stats(&regs);

    /* --- Rings --- */
    let (tx_dma, rx_dma) = match (dma::DmaBuffer::new(RING_PAGES), dma::DmaBuffer::new(RING_PAGES)) {
        (Some(tx), Some(rx)) => (tx, rx),
        _ => {
            klog!(0, "e1000e: no memory for the rings");
            regs.write32(CTRL_EXT, regs.read32(CTRL_EXT) & !CTRL_EXT_DRV_LOAD);
            return;
        }
    };
    let (tx_ring, mut rx_ring) = match (TxRing::new(tx_dma), RxRing::new(rx_dma)) {
        (Some(tx), Some(rx)) => (tx, rx),
        _ => {
            klog!(0, "e1000e: no memory for the rings' bookkeeping");
            regs.write32(CTRL_EXT, regs.read32(CTRL_EXT) & !CTRL_EXT_DRV_LOAD);
            return;
        }
    };

    /* Every buffer posted before the receiver is switched on; desc_unused
     * keeps the one-descriptor gap, so this is RING_SIZE - 1 of them. */
    let mut posted = 0;
    while rx_ring.desc_unused() > 0 {
        match Frame::alloc_rx(RX_BUF_SIZE) {
            Some(frame) => {
                if rx_ring.post_next(frame).is_err() {
                    break;
                }
                posted += 1;
            }
            None => {
                /* A ring partly filled still works: process_rx refills. */
                klog!(0, "e1000e: receive frames ran out after {} slots", posted);
                break;
            }
        }
    }

    /* --- Transmit queue 0 --- */
    let tx_phys = tx_ring.phys;
    regs.write32(TDBAL, tx_phys as u32);
    regs.write32(TDBAH, (tx_phys >> 32) as u32);
    regs.write32(TDLEN, (RING_SIZE * desc::DESC_BYTES) as u32);
    regs.write32(TDH, 0);
    regs.write32(TDT, 0);
    regs.write32(TIDV, TIDV_DEFAULT);
    regs.write32(TADV, TADV_DEFAULT);
    let tctl = regs.read32(TCTL) & !(TCTL_CT_MASK | TCTL_COLD_MASK);
    regs.write32(TCTL, tctl | TCTL_EN | TCTL_PSP | TCTL_RTLC | TCTL_CT_DEFAULT | TCTL_COLD_DEFAULT);

    /* --- Receive queue 0 --- */
    let rx_phys = rx_ring.phys;
    regs.write32(RDBAL, rx_phys as u32);
    regs.write32(RDBAH, (rx_phys >> 32) as u32);
    regs.write32(RDLEN, (RING_SIZE * desc::DESC_BYTES) as u32);
    regs.write32(RDH, 0);
    regs.write32(RDT, 0);
    /* A receive interrupt as soon as a frame is in: the throttle, not the
     * delay timers, is what holds interrupts apart here. */
    regs.write32(RDTR, 0);
    regs.write32(RADV, 0);
    /* Legacy descriptors (EXTEN off), and no interrupt per TCP ACK. */
    regs.write32(RFCTL, (regs.read32(RFCTL) | RFCTL_ACK_DIS) & !RFCTL_EXTEN);
    /* No checksum offload: the stack checks its own. */
    regs.write32(RXCSUM, 0);

    /* Descriptors visible before the receiver that will read them. */
    kcore::barrier::dma_wmb();
    regs.write32(RCTL, RCTL_EN | RCTL_BAM | RCTL_SZ_2048 | RCTL_SECRC);
    regs.write32(RDT, rx_ring.tail());
    rx_ring.mark_tail_written();

    /* The address filter, once the receiver is set up and before anything
     * could match it. */
    write_mac(&regs, &mac);

    /* The throttle at its floor until the receive rate says otherwise. */
    regs.write32(ITR, itr_units(ITR_MIN_US));

    /* Still silent: no handler exists yet. */
    regs.write32(IMC, u32::MAX);
    let _ = regs.read32(ICR);

    /* Up to a second for the link, to be able to say in the log what it
     * came up as; one that takes longer says so through its interrupt. With
     * every cause masked: a link change in the meantime waits in ICR for
     * the arming below. */
    if !phy_blocked
        && !wait_for(LINK_WAIT_US, LINK_WAIT_TRIES, || regs.read32(STATUS) & STATUS_LU != 0)
    {
        klog!(0, "e1000e: no link yet on {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.slot, pci_dev.func);
    }

    let mut name_buf = [0u8; ETH_NAME_MAX];
    let name_len = match net::free_eth_name(&mut name_buf) {
        Some(name) => name.len(),
        None => {
            klog!(0, "e1000e: no ethN name left in the device table");
            stop_engines(pci_dev, &regs);
            return;
        }
    };

    let phy = match Mutex::new(()) {
        Some(m) => m,
        None => {
            klog!(0, "e1000e: no memory for the PHY's mutex");
            stop_engines(pci_dev, &regs);
            return;
        }
    };

    /* For good from here: the interrupt handlers are pointed at the device,
     * and then the net stack is, with each ring handed over to it. */
    let (next_to_clean, next_to_use, head_posted) = rx_ring.pointers();
    let dev: &'static E1000eDevice = Box::leak(Box::new(E1000eDevice {
        irqs: IrqSpinLock::new(None),
        msix: AtomicBool::new(false),
        two_vector: AtomicBool::new(false),
        link_event: AtomicBool::new(false),
        link_up: AtomicBool::new(false),
        name: name_buf,
        name_len,
        pci: *pci_dev,
        phy,
        phy_id,
        phy_blocked,
        manc,
        rx_view: rx_ring.view(),
        rx_next_to_clean: AtomicU32::new(next_to_clean),
        rx_next_to_use: AtomicU32::new(next_to_use),
        rx_head_posted: AtomicBool::new(head_posted),
        tx_packets: AtomicU64::new(0),
        rx_packets: AtomicU64::new(0),
        rx_dropped: AtomicU64::new(0),
        rx_polls: AtomicU64::new(0),
        rx_budget_hits: AtomicU64::new(0),
        rx_overruns: AtomicU64::new(0),
        rx_rate_pps: AtomicU32::new(0),
        rate_last_ns: AtomicU64::new(0),
        rate_last_pkts: AtomicU64::new(0),
        itr_us: AtomicU32::new(ITR_MIN_US),
        isr_queue: AtomicU64::new(0),
        isr_other: AtomicU64::new(0),
        empty_since: AtomicU64::new(0),
        rx_repolls: AtomicU64::new(0),
        rx_repoll_hits: AtomicU64::new(0),
        rx_repoll_ns: AtomicU64::new(0),
        stat_tpr: AtomicU64::new(0),
        stat_gprc: AtomicU64::new(0),
        stat_mpc: AtomicU64::new(0),
        stat_rnbc: AtomicU64::new(0),
        stat_rxerrc: AtomicU64::new(0),
        stat_crcerrs: AtomicU64::new(0),
        stat_tpt: AtomicU64::new(0),
        stat_gptc: AtomicU64::new(0),
        _bar_mapping: bar_mapping,
        regs,
    }));

    dev.report_link();

    /* The handlers in place, nothing armed yet: every cause is masked. */
    if !attach_interrupt(pci_dev, dev) {
        klog!(0, "e1000e: {}: no interrupt could be registered", dev.name());
        dev.quiesce();
        return;
    }

    /* Registered before anything is armed. The other way round, the first
     * frame or link change would mask the queue causes as its message went
     * (IAM) and raise a receive softirq that, for a device the net layer
     * does not know yet, runs no poll -- and the poll is the only thing
     * that arms them again: a NIC that never receives. */
    if net::register(dev.name(), mac, dev, tx_ring, rx_ring).is_none() {
        klog!(0, "e1000e: {}: registration with the net layer failed", dev.name());
        dev.quiesce();
        return;
    }

    let receive_cause = if dev.is_msix() {
        arm_msix(&dev.regs, dev.two_vector.load(Ordering::Relaxed));
        ICR_RXQ0
    } else {
        arm_intx(&dev.regs);
        ICR_RXT0
    };

    /* Frames that came in while every cause was masked -- the wait for the
     * link above, the receiver on throughout -- are in the ring with
     * nothing to say so: their cause was RXT0, raised before MSI-X was on,
     * which no vector carries. Left there, a busy broadcast domain fills
     * the ring in that second, and a full ring takes no frame that would
     * raise a receive cause again. One poll now takes them. */
    dev.regs.write32(ICS, receive_cause);

    let _ = DEVICES[idx].set(dev);
    DEVICE_COUNT.store(idx as u32 + 1, Ordering::Release);

    klog!(0, "e1000e: {} ready, PHY {:#010x}, {}", dev.name(), phy_id,
        if !dev.is_msix() { "INTx" } else if dev.two_vector.load(Ordering::Relaxed) { "MSI-X queue + other" }
        else { "MSI-X one vector" });
}

fn attach_interrupt(pci_dev: &pci::PciDevice, dev: &'static E1000eDevice) -> bool {
    /* Two vectors where the table has room: entry 0 for the queue causes,
     * entry 1 for Other. The queue handler then reads no register -- the
     * hardware clears and masks its causes -- where a single vector must
     * read ICR on every interrupt to tell a frame from a link change. */
    if let Some(table) = msix::MsixTable::new(pci_dev) {
        match msix::MsixInterrupt::register_for(&table, 0, dev, isr_queue) {
            Some(irq) => {
                dev.msix.store(true, Ordering::Release);
                let other = if table.table_size() >= 2 {
                    msix::MsixInterrupt::register_for(&table, 1, dev, isr_other)
                } else {
                    None
                };
                dev.two_vector.store(other.is_some(), Ordering::Release);
                klog!(0, "e1000e: {}: MSI-X vector {} ({} entries), {}", dev.name(), irq.vector(),
                    table.table_size(), if other.is_some() { "queue + other" } else { "one vector" });
                *dev.irqs.lock() = Some(Irqs {
                    _msix_queue: Some(irq),
                    _msix_other: other,
                    _msix_table: Some(table),
                    _intx: None,
                });
                return true;
            }
            None => klog!(0, "e1000e: {}: MSI-X entry 0 unavailable, falling back to INTx", dev.name()),
        }
    }

    /* Firmware can leave INTx masked in the command register; the MSI-X
     * path clears that, this one must not assume it. */
    let cmd = pci_dev.read_config16(PCI_COMMAND);
    if cmd & PCI_COMMAND_INTX_DISABLE != 0 {
        pci_dev.write_config16(PCI_COMMAND, cmd & !PCI_COMMAND_INTX_DISABLE);
    }

    match interrupt::LegacyInterrupt::register_level_for(pci_dev, dev, isr) {
        Some(irq) => {
            klog!(0, "e1000e: {}: INTx vector {} (irq {})", dev.name(), irq.vector(), pci_dev.irq_line);
            *dev.irqs.lock() = Some(Irqs { _msix_queue: None, _msix_other: None, _msix_table: None, _intx: Some(irq) });
            true
        }
        None => false,
    }
}

/// Route the causes to their vectors and arm them, as Linux's
/// e1000_configure_msix and e1000_irq_enable do for the 82574 -- with the
/// receive and transmit causes on one vector rather than two.
///
/// IVAR names a vector a cause, four bits each, the top one saying the
/// entry means anything. With `two_vector` the queue causes go to vector 0
/// and Other to vector 1; with one, all three to vector 0.
fn arm_msix(regs: &io::MmioRegion, two_vector: bool) {
    let other_vec = if two_vector { 1 } else { 0 };
    regs.write32(IVAR, (IVAR_VALID << IVAR_RXQ0_SHIFT)
        | (IVAR_VALID << IVAR_TXQ0_SHIFT)
        | ((IVAR_VALID | other_vec) << IVAR_OTHER_SHIFT)
        | IVAR_TX_EVERY_WB);

    /* The throttle is a vector's own in MSI-X mode. */
    regs.write32(eitr(0), itr_units(ITR_MIN_US));
    if two_vector {
        regs.write32(eitr(1), itr_units(ITR_MIN_US));
    }

    /* Masking on delivery (EIAME, by IAM), never on a read of ICR (IAME);
     * and the pending bits clear with their causes (PBA_CLR). */
    let ctrl_ext = regs.read32(CTRL_EXT) & !CTRL_EXT_IAME;
    regs.write32(CTRL_EXT, ctrl_ext | CTRL_EXT_PBA_CLR | CTRL_EXT_EIAME);

    if two_vector {
        /* The queue causes cleared (EIAC) and masked (IAM) as vector 0's
         * message goes: its handler reads no register, and the receive poll
         * re-arms them. Other is masked as its message goes but not cleared
         * -- its handler reads ICR for what it was, and a cause cleared
         * behind that read is an event the handler never sees (Linux took
         * Other out of EIAC for that). */
        regs.write32(EIAC, QUEUE_CAUSES);
        regs.write32(IAM, QUEUE_CAUSES | ICR_OTHER);
    } else {
        /* One vector: its handler reads ICR and masks the receive side
         * itself; nothing is cleared or masked behind it. */
        regs.write32(EIAC, 0);
        regs.write32(IAM, 0);
    }

    regs.write32(IMS, QUEUE_CAUSES | ICR_OTHER | OTHER_CAUSES);
}

/// INTx: no masking behind the handler's back -- a read of ICR clears the
/// causes, and the handler masks the receive side itself.
fn arm_intx(regs: &io::MmioRegion) {
    regs.write32(CTRL_EXT, regs.read32(CTRL_EXT) & !(CTRL_EXT_IAME | CTRL_EXT_EIAME));
    regs.write32(IAM, 0);
    regs.write32(IMS, INTX_MASK_BITS);
}

/* ================================================================== */
/* Interrupts */

/* MSI-X entry 0, the queue vector. With two vectors it reads no register:
 * the hardware cleared its causes (EIAC) and masked them (IAM) before this
 * ran, and the receive poll re-arms them. Transmit shares it, so both
 * softirqs are raised -- one wake for the two. With one vector it is the
 * shared handler's job, which reads ICR. */
fn isr_queue(dev: &'static E1000eDevice) {
    if !dev.two_vector.load(Ordering::Relaxed) {
        isr(dev);
        return;
    }

    dev.isr_queue.fetch_add(1, Ordering::Relaxed);
    softirq::raise_mask((1 << softirq::TYPE_NET_TX) | (1 << softirq::TYPE_NET_RX));
}

/* MSI-X entry 1: Other -- a link change, an overrun -- seldom. */
fn isr_other(dev: &'static E1000eDevice) {
    let regs = &dev.regs;
    dev.isr_other.fetch_add(1, Ordering::Relaxed);

    /* A read of ICR clears every cause in it, the queue causes included:
     * one whose message had not gone yet would be lost with it, and the
     * frame behind it would wait for the next. Raised again, as Linux's
     * e1000_msix_other does. */
    let icr = regs.read32(ICR);
    if icr & QUEUE_CAUSES != 0 {
        regs.write32(ICS, icr & QUEUE_CAUSES);
    }

    if icr & ICR_LSC != 0 {
        dev.link_event.store(true, Ordering::Release);
        /* The poll reports it, and on the queue vector's CPU: a receive
         * cause set has the chip interrupt there -- at once if the vector is
         * armed, as the poll re-arms it if not. The flag is out before the
         * write that sends a CPU to read it. */
        kcore::barrier::dma_wmb();
        regs.write32(ICS, ICR_RXQ0);
    }
    if icr & ICR_RXO != 0 {
        dev.note_overrun(icr);
    }

    /* Masked as its message went (IAM): armed again. */
    regs.write32(IMS, ICR_OTHER | OTHER_CAUSES);
}

/* The shared handler: MSI-X with one vector, or INTx. */
fn isr(dev: &'static E1000eDevice) {
    let regs = &dev.regs;
    let msix = dev.is_msix();

    /* Reading the cause register clears it: everything it asks for starts
     * here. On INTx the line is shared, and a cause register that does not
     * say this device asserted it means a neighbour's interrupt. */
    let icr = regs.read32(ICR);
    if !msix && icr & ICR_INT_ASSERTED == 0 {
        return;
    }
    dev.isr_queue.fetch_add(1, Ordering::Relaxed);

    if icr & ICR_LSC != 0 {
        dev.link_event.store(true, Ordering::Release);
    }
    if icr & ICR_RXO != 0 {
        dev.note_overrun(icr);
    }

    /* Transmit reaping stays in flush_tx -- here it would race a flush on
     * another CPU; the softirq drains what queued while the ring was full. */
    let tx = if msix { icr & ICR_TXQ0 != 0 } else { icr & ICR_TXDW != 0 };
    let mut raise = if tx { 1 << softirq::TYPE_NET_TX } else { 0 };

    /* Go quiet on receive and hand the ring to the poll -- which also
     * reports a link change. With one MSI-X vector every cause shares the
     * interrupt, so the poll is entered whenever it fires: a harvest that
     * finds nothing is cheap. */
    if msix || icr & (RX_INTR_BITS | ICR_LSC) != 0 {
        dev.mask_rx();
        raise |= 1 << softirq::TYPE_NET_RX;
    }
    if raise != 0 {
        softirq::raise_mask(raise);
    }
}

/* ================================================================== */
/* Receive */

impl E1000eDevice {
    fn note_overrun(&self, icr: u32) {
        let n = self.rx_overruns.fetch_add(1, Ordering::Relaxed);
        if n < 10 {
            klog!(0, "e1000e: {}: receiver overrun, icr {:#x} (event {})", self.name(), icr, n + 1);
        }
    }

    /// Say what the link is now, if it changed since it was last said.
    /// STATUS alone, no PHY: the MAC's speed is the PHY's on this part.
    fn report_link(&self) {
        let status = self.regs.read32(STATUS);
        let up = status & STATUS_LU != 0;
        let was = self.link_up.swap(up, Ordering::AcqRel);
        if up == was {
            return;
        }
        if up {
            let speed = match status & STATUS_SPEED_MASK {
                STATUS_SPEED_10 => 10,
                STATUS_SPEED_100 => 100,
                STATUS_SPEED_1000 => 1000,
                _ => 0,
            };
            let duplex = if status & STATUS_FD != 0 { "full" } else { "half" };
            klog!(0, "e1000e: {}: link up, {} Mbit/s {} duplex", self.name(), speed, duplex);
        } else {
            klog!(0, "e1000e: {}: link down", self.name());
        }
    }

    fn handle_link_event(&self) {
        if self.link_event.swap(false, Ordering::AcqRel) {
            self.report_link();
        }
    }

    /// Put buffers back into every slot the chip gave up, and publish them.
    fn refill_rx(&self, ring: &mut RxRing) {
        while ring.desc_unused() > 0 {
            match Frame::alloc_rx(RX_BUF_SIZE) {
                Some(frame) => {
                    if ring.post_next(frame).is_err() {
                        break;
                    }
                }
                None => {
                    /* Out of frames: the slots stay empty until a later pass,
                     * the ring running on what it still holds. */
                    self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }

        /* On the tail having moved, not on something posted here: an errored
         * frame put straight back moves it too. */
        if ring.needs_tail_write() {
            /* Descriptors visible before the tail that points past them. */
            kcore::barrier::dma_wmb();
            self.regs.write32(RDT, ring.tail());
            ring.mark_tail_written();
        }
    }

    /// Resample the receive rate if a window has passed, and set the
    /// throttle by it; the rate as it stands.
    fn sample_rate(&self, now: u64) -> u32 {
        let last = self.rate_last_ns.load(Ordering::Relaxed);
        let dt = now.wrapping_sub(last);
        if last != 0 && dt >= RATE_SAMPLE_NS {
            let pkts = self.rx_packets.load(Ordering::Relaxed);
            let dpkts = pkts.wrapping_sub(self.rate_last_pkts.load(Ordering::Relaxed));
            let pps = (dpkts.saturating_mul(1_000_000_000) / dt) as u32;
            self.rx_rate_pps.store(pps, Ordering::Relaxed);
            self.rate_last_ns.store(now, Ordering::Relaxed);
            self.rate_last_pkts.store(pkts, Ordering::Relaxed);
            self.apply_itr(pps);
        } else if last == 0 {
            self.rate_last_ns.store(now, Ordering::Relaxed);
            self.rate_last_pkts.store(self.rx_packets.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        self.rx_rate_pps.load(Ordering::Relaxed)
    }

    /// Widen the throttle with the rate, 2 us idle to 20 at 400k a second:
    /// the queue vector's EITR under MSI-X, ITR on INTx. Written only when
    /// it changes.
    fn apply_itr(&self, pps: u32) {
        let want = (pps / ITR_PPS_PER_US).clamp(ITR_MIN_US, ITR_MAX_US);
        if want != self.itr_us.load(Ordering::Relaxed) {
            let reg = if self.is_msix() { eitr(0) } else { ITR };
            self.regs.write32(reg, itr_units(want));
            self.itr_us.store(want, Ordering::Relaxed);
        }
    }

    fn end_repolls(&self, now: u64, found: bool) {
        let since = self.empty_since.swap(0, Ordering::Relaxed);
        if since == 0 {
            return;
        }
        self.rx_repoll_ns.fetch_add(now.saturating_sub(since), Ordering::Relaxed);
        if found {
            self.rx_repoll_hits.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn repoll(&self, now: u64) -> bool {
        let since = self.empty_since.load(Ordering::Relaxed);
        if since == 0 {
            /* Never 0 itself, which means "not in a run". */
            self.empty_since.store(now.max(1), Ordering::Relaxed);
        } else if now.saturating_sub(since) >= REPOLL_NS {
            self.end_repolls(now, false);
            return false;
        }
        self.rx_repolls.fetch_add(1, Ordering::Relaxed);
        true
    }

    fn publish_rx(&self, ring: &RxRing) {
        let (next_to_clean, next_to_use, posted) = ring.pointers();
        self.rx_next_to_clean.store(next_to_clean, Ordering::Relaxed);
        self.rx_next_to_use.store(next_to_use, Ordering::Relaxed);
        self.rx_head_posted.store(posted, Ordering::Relaxed);
    }

    fn poll_rx(&self, ring: &mut RxRing, up: &mut RxQueue<'_>) {
        let mut polls: u32 = 0;

        self.handle_link_event();

        let now = boot_time_ns();
        let hot = self.sample_rate(now) >= REPOLL_MIN_PPS;

        /* The harvest waits here until the batch is whole, so the receive
         * queue's lock is taken once rather than a frame at a time. */
        let mut batch = FrameQueue::new();
        let mut took = false;

        loop {
            self.rx_polls.fetch_add(1, Ordering::Relaxed);

            let mut taken = 0u32;
            let mut budget_hit = false;

            loop {
                if taken >= RX_BUDGET {
                    budget_hit = true;
                    break;
                }

                let (mut frame, status, len) = match ring.harvest() {
                    None => break,
                    Some(triple) => triple,
                };
                taken += 1;

                /* With RCTL.LPE off and 2 KiB buffers no frame spans two
                 * descriptors, so one that is not end-of-packet is not one
                 * to pass up; nor one the MAC flagged, nor an empty one. */
                if status & RXD_ERR_FRAME_MASK != 0 || status & RXD_STAT_EOP == 0 || len == 0 || len > RX_BUF_SIZE {
                    self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                    frame.set_len(0);
                    /* Straight back into the slot the harvest just freed:
                     * the buffer is good, and this path allocates nothing. */
                    let _ = ring.post_next(frame);
                    continue;
                }

                frame.set_len(len);
                self.rx_packets.fetch_add(1, Ordering::Relaxed);
                batch.push(frame);
            }

            if taken != 0 {
                took = true;
                self.end_repolls(now, true);
            }

            /* The harvest handed over in one piece, then the chip's buffers
             * given back from the pool the dispatch replenishes. */
            up.deliver(&mut batch);
            self.refill_rx(ring);

            if budget_hit {
                self.rx_budget_hits.fetch_add(1, Ordering::Relaxed);
                /* Still ours: silent, back through the softirq, letting the
                 * dispatch and the rest of this CPU have their turn. */
                softirq::raise(softirq::TYPE_NET_RX);
                return;
            }

            if !ring.has_work() {
                /* Running hot: look again on the softirq's next pass rather
                 * than arm an interrupt for a frame microseconds away -- for
                 * up to REPOLL_NS of passes that find nothing. */
                if hot && (took || self.repoll(now)) {
                    softirq::raise(softirq::TYPE_NET_RX);
                    return;
                }

                /* Empty: the ring back to the interrupt. */
                self.end_repolls(now, false);
                self.arm_rx();

                /* Again after arming, for a frame that landed between the
                 * last harvest and the write. */
                if !ring.has_work() {
                    return;
                }
                self.mask_rx();
            }

            polls += 1;
            if polls >= MAX_POLLS {
                /* Work left and out of rounds -- also what a run of failed
                 * refills looks like. Back through the softirq, silent. */
                softirq::raise(softirq::TYPE_NET_RX);
                return;
            }
        }
    }
}

/* ================================================================== */
/* The net stack's two calls */

impl NetDriver for E1000eDevice {
    type Tx = TxRing;
    type Rx = RxRing;

    fn flush_tx(&'static self, ring: &mut TxRing, stack: &mut TxQueue<'_>) {
        /* What the chip finished, given back before room is asked for. */
        ring.reap_completed(stack);

        let mut submitted: u32 = 0;
        while ring.can_submit() {
            let frame = match stack.dequeue() {
                None => break,
                Some(f) => f,
            };

            let rs = (submitted + 1) % TX_RS_EVERY == 0;
            if let Err(frame) = ring.submit(frame, rs) {
                /* Not one the ring takes -- empty, or longer than a
                 * descriptor says: handed back, never dropped here. */
                stack.done(frame);
                continue;
            }
            submitted += 1;
        }

        if submitted != 0 {
            /* Only a descriptor that reports lets those before it be
             * reaped, so every run ends with one. */
            ring.report_last();
            self.tx_packets.fetch_add(u64::from(submitted), Ordering::Relaxed);

            /* Descriptors visible before the doorbell that points past them. */
            kcore::barrier::dma_wmb();
            self.regs.write32(TDT, ring.tail());
        }
    }

    fn process_rx(&'static self, ring: &mut RxRing, up: &mut RxQueue<'_>) {
        self.poll_rx(ring, up);
        self.publish_rx(ring);
    }
}

/* ================================================================== */
/* State dump, for `e1000edump`: the chip's own registers, not the driver's
 * idea of them. ICR is left out -- it is read-clear, and a dump that showed
 * it would also consume it. */

/// Add a read-clear register's delta to its running total, and return it.
fn accumulate(total: &AtomicU64, delta: u32) -> u64 {
    total.fetch_add(u64::from(delta), Ordering::Relaxed) + u64::from(delta)
}

fn dump(args: &str, out: &mut Output) {
    let index = match args.split_whitespace().next() {
        None => 0,
        Some(word) => match word.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                let _ = writeln!(out, "e1000edump: '{}' is no device number", word);
                return;
            }
        },
    };
    let dev = match DEVICES.get(index).and_then(|slot| slot.get()) {
        Some(dev) => *dev,
        None => {
            let _ = writeln!(out, "e1000edump: no e1000e {}", index);
            return;
        }
    };
    let regs = &dev.regs;
    let bit = |word: u32, mask: u32| u32::from(word & mask != 0);

    let status = regs.read32(STATUS);
    let _ = writeln!(out, "{}: 82574L at {:02x}:{:02x}.{}, PHY {:#010x}{}", dev.name(), dev.pci.bus, dev.pci.slot,
        dev.pci.func, dev.phy_id, if dev.phy_blocked { " (held by the firmware)" } else { "" });
    let _ = writeln!(out, "ctrl {:#x} ctrl_ext {:#x} status {:#x} link {} manc {:#x} (at bring-up {:#x})",
        regs.read32(CTRL), regs.read32(CTRL_EXT), status, bit(status, STATUS_LU), regs.read32(MANC), dev.manc);

    /* What the PHY says, which on a machine whose only console is this NIC
     * is the difference between a cable that is out and a link the driver
     * never brought up. Under the device's mutex and the firmware's claim;
     * zeroes where the claim is refused. */
    {
        let _guard = dev.phy.lock();
        match phy::Mdio::take(regs) {
            Some(mdio) => {
                let p = phy::snapshot(&mdio);
                drop(mdio);
                let _ = writeln!(out, "phy bmcr {:#x} bmsr {:#x} link {} autoneg-done {}", p[0], p[1],
                    bit(u32::from(p[1]), u32::from(BMSR_LSTATUS)), bit(u32::from(p[1]), u32::from(BMSR_ANEGCOMPLETE)));
                let _ = writeln!(out, "phy adv {:#x} partner {:#x}  1000: ctrl {:#x} status {:#x}", p[2], p[3], p[4], p[5]);
            }
            None => {
                let _ = writeln!(out, "phy: the firmware would not give up the MDIO bus");
            }
        }
    }

    let rctl = regs.read32(RCTL);
    let tctl = regs.read32(TCTL);
    let _ = writeln!(out, "rctl {:#x} rx-en {}  tctl {:#x} tx-en {}  ims {:#x}", rctl, bit(rctl, RCTL_EN), tctl,
        bit(tctl, TCTL_EN), regs.read32(IMS));
    let msix = dev.is_msix();
    let two = dev.two_vector.load(Ordering::Relaxed);
    let _ = writeln!(out, "interrupts: {}, queue {}, other {}; ivar {:#x} iam {:#x} eiac {:#x}",
        if !msix { "INTx" } else if two { "MSI-X queue + other" } else { "MSI-X one vector" },
        dev.isr_queue.load(Ordering::Relaxed), dev.isr_other.load(Ordering::Relaxed), regs.read32(IVAR),
        regs.read32(IAM), regs.read32(EIAC));
    let _ = writeln!(out, "throttle {} us, rx rate {} pps; repolls {}, {} ended by a frame, {} us on an empty ring",
        dev.itr_us.load(Ordering::Relaxed), dev.rx_rate_pps.load(Ordering::Relaxed),
        dev.rx_repolls.load(Ordering::Relaxed), dev.rx_repoll_hits.load(Ordering::Relaxed),
        dev.rx_repoll_ns.load(Ordering::Relaxed) / 1000);

    let next_to_clean = dev.rx_next_to_clean.load(Ordering::Relaxed);
    let head = dev.rx_view.status(next_to_clean as usize);
    let _ = writeln!(out, "rx ring: rdh {} rdt {}  clean {} use {}; head status {:#x} dd {} posted {}",
        regs.read32(RDH), regs.read32(RDT), next_to_clean, dev.rx_next_to_use.load(Ordering::Relaxed), head,
        bit(head, RXD_STAT_DD), u32::from(dev.rx_head_posted.load(Ordering::Relaxed)));
    let _ = writeln!(out, "tx ring: tdh {} tdt {}  packets {}; rxdctl {:#x} txdctl {:#x}", regs.read32(TDH),
        regs.read32(TDT), dev.tx_packets.load(Ordering::Relaxed), regs.read32(RXDCTL), regs.read32(TXDCTL0));
    let _ = writeln!(out, "rx packets {} dropped {} overruns {}; polls {}, budget-limited {}",
        dev.rx_packets.load(Ordering::Relaxed), dev.rx_dropped.load(Ordering::Relaxed),
        dev.rx_overruns.load(Ordering::Relaxed), dev.rx_polls.load(Ordering::Relaxed),
        dev.rx_budget_hits.load(Ordering::Relaxed));

    /* The MAC's own counts, which the rings cannot see: a frame dropped
     * before it reached for a descriptor appears nowhere above. */
    let _ = writeln!(out, "mac rx: total {} good {} missed {} no-buffer {} errors {} crc {}",
        accumulate(&dev.stat_tpr, regs.read32(TPR)), accumulate(&dev.stat_gprc, regs.read32(GPRC)),
        accumulate(&dev.stat_mpc, regs.read32(MPC)), accumulate(&dev.stat_rnbc, regs.read32(RNBC)),
        accumulate(&dev.stat_rxerrc, regs.read32(RXERRC)), accumulate(&dev.stat_crcerrs, regs.read32(CRCERRS)));
    let _ = writeln!(out, "mac tx: total {} good {}", accumulate(&dev.stat_tpt, regs.read32(TPT)),
        accumulate(&dev.stat_gptc, regs.read32(GPTC)));
}
