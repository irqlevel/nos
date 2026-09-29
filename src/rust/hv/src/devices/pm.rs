//! ACPI's fixed hardware, the registers a guest's OS drives itself: the PM1
//! event block -- status and enable -- the PM1 control register and the
//! power management timer, all on I/O ports; and the SCI, the interrupt the
//! event block raises, which is the 8259's IRQ 9. (ACPI 6.5, 4.8, "ACPI
//! Hardware Features"; the tables that tell a guest where these are, and
//! what they do, are `crate::acpi`'s.)
//!
//! What a guest does with them: its ACPI driver enables the events it
//! handles -- the power button's among them -- and, when one is set, takes
//! the SCI and clears the event's status by writing it back; its clock code
//! reads the timer; and its `poweroff` writes S5's sleep type into the
//! control register with the sleep enable bit, which is where it stops, the
//! machine off. The timer counts the host's time from when the guest was
//! made, at the rate the specification fixes, so it and the TSC -- the
//! host's own, which the host's clock is kept by -- agree.
//!
//! The machine is always in ACPI mode: SCI_EN reads set, and the FADT names
//! no SMI command port to switch modes with -- there is no firmware to hand
//! the events to. And it has no GPE block: nothing here raises a
//! general-purpose event.

use kcore::consts::NS_PER_SEC;

/// Where the registers are: the event block (status, then enable), the
/// control register, and the timer -- a chipset's PMBASE block, laid out as
/// an ICH lays it out, at 0x600.
pub const PM1_EVT: u16 = 0x600;
pub const PM1_EVT_LEN: u8 = 4;
pub const PM1_CNT: u16 = 0x604;
pub const PM1_CNT_LEN: u8 = 2;
pub const PM_TMR: u16 = 0x608;
pub const PM_TMR_LEN: u8 = 4;
/// The ports the block spans, the two between the control register and the
/// timer included: they float.
const FIRST: u16 = PM1_EVT;
const END: u16 = PM_TMR + PM_TMR_LEN as u16;
/// The status register's two bytes, then the enable register's.
const PM1_STS: u16 = PM1_EVT;
const PM1_EN: u16 = PM1_EVT + 2;

/// The SCI: IRQ 9, level-triggered, as a PC's is.
pub const SCI_IRQ: u8 = 9;

/// The sleep type the DSDT's `\_S5` names: what a guest writes to SLP_TYP to
/// be turned off. The only sleep state the machine offers.
pub const SLP_TYP_S5: u8 = 5;

/// The timer's rate, which the specification fixes: 3.579545 MHz.
pub const TIMER_HZ: u64 = 3_579_545;
/// Its width: 24 bits, the FADT's TMR_VAL_EXT being clear.
const TIMER_BITS: u32 = 24;
const TIMER_MASK: u64 = (1 << TIMER_BITS) - 1;

/* PM1 status and enable bits (ACPI 6.5, tables 4.16 and 4.17): each event's
 * status and enable are the same bit of the two registers. */
const TMR: u16 = 1 << 0;
const GBL: u16 = 1 << 5;
const PWRBTN: u16 = 1 << 8;
const SLPBTN: u16 = 1 << 9;
const RTC: u16 = 1 << 10;
const PCIEXP_WAKE: u16 = 1 << 14;
/// The enable bits a guest may set: every fixed event's, whether or not the
/// machine ever raises it -- an OS reads an enable back to learn whether the
/// hardware is there, and one that did not stick would be reported as
/// missing hardware (ACPICA's "Could not enable ... event").
const ENABLE_WRITABLE: u16 = TMR | GBL | PWRBTN | SLPBTN | RTC | PCIEXP_WAKE;

/* PM1 control bits (table 4.18). */
const SCI_EN: u16 = 1 << 0;
const BM_RLD: u16 = 1 << 1;
const SLP_TYP_SHIFT: u32 = 10;
const SLP_TYP_MASK: u16 = 0x7;
const SLP_EN: u16 = 1 << 13;
/// What of the control register reads back as written: BM_RLD, and the
/// sleep type. SCI_EN reads set whatever is written; GBL_RLS, which would
/// hand the global lock to firmware that never takes it, and SLP_EN are
/// write-only, and read as zero.
const CONTROL_KEPT: u16 = BM_RLD | (SLP_TYP_MASK << SLP_TYP_SHIFT);

/// The registers, and the timer's zero.
pub struct Pm {
    /// PM1 status as latched -- the power button's, and any a guest set; the
    /// timer's carry is not kept here but worked out from the time
    /// (`timer_status`).
    status: u16,
    enable: u16,
    /// BM_RLD and SLP_TYP as last written.
    control: u16,
    /// The host's time the timer counts from.
    zero_ns: u64,
    /// How many times the timer's top bit had flipped when its status was
    /// last cleared: TMR_STS is set once the count moves past it.
    carries_seen: u64,
    /// Times the SCI was delivered -- by the 8259 or the IO-APIC, the run
    /// loop counting -- and the power button pressed, and whether
    /// the guest's OS ever wrote the event enables -- took the ACPI it was
    /// given, which starts by disabling every event -- for a report.
    pub scis: u64,
    pub presses: u32,
    pub used: bool,
    /// The last sleep type written with the sleep enable bit that was not
    /// S5's: a state the DSDT does not offer, and nothing was done -- the
    /// guest waits for a wake that never comes, as on a chipset without the
    /// state. For a report: a guest should never ask.
    pub other_sleep: Option<u8>,
}

impl Pm {
    /// The registers at power-on, the timer at zero at `now` (host
    /// nanoseconds since boot).
    pub fn new(now: u64) -> Pm {
        Pm {
            status: 0,
            enable: 0,
            control: 0,
            zero_ns: now,
            carries_seen: 0,
            scis: 0,
            presses: 0,
            used: false,
            other_sleep: None,
        }
    }

    pub fn owns(port: u16) -> bool {
        (FIRST..END).contains(&port)
    }

    /// The timer's ticks since its zero, not yet cut to its width.
    fn ticks(&self, now: u64) -> u64 {
        let ns = now.saturating_sub(self.zero_ns);
        /* In 128 bits: ns times the rate is past 64 within two months. */
        (u128::from(ns) * u128::from(TIMER_HZ) / u128::from(NS_PER_SEC)) as u64
    }

    /// Times the timer's top bit has flipped by `now`.
    fn carries(&self, now: u64) -> u64 {
        self.ticks(now) >> (TIMER_BITS - 1)
    }

    /// TMR_STS: set when the timer's top bit has flipped since the status
    /// was last cleared, `ticks` into its count.
    fn timer_status(&self, ticks: u64) -> u16 {
        if ticks >> (TIMER_BITS - 1) != self.carries_seen { TMR } else { 0 }
    }

    /// The SCI's level: an event whose status and enable are both set.
    pub fn sci(&self, now: u64) -> bool {
        let mut pending = self.status & self.enable;
        /* The timer's carry, worked out only when its event is enabled:
         * nothing else asks for it on every round. */
        if self.enable & TMR != 0 {
            pending |= self.timer_status(self.ticks(now));
        }
        pending != 0
    }

    /// The power button pressed: its status set, as the chipset latches it
    /// whatever the enable. Whether the guest will hear of it -- its OS has
    /// enabled the event, and so takes an SCI for it.
    pub fn press_power_button(&mut self) -> bool {
        self.presses = self.presses.saturating_add(1);
        self.status |= PWRBTN;
        self.enable & PWRBTN != 0
    }

    /// A read of `size` bytes (1, 2 or 4) from `port`, at `now`: the bytes
    /// of whichever registers they fall in, and all ones past the block. A
    /// wide read of the timer is one reading of it.
    pub fn read(&self, port: u16, size: u8, now: u64) -> u32 {
        let ticks = self.ticks(now);
        let status = self.status | self.timer_status(ticks);
        let timer = (ticks & TIMER_MASK) as u32;
        let mut v = 0u32;
        for i in 0..u16::from(size.min(4)) {
            v |= u32::from(self.byte(port.wrapping_add(i), status, timer)) << (8 * i);
        }
        v
    }

    /// The byte at `port`, the status and the timer being `status` and
    /// `timer`.
    fn byte(&self, port: u16, status: u16, timer: u32) -> u8 {
        match port {
            p if (PM1_STS..PM1_STS + 2).contains(&p) => status.to_le_bytes()[usize::from(p - PM1_STS)],
            p if (PM1_EN..PM1_EN + 2).contains(&p) => self.enable.to_le_bytes()[usize::from(p - PM1_EN)],
            p if (PM1_CNT..PM1_CNT + 2).contains(&p) => {
                (self.control | SCI_EN).to_le_bytes()[usize::from(p - PM1_CNT)]
            }
            p if (PM_TMR..END).contains(&p) => timer.to_le_bytes()[usize::from(p - PM_TMR)],
            _ => 0xFF,
        }
    }

    /// A write of `size` bytes (1, 2 or 4) from `port`, at `now`, byte by
    /// byte into whichever registers they fall in: status bits cleared where
    /// ones are written, enables and the control register's kept bits set as
    /// written, the timer and the gap left alone. Whether it turned the
    /// machine off: S5's sleep type with the sleep enable bit.
    pub fn write(&mut self, port: u16, size: u8, value: u32, now: u64) -> bool {
        let mut sleep = None;
        for i in 0..u16::from(size.min(4)) {
            let p = port.wrapping_add(i);
            let byte = (value >> (8 * i)) as u8;
            if (PM1_STS..PM1_STS + 2).contains(&p) {
                let bits = u16::from(byte) << (8 * (p - PM1_STS));
                self.status &= !bits;
                if bits & TMR != 0 {
                    self.carries_seen = self.carries(now);
                }
            } else if (PM1_EN..PM1_EN + 2).contains(&p) {
                let shift = 8 * (p - PM1_EN);
                let mask = 0xFFu16 << shift;
                self.enable = (self.enable & !mask) | ((u16::from(byte) << shift) & mask & ENABLE_WRITABLE);
                self.used = true;
            } else if (PM1_CNT..PM1_CNT + 2).contains(&p) {
                let shift = 8 * (p - PM1_CNT);
                let bits = u16::from(byte) << shift;
                let mask = 0xFFu16 << shift;
                self.control = (self.control & !mask) | (bits & mask & CONTROL_KEPT);
                if bits & SLP_EN != 0 {
                    sleep = Some(((self.control >> SLP_TYP_SHIFT) & SLP_TYP_MASK) as u8);
                }
            }
        }
        match sleep {
            Some(t) if t == SLP_TYP_S5 => true,
            Some(t) => {
                self.other_sleep = Some(t);
                false
            }
            None => false,
        }
    }
}
