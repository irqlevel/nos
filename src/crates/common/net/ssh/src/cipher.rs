//! chacha20-poly1305@openssh.com, the one cipher the server speaks, and a
//! session's own random stream: both made of ChaCha20 and Poly1305 as the
//! machine has them (`Primitives`).

use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// The Poly1305 tag after every packet.
pub const TAG_LEN: usize = 16;
/// Key material for one direction: two ChaCha20 keys.
pub const KEY_LEN: usize = 64;

/// ChaCha20 block 1: where a packet's payload starts in its stream, block 0
/// having given the Poly1305 key.
const PAYLOAD_BLOCK: u32 = 1;

/// ChaCha20 and Poly1305: what the cipher and a session's random stream are
/// made of. There is no kernel here, so the server hands them in -- the
/// sshd module the kernel's crypto crate's, CRYPTOGAMS' assembly in the
/// kernel's SIMD sections; the net fuzzer the same, the client it holds
/// them to having RustCrypto's.
pub trait Primitives: Sync {
    /// `data` XORed in place with ChaCha20's key stream under `key`, from
    /// the counter block `counter` on: the state's last four words, the
    /// first of which counts the blocks -- and is not to pass 2^32 within
    /// `data`.
    fn chacha20(&self, key: &[u8; 32], counter: &[u32; 4], data: &mut [u8]);
    /// Poly1305 of `data` under the one-time key `key`, as RFC 8439 2.5 has
    /// it: its last block padded with a 1 and zeros.
    fn poly1305(&self, key: &[u8; 32], data: &[u8]) -> [u8; TAG_LEN];
}

/// Block `block` of a packet's stream: OpenSSH's ChaCha20 is the original
/// one, a 64-bit block counter and a 64-bit nonce -- the packet's sequence
/// number, big-endian -- each two little-endian words of the state.
fn counter(seq: u32, block: u32) -> [u32; 4] {
    let nonce = u64::from(seq).to_be_bytes();
    [
        block,
        0,
        u32::from_le_bytes([nonce[0], nonce[1], nonce[2], nonce[3]]),
        u32::from_le_bytes([nonce[4], nonce[5], nonce[6], nonce[7]]),
    ]
}

/// chacha20-poly1305@openssh.com (OpenSSH's PROTOCOL.chacha20poly1305):
/// ChaCha20 with the packet's sequence number for the nonce, under two keys.
/// The first 32 bytes of key material encrypt the packet and give each
/// packet its Poly1305 key, the first half of ChaCha20 block 0, the payload
/// taking block 1 on; the last 32 encrypt the 4-byte length alone, so a
/// reader learns how long a packet is before it has the rest. The tag covers
/// the encrypted length and payload. Not RFC 8439's AEAD: the original 64-bit
/// nonce, and a length under a key of its own.
pub struct ChaChaPoly<'p> {
    primitives: &'p dyn Primitives,
    main: [u8; 32],
    header: [u8; 32],
}

impl<'p> ChaChaPoly<'p> {
    pub fn new(primitives: &'p dyn Primitives, key: &[u8; KEY_LEN]) -> Self {
        let mut main = [0u8; 32];
        let mut header = [0u8; 32];
        main.copy_from_slice(&key[..32]);
        header.copy_from_slice(&key[32..]);
        Self { primitives, main, header }
    }

    /// A packet's Poly1305 key: the first half of block 0 of its stream.
    fn mac_key(&self, seq: u32) -> [u8; 32] {
        let mut key = [0u8; 32];
        self.primitives.chacha20(&self.main, &counter(seq, 0), &mut key);
        key
    }

    /// The tag over `packet` -- the length and the payload, encrypted.
    fn tag(&self, seq: u32, packet: &[u8]) -> [u8; TAG_LEN] {
        let mut key = self.mac_key(seq);
        let tag = self.primitives.poly1305(&key, packet);
        key.zeroize();
        tag
    }

    /// A packet's length field, from the 4 bytes it starts with on the wire.
    pub fn length(&self, seq: u32, wire: &[u8]) -> u32 {
        let mut len = [wire[0], wire[1], wire[2], wire[3]];
        self.primitives.chacha20(&self.header, &counter(seq, 0), &mut len);
        u32::from_be_bytes(len)
    }

    /// Checks `tag` over `packet` -- the length and the payload, as they came
    /// -- and decrypts the payload, all of it past the first 4 bytes, in
    /// place. False, with nothing decrypted, when the tag is wrong.
    pub fn open(&self, seq: u32, packet: &mut [u8], tag: &[u8]) -> bool {
        let expected = self.tag(seq, packet);
        if !bool::from(expected.as_slice().ct_eq(tag)) {
            return false;
        }
        self.primitives.chacha20(&self.main, &counter(seq, PAYLOAD_BLOCK), &mut packet[4..]);
        true
    }

    /// Encrypts `packet` in place -- its plaintext length, then the rest --
    /// and gives back the tag that follows it on the wire.
    pub fn seal(&self, seq: u32, packet: &mut [u8]) -> [u8; TAG_LEN] {
        self.primitives.chacha20(&self.header, &counter(seq, 0), &mut packet[..4]);
        self.primitives.chacha20(&self.main, &counter(seq, PAYLOAD_BLOCK), &mut packet[4..]);
        self.tag(seq, packet)
    }
}

impl Drop for ChaChaPoly<'_> {
    fn drop(&mut self) {
        self.main.zeroize();
        self.header.zeroize();
    }
}

/// A session's own random stream: ChaCha20 keyed from the machine's pool
/// once, when the session starts, a fill a run of its blocks -- a 64-bit
/// count of them, the nonce zero, so none comes twice. Padding and a
/// rekey's ephemeral key come from it: no call into the kernel on the way
/// out of a packet, and nothing that can fail half-way through one.
pub struct Rng<'p> {
    primitives: &'p dyn Primitives,
    seed: [u8; 32],
    /// The stream's next block: what the last fill left of its last block
    /// is not used.
    block: u64,
}

impl<'p> Rng<'p> {
    pub fn new(primitives: &'p dyn Primitives, seed: &[u8; 32]) -> Self {
        Self { primitives, seed: *seed, block: 0 }
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        buf.fill(0);
        let mut at = 0;
        while at < buf.len() {
            /* A call takes the counter's low word to 2^32 at most: the
             * high word moves on between two */
            let room = ((1u64 << 32) - (self.block & 0xffff_ffff)).saturating_mul(64);
            let end = buf.len().min(at.saturating_add(usize::try_from(room).unwrap_or(usize::MAX)));
            let counter = [self.block as u32, (self.block >> 32) as u32, 0, 0];
            self.primitives.chacha20(&self.seed, &counter, &mut buf[at..end]);
            self.block += ((end - at) as u64).div_ceil(64);
            at = end;
        }
    }
}

impl Drop for Rng<'_> {
    fn drop(&mut self) {
        self.seed.zeroize();
    }
}
