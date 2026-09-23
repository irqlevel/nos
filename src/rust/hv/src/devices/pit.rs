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
//! And channel 0 is the guest's tick, IRQ0 at the emulated 8259 (the run
//! loop asks `ch0_fire` each time round): an edge a period in the periodic
//! modes 2 and 3, and in the one-shot modes 0 and 4 one edge when a loaded
//! count runs out -- what Linux's `i8253` clockevent device programs once it
//! has a clocksource good enough for high-resolution timers, the TSC, and
//! drives the tick and every timer from, a count at a time.

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
        /* In 128 bits: nanoseconds times 1.19 MHz outgrows 64 bits after some
         * four hours of a channel counting, and saturating there would freeze
         * the counter -- the guest's tick would stop with nothing said. The
         * quotient fits: a u64 of nanoseconds is 2^64 * 1.19e6 / 1e9 ticks. */
        (ns as u128 * PIT_HZ as u128 / kcore::consts::NS_PER_SEC as u128) as u64
    }

    /// The counter as it reads now, 16 bits.
    fn current(&self) -> u16 {
        let reload = self.reload_ticks();
        if !self.running {
            return self.reload as u16;
        }
        let elapsed = self.elapsed();
        match self.mode {
            /* The one-shot modes, 0 and 4 and their gate-triggered 1 and 5:
             * count down once to 0; the 8254 wraps to 0xFFFF and keeps going,
             * which is what a reader sees after the terminal count. */
            0 | 1 | 4 | 5 => {
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
            /* Low for the one clock of the terminal count. */
            4 | 5 => elapsed != reload,
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
    /// How many channel-0 periods had elapsed at the last `ch0_fire`, so an
    /// edge is counted once: the running total of IRQ0s the timer owes.
    ch0_edges_seen: u64,
    /// In a one-shot mode, whether channel 0's count has been loaded and has
    /// not yet run out: its one edge is still to come.
    ch0_armed: bool,
}

/// The one-shot modes: one edge when the count runs out, and none after
/// until a count is loaded again. Mode 0's output rises at the terminal
/// count; mode 4's strobes low there for a clock, and rises after it.
fn one_shot(mode: u8) -> bool {
    matches!(mode, 0 | 4)
}

fn periodic(mode: u8) -> bool {
    /* Modes 6 and 7 are 2 and 3 again, as the 8254 decodes them. */
    matches!(mode, 2 | 3 | 6 | 7)
}

impl Pit {
    pub fn new() -> Self {
        Self {
            ch: [Channel::new(), Channel::new(), Channel::new()],
            port61: 0,
            ch0_edges_seen: 0,
            ch0_armed: false,
        }
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
                let loaded = self.write_counter(index, value);
                if index == 0 {
                    /* A fresh channel-0 program restarts its edge count; a
                     * count loaded in a one-shot mode is one edge to come. */
                    self.ch0_edges_seen = 0;
                    if loaded {
                        self.ch0_armed = one_shot(self.ch[0].mode);
                    }
                }
            }
            _ => {}
        }
    }

    /// Whether channel 0 has completed at least one more period since this
    /// was last asked -- an IRQ0 edge the run loop should raise. Advances the
    /// seen count by all whole periods elapsed, so a burst behind a slow
    /// entry collapses to one interrupt rather than a backlog (the tick a
    /// guest missed while it was not running is not owed to it many times
    /// over).
    pub fn ch0_fire(&mut self) -> bool {
        let ch = &self.ch[0];
        if !ch.running {
            return false;
        }
        if one_shot(ch.mode) {
            if self.ch0_armed && ch.elapsed() >= ch.reload_ticks() {
                self.ch0_armed = false;
                return true;
            }
            return false;
        }
        if !periodic(ch.mode) {
            return false;
        }
        let edges = ch.elapsed() / ch.reload_ticks();
        if edges > self.ch0_edges_seen {
            self.ch0_edges_seen = edges;
            true
        } else {
            false
        }
    }

    /// When, in host nanoseconds since boot, channel 0 next raises IRQ0: the
    /// edge after the last one `ch0_fire` reported, or a one-shot's one edge.
    /// None when channel 0 has no edge to come -- stopped, a one-shot that has
    /// fired, a mode that makes none. What a halted vCPU sleeps until: the
    /// only thing here that becomes pending with time.
    pub fn next_ch0_edge_ns(&self) -> Option<u64> {
        let ch = &self.ch[0];
        if !ch.running {
            return None;
        }
        /* Edge k is due once `elapsed()` reaches k * reload ticks, which is
         * the first nanosecond at or past k * reload * 1e9 / PIT_HZ: rounded
         * up, so that at that instant `ch0_fire` does see it. A one-shot's
         * one edge is edge 1. */
        let edge = if one_shot(ch.mode) && self.ch0_armed {
            1
        } else if periodic(ch.mode) {
            self.ch0_edges_seen as u128 + 1
        } else {
            return None;
        };
        let ticks = edge * ch.reload_ticks() as u128;
        let ns = (ticks * kcore::consts::NS_PER_SEC as u128).div_ceil(PIT_HZ as u128);
        let at = (ch.loaded_ns as u128).saturating_add(ns);
        Some(u64::try_from(at).unwrap_or(u64::MAX))
    }

    /// Channel 0's mode, reload and running flag, for a diagnostic.
    pub fn ch0_state(&self) -> (u8, u16, bool) {
        (self.ch[0].mode, self.ch[0].reload, self.ch[0].running)
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
        if index == 0 {
            /* A new mode counts nothing until its count is written: a
             * one-shot armed before it is not one now. */
            self.ch0_armed = false;
        }
    }

    /// A byte of a count: true once a whole count has been loaded.
    fn write_counter(&mut self, index: usize, value: u8) -> bool {
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
                    return false;
                }
                Half::Hi => {
                    let reload = (ch.reload & 0x00FF) | ((value as u16) << 8);
                    ch.load(reload, running);
                    ch.write_half = Half::Lo;
                }
            },
        }
        true
    }
}
