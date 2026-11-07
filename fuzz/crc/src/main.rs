//! The tree's CRCs (src/crates/common/crc) on the host, run by
//! scripts/crc-test.py:
//!
//! - `all --seed N --cases M`: the known answers -- each CRC's check value
//!   and RFC 3720's CRC-32C vectors -- then `M` random messages, each CRC
//!   held to the CRC computed from its definition: the polynomial as it is
//!   written, not reflected, divided into the message a bit at a time, most
//!   significant first, with the bytes and the register reflected around it
//!   -- nothing shared with the crate's reflected tables but the answer.
//!   Every form (`raw` from any register, `update` from any CRC, `checksum`)
//!   whole and carried on across a split, and `with_hole` against the block
//!   with its field zeroed;
//! - `zlib <cases>`: messages the script made, each with zlib's CRC-32 of
//!   it, held to `checksum` and to `update` fed in random pieces.
//!
//! Exit code 0 = every check passed; a failure says which and with what.

use std::process::ExitCode;

/// xorshift64*: the messages' bytes, their lengths and their splits.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// `v`'s low `width` bits in the opposite order.
fn reflect(v: u64, width: u32) -> u64 {
    (0..width).filter(|i| v >> i & 1 != 0).fold(0, |r, i| r | 1 << (width - 1 - i))
}

/// The register a reflected CRC of `width` bits holds after `data`, from
/// `reg` -- computed as the definition has it: the register in the
/// polynomial's own (unreflected) order, each byte's least significant bit
/// first into its top, a division step a bit.
fn by_definition(width: u32, poly: u64, reg: u64, data: &[u8]) -> u64 {
    let mask = (1u64 << width) - 1;
    let top = 1u64 << (width - 1);
    let mut r = reflect(reg, width);
    for &b in data {
        r ^= reflect(u64::from(b), 8) << (width - 8);
        for _ in 0..8 {
            r = if r & top != 0 { ((r << 1) ^ poly) & mask } else { (r << 1) & mask };
        }
    }
    reflect(r, width)
}

/// The polynomials as they are written.
const CRC32_POLY: u64 = 0x04C1_1DB7;
const CRC32C_POLY: u64 = 0x1EDC_6F41;
const CRC16_POLY: u64 = 0x8005;

struct Checks {
    failed: u64,
    passed: u64,
}

impl Checks {
    fn check(&mut self, ok: bool, what: impl FnOnce() -> String) {
        if ok {
            self.passed += 1;
        } else {
            self.failed += 1;
            if self.failed <= 20 {
                println!("FAIL: {}", what());
            }
        }
    }
}

fn known(c: &mut Checks) {
    const CHECK: &[u8] = b"123456789";
    let fox: &[u8] = b"The quick brown fox jumps over the lazy dog";
    c.check(crc::crc32::checksum(CHECK) == 0xCBF4_3926, || "CRC-32's check value".into());
    c.check(crc::crc32::checksum(fox) == 0x414F_A339, || "CRC-32 of the quick brown fox".into());
    c.check(crc::crc32::checksum(&[]) == 0, || "CRC-32 of nothing".into());
    c.check(crc::crc32c::checksum(CHECK) == 0xE306_9283, || "CRC-32C's check value".into());
    c.check(crc::crc32c::checksum(&[]) == 0, || "CRC-32C of nothing".into());
    /* CRC-16/ARC: the register from 0, no inversion at either end */
    c.check(crc::crc16::raw(0, CHECK) == 0xBB3D, || "CRC-16/ARC's check value".into());
    /* CRC-16/MODBUS: the same register from !0, as gdt_csum starts it */
    c.check(crc::crc16::raw(0xFFFF, CHECK) == 0x4B37, || "CRC-16/MODBUS's check value".into());
    /* ext4's form: the register itself, `!raw(!0, m)` the message's CRC */
    c.check(!crc::crc32c::raw(!0, CHECK) == 0xE306_9283, || "CRC-32C's register form".into());

    /* RFC 3720, B.4 */
    let up: Vec<u8> = (0u8..32).collect();
    let down: Vec<u8> = (0u8..32).rev().collect();
    let cases: [(&[u8], u32); 4] =
        [(&[0u8; 32], 0x8A91_36AA), (&[0xFFu8; 32], 0x62A8_AB43), (&up, 0x46DD_794E), (&down, 0x113F_DB5C)];
    for (i, (bytes, want)) in cases.iter().enumerate() {
        let got = crc::crc32c::checksum(bytes);
        c.check(got == *want, || format!("RFC 3720's vector {}: {:08x}, not {:08x}", i, got, want));
    }

    /* The references themselves, against the same values: a reference that
     * is wrong would pass the crate wrong in the same way */
    c.check(by_definition(32, CRC32_POLY, !0, CHECK) as u32 == !0xCBF4_3926u32, || "the CRC-32 reference".into());
    c.check(by_definition(32, CRC32C_POLY, !0, CHECK) as u32 == !0xE306_9283u32, || "the CRC-32C reference".into());
    c.check(by_definition(16, CRC16_POLY, 0, CHECK) == 0xBB3D, || "the CRC-16 reference".into());

    /* with_hole: a field that is not inside the block, refused */
    let block = [0x5Au8; 64];
    c.check(crc::crc32::with_hole(&block, 60).is_some(), || "with_hole of the last four bytes".into());
    c.check(crc::crc32::with_hole(&block, 61).is_none(), || "with_hole of a field past the end".into());
    c.check(crc::crc32::with_hole(&block, usize::MAX - 1).is_none(), || "with_hole of a field past usize".into());
    c.check(crc::crc32::with_hole(&[], 0).is_none(), || "with_hole of an empty block".into());
}

/// A message's length: mostly short, around the eight-byte steps, some long.
fn length(r: &mut Rng) -> usize {
    match r.below(10) {
        0..=5 => r.below(40) as usize,
        6 | 7 => (8 * r.below(64) + r.below(3)) as usize,
        _ => r.below(9000) as usize,
    }
}

fn random(c: &mut Checks, seed: u64, cases: u64) {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    for case in 0..cases {
        /* A slice of a larger buffer, at an offset of any alignment */
        let len = length(&mut r);
        let off = r.below(8) as usize;
        let buf = r.bytes(off + len);
        let d = &buf[off..];
        let split = r.below(len as u64 + 1) as usize;
        let reg = r.next() as u32;
        let reg16 = r.next() as u16;
        let what = |name: &str| format!("case {} (seed {}): {} over {} bytes, register {:08x}", case, seed, name, len, reg);

        let def = by_definition(32, CRC32_POLY, u64::from(reg), d) as u32;
        c.check(crc::crc32::raw(reg, d) == def, || what("CRC-32 raw"));
        let def_update = !(by_definition(32, CRC32_POLY, u64::from(!reg), d) as u32);
        c.check(crc::crc32::update(reg, d) == def_update, || what("CRC-32 update"));
        c.check(crc::crc32::update(crc::crc32::update(reg, &d[..split]), &d[split..]) == def_update,
                || what("CRC-32 update across a split"));
        c.check(crc::crc32::checksum(d) == crc::crc32::update(0, d), || what("CRC-32 checksum"));

        let def = by_definition(32, CRC32C_POLY, u64::from(reg), d) as u32;
        c.check(crc::crc32c::raw(reg, d) == def, || what("CRC-32C raw"));
        c.check(crc::crc32c::raw(crc::crc32c::raw(reg, &d[..split]), &d[split..]) == def,
                || what("CRC-32C raw across a split"));
        let def_update = !(by_definition(32, CRC32C_POLY, u64::from(!reg), d) as u32);
        c.check(crc::crc32c::update(reg, d) == def_update, || what("CRC-32C update"));
        c.check(crc::crc32c::update(crc::crc32c::update(reg, &d[..split]), &d[split..]) == def_update,
                || what("CRC-32C update across a split"));
        c.check(crc::crc32c::checksum(d) == crc::crc32c::update(0, d), || what("CRC-32C checksum"));

        let def = by_definition(16, CRC16_POLY, u64::from(reg16), d) as u16;
        c.check(crc::crc16::raw(reg16, d) == def, || what("CRC-16 raw"));
        c.check(crc::crc16::raw(crc::crc16::raw(reg16, &d[..split]), &d[split..]) == def,
                || what("CRC-16 raw across a split"));

        /* with_hole against the block with its field zeroed */
        if len >= 4 {
            let hole = r.below(len as u64 - 3) as usize;
            let mut zeroed = d.to_vec();
            zeroed[hole..hole + 4].fill(0);
            c.check(crc::crc32::with_hole(d, hole) == Some(crc::crc32::checksum(&zeroed)),
                    || what(&format!("with_hole at {}", hole)));
        }
    }
}

/// The script's cases: a u32 length, the bytes, zlib's CRC-32 of them.
fn zlib(c: &mut Checks, path: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
    let mut r = Rng(0x5EED);
    let mut pos = 0;
    let mut case = 0;
    let word = |at: usize| -> Result<u32, String> {
        bytes.get(at..at + 4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).ok_or("a case cut short".into())
    };
    while pos < bytes.len() {
        let len = word(pos)? as usize;
        let data = bytes.get(pos + 4..pos + 4 + len).ok_or("a case cut short")?;
        let want = word(pos + 4 + len)?;
        pos += 8 + len;

        let got = crc::crc32::checksum(data);
        c.check(got == want, || format!("zlib case {}: {:08x} over {} bytes, zlib's {:08x}", case, got, len, want));
        /* Fed in random pieces, as the command feeds a file */
        let mut crc = 0;
        let mut at = 0;
        while at < len {
            let take = (1 + r.below(70_000) as usize).min(len - at);
            crc = crc::crc32::update(crc, &data[at..at + take]);
            at += take;
        }
        c.check(crc == want, || format!("zlib case {} in pieces: {:08x}, zlib's {:08x}", case, crc, want));
        case += 1;
    }
    println!("{} zlib cases", case);
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut c = Checks { failed: 0, passed: 0 };
    match args.get(1).map(String::as_str) {
        Some("all") => {
            let mut seed = 1;
            let mut cases = 20_000;
            let mut i = 2;
            while i + 1 < args.len() {
                let v: u64 = match args[i + 1].parse() {
                    Ok(v) => v,
                    Err(_) => {
                        println!("crc-test: {} is not a number", args[i + 1]);
                        return ExitCode::from(2);
                    }
                };
                match args[i].as_str() {
                    "--seed" => seed = v,
                    "--cases" => cases = v,
                    other => {
                        println!("crc-test: what is {}?", other);
                        return ExitCode::from(2);
                    }
                }
                i += 2;
            }
            known(&mut c);
            random(&mut c, seed, cases);
        }
        Some("zlib") if args.len() == 3 => {
            if let Err(e) = zlib(&mut c, &args[2]) {
                println!("crc-test: {}", e);
                return ExitCode::from(2);
            }
        }
        _ => {
            println!("usage: crc-test all [--seed N] [--cases M] | crc-test zlib <cases>");
            return ExitCode::from(2);
        }
    }
    println!("{} checks passed, {} failed", c.passed, c.failed);
    if c.failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
