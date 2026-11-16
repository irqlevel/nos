#![no_std]

//! ChaCha20-Poly1305, RFC 8439's AEAD, in CRYPTOGAMS' assembly: x86-64's
//! SSSE3 and AVX2 code and arm64's Advanced SIMD code in the kernel's SIMD
//! sections, and each architecture's integer code where no section can be
//! had -- the same bytes either way (`asm`). What TLS's ChaCha20-Poly1305
//! suites run on in the kernel (the tls crate), and vnet's tunnel between
//! hosts (hv.ko); and its two halves on their own, [`chacha20`] from any
//! counter block and [`poly1305`], for what puts them together otherwise --
//! OpenSSH's chacha20-poly1305, which sshd speaks.
//!
//! A message is worked through in pieces of [`PIECE`] bytes, a section a
//! piece, so that preemption is never off for longer than one piece takes:
//! some microseconds.

mod asm;
mod selftest;

pub use selftest::selftest;

use asm::{Counter, Key, Poly1305, Unit};
use kcore::simd::Section;
use zeroize::Zeroize;

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

/// How much of a message one section works through: a TLS record's
/// plaintext at most, and a whole number of ChaCha20's blocks and of
/// Poly1305's.
pub const PIECE: usize = 16 * 1024;

/// The most a message may be: the 32-bit block counter, which starts at 1,
/// runs out after this many bytes (RFC 8439 2.8).
pub const MESSAGE_MAX: u64 = ((1u64 << 32) - 1) * 64;

/// Which of CRYPTOGAMS' code a section runs here: what every CPU's sections
/// may use, named -- "AVX2", "SSSE3 and AVX", "SSSE3", "NEON" -- or
/// "integer" where they may use nothing.
pub fn code() -> &'static str {
    let features = kcore::simd::features();
    #[cfg(target_arch = "x86_64")]
    {
        if features & kcore::simd::AVX2 != 0 {
            return "AVX2";
        }
        if features & kcore::simd::AVX != 0 {
            return "SSSE3 and AVX";
        }
        if features & kcore::simd::SSSE3 != 0 {
            return "SSSE3";
        }
    }
    #[cfg(target_arch = "aarch64")]
    if features & kcore::simd::NEON != 0 {
        return "NEON";
    }
    "integer"
}

/// What [`ChaCha20Poly1305::open_in_place`] says of a message whose tag is
/// not its own: forged, damaged, or under another key or nonce.
#[derive(Debug, PartialEq, Eq)]
pub struct BadTag;

/// A key, for any number of messages each under a nonce of its own; wiped
/// when dropped.
pub struct ChaCha20Poly1305 {
    key: Key,
}

/// How the pieces of a message are worked through: each with the best path
/// a section of its own gives, or -- the self-test's other half -- every
/// one with the paths `mask` leaves of that, 0 for the integer code alone.
#[derive(Clone, Copy)]
enum Paths {
    Best,
    Only(u32),
}

impl Paths {
    /// `work` with this CPU's vector registers where they can be had.
    fn run<R>(self, work: impl FnOnce(&Unit) -> R) -> R {
        if let Paths::Only(0) = self {
            return work(&Unit::INTEGER);
        }
        match Section::begin() {
            Some(section) => {
                let unit = match self {
                    Paths::Best => Unit::of(&section),
                    Paths::Only(mask) => Unit::of_only(&section, mask),
                };
                work(&unit)
            }
            None => work(&Unit::INTEGER),
        }
    }
}

/// A key as the assembly reads it: eight little-endian words.
fn key_words(key: &[u8; KEY_LEN]) -> Key {
    let mut words = [0u32; 8];
    for (i, word) in words.iter_mut().enumerate() {
        *word = u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    let key = Key(words);
    words.zeroize();
    key
}

/// `data` XORed in place with ChaCha20's key stream under `key`, from the
/// counter block `counter` on: the state's last four words -- RFC 8439's 32-bit
/// block counter and the nonce's three words, or the original layout's
/// 64-bit counter and 64-bit nonce, two words each, which OpenSSH's
/// chacha20-poly1305 keeps. The first word counts the blocks; it is not to
/// run past 2^32 within `data` -- a panic before a byte is touched where it
/// would, since the key stream would repeat. Worked through a section a
/// [`PIECE`].
pub fn chacha20(key: &[u8; KEY_LEN], counter: &[u32; 4], data: &mut [u8]) {
    chacha20_with(Paths::Best, key, counter, data)
}

fn chacha20_with(paths: Paths, key: &[u8; KEY_LEN], counter: &[u32; 4], data: &mut [u8]) {
    let blocks = (data.len() as u64).div_ceil(64);
    assert!(u64::from(counter[0]) + blocks <= 1u64 << 32, "chacha20: a block counter past 2^32");
    let key = key_words(key);
    let mut done = 0;
    while done < data.len() {
        let end = data.len().min(done + PIECE);
        let at = Counter([counter[0] + (done / 64) as u32, counter[1], counter[2], counter[3]]);
        let piece = &mut data[done..end];
        paths.run(|unit| asm::chacha20(unit, piece, &key, &at));
        done = end;
    }
}

/// Poly1305 of `data` under the one-time key `key`, RFC 8439 2.5's: its
/// whole blocks each with its 2^128 bit, its last bytes padded with a 1 and
/// zeros. Worked through a section a [`PIECE`].
pub fn poly1305(key: &[u8; KEY_LEN], data: &[u8]) -> [u8; TAG_LEN] {
    poly1305_with(Paths::Best, key, data)
}

fn poly1305_with(paths: Paths, key: &[u8; KEY_LEN], data: &[u8]) -> [u8; TAG_LEN] {
    let mut mac = Poly1305::new(key);
    let whole = data.len() & !15;
    for piece in data[..whole].chunks(PIECE) {
        paths.run(|unit| mac.blocks(unit, piece));
    }
    mac.last(&data[whole..]);
    mac.finish()
}

impl ChaCha20Poly1305 {
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        Self { key: key_words(key) }
    }

    /// `data` encrypted in place under `nonce`, and the tag over `aad` and
    /// the ciphertext.
    pub fn seal_in_place(&self, nonce: &[u8; NONCE_LEN], aad: &[u8], data: &mut [u8]) -> [u8; TAG_LEN] {
        self.seal(Paths::Best, nonce, aad, data)
    }

    /// `data`, the ciphertext, decrypted in place under `nonce` if `tag` is
    /// its tag over `aad` and it; left as it is if not.
    pub fn open_in_place(
        &self,
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        data: &mut [u8],
        tag: &[u8; TAG_LEN],
    ) -> Result<(), BadTag> {
        self.open(Paths::Best, nonce, aad, data, tag)
    }

    fn seal(&self, paths: Paths, nonce: &[u8; NONCE_LEN], aad: &[u8], data: &mut [u8]) -> [u8; TAG_LEN] {
        check_length(data.len());
        let mut mac = self.start(paths, nonce, aad);
        let mut done = 0;
        while done < data.len() {
            let end = data.len().min(done + PIECE);
            let piece = &mut data[done..end];
            let counter = data_counter(nonce, done);
            paths.run(|unit| {
                asm::chacha20(unit, piece, &self.key, &counter);
                mac.padded(unit, piece);
            });
            done = end;
        }
        finish(mac, aad.len(), data.len())
    }

    fn open(
        &self,
        paths: Paths,
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        data: &mut [u8],
        tag: &[u8; TAG_LEN],
    ) -> Result<(), BadTag> {
        check_length(data.len());
        /* The tag first, over the whole of the ciphertext: what is not its
         * own is never decrypted, and the caller's buffer is as it was. */
        let mut mac = self.start(paths, nonce, aad);
        for piece in data.chunks(PIECE) {
            paths.run(|unit| mac.padded(unit, piece));
        }
        let mut expected = finish(mac, aad.len(), data.len());
        let same = tags_equal(&expected, tag);
        expected.zeroize();
        if !same {
            return Err(BadTag);
        }
        let mut done = 0;
        while done < data.len() {
            let end = data.len().min(done + PIECE);
            let counter = data_counter(nonce, done);
            let piece = &mut data[done..end];
            paths.run(|unit| asm::chacha20(unit, piece, &self.key, &counter));
            done = end;
        }
        Ok(())
    }

    /// The MAC under the message's one-time key -- the first half of the
    /// key stream's block 0 -- with the associated data in.
    fn start(&self, paths: Paths, nonce: &[u8; NONCE_LEN], aad: &[u8]) -> Poly1305 {
        paths.run(|unit| {
            let mut block = [0u8; 64];
            asm::chacha20(unit, &mut block, &self.key, &counter(nonce, 0));
            let mut one_time = [0u8; 32];
            one_time.copy_from_slice(&block[..32]);
            block.zeroize();
            let mut mac = Poly1305::new(&one_time);
            one_time.zeroize();
            mac.padded(unit, aad);
            mac
        })
    }
}

/// A message the block counter cannot number to its end would be one whose
/// key stream repeats: a panic, before a byte of it is touched, rather than
/// that -- and no TLS record or tunnel datagram comes near it.
fn check_length(len: usize) {
    assert!(len as u64 <= MESSAGE_MAX, "chacha20poly1305: a message past what the block counter numbers");
}

/// The tag: the MAC with the two lengths in, as eight-byte little-endian
/// numbers. Integer code: one block.
fn finish(mut mac: Poly1305, aad_len: usize, data_len: usize) -> [u8; TAG_LEN] {
    let mut lengths = [0u8; 16];
    lengths[..8].copy_from_slice(&(aad_len as u64).to_le_bytes());
    lengths[8..].copy_from_slice(&(data_len as u64).to_le_bytes());
    mac.blocks(&Unit::INTEGER, &lengths);
    mac.finish()
}

/// The counter block for the message's data `offset` bytes in: block 1 is
/// its first.
fn data_counter(nonce: &[u8; NONCE_LEN], offset: usize) -> Counter {
    counter(nonce, 1 + (offset / 64) as u32)
}

fn counter(nonce: &[u8; NONCE_LEN], block: u32) -> Counter {
    let word = |i: usize| u32::from_le_bytes([nonce[4 * i], nonce[4 * i + 1], nonce[4 * i + 2], nonce[4 * i + 3]]);
    Counter([block, word(0), word(1), word(2)])
}

/// Whether two tags are one, in a time that says nothing of where they
/// differ.
fn tags_equal(a: &[u8; TAG_LEN], b: &[u8; TAG_LEN]) -> bool {
    let mut difference = 0u8;
    for i in 0..TAG_LEN {
        difference |= a[i] ^ b[i];
    }
    core::hint::black_box(difference) == 0
}
