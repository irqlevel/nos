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
//!
//! Its periodic edges repeat no faster than the local APIC's timer is let
//! repeat at (`lapic::MIN_PERIOD_NS`), whatever count a guest loads: the
//! count it reads back runs at the reload it gave, and IRQ0 comes a period
//! of at least that apart -- KVM's PIT has its floor too. A reload of a few
//! ticks would otherwise be an edge owed every microsecond, which the first
//! CPU's loop hands over one a round and rounds again for: with the timer's
//! pin routed to another CPU, whose taking of it this loop cannot see, it
//! never entered its own guest again (hv-fuzz found it).

use kcore::time;

/// The 8254 input frequency, in hertz.
const PIT_HZ: u64 = 1_193_182;

/// The fewest ticks apart channel 0's periodic edges come: the local
/// APIC's floor, in the 8254's ticks, rounded up.
const MIN_EDGE_TICKS: u64 = (PIT_HZ * crate::lapic::MIN_PERIOD_NS).div_ceil(kcore::consts::NS_PER_SEC);

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
    /// The count loaded last; 0 means 65536, as the 8254 has it.
    reload: u16,
    /// The access mode from the last control word: lo, hi or lo/hi.
    access: u8,
    /// The operating mode 0..5.
    mode: u8,
    /// For lo/hi access, which byte comes next on a write or a read.
    write_half: Half,
    read_half: Half,
    /// The low byte of a lo/hi count, until its high byte is written: a
    /// count is loaded whole, and nothing counts by half of one. Put into
    /// the reload as it came, the counter ran meanwhile on the old count's
    /// high byte and the new one's low, its edge count started again: one
    /// IRQ0 the chip never gives, or a one-shot up to 256 ticks early.
    low: u8,
    /// A latched count, if the guest latched one; read back before the live
    /// counter.
    latched: Option<u16>,
    /// Whether a count has been loaded since the control word that set the
    /// mode: until one is, the counter waits, counting nothing.
    loaded: bool,
    /// The ticks counted before `since_ns`: where a count that stopped -- its
    /// gate low, its first byte written in mode 0, a control word -- had got.
    counted: u64,
    /// When the count last started counting, in host nanoseconds: None while
    /// it does not count. Channel 0's gate is tied high; channel 2's is port
    /// 0x61 bit 0.
    since_ns: Option<u64>,
}

/// The 8254's ticks in `ns` nanoseconds. In 128 bits: nanoseconds times 1.19
/// MHz outgrows 64 bits after some four hours of a channel counting, and
/// saturating there would freeze the counter -- the guest's tick would stop
/// with nothing said. The quotient fits: a u64 of nanoseconds is 2^64 *
/// 1.19e6 / 1e9 ticks.
fn ticks(ns: u64) -> u64 {
    (ns as u128 * PIT_HZ as u128 / kcore::consts::NS_PER_SEC as u128) as u64
}

impl Channel {
    /// Out of reset: counting, or -- channel 2, whose gate starts low --
    /// held.
    fn new(counting: bool) -> Self {
        Self {
            reload: 0,
            access: ACCESS_LOHI,
            mode: 0,
            write_half: Half::Lo,
            read_half: Half::Lo,
            low: 0,
            latched: None,
            loaded: true,
            counted: 0,
            since_ns: counting.then(time::boot_time_ns),
        }
    }

    fn reload_ticks(&self) -> u64 {
        if self.reload == 0 { 65536 } else { self.reload as u64 }
    }

    /// Ticks between a periodic count's edges: its reload, but no fewer
    /// than the floor (`MIN_EDGE_TICKS`).
    fn edge_ticks(&self) -> u64 {
        self.reload_ticks().max(MIN_EDGE_TICKS)
    }

    /// Whether the count is counting now.
    fn counting(&self) -> bool {
        self.since_ns.is_some()
    }

    /// Ticks counted since the count was loaded.
    fn elapsed(&self) -> u64 {
        let running = self.since_ns.map_or(0, |since| ticks(time::boot_time_ns().saturating_sub(since)));
        self.counted.saturating_add(running)
    }

    /// The count stopped where it is.
    fn hold(&mut self) {
        self.counted = self.elapsed();
        self.since_ns = None;
    }

    /// Port 0x61's gate bit set where it was clear: in modes 0 and 4 the
    /// count goes on from where it stopped -- unless it waits for a count,
    /// or for the high byte of one in mode 0 -- and in the others it starts
    /// again from its reload, the gate's rise a trigger (the 8254's table of
    /// gate operations).
    fn gate_rose(&mut self) {
        if !self.loaded || (self.mode == 0 && self.write_half == Half::Hi) {
            return;
        }
        let now = time::boot_time_ns();
        if !one_shot(self.mode) {
            self.counted = 0;
            self.since_ns = Some(now);
        } else if self.since_ns.is_none() {
            self.since_ns = Some(now);
        }
    }

    /// Port 0x61's gate bit cleared: the count held where it is, in every
    /// mode but 1 and 5, whose gate is a trigger and no more.
    fn gate_fell(&mut self) {
        if !matches!(self.mode, 1 | 5) {
            self.hold();
        }
    }

    /// The counter as it reads now, 16 bits: where it stopped, while it does
    /// not count.
    fn current(&self) -> u16 {
        let reload = self.reload_ticks();
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
    /// Out of a control word it is low in mode 0 and high in the others
    /// until a count comes, and a periodic mode's gate low sets it high.
    fn output_high(&self) -> bool {
        if !self.loaded {
            return self.mode != 0;
        }
        if !self.counting() && periodic(self.mode) {
            return true;
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

    /// A whole count, `reload`, loaded: counted from now when `counting`.
    fn load(&mut self, reload: u16, counting: bool) {
        self.reload = reload;
        self.loaded = true;
        self.counted = 0;
        self.since_ns = counting.then(time::boot_time_ns);
        self.latched = None;
    }
}

/// The three counters, and the speaker/gate byte.
pub struct Pit {
    ch: [Channel; 3],
    /// Port 0x61 as last written, less the read-only output bit.
    port61: u8,
    /// How many channel-0 periods have been handed to the run loop as IRQ0
    /// edges since the count was loaded. What has elapsed beyond it is owed.
    ch0_edges_given: u64,
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
    /// Out of reset: channels 0 and 1 counting, their gates tied high, and
    /// channel 2 held -- port 0x61's gate bit is clear.
    pub fn new() -> Self {
        Self {
            ch: [Channel::new(true), Channel::new(true), Channel::new(false)],
            port61: 0,
            ch0_edges_given: 0,
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
                let ch = &mut self.ch[index];
                let value = ch.latched.unwrap_or_else(|| ch.current());
                /* A latched count lasts until its last byte is read: one
                 * byte in lo-only and hi-only access, both in lo/hi -- the
                 * high byte of the same count, not of the counter as it is
                 * by then, which one exit later could be 256 ticks lower,
                 * a counter running backwards. */
                match ch.access {
                    ACCESS_LO => {
                        ch.latched = None;
                        value as u8
                    }
                    ACCESS_HI => {
                        ch.latched = None;
                        (value >> 8) as u8
                    }
                    _ => {
                        let byte = match ch.read_half {
                            Half::Lo => value as u8,
                            Half::Hi => {
                                ch.latched = None;
                                (value >> 8) as u8
                            }
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
                let was = self.port61 & P61_CH2_GATE != 0;
                self.port61 = value & (P61_CH2_GATE | P61_SPEAKER);
                let gate = value & P61_CH2_GATE != 0;
                match (was, gate) {
                    (false, true) => self.ch[2].gate_rose(),
                    (true, false) => self.ch[2].gate_fell(),
                    _ => {}
                }
            }
            CONTROL => self.control(value),
            CH0..=CH2 => {
                let index = (port - CH0) as usize;
                let loaded = self.write_counter(index, value);
                if index == 0 {
                    if loaded {
                        /* A count loaded restarts channel 0's edge count; in
                         * a one-shot mode it is one edge to come. */
                        self.ch0_edges_given = 0;
                        self.ch0_armed = one_shot(self.ch[0].mode);
                    } else if self.ch[0].mode == 0 {
                        /* Mode 0's first byte stops the count: a shot armed
                         * is not one now, until the high byte loads one. */
                        self.ch0_armed = false;
                    }
                }
            }
            _ => {}
        }
    }

    /// Whether channel 0 has an IRQ0 edge for the run loop to raise now,
    /// which the loop asks only once the guest has taken the last one.
    ///
    /// In the periodic modes every period that elapsed is owed, and handed
    /// over one at a time: a halted guest's vCPU sleeps until the host wakes
    /// it, at the host tick's grain, and a guest ticking at 250 Hz that was
    /// given one edge for every two or three periods that passed counted
    /// 40% of real time -- a guest keeping time in jiffies falls behind the
    /// host by more than a minute in two, and one on the TSC has its
    /// clocksource watchdog, which compares the TSC against jiffies, call
    /// the TSC unstable. Owed ticks come late and together, as KVM's PIT
    /// delivers them, but they come. No more than a second's worth is owed:
    /// a guest the host did not run for longer takes its time up from its
    /// clocksource, not from a flood.
    pub fn ch0_fire(&mut self) -> bool {
        let ch = &self.ch[0];
        if !ch.counting() {
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
        let edges = ch.elapsed() / ch.edge_ticks();
        let most_owed = (PIT_HZ / ch.edge_ticks()).max(1);
        if edges > self.ch0_edges_given.saturating_add(most_owed) {
            self.ch0_edges_given = edges - most_owed;
        }
        if edges > self.ch0_edges_given {
            self.ch0_edges_given += 1;
            true
        } else {
            false
        }
    }

    /// When, in host nanoseconds since boot, channel 0 next raises IRQ0: the
    /// edge after the last one `ch0_fire` reported, or a one-shot's one edge.
    /// None when channel 0 has no edge to come -- not counting, a one-shot
    /// that has fired, a mode that makes none. What a halted vCPU sleeps
    /// until: the only thing here that becomes pending with time.
    pub fn next_ch0_edge_ns(&self) -> Option<u64> {
        let ch = &self.ch[0];
        let since = ch.since_ns?;
        /* Edge k is due once `elapsed()` reaches k times the ticks between
         * edges -- the reload, or the floor for a periodic count -- which is
         * the first nanosecond at or past that many ticks' 1e9 / PIT_HZ past
         * when it began counting, less what it counted before: rounded up,
         * so that at that instant `ch0_fire` does see it. A one-shot's one
         * edge is edge 1, at its reload. */
        let (edge, every) = if one_shot(ch.mode) && self.ch0_armed {
            (1, ch.reload_ticks())
        } else if periodic(ch.mode) {
            (self.ch0_edges_given as u128 + 1, ch.edge_ticks())
        } else {
            return None;
        };
        let ticks = (edge * every as u128).saturating_sub(u128::from(ch.counted));
        let ns = (ticks * kcore::consts::NS_PER_SEC as u128).div_ceil(PIT_HZ as u128);
        let at = u128::from(since).saturating_add(ns);
        Some(u64::try_from(at).unwrap_or(u64::MAX))
    }

    /// Channel 0's mode, reload and whether it counts, for a diagnostic.
    pub fn ch0_state(&self) -> (u8, u16, bool) {
        (self.ch[0].mode, self.ch[0].reload, self.ch[0].counting())
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
            /* Latch the current count for a stable read -- one latched and
             * not yet read whole stays, as a second latch command is
             * ignored by the chip. */
            if self.ch[index].latched.is_none() {
                self.ch[index].latched = Some(self.ch[index].current());
            }
            return;
        }
        let ch = &mut self.ch[index];
        ch.access = access;
        ch.mode = (value >> MODE_SHIFT) & 0x7;
        ch.write_half = Half::Lo;
        ch.read_half = Half::Lo;
        /* A new mode counts nothing until its count is written: the counter
         * waits -- the old count does not run on in the new mode, its edges
         * channel 0's IRQ0 -- and a one-shot armed before it is not one
         * now. */
        ch.hold();
        ch.loaded = false;
        if index == 0 {
            self.ch0_armed = false;
        }
    }

    /// A byte of a count: true once a whole count has been loaded.
    fn write_counter(&mut self, index: usize, value: u8) -> bool {
        let ch = &mut self.ch[index];
        let counting = index != 2 || self.port61 & P61_CH2_GATE != 0;
        match ch.access {
            ACCESS_LO => ch.load(value as u16, counting),
            ACCESS_HI => ch.load((value as u16) << 8, counting),
            _ => match ch.write_half {
                Half::Lo => {
                    /* Kept until the high byte comes, which loads the two:
                     * in mode 0 the first byte stops the count until then,
                     * and in the others it changes nothing that counts. */
                    ch.low = value;
                    ch.write_half = Half::Hi;
                    if ch.mode == 0 {
                        ch.hold();
                    }
                    return false;
                }
                Half::Hi => {
                    let reload = u16::from(ch.low) | (u16::from(value) << 8);
                    ch.load(reload, counting);
                    ch.write_half = Half::Lo;
                }
            },
        }
        true
    }
}
