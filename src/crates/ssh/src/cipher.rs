//! chacha20-poly1305@openssh.com, the one cipher the server speaks, and a
//! session's own random stream.

use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use chacha20::{ChaCha20, ChaCha20Legacy, Key, LegacyNonce, Nonce};
use poly1305::universal_hash::KeyInit;
use poly1305::Poly1305;
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// The Poly1305 tag after every packet.
pub const TAG_LEN: usize = 16;
/// Key material for one direction: two ChaCha20 keys.
pub const KEY_LEN: usize = 64;

/// ChaCha20 block 1: where a packet's payload starts in its stream, block 0
/// having given the Poly1305 key.
const PAYLOAD_OFFSET: u64 = 64;

/// chacha20-poly1305@openssh.com (OpenSSH's PROTOCOL.chacha20poly1305):
/// ChaCha20 with the packet's sequence number for the nonce, under two keys.
/// The first 32 bytes of key material encrypt the packet and give each
/// packet its Poly1305 key, the first half of ChaCha20 block 0, the payload
/// taking block 1 on; the last 32 encrypt the 4-byte length alone, so a
/// reader learns how long a packet is before it has the rest. The tag covers
/// the encrypted length and payload. Not RFC 8439's AEAD: the original 64-bit
/// nonce, and a length under a key of its own.
pub struct ChaChaPoly {
    main: [u8; 32],
    header: [u8; 32],
}

impl ChaChaPoly {
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        let mut main = [0u8; 32];
        let mut header = [0u8; 32];
        main.copy_from_slice(&key[..32]);
        header.copy_from_slice(&key[32..]);
        Self { main, header }
    }

    fn stream(key: &[u8; 32], seq: u32) -> ChaCha20Legacy {
        let nonce = u64::from(seq).to_be_bytes();
        ChaCha20Legacy::new(Key::from_slice(key), LegacyNonce::from_slice(&nonce))
    }

    /// The Poly1305 for a packet, and its stream where the payload starts.
    fn payload_stream(&self, seq: u32) -> (Poly1305, ChaCha20Legacy) {
        let mut stream = Self::stream(&self.main, seq);
        let mut poly_key = [0u8; 32];
        stream.apply_keystream(&mut poly_key);
        stream.seek(PAYLOAD_OFFSET);
        (Poly1305::new(poly1305::Key::from_slice(&poly_key)), stream)
    }

    /// A packet's length field, from the 4 bytes it starts with on the wire.
    pub fn length(&self, seq: u32, wire: &[u8]) -> u32 {
        let mut len = [wire[0], wire[1], wire[2], wire[3]];
        Self::stream(&self.header, seq).apply_keystream(&mut len);
        u32::from_be_bytes(len)
    }

    /// Checks `tag` over `packet` -- the length and the payload, as they came
    /// -- and decrypts the payload, all of it past the first 4 bytes, in
    /// place. False, with nothing decrypted, when the tag is wrong.
    pub fn open(&self, seq: u32, packet: &mut [u8], tag: &[u8]) -> bool {
        let (mac, mut stream) = self.payload_stream(seq);
        let expected = mac.compute_unpadded(packet);
        if !bool::from(expected.as_slice().ct_eq(tag)) {
            return false;
        }
        stream.apply_keystream(&mut packet[4..]);
        true
    }

    /// Encrypts `packet` in place -- its plaintext length, then the rest --
    /// and gives back the tag that follows it on the wire.
    pub fn seal(&self, seq: u32, packet: &mut [u8]) -> [u8; TAG_LEN] {
        Self::stream(&self.header, seq).apply_keystream(&mut packet[..4]);
        let (mac, mut stream) = self.payload_stream(seq);
        stream.apply_keystream(&mut packet[4..]);
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(mac.compute_unpadded(packet).as_slice());
        tag
    }
}

impl Drop for ChaChaPoly {
    fn drop(&mut self) {
        self.main.zeroize();
        self.header.zeroize();
    }
}

/// A session's own random stream: ChaCha20 keyed from the machine's pool
/// once, when the session starts. Padding and a rekey's ephemeral key come
/// from it: no call into the kernel on the way out of a packet, and nothing
/// that can fail half-way through one.
pub struct Rng {
    stream: ChaCha20,
}

impl Rng {
    pub fn new(seed: &[u8; 32]) -> Self {
        Self { stream: ChaCha20::new(Key::from_slice(seed), Nonce::from_slice(&[0u8; 12])) }
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        buf.fill(0);
        self.stream.apply_keystream(buf);
    }
}
