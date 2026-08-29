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
 *      used by (2) and (3) read back as zero on these chipsets, so this
 *      probe has to come first.
 *   2. ICH9-class chipsets (and QEMU's q35): TCOBASE from the LPC bridge,
 *      PCI 0:1F.0 config 0x50.
 *   3. Same bridge, ACPI PMBASE (config 0x40) plus the fixed 0x60 offset.
 *
 * NO_REBOOT (the bit that inhibits the reset on the second expiry) is only
 * cleared on the (2)/(3) generations, where it sits in the LPC bridge's
 * config space.  From 100-series on it lives behind the P2SB sideband
 * (SBREG_BAR + 0xC6000C) on a device the firmware hides from PCI
 * enumeration, so it is left alone: if the firmware set it, the timer still
 * counts but the board will not reset.  Callers that depend on reset-on-hang
 * must check is_counting() -- see the TCO code in kernel/src/lib.rs.
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

use crate::io::Port;

/* TCO register offsets relative to TCOBASE */
const TCO_RLD:    u16 = 0x00; /* reload / current count (r/w, 9-bit) */
const TCO1_STS:   u16 = 0x04; /* status 1 (bit 3 = TIMEOUT) */
const TCO2_STS:   u16 = 0x06; /* status 2 (bit 1 = SECOND_TO_STS) */
const TCO1_CNT:   u16 = 0x08; /* control 1 (bit 11 = TMR_HLT) */
const TCO_TMR:    u16 = 0x12; /* timer initial value (lower 9 bits used) */

/* TMR_HLT: bit 11 of TCO1_CNT halts the watchdog */
const TMR_HLT: u16 = 1 << 11;

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

/* LPC bridge registers: ICH9-class chipsets */
const LPC_TCOBASE_REG: u16 = 0x50;
const LPC_ACPI_BASE_REG: u16 = 0x40;
const ACPI_TCO_OFFSET: u16 = 0x60;
const PMC_GCS_REG: u16 = 0xAC; /* General Control and Status in PMC config space */
const GCS_NO_REBOOT: u32 = 1 << 1;

/* Approximate seconds per TCO tick */
const TCO_TICK_NS: u64 = 600_000_000; /* 0.6 s */

/* Where TCOBASE was found.  Also says whether NO_REBOOT was reachable:
   only the two Lpc* sources have it in PCI config space. */
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TcoBaseSource {
    PchSmbus,
    LpcTcoBase,
    LpcAcpiBase,
}

impl TcoBaseSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TcoBaseSource::PchSmbus    => "SMBus 0:1F.4 TCOBASE",
            TcoBaseSource::LpcTcoBase  => "LPC 0:1F.0 TCOBASE",
            TcoBaseSource::LpcAcpiBase => "LPC 0:1F.0 ACPI base + 0x60",
        }
    }

    /// True when probe() was able to clear NO_REBOOT for this source.
    pub fn no_reboot_cleared(self) -> bool {
        self != TcoBaseSource::PchSmbus
    }
}

pub struct TcoWatchdog {
    tco_base: u16,
    /* Tick value programmed into TCO_TMR by start(); used by is_counting()
       to detect a timer that never runs (e.g. NO_REBOOT still set). */
    armed_ticks: u16,
    source: TcoBaseSource,
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

        /* NO_REBOOT is behind the hidden P2SB device on this generation
           (see the file header); leave it as the firmware set it. */
        Some(Self::from_base(tco_base, TcoBaseSource::PchSmbus))
    }

    /// ICH9-class chipsets, including QEMU's q35.
    fn probe_lpc() -> Option<Self> {
        use crate::pci;

        let dev = pci::get_device_by_bdf(PCH_BUS, PCH_SLOT, LPC_FUNC)?;
        if dev.vendor != PCI_VENDOR_INTEL {
            return None;
        }

        let tco_raw = dev.read_config16(LPC_TCOBASE_REG);
        let (tco_base, source) = if tco_raw != 0 && tco_raw != 0xFFFF {
            (tco_raw & TCOBASE_MASK, TcoBaseSource::LpcTcoBase)
        } else {
            /* Fallback: ACPI PMBASE + 0x60 */
            let acpi_base = dev.read_config16(LPC_ACPI_BASE_REG) & 0xFFFE;
            if acpi_base == 0 || acpi_base == 0xFFFE {
                return None;
            }
            (acpi_base + ACPI_TCO_OFFSET, TcoBaseSource::LpcAcpiBase)
        };

        if tco_base == 0 {
            return None;
        }

        /* Disable No Reboot so the second expiry triggers a reset.
           CAVEAT: the NO_REBOOT location is chipset-specific.  Config
           register 0xAC works on PCH generations where the PMC exposes GCS
           there; on ICH9-class chipsets it lives in RCBA MMIO (+0x3410)
           and this write has no effect.  Callers should verify the timer
           actually counts (is_counting()) before relying on it for reset. */
        let gcs = dev.read_config32(PMC_GCS_REG);
        dev.write_config32(PMC_GCS_REG, gcs & !GCS_NO_REBOOT);

        Some(Self::from_base(tco_base, source))
    }

    /// Common register init once TCOBASE is known.
    fn from_base(tco_base: u16, source: TcoBaseSource) -> Self {
        let wdt = Self { tco_base, armed_ticks: 0, source };

        /* Clear TMR_HLT so the watchdog can be started */
        let cnt = Port::<u16>::new(tco_base + TCO1_CNT).read();
        Port::<u16>::new(tco_base + TCO1_CNT).write(cnt & !TMR_HLT);

        /* Clear stale timeout status bits */
        Port::<u16>::new(tco_base + TCO1_STS).write(1 << 3); /* TIMEOUT */
        Port::<u16>::new(tco_base + TCO2_STS).write(1 << 1); /* SECOND_TO_STS */

        wdt
    }

    /// Start the watchdog with the given timeout in seconds.
    /// The actual timeout is rounded to the nearest 0.6-second boundary.
    pub fn start(&mut self, timeout_secs: u32) {
        /* Convert seconds to TCO ticks (round up, minimum 2 ticks) */
        let ticks = {
            let t = ((timeout_secs as u64) * 1_000_000_000 + TCO_TICK_NS - 1) / TCO_TICK_NS;
            (t as u16).max(2).min(0x1FF)
        };

        /* Write timeout value; spec requires write to TMR before RLD */
        let tmr = Port::<u16>::new(self.tco_base + TCO_TMR);
        let cur = tmr.read() & !0x1FF;
        tmr.write(cur | ticks);
        self.armed_ticks = ticks;

        /* Reload (kick) to arm with the new value */
        self.kick();

        /* Ensure timer is not halted */
        let cnt = Port::<u16>::new(self.tco_base + TCO1_CNT).read();
        Port::<u16>::new(self.tco_base + TCO1_CNT).write(cnt & !TMR_HLT);
    }

    /// Stop (halt) the watchdog.
    pub fn stop(&self) {
        let cnt = Port::<u16>::new(self.tco_base + TCO1_CNT).read();
        Port::<u16>::new(self.tco_base + TCO1_CNT).write(cnt | TMR_HLT);
    }

    /// Kick (pet) the watchdog to prevent reset.
    pub fn kick(&self) {
        Port::<u16>::new(self.tco_base + TCO_RLD).write(0x0001);
    }

    /// Current countdown value (9-bit field of TCO_RLD).
    pub fn current_count(&self) -> u16 {
        Port::<u16>::new(self.tco_base + TCO_RLD).read() & 0x1FF
    }

    /// Returns `true` if the timer appears to be running.  Call this at
    /// least one tick (~0.6s) after `start()`/`kick()`: a count still equal
    /// to the armed value means the watchdog never started counting (e.g.
    /// NO_REBOOT could not be cleared on this chipset) and a hang will NOT
    /// trigger a reset.
    pub fn is_counting(&self) -> bool {
        self.current_count() != self.armed_ticks
    }
}
