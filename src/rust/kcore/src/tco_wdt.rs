/*
 * Intel TCO Hardware Watchdog driver.
 *
 * The TCO (Total Cost of Ownership) watchdog is built into Intel PCH
 * (Platform Controller Hub) chipsets and accessed via I/O ports at TCOBASE.
 *
 * Discovery:
 *   1. Find the LPC/eSPI bridge: PCI bus 0, device 0x1F, function 0.
 *   2. Read TCOBASE from PCI config register 0x50, mask & 0xFFE0.
 *
 * On systems where the TCOBASE config register is not accessible via standard
 * PCI config reads (common on newer PCH), we fall back to reading the ACPI
 * PMBASE (config register 0x40) and adding the fixed 0x60 offset.
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

/* PMC GCS register to disable No Reboot bit (PCI config + fixed offsets) */
const PMC_BUS:    u16 = 0;
const PMC_SLOT:   u16 = 0x1F;
const PMC_FUNC:   u16 = 0;
const LPC_TCOBASE_REG: u16 = 0x50;
const LPC_ACPI_BASE_REG: u16 = 0x40;
const ACPI_TCO_OFFSET: u16 = 0x60;
const PMC_GCS_REG: u16 = 0xAC; /* General Control and Status in PMC config space */
const GCS_NO_REBOOT: u32 = 1 << 1;

/* Approximate seconds per TCO tick */
const TCO_TICK_NS: u64 = 600_000_000; /* 0.6 s */

pub struct TcoWatchdog {
    tco_base: u16,
}

impl TcoWatchdog {
    /// Probe for the Intel TCO watchdog.
    /// Returns None if the LPC bridge is not found or TCOBASE is zero.
    pub fn probe() -> Option<Self> {
        use crate::pci;

        /* Intel LPC/eSPI bridge is always at bus 0, device 0x1F, function 0 */
        let dev = pci::get_device_by_bdf(PMC_BUS, PMC_SLOT, PMC_FUNC)?;

        /* Only Intel PCH (vendor 0x8086) */
        if dev.vendor != 0x8086 {
            return None;
        }

        /* Try TCOBASE directly from PCI config 0x50 */
        let tco_raw = dev.read_config16(LPC_TCOBASE_REG);
        let tco_base: u16 = if tco_raw != 0 && tco_raw != 0xFFFF {
            tco_raw & 0xFFE0
        } else {
            /* Fallback: ACPI PMBASE + 0x60 */
            let acpi_base = dev.read_config16(LPC_ACPI_BASE_REG) & 0xFFFE;
            if acpi_base == 0 || acpi_base == 0xFFFE {
                return None;
            }
            acpi_base + ACPI_TCO_OFFSET
        };

        if tco_base == 0 {
            return None;
        }

        let wdt = Self { tco_base };

        /* Clear TMR_HLT so the watchdog can be started */
        let cnt = Port::<u16>::new(tco_base + TCO1_CNT).read();
        Port::<u16>::new(tco_base + TCO1_CNT).write(cnt & !TMR_HLT);

        /* Disable No Reboot bit in PMC GCS so the second expiry triggers a reset */
        let gcs = dev.read_config32(PMC_GCS_REG);
        dev.write_config32(PMC_GCS_REG, gcs & !GCS_NO_REBOOT);

        /* Clear stale timeout status bits */
        Port::<u16>::new(tco_base + TCO1_STS).write(1 << 3); /* TIMEOUT */
        Port::<u16>::new(tco_base + TCO2_STS).write(1 << 1); /* SECOND_TO_STS */

        Some(wdt)
    }

    /// Start the watchdog with the given timeout in seconds.
    /// The actual timeout is rounded to the nearest 0.6-second boundary.
    pub fn start(&self, timeout_secs: u32) {
        /* Convert seconds to TCO ticks (round up, minimum 2 ticks) */
        let ticks = {
            let t = ((timeout_secs as u64) * 1_000_000_000 + TCO_TICK_NS - 1) / TCO_TICK_NS;
            (t as u16).max(2).min(0x1FF)
        };

        /* Write timeout value; spec requires write to TMR before RLD */
        let tmr = Port::<u16>::new(self.tco_base + TCO_TMR);
        let cur = tmr.read() & !0x1FF;
        tmr.write(cur | ticks);

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
}
