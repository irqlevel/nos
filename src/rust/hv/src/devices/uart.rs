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
//! receiver takes a byte at a time from whoever runs the guest -- what is
//! typed at its console. The bits
//! that matter to Linux's `8250` driver on the way up are the ones that say
//! "you may send" (LSR.THRE and LSR.TEMT), the divisor latch (so a probe
//! that sets a baud rate reads back what it wrote), the scratch register
//! (which the driver writes and reads to tell a UART is there at all), and
//! the loopback path of the modem-control register (likewise a presence
//! test).

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

/* Interrupt-enable bits (register 1). */
const IER_RX: u8 = 1 << 0;
const IER_THRE: u8 = 1 << 1;

/* Interrupt-identification (register 2): bit 0 set means no interrupt is
 * pending; otherwise bits 1-3 name the cause. */
const IIR_NONE: u8 = 0x01;
const IIR_THRE: u8 = 0x02;
const IIR_RX: u8 = 0x04;

const ESC: u8 = 0x1B;
const BACKSPACE: u8 = 0x08;
/// The answer to a cursor-position query, `ESC [ row ; col R`, is at most
/// this long: `ESC [ 24 ; 1000 R`.
const REPLY_MAX: usize = 12;
/// The row a query is told the cursor is on. Nothing here keeps rows, and a
/// line editor uses only the column.
const REPLY_ROW: u16 = 24;
/// The furthest column counted: a longer line is told it is here.
const COL_MAX: u16 = 999;
/// Where the tab stops are.
const TAB: u16 = 8;

/// Where the guest's output is in an ANSI escape sequence: what tells the
/// cursor-position query apart from text, and text from the bytes of a
/// sequence, which move no cursor.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Esc {
    /// Text.
    Text,
    /// ESC.
    Start,
    /// ESC [, no parameter yet.
    Csi,
    /// ESC [ 6: the query, if `n` is next.
    Csi6,
    /// ESC [ and anything else, to its final byte.
    CsiOther,
}

/// The answer to the last query, in room of its own: a guest that asks and
/// never reads costs the host nothing more than one that asks once.
struct Reply {
    buf: [u8; REPLY_MAX],
    len: usize,
    /// How much of it the guest has read.
    pos: usize,
}

impl core::fmt::Write for Reply {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.len + s.len();
        let dst = self.buf.get_mut(self.len..end).ok_or(core::fmt::Error)?;
        dst.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// One 16550A.
pub struct Uart {
    dll: u8,
    dlm: u8,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    /// How many bytes the guest has sent. The bytes themselves go to whoever
    /// runs the guest (`write` returns each), which keeps them where it wants
    /// them -- this device keeps none.
    written: u64,
    /// A byte waiting to be read back, if the last thing written went round
    /// the loopback.
    loopback: Option<u8>,
    /// A byte waiting for the guest to read (receive path).
    rx: Option<u8>,
    /// Where the guest's output is in an escape sequence, and the column it
    /// has left the cursor at: what a cursor-position query is answered
    /// with, and the answer.
    esc: Esc,
    col: u16,
    reply: Reply,
    /// Set once the guest has sent its first cursor-position query, which a
    /// shell does when it is at a prompt ready to read: until then, typed
    /// input would be swallowed by the boot, so it is held back.
    prompt_seen: bool,
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
            written: 0,
            loopback: None,
            rx: None,
            esc: Esc::Text,
            col: 0,
            reply: Reply { buf: [0; REPLY_MAX], len: 0, pos: 0 },
            prompt_seen: false,
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
                /* Data register: a received byte, whatever loopback left,
                 * else nothing. */
                self.rx.take().or_else(|| self.loopback.take()).unwrap_or(0)
            }
            IER_DLM if self.dlab() => self.dlm,
            IER_DLM => self.ier,
            IIR_FCR => {
                /* The pending cause, highest priority first; reading it is
                 * one of the ways the THR-empty interrupt is cleared. */
                if self.ier & IER_RX != 0 && self.rx.is_some() {
                    IIR_RX
                } else if self.ier & IER_THRE != 0 {
                    IIR_THRE
                } else {
                    IIR_NONE
                }
            }
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => {
                let mut lsr = LSR_THRE | LSR_TEMT;
                if self.loopback.is_some() || self.rx.is_some() {
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
                self.track(value);
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

    /// Whether the UART is asserting its interrupt line (IRQ4 on COM1): the
    /// transmitter-holding-register-empty interrupt is enabled and, since the
    /// transmitter here is always ready, always pending; the receiver
    /// interrupt is enabled and a byte is waiting. What the run loop turns
    /// into a raised IRQ.
    pub fn irq_active(&self) -> bool {
        (self.ier & IER_THRE != 0) || (self.ier & IER_RX != 0 && self.rx.is_some())
    }

    /// Whether a query reply is waiting to be fed to the guest.
    pub fn reply_pending(&self) -> bool {
        self.reply.pos < self.reply.len
    }

    /// Whether the guest has reached a shell prompt (asked where the cursor
    /// is): only then is typed input handed over, so the boot does not eat it.
    pub fn prompt_seen(&self) -> bool {
        self.prompt_seen
    }

    /// Follow what the guest writes as a terminal would: the column the
    /// cursor ends at, and the one query a shell sends.
    ///
    /// A terminal like busybox's line editor asks where the cursor is by
    /// writing the escape sequence ESC [ 6 n, and waits for the answer before
    /// it takes a command. Nothing here is a terminal, so the answer is made
    /// up from the column the guest's own output has left the cursor at --
    /// the one number the editor uses: told any other column than the one
    /// its prompt ended at, it takes the line for that much longer, and
    /// wraps what is typed early (it was once told 80, and `id` came back as
    /// `i`, a newline, `d`). Without an answer, the shell swallows what is
    /// typed as the reply it was waiting for.
    fn track(&mut self, byte: u8) {
        let in_csi = matches!(self.esc, Esc::Csi | Esc::Csi6 | Esc::CsiOther);
        self.esc = match (self.esc, byte) {
            (_, ESC) => Esc::Start,
            (Esc::Start, b'[') => Esc::Csi,
            (Esc::Start, _) => Esc::Text,
            (Esc::Csi, b'6') => Esc::Csi6,
            (Esc::Csi6, b'n') => {
                self.answer();
                Esc::Text
            }
            /* Parameters and intermediates, then a final byte. */
            (_, 0x20..=0x3F) if in_csi => Esc::CsiOther,
            (_, _) if in_csi => Esc::Text,
            (_, b'\r' | b'\n') => {
                self.col = 0;
                Esc::Text
            }
            (_, BACKSPACE) => {
                self.col = self.col.saturating_sub(1);
                Esc::Text
            }
            (_, b'\t') => {
                self.col = ((self.col / TAB + 1) * TAB).min(COL_MAX);
                Esc::Text
            }
            /* A printable character, or the first byte of a UTF-8 one. */
            (_, 0x20..=0x7E | 0xC0..=0xFF) => {
                self.col = (self.col + 1).min(COL_MAX);
                Esc::Text
            }
            (_, _) => Esc::Text,
        };
    }

    /// The answer to a cursor-position query, in place of one the guest has
    /// not started to read -- one it is part way through stands, rather than
    /// be cut into -- and from the first query on, typed input is let
    /// through.
    fn answer(&mut self) {
        use core::fmt::Write;
        self.prompt_seen = true;
        if self.reply.pos != 0 && self.reply.pos < self.reply.len {
            return;
        }
        self.reply.len = 0;
        self.reply.pos = 0;
        /* Fits: `REPLY_MAX` is the longest, with the column at its most. */
        let _ = write!(self.reply, "\x1b[{};{}R", REPLY_ROW, self.col + 1);
    }

    /// The next byte of the answer to a query, if any: fed to the guest ahead
    /// of anything typed, so the shell gets the answer it waits for before
    /// the command.
    pub fn take_reply(&mut self) -> Option<u8> {
        if self.reply.pos >= self.reply.len {
            return None;
        }
        let byte = *self.reply.buf.get(self.reply.pos)?;
        self.reply.pos += 1;
        Some(byte)
    }

    /// Whether the receive register is free to take another byte.
    pub fn rx_empty(&self) -> bool {
        self.rx.is_none()
    }

    /// Hand the guest a byte on the receive path (as if typed at the
    /// console): it reads it from the data register, and while its receive
    /// interrupt is enabled the UART asserts IRQ4 until it does.
    pub fn set_rx(&mut self, byte: u8) {
        self.rx = Some(byte);
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// The interrupt-enable register, for a diagnostic on whether the guest
    /// turned the receive interrupt on.
    pub fn ier(&self) -> u8 {
        self.ier
    }

}
