//! An IO-APIC: what routes the interrupts of devices on pins -- the ISA
//! ones, the timer, the serial port, the SCI, the PCI functions' INTx lines
//! -- to the CPUs' local APICs, where the 8259 takes them to the first CPU
//! alone.
//!
//! It is 24 pins and a redirection entry each, reached through a page of
//! MMIO at 0xFEC00000 -- a select register, and a window onto the register it
//! selects -- and version 0x20's third register, EOI (the 82093AA's
//! datasheet, and the ICH's IOxAPIC for version 0x20). An entry says what an
//! interrupt on its pin is: a vector, a delivery mode, a destination -- an
//! APIC ID, or a logical set -- whether it is masked, and whether the pin is
//! edge- or level-triggered. The ID, the version and the arbitration ID are
//! the rest.
//!
//! A device drives its pin: [`IoApic::set_line`] says whether it asserts it
//! now, and [`IoApic::edge`] that it pulsed it. An edge-triggered pin's
//! interrupt is sent as the pin rises; a level-triggered one's while the pin
//! is asserted and the last one has been ended -- its remote IRR set when it
//! is sent, and cleared by the EOI that ends it, which the local APIC that
//! took it as a level broadcasts ([`IoApic::eoi`]; or the guest writes the
//! EOI register), and a pin asserted still is sent again. Each of these
//! says which pins have an interrupt to send now, and [`IoApic::send`] makes
//! each one's message: what the APIC bus carries, as a device's MSI does,
//! for the caller to deliver to the CPUs it names.
//!
//! The polarity bit is kept and not applied: a device here says when it
//! asserts its line, which is what the entry's polarity says of the wire.
//! And nothing is sent in a delivery mode no local APIC here takes by
//! message -- ExtINT, which is the 8259's own path to the first CPU, SMI, the
//! reserved ones -- which is counted instead.
//!
//! Plain data, as every device here is.

use crate::lapic::{self, Delivery, Ipi};

/// Where a PC's firmware leaves it, and what the tables say.
pub const BASE: u64 = 0xFEC0_0000;
/// The page it answers in.
pub const PAGE_SIZE: u64 = 4096;
/// Its pins.
pub const PINS: usize = 24;

/// The pin an ISA interrupt is wired to, as on a PC: the one of its own
/// number -- but the timer's, IRQ 0, which is on pin 2, pin 0 being the
/// 8259's output.
pub fn isa_pin(irq: u8) -> usize {
    if irq == 0 { TIMER_PIN } else { usize::from(irq) }
}
/// The timer's pin.
pub const TIMER_PIN: usize = 2;

/* Its page: the select register, the window, the EOI register. */
const IOREGSEL: u32 = 0x00;
const IOWIN: u32 = 0x10;
const IOEOI: u32 = 0x40;
/// The window's register, 32 bits: what an access of it is.
const WINDOW_BYTES: u8 = 4;
/// The select register holds an 8-bit index.
const SELECT_MASK: u32 = 0xFF;

/* The registers the window shows. */
const REG_ID: u32 = 0x00;
const REG_VERSION: u32 = 0x01;
const REG_ARB: u32 = 0x02;
/// The first redirection entry's low half: each entry is two registers.
const REG_RTE: u32 = 0x10;

/// The ID register: the ID in bits 31:24, the rest reserved.
const ID_SHIFT: u32 = 24;
const ID_MASK: u32 = 0xFF << ID_SHIFT;
/// Version 0x20, which has the EOI register, and its highest entry's index
/// in bits 23:16.
const VERSION: u32 = 0x20 | ((PINS as u32 - 1) << 16);

/* A redirection entry's fields. */
const RTE_VECTOR: u64 = 0xFF;
const RTE_DELIVERY_SHIFT: u32 = 8;
const RTE_DELIVERY_MASK: u64 = 0x7;
const RTE_LOGICAL: u64 = 1 << 11;
const RTE_POLARITY_LOW: u64 = 1 << 13;
const RTE_REMOTE_IRR: u64 = 1 << 14;
const RTE_LEVEL: u64 = 1 << 15;
const RTE_MASKED: u64 = 1 << 16;
const RTE_DEST_SHIFT: u32 = 56;
/// What a write sets: all but the delivery status -- always idle, a message
/// going out as it is made -- the remote IRR, which is the IO-APIC's to
/// report, and the reserved bits 55:17.
const RTE_WRITABLE: u64 = RTE_VECTOR
    | (RTE_DELIVERY_MASK << RTE_DELIVERY_SHIFT)
    | RTE_LOGICAL
    | RTE_POLARITY_LOW
    | RTE_LEVEL
    | RTE_MASKED
    | (0xFF << RTE_DEST_SHIFT);
/// Out of reset every entry is masked.
const RTE_RESET: u64 = RTE_MASKED;

/* Delivery modes, as an entry has them. */
const DELIVERY_FIXED: u64 = 0;
const DELIVERY_LOWEST: u64 = 1;
const DELIVERY_NMI: u64 = 4;
const DELIVERY_INIT: u64 = 5;

/// Pins, a bit each: those with an interrupt to send now, which the caller
/// sends one at a time ([`IoApic::send`]).
pub type Pins = u32;

/// An interrupt sent: the message, and whether the local APIC takes it as a
/// level-triggered one -- its EOI broadcast back here.
#[derive(Clone, Copy, Debug)]
pub struct Message {
    pub ipi: Ipi,
    pub level: bool,
}

/// What it has done, for a report.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Interrupts sent, and those of them level-triggered.
    pub sent: u64,
    pub level: u64,
    /// Level-triggered interrupts ended, by EOI.
    pub eois: u64,
    /// Interrupts of a delivery mode no message here has, not sent.
    pub dropped: u64,
}

/// One IO-APIC.
pub struct IoApic {
    /// The ID register, as written.
    id: u32,
    select: u32,
    rte: [u64; PINS],
    /// Each pin as its device drives it: asserted or not, a bit a pin.
    lines: u32,
    /// Level-triggered interrupts sent and not yet ended, a bit a pin: the
    /// entries' remote IRR.
    remote_irr: u32,
    pub stats: Stats,
}

impl IoApic {
    /// One whose ID is `id`, as the tables that describe it say, every entry
    /// masked.
    pub fn new(id: u8) -> IoApic {
        IoApic {
            id: u32::from(id) << ID_SHIFT,
            select: 0,
            rte: [RTE_RESET; PINS],
            lines: 0,
            remote_irr: 0,
            stats: Stats::default(),
        }
    }

    /// Whether `gpa` is in its page.
    pub fn owns(gpa: u64) -> bool {
        gpa & !(PAGE_SIZE - 1) == BASE
    }

    /// A read of `size` bytes at `offset` in its page: the select register;
    /// or, 32 bits at once, the register the window shows. Anything else
    /// reads zero.
    pub fn mmio_read(&self, offset: u32, size: u8) -> u32 {
        match offset {
            IOREGSEL => self.select,
            IOWIN if size == WINDOW_BYTES => self.read_register(),
            _ => 0,
        }
    }

    /// A write of `size` bytes of `value` at `offset` in its page: the select
    /// register set; the register the window shows written, 32 bits at once;
    /// or the EOI register -- the level-triggered interrupts of the vector
    /// written ended. The pins that has an interrupt to send.
    pub fn mmio_write(&mut self, offset: u32, size: u8, value: u32) -> Pins {
        match offset {
            IOREGSEL => {
                self.select = value & SELECT_MASK;
                0
            }
            IOWIN if size == WINDOW_BYTES => self.write_register(value),
            IOEOI => self.eoi(value as u8),
            _ => 0,
        }
    }

    /// The register the select register names, as the window reads it.
    fn read_register(&self) -> u32 {
        match self.select {
            REG_ID => self.id,
            REG_VERSION => VERSION,
            /* The arbitration ID, which is the ID's. */
            REG_ARB => self.id,
            r => match rte_index(r) {
                Some((pin, high)) => {
                    let irr = if self.remote_irr & (1 << pin) != 0 { RTE_REMOTE_IRR } else { 0 };
                    let entry = self.rte[pin] | irr;
                    if high { (entry >> 32) as u32 } else { entry as u32 }
                }
                None => 0,
            },
        }
    }

    /// A write of the register the select register names. An entry turned
    /// edge-triggered has no interrupt waiting to be ended -- which is how a
    /// kernel without the EOI register clears a remote IRR a lost EOI left:
    /// it turns the entry to edge and back. A level-triggered one unmasked,
    /// or pointed elsewhere, with its pin asserted and nothing of it waiting
    /// to be ended, sends -- as it would have when the line went up, had it
    /// been unmasked then.
    fn write_register(&mut self, value: u32) -> Pins {
        match self.select {
            REG_ID => {
                self.id = value & ID_MASK;
                0
            }
            r => {
                let Some((pin, high)) = rte_index(r) else { return 0 };
                let old = self.rte[pin];
                let new = if high {
                    (old & u64::from(u32::MAX)) | (u64::from(value) << 32)
                } else {
                    (old & !u64::from(u32::MAX)) | u64::from(value)
                };
                self.rte[pin] = new & RTE_WRITABLE;
                if new & RTE_LEVEL == 0 {
                    self.remote_irr &= !(1 << pin);
                    return 0;
                }
                self.level_pending(pin)
            }
        }
    }

    /// Pin `pin`'s bit when it is level-triggered, unmasked and asserted,
    /// with nothing of it waiting to be ended: an interrupt to send.
    fn level_pending(&self, pin: usize) -> Pins {
        let bit = 1 << pin;
        let e = self.rte[pin];
        let ready = e & RTE_LEVEL != 0 && e & RTE_MASKED == 0 && self.lines & bit != 0 && self.remote_irr & bit == 0;
        if ready { bit } else { 0 }
    }

    /// Pin `pin` driven by its device, `asserted` or not: its bit when that
    /// sends an interrupt -- an edge-triggered pin rising, unmasked; a
    /// level-triggered one asserted, unmasked, its last interrupt ended.
    pub fn set_line(&mut self, pin: usize, asserted: bool) -> Pins {
        let Some(&e) = self.rte.get(pin) else { return 0 };
        let bit = 1 << pin;
        let was = self.lines & bit != 0;
        if asserted {
            self.lines |= bit;
        } else {
            self.lines &= !bit;
        }
        if e & RTE_LEVEL != 0 {
            return self.level_pending(pin);
        }
        if asserted && !was && e & RTE_MASKED == 0 { bit } else { 0 }
    }

    /// Pin `pin` pulsed -- down and up again, whatever it was before: an edge,
    /// which is what a timer's tick is, and a byte the serial port sent.
    pub fn edge(&mut self, pin: usize) -> Pins {
        self.set_line(pin, false);
        self.set_line(pin, true)
    }

    /// The end of a level-triggered interrupt of `vector`, broadcast by the
    /// local APIC that took it, or written to the EOI register: each entry
    /// of that vector with an interrupt waiting to be ended is ended, and
    /// those whose pins are asserted still are the pins returned, to be sent
    /// again.
    pub fn eoi(&mut self, vector: u8) -> Pins {
        let mut again = 0;
        for pin in 0..PINS {
            let bit = 1 << pin;
            if self.remote_irr & bit != 0 && self.rte[pin] & RTE_VECTOR == u64::from(vector) {
                self.remote_irr &= !bit;
                self.stats.eois += 1;
                again |= self.level_pending(pin);
            }
        }
        again
    }

    /// The message pin `pin`'s interrupt is, sent now, for the caller to
    /// deliver: a level-triggered one waits to be ended from here on. None
    /// for a pin that is none, or a delivery mode no message here has.
    pub fn send(&mut self, pin: usize) -> Option<Message> {
        let &e = self.rte.get(pin)?;
        let delivery = match (e >> RTE_DELIVERY_SHIFT) & RTE_DELIVERY_MASK {
            DELIVERY_FIXED => Delivery::Fixed,
            DELIVERY_LOWEST => Delivery::LowestPriority,
            DELIVERY_NMI => Delivery::Nmi,
            DELIVERY_INIT => Delivery::Init,
            _ => {
                self.stats.dropped += 1;
                return None;
            }
        };
        /* Only a fixed interrupt is ended by an EOI: an NMI or an INIT is
         * never in service anywhere. */
        let level = e & RTE_LEVEL != 0 && matches!(delivery, Delivery::Fixed | Delivery::LowestPriority);
        if level {
            self.remote_irr |= 1 << pin;
            self.stats.level += 1;
        }
        self.stats.sent += 1;
        let ipi = lapic::message((e >> RTE_DEST_SHIFT) as u8, e & RTE_LOGICAL != 0, delivery, e as u8);
        Some(Message { ipi, level })
    }

    /// The message pin `pin` would send, while its entry is unmasked and
    /// fixed: what a caller with an edge to send asks, to tell whether the
    /// last one's interrupt has been taken yet.
    pub fn route(&self, pin: usize) -> Option<Ipi> {
        let &e = self.rte.get(pin)?;
        if e & RTE_MASKED != 0 {
            return None;
        }
        let delivery = match (e >> RTE_DELIVERY_SHIFT) & RTE_DELIVERY_MASK {
            DELIVERY_FIXED => Delivery::Fixed,
            DELIVERY_LOWEST => Delivery::LowestPriority,
            _ => return None,
        };
        Some(lapic::message((e >> RTE_DEST_SHIFT) as u8, e & RTE_LOGICAL != 0, delivery, e as u8))
    }
}

/// The entry a register of the window is half of, and whether its high half.
fn rte_index(reg: u32) -> Option<(usize, bool)> {
    let i = reg.checked_sub(REG_RTE)? as usize;
    let pin = i / 2;
    (pin < PINS).then_some((pin, i % 2 == 1))
}
