//! The tunnel's ChaCha20-Poly1305, vnetwire's `Aead`: the kernel's crypto
//! crate -- CRYPTOGAMS' assembly in the kernel's SIMD sections, its integer
//! code where a section cannot be had. A file of its own, for tunnel.rs is
//! built by the hv fuzzer too, with RustCrypto's in its place.

use vnetwire::{Aead, KEY, NONCE, TAG};

pub struct Cryptogams;

impl Aead for Cryptogams {
    fn seal(&self, key: &[u8; KEY], nonce: &[u8; NONCE], aad: &[u8], data: &mut [u8]) -> [u8; TAG] {
        crypto::ChaCha20Poly1305::new(key).seal_in_place(nonce, aad, data)
    }

    fn open(&self, key: &[u8; KEY], nonce: &[u8; NONCE], aad: &[u8], data: &mut [u8], tag: &[u8; TAG]) -> bool {
        crypto::ChaCha20Poly1305::new(key).open_in_place(nonce, aad, data, tag).is_ok()
    }
}
