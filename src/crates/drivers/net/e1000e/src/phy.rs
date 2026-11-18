/* The 82574's PHY: a BME1000, reached through MDIC while software owns the
 * MDIO bus.
 *
 * The bus is shared with the manageability firmware, so every access is
 * made holding the 82574's semaphore for it (EXTCNF_CTRL, regs.rs), and the
 * holding is a value: `Mdio` is the only way to a PHY register, and the bus
 * goes back to the firmware when it is dropped. Two masters on one MDIO bus
 * do not fail cleanly -- they produce reads that look like data -- so a
 * read without the claim is not something this file can express.
 *
 * What is written, and in which order, is Linux's e1000e for the 82574:
 * e1000e_copper_link_setup_m88 for the BM PHY, then the advertisement and
 * the restart of e1000_copper_link_autoneg. */

use kcore::io::MmioRegion;
use kcore::klog;
use kcore::time::delay_us;

use crate::regs::*;
use crate::wait_for;

/// Software's hold on the MDIO bus.
pub struct Mdio<'a> {
    regs: &'a MmioRegion,
}

impl<'a> Mdio<'a> {
    /// Take the bus from the firmware: None when it does not let go in the
    /// tries Linux gives it (e1000_get_hw_semaphore_82573). The bit is set
    /// and read back; it only reads back set once the claim has taken.
    pub fn take(regs: &'a MmioRegion) -> Option<Mdio<'a>> {
        for _ in 0..MDIO_OWNERSHIP_TRIES {
            let extcnf = regs.read32(EXTCNF_CTRL);
            regs.write32(EXTCNF_CTRL, extcnf | EXTCNF_CTRL_MDIO_SW_OWNERSHIP);
            if regs.read32(EXTCNF_CTRL) & EXTCNF_CTRL_MDIO_SW_OWNERSHIP != 0 {
                return Some(Mdio { regs });
            }
            delay_us(MDIO_OWNERSHIP_WAIT_US);
        }
        /* As Linux does on the timeout: whatever half of a claim was made
         * is let go of, so that the firmware is not locked out by it. */
        give_back(regs);
        None
    }

    /// One MDIC transaction, waited for: the register's value, None when it
    /// timed out or the PHY said error.
    fn transact(&self, command: u32) -> Option<u32> {
        self.regs.write32(MDIC, command);
        let mut value = 0u32;
        let ready = wait_for(MDIC_WAIT_US, MDIC_WAIT_TRIES, || {
            value = self.regs.read32(MDIC);
            value & MDIC_READY != 0
        });
        if !ready || value & MDIC_ERROR != 0 {
            return None;
        }
        Some(value)
    }

    /// The registers past 15 are paged: page 0 is chosen first, as
    /// e1000e_read_phy_reg_bm2 does, so that a page some firmware left
    /// selected does not take the access somewhere else.
    fn select_page(&self, reg: u32) -> bool {
        if reg < PHY_MULTI_PAGE_MIN {
            return true;
        }
        self.transact(mdic_write(PHY_PAGE_SELECT, 0)).is_some()
    }

    pub fn read(&self, reg: u32) -> Option<u16> {
        if !self.select_page(reg) {
            return None;
        }
        self.transact((reg << MDIC_REG_SHIFT) | (PHY_ADDR << MDIC_PHY_SHIFT) | MDIC_OP_READ)
            .map(|v| (v & MDIC_DATA_MASK) as u16)
    }

    pub fn write(&self, reg: u32, data: u16) -> bool {
        self.select_page(reg) && self.transact(mdic_write(reg, data)).is_some()
    }
}

impl Drop for Mdio<'_> {
    fn drop(&mut self) {
        give_back(self.regs);
    }
}

/* MDIC is polled every 50 us, for up to 5 ms: a transaction takes some
 * 64 us of MDIO clock, and a PHY that has not answered in a hundred times
 * that is not going to. */
const MDIC_WAIT_US: u64 = 50;
const MDIC_WAIT_TRIES: u32 = 100;

/* A PHY's soft reset (BMCR.RESET) says it is done by clearing the bit;
 * given half a second. */
const PHY_RESET_WAIT_US: u64 = 5000;
const PHY_RESET_WAIT_TRIES: u32 = 100;

fn mdic_write(reg: u32, data: u16) -> u32 {
    u32::from(data) | (reg << MDIC_REG_SHIFT) | (PHY_ADDR << MDIC_PHY_SHIFT) | MDIC_OP_WRITE
}

fn give_back(regs: &MmioRegion) {
    let extcnf = regs.read32(EXTCNF_CTRL);
    regs.write32(EXTCNF_CTRL, extcnf & !EXTCNF_CTRL_MDIO_SW_OWNERSHIP);
}

/// The PHY's identity, revision included: what Linux checks the part by
/// (BME1000_E_PHY_ID_R2 for every 82574). None when it does not answer.
pub fn id(mdio: &Mdio) -> Option<u32> {
    let high = mdio.read(PHY_ID1)?;
    let low = mdio.read(PHY_ID2)?;
    Some((u32::from(high) << 16) | u32::from(low))
}

/// What Linux calls committing the PHY's settings: a soft reset, which the
/// M88-style control register is only read back from. Waited for, where
/// Linux gives it a microsecond: the writes that follow go to registers the
/// reset is still putting in place.
fn commit(mdio: &Mdio) -> bool {
    let bmcr = match mdio.read(PHY_BMCR) {
        Some(v) => v,
        None => return false,
    };
    if !mdio.write(PHY_BMCR, bmcr | BMCR_RESET) {
        return false;
    }
    wait_for(PHY_RESET_WAIT_US, PHY_RESET_WAIT_TRIES, || {
        mdio.read(PHY_BMCR).is_some_and(|v| v & BMCR_RESET == 0)
    })
}

/// Set the PHY up for the link and restart auto-negotiation: powered up,
/// automatic crossover, downshift on, every speed offered but 1000 half, no
/// pause. Returns the PHY's identity, for the log; false where a step the
/// link needs was refused, the log saying which.
///
/// Auto-negotiation is only started here: it completes in a second or
/// three, and the link-change interrupt says when.
pub fn setup_link(mdio: &Mdio) -> (u32, bool) {
    let id = match id(mdio) {
        Some(id) => id,
        None => {
            klog!(0, "e1000e: the PHY does not answer on MDIC");
            return (0, false);
        }
    };
    if id & PHY_ID_MASK != PHY_ID_BME1000 {
        klog!(0, "e1000e: PHY id {:#010x} is not the 82574's BME1000; set up as one", id);
    }
    let r2 = id == PHY_ID_BME1000_R2;

    /* Out of power-down: a driver that went before -- the last OS's, at
     * its shutdown -- may have left it there, and a powered-down PHY keeps
     * its settings but has no link to report (e1000_power_up_phy_copper). */
    match mdio.read(PHY_BMCR) {
        Some(bmcr) if bmcr & BMCR_PDOWN != 0 => {
            if !mdio.write(PHY_BMCR, bmcr & !BMCR_PDOWN) {
                klog!(0, "e1000e: the PHY would not leave power-down");
                return (id, false);
            }
        }
        Some(_) => {}
        None => {
            klog!(0, "e1000e: the PHY's control register cannot be read");
            return (id, false);
        }
    }

    let mut pscr = match mdio.read(PHY_PSCR) {
        Some(v) => v,
        None => {
            klog!(0, "e1000e: the PHY's specific control register cannot be read");
            return (id, false);
        }
    };
    pscr = (pscr & !PSCR_AUTO_X_MODE) | PSCR_AUTO_X_MODE;
    pscr &= !PSCR_POLARITY_REVERSAL;

    /* On revision 2 downshift is switched off and committed before it is
     * switched on, as Linux does for the 82574 -- the bit only takes from
     * off. */
    if r2 {
        pscr &= !PSCR_ENABLE_DOWNSHIFT;
        if !mdio.write(PHY_PSCR, pscr) || !commit(mdio) {
            klog!(0, "e1000e: the PHY refused its downshift setting");
            return (id, false);
        }
    }
    pscr |= PSCR_ENABLE_DOWNSHIFT;
    if !mdio.write(PHY_PSCR, pscr) {
        klog!(0, "e1000e: the PHY refused its specific control setting");
        return (id, false);
    }

    /* Revision 2's two undocumented registers, as Linux sets them. Not a
     * reason to give up the link if refused -- QEMU's model of the PHY has
     * neither -- but worth a line. */
    if r2
        && (!mdio.write(PHY_BM_R2_REG_29, PHY_BM_R2_REG_29_VALUE)
            || !mdio.write(PHY_BM_R2_REG_30, PHY_BM_R2_REG_30_VALUE))
    {
        klog!(0, "e1000e: the PHY refused its revision-2 settings (registers 29 and 30)");
    }

    if !commit(mdio) {
        klog!(0, "e1000e: the PHY's soft reset did not complete");
        return (id, false);
    }

    /* What to offer. The reset above may have put the advertisement back
     * to what the NVM says; this is what the link is asked to be. */
    let anar = mdio.read(PHY_ANAR).unwrap_or(0);
    let anar = (anar & !(ANAR_SPEEDS | ANAR_PAUSE_CAP | ANAR_PAUSE_ASYM)) | ANAR_SPEEDS | ANAR_SELECTOR_802_3;
    let gctl = mdio.read(PHY_GCTL).unwrap_or(0);
    let gctl = (gctl & !(GCTL_1000_HALF | GCTL_1000_FULL)) | GCTL_1000_FULL;
    if !mdio.write(PHY_ANAR, anar) || !mdio.write(PHY_GCTL, gctl) {
        klog!(0, "e1000e: the PHY refused the advertisement");
        return (id, false);
    }

    let bmcr = match mdio.read(PHY_BMCR) {
        Some(v) => v,
        None => {
            klog!(0, "e1000e: the PHY's control register cannot be read");
            return (id, false);
        }
    };
    if !mdio.write(PHY_BMCR, (bmcr & !BMCR_PDOWN) | BMCR_ANENABLE | BMCR_ANRESTART) {
        klog!(0, "e1000e: the PHY would not restart auto-negotiation");
        return (id, false);
    }

    (id, true)
}

/// The PHY's view of the link, for the state dump: BMCR, BMSR (read twice:
/// its link bit latches low), the advertisement both ways, both 1000BASE-T
/// registers. Zeroes for what does not answer.
pub fn snapshot(mdio: &Mdio) -> [u16; 6] {
    let _ = mdio.read(PHY_BMSR);
    [
        mdio.read(PHY_BMCR).unwrap_or(0),
        mdio.read(PHY_BMSR).unwrap_or(0),
        mdio.read(PHY_ANAR).unwrap_or(0),
        mdio.read(PHY_ANLPAR).unwrap_or(0),
        mdio.read(PHY_GCTL).unwrap_or(0),
        mdio.read(PHY_GSTAT).unwrap_or(0),
    ]
}
