//! A panic device: QEMU's `pvpanic`, one I/O port a guest's kernel writes
//! when it panics -- found through the ACPI device `QEMU0001` in the DSDT
//! (`acpi`), whose resource is the port, and driven by Linux's
//! `pvpanic-mmio` (`PVPANIC_MMIO`, which a distribution's kernel has).
//!
//! The driver reads the port first, for the events the device takes, and
//! from then on writes one when it happens: PANICKED as its panic notifier
//! runs -- its other CPUs stopped already -- or CRASH_LOADED in its place
//! when a crash kernel is loaded, which the guest is about to boot itself.
//! A panicked guest is stopped where it is (`Stop::Panicked`): left alone,
//! its kernel spins in `panic` on a host CPU of its own until `panic=`
//! reboots it, if it ever does, and the report could only say it halted.
//! One with a crash kernel is let go on, its event counted.

/// The port, where QEMU's ISA device has it.
pub const PORT: u16 = 0x505;
/// The events, a bit each: the guest panicked; it panicked with a crash
/// kernel loaded, which it boots.
pub const PANICKED: u8 = 1 << 0;
pub const CRASH_LOADED: u8 = 1 << 1;
/// What a read of the port says the device takes.
const OFFERED: u8 = PANICKED | CRASH_LOADED;

/// What a write to the port said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Panicked,
    CrashLoaded,
}

/// The device: what it has heard, for a report.
#[derive(Clone, Copy, Default)]
pub struct PvPanic {
    pub crash_loaded: u32,
}

impl PvPanic {
    pub fn owns(port: u16) -> bool {
        port == PORT
    }

    /// A read of the port: the events it takes.
    pub fn read(&self) -> u8 {
        OFFERED
    }

    /// A write of `value`: the guest's panic when it says so -- PANICKED
    /// over CRASH_LOADED, should it say both -- or its crash kernel's,
    /// counted; nothing for a bit the device does not take.
    pub fn write(&mut self, value: u8) -> Option<Event> {
        if value & PANICKED != 0 {
            return Some(Event::Panicked);
        }
        if value & CRASH_LOADED != 0 {
            self.crash_loaded = self.crash_loaded.saturating_add(1);
            return Some(Event::CrashLoaded);
        }
        None
    }
}
