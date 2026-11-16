//! The tunnel's ChaCha20-Poly1305 on the host: RustCrypto's, on the
//! software backends -- in the place of hv.ko's, the kernel's crypto
//! crate's CRYPTOGAMS assembly (src/crates/modules/hypervisor/hv/src/aead.rs),
//! which net-fuzz's `aead` target holds to this same code on every path the
//! host has, and every boot's `crypto::selftest` to the integer code.

use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use vnetwire::{Aead, KEY, NONCE, TAG};

pub struct RustCrypto;

impl Aead for RustCrypto {
    fn seal(&self, key: &[u8; KEY], nonce: &[u8; NONCE], aad: &[u8], data: &mut [u8]) -> [u8; TAG] {
        ChaCha20Poly1305::new(key.into())
            .encrypt_in_place_detached(nonce.into(), aad, data)
            .expect("a datagram RustCrypto seals")
            .into()
    }

    fn open(&self, key: &[u8; KEY], nonce: &[u8; NONCE], aad: &[u8], data: &mut [u8], tag: &[u8; TAG]) -> bool {
        ChaCha20Poly1305::new(key.into()).decrypt_in_place_detached(nonce.into(), aad, data, tag.into()).is_ok()
    }
}
