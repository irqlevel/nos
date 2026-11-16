//! `aead`: the kernel's ChaCha20-Poly1305 -- the crypto crate's CRYPTOGAMS
//! assembly, on the path each input lets its sections take of what the
//! host's CPU has: the integer code, SSSE3's ChaCha20, AVX's Poly1305,
//! AVX2's both; arm64's NEON -- held to RustCrypto's chacha20poly1305 on
//! its portable code, which shares nothing with it (the RFC is all the two
//! have in common).
//!
//! An input is a run of messages, each with its key, nonce, associated data
//! and length -- weighted to the lengths where the assembly changes how it
//! works: a block, the 128, 192, 256 and 512 bytes its wider paths start
//! at, the pieces a message is worked through a section at a time -- at an
//! offset into its buffer, in place. Each is sealed by both, the ciphertext
//! and the tag held to the judge's; opened again on another path, back to
//! the plaintext; and opened once more with one bit changed -- of the
//! ciphertext, the associated data, the tag or the nonce -- refused by both,
//! and the buffer left as it was.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use crypto::{ChaCha20Poly1305, KEY_LEN, NONCE_LEN, PIECE, TAG_LEN};

use crate::Input;

/// What an input lets a section use of what the host has, as
/// `kcore::simd`'s bits: nothing (the integer code), x86-64's SSSE3, SSSE3
/// and AVX, and all of it.
const PATHS: [u32; 4] = [
    0,
    kcore::simd::SSE2 | kcore::simd::SSSE3,
    kcore::simd::SSE2 | kcore::simd::SSSE3 | kcore::simd::AVX,
    u32::MAX,
];

/// Where the assembly's work changes: whole and partial blocks, its wider
/// paths' starts, and a piece's edges.
const EDGES: [usize; 22] = [
    0, 1, 15, 16, 17, 63, 64, 65, 127, 128, 129, 191, 192, 193, 255, 256, 257, 511, 512, 513, PIECE, PIECE + 1,
];

/// The longest message: three pieces and some.
const LONGEST: u64 = 3 * PIECE as u64 + 300;

/// Bytes from a seed: an input says a message's length and a seed, not
/// every byte of a message tens of kilobytes long.
fn fill(seed: u64, buf: &mut [u8]) {
    let mut x = seed | 1;
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 24) as u8;
    }
}

pub fn aead(r: &mut Input) {
    while r.more() {
        message(r);
    }
}

fn message(r: &mut Input) {
    let mut key = [0u8; KEY_LEN];
    fill(r.u64(), &mut key);
    let mut nonce = [0u8; NONCE_LEN];
    fill(r.u64(), &mut nonce);
    let mut aad = vec![0u8; r.below(64) as usize];
    fill(r.u64(), &mut aad);
    let len = if r.chance(2) { r.pick(&EDGES) + r.below(3) as usize } else { r.below(LONGEST) as usize };
    let mut plaintext = vec![0u8; len];
    fill(r.u64(), &mut plaintext);
    let offset = r.below(16) as usize;
    let seal_path = r.pick(&PATHS);
    let open_path = r.pick(&PATHS);
    if len > 2 * PIECE {
        crate::common::runner::reached("aead: a message of three pieces");
    }

    let ours = ChaCha20Poly1305::new(&key);
    let judge = chacha20poly1305::ChaCha20Poly1305::new(&key.into());

    /* Sealed by both */
    let mut want = plaintext.clone();
    let want_tag = judge
        .encrypt_in_place_detached(&nonce.into(), &aad, &mut want)
        .expect("RustCrypto sealed nothing");
    let mut buf = vec![0u8; offset + len];
    buf[offset..].copy_from_slice(&plaintext);
    crate::machine::ffi::simd_allow(seal_path);
    let tag = ours.seal_in_place(&nonce, &aad, &mut buf[offset..]);
    invariant!(buf[offset..] == want[..], "{} bytes (at +{}) sealed on paths {:#x} unlike RustCrypto's: first \
               difference at byte {}", len, offset, seal_path,
               buf[offset..].iter().zip(want.iter()).position(|(a, b)| a != b).unwrap_or(len));
    invariant!(tag[..] == want_tag[..], "{} bytes (at +{}) sealed on paths {:#x}: tag {:02x?}, RustCrypto's \
               {:02x?}", len, offset, seal_path, tag, &want_tag[..]);

    /* Opened on another path, back to the plaintext */
    crate::machine::ffi::simd_allow(open_path);
    invariant!(ours.open_in_place(&nonce, &aad, &mut buf[offset..], &tag).is_ok(),
               "{} bytes sealed on paths {:#x} refused on paths {:#x}", len, seal_path, open_path);
    invariant!(buf[offset..] == plaintext[..], "{} bytes sealed on paths {:#x} opened on {:#x} to something else",
               len, seal_path, open_path);

    /* One bit changed: refused, the buffer left as it was */
    buf[offset..].copy_from_slice(&want);
    let mut bad_nonce = nonce;
    let mut bad_aad = aad.clone();
    let mut bad_tag = tag;
    let bit = 1u8 << r.below(8);
    match r.below(4) {
        0 if len > 0 => {
            let at = offset + r.below(len as u64) as usize;
            buf[at] ^= bit;
        }
        1 if !aad.is_empty() => {
            let at = r.below(aad.len() as u64) as usize;
            bad_aad[at] ^= bit;
        }
        3 => {
            let at = r.below(NONCE_LEN as u64) as usize;
            bad_nonce[at] ^= bit;
        }
        _ => {
            let at = r.below(TAG_LEN as u64) as usize;
            bad_tag[at] ^= bit;
        }
    }
    let before = buf.clone();
    invariant!(ours.open_in_place(&bad_nonce, &bad_aad, &mut buf[offset..], &bad_tag).is_err(),
               "{} bytes with a bit changed opened on paths {:#x}", len, open_path);
    invariant!(buf == before, "{} bytes refused on paths {:#x}, and the buffer changed all the same", len, open_path);
    let mut judged = before[offset..].to_vec();
    invariant!(judge
        .decrypt_in_place_detached(&bad_nonce.into(), &bad_aad, &mut judged, &bad_tag.into())
        .is_err(), "RustCrypto took {} bytes with a bit changed: the change was no change", len);
    crate::common::runner::reached("aead: a message sealed, opened and refused");
}
