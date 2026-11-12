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
//!
//! Beside the pair, the chipset's edge/level control registers (ELCR), a bit
//! a line, set for a level-triggered one: kept as written and read back,
//! which is all an OS does with them. A line here is raised when its device
//! has something, and a level-triggered device that still has it after the
//! interrupt is ended raises it again -- the SCI, whose level is looked at
//! again every round (`pm`) -- and lowers it, withdrawing a request not yet
//! taken, when it no longer has it (`lower`).

/* Ports. */
pub const MASTER_CMD: u16 = 0x20;
pub const MASTER_DATA: u16 = 0x21;
pub const SLAVE_CMD: u16 = 0xA0;
pub const SLAVE_DATA: u16 = 0xA1;
/// The edge/level control registers: IRQs 0-7, then 8-15.
pub const ELCR1: u16 = 0x4D0;
pub const ELCR2: u16 = 0x4D1;

/* Command bytes. */
const ICW1_INIT: u8 = 0x10;
const ICW1_ICW4: u8 = 0x01;
const OCW2_EOI: u8 = 0x20;
const OCW3: u8 = 0x08;
const OCW3_READ_ISR: u8 = 0x03;
const OCW3_READ_IRR: u8 = 0x02;

/// The IRQ the slave is cascaded onto the master at.
const CASCADE_IRQ: u8 = 2;

/// The ELCR bits a guest may set: every line's but the timer's, the
/// keyboard's, the cascade, the RTC's and the FPU's (0, 1, 2, 8 and 13),
/// which are edge-triggered on every PC and whose bits read as zero.
const ELCR_WRITABLE: u16 = !((1 << 0) | (1 << 1) | (1 << 2) | (1 << 8) | (1 << 13));
/// The ELCR as firmware leaves it: the SCI's line, IRQ 9, level-triggered --
/// what an OS with ACPI sets it to, and, finding it so, need not say it did.
/// The PCI devices' lines an OS sets when it enables them.
const ELCR_RESET: u16 = 1 << 9;

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

    /// The highest-priority line (0 the highest) that is requested -- in
    /// IRR, or among `lines`, which are up as well -- not masked, and not
    /// behind one already in service.
    fn highest(&self, lines: u8) -> Option<u8> {
        let requested = self.irr | lines;
        for irq in 0..8 {
            let bit = 1u8 << irq;
            if self.isr & bit != 0 {
                /* A line in service blocks it and everything below. */
                return None;
            }
            if requested & bit != 0 && self.imr & bit == 0 {
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

/// The cascaded pair, and the ELCR.
pub struct Pic {
    master: Chip,
    slave: Chip,
    elcr: u16,
}

impl Pic {
    pub fn new() -> Self {
        Self { master: Chip::new(), slave: Chip::new(), elcr: ELCR_RESET }
    }

    pub fn owns(port: u16) -> bool {
        matches!(port, MASTER_CMD | MASTER_DATA | SLAVE_CMD | SLAVE_DATA | ELCR1 | ELCR2)
    }

    /// Raise IRQ `irq` (0-15): mark it requested. 8-15 are the slave's;
    /// the master's IR2 is the slave's INT output, up exactly while the
    /// slave has a request it would deliver (`cascade`) -- not a bit latched
    /// here, which a request the slave masked held up, and every master line
    /// below it held off with it.
    pub fn raise(&mut self, irq: u8) {
        if irq < 8 {
            self.master.irr |= 1 << irq;
        } else if irq < 16 {
            self.slave.irr |= 1 << (irq - 8);
        }
    }

    /// The master's IR2 as the slave drives it: up while the slave has a
    /// request it would deliver -- unmasked, not behind one of its own in
    /// service.
    fn cascade(&self) -> u8 {
        if self.slave.highest(0).is_some() { 1 << CASCADE_IRQ } else { 0 }
    }

    /// Withdraw IRQ `irq`'s request, if it has one the CPU has not taken: a
    /// level-triggered line gone low before the interrupt was acknowledged,
    /// which an 8259 in level mode no longer requests. What is in service
    /// stays so until its end of interrupt.
    pub fn lower(&mut self, irq: u8) {
        match irq {
            0..=7 => self.master.irr &= !(1 << irq),
            8..=15 => self.slave.irr &= !(1 << (irq - 8)),
            _ => {}
        }
    }

    /// Whether IRQ `irq` is masked off: on its chip, or -- the slave's -- the
    /// cascade on the master. A guest that takes its interrupts from an
    /// IO-APIC masks every line here.
    pub fn masked(&self, irq: u8) -> bool {
        match irq {
            0..=7 => self.master.imr & (1 << irq) != 0,
            8..=15 => self.slave.imr & (1 << (irq - 8)) != 0 || self.master.imr & (1 << CASCADE_IRQ) != 0,
            _ => true,
        }
    }

    /// Whether IRQ `irq` is requested or in service: a new edge on it now
    /// would be the same request again, and lost.
    pub fn busy(&self, irq: u8) -> bool {
        let (chip, bit) = match irq {
            0..=7 => (&self.master, irq),
            8..=15 => (&self.slave, irq - 8),
            _ => return false,
        };
        (chip.irr | chip.isr) & (1 << bit) != 0
    }

    /// The vector of the highest-priority interrupt the guest could take
    /// now, and the IRQ it is, without acknowledging it. `None` when nothing
    /// is pending or everything is masked or blocked.
    pub fn pending(&self) -> Option<(u8, u8)> {
        let m = self.master.highest(self.cascade())?;
        if m == CASCADE_IRQ {
            /* The interrupt is really the slave's -- which has one it would
             * deliver, or IR2 would be down. */
            let s = self.slave.highest(0)?;
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
            /* The cascade in service on the master; its request is the
             * slave's to drive (`cascade`). */
            self.master.isr |= 1 << CASCADE_IRQ;
        }
    }

    pub fn read(&mut self, port: u16) -> u8 {
        match port {
            /* IRR's IR2 is the slave's INT output, as the line is wired */
            MASTER_CMD if !self.master.read_isr => self.master.read_command() | self.cascade(),
            MASTER_CMD => self.master.read_command(),
            MASTER_DATA => self.master.read_data(),
            SLAVE_CMD => self.slave.read_command(),
            SLAVE_DATA => self.slave.read_data(),
            ELCR1 => self.elcr as u8,
            ELCR2 => (self.elcr >> 8) as u8,
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
            ELCR1 => self.elcr = (self.elcr & 0xFF00) | (u16::from(value) & ELCR_WRITABLE & 0x00FF),
            ELCR2 => self.elcr = (self.elcr & 0x00FF) | ((u16::from(value) << 8) & ELCR_WRITABLE & 0xFF00),
            _ => {}
        }
    }
}
