//! `src/bars.rs` against simulated PCI I/O firmwares (#15): EDK2, which
//! numbers BAR registers (UAR = BarIndex 2), and AMI Aptio 4, which numbers
//! BARs (UAR = BarIndex 1, BarIndex 2 refused). Run by `tests/mlx4-test.py`.
#[path = "../../../src/crates/drivers/net/mlx4/src/bars.rs"]
mod bars;
use bars::{choose, How, Pick};

const DCS: u64 = 0x1_f780_0000;
const DCS_LEN: u64 = 0x10_0000;
const UAR: u64 = 0x1_f000_0000;
const UAR_LEN: u64 = 0x80_0000;

/// ConnectX-3's BAR registers: two 64-bit prefetchable memory BARs.
fn regs() -> [u32; 6] {
    let lo = |a: u64| (a as u32) | 0xc;
    [lo(DCS), (DCS >> 32) as u32, lo(UAR), (UAR >> 32) as u32, 0, 0]
}

#[derive(Clone, Copy)]
enum Fw {
    Edk2,
    Ami,
    /// AMI numbering, but descriptors carry no base (0).
    AmiNoBase,
    /// Answers only at index 4 (neither candidate) with the right base.
    Odd,
    Nothing,
}

/// GetBarAttributes as each firmware answers it; records the indexes asked.
fn sim(fw: Fw, asked: &mut Vec<u8>) -> impl FnMut(u8) -> bars::Answer + '_ {
    move |i| {
        asked.push(i);
        match (fw, i) {
            (Fw::Nothing, _) => None,
            (Fw::Odd, 4) => Some((UAR, UAR_LEN)),
            (Fw::Odd, _) => None,
            (_, 0) => Some((DCS, DCS_LEN)),
            (Fw::Edk2, 2) => Some((UAR, UAR_LEN)),
            (Fw::Ami, 1) => Some((UAR, UAR_LEN)),
            (Fw::AmiNoBase, 1) => Some((0, UAR_LEN)),
            _ => None,
        }
    }
}

fn pick(fw: Fw, reg: usize) -> (Pick, Vec<u8>) {
    let mut asked = Vec::new();
    let p = choose(&regs(), reg, sim(fw, &mut asked));
    (p, asked)
}

#[test]
fn candidates_for_connectx3() {
    assert_eq!(bars::candidates(&regs(), 0), [0, 0]);
    assert_eq!(bars::candidates(&regs(), 2), [2, 1]);
    assert_eq!(bars::base(&regs(), 0), DCS);
    assert_eq!(bars::base(&regs(), 2), UAR);
}

#[test]
fn edk2_uar_is_index_2() {
    let (p, _) = pick(Fw::Edk2, 2);
    assert_eq!(p, Pick { index: 2, size: Some(UAR_LEN), how: How::BaseMatches });
}

#[test]
fn ami_uar_is_index_1() {
    let (p, asked) = pick(Fw::Ami, 2);
    assert_eq!(p, Pick { index: 1, size: Some(UAR_LEN), how: How::BaseMatches });
    assert_eq!(asked, [2, 1]);
}

#[test]
fn dcs_is_index_0_on_both() {
    for fw in [Fw::Edk2, Fw::Ami] {
        let (p, asked) = pick(fw, 0);
        assert_eq!(p, Pick { index: 0, size: Some(DCS_LEN), how: How::BaseMatches });
        assert_eq!(asked, [0]);
    }
}

#[test]
fn ami_without_base_takes_the_only_answer() {
    let (p, _) = pick(Fw::AmiNoBase, 2);
    assert_eq!(p, Pick { index: 1, size: Some(UAR_LEN), how: How::OnlyAnswer });
}

#[test]
fn scan_finds_the_base_elsewhere() {
    let (p, asked) = pick(Fw::Odd, 2);
    assert_eq!(p, Pick { index: 4, size: Some(UAR_LEN), how: How::Scan });
    assert_eq!(asked, [2, 1, 0, 3, 4]);
}

#[test]
fn nothing_known_falls_back_to_register_numbering() {
    let (p, _) = pick(Fw::Nothing, 2);
    assert_eq!(p, Pick { index: 2, size: None, how: How::NoAnswer });
}

#[test]
fn bases_of_32bit_and_io_bars() {
    let r = [0xfe00_0000, 0xe001, 0, 0, 0, 0];
    assert_eq!(bars::base(&r, 0), 0xfe00_0000);
    assert_eq!(bars::base(&r, 1), 0xe000);
    assert_eq!(bars::candidates(&r, 1), [1, 1]);
}

#[test]
fn every_choice_is_described() {
    for h in [How::BaseMatches, How::Scan, How::OnlyAnswer, How::NoMatch, How::NoAnswer] {
        assert!(!h.describe().is_empty());
    }
}
