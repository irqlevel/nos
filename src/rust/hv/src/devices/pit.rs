//! A minimal Intel 8254 programmable interval timer, ports 0x40-0x43 and the
//! bits of port 0x61 that gate channel 2.
//!
//! A guest cannot calibrate its time without one: with no PIT the kernel
//! reads a counter that never counts and a channel-2 output that never goes
//! high, and spins there for good. What is emulated is what calibration and
//! timekeeping read -- a counter that counts down at the 8254's own
//! 1.193182 MHz off the host's clock, and channel 2's output going high when
//! its count runs out (`pit_calibrate_tsc`'s wait on port 0x61 bit 5).
//!
//! No interrupt is delivered yet: channel 0's terminal count would raise
//! IRQ0, and there is no interrupt controller for a guest to take it from.
//! That comes with the APIC. Until then this is enough to get a guest
//! through the time setup that would otherwise hang it, reading a counter
//! that moves and an output that eventually fires.

use kcore::time;

/// The 8254 input frequency, in hertz.
const PIT_HZ: u64 = 1_193_182;

/* Ports. */
pub const CH0: u16 = 0x40;
pub const CH2: u16 = 0x42;
pub const CONTROL: u16 = 0x43;
/// The NMI status and control port, whose low bits gate channel 2 and whose
/// bit 5 is channel 2's output.
pub const PORT_61: u16 = 0x61;

/* Port 0x61 bits. */
const P61_CH2_GATE: u8 = 1 << 0;
const P61_SPEAKER: u8 = 1 << 1;
const P61_CH2_OUT: u8 = 1 << 5;

/* Control-word fields. */
const ACCESS_SHIFT: u8 = 4;
const ACCESS_LATCH: u8 = 0;
const ACCESS_LO: u8 = 1;
const ACCESS_HI: u8 = 2;
const ACCESS_LOHI: u8 = 3;
const MODE_SHIFT: u8 = 1;

/// Which byte a lo/hi channel expects next.
#[derive(Clone, Copy, PartialEq)]
enum Half {
    Lo,
    Hi,
}

/// One counter.
struct Channel {
    /// The reload value; 0 means 65536, as the 8254 has it.
    reload: u16,
    /// The access mode from the last control word: lo, hi or lo/hi.
    access: u8,
    /// The operating mode 0..5.
    mode: u8,
    /// For lo/hi access, which byte comes next on a write or a read.
    write_half: Half,
    read_half: Half,
    /// A latched count, if the guest latched one; read back before the live
    /// counter.
    latched: Option<u16>,
    /// When the current count was loaded, in host nanoseconds -- the zero
    /// from which the counter has been counting down.
    loaded_ns: u64,
    /// Whether the counter is counting: channel 0 always is; channel 2 only
    /// while its gate (port 0x61 bit 0) is set.
    running: bool,
}

impl Channel {
    fn new() -> Self {
        Self {
            reload: 0,
            access: ACCESS_LOHI,
            mode: 0,
            write_half: Half::Lo,
            read_half: Half::Lo,
            latched: None,
            loaded_ns: time::boot_time_ns(),
            running: true,
        }
    }

    fn reload_ticks(&self) -> u64 {
        if self.reload == 0 { 65536 } else { self.reload as u64 }
    }

    /// Ticks elapsed since the count was loaded.
    fn elapsed(&self) -> u64 {
        let ns = time::boot_time_ns().saturating_sub(self.loaded_ns);
        ns.saturating_mul(PIT_HZ) / kcore::consts::NS_PER_SEC
    }

    /// The counter as it reads now, 16 bits.
    fn current(&self) -> u16 {
        let reload = self.reload_ticks();
        if !self.running {
            return self.reload as u16;
        }
        let elapsed = self.elapsed();
        match self.mode {
            /* Mode 0: count down once to 0 and stay; the 8254 wraps to
             * 0xFFFF and keeps going, which is what a reader sees after the
             * terminal count. */
            0 | 1 => {
                if elapsed >= reload {
                    (0u16).wrapping_sub((elapsed - reload) as u16)
                } else {
                    (reload - elapsed) as u16
                }
            }
            /* The periodic modes: count down, reload, repeat. */
            _ => (reload - 1 - (elapsed % reload)) as u16,
        }
    }

    /// Channel-2's output line: mode 0 goes high at the terminal count and
    /// stays; the periodic modes pulse, reading high for most of the cycle.
    fn output_high(&self) -> bool {
        if !self.running {
            return false;
        }
        let reload = self.reload_ticks();
        let elapsed = self.elapsed();
        match self.mode {
            0 | 1 => elapsed >= reload,
            _ => elapsed % reload != 0,
        }
    }

    fn load(&mut self, reload: u16, running: bool) {
        self.reload = reload;
        self.loaded_ns = time::boot_time_ns();
        self.running = running;
        self.latched = None;
    }
}

/// The three counters, and the speaker/gate byte.
pub struct Pit {
    ch: [Channel; 3],
    /// Port 0x61 as last written, less the read-only output bit.
    port61: u8,
}

impl Pit {
    pub fn new() -> Self {
        Self { ch: [Channel::new(), Channel::new(), Channel::new()], port61: 0 }
    }

    /// Whether `port` is one this device answers.
    pub fn owns(port: u16) -> bool {
        (CH0..=CONTROL).contains(&port) || port == PORT_61
    }

    /// A guest read of `port`.
    pub fn read(&mut self, port: u16) -> u8 {
        match port {
            PORT_61 => {
                let mut v = self.port61 & !P61_CH2_OUT;
                if self.ch[2].output_high() {
                    v |= P61_CH2_OUT;
                }
                v
            }
            CH0..=CH2 => {
                let index = (port - CH0) as usize;
                let value = match self.ch[index].latched.take() {
                    Some(v) => v,
                    None => self.ch[index].current(),
                };
                let ch = &mut self.ch[index];
                match ch.access {
                    ACCESS_LO => value as u8,
                    ACCESS_HI => (value >> 8) as u8,
                    _ => {
                        let byte = match ch.read_half {
                            Half::Lo => value as u8,
                            Half::Hi => (value >> 8) as u8,
                        };
                        ch.read_half = if ch.read_half == Half::Lo { Half::Hi } else { Half::Lo };
                        byte
                    }
                }
            }
            _ => 0xFF,
        }
    }

    /// A guest write of `value` to `port`.
    pub fn write(&mut self, port: u16, value: u8) {
        match port {
            PORT_61 => {
                self.port61 = value & (P61_CH2_GATE | P61_SPEAKER);
                let gate = value & P61_CH2_GATE != 0;
                /* Enabling the gate (re)starts channel 2 counting from its
                 * reload; clearing it freezes the count. */
                let ch = &mut self.ch[2];
                if gate && !ch.running {
                    ch.load(ch.reload, true);
                } else if !gate {
                    ch.running = false;
                }
            }
            CONTROL => self.control(value),
            CH0..=CH2 => {
                let index = (port - CH0) as usize;
                self.write_counter(index, value);
            }
            _ => {}
        }
    }

    fn control(&mut self, value: u8) {
        let index = (value >> 6) as usize;
        if index == 3 {
            /* Read-back command: not used by the paths that matter, and
             * ignored -- a guest that issues one reads the live counter,
             * which is close enough. */
            return;
        }
        let access = (value >> ACCESS_SHIFT) & 0x3;
        if access == ACCESS_LATCH {
            /* Latch the current count for a stable read. */
            self.ch[index].latched = Some(self.ch[index].current());
            return;
        }
        let ch = &mut self.ch[index];
        ch.access = access;
        ch.mode = (value >> MODE_SHIFT) & 0x7;
        ch.write_half = Half::Lo;
        ch.read_half = Half::Lo;
    }

    fn write_counter(&mut self, index: usize, value: u8) {
        let ch = &mut self.ch[index];
        let running = index != 2 || self.port61 & P61_CH2_GATE != 0;
        match ch.access {
            ACCESS_LO => ch.load(value as u16, running),
            ACCESS_HI => ch.load((value as u16) << 8, running),
            _ => match ch.write_half {
                Half::Lo => {
                    /* Keep the low byte until the high one arrives; the
                     * counter reloads on the high byte. */
                    ch.reload = (ch.reload & 0xFF00) | value as u16;
                    ch.write_half = Half::Hi;
                }
                Half::Hi => {
                    let reload = (ch.reload & 0x00FF) | ((value as u16) << 8);
                    ch.load(reload, running);
                    ch.write_half = Half::Lo;
                }
            },
        }
    }
}
