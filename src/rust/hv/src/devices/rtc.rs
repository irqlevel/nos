//! A minimal MC146818 real-time clock and its CMOS RAM, ports 0x70 (index)
//! and 0x71 (data).
//!
//! A guest reads the RTC to learn the wall-clock time at boot, and it reads
//! status register A's "update in progress" bit first and waits for it to
//! clear. With no RTC the data port floats to 0xFF, the update bit is always
//! set, and the guest waits there for good. What is emulated: an update bit
//! that is always clear, a valid-RAM bit that is set, and the host's own
//! wall clock -- read when the guest is made and counted on from there by
//! the host's clock since boot, UTC, in BCD and 24-hour time. A guest with
//! a clock months off takes every file its distribution shipped for one
//! from the future, and says so at every service it starts. What it writes
//! to the time registers does not stick: the host's clock is the time.

use kcore::time;

/// Ports.
pub const INDEX: u16 = 0x70;
pub const DATA: u16 = 0x71;

/* CMOS register numbers. */
const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY_OF_WEEK: u8 = 0x06;
const DAY_OF_MONTH: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0A;
const STATUS_B: u8 = 0x0B;
const STATUS_C: u8 = 0x0C;
const STATUS_D: u8 = 0x0D;
/// Where a PC's CMOS keeps the century, as ACPI's FADT names it by default.
const CENTURY: u8 = 0x32;

/// Status A: the update-in-progress bit, which is kept clear here.
const STATUS_A_UIP: u8 = 1 << 7;
/// Status B: 24-hour mode and BCD (bit 1 set = 24h, bit 2 clear = BCD).
const STATUS_B_24H: u8 = 1 << 1;
/// Status D: the valid-RAM-and-time bit, which says the battery is good.
const STATUS_D_VRT: u8 = 1 << 7;

/// The high bit of the index port disables the NMI; the register is the low
/// seven bits.
const INDEX_MASK: u8 = 0x7F;

/// What the guest is told when the host has no wall clock to go by (it
/// could not read its own RTC at boot, and counts from 1970): 2026-01-01
/// 00:00:00 UTC.
const FALLBACK_EPOCH: u64 = 1_767_225_600;
/// A wall clock before this (2020-01-01) is taken for one the host never
/// set.
const PLAUSIBLE_EPOCH: u64 = 1_577_836_800;

const SECS_PER_DAY: u64 = 86_400;
/// 1970-01-01 was a Thursday: the RTC counts Sunday as day 1.
const EPOCH_WEEKDAY: u64 = 4;

/// The RTC/CMOS.
pub struct Rtc {
    index: u8,
    /// General-purpose CMOS RAM the guest may write and read back.
    ram: [u8; 128],
    /// The wall clock when the guest was made, in seconds since 1970, and
    /// the host's clock since boot at that moment.
    epoch: u64,
    epoch_ns: u64,
}

fn bcd(v: u64) -> u8 {
    let v = (v % 100) as u8;
    (v / 10) << 4 | (v % 10)
}

/// Year, month (1-12) and day (1-31) of the day `days` after 1970-01-01:
/// Howard Hinnant's `civil_from_days`, for days that are not negative.
fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

impl Rtc {
    pub fn new() -> Self {
        let wall = time::wall_clock_secs();
        let epoch = if wall >= PLAUSIBLE_EPOCH { wall } else { FALLBACK_EPOCH };
        Self { index: 0, ram: [0; 128], epoch, epoch_ns: time::boot_time_ns() }
    }

    pub fn owns(port: u16) -> bool {
        port == INDEX || port == DATA
    }

    /// A time register as it reads now, in BCD; None for any other.
    fn clock(&self, reg: u8) -> Option<u8> {
        if !matches!(reg, SECONDS | MINUTES | HOURS | DAY_OF_WEEK | DAY_OF_MONTH | MONTH | YEAR | CENTURY) {
            return None;
        }
        let now = self.epoch + time::boot_time_ns().saturating_sub(self.epoch_ns) / kcore::consts::NS_PER_SEC;
        let (days, secs) = (now / SECS_PER_DAY, now % SECS_PER_DAY);
        let (year, month, day) = civil(days);
        Some(match reg {
            SECONDS => bcd(secs % 60),
            MINUTES => bcd(secs / 60 % 60),
            HOURS => bcd(secs / 3600),
            DAY_OF_WEEK => bcd((days + EPOCH_WEEKDAY) % 7 + 1),
            DAY_OF_MONTH => bcd(day),
            MONTH => bcd(month),
            YEAR => bcd(year),
            _ => bcd(year / 100),
        })
    }

    pub fn read(&mut self, port: u16) -> u8 {
        if port == INDEX {
            return self.index;
        }
        match self.index {
            STATUS_A => 0x26 & !STATUS_A_UIP, // a normal divider, update never in progress
            STATUS_B => STATUS_B_24H,
            STATUS_C => 0, // no interrupt pending
            STATUS_D => STATUS_D_VRT,
            reg => self.clock(reg).unwrap_or(self.ram[(reg & 0x7F) as usize]),
        }
    }

    pub fn write(&mut self, port: u16, value: u8) {
        if port == INDEX {
            self.index = value & INDEX_MASK;
            return;
        }
        /* The status and time registers are read-only here; other CMOS RAM
         * the guest may scribble on and read back. */
        match self.index {
            STATUS_A | STATUS_B | STATUS_C | STATUS_D => {}
            reg if self.clock(reg).is_some() => {}
            reg => self.ram[(reg & 0x7F) as usize] = value,
        }
    }
}
