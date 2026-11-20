//! The calendar of a Unix time: the date `days` after 1970-01-01 -- Howard
//! Hinnant's `civil_from_days` -- and a time's date and time of day, in UTC.
//! Written once for everything that prints or encodes a date: an X.509
//! certificate's validity (`web::x509`), the clock a guest's RTC reads and
//! the dates in a VM's console log and cloud-init seed (`hv`), and the
//! stamps of the kernel's file logs (`fs::logs`). There were two copies of
//! it, the RTC's and the certificate's, before the third was wanted.
//!
//! No kernel and no `unsafe` in it, so the image, the modules and the host's
//! fuzzers link the same one.

#![no_std]
#![forbid(unsafe_code)]

pub const SECS_PER_DAY: u64 = 86_400;
const SECS_PER_HOUR: u64 = 3_600;
const SECS_PER_MINUTE: u64 = 60;

/// Days from 0000-03-01 to 1970-01-01, the shift `civil_from_days` counts
/// its eras from.
const DAYS_TO_EPOCH: u64 = 719_468;
const DAYS_PER_ERA: u64 = 146_097;

/// Year, month (1-12) and day (1-31) of the day `days` after 1970-01-01.
/// Total: a `days` so large that the shift to the eras would overflow --
/// no Unix time in seconds comes near it -- is clamped where it would.
pub fn civil(days: u64) -> (u64, u32, u32) {
    let z = days.saturating_add(DAYS_TO_EPOCH);
    let era = z / DAYS_PER_ERA;
    let doe = z - era * DAYS_PER_ERA;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    /* A day of a month and a month: each under 32, as the expressions
     * bound them, so the narrowing loses nothing */
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// A Unix time's date and time of day, in UTC, to the second.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Utc {
    pub year: u64,
    /// 1-12
    pub month: u32,
    /// 1-31
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Utc {
    /// `secs` after 1970-01-01 00:00:00 UTC.
    pub fn of(secs: u64) -> Utc {
        let (days, rest) = (secs / SECS_PER_DAY, secs % SECS_PER_DAY);
        let (year, month, day) = civil(days);
        /* Each under 86400 */
        Utc {
            year,
            month,
            day,
            hour: (rest / SECS_PER_HOUR) as u32,
            minute: (rest / SECS_PER_MINUTE % 60) as u32,
            second: (rest % 60) as u32,
        }
    }
}
