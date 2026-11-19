//! The root port above a host's device (`devices::port`), against a model
//! of its configuration space of its own: what the PCI and PCIe
//! specifications say a root port of no slot, no interrupt and no extended
//! capability shows, and what the port promises -- the guest's writes kept
//! where the guest may write and nowhere else, the link the device's as it
//! is at each read, its windows holding the spans of the BARs below it and
//! nothing a MiB past them, and the bus below it the one the guest numbered.

use crate::devices::port::{Link, RootPort};
use crate::targets::Input;

/// What of each dword the guest may write: the command register's I/O,
/// memory, master, parity, SERR and INTx disable; the cache line size; the
/// bus numbers; the windows' address bits; the interrupt line and bridge
/// control's first seven bits; device control's reporting enables and
/// relaxed ordering; link control's ASPM, read completion boundary, common
/// clock, extended synch, clock PM and width disable; root control's SERR
/// and PME enables; device control 2's completion timeout; link control 2's
/// target speed.
fn guest_bits(dword: usize) -> u32 {
    match dword {
        0x04 => 0x0000_0547,
        0x0C => 0x0000_00FF,
        0x18 => 0x00FF_FFFF,
        0x20 | 0x24 => 0xFFF0_FFF0,
        0x28 | 0x2C => u32::MAX,
        0x3C => 0x007F_00FF,
        0x48 => 0x0000_001F,
        0x50 => 0x0000_03CB,
        0x5C => 0x0000_000F,
        0x68 => 0x0000_001F,
        0x70 => 0x0000_000F,
        _ => 0,
    }
}

/// The model: what the guest wrote, a dword each, as a port that kept it
/// would hold it, and the link the device below says it has.
struct Model {
    kept: [u32; 64],
    number: u8,
    link: Link,
}

const MIB: u64 = 1 << 20;

impl Model {
    fn new(number: u8, secondary: u8) -> Model {
        let mut kept = [0u32; 64];
        kept[0x04 / 4] = 0x0006;
        kept[0x18 / 4] = (u32::from(secondary) << 8) | (u32::from(secondary) << 16);
        kept[0x20 / 4] = 0x0000_FFF0;
        kept[0x24 / 4] = 0x0000_FFF0;
        Model { kept, number, link: Link::default() }
    }

    /// The windows as firmware sets them for `low` and `high`.
    fn windows(&mut self, low: Option<(u64, u64)>, high: Option<(u64, u64)>) {
        let reg = |a: u64| (((a >> 20) & 0xFFF) << 4) as u32;
        let open = |s: Option<(u64, u64)>| s.filter(|&(start, end)| end > start);
        self.kept[0x20 / 4] = match open(low).filter(|&(_, end)| end - 1 < 1 << 32) {
            Some((start, end)) => reg(start) | (reg(end - 1) << 16),
            None => 0x0000_FFF0,
        };
        match open(high) {
            Some((start, end)) => {
                self.kept[0x24 / 4] = reg(start) | (reg(end - 1) << 16);
                self.kept[0x28 / 4] = (start >> 32) as u32;
                self.kept[0x2C / 4] = ((end - 1) >> 32) as u32;
            }
            None => {
                self.kept[0x24 / 4] = 0x0000_FFF0;
                self.kept[0x28 / 4] = 0;
                self.kept[0x2C / 4] = 0;
            }
        }
    }

    /// The speeds a port of this link supports, a bit each from bit 1.
    fn speeds(&self) -> u32 {
        let most = self.link.cap & 0xF;
        let allowed: u32 = (1..=most).fold(0, |a, s| a | (1 << s)) & 0xFE;
        match self.link.cap2 & 0xFE {
            0 => allowed,
            listed => listed & allowed,
        }
    }

    /// The dword at `d` as the guest reads it.
    fn dword(&self, d: usize) -> u32 {
        let kept = self.kept[d / 4] & guest_bits(d);
        let payload = u32::from(self.link.payload & 7);
        match d {
            0x00 => 0x000C_1B36,
            0x04 => kept | 0x0010_0000,
            0x08 => 0x0604_0000,
            0x0C => kept | 0x0001_0000,
            0x24 => kept | 0x0001_0001,
            0x34 => 0x40,
            0x40 => 0x0042_0010,
            0x44 => payload | 1 << 15,
            0x48 => kept | payload << 5,
            0x4C => (self.link.cap & 0x3FF) | 1 << 20 | u32::from(self.number) << 24,
            0x50 => kept | (u32::from(self.link.status & 0x13FF) | 0x2000) << 16,
            0x6C => self.speeds(),
            0x70 => match kept {
                0 => self.link.cap & 0xF,
                target => target,
            },
            _ => kept,
        }
    }

    fn read(&self, at: usize, size: usize) -> u32 {
        let mask = match size {
            1 => 0xFF,
            2 => 0xFFFF,
            _ => u32::MAX,
        };
        if !matches!(size, 1 | 2 | 4) || at % size != 0 || at + size > 4096 {
            return mask;
        }
        if at >= 256 {
            return 0;
        }
        (self.dword(at & !3) >> (8 * (at % 4))) & mask
    }

    fn write(&mut self, at: usize, size: usize, value: u32) {
        if !matches!(size, 1 | 2 | 4) || at % size != 0 || at + size > 256 {
            return;
        }
        let shift = 8 * (at % 4) as u32;
        let width = if size == 4 { u32::MAX } else { (1u32 << (8 * size)) - 1 };
        let mask = (width << shift) & guest_bits(at & !3);
        let d = (at & !3) / 4;
        self.kept[d] = (self.kept[d] & !mask) | ((value << shift) & mask);
    }

    fn secondary(&self) -> u8 {
        (self.kept[0x18 / 4] >> 8) as u8
    }
}

/// The span a window's registers say, where they say one: its first byte
/// and its last.
fn decoded(low: bool, reg: u32, upper_base: u32, upper_limit: u32) -> Option<(u64, u64)> {
    let base = (u64::from(reg & 0xFFF0) << 16) | if low { 0 } else { u64::from(upper_base) << 32 };
    let limit = (u64::from(reg >> 16 & 0xFFF0) << 16) | 0xF_FFFF | if low { 0 } else { u64::from(upper_limit) << 32 };
    (base <= limit).then_some((base, limit))
}

fn span(r: &mut Input) -> Option<(u64, u64)> {
    if r.u8() < 32 {
        return None;
    }
    let start = match r.u8() % 4 {
        0 => r.below(1 << 32),
        1 => 0xC000_0000 + r.below(0x3E00_0000),
        2 => (1 << 32) + r.below(1 << 40),
        _ => r.u64(),
    };
    let len = match r.u8() % 4 {
        0 => 0,
        1 => 1 + r.below(MIB),
        2 => (1 + r.below(64)) * 4096,
        _ => 1u64 << r.below(40),
    };
    Some((start, start.saturating_add(len)))
}

fn link(r: &mut Input) -> Link {
    Link { cap: r.value32(), status: r.u16(), cap2: if r.bool() { r.value32() } else { 0 }, payload: r.u16() }
}

pub fn port_target(r: &mut Input) {
    let number = 1 + r.below(7) as u8;
    let mut port = RootPort::new(number, number);
    let mut model = Model::new(number, number);
    model.link = link(r);
    while let Some(op) = r.op(6) {
        match op {
            0 | 1 => {
                let at = match r.u8() {
                    0..=199 => r.below(0x80) as usize,
                    200..=239 => r.below(0x110) as usize,
                    _ => r.below(0x1010) as usize,
                };
                let size = r.pick(&[4usize, 4, 2, 1, 3, 8]);
                let got = port.read(at, size, &model.link);
                let want = model.read(at, size);
                invariant!(got == want, "a read of {} bytes at {:#x}: {:#x}, not {:#x}", size, at, got, want);
            }
            2 => {
                let at = if r.u8() < 200 { r.below(0x80) as usize } else { r.below(0x110) as usize };
                let size = r.pick(&[4usize, 4, 2, 1, 3]);
                let value = r.value32();
                port.write(at, size, value);
                model.write(at, size, value);
            }
            3 => model.link = link(r),
            4 => {
                /* Its windows as the run loop sets them for the BARs below:
                 * each holds its span from the MiB the span starts in, and
                 * ends in the MiB the span ends in -- a low one above 4 GiB
                 * none */
                let (low, high) = (span(r), span(r));
                port.set_windows(low, high);
                model.windows(low, high);
                let read = |at: usize| port.read(at, 4, &model.link);
                let open = |s: Option<(u64, u64)>| s.filter(|&(start, end)| end > start);
                let got_low = decoded(true, read(0x20), 0, 0);
                let want_low = open(low).filter(|&(_, end)| end - 1 < 1 << 32);
                let got_high = decoded(false, read(0x24), read(0x28), read(0x2C));
                for (name, got, want) in [("memory", got_low, want_low), ("prefetchable", got_high, open(high))] {
                    match (got, want.map(|(start, end)| (start, end - 1))) {
                        (None, None) => {}
                        (Some((base, limit)), Some((first, last))) => {
                            invariant!(base <= first && first - base < MIB && limit >= last && limit - last < MIB,
                                       "the {} window [{:#x}, {:#x}] for the BARs' [{:#x}, {:#x}]", name, base, limit,
                                       first, last);
                        }
                        (got, want) => invariant!(false, "the {} window {:x?} for the BARs' {:x?}", name, got, want),
                    }
                }
            }
            _ => {
                let bus = r.u8();
                let want = bus != 0 && bus == model.secondary();
                invariant!(port.below(bus) == want, "bus {:#x} {} below a port whose bus below is {:#x}", bus,
                           if want { "not" } else { "" }, model.secondary());
            }
        }
    }
}
