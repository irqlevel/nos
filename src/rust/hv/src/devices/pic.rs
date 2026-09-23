//! A pair of cascaded Intel 8259 programmable interrupt controllers, the
//! master at ports 0x20/0x21 and the slave at 0xA0/0xA1.
//!
//! This is what a guest takes its interrupts from when it runs without a
//! local APIC -- above all the timer, IRQ0 off the PIT's channel 0. The run
//! loop raises an IRQ here, asks for the highest-priority one the guest is
//! ready to take, injects it, and clears it when the guest signals
//! end-of-interrupt.
//!
//! Only what a guest actually programs is modelled: the ICW1-4 init
//! sequence (to learn each chip's vector base), the interrupt mask (OCW1),
//! and end-of-interrupt (OCW2). The request and in-service registers track
//! which interrupts are pending and which are being serviced, so priority
//! and masking are right; the niceties -- special mask mode, rotating
//! priority, polled mode -- are not, because a Linux guest on a PIC does not
//! use them.

/* Ports. */
pub const MASTER_CMD: u16 = 0x20;
pub const MASTER_DATA: u16 = 0x21;
pub const SLAVE_CMD: u16 = 0xA0;
pub const SLAVE_DATA: u16 = 0xA1;

/* Command bytes. */
const ICW1_INIT: u8 = 0x10;
const ICW1_ICW4: u8 = 0x01;
const OCW2_EOI: u8 = 0x20;
const OCW3: u8 = 0x08;
const OCW3_READ_ISR: u8 = 0x03;
const OCW3_READ_IRR: u8 = 0x02;

/// The IRQ the slave is cascaded onto the master at.
const CASCADE_IRQ: u8 = 2;

/// One 8259.
#[derive(Clone, Copy)]
struct Chip {
    /// Interrupt request: a raised, not-yet-serviced line.
    irr: u8,
    /// In service: an interrupt injected and not yet ended.
    isr: u8,
    /// Interrupt mask: a set bit is a line the guest has masked off.
    imr: u8,
    /// The vector the chip's IRQ0 maps to (ICW2); its IRQn is base + n.
    vector_base: u8,
    /// How far through the ICW init sequence a write to the command port
    /// has taken this chip: 0 = done, else the next ICW expected.
    init_step: u8,
    expect_icw4: bool,
    /// What the next command-port read returns: the IRR or the ISR.
    read_isr: bool,
}

impl Chip {
    fn new() -> Self {
        Self {
            irr: 0,
            isr: 0,
            imr: 0xFF,
            vector_base: 0,
            init_step: 0,
            expect_icw4: false,
            read_isr: false,
        }
    }

    /// The highest-priority line (0 the highest) that is requested, not
    /// masked, and not behind one already in service.
    fn highest(&self) -> Option<u8> {
        for irq in 0..8 {
            let bit = 1u8 << irq;
            if self.isr & bit != 0 {
                /* A line in service blocks it and everything below. */
                return None;
            }
            if self.irr & bit != 0 && self.imr & bit == 0 {
                return Some(irq);
            }
        }
        None
    }

    fn command(&mut self, value: u8) {
        if value & ICW1_INIT != 0 {
            self.imr = 0;
            self.isr = 0;
            self.irr = 0;
            self.expect_icw4 = value & ICW1_ICW4 != 0;
            self.init_step = 2;
            return;
        }
        if value & OCW3 != 0 {
            /* OCW3: choose what the command port reads back. */
            if value & OCW3_READ_ISR == OCW3_READ_ISR {
                self.read_isr = true;
            } else if value & OCW3_READ_ISR == OCW3_READ_IRR {
                self.read_isr = false;
            }
            return;
        }
        if value & OCW2_EOI != 0 {
            /* End of interrupt: clear the highest in-service line (a
             * non-specific EOI), or the named one (specific). */
            let specific = value & 0x40 != 0;
            if specific {
                self.isr &= !(1u8 << (value & 0x7));
            } else {
                for irq in 0..8 {
                    let bit = 1u8 << irq;
                    if self.isr & bit != 0 {
                        self.isr &= !bit;
                        break;
                    }
                }
            }
        }
    }

    fn data(&mut self, value: u8) {
        match self.init_step {
            2 => {
                self.vector_base = value & 0xF8;
                self.init_step = 3;
            }
            3 => {
                /* ICW3: the cascade wiring, which we do not need to keep. */
                self.init_step = if self.expect_icw4 { 4 } else { 0 };
            }
            4 => self.init_step = 0,
            _ => self.imr = value, // OCW1: the mask
        }
    }

    fn read_data(&self) -> u8 {
        self.imr
    }

    fn read_command(&self) -> u8 {
        if self.read_isr {
            self.isr
        } else {
            self.irr
        }
    }
}

/// The cascaded pair.
pub struct Pic {
    master: Chip,
    slave: Chip,
}

impl Pic {
    pub fn new() -> Self {
        Self { master: Chip::new(), slave: Chip::new() }
    }

    pub fn owns(port: u16) -> bool {
        matches!(port, MASTER_CMD | MASTER_DATA | SLAVE_CMD | SLAVE_DATA)
    }

    /// Raise IRQ `irq` (0-15): mark it requested. 8-15 are the slave's, and
    /// also raise the cascade line on the master.
    pub fn raise(&mut self, irq: u8) {
        if irq < 8 {
            self.master.irr |= 1 << irq;
        } else if irq < 16 {
            self.slave.irr |= 1 << (irq - 8);
            self.master.irr |= 1 << CASCADE_IRQ;
        }
    }

    /// The vector of the highest-priority interrupt the guest could take
    /// now, and the IRQ it is, without acknowledging it. `None` when nothing
    /// is pending or everything is masked or blocked.
    pub fn pending(&self) -> Option<(u8, u8)> {
        let m = self.master.highest()?;
        if m == CASCADE_IRQ {
            /* The interrupt is really the slave's; find which. */
            let s = self.slave.highest()?;
            Some((8 + s, self.slave.vector_base + s))
        } else {
            Some((m, self.master.vector_base + m))
        }
    }

    /// Acknowledge IRQ `irq`: move it from requested to in-service, which is
    /// what the CPU's interrupt-acknowledge cycle does when the interrupt is
    /// taken. Called when the run loop injects the vector.
    pub fn acknowledge(&mut self, irq: u8) {
        if irq < 8 {
            self.master.isr |= 1 << irq;
            self.master.irr &= !(1 << irq);
        } else if irq < 16 {
            self.slave.isr |= 1 << (irq - 8);
            self.slave.irr &= !(1 << (irq - 8));
            /* The master's cascade line clears once the slave has no more
             * requests waiting -- and only then: it used to clear whatever
             * the slave still had, and with two devices on the slave the
             * second's interrupt waited for a third to raise the line. */
            self.master.isr |= 1 << CASCADE_IRQ;
            if self.slave.irr == 0 {
                self.master.irr &= !(1 << CASCADE_IRQ);
            }
        }
    }

    pub fn read(&mut self, port: u16) -> u8 {
        match port {
            MASTER_CMD => self.master.read_command(),
            MASTER_DATA => self.master.read_data(),
            SLAVE_CMD => self.slave.read_command(),
            SLAVE_DATA => self.slave.read_data(),
            _ => 0xFF,
        }
    }

    /// The master chip's request, in-service and mask registers, for a
    /// diagnostic on stuck interrupt delivery.
    pub fn master_state(&self) -> (u8, u8, u8) {
        (self.master.irr, self.master.isr, self.master.imr)
    }

    pub fn write(&mut self, port: u16, value: u8) {
        match port {
            MASTER_CMD => self.master.command(value),
            MASTER_DATA => self.master.data(value),
            SLAVE_CMD => self.slave.command(value),
            SLAVE_DATA => self.slave.data(value),
            _ => {}
        }
    }
}
