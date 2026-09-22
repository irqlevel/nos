//! An emulated 16550A UART, the guest's serial console.
//!
//! A guest reaches a serial port by `in` and `out` on eight consecutive
//! ports, and every one of those is an intercept (`IOIO_PROT`) that stops
//! the guest with the port, the direction and the byte -- so a serial port
//! is emulated with no instruction decoder at all, which is why Linux is
//! brought up on one before anything else. This is the device the guest's
//! `console=ttyS0` and `earlyprintk=serial` write to.
//!
//! What it models is a UART whose transmitter is always ready and whose
//! receiver, for now, never has anything: the guest's output is the point,
//! and its input comes later with a console that can type back. The bits
//! that matter to Linux's `8250` driver on the way up are the ones that say
//! "you may send" (LSR.THRE and LSR.TEMT), the divisor latch (so a probe
//! that sets a baud rate reads back what it wrote), the scratch register
//! (which the driver writes and reads to tell a UART is there at all), and
//! the loopback path of the modem-control register (likewise a presence
//! test).

use alloc::string::String;

/// The eight registers, at offsets 0..8 from the port base.
pub const REGISTERS: u16 = 8;

/* Reads and writes, by offset -- some share an offset and are told apart by
 * the divisor-latch bit of the line-control register. */
const RBR_THR_DLL: u16 = 0; // read: receive buffer / write: transmit / DLAB: divisor low
const IER_DLM: u16 = 1; // interrupt enable / DLAB: divisor high
const IIR_FCR: u16 = 2; // read: interrupt identification / write: FIFO control
const LCR: u16 = 3; // line control
const MCR: u16 = 4; // modem control
const LSR: u16 = 5; // line status (read only)
const MSR: u16 = 6; // modem status (read only)
const SCR: u16 = 7; // scratch

/* Line-control. */
const LCR_DLAB: u8 = 1 << 7;

/* Line-status: what a sender polls. THRE, the holding register is empty;
 * TEMT, the shift register is too. Both always set here -- the transmitter
 * is never busy. DR, data ready, is set when there is a byte to read. */
const LSR_DR: u8 = 1 << 0;
const LSR_THRE: u8 = 1 << 5;
const LSR_TEMT: u8 = 1 << 6;

/* Modem-control. LOOP wires the modem-control outputs back to the
 * modem-status inputs, which a driver uses to tell a UART is present. */
const MCR_DTR: u8 = 1 << 0;
const MCR_RTS: u8 = 1 << 1;
const MCR_OUT1: u8 = 1 << 2;
const MCR_OUT2: u8 = 1 << 3;
const MCR_LOOP: u8 = 1 << 4;

/* Modem-status, the inputs LOOP feeds from the outputs above. */
const MSR_CTS: u8 = 1 << 4;
const MSR_DSR: u8 = 1 << 5;
const MSR_RI: u8 = 1 << 6;
const MSR_DCD: u8 = 1 << 7;

/* Interrupt-identification: bit 0 set means no interrupt is pending, which
 * is always so here -- this UART raises none yet. */
const IIR_NONE: u8 = 0x01;

/// The most of a guest's console output kept for a report, so a guest that
/// prints without end is not a reason for the host to run out of memory.
const OUTPUT_MAX: usize = 16 * 1024;

/// One 16550A.
pub struct Uart {
    dll: u8,
    dlm: u8,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    /// Everything the guest has sent, up to `OUTPUT_MAX`.
    output: String,
    /// Whether output was dropped for the cap.
    truncated: bool,
    /// How many bytes the guest has written, cap or no cap.
    written: u64,
    /// A byte waiting to be read back, if the last thing written went round
    /// the loopback.
    loopback: Option<u8>,
}

impl Uart {
    pub fn new() -> Self {
        Self {
            dll: 0,
            dlm: 0,
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            output: String::new(),
            truncated: false,
            written: 0,
            loopback: None,
        }
    }

    /// Whether `port` is one of this UART's, given its base.
    pub fn owns(base: u16, port: u16) -> bool {
        port >= base && port < base + REGISTERS
    }

    fn dlab(&self) -> bool {
        self.lcr & LCR_DLAB != 0
    }

    /// The guest read register `offset`.
    pub fn read(&mut self, offset: u16) -> u8 {
        match offset {
            RBR_THR_DLL if self.dlab() => self.dll,
            RBR_THR_DLL => {
                /* Data register: whatever loopback left, else nothing. */
                self.loopback.take().unwrap_or(0)
            }
            IER_DLM if self.dlab() => self.dlm,
            IER_DLM => self.ier,
            IIR_FCR => IIR_NONE,
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => {
                let mut lsr = LSR_THRE | LSR_TEMT;
                if self.loopback.is_some() {
                    lsr |= LSR_DR;
                }
                lsr
            }
            MSR => {
                if self.mcr & MCR_LOOP != 0 {
                    /* The outputs, wired to the inputs: RTS->CTS, DTR->DSR,
                     * OUT1->RI, OUT2->DCD. */
                    let mut msr = 0;
                    if self.mcr & MCR_RTS != 0 {
                        msr |= MSR_CTS;
                    }
                    if self.mcr & MCR_DTR != 0 {
                        msr |= MSR_DSR;
                    }
                    if self.mcr & MCR_OUT1 != 0 {
                        msr |= MSR_RI;
                    }
                    if self.mcr & MCR_OUT2 != 0 {
                        msr |= MSR_DCD;
                    }
                    msr
                } else {
                    /* A line that is up and clear to send. */
                    MSR_CTS | MSR_DSR | MSR_DCD
                }
            }
            SCR => self.scr,
            _ => 0,
        }
    }

    /// The guest wrote `value` to register `offset`. Returns the byte if
    /// this was a character for the console.
    pub fn write(&mut self, offset: u16, value: u8) -> Option<u8> {
        match offset {
            RBR_THR_DLL if self.dlab() => self.dll = value,
            RBR_THR_DLL => {
                if self.mcr & MCR_LOOP != 0 {
                    /* In loopback the byte comes back on the receiver
                     * instead of going out. */
                    self.loopback = Some(value);
                    return None;
                }
                self.written = self.written.saturating_add(1);
                if self.output.len() < OUTPUT_MAX {
                    self.output.push(value as char);
                } else {
                    self.truncated = true;
                }
                return Some(value);
            }
            IER_DLM if self.dlab() => self.dlm = value,
            IER_DLM => self.ier = value,
            IIR_FCR => {} // FIFO control: accepted and ignored
            LCR => self.lcr = value,
            MCR => self.mcr = value,
            SCR => self.scr = value,
            _ => {} // LSR and MSR are read-only
        }
        None
    }

    pub fn output(&self) -> &str {
        &self.output
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }
}
