/* RTL8125 bring-up: chip identification, the MAC OCP window, and the reset
 * and start sequences the chip needs before its DMA engines will run.
 *
 * Everything here executes from `rust_init()`, which the BSP calls with
 * interrupts still disabled, so nothing in this file may sleep -- the delays
 * are busy waits against the HPET (see `udelay`).
 *
 * The sequences mirror the vendor bring-up as documented by the Linux r8169
 * driver.  Most of the writes are undocumented errata workarounds; the ones
 * this driver's correctness actually depends on are called out in comments:
 * the legacy TX descriptor format, RSS/multi-queue off, and the RX datapath
 * gate.  The rest is kept because the chip is not known to work without it.
 */

use kcore::{io, trace};

use crate::regs::*;

/* ================================================================== */
/* Chip revision */

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    /* XID 0x609 -- RTL8125A */
    A,
    /* XID 0x641 -- RTL8125B, the revision on B760/B650-class boards */
    B,
}

impl Chip {
    pub fn as_str(self) -> &'static str {
        match self {
            Chip::A => "RTL8125A",
            Chip::B => "RTL8125B",
        }
    }
}

/// Identify the chip from the XID field at the top of TX_CONFIG.
/// An unrecognised XID is reported as `None`; the caller decides whether to
/// go on (a later revision is more likely to behave like the 8125B than to
/// need a different driver).
pub fn identify(regs: &io::MmioRegion) -> Option<Chip> {
    let xid = (regs.read32(TX_CONFIG) >> TX_CONFIG_XID_SHIFT) & TX_CONFIG_XID_MASK;
    match xid & XID_MATCH_MASK {
        XID_RTL8125A => Some(Chip::A),
        XID_RTL8125B => Some(Chip::B),
        _ => None,
    }
}

pub fn read_xid(regs: &io::MmioRegion) -> u32 {
    (regs.read32(TX_CONFIG) >> TX_CONFIG_XID_SHIFT) & TX_CONFIG_XID_MASK
}

/* ================================================================== */
/* Busy delays */

const NS_PER_US: u64 = 1000;
/* Spin-loop iterations allowed per requested microsecond.  Used both as the
 * escape hatch when the HPET is present but not counting, and as the whole
 * delay when there is no HPET at all.  A `pause` is tens to ~150 cycles, so
 * this over-delivers by an order of magnitude -- every delay below only
 * needs a lower bound. */
const SPIN_PER_US: u64 = 1000;
const SPIN_MIN: u64 = 1000;

/// Busy-wait roughly `us` microseconds.  Never blocks indefinitely, even if
/// the time source turns out to be stopped.
pub fn udelay(us: u64) {
    let budget = us.saturating_mul(SPIN_PER_US).max(SPIN_MIN);

    if kcore::hpet::is_available() {
        let start = kcore::hpet::read_ns();
        let want = us.saturating_mul(NS_PER_US);
        let mut left = budget;
        while kcore::hpet::read_ns().wrapping_sub(start) < want {
            core::hint::spin_loop();
            left = left - 1;
            if left == 0 {
                return; /* counter is not advancing; do not hang the boot */
            }
        }
        return;
    }

    for _ in 0..budget {
        core::hint::spin_loop();
    }
}

/// Poll `cond` up to `tries` times, `us` microseconds apart.
/// Returns false if it never came true.
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
/* MAC OCP window
 *
 * The chip's internal register file is reached through a single 32-bit
 * doorbell: bit 31 selects write, bits 30:15 carry the (even) register
 * number, bits 15:0 the data.  A read is a write of the address with bit 31
 * clear, followed by a read back of the same register. */

pub fn mac_ocp_write(regs: &io::MmioRegion, reg: u16, data: u16) {
    debug_assert!(reg & 1 == 0);
    regs.write32(
        OCPDR,
        OCP_WRITE_FLAG | ((reg as u32) << OCP_REG_SHIFT) | (data as u32),
    );
}

pub fn mac_ocp_read(regs: &io::MmioRegion, reg: u16) -> u16 {
    debug_assert!(reg & 1 == 0);
    regs.write32(OCPDR, (reg as u32) << OCP_REG_SHIFT);
    regs.read32(OCPDR) as u16
}

/// Read-modify-write of one MAC OCP register: clear `mask`, then set `set`.
pub fn mac_ocp_modify(regs: &io::MmioRegion, reg: u16, mask: u16, set: u16) {
    let v = mac_ocp_read(regs, reg);
    mac_ocp_write(regs, reg, (v & !mask) | set);
}

/* ================================================================== */
/* Stopping the datapath */

/* Wait for both FIFOs to drain.  On the 8125B the drain has to be requested
 * (StopReq) and reports completion in two places. */
fn wait_txrx_fifo_empty(regs: &io::MmioRegion, chip: Chip) {
    if chip == Chip::B {
        regs.write8(CMD_REG, regs.read8(CMD_REG) | CMD_STOP_REQ);
    }

    if !wait_for(100, 42, || {
        regs.read8(MCU) & MCU_RXTX_EMPTY == MCU_RXTX_EMPTY
    }) {
        trace!(0, "r8125: FIFOs did not drain (MCU {:#04x})", regs.read8(MCU));
    }

    if chip == Chip::B
        && !wait_for(100, 42, || {
            regs.read16(INTR_MITIGATE) & MITIGATE_DRAINED == MITIGATE_DRAINED
        })
    {
        trace!(0, "r8125: TX/RX drain flag never set");
    }
}

/* Close the RX datapath gate so nothing arrives while the rings are being
 * reprogrammed.  hw_start() opens it again as its last step. */
fn enable_rxdvgate(regs: &io::MmioRegion, chip: Chip) {
    regs.write32(MISC, regs.read32(MISC) | MISC_RXDV_GATED_EN);
    udelay(2000);
    wait_txrx_fifo_empty(regs, chip);
}

fn wait_ll_share_fifo_ready(regs: &io::MmioRegion) {
    if !wait_for(100, 42, || regs.read8(MCU) & MCU_LINK_LIST_RDY != 0) {
        trace!(0, "r8125: link list not ready (MCU {:#04x})", regs.read8(MCU));
    }
}

/* ================================================================== */
/* Reset and initial handoff */

/// Take the MAC away from the firmware and put it in a known state.
/// Mirrors the vendor "hardware init" step, which runs before the soft reset.
pub fn hw_init(regs: &io::MmioRegion, chip: Chip) {
    enable_rxdvgate(regs, chip);

    regs.write8(CMD_REG, regs.read8(CMD_REG) & !(CMD_TX_EN | CMD_RX_EN));
    udelay(1000);

    /* Tell the MAC it is no longer in out-of-band (firmware-owned) mode. */
    regs.write8(MCU, regs.read8(MCU) & !MCU_NOW_IS_OOB);

    mac_ocp_modify(regs, 0xE8DE, 1 << 14, 0);
    wait_ll_share_fifo_ready(regs);

    mac_ocp_write(regs, 0xC0AA, 0x07D0);
    mac_ocp_write(regs, 0xC0A6, 0x0150);
    mac_ocp_write(regs, 0xC01E, 0x5555);
    wait_ll_share_fifo_ready(regs);
}

/// Soft-reset the MAC.  Returns false if the reset bit never cleared.
pub fn reset(regs: &io::MmioRegion) -> bool {
    regs.write8(CMD_REG, CMD_RESET);
    wait_for(100, 100, || regs.read8(CMD_REG) & CMD_RESET == 0)
}

/* ================================================================== */
/* Start sequence */

fn config_eee_mac(regs: &io::MmioRegion, chip: Chip) {
    match chip {
        Chip::B => {
            regs.write16(EEE_TXIDLE_TIMER, EEE_TXIDLE_VAL);
            mac_ocp_modify(regs, 0xE040, 0, (1 << 1) | (1 << 0));
        }
        Chip::A => {
            mac_ocp_modify(regs, 0xE040, 0, (1 << 1) | (1 << 0));
            mac_ocp_modify(regs, 0xEB62, 0, (1 << 2) | (1 << 1));
        }
    }
}

/// The chip-specific half of bring-up: everything between unlocking the
/// config registers and programming the rings.  Caller must have unlocked
/// CFG9346 and must lock it again afterwards.
pub fn hw_start(regs: &io::MmioRegion, chip: Chip) {
    /* Two steps of the vendor sequence are deliberately left out here.
     *
     * The PCIe ePHY table (electrical tuning of the link) is skipped: it
     * exists to make ASPM transitions reliable, and this driver keeps ASPM
     * off entirely.  It is also the one part of the sequence that cannot be
     * transcribed faithfully -- the upstream ePHY accessor masks the
     * register number to five bits, which folds the second half of the
     * 8125 table onto the first half, so the addresses in that table do not
     * mean what they appear to.
     *
     * The default ASPM entry latency (L0 7us / L1 16us) is skipped for the
     * same reason: it is programmed through the PCIe config-space side door
     * and only matters once ASPM is enabled. */

    /* Interrupt coalescing off: this driver wants an interrupt per batch,
     * not a timer-delayed one. */
    let mut off = COALESCE_BASE;
    while off < COALESCE_END {
        regs.write32(off, 0);
        off = off + 4;
    }

    /* Work around a chip issue when a PCIe reset arrives in L2/L3. */
    regs.write8(CONFIG3, regs.read8(CONFIG3) & !CONFIG3_RDY_TO_L23);

    regs.write16(MAGIC_0382, 0x221B);

    /* Single RX queue, no RSS -- this driver services one ring, and with
     * RSS on the chip would spread frames into rings that do not exist. */
    regs.write8(RSS_CTRL, 0);
    regs.write16(Q_NUM_CTRL, 0);

    /* Disable UPS: in ultra-low-power state the MAC stops answering. */
    mac_ocp_modify(regs, 0xD40A, 0x0010, 0x0000);

    /* Do not let the PHY drop to a lower speed on its own. */
    regs.write8(CONFIG1, regs.read8(CONFIG1) & !CONFIG1_SPEED_DOWN);

    mac_ocp_write(regs, 0xC140, 0xFFFF);
    mac_ocp_write(regs, 0xC142, 0xFFFF);

    mac_ocp_modify(regs, 0xD3E2, 0x0FFF, 0x03A9);
    mac_ocp_modify(regs, 0xD3E4, 0x00FF, 0x0000);
    mac_ocp_modify(regs, 0xE860, 0x0000, 0x0080);

    /* Legacy (16-byte) TX descriptor format.  desc.rs lays out descriptors
     * this way; with the new format selected the chip would read a
     * different structure and DMA from garbage addresses. */
    mac_ocp_modify(regs, 0xEB58, 0x0001, 0x0000);

    if chip == Chip::B {
        mac_ocp_modify(regs, 0xE614, 0x0700, 0x0200);
        mac_ocp_modify(regs, 0xE63E, 0x0C30, 0x0000);
    } else {
        mac_ocp_modify(regs, 0xE614, 0x0700, 0x0400);
        mac_ocp_modify(regs, 0xE63E, 0x0C30, 0x0020);
    }

    mac_ocp_modify(regs, 0xC0B4, 0x0000, 0x000C);
    mac_ocp_modify(regs, 0xEB6A, 0x00FF, 0x0033);
    mac_ocp_modify(regs, 0xEB50, 0x03E0, 0x0040);
    mac_ocp_modify(regs, 0xE056, 0x00F0, 0x0030);
    mac_ocp_modify(regs, 0xE040, 0x1000, 0x0000);
    mac_ocp_modify(regs, 0xEA1C, 0x0003, 0x0001);
    mac_ocp_modify(regs, 0xE0C0, 0x4F0F, 0x4403);
    mac_ocp_modify(regs, 0xE052, 0x0080, 0x0068);
    mac_ocp_modify(regs, 0xD430, 0x0FFF, 0x047F);

    mac_ocp_modify(regs, 0xEA1C, 0x0004, 0x0000);
    mac_ocp_modify(regs, 0xEB54, 0x0000, 0x0001);
    udelay(1);
    mac_ocp_modify(regs, 0xEB54, 0x0001, 0x0000);

    regs.write16(MAGIC_1880, regs.read16(MAGIC_1880) & !0x0030);

    /* Hand the on-chip MCU its start vector and wait for it to come up. */
    mac_ocp_write(regs, 0xE098, 0xC302);
    if !wait_for(1000, 10, || mac_ocp_read(regs, 0xE00E) & (1 << 13) == 0) {
        trace!(0, "r8125: MCU did not report ready (OCP 0xE00E)");
    }

    config_eee_mac(regs, chip);

    /* Let ASPM L1 exit on the events that matter for latency. */
    mac_ocp_modify(regs, 0xC0AC, 0, 0x1F80);

    /* Open the RX datapath gate closed by hw_init(). */
    regs.write32(MISC, regs.read32(MISC) & !MISC_RXDV_GATED_EN);
}

/// Keep the link out of ASPM/clock-request power states.  nos has no PCIe
/// power management, so the safe setting is "off": the vendor driver only
/// turns these on when the platform is known to manage them.
pub fn aspm_disable(regs: &io::MmioRegion) {
    mac_ocp_modify(regs, 0xE092, 0x00FF, 0);
    regs.write8(CONFIG2, regs.read8(CONFIG2) & !CONFIG2_CLKREQ_EN);
    regs.write8(CONFIG5, regs.read8(CONFIG5) & !CONFIG5_ASPM_EN);
}
