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
 *  - TX: the net stack calls flush_tx() under the device's transmit lock. It
 *    reaps finished descriptors, drains the software queue into the ring and
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
 * Interrupts are MSI-X where the part offers it (INTx otherwise): the one
 * receive and one transmit queue on vector 0, the rare non-queue causes on
 * vector 1 where the table has room. Splitting them lets the queue interrupt
 * read no register at all -- the hardware clears and masks its cause -- where
 * a shared vector must read ICR on every interrupt to tell a packet from a
 * link change. Under load the receive poll also goes round again through
 * the softirq when the ring runs dry, rather than arm the interrupt, and the
 * throttle widens with the rate, so a flood costs far fewer interrupts than
 * one apiece. See arm_msix and REPOLL_NS.
 *
 * Locking, which is to say who is handed what (net::NetDriver):
 *  - the transmit ring is flush_tx's, which the net stack calls under the
 *    device's transmit lock. The ISR never touches it.
 *  - the receive ring is process_rx's, which the softirq layer runs on one
 *    CPU at a time.
 *  - everything else -- IgbDevice -- is shared by those two, the interrupt
 *    handlers and the state dump: registers and atomics.
 */

#![no_std]
extern crate alloc;

use alloc::boxed::Box;

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use kcore::cmd::{Command, Output};
use kcore::once::Once;
use kcore::sync::IrqSpinLock;
use kcore::time::boot_time_ns;
use kcore::{dma, interrupt, io, msix, pci, softirq, trace};
use net::{Frame, FrameQueue, NetDriver, RxQueue, TxQueue};

mod desc;
mod regs;

use desc::{RxRing, RxView, TxRing, RING_PAGES, RING_SIZE, RX_BUF_SIZE};
use regs::*;

/* ================================================================== */
/* Module-level device registry (same pattern as the other drivers) */

const MAX_DEVICES: usize = 4;
static DEVICES: [Once<&'static IgbDevice>; MAX_DEVICES] = [const { Once::new() }; MAX_DEVICES];
static DEVICE_COUNT: AtomicU32 = AtomicU32::new(0);

/* Counters worth having when something is wrong on a machine whose only
 * console is a UDP socket. */
static RX_POLLS: AtomicU64 = AtomicU64::new(0);
static RX_BUDGET_HITS: AtomicU64 = AtomicU64::new(0);
static RX_ERR_EVENTS: AtomicU64 = AtomicU64::new(0);

/* The chip's own statistics, accumulated because the registers are
   read-clear. Sampled whenever the state is dumped, which is often enough:
   they are 32-bit and would need hours at line rate to wrap. */
static STAT_TPR: AtomicU64 = AtomicU64::new(0);
static STAT_GPRC: AtomicU64 = AtomicU64::new(0);
static STAT_MPC: AtomicU64 = AtomicU64::new(0);
static STAT_RNBC: AtomicU64 = AtomicU64::new(0);
static STAT_RXERRC: AtomicU64 = AtomicU64::new(0);

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

/* When the ring empties but the receive side is running at least this fast,
 * the poll does not hand it back to the interrupt: it returns with the
 * receive sources still masked and asks the softirq for another pass, and
 * goes on doing so until the ring has sat empty this long. At a high rate the
 * next frame is a couple of microseconds away, and an arm-interrupt-mask
 * round trip costs more than looking again; below the rate it is not worth a
 * microsecond of a CPU, and the ring is handed straight back.
 *
 * Another pass, not a spin in place -- which is what this first was. The
 * frames already taken are dispatched only once this function returns, so a
 * spin here held them back for as long as it lasted; and the receive softirq
 * runs on one CPU at a time, so nothing else could be delivered meanwhile,
 * from any device, on any CPU. Between passes both happen. */
const REPOLL_MIN_PPS: u32 = 200_000;
const REPOLL_NS: u64 = 20_000;

/* The receive rate is resampled no more often than this. */
const RATE_SAMPLE_NS: u64 = 1_000_000;

/// The interrupts a device has, in the order they are to go: the handlers
/// before the table their vectors are entries of.
struct Irqs {
    _msix_irq: Option<msix::MsixInterrupt>,
    /// The second MSI-X vector, for the non-queue causes, when there is one.
    _msix_irq_other: Option<msix::MsixInterrupt>,
    _msix_table: Option<msix::MsixTable>,
    _intx: Option<interrupt::LegacyInterrupt>,
}

struct IgbDevice {
    /// Kept: dropped, they would take the handlers away -- which is what
    /// shutdown does with them.
    irqs: IrqSpinLock<Option<Irqs>>,
    /// Whether causes arrive through the extended block rather than ICR/IMS.
    /// It decides which pair of registers masks and arms the receive side.
    /// Settled when the interrupt is attached, before anything is armed.
    msix: AtomicBool,
    /// Whether the queues and the non-queue causes have a vector each. When
    /// they do, the queue interrupt reads no register at all: the hardware
    /// auto-clears its cause (EIAC) and auto-masks it (EIAME, with EIAM
    /// naming it), and only the rare other vector reads ICR. When they share
    /// one vector, that one handler reads EICR and ICR as before.
    two_vector: AtomicBool,
    generation: Generation,
    phy_addr: u32,
    /* Set by the interrupt when the link changes, acted on by the poll: the
       work it asks for is a PHY read, which means polling MDIC for
       milliseconds, and that has no business in an interrupt handler. */
    link_event: AtomicU32,
    /// The receive ring's descriptors, for the state dump: the poll owns the
    /// ring, but what the chip has written into it is there for anyone to
    /// read -- and reading the chip rather than the driver's idea of it is
    /// what the dump is for.
    rx_view: RxView,
    /// Where the poll last left the ring, for the same dump.
    rx_next_to_clean: AtomicU32,
    rx_next_to_use: AtomicU32,
    rx_head_posted: AtomicBool,
    tx_packets: AtomicU64,
    rx_packets: AtomicU64,
    rx_dropped: AtomicU64,
    /// Receive rate, packets a second, resampled once a millisecond in
    /// process_rx. It drives two things: how wide to set the interrupt
    /// throttle (apply_eitr), and whether to look at the ring again when it
    /// empties rather than hand it back to the interrupt (REPOLL_NS).
    /// Written by the receive poll, one CPU at a time.
    rx_rate_pps: AtomicU32,
    rate_last_ns: AtomicU64,
    rate_last_pkts: AtomicU64,
    /// The EITR interval currently programmed, microseconds, so apply_eitr
    /// writes the register only when it changes.
    eitr_us: AtomicU32,
    /// Interrupts taken on each vector, for igbdump.
    isr_queue: AtomicU64,
    isr_other: AtomicU64,
    /// Boot time the ring was first found empty in a run of repolls (see
    /// REPOLL_NS), 0 outside one. The receive poll's, one CPU at a time.
    empty_since: AtomicU64,
    /// For igbdump: passes that found nothing and went round again rather
    /// than arm; runs of those that ended in a frame -- an interrupt
    /// spared each time; and how long the ring sat empty through them, which
    /// is the CPU repolling cost. The receive softirq's share in `top`, less
    /// that, is the work.
    rx_repolls: AtomicU64,
    rx_repoll_hits: AtomicU64,
    rx_repoll_ns: AtomicU64,
    _bar_mapping: dma::PhysMapping,
    regs: io::MmioRegion,
}

impl IgbDevice {
    fn is_msix(&self) -> bool {
        self.msix.load(Ordering::Relaxed)
    }

    /// Go quiet on receive for the duration of a poll.
    fn mask_rx(&self) {
        if self.is_msix() {
            self.regs.write32(EIMC, EICR_VECTOR0);
        } else {
            self.regs.write32(IMC, RX_INTR_BITS);
        }
    }

    /// Hand the ring back to the interrupt.
    fn arm_rx(&self) {
        if self.is_msix() {
            self.regs.write32(EIMS, EICR_VECTOR0);
        } else {
            self.regs.write32(IMS, RX_INTR_BITS);
        }
    }
}

impl IgbDevice {
    /// Every interrupt masked and both engines stopped: what a shutdown
    /// leaves, and a bring-up that could not finish. The rings stay where
    /// they are -- the device is for good -- with nothing reading them.
    fn quiesce(&self) {
        self.regs.write32(IMC, u32::MAX);
        self.regs.write32(EIMC, u32::MAX);
        let rctl = self.regs.read32(RCTL);
        self.regs.write32(RCTL, rctl & !RCTL_EN);
        let tctl = self.regs.read32(TCTL);
        self.regs.write32(TCTL, tctl & !TCTL_EN);
        self.regs.write32(RXDCTL0, 0);
        self.regs.write32(TXDCTL0, 0);
        let _ = self.regs.read32(STATUS); /* flush the posted writes */

        /* And the handlers: taken out under the lock, dropped after it --
         * the drop waits for one that may be running. */
        let irqs = self.irqs.lock().take();
        drop(irqs);
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
    match Command::register("igbdump", "igbdump - igb chip and ring state", dump) {
        /* The command is the kernel's own and stays for good. */
        Ok(cmd) => core::mem::forget(cmd),
        Err(_) => trace!(0, "igb: cannot register the igbdump command"),
    }

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
    for slot in DEVICES[..count].iter() {
        if let Some(dev) = slot.get() {
            dev.quiesce();
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

    regs.write32(RXDCTL0, regs.read32(RXDCTL0) & !XDCTL_QUEUE_ENABLE);
    regs.write32(TXDCTL0, regs.read32(TXDCTL0) & !XDCTL_QUEUE_ENABLE);

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

    /* Reset the PHY first. Firmware has been driving it, and it hands the
     * part over in whatever state it left -- possibly on a register page
     * other than zero, where the advertisement writes below would land on
     * something else entirely. A reset puts the standard MII registers back
     * where they belong. */
    if !phy_write(regs, phy_addr, PHY_BMCR, BMCR_RESET) {
        trace!(0, "igb: PHY reset write failed");
        return false;
    }

    /* The bit clears itself when the reset finishes. */
    if !wait_for(1000, 100, || {
        match phy_read(regs, phy_addr, PHY_BMCR) {
            Some(v) => v & BMCR_RESET == 0,
            None => false,
        }
    }) {
        trace!(0, "igb: PHY reset did not complete");
        return false;
    }

    let bmcr = phy_read(regs, phy_addr, PHY_BMCR).unwrap_or(0);

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

    /* What was offered and what came back. A link that resolves lower than it
     * should is either an advertisement that did not take or a partner that
     * offered nothing better, and these two registers are the difference. */
    trace!(
        0,
        "igb: PHY advertised {:#06x}, partner offered {:#06x}, 1000 ctrl {:#06x} status {:#06x}",
        phy_read(regs, phy_addr, PHY_ANAR).unwrap_or(0),
        phy_read(regs, phy_addr, PHY_ANLPAR).unwrap_or(0),
        phy_read(regs, phy_addr, PHY_GCTL).unwrap_or(0),
        phy_read(regs, phy_addr, PHY_GSTAT).unwrap_or(0)
    );

    up
}

/// What auto-negotiation resolved to, worked out from the advertisement both
/// sides made. Returns the CTRL.SPEED encoding and whether it is full duplex.
///
/// Read from the PHY rather than taken from STATUS.ASDV: that field is
/// documented as diagnostic, and on a device that does not implement it -- a
/// QEMU model, say -- it reads zero, which taken at face value forces a
/// gigabit link down to 10 Mb/s. The advertisements cannot lie in that
/// direction; the highest ability both sides claim is what the link is.
fn resolve_link(regs: &io::MmioRegion, phy_addr: u32) -> Option<(u32, bool)> {
    let bmsr = phy_read(regs, phy_addr, PHY_BMSR)?;
    if bmsr & BMSR_ANEGCOMPLETE == 0 {
        return None;
    }

    let ours = phy_read(regs, phy_addr, PHY_ANAR)?;
    let theirs = phy_read(regs, phy_addr, PHY_ANLPAR)?;
    let gctl = phy_read(regs, phy_addr, PHY_GCTL)?;
    let gstat = phy_read(regs, phy_addr, PHY_GSTAT)?;

    /* In the order 802.3 resolves them: fastest common ability wins, full
     * duplex ahead of half at the same speed. */
    if gctl & GCTL_1000_FULL != 0 && gstat & GSTAT_PARTNER_1000_FULL != 0 {
        return Some((CTRL_SPEED_1000, true));
    }
    if gctl & GCTL_1000_HALF != 0 && gstat & GSTAT_PARTNER_1000_HALF != 0 {
        return Some((CTRL_SPEED_1000, false));
    }
    if ours & theirs & ANAR_100_FULL != 0 {
        return Some((CTRL_SPEED_100, true));
    }
    if ours & theirs & ANAR_100_HALF != 0 {
        return Some((CTRL_SPEED_100, false));
    }
    if ours & theirs & ANAR_10_FULL != 0 {
        return Some((CTRL_SPEED_10, true));
    }
    if ours & theirs & ANAR_10_HALF != 0 {
        return Some((CTRL_SPEED_10, false));
    }

    None
}

/// Put the MAC on the speed the link actually settled at.
///
/// Auto-speed detection is not a standing arrangement: the datasheet says the
/// speed "is configured only once after the LINK signal is asserted by the
/// PHY". A PHY that asserts link early -- before negotiation finishes -- and
/// then resolves upward leaves the MAC latched at whatever it saw first.
/// Measured on an I210 against a gigabit switch: the MAC ran at 10 Mb/s with
/// both sides advertising 1000BASE-T full and no master/slave fault. A MAC
/// clocked for 10 against a PHY running at 1000 receives nothing, which is
/// exactly what a dead receive ring with a healthy link and a healthy filter
/// looks like from the outside.
///
/// Runs in task context, not from the interrupt: reading the PHY means
/// polling MDIC, which is milliseconds.
fn sync_mac_speed(regs: &io::MmioRegion, phy_addr: u32) {
    let status = regs.read32(STATUS);
    if status & STATUS_LU == 0 {
        return;
    }

    let (speed, full) = match resolve_link(regs, phy_addr) {
        Some(pair) => pair,
        None => return,
    };

    let latched = (status & STATUS_SPEED_MASK) >> STATUS_SPEED_SHIFT;
    let latched_full = status & STATUS_FD != 0;
    if latched == speed && latched_full == full {
        return;
    }

    let mut ctrl = regs.read32(CTRL);
    /* Forcing is only honoured with detection out of the way: CTRL.SPEED is
     * documented as ignored while ASDE is set. */
    ctrl = ctrl & !(CTRL_ASDE | CTRL_SPEED_MASK | CTRL_FD);
    ctrl = ctrl | CTRL_FRCSPD | CTRL_FRCDPLX;
    ctrl = ctrl | (speed << CTRL_SPEED_SHIFT);
    if full {
        ctrl = ctrl | CTRL_FD;
    }
    regs.write32(CTRL, ctrl);

    trace!(
        0,
        "igb: MAC was latched at speed {} duplex {}, link resolved to {} duplex {}; forced",
        latched,
        if latched_full { 1 } else { 0 },
        speed,
        if full { 1 } else { 0 }
    );
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
fn write_device_name(buf: &mut [u8; 16], idx: u32) -> &str {
    let name = b"eth";
    let mut i = 0;
    while i < name.len() {
        buf[i] = name[i];
        i += 1;
    }
    /* Single digit is enough: MAX_DEVICES is 4. */
    buf[i] = b'0' + (idx as u8 % 10);
    core::str::from_utf8(&buf[..i + 1]).unwrap_or("eth?")
}

/* ================================================================== */
/* Device initialisation */

fn init_device(pci_dev: &pci::PciDevice, generation: Generation) {
    /* Claim a slot before touching the hardware, as the r8125 driver does:
     * every failure below either happens before the DMA engines start or
     * goes through `stop_engines` or `quiesce`, which stop them first. */
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

    let (tx_ring, mut rx_ring) = match (TxRing::new(tx_dma), RxRing::new(rx_dma)) {
        (Some(tx), Some(rx)) => (tx, rx),
        _ => {
            trace!(0, "igb: no memory for the rings' bookkeeping");
            return;
        }
    };

    /* Fill the ring before the engine is switched on. desc_unused enforces
     * the one-descriptor gap, so this posts RING_SIZE - 1 buffers. */
    let mut posted = 0;
    while rx_ring.desc_unused() > 0 {
        match Frame::alloc_rx(RX_BUF_SIZE) {
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
    let rx_phys = rx_ring.phys;
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

    /* Read-modify-write, and not a bare enable bit. The other fields of this
     * register are the descriptor prefetch thresholds, and they come out of
     * reset with values the part was designed around -- PTHRESH 12, HTHRESH
     * 10, WTHRESH 1. Writing the enable bit alone zeroes them, and PTHRESH at
     * zero means the on-chip descriptor count must fall below zero before a
     * prefetch is even considered, which it never does. The ring in host
     * memory then stays full while the chip runs out of descriptors on the
     * die and drops what arrives: measured at 443,000 packets a second
     * discarded with all 255 descriptors sitting available.
     *
     * Written rather than preserved, because preserving is only as good as
     * what was there: QEMU's model resets them to zero where the silicon
     * resets them to 12/10/1, and a fix that depends on which is which is
     * not a fix. */
    let rxdctl = (regs.read32(RXDCTL0) & !XDCTL_THRESH_MASK) | XDCTL_THRESH_DEFAULT;
    regs.write32(RXDCTL0, rxdctl | XDCTL_QUEUE_ENABLE);
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
    let tx_phys = tx_ring.phys;
    regs.write32(TDBAL0, tx_phys as u32);
    regs.write32(TDBAH0, (tx_phys >> 32) as u32);
    regs.write32(TDLEN0, (RING_SIZE * desc::DESC_BYTES) as u32);
    regs.write32(TDH0, 0);
    regs.write32(TDT0, 0);

    /* Same register layout, same reason. */
    let txdctl = (regs.read32(TXDCTL0) & !XDCTL_THRESH_MASK) | XDCTL_THRESH_DEFAULT;
    regs.write32(TXDCTL0, txdctl | XDCTL_QUEUE_ENABLE);
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
    let name = write_device_name(&mut name_buf, idx);

    /* For good from here: the interrupt handlers are pointed at the device,
     * and then the net stack is -- with each ring handed over to it, for the
     * one call that touches it. */
    let (next_to_clean, next_to_use, head_posted) = rx_ring.pointers();
    let dev: &'static IgbDevice = Box::leak(Box::new(
        IgbDevice {
            irqs: IrqSpinLock::new(None),
            msix: AtomicBool::new(false),
            two_vector: AtomicBool::new(false),
            generation,
            phy_addr,
            link_event: AtomicU32::new(0),
            rx_view: rx_ring.view(),
            rx_next_to_clean: AtomicU32::new(next_to_clean),
            rx_next_to_use: AtomicU32::new(next_to_use),
            rx_head_posted: AtomicBool::new(head_posted),
            tx_packets: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
            rx_dropped: AtomicU64::new(0),
            rx_rate_pps: AtomicU32::new(0),
            rate_last_ns: AtomicU64::new(0),
            rate_last_pkts: AtomicU64::new(0),
            eitr_us: AtomicU32::new(EITR_INTERVAL_US),
            isr_queue: AtomicU64::new(0),
            isr_other: AtomicU64::new(0),
            empty_since: AtomicU64::new(0),
            rx_repolls: AtomicU64::new(0),
            rx_repoll_hits: AtomicU64::new(0),
            rx_repoll_ns: AtomicU64::new(0),
            _bar_mapping: bar_mapping,
            regs,
        },
    ));

    if !attach_interrupt(pci_dev, dev) {
        trace!(0, "igb: no interrupt could be registered");
        dev.quiesce();
        return;
    }

    if dev.is_msix() {
        arm_msix(&dev.regs, dev.two_vector.load(Ordering::Relaxed));
    } else {
        dev.regs.write32(IMS, INTR_MASK_BITS);
    }

    trace_link(&dev.regs);

    if net::register(name, mac, dev, tx_ring, rx_ring).is_none() {
        trace!(0, "igb: NetDevice registration failed");
        dev.quiesce();
        return;
    }

    let _ = DEVICES[idx as usize].set(dev);
    DEVICE_COUNT.store(idx + 1, Ordering::Release);

    trace!(0, "igb: device {} ready ({})", idx, generation.as_str());
}

fn attach_interrupt(pci_dev: &pci::PciDevice, dev: &'static IgbDevice) -> bool {
    /* Two vectors where the table has room for them: entry 0 for the one
     * receive and the one transmit queue, entry 1 for the rare non-queue
     * causes. There is one queue, so this is not about spreading queues --
     * it is that a queue interrupt separated from the others needs to read no
     * register to know what it is for, where a single shared vector must read
     * ICR on every interrupt to tell a received packet from a link change. On
     * the AX41 under a small-packet flood those reads were a tenth of the
     * receive CPU. Entry 1's handler still reads ICR; it fires seldom. */
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

                trace!(
                    0,
                    "igb: MSI-X vector={} ({} entries), {}",
                    irq.vector(),
                    table.table_size(),
                    if other.is_some() { "queue + other" } else { "one vector" }
                );
                *dev.irqs.lock() = Some(Irqs {
                    _msix_irq: Some(irq),
                    _msix_irq_other: other,
                    _msix_table: Some(table),
                    _intx: None,
                });
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

    match interrupt::LegacyInterrupt::register_level_for(pci_dev, dev, isr) {
        Some(irq) => {
            trace!(0, "igb: INTx vector={} (irq {})", irq.vector(), pci_dev.irq_line);
            *dev.irqs.lock() = Some(Irqs {
                _msix_irq: None,
                _msix_irq_other: None,
                _msix_table: None,
                _intx: Some(irq),
            });
            true
        }
        None => false,
    }
}

/// Route the causes to their vectors and arm them.
///
/// IVAR maps queues to vectors a byte apiece -- receive queue 0 in the low
/// byte of IVAR0, transmit queue 0 in the next, everything that is not a
/// queue in the second byte of IVAR_MISC -- and the top bit of each byte is
/// what makes the entry mean anything. With `two_vector`, the queues stay on
/// vector 0 and the non-queue causes move to vector 1; otherwise all of them
/// share vector 0, the way this driver used to run.
fn arm_msix(regs: &io::MmioRegion, two_vector: bool) {
    regs.write32(
        GPIE,
        GPIE_MSIX_MODE | GPIE_PBA | GPIE_EIAME | GPIE_NSICR,
    );

    /* Take the throttle off before anything is armed. Firmware leaves a value
     * here and a device reset does not clear it: measured on the I210, the
     * receive rate sat at 169k packets a second with the CPU 93% idle,
     * because the chip was holding interrupts apart and nothing else was
     * wrong. apply_eitr widens it again under load. */
    let inherited = (regs.read32(EITR0) & EITR_INTERVAL_MASK) >> EITR_INTERVAL_SHIFT;
    regs.write32(EITR0, EITR_INTERVAL_US << EITR_INTERVAL_SHIFT);
    trace!(
        0,
        "igb: interrupt throttle was {} us, set to {}",
        inherited,
        EITR_INTERVAL_US
    );

    let other_vec = if two_vector { 1 } else { 0 };
    regs.write32(IVAR0, IVAR_VALID | (IVAR_VALID << 8));
    regs.write32(IVAR_MISC, (IVAR_VALID | other_vec) << 8);

    if two_vector {
        /* The queue vector's cause is cleared by the hardware when the
         * interrupt is delivered (EIAC), so its handler reads no register --
         * matching what Linux's igb does with its ring vectors. The other
         * vector is left out of EIAC: its handler reads ICR, and having the
         * hardware clear causes behind that read is how a driver loses an
         * event it never saw.
         *
         * Both vectors are masked as their message goes out, and each handler
         * (the receive poll, for the queue vector) re-arms its own. That takes
         * EIAM as well as GPIE.EIAME: EIAME says to mask on delivery, EIAM
         * says which vectors, and at zero it names none -- the queue vector
         * then stays armed through the whole poll and fires every throttle
         * interval for frames the poll is already taking. Nothing is lost
         * that way, so nothing would ever say so. Linux's igb_irq_enable
         * writes the two together. */
        regs.write32(EIAC, EICR_VECTOR0);
        regs.write32(EIAM, EICR_VECTOR0 | EICR_VECTOR1);
    } else {
        /* One vector: the shared handler reads EICR and masks the receive
         * side itself, and nothing is cleared or masked behind it. */
        regs.write32(EIAC, 0);
        regs.write32(EIAM, 0);
    }

    /* The non-queue causes, enabled in the legacy mask so they reach ICR for
     * the other vector's handler (or the shared handler) to read. */
    regs.write32(IMS, ICR_LSC | ICR_RXO);

    let eims = if two_vector { EICR_VECTOR0 | EICR_VECTOR1 } else { EICR_VECTOR0 };
    regs.write32(EIMS, eims);
}

/* ================================================================== */
/* Interrupt */

/* MSI-X entry 0. With the causes split in two it is the queue vector, and it
 * reads no register: the hardware cleared the cause (EIAC) and masked the
 * vector (EIAME, on the vectors EIAM names) before this ran, and the receive
 * poll re-arms it. Transmit shares the vector, so the transmit softirq is
 * raised too -- cheap when nothing is queued, and the datapath reaps
 * transmits from flush_tx on the send side anyway.
 *
 * With one vector it is the shared handler's job. Entry 0 is registered
 * before it is known whether entry 1 will be, so this is the handler it gets
 * either way -- and with one vector nothing clears the cause behind it (EIAC
 * is 0 there). Returning without `isr`'s read and write-back of EICR would
 * leave the cause standing and the vector firing for good: the storm
 * `isr`'s own comment measured on an I210. */
fn isr_queue(dev: &'static IgbDevice) {
    if !dev.two_vector.load(Ordering::Relaxed) {
        isr(dev);
        return;
    }

    dev.isr_queue.fetch_add(1, Ordering::Relaxed);
    softirq::raise(softirq::TYPE_NET_TX);
    softirq::raise(softirq::TYPE_NET_RX);
}

/* The other vector: link change and receiver overrun, seldom. It reads ICR
 * for the cause, clears its own EICR bit and re-arms it (EIAME masked it). */
fn isr_other(dev: &'static IgbDevice) {
    let regs = &dev.regs;
    dev.isr_other.fetch_add(1, Ordering::Relaxed);

    let icr = regs.read32(ICR);
    if icr & ICR_LSC != 0 {
        dev.link_event.store(1, Ordering::Release);

        /* The poll acts on it, and should on the queue vector's CPU, not
         * this one: IrqBalance places each MSI-X entry on a CPU of its own,
         * and raising the receive softirq here would run the whole poll here
         * -- the PHY reads, which take milliseconds, included -- while the
         * CPU the frames land on waited for it. Setting the queue vector's
         * cause has the chip interrupt that CPU instead: now if the vector
         * is armed, the moment the poll re-arms it if not. Linux's igb
         * watchdog kicks its ring vectors the same way. The flag is out
         * before the kick that sends a CPU to read it. */
        kcore::barrier::dma_wmb();
        regs.write32(EICS, EICR_VECTOR0);
    }
    if icr & ICR_RXO != 0 {
        let n = RX_ERR_EVENTS.fetch_add(1, Ordering::Relaxed);
        if n < 10 {
            trace!(0, "igb: receiver overrun, icr {:#x} (event {})", icr, n + 1);
        }
    }

    regs.write32(EICR, EICR_VECTOR1);
    regs.write32(EIMS, EICR_VECTOR1);
}

/* The shared vector: MSI-X with one vector, or INTx. Reads EICR and ICR to
 * tell the causes apart. */
fn isr(dev: &'static IgbDevice) {
    let regs = &dev.regs;
    let msix = dev.is_msix();
    dev.isr_queue.fetch_add(1, Ordering::Relaxed);

    /* In MSI-X mode the vector's own cause register says whether this
     * interrupt is ours; the per-event detail still arrives in ICR. */
    if msix {
        let eicr = regs.read32(EICR);
        if eicr & EICR_VECTOR0 == 0 {
            return;
        }

        /* Clear it by writing the bits back. EICR is documented as cleared
         * on read only when GPIE.Multiple_MSIX is zero, and this driver sets
         * that bit -- so a read alone leaves the cause standing and the
         * interrupt re-asserts the moment it is armed again. Measured on an
         * I210 before this line existed: 434 million interrupts and 144
         * million poll rounds, with not one frame received. (333016 rev 3.7,
         * section 8.8.3.) */
        regs.write32(EICR, eicr);
    }

    /* Reading the cause register clears it, so this is the only chance to
     * see these bits: everything they ask for has to be started here. A zero
     * read means the line belongs to somebody else -- INTx is shared. */
    let icr = regs.read32(ICR);
    if icr == 0 && !msix {
        return;
    }

    if icr & ICR_LSC != 0 {
        dev.link_event.store(1, Ordering::Release);
    }

    if icr & ICR_RXO != 0 {
        let n = RX_ERR_EVENTS.fetch_add(1, Ordering::Relaxed);
        if n < 10 {
            trace!(0, "igb: receiver overrun, icr {:#x} (event {})", icr, n + 1);
        }
    }

    /* TX reaping stays in flush_tx -- doing it here would race a flush_tx on
     * another CPU. The softirq drains frames that piled up in the stack's
     * transmit queue while the ring was full. */
    if icr & ICR_TXDW != 0 {
        softirq::raise(softirq::TYPE_NET_TX);
    }

    /* Go quiet on receive and hand the ring to the poll. Unlike the Realtek
     * parts there is no edge to lose here: the causes are cleared by the
     * reads above, so re-arming later cannot land on a stale one.
     *
     * On the single MSI-X vector every cause shares the interrupt, so the
     * poll is entered whenever it fires; a harvest that finds nothing is
     * cheap, and it is one fewer thing this register can lose. */
    if msix || icr & RX_INTR_BITS != 0 {
        dev.mask_rx();
        softirq::raise(softirq::TYPE_NET_RX);
    }
}

/* ================================================================== */
/* Receive */

impl IgbDevice {
    /// Bring the MAC into step with a link that has just changed, and say so.
    ///
    /// Task context, from the poll: takes the PHY semaphore where the part
    /// shares its MDIO bus with firmware, which is not something to do from
    /// an interrupt.
    fn handle_link_event(&self) {
        if self.link_event.swap(0, Ordering::AcqRel) == 0 {
            return;
        }

        let regs = &self.regs;

        if self.generation == Generation::I210 && !swfw_acquire(regs, SWFW_PHY0_SM) {
            trace!(0, "igb: link changed, but the PHY semaphore is held elsewhere");
            return;
        }

        sync_mac_speed(regs, self.phy_addr);

        if self.generation == Generation::I210 {
            swfw_release(regs, SWFW_PHY0_SM);
        }

        trace_link(regs);
    }

    /// Put buffers back into every slot the chip has given up, and publish
    /// them.
    fn refill_rx(&self, ring: &mut RxRing) {
        while ring.desc_unused() > 0 {
            match Frame::alloc_rx(RX_BUF_SIZE) {
                Some(frame) => {
                    if ring.post_next(frame).is_none() {
                        break;
                    }
                }
                None => {
                    /* Out of frames: leave the slots empty and try again next
                     * pass. The ring keeps running on what it still holds. */
                    self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }

        /* Publish on the tail having moved, not on this function having
         * posted something: an error frame reposted in the harvest loop moves
         * it too, and if it took the last free slot the loop above adds
         * nothing. Keying off `posted` there would leave that descriptor
         * sitting in the ring with the chip never told it was available. */
        if ring.needs_tail_write() {
            /* Descriptors visible before the tail that points past them. */
            kcore::barrier::dma_wmb();
            self.regs.write32(RDT0, ring.tail());
            ring.mark_tail_written();
        }
    }

    /// Resample the receive rate if a sample window has passed, and adapt the
    /// interrupt throttle to it. Called at the head of the poll, one CPU at a
    /// time, with the time the pass began. Returns the current rate estimate.
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
            self.apply_eitr(pps);
        } else if last == 0 {
            self.rate_last_ns.store(now, Ordering::Relaxed);
            self.rate_last_pkts.store(self.rx_packets.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        self.rx_rate_pps.load(Ordering::Relaxed)
    }

    /// Widen the interrupt throttle with the rate: 2 us idle, 20 us at 400k a
    /// second and up. Written only when it changes -- and only on MSI-X. EITR
    /// throttles MSI-X vectors; on INTx the part holds interrupts apart
    /// through ITR, which this driver leaves alone, and writing EITR there
    /// would change nothing while igbdump reported it as the throttle in
    /// force.
    fn apply_eitr(&self, pps: u32) {
        if !self.is_msix() {
            return;
        }

        let want = (pps / EITR_PPS_PER_US).clamp(EITR_MIN_US, EITR_MAX_US);
        if want != self.eitr_us.load(Ordering::Relaxed) {
            self.regs.write32(EITR0, (want << EITR_INTERVAL_SHIFT) | EITR_CNT_IGNR);
            self.eitr_us.store(want, Ordering::Relaxed);
        }
    }

    /// The ring gave up a frame, or is being handed back to the interrupt: a
    /// run of repolls over an empty ring, if one was going, is over. Its
    /// length is what it cost; ending in a frame, it spared an interrupt.
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

    /// The ring is empty and the rate high: whether to look again on another
    /// softirq pass rather than arm -- yes until it has sat empty for
    /// REPOLL_NS.
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

    /// Where the poll leaves the ring, for the state dump.
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

        /* Harvested frames wait here until the batch is complete, so the
         * receive queue's lock is taken once rather than once per frame. */
        let mut batch = FrameQueue::new();

        /* Whether this pass took anything, in any of its rounds. */
        let mut took = false;

        loop {
            RX_POLLS.fetch_add(1, Ordering::Relaxed);

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

                /* A frame that is not end-of-packet would be one piece of a
                 * multi-descriptor packet. RCTL.LPE is off and the buffer is
                 * 2 KiB, so the chip has no way to make one -- but a frame
                 * that claims to be a fragment is not one to pass up. */
                if status & RXD_ERR_MASK != 0 || status & RXD_STAT_EOP == 0 || len == 0 {
                    self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                    frame.set_len(0);

                    /* Straight back into the ring rather than out through the
                     * pool and in again: the buffer is still perfectly good,
                     * and the receive path has no business allocating. The
                     * slot is there because the harvest above just freed one,
                     * so this cannot fail. */
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

            /* Hand the harvest over in one piece, then give the chip its
             * buffers back. Refilling after the batch keeps the ring supplied
             * from the pool the dispatch below will replenish. */
            up.deliver(&mut batch);
            self.refill_rx(ring);

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
            if !ring.has_work() {
                /* Running hot: rather than arm the interrupt for a frame a
                 * couple of microseconds away, look again on the softirq's
                 * next pass, receive sources still masked. After a pass that
                 * took frames that is simply the next pass -- they are
                 * dispatched in between, which is where a flood's time goes.
                 * After one that took nothing it is a repoll proper, and
                 * REPOLL_NS of those in a row hands the ring back; under a
                 * sustained flood the ring never sits empty that long, so the
                 * interrupt is spared for as long as the flood lasts. Only a
                 * pass that took nothing starts that clock: one started by a
                 * pass with frames would count their dispatch, which comes
                 * after this function returns, as time the ring sat empty. */
                if hot && (took || self.repoll(now)) {
                    softirq::raise(softirq::TYPE_NET_RX);
                    return;
                }

                /* Empty: hand the ring back to the interrupt. */
                self.end_repolls(now, false);
                self.arm_rx();

                /* Re-checked after arming, for a frame that landed between
                 * the last harvest and the write above. */
                if !ring.has_work() {
                    return;
                }

                self.mask_rx();
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
/* Transmit: called by the net stack under the device's transmit lock */

/* A write-back asked for every this many descriptors of a run, besides the
 * run's last one: often enough that a long run gives its frames back while
 * it is still going out, rarely enough that a run of echoes costs the chip
 * one descriptor write and the CPU one TXDW interrupt, not one a packet. */
const TX_RS_EVERY: u32 = 32;

impl NetDriver for IgbDevice {
    type Tx = TxRing;
    type Rx = RxRing;

    fn flush_tx(&'static self, ring: &mut TxRing, stack: &mut TxQueue<'_>) {
        /* Give back what the chip has finished before asking for room. */
        ring.reap_completed(stack);

        let mut submitted: u32 = 0;
        while ring.can_submit() {
            let frame = match stack.dequeue() {
                None => break,
                Some(f) => f,
            };

            let rs = (submitted + 1) % TX_RS_EVERY == 0;
            if let Err(frame) = ring.submit(frame, rs) {
                /* There was room a moment ago; handed back rather than
                 * dropped, as everything is under this lock. */
                stack.done(frame);
                break;
            }
            submitted += 1;
        }

        if submitted != 0 {
            /* Only a descriptor that reports lets the ones before it be
             * reaped, so every run ends with one. */
            ring.report_last();

            self.tx_packets.fetch_add(submitted as u64, Ordering::Relaxed);

            /* Descriptors visible before the doorbell that points past them. */
            kcore::barrier::dma_wmb();
            self.regs.write32(TDT0, ring.tail());
        }
    }

    fn process_rx(&'static self, ring: &mut RxRing, up: &mut RxQueue<'_>) {
        self.poll_rx(ring, up);
        self.publish_rx(ring);
    }
}

/* ================================================================== */
/* State dump, for the `igbdump` shell command.
 *
 * Reading the chip rather than the driver's idea of it: the receive stall on
 * the other card in this tree was found that way and not by any amount of
 * reading driver code. ICR is deliberately absent -- it is read-to-clear, so
 * a dump that showed it would also consume it. */

struct State {
    generation: Generation,
    phy_bmcr: u32,
    phy_bmsr: u32,
    phy_anar: u32,
    phy_anlpar: u32,
    phy_gctl: u32,
    phy_gstat: u32,
    ctrl: u32,
    status: u32,
    rctl: u32,
    tctl: u32,
    ims: u32,
    eitr: u32,
    stat_tpr: u64,
    stat_gprc: u64,
    stat_mpc: u64,
    stat_rnbc: u64,
    stat_rxerrc: u64,
    stat_rqdpc: u32,
    stat_pqgprc: u32,
    rxdctl: u32,
    srrctl: u32,
    rdh: u32,
    rdt: u32,
    tdh: u32,
    tdt: u32,
    next_to_clean: u32,
    next_to_use: u32,
    head_status: u32,
    head_posted: bool,
    rx_polls: u64,
    rx_budget_hits: u64,
    rx_err_events: u64,
    rx_packets: u64,
    rx_dropped: u64,
    tx_packets: u64,
    two_vector: bool,
    rx_rate_pps: u32,
    isr_queue: u64,
    isr_other: u64,
    rx_repoll_hits: u64,
    rx_repolls: u64,
    rx_repoll_ns: u64,
    /// On MSI-X, not INTx -- where EITR is not the throttle.
    msix: bool,
}

/// Add a read-clear register's delta to its running total and return it.
fn accumulate(total: &AtomicU64, delta: u32) -> u64 {
    total.fetch_add(delta as u64, Ordering::Relaxed) + delta as u64
}

/// The chip and the rings as they are this instant: what `igbdump` prints.
fn snapshot(dev: &'static IgbDevice) -> State {
    let regs = &dev.regs;

    /* What the PHY itself says, which on a machine with no console but this
     * NIC is the difference between "the cable is out" and "the driver never
     * brought the link up". Read under the same claim the bring-up takes; if
     * firmware will not give it up, report zeroes rather than whatever a
     * contended MDIO bus hands back. */
    let mut phy = [0u32; 6];
    let phy_locked = dev.generation != Generation::I210 || swfw_acquire(regs, SWFW_PHY0_SM);
    if phy_locked {
        let addr = dev.phy_addr;
        phy[0] = phy_read(regs, addr, PHY_BMCR).unwrap_or(0) as u32;

        /* Twice: the link bit in BMSR latches low, so the first read after
         * any drop reports the old state rather than the current one. */
        let _ = phy_read(regs, addr, PHY_BMSR);
        phy[1] = phy_read(regs, addr, PHY_BMSR).unwrap_or(0) as u32;

        phy[2] = phy_read(regs, addr, PHY_ANAR).unwrap_or(0) as u32;
        phy[3] = phy_read(regs, addr, PHY_ANLPAR).unwrap_or(0) as u32;
        phy[4] = phy_read(regs, addr, PHY_GCTL).unwrap_or(0) as u32;
        phy[5] = phy_read(regs, addr, PHY_GSTAT).unwrap_or(0) as u32;
        if dev.generation == Generation::I210 {
            swfw_release(regs, SWFW_PHY0_SM);
        }
    }

    let ctrl = regs.read32(CTRL);
    let status = regs.read32(STATUS);
    let rctl = regs.read32(RCTL);
    let tctl = regs.read32(TCTL);
    let ims = regs.read32(IMS);
    let eitr = (regs.read32(EITR0) & EITR_INTERVAL_MASK) >> EITR_INTERVAL_SHIFT;

    /* Read-clear, so each read is a delta and has to be added on. */
    let stat_tpr = accumulate(&STAT_TPR, regs.read32(TPR));
    let stat_gprc = accumulate(&STAT_GPRC, regs.read32(GPRC));
    let stat_mpc = accumulate(&STAT_MPC, regs.read32(MPC));
    let stat_rnbc = accumulate(&STAT_RNBC, regs.read32(RNBC));
    let stat_rxerrc = accumulate(&STAT_RXERRC, regs.read32(RXERRC));

    /* Not read-clear: taken as they stand. */
    let stat_rqdpc = regs.read32(RQDPC0);
    let stat_pqgprc = regs.read32(PQGPRC0);
    let rxdctl = regs.read32(RXDCTL0);
    let srrctl = regs.read32(SRRCTL0);
    let rdh = regs.read32(RDH0);
    let rdt = regs.read32(RDT0);
    let tdh = regs.read32(TDH0);
    let tdt = regs.read32(TDT0);

    /* Where the poll last left the ring -- and what the chip has written,
     * by now, into the descriptor the poll would look at next: read out of
     * the ring itself, this instant, not out of what the poll remembers. */
    let next_to_clean = dev.rx_next_to_clean.load(Ordering::Relaxed);

    State {
        generation: dev.generation,
        phy_bmcr: phy[0],
        phy_bmsr: phy[1],
        phy_anar: phy[2],
        phy_anlpar: phy[3],
        phy_gctl: phy[4],
        phy_gstat: phy[5],
        ctrl,
        status,
        rctl,
        tctl,
        ims,
        eitr,
        stat_tpr,
        stat_gprc,
        stat_mpc,
        stat_rnbc,
        stat_rxerrc,
        stat_rqdpc,
        stat_pqgprc,
        rxdctl,
        srrctl,
        rdh,
        rdt,
        tdh,
        tdt,
        next_to_clean,
        next_to_use: dev.rx_next_to_use.load(Ordering::Relaxed),
        head_status: dev.rx_view.status(next_to_clean as usize),
        head_posted: dev.rx_head_posted.load(Ordering::Relaxed),
        rx_polls: RX_POLLS.load(Ordering::Relaxed),
        rx_budget_hits: RX_BUDGET_HITS.load(Ordering::Relaxed),
        rx_err_events: RX_ERR_EVENTS.load(Ordering::Relaxed),
        rx_packets: dev.rx_packets.load(Ordering::Relaxed),
        rx_dropped: dev.rx_dropped.load(Ordering::Relaxed),
        tx_packets: dev.tx_packets.load(Ordering::Relaxed),
        two_vector: dev.two_vector.load(Ordering::Relaxed),
        rx_rate_pps: dev.rx_rate_pps.load(Ordering::Relaxed),
        isr_queue: dev.isr_queue.load(Ordering::Relaxed),
        isr_other: dev.isr_other.load(Ordering::Relaxed),
        rx_repoll_hits: dev.rx_repoll_hits.load(Ordering::Relaxed),
        rx_repolls: dev.rx_repolls.load(Ordering::Relaxed),
        rx_repoll_ns: dev.rx_repoll_ns.load(Ordering::Relaxed),
        msix: dev.is_msix(),
    }
}

/// `igbdump`: the first igb's state, a line to a question somebody has had
/// to ask of a machine whose only console is this NIC.
fn dump(_args: &str, out: &mut Output) {
    let dev = match DEVICES[0].get() {
        Some(dev) => *dev,
        None => {
            let _ = writeln!(out, "igbdump: no igb");
            return;
        }
    };
    let st = snapshot(dev);

    /* The prefetch, host and write-back thresholds: five bits each. */
    const XDCTL_THRESH_FIELD: u32 = 0x1F;
    const XDCTL_HTHRESH_SHIFT: u32 = 8;
    const XDCTL_WTHRESH_SHIFT: u32 = 16;
    const NS_PER_US: u64 = 1000;

    let bit = |word: u32, mask: u32| (word & mask != 0) as u32;

    let _ = writeln!(out, "part {}", if st.generation == Generation::I210 { "I210" } else { "82576" });
    let _ = writeln!(out, "ctrl 0x{:X} status 0x{:X} link {}", st.ctrl, st.status, bit(st.status, STATUS_LU));
    /* The PHY's own view, which is what tells a link the driver never brought
     * up apart from a cable that is not plugged in. */
    let _ = writeln!(out, "phy bmcr 0x{:X} bmsr 0x{:X} link {} autoneg-done {}",
        st.phy_bmcr, st.phy_bmsr,
        bit(st.phy_bmsr, BMSR_LSTATUS as u32), bit(st.phy_bmsr, BMSR_ANEGCOMPLETE as u32));
    /* What we offered against what came back: a link that resolves lower than
     * it should is one or the other, and nothing else distinguishes them. */
    let _ = writeln!(out, "phy adv 0x{:X} partner 0x{:X}  1000: ctrl 0x{:X} status 0x{:X}",
        st.phy_anar, st.phy_anlpar, st.phy_gctl, st.phy_gstat);
    let _ = writeln!(out, "rctl 0x{:X} rx-en {}  tctl 0x{:X} tx-en {}  ims 0x{:X}",
        st.rctl, bit(st.rctl, RCTL_EN), st.tctl, bit(st.tctl, TCTL_EN), st.ims);
    /* Microseconds the chip holds interrupts apart. Firmware leaves a value
     * here that a device reset does not clear, and it caps the receive rate
     * on its own -- on MSI-X. On INTx the part throttles through ITR, and
     * this register, which the driver then leaves alone, is not in force. */
    let _ = writeln!(out, "interrupt throttle {} us{}, rx rate {} pps",
        st.eitr, if st.msix { "" } else { " (not in force: INTx)" }, st.rx_rate_pps);
    let _ = writeln!(out, "interrupts: {}, queue {}, other {}",
        if !st.msix { "INTx" } else if st.two_vector { "queue + other vector" } else { "one vector" },
        st.isr_queue, st.isr_other);
    /* Under load the poll looks at an empty ring again rather than arm the
     * interrupt: how many such passes, how many runs of them a frame ended --
     * an interrupt spared each time -- and how long the ring sat empty
     * through them, which is what they cost. The receive softirq's CPU in
     * `top`, less that, is the work. */
    let _ = writeln!(out, "rx repolls {}, {} runs ended by a frame, {} us on an empty ring",
        st.rx_repolls, st.rx_repoll_hits, st.rx_repoll_ns / NS_PER_US);
    /* The prefetch thresholds live in the low fields of RXDCTL. Zero there
     * means the chip never prefetches descriptors and drops packets with a
     * full ring, so they are worth reading back rather than assuming. */
    let _ = writeln!(out, "rxdctl 0x{:X} queue-en {} (pthresh {} hthresh {} wthresh {}) srrctl 0x{:X}",
        st.rxdctl, bit(st.rxdctl, XDCTL_QUEUE_ENABLE),
        st.rxdctl & XDCTL_THRESH_FIELD,
        (st.rxdctl >> XDCTL_HTHRESH_SHIFT) & XDCTL_THRESH_FIELD,
        (st.rxdctl >> XDCTL_WTHRESH_SHIFT) & XDCTL_THRESH_FIELD,
        st.srrctl);
    let _ = writeln!(out, "rx ring: rdh {} rdt {}  clean {} use {}",
        st.rdh, st.rdt, st.next_to_clean, st.next_to_use);
    let _ = writeln!(out, "rx head: status 0x{:X} dd {} posted {}",
        st.head_status, bit(st.head_status, RXD_STAT_DD), st.head_posted as u32);
    let _ = writeln!(out, "tx ring: tdh {} tdt {} packets {}", st.tdh, st.tdt, st.tx_packets);
    let _ = writeln!(out, "rx packets {} dropped {} err events {}",
        st.rx_packets, st.rx_dropped, st.rx_err_events);
    let _ = writeln!(out, "rx polls {}, of them budget-limited {}", st.rx_polls, st.rx_budget_hits);
    /* The chip's own view, which the ring counters cannot see: a frame the MAC
     * dropped before it reached for a descriptor never appears above. TPR is
     * everything taken off the wire, MPC what the receive FIFO had no room
     * for, RNBC what found no descriptor waiting. */
    let _ = writeln!(out, "mac: total {} good {} missed {} no-buffer {} errors {}",
        st.stat_tpr, st.stat_gprc, st.stat_mpc, st.stat_rnbc, st.stat_rxerrc);
    /* Per queue. RQDPC counts packets the queue was offered and had no
     * descriptor for -- the one drop nothing else in this dump can see. */
    let _ = writeln!(out, "queue0: good {} dropped-no-descriptor {}", st.stat_pqgprc, st.stat_rqdpc);
}
