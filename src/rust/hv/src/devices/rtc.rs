//! A minimal MC146818 real-time clock and its CMOS RAM, ports 0x70 (index)
//! and 0x71 (data).
//!
//! A guest reads the RTC to learn the wall-clock time at boot, and it reads
//! status register A's "update in progress" bit first and waits for it to
//! clear. With no RTC the data port floats to 0xFF, the update bit is always
//! set, and the guest waits there for good. What is emulated is enough to
//! get it past that: an update bit that is always clear, a valid-RAM bit
//! that is set, and a plausible fixed time (the emulator has no wall clock
//! of its own to hand the guest, and a Linux guest does not need a true one
//! to boot).

/// Ports.
pub const INDEX: u16 = 0x70;
pub const DATA: u16 = 0x71;

/* CMOS register numbers. */
const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY_OF_MONTH: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0A;
const STATUS_B: u8 = 0x0B;
const STATUS_C: u8 = 0x0C;
const STATUS_D: u8 = 0x0D;

/// Status A: the update-in-progress bit, which is kept clear here.
const STATUS_A_UIP: u8 = 1 << 7;
/// Status B: 24-hour mode and BCD (bit 1 set = 24h, bit 2 clear = BCD).
const STATUS_B_24H: u8 = 1 << 1;
/// Status D: the valid-RAM-and-time bit, which says the battery is good.
const STATUS_D_VRT: u8 = 1 << 7;

/// The high bit of the index port disables the NMI; the register is the low
/// seven bits.
const INDEX_MASK: u8 = 0x7F;

/// A fixed date to hand the guest: 2026-01-01 00:00:00, in BCD.
const FIXED: [(u8, u8); 6] = [
    (SECONDS, 0x00),
    (MINUTES, 0x00),
    (HOURS, 0x00),
    (DAY_OF_MONTH, 0x01),
    (MONTH, 0x01),
    (YEAR, 0x26),
];

/// The RTC/CMOS.
pub struct Rtc {
    index: u8,
    /// General-purpose CMOS RAM the guest may write and read back.
    ram: [u8; 128],
}

impl Rtc {
    pub fn new() -> Self {
        Self { index: 0, ram: [0; 128] }
    }

    pub fn owns(port: u16) -> bool {
        port == INDEX || port == DATA
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
            reg => {
                if let Some((_, bcd)) = FIXED.iter().find(|(r, _)| *r == reg) {
                    *bcd
                } else {
                    self.ram[(reg & 0x7F) as usize]
                }
            }
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
            reg if FIXED.iter().any(|(r, _)| *r == reg) => {}
            reg => self.ram[(reg & 0x7F) as usize] = value,
        }
    }
}
