//! The hypervisor's MMIO decoder and page walker, checked on the host: the
//! decoder against the encodings clang gives for every instruction form
//! scripts/insn-test.py lists (the cases file, its first argument), each also
//! cut short at every length, and against random bytes; the walker against
//! page tables built here by hand. The two files are compiled as they are in
//! the hypervisor -- they are plain code over slices.
#![allow(dead_code)]
#[path = "../../../src/rust/hv/src/insn.rs"]
mod insn;
#[path = "../../../src/rust/hv/src/walk.rs"]
mod walk;

use insn::{decode, Access, Extend, Insn, Mode, Reg};
use std::collections::HashMap;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn expected(e: &str, len: usize) -> Option<Insn> {
    let f: Vec<&str> = e.split_whitespace().collect();
    let n = |i: usize| f[i].parse::<u64>().unwrap();
    let access = match f[0] {
        "-" => return None,
        "L" => Access::Load {
            size: n(1) as u8,
            dest_size: n(2) as u8,
            extend: match f[3] { "N" => Extend::None, "Z" => Extend::Zero, _ => Extend::Sign },
            reg: Reg { num: n(4) as u8, high_byte: n(5) == 1 },
        },
        "S" => Access::Store { size: n(1) as u8, reg: Reg { num: n(2) as u8, high_byte: n(3) == 1 } },
        "I" => Access::StoreImm { size: n(1) as u8, value: n(2) },
        _ => panic!("bad expectation {}", e),
    };
    Some(Insn { access, len: len as u8 })
}

fn decoder() {
    let path = std::env::args().nth(1).expect("the cases file");
    let text = std::fs::read_to_string(path).unwrap();
    let (mut ok, mut bad) = (0, 0);
    for line in text.lines() {
        let parts: Vec<&str> = line.splitn(3, '|').collect();
        let bytes = hex(parts[0]);
        let want = expected(parts[1], bytes.len());
        /* The instruction with bytes of the next one after it, as memory has
         * them: the decoder must stop at its own end. */
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0x90; 8]);
        let got = decode(&padded, Mode::Long);
        if got == want {
            ok += 1;
        } else {
            bad += 1;
            println!("MISMATCH {}: {} -> got {:?}, want {:?}", parts[2], parts[0], got, want);
        }
        /* Cut short anywhere, it is refused -- never a shorter instruction. */
        if want.is_some() {
            for cut in 0..bytes.len() {
                if let Some(g) = decode(&bytes[..cut], Mode::Long) {
                    bad += 1;
                    println!("TRUNCATED {} at {}: {:?}", parts[2], cut, g);
                }
            }
        }
    }
    /* Random bytes: no panic, and never a length past what was there. */
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut decoded = 0;
    for _ in 0..2_000_000 {
        let mut b = [0u8; 15];
        for v in b.iter_mut() {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            *v = x as u8;
        }
        let n = (x >> 40) as usize % 16;
        for mode in [Mode::Long, Mode::Protected32] {
            if let Some(i) = decode(&b[..n], mode) {
                decoded += 1;
                assert!(i.len as usize <= n && i.len >= 2, "len {} of {}", i.len, n);
            }
        }
    }
    println!("decoder: {} reference cases right, {} wrong; random: {} decoded, none past its bytes", ok, bad, decoded);
    assert_eq!(bad, 0);
}

fn walker() {
    use walk::*;
    let mut mem: HashMap<u64, u64> = HashMap::new();
    let p = |v: u64| v | 1;
    /* PML4 at 0x1000; PDPT 0x2000; PD 0x3000; PT 0x4000. */
    let la4k: u64 = 0xffff_8000_0012_3456; // PML4 256, PDPT 0, PD 0, PT 0x123 (>>12 & 511)
    let pml4 = (la4k >> 39) & 511;
    let pdpt = (la4k >> 30) & 511;
    let pd = (la4k >> 21) & 511;
    let pt = (la4k >> 12) & 511;
    mem.insert(0x1000 + pml4 * 8, p(0x2000));
    mem.insert(0x2000 + pdpt * 8, p(0x3000));
    mem.insert(0x3000 + pd * 8, p(0x4000));
    mem.insert(0x4000 + pt * 8, p(0x7777_7000) | (1 << 63)); // NX bit set: not part of the address
    /* A 2 MiB page and a 1 GiB page beside it. */
    let la2m: u64 = 0xffff_8000_0040_1234; // PD index 2
    mem.insert(0x3000 + ((la2m >> 21) & 511) * 8, p(0x1_2340_0000) | (1 << 7));
    let la1g: u64 = 0xffff_8000_4000_5678; // PDPT index 1
    mem.insert(0x2000 + ((la1g >> 30) & 511) * 8, p(0x8_4000_0000) | (1 << 7));
    let read = |a: u64| Some(*mem.get(&a).unwrap_or(&0));
    let lm = Paging { cr0: 1 << 31 | 1, cr3: 0x1000 | 0x5 /* a PCID */, cr4: 1 << 5, efer: 1 << 10 | 1 << 8 };
    assert_eq!(translate(&lm, la4k, read), Ok(0x7777_7456));
    assert_eq!(translate(&lm, la2m, read), Ok(0x1_2340_0000 | (la2m & 0x1F_FFFF)));
    assert_eq!(translate(&lm, la1g, read), Ok(0x8_4000_0000 | (la1g & 0x3FFF_FFFF)));
    assert_eq!(translate(&lm, 0xffff_8000_0012_4000, read), Err(Miss::NotMapped));
    assert_eq!(translate(&lm, 0x0000_8000_0000_0000, read), Err(Miss::NonCanonical));
    assert_eq!(translate(&lm, 0xffff_0000_0000_0000, read), Err(Miss::NonCanonical));
    let off = Paging { cr0: 1, cr3: 0, cr4: 0, efer: 0 };
    assert_eq!(translate(&off, 0x7c00, read), Ok(0x7c00));
    let pae32 = Paging { cr0: 1 << 31 | 1, cr3: 0x1000, cr4: 1 << 5, efer: 0 };
    assert_eq!(translate(&pae32, 0x1000, read), Err(Miss::Unsupported));
    let nopae = Paging { cr0: 1 << 31 | 1, cr3: 0x1000, cr4: 0, efer: 1 << 10 };
    assert_eq!(translate(&nopae, 0x1000, read), Err(Miss::Unsupported));
    /* Five levels: one more table above. */
    let mut mem5 = mem.clone();
    let la57: u64 = 0xff01_0000_0012_3456; // PML5 index 0x1fe (bits 56:48 of 0xff01...)
    let pml5 = (la57 >> 48) & 511;
    mem5.insert(0x9000 + pml5 * 8, p(0x1000));
    let pml4b = (la57 >> 39) & 511;
    mem5.insert(0x1000 + pml4b * 8, p(0x2000));
    let read5 = |a: u64| Some(*mem5.get(&a).unwrap_or(&0));
    let l5 = Paging { cr0: 1 << 31 | 1, cr3: 0x9000, cr4: 1 << 5 | 1 << 12, efer: 1 << 10 };
    assert_eq!(translate(&l5, la57, read5), Ok(0x7777_7456));
    assert_eq!(translate(&l5, 0x0100_0000_0000_0000, read5), Err(Miss::NonCanonical));
    /* A fetch across a page boundary into a page mapped elsewhere, and one
     * whose second page is not mapped. */
    let mut mem2 = mem.clone();
    let near_end = 0xffff_8000_0012_3ffc; // 4 bytes before the page's end
    mem2.insert(0x4000 + (((near_end + 4) >> 12) & 511) * 8, p(0x5555_5000));
    let read2 = |a: u64| Some(*mem2.get(&a).unwrap_or(&0));
    let bytes_at = |gpa: u64, buf: &mut [u8]| {
        for (i, v) in buf.iter_mut().enumerate() { *v = ((gpa + i as u64) >> 12) as u8 ^ (gpa + i as u64) as u8; }
        true
    };
    let mut buf = [0u8; 15];
    let n = fetch(&lm, near_end, &mut buf, read2, bytes_at).unwrap();
    assert_eq!(n, 15);
    let want: Vec<u8> = (0..15u64).map(|i| {
        let gpa = if i < 4 { 0x7777_7ffc + i } else { 0x5555_5000 + i - 4 };
        (gpa >> 12) as u8 ^ gpa as u8
    }).collect();
    assert_eq!(&buf[..], &want[..]);
    let n = fetch(&lm, near_end, &mut buf, read, bytes_at).unwrap();
    assert_eq!(n, 4, "stops where the next page is not mapped");
    assert_eq!(fetch(&lm, 0xffff_8000_0012_4000, &mut buf, read, bytes_at), Err(Miss::NotMapped));
    println!("walker: all checks pass");
}

fn main() {
    decoder();
    walker();
}
