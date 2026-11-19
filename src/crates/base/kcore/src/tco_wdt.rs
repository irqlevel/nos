/*
 * Intel TCO Hardware Watchdog driver.
 *
 * The TCO (Total Cost of Ownership) watchdog is built into Intel PCH
 * (Platform Controller Hub) chipsets and accessed via I/O ports at TCOBASE.
 *
 * Discovery, in order (the first match wins):
 *   1. 100-series PCH ("Sunrise Point", 2015) and later: TCOBASE lives in
 *      the SMBus function, PCI 0:1F.4 config 0x50, and the window is only
 *      decoded while TCO_BASE_EN (TCOCTL, config 0x54 bit 8) is set.  This
 *      is where Linux's i2c-i801 reads it from.  The LPC bridge registers
 *      used by (2) read back as zero on these chipsets, so this probe has
 *      to come first.
 *   2. ICH6 to the 9-series and the server PCHs of their time (C600, C610
 *      "Wellsburg"), and QEMU's q35 -- what Linux's lpc_ich calls TCO
 *      version 2: the block is in the LPC bridge's ACPI I/O space, at
 *      PMBASE (PCI 0:1F.0 config 0x40) plus the fixed 0x60. Nothing of
 *      these chipsets keeps a TCOBASE in the bridge's config 0x50: on the
 *      Supermicro X10DRi's C610 that register reads 0xF8, which was once
 *      taken for one and put the timer's writes on ports 0xE4 to 0xE8.
 *
 * NO_REBOOT (the bit that inhibits the reset on the second expiry) is where
 * the generation keeps it, and cleared where it can be reached:
 *   - on the (2) generations, in GCS, bit 5 of the chipset configuration
 *     registers' General Control and Status (RCBA + 0x3410: RCBA is the
 *     bridge's config 0xF0, bit 0 its enable), as Linux's iTCO_wdt clears
 *     it for version 2 -- cleared and read back, since a strap can hold it
 *     set;
 *   - from Cannon Lake (300-series, 2018) on, in TCO1_CNT itself -- bit 0,
 *     in the window (1) found: cleared and read back, as Linux's iTCO_wdt
 *     does for what it calls TCO version 6.  Only the SMBus function's
 *     device ID says a PCH is one of those (CNL_SMBUS_IDS);
 *   - the 100- and 200-series keep it behind the P2SB sideband
 *     (SBREG_BAR + 0xC6000C), on a device the firmware hides from PCI
 *     enumeration, so there it is left alone: if the firmware set it, the
 *     timer still counts but the board will not reset.
 * no_reboot() says which.  Callers that depend on reset-on-hang must check
 * is_counting() too -- see the TCO code in kernel/src/lib.rs.
 *
 * Timeout unit: each TCO timer tick is approximately 0.6 seconds.
 * The TCO timer reloads on write to TCO_RLD and fires twice:
 *   - First expiry: asserts NMI or SMI (configurable)
 *   - Second expiry: system reset
 * Bit TMR_HLT in TCO1_CNT halts the timer.
 *
 * To use this driver from rust_init():
 *
 *   if let Some(wdt) = tco_wdt::TcoWatchdog::probe() {
 *       wdt.start(30);  // 30-second timeout
 *       // ...store wdt somewhere and kick periodically...
 *   }
 */

use core::sync::atomic::{AtomicU16, Ordering};

use crate::io::Port;

/* TCO register offsets relative to TCOBASE */
const TCO_RLD:    u16 = 0x00; /* reload / current count (r/w, 10-bit) */
const TCO1_STS:   u16 = 0x04; /* status 1 (bit 3 = TIMEOUT) */
const TCO2_STS:   u16 = 0x06; /* status 2 (bit 1 = SECOND_TO_STS) */
const TCO1_CNT:   u16 = 0x08; /* control 1 (bit 11 = TMR_HLT) */
const TCO_TMR:    u16 = 0x12; /* timer initial value (bits 9:0) */

/* The timer's field: ten bits on every TCO from ICH6 on (version 2 and
   later; Linux's iTCO_wdt masks 0x3FF and refuses less than 4), the values
   0 to 3 ignored by the chipset. Nine bits were masked here, so a bit 9
   the firmware left set stayed in the value programmed, and 2 was taken
   as the least. */
const TCO_TMR_MASK: u16 = 0x3FF;
const TCO_TMR_MIN:  u16 = 4;

/* TMR_HLT: bit 11 of TCO1_CNT halts the watchdog */
const TMR_HLT: u16 = 1 << 11;

/* NMI_NOW, bit 8 of TCO1_CNT, flips when it is written as 1, so a value
   read with it set is never written back so: every update of the register
   writes it as 0 (update_cnt; Linux's iTCO_wdt masks it the same way) */
const NMI_NOW: u16 = 1 << 8;

/* From Cannon Lake on, NO_REBOOT is bit 0 of TCO1_CNT */
const CNT_NO_REBOOT: u16 = 1 << 0;

/* The SMBus functions of the PCHs that keep NO_REBOOT in TCO1_CNT: Cannon
   Lake's and those after it -- the ones Linux's i2c-i801 marks
   FEATURE_TCO_CNL. Comet Lake-V (0xA3A3) is a 200-series PCH by another
   name, which keeps it behind the P2SB, and is not here. */
const CNL_SMBUS_IDS: [u16; 24] = [
    0x18DF, /* Cedar Fork */
    0x1BC9, /* Emmitsburg */
    0xA323, /* Cannon Lake-H (300-series) */
    0x9DA3, /* Cannon Lake-LP */
    0x34A3, /* Ice Lake-LP */
    0x38A3, /* Ice Lake-N */
    0x02A3, /* Comet Lake-LP */
    0x06A3, /* Comet Lake-H (400-series) */
    0x4B23, /* Elkhart Lake */
    0xA0A3, /* Tiger Lake-LP */
    0x43A3, /* Tiger Lake-H (500-series) */
    0x4DA3, /* Jasper Lake */
    0x7AA3, /* Alder Lake-S (600-series) */
    0x51A3, /* Alder Lake-P */
    0x54A3, /* Alder Lake-M */
    0x7A23, /* Raptor Lake-S (700-series) */
    0x7E22, /* Meteor Lake-P */
    0xAE22, /* Meteor Lake SoC-S */
    0x7F23, /* Meteor Lake PCH-S (800-series) */
    0x7722, /* Arrow Lake-H */
    0xE322, /* Panther Lake-H */
    0xE422, /* Panther Lake-P */
    0x4D22, /* Wildcat Lake-U */
    0x6E23, /* Nova Lake-S */
];

/* TCOBASE decodes a 32-byte I/O window: bit 0 is a hardwired indicator and
   bits 4:1 are reserved, so mask them off before using it as a port base. */
const TCOBASE_MASK: u16 = 0xFFE0;

/* The PCH sits at bus 0, device 0x1F; the LPC/eSPI bridge is function 0 and
   the SMBus controller function 4. */
const PCH_BUS:    u16 = 0;
const PCH_SLOT:   u16 = 0x1F;
const LPC_FUNC:   u16 = 0;
const SMBUS_FUNC: u16 = 4;

const PCI_VENDOR_INTEL: u16 = 0x8086;

/* PCI config 0x08 holds the class code triple (class, subclass, prog_if)
   above the revision byte. */
const PCI_CLASS_REG: u16 = 0x08;
const PCI_CLASS_SERIAL_BUS: u8 = 0x0C;
const PCI_SUBCLASS_SMBUS: u8 = 0x05;

/* SMBus function registers: 100-series PCH and later */
const SMB_TCOBASE_REG: u16 = 0x50;
const SMB_TCOCTL_REG:  u16 = 0x54;
const TCOCTL_TCO_BASE_EN: u32 = 1 << 8;

/* LPC bridge registers: TCO version 2 chipsets */
const LPC_ACPI_BASE_REG: u16 = 0x40;
const ACPI_TCO_OFFSET: u16 = 0x60;
/* PMBASE decodes 128 ports: bit 0 says I/O space, bits 6:1 are reserved */
const ACPI_BASE_MASK: u16 = 0xFF80;

/* The chipset configuration registers: RCBA, config 0xF0 of the bridge,
   is their 16 KiB window's base (bits 31:14) and its decode enable (bit 0),
   and GCS is at 0x3410 in it -- NO_REBOOT its bit 5 */
const LPC_RCBA_REG: u16 = 0xF0;
const RCBA_ENABLE: u32 = 1 << 0;
const RCBA_BASE_MASK: u32 = 0xFFFF_C000;
const RCBA_GCS_OFFSET: u64 = 0x3410;
const GCS_NO_REBOOT: u32 = 1 << 5;
const PAGE_BYTES: u64 = 4096;

/* Approximate seconds per TCO tick */
const TCO_TICK_NS: u64 = 600_000_000; /* 0.6 s */

/* Where TCOBASE was found. */
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TcoBaseSource {
    PchSmbus,
    LpcAcpiBase,
}

impl TcoBaseSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TcoBaseSource::PchSmbus    => "SMBus 0:1F.4 TCOBASE",
            TcoBaseSource::LpcAcpiBase => "LPC 0:1F.0 ACPI base + 0x60",
        }
    }
}

/// What probe() made of NO_REBOOT, the bit that keeps the second expiry
/// from resetting the machine.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoReboot {
    /// Cleared in TCO1_CNT, and read back clear: a Cannon Lake or later PCH
    ClearedInCnt,
    /// Clear in GCS, read back so -- cleared there, or found so: a TCO
    /// version 2 chipset
    ClearedInGcs,
    /// Cleared and read back set: the firmware or a strap locked it so
    Locked,
    /// Out of reach -- behind the P2SB (the 100- and 200-series), or in
    /// chipset configuration registers the firmware left undecoded: as the
    /// firmware set it
    Left,
}

impl NoReboot {
    /// Whether a hang should reset the machine, as far as this bit goes:
    /// is_counting() says the rest.
    pub fn cleared(self) -> bool {
        matches!(self, NoReboot::ClearedInCnt | NoReboot::ClearedInGcs)
    }

    /// The bit's fate, for the line the kernel logs.
    pub fn as_str(self) -> &'static str {
        match self {
            NoReboot::ClearedInCnt => ", NO_REBOOT cleared in TCO1_CNT",
            NoReboot::ClearedInGcs => ", NO_REBOOT clear in GCS",
            NoReboot::Locked       => ", NO_REBOOT locked set by the firmware",
            NoReboot::Left         => ", NO_REBOOT left as firmware set it",
        }
    }
}

pub struct TcoWatchdog {
    tco_base: u16,
    /* Tick value programmed into TCO_TMR by start(); used by is_counting()
       to detect a timer that never runs (e.g. NO_REBOOT still set). Atomic
       because the kick timer is already running, and looking, when start()
       writes it. */
    armed_ticks: AtomicU16,
    source: TcoBaseSource,
    no_reboot: NoReboot,
}

impl TcoWatchdog {
    /// Probe for the Intel TCO watchdog.
    /// Returns None if no PCH TCO I/O window can be located.
    pub fn probe() -> Option<Self> {
        match Self::probe_pch_smbus() {
            Some(wdt) => Some(wdt),
            None => Self::probe_lpc(),
        }
    }

    /// I/O base of the TCO register block.
    pub fn base(&self) -> u16 {
        self.tco_base
    }

    /// Which discovery path produced the base.
    pub fn source(&self) -> TcoBaseSource {
        self.source
    }

    /// What became of NO_REBOOT.
    pub fn no_reboot(&self) -> NoReboot {
        self.no_reboot
    }

    /// 100-series PCH (Sunrise Point) and later.
    fn probe_pch_smbus() -> Option<Self> {
        use crate::pci;

        let dev = pci::get_device_by_bdf(PCH_BUS, PCH_SLOT, SMBUS_FUNC)?;
        if dev.vendor != PCI_VENDOR_INTEL {
            return None;
        }

        /* get_device_by_bdf() zero-fills the class of a device the PCI scan
           did not enumerate, so read the class code straight from config
           space: 0:1F.4 is not the SMBus controller on every chipset and its
           0x50 must not be mistaken for a TCOBASE. */
        let class = dev.read_config32(PCI_CLASS_REG);
        if (class >> 24) as u8 != PCI_CLASS_SERIAL_BUS
            || (class >> 16) as u8 != PCI_SUBCLASS_SMBUS
        {
            return None;
        }

        /* Without TCO_BASE_EN the window is not decoded and the port reads
           below would go nowhere. */
        if (dev.read_config32(SMB_TCOCTL_REG) & TCOCTL_TCO_BASE_EN) == 0 {
            return None;
        }

        let tco_base = (dev.read_config32(SMB_TCOBASE_REG) as u16) & TCOBASE_MASK;
        if tco_base == 0 {
            return None;
        }

        /* NO_REBOOT is where the generation keeps it (the file header): in
           TCO1_CNT from Cannon Lake on -- cleared, and read back, since
           firmware may have locked it set -- and otherwise behind the hidden
           P2SB device, left as the firmware set it. */
        let no_reboot = if CNL_SMBUS_IDS.contains(&dev.device) {
            if update_cnt(tco_base, CNT_NO_REBOOT, 0) & CNT_NO_REBOOT == 0 {
                NoReboot::ClearedInCnt
            } else {
                NoReboot::Locked
            }
        } else {
            NoReboot::Left
        };
        Some(Self::from_base(tco_base, TcoBaseSource::PchSmbus, no_reboot))
    }

    /// TCO version 2: ICH6 to the 9-series, C600 and C610, QEMU's q35.
    fn probe_lpc() -> Option<Self> {
        use crate::pci;

        let dev = pci::get_device_by_bdf(PCH_BUS, PCH_SLOT, LPC_FUNC)?;
        if dev.vendor != PCI_VENDOR_INTEL {
            return None;
        }

        /* PMBASE says it is I/O space in bit 0: a register that reads
           otherwise -- all ones from nothing there, or zero on a PCH that
           moved it to the PMC -- names no block */
        let raw = dev.read_config16(LPC_ACPI_BASE_REG);
        if raw & 1 == 0 || raw == 0xFFFF {
            return None;
        }
        let acpi_base = raw & ACPI_BASE_MASK;
        if acpi_base == 0 {
            return None;
        }
        /* The mask keeps it to 0xFF80 at the most: the TCO's 32 ports from
           0x60 in the 128 end at 0xFFFF at the highest */
        let tco_base = acpi_base + ACPI_TCO_OFFSET;

        let no_reboot = Self::clear_gcs_no_reboot(&dev);
        Some(Self::from_base(tco_base, TcoBaseSource::LpcAcpiBase, no_reboot))
    }

    /// NO_REBOOT in GCS (the file header) cleared where it is set, and read
    /// back: what became of it. Callers that need the reset still check
    /// is_counting().
    fn clear_gcs_no_reboot(dev: &crate::pci::PciDevice) -> NoReboot {
        let rcba = dev.read_config32(LPC_RCBA_REG);
        let base = rcba & RCBA_BASE_MASK;
        if rcba & RCBA_ENABLE == 0 || rcba == 0xFFFF_FFFF || base == 0 {
            return NoReboot::Left;
        }

        /* The page GCS is in, as device memory: map_mmio_region refuses one
           that is RAM, so a register misread cannot turn into a write into
           somebody's page */
        let gcs = u64::from(base) + RCBA_GCS_OFFSET;
        let page = gcs & !(PAGE_BYTES - 1);
        let Some(mapping) = crate::dma::PhysMapping::map(page, 1) else {
            return NoReboot::Left;
        };
        let regs = crate::io::MmioRegion::new(mapping.as_mut_ptr(), mapping.len());
        let at = (gcs - page) as usize;

        let value = regs.read32(at);
        if value & GCS_NO_REBOOT != 0 {
            regs.write32(at, value & !GCS_NO_REBOOT);
        }
        if regs.read32(at) & GCS_NO_REBOOT == 0 {
            NoReboot::ClearedInGcs
        } else {
            NoReboot::Locked
        }
    }

    /// Common register init once TCOBASE is known.
    fn from_base(tco_base: u16, source: TcoBaseSource, no_reboot: NoReboot) -> Self {
        let wdt = Self { tco_base, armed_ticks: AtomicU16::new(0), source, no_reboot };

        /* Clear TMR_HLT so the watchdog can be started */
        update_cnt(tco_base, TMR_HLT, 0);

        /* Clear stale timeout status bits */
        Port::<u16>::new(tco_base + TCO1_STS).write(1 << 3); /* TIMEOUT */
        Port::<u16>::new(tco_base + TCO2_STS).write(1 << 1); /* SECOND_TO_STS */

        wdt
    }

    /// Start the watchdog with the given timeout in seconds.
    /// The actual timeout is rounded to the nearest 0.6-second boundary.
    pub fn start(&self, timeout_secs: u32) {
        /* Convert seconds to TCO ticks, rounded up, clamped to what the
         * field takes -- clamped before the cast: a timeout past 10.9 hours
         * once wrapped to a handful of ticks */
        let ticks = {
            let t = (u64::from(timeout_secs) * 1_000_000_000).div_ceil(TCO_TICK_NS);
            t.clamp(u64::from(TCO_TMR_MIN), u64::from(TCO_TMR_MASK)) as u16
        };

        /* Write timeout value; spec requires write to TMR before RLD */
        let tmr = Port::<u16>::new(self.tco_base + TCO_TMR);
        let cur = tmr.read() & !TCO_TMR_MASK;
        tmr.write(cur | ticks);
        self.armed_ticks.store(ticks, Ordering::Relaxed);

        /* Reload (kick) to arm with the new value */
        self.kick();

        /* Ensure timer is not halted */
        update_cnt(self.tco_base, TMR_HLT, 0);
    }

    /// Stop (halt) the watchdog.
    pub fn stop(&self) {
        update_cnt(self.tco_base, 0, TMR_HLT);
    }

    /// Kick (pet) the watchdog to prevent reset.
    pub fn kick(&self) {
        Port::<u16>::new(self.tco_base + TCO_RLD).write(0x0001);
    }

    /// Current countdown value (the 10-bit field of TCO_RLD).
    pub fn current_count(&self) -> u16 {
        Port::<u16>::new(self.tco_base + TCO_RLD).read() & TCO_TMR_MASK
    }

    /// Returns `true` if the timer appears to be running.  Call this at
    /// least one tick (~0.6s) after `start()`/`kick()`: a count still equal
    /// to the armed value means the watchdog never started counting (e.g.
    /// NO_REBOOT could not be cleared on this chipset) and a hang will NOT
    /// trigger a reset.
    pub fn is_counting(&self) -> bool {
        self.current_count() != self.armed_ticks.load(Ordering::Relaxed)
    }
}

/// TCO1_CNT with the bits of `clear` cleared and those of `set` set, and
/// NMI_NOW written as 0 whatever it read; what the register reads after.
fn update_cnt(tco_base: u16, clear: u16, set: u16) -> u16 {
    let cnt = Port::<u16>::new(tco_base + TCO1_CNT);
    let value = cnt.read() & !NMI_NOW;
    cnt.write((value & !clear) | set);
    cnt.read()
}
