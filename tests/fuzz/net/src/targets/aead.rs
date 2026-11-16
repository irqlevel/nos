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
//!
//! Or, as sshd's cipher takes them, the two halves on their own: ChaCha20
//! from a counter block of four words of any value -- the block count low,
//! or as high as the judge goes, its last block 2^32 - 2 (RustCrypto's
//! ChaCha20 makes no block 2^32 - 1; every boot's `crypto::selftest` holds
//! the paths to each other up to it) -- held to RustCrypto's chacha20, and
//! Poly1305 over what that gave, its last block padded as RFC 8439 2.5 pads
//! it, held to RustCrypto's poly1305.

use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
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
        /* A third of them the halves on their own */
        if r.below(3) == 0 {
            halves(r);
        } else {
            message(r);
        }
    }
}

/// A message's length: one where the assembly changes its way -- half of
/// them -- or any.
fn length(r: &mut Input) -> usize {
    if r.bool() {
        r.pick(&EDGES) + r.below(3) as usize
    } else {
        r.below(LONGEST) as usize
    }
}

fn message(r: &mut Input) {
    let mut key = [0u8; KEY_LEN];
    fill(r.u64(), &mut key);
    let mut nonce = [0u8; NONCE_LEN];
    fill(r.u64(), &mut nonce);
    let mut aad = vec![0u8; r.below(64) as usize];
    fill(r.u64(), &mut aad);
    let len = length(r);
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

fn halves(r: &mut Input) {
    let mut key = [0u8; KEY_LEN];
    fill(r.u64(), &mut key);
    let len = length(r);
    let mut data = vec![0u8; len];
    fill(r.u64(), &mut data);
    let offset = r.below(16) as usize;
    let blocks = (len as u64).div_ceil(64);
    let first = if r.bool() { (u64::from(u32::MAX) - blocks) as u32 } else { r.u32() >> 8 };
    let counter = [first, r.u32(), r.u32(), r.u32()];
    let path = r.pick(&PATHS);

    /* The judge's: RFC 8439's ChaCha20 -- the counter block's first word
     * the block, the other three the nonce -- at that block */
    let mut nonce = [0u8; NONCE_LEN];
    for (i, word) in counter[1..].iter().enumerate() {
        nonce[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
    let mut want = data.clone();
    let mut stream = chacha20::ChaCha20::new(&key.into(), &nonce.into());
    stream.seek(u64::from(first) * 64);
    stream.apply_keystream(&mut want);

    let mut buf = vec![0u8; offset + len];
    buf[offset..].copy_from_slice(&data);
    crate::machine::ffi::simd_allow(path);
    crypto::chacha20(&key, &counter, &mut buf[offset..]);
    invariant!(buf[offset..] == want[..], "{} bytes (at +{}) from block {:#x} on paths {:#x} unlike RustCrypto's \
               ChaCha20: first difference at byte {}", len, offset, first, path,
               buf[offset..].iter().zip(want.iter()).position(|(a, b)| a != b).unwrap_or(len));

    let tag = crypto::poly1305(&key, &buf[offset..]);
    let want_tag = poly1305::Poly1305::new(&key.into()).compute_unpadded(&want);
    invariant!(tag[..] == want_tag[..], "Poly1305 over {} bytes (at +{}) on paths {:#x}: {:02x?}, RustCrypto's \
               {:02x?}", len, offset, path, tag, &want_tag[..]);
    crate::common::runner::reached("aead: ChaCha20 and Poly1305 on their own");
}
