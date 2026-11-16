//! The boot's check of the assembly: RFC 8439's known answers on every
//! code path this CPU has, and each path against the integer one -- over
//! the lengths where the assembly changes how it works (a block, the 128,
//! 192, 256 and 512 bytes its wider paths start at, a piece), at every
//! alignment, in place -- and a tag that is not the message's refused with
//! the message left as it was. The host's fuzzer holds the same code to
//! RustCrypto's, which shares nothing with it (tests/fuzz/net).

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use crate::{BadTag, ChaCha20Poly1305, Paths, NONCE_LEN, PIECE, TAG_LEN};
use kcore::klog;

/// RFC 8439 2.4.2's and 2.8.2's plaintext.
const SUNSCREEN: &[u8] = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";

/// RFC 8439 2.8.2: the AEAD's known answer.
const AEAD_KEY: [u8; 32] = [
    0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f,
    0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f,
];
const AEAD_NONCE: [u8; NONCE_LEN] = [0x07, 0x00, 0x00, 0x00, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47];
const AEAD_AAD: [u8; 12] = [0x50, 0x51, 0x52, 0x53, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7];
const AEAD_CIPHERTEXT: [u8; 114] = [
    0xd3, 0x1a, 0x8d, 0x34, 0x64, 0x8e, 0x60, 0xdb, 0x7b, 0x86, 0xaf, 0xbc, 0x53, 0xef, 0x7e, 0xc2,
    0xa4, 0xad, 0xed, 0x51, 0x29, 0x6e, 0x08, 0xfe, 0xa9, 0xe2, 0xb5, 0xa7, 0x36, 0xee, 0x62, 0xd6,
    0x3d, 0xbe, 0xa4, 0x5e, 0x8c, 0xa9, 0x67, 0x12, 0x82, 0xfa, 0xfb, 0x69, 0xda, 0x92, 0x72, 0x8b,
    0x1a, 0x71, 0xde, 0x0a, 0x9e, 0x06, 0x0b, 0x29, 0x05, 0xd6, 0xa5, 0xb6, 0x7e, 0xcd, 0x3b, 0x36,
    0x92, 0xdd, 0xbd, 0x7f, 0x2d, 0x77, 0x8b, 0x8c, 0x98, 0x03, 0xae, 0xe3, 0x28, 0x09, 0x1b, 0x58,
    0xfa, 0xb3, 0x24, 0xe4, 0xfa, 0xd6, 0x75, 0x94, 0x55, 0x85, 0x80, 0x8b, 0x48, 0x31, 0xd7, 0xbc,
    0x3f, 0xf4, 0xde, 0xf0, 0x8e, 0x4b, 0x7a, 0x9d, 0xe5, 0x76, 0xd2, 0x65, 0x86, 0xce, 0xc6, 0x4b,
    0x61, 0x16,
];
const AEAD_TAG: [u8; TAG_LEN] = [
    0x1a, 0xe1, 0x0b, 0x59, 0x4f, 0x09, 0xe2, 0x6a, 0x7e, 0x90, 0x2e, 0xcb, 0xd0, 0x60, 0x06, 0x91,
];

/// RFC 8439 2.4.2: ChaCha20 alone, from block 1.
const STREAM_KEY: [u8; 32] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
];
const STREAM_NONCE: [u8; NONCE_LEN] = [0, 0, 0, 0, 0, 0, 0, 0x4a, 0, 0, 0, 0];
const STREAM_CIPHERTEXT: [u8; 114] = [
    0x6e, 0x2e, 0x35, 0x9a, 0x25, 0x68, 0xf9, 0x80, 0x41, 0xba, 0x07, 0x28, 0xdd, 0x0d, 0x69, 0x81,
    0xe9, 0x7e, 0x7a, 0xec, 0x1d, 0x43, 0x60, 0xc2, 0x0a, 0x27, 0xaf, 0xcc, 0xfd, 0x9f, 0xae, 0x0b,
    0xf9, 0x1b, 0x65, 0xc5, 0x52, 0x47, 0x33, 0xab, 0x8f, 0x59, 0x3d, 0xab, 0xcd, 0x62, 0xb3, 0x57,
    0x16, 0x39, 0xd6, 0x24, 0xe6, 0x51, 0x52, 0xab, 0x8f, 0x53, 0x0c, 0x35, 0x9f, 0x08, 0x61, 0xd8,
    0x07, 0xca, 0x0d, 0xbf, 0x50, 0x0d, 0x6a, 0x61, 0x56, 0xa3, 0x8e, 0x08, 0x8a, 0x22, 0xb6, 0x5e,
    0x52, 0xbc, 0x51, 0x4d, 0x16, 0xcc, 0xf8, 0x06, 0x81, 0x8c, 0xe9, 0x1a, 0xb7, 0x79, 0x37, 0x36,
    0x5a, 0xf9, 0x0b, 0xbf, 0x74, 0xa3, 0x5b, 0xe6, 0xb4, 0x0b, 0x8e, 0xed, 0xf2, 0x78, 0x5e, 0x42,
    0x87, 0x4d,
];

/// The paths to drive, as masks of what a section may use: the integer
/// code; then, where the CPU has them, x86-64's SSSE3 ChaCha20 with integer
/// Poly1305, the same with AVX Poly1305, and AVX2's both -- arm64's NEON.
#[cfg(target_arch = "x86_64")]
const MASKS: [u32; 4] = [
    0,
    kcore::simd::SSE2 | kcore::simd::SSSE3,
    kcore::simd::SSE2 | kcore::simd::SSSE3 | kcore::simd::AVX,
    u32::MAX,
];
#[cfg(target_arch = "aarch64")]
const MASKS: [u32; 2] = [0, u32::MAX];

/// The lengths the paths are held to each other over.
const LENGTHS: [usize; 30] = [
    0, 1, 15, 16, 17, 63, 64, 65, 127, 128, 129, 191, 192, 193, 255, 256, 257, 383, 384, 511, 512,
    513, 1023, 1024, 4095, 4097, PIECE - 1, PIECE, PIECE + 1, 2 * PIECE + 129,
];

/// Inputs from a seed: the boot's random pool's where it has one, so that
/// boots differ, and said in the failure so that one can be run again.
struct Inputs(u64);

impl Inputs {
    fn next(&mut self) -> u64 {
        /* xorshift64*: nothing secret, only bytes that differ */
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for b in buf.iter_mut() {
            *b = self.next() as u8;
        }
    }
}

pub fn selftest() {
    for &mask in MASKS.iter() {
        known_answers(mask);
    }

    let mut seed = [0u8; 8];
    let _ = kcore::random::fill_random(&mut seed);
    let seed = u64::from_le_bytes(seed) | 1;
    let mut inputs = Inputs(seed);
    for &len in LENGTHS.iter() {
        for misalign in [0usize, 1, 7] {
            paths_agree(&mut inputs, seed, len, misalign);
        }
    }

    klog!(0, "crypto: ChaCha20-Poly1305 on CRYPTOGAMS' {} code", crate::code());
}

fn known_answers(mask: u32) {
    let paths = Paths::Only(mask);

    let aead = ChaCha20Poly1305::new(&AEAD_KEY);
    let mut data = [0u8; 114];
    data.copy_from_slice(SUNSCREEN);
    let tag = aead.seal(paths, &AEAD_NONCE, &AEAD_AAD, &mut data);
    assert!(data == AEAD_CIPHERTEXT && tag == AEAD_TAG, "crypto selftest: RFC 8439 2.8.2 sealed wrong, paths {:#x}", mask);
    assert!(aead.open(paths, &AEAD_NONCE, &AEAD_AAD, &mut data, &AEAD_TAG).is_ok() && data[..] == *SUNSCREEN,
        "crypto selftest: RFC 8439 2.8.2 opened wrong, paths {:#x}", mask);

    /* ChaCha20 alone: the AEAD's data starts at block 1, as 2.4.2's
     * encryption does, so its ciphertext is 2.4.2's. */
    let stream = ChaCha20Poly1305::new(&STREAM_KEY);
    data.copy_from_slice(SUNSCREEN);
    let _ = stream.seal(paths, &STREAM_NONCE, &[], &mut data);
    assert!(data == STREAM_CIPHERTEXT, "crypto selftest: RFC 8439 2.4.2's key stream wrong, paths {:#x}", mask);
}

fn paths_agree(inputs: &mut Inputs, seed: u64, len: usize, misalign: usize) {
    let mut key = [0u8; 32];
    let mut nonce = [0u8; NONCE_LEN];
    let mut aad = [0u8; 37];
    inputs.fill(&mut key);
    inputs.fill(&mut nonce);
    inputs.fill(&mut aad);
    let aad = &aad[..(inputs.next() as usize) % 38];
    let aead = ChaCha20Poly1305::new(&key);

    let mut plaintext = vec![0u8; len];
    inputs.fill(&mut plaintext);

    let mut reference = plaintext.clone();
    let reference_tag = aead.seal(Paths::Only(0), &nonce, aad, &mut reference);

    let mut storage: Vec<u8> = vec![0u8; len + misalign];
    for &mask in MASKS.iter() {
        let data = &mut storage[misalign..];
        data.copy_from_slice(&plaintext);
        let tag = aead.seal(Paths::Only(mask), &nonce, aad, data);
        assert!(*data == reference[..] && tag == reference_tag,
            "crypto selftest: paths {:#x} seal {} bytes (at +{}) unlike the integer code's, seed {:#x}", mask, len, misalign, seed);

        /* Opened by another path than the one that sealed it */
        let other = MASKS[(MASKS.iter().position(|&m| m == mask).unwrap_or(0) + 1) % MASKS.len()];
        assert!(aead.open(Paths::Only(other), &nonce, aad, data, &tag).is_ok() && *data == plaintext[..],
            "crypto selftest: paths {:#x} open {} bytes (at +{}) wrong, seed {:#x}", other, len, misalign, seed);
    }

    /* A tag that is not the message's: refused, the message untouched */
    let data = &mut storage[misalign..];
    data.copy_from_slice(&reference);
    let mut forged = reference_tag;
    forged[(inputs.next() as usize) % TAG_LEN] ^= 1u8 << (inputs.next() % 8);
    for &mask in MASKS.iter() {
        assert!(aead.open(Paths::Only(mask), &nonce, aad, data, &forged) == Err(BadTag) && *data == reference[..],
            "crypto selftest: paths {:#x} took a forged tag over {} bytes, seed {:#x}", mask, len, seed);
    }
}
