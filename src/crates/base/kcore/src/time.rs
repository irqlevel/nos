use crate::consts::{NS_PER_SEC, NS_PER_US};
use ffi::time;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Duration {
    pub nanos: u64,
}

impl Duration {
    pub const fn zero() -> Self {
        Self { nanos: 0 }
    }

    pub const fn from_nanos(nanos: u64) -> Self {
        Self { nanos }
    }

    /// Saturating, as `from_millis`: a wait "for ever" -- `u64::MAX`
    /// seconds -- is the longest there is, not what its product wraps to.
    pub const fn from_secs(secs: u64) -> Self {
        Self {
            nanos: secs.saturating_mul(NS_PER_SEC),
        }
    }

    pub const fn from_millis(ms: u64) -> Self {
        Self {
            nanos: ms.saturating_mul(1_000_000),
        }
    }

    pub fn as_nanos(self) -> u64 {
        self.nanos
    }

    pub fn as_secs(self) -> u64 {
        self.nanos / NS_PER_SEC
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.nanos.checked_add(other.nanos).map(|nanos| Self { nanos })
    }

    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            nanos: self.nanos.saturating_add(other.nanos),
        }
    }
}

#[inline]
pub fn boot_time() -> Duration {
    let mut secs: u64 = 0;
    let mut usecs: u64 = 0;
    unsafe {
        time::kernel_get_boot_time(&mut secs, &mut usecs);
    }
    Duration {
        nanos: secs * NS_PER_SEC + usecs * NS_PER_US,
    }
}

pub fn wall_clock_secs() -> u64 {
    time::kernel_get_wall_time_secs()
}

/// Nanoseconds since boot at the clock's full resolution -- what timing a
/// single I/O wants; boot_time() comes rounded to microseconds.
#[inline]
pub fn boot_time_ns() -> u64 {
    time::kernel_get_boot_time_ns()
}

/// The cycle counter as it stands: the TSC on x86-64, the generic timer's
/// virtual count on arm64. A number to take differences of, at
/// `cycle_counter_hz`.
#[inline]
pub fn cycle_counter() -> u64 {
    time::kernel_read_cycle_counter()
}

/// The cycle counter read once every instruction before it has finished, and
/// before any after it starts: what times a stretch of work exactly -- two of
/// these around it. The plain read may be taken before the work has run
/// (x86-64's RDTSC is not ordered), and costs less; this one is for
/// measuring, not for stamping.
#[inline]
pub fn cycle_counter_serialized() -> u64 {
    time::kernel_read_cycle_counter_serialized()
}

/// How fast the cycle counter counts, in Hz: the TSC as the kernel calibrated
/// it on x86-64, the generic timer on arm64. None while it is not known.
pub fn cycle_counter_hz() -> Option<u64> {
    match time::kernel_cycle_counter_hz() {
        0 => None,
        hz => Some(hz),
    }
}

/// Spin for `us` microseconds, by the boot clock, without sleeping: what a
/// driver's bring-up waits with -- a register to settle, a reset to finish
/// -- some of it before there is a task to sleep in. Bounded besides by a
/// count of spins, so that a clock that stood still costs a short wait, not
/// a boot that never goes on: each spin reads the clock, which takes far
/// longer than the fraction of a microsecond the count allows it.
pub fn delay_us(us: u64) {
    const SPINS_PER_US: u64 = 200;
    const SPINS_MIN: u64 = 1000;

    let want = us.saturating_mul(NS_PER_US);
    let start = boot_time_ns();
    let mut left = us.saturating_mul(SPINS_PER_US).max(SPINS_MIN);
    while boot_time_ns().wrapping_sub(start) < want && left != 0 {
        core::hint::spin_loop();
        left -= 1;
    }
}

/// Busy-poll `condition` for up to `iterations` iterations without sleeping.
///
/// Intended for early-boot or IRQ-disabled contexts where sleeping is not
/// possible.  Each iteration is roughly one function call + some work done
/// by the caller's closure.
///
/// Returns `true` if `condition` returned `true` before iterations ran out,
/// `false` on timeout.
pub fn poll_until_busy<F: FnMut() -> bool>(iterations: usize, mut condition: F) -> bool {
    for _ in 0..iterations {
        if condition() {
            return true;
        }
    }
    false
}

/// Sleep-poll `condition` until it returns `true` or `timeout` elapses.
///
/// Sleeps `interval` between polls so the calling task yields the CPU.
/// Use this in task context where sleeping is safe.
///
/// Returns `true` if `condition` returned `true` within the timeout,
/// `false` otherwise.
pub fn poll_until<F: FnMut() -> bool>(timeout: Duration, interval: Duration, mut condition: F) -> bool {
    let deadline = boot_time().saturating_add(timeout);
    loop {
        if condition() {
            return true;
        }
        if boot_time() >= deadline {
            return false;
        }
        crate::task::sleep(interval);
    }
}
