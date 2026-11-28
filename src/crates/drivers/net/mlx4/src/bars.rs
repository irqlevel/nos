//! Which `BarIndex` PCI I/O wants for a BAR (docs/spec/connectx3.md 1.2).
//!
//! EDK2's PCI I/O counts BAR *registers*: the 64-bit DCS BAR takes indexes 0
//! and 1, so the UAR BAR (config 0x18) is index 2. AMI Aptio 4 (the X9
//! blades, #15) counts *BARs*: a 64-bit BAR takes one index, so the UAR BAR
//! is index 1 and index 2 is refused with UNSUPPORTED. Both candidates are
//! asked for their `GetBarAttributes` descriptor, and the one whose base is
//! the address in the BAR's config registers wins; failing that, indexes 0–5
//! are asked for that base.
//!
//! No UEFI here, so `test/bars.rs` can run it on the host against both
//! numbering schemes.

/// What PCI I/O said about one `BarIndex`: the base and length of the QWORD
/// descriptor `GetBarAttributes` returned, or `None` when it refused.
pub type Answer = Option<(u64, u64)>;

/// The chosen index and what PCI I/O said about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    pub index: u8,
    /// The BAR's length, when PCI I/O gave one for `index`.
    pub size: Option<u64>,
    pub how: How,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// The descriptor's base is the address in config space.
    BaseMatches,
    /// Neither candidate's base matched; another index 0–5 had it.
    Scan,
    /// Only this index answered (its base did not match, or was not given).
    OnlyAnswer,
    /// Both answered and neither base matched: register numbering (spec 1.2).
    NoMatch,
    /// PCI I/O refused every candidate: register numbering (spec 1.2).
    NoAnswer,
}

impl How {
    pub fn describe(self) -> &'static str {
        match self {
            How::BaseMatches => "its base matches config space",
            How::Scan => "found by asking indexes 0-5 for the config-space base",
            How::OnlyAnswer => "the only index PCI I/O knows",
            How::NoMatch => "no base matches config space; register numbering",
            How::NoAnswer => "PCI I/O knows neither index; register numbering",
        }
    }
}

/// A 64-bit memory BAR: memory space (bit 0 clear), type 10b in bits 2:1.
fn is_64bit(dword: u32) -> bool {
    dword & 0x7 == 0x4
}

/// The address the BAR in register `reg` holds; `regs` are the six BAR
/// dwords from config 0x10..0x28.
pub fn base(regs: &[u32; 6], reg: usize) -> u64 {
    let lo = regs[reg];
    if lo & 1 != 0 {
        return u64::from(lo & !0x3);
    }
    let high = if is_64bit(lo) && reg + 1 < 6 { u64::from(regs[reg + 1]) << 32 } else { 0 };
    high | u64::from(lo & !0xf)
}

/// The candidate indexes for register `reg`: the register index (EDK2) and
/// the number of BARs below it (AMI), the same when nothing 64-bit is below.
pub fn candidates(regs: &[u32; 6], reg: usize) -> [u8; 2] {
    let (mut r, mut n) = (0, 0u8);
    while r < reg {
        r += if is_64bit(regs[r]) { 2 } else { 1 };
        n += 1;
    }
    [reg as u8, n]
}

/// Pick the `BarIndex` for the BAR in register `reg`, asking PCI I/O about
/// each candidate with `ask`.
pub fn choose(regs: &[u32; 6], reg: usize, mut ask: impl FnMut(u8) -> Answer) -> Pick {
    let want = base(regs, reg);
    let c = candidates(regs, reg);
    let first = ask(c[0]);
    let second = if c[1] != c[0] { ask(c[1]) } else { None };
    let answers = [(c[0], first), (c[1], second)];
    if want != 0 {
        if let Some(&(index, Some((_, len)))) = answers.iter().find(|(_, a)| matches!(a, Some((b, _)) if *b == want)) {
            return Pick { index, size: Some(len), how: How::BaseMatches };
        }
        for index in (0..6).filter(|i| !c.contains(i)) {
            if let Some((b, len)) = ask(index) {
                if b == want {
                    return Pick { index, size: Some(len), how: How::Scan };
                }
            }
        }
    }
    match (first, second) {
        (Some((_, len)), None) => Pick { index: c[0], size: Some(len), how: How::OnlyAnswer },
        (None, Some((_, len))) => Pick { index: c[1], size: Some(len), how: How::OnlyAnswer },
        (Some((_, len)), Some(_)) => Pick { index: c[0], size: Some(len), how: How::NoMatch },
        (None, None) => Pick { index: c[0], size: None, how: How::NoAnswer },
    }
}
