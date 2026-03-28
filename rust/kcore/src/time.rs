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

    pub const fn from_secs(secs: u64) -> Self {
        Self {
            nanos: secs * NS_PER_SEC,
        }
    }

    pub const fn from_millis(ms: u64) -> Self {
        Self {
            nanos: ms * 1_000_000,
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
    unsafe { time::kernel_get_wall_time_secs() }
}
