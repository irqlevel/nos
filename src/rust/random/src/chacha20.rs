//! The ChaCha20 block function of RFC 8439: twenty rounds of the
//! quarter-round over a 64-byte state built from a 256-bit key, a 32-bit block
//! counter and a 96-bit nonce, added back to the state it started from.
//!
//! Only the block function is here, not a stream cipher: the pool wants raw
//! keystream. The TLS client's ChaCha20 is a different implementation
//! entirely -- RustCrypto's -- and neither has to know about the other.

pub const KEY_SIZE: usize = 32;
pub const NONCE_SIZE: usize = 12;
pub const BLOCK_SIZE: usize = 64;

const STATE_WORDS: usize = 16;
/// Twenty rounds, run as ten column/diagonal pairs
const ROUND_PAIRS: usize = 10;
/// "expa" "nd 3" "2-by" "te k" as four little-endian words
const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646E, 0x7962_2D32, 0x6B20_6574];

#[inline]
fn quarter_round(x: &mut [u32; STATE_WORDS], a: usize, b: usize, c: usize, d: usize) {
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(16);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(12);
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(8);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(7);
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub fn block(key: &[u8; KEY_SIZE], counter: u32, nonce: &[u8; NONCE_SIZE]) -> [u8; BLOCK_SIZE] {
    let mut state = [0u32; STATE_WORDS];
    state[..4].copy_from_slice(&SIGMA);
    for (word, bytes) in state[4..12].iter_mut().zip(key.chunks_exact(4)) {
        *word = le32(bytes);
    }
    state[12] = counter;
    for (word, bytes) in state[13..].iter_mut().zip(nonce.chunks_exact(4)) {
        *word = le32(bytes);
    }

    let mut x = state;
    for _ in 0..ROUND_PAIRS {
        /* Columns */
        quarter_round(&mut x, 0, 4, 8, 12);
        quarter_round(&mut x, 1, 5, 9, 13);
        quarter_round(&mut x, 2, 6, 10, 14);
        quarter_round(&mut x, 3, 7, 11, 15);
        /* Diagonals */
        quarter_round(&mut x, 0, 5, 10, 15);
        quarter_round(&mut x, 1, 6, 11, 12);
        quarter_round(&mut x, 2, 7, 8, 13);
        quarter_round(&mut x, 3, 4, 9, 14);
    }

    /* The feed-forward addition is what makes the block function one-way:
     * without it the twenty rounds are a permutation anyone could run
     * backwards, and the pool's rekeying would be reversible. */
    let mut out = [0u8; BLOCK_SIZE];
    for ((bytes, word), start) in out.chunks_exact_mut(4).zip(x).zip(state) {
        bytes.copy_from_slice(&word.wrapping_add(start).to_le_bytes());
    }
    out
}

/// RFC 8439, 2.3.2: the block function's own test vector -- key 00..1f,
/// nonce 00:00:00:09:00:00:00:4a:00:00:00:00, block counter 1. The pool is
/// only as good as this is right, and there is no other way to find out that
/// a rotation or a round order is wrong: wrong output still looks random.
pub fn selftest() {
    let mut key = [0u8; KEY_SIZE];
    for (i, k) in key.iter_mut().enumerate() {
        *k = i as u8;
    }
    let nonce = [0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x4a, 0x00, 0x00, 0x00, 0x00];
    let expected: [u8; BLOCK_SIZE] = [
        0x10, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15,
        0x50, 0x0f, 0xdd, 0x1f, 0xa3, 0x20, 0x71, 0xc4,
        0xc7, 0xd1, 0xf4, 0xc7, 0x33, 0xc0, 0x68, 0x03,
        0x04, 0x22, 0xaa, 0x9a, 0xc3, 0xd4, 0x6c, 0x4e,
        0xd2, 0x82, 0x64, 0x46, 0x07, 0x9f, 0xaa, 0x09,
        0x14, 0xc2, 0xd7, 0x05, 0xd9, 0x8b, 0x02, 0xa2,
        0xb5, 0x12, 0x9c, 0xd1, 0xde, 0x16, 0x4e, 0xb9,
        0xcb, 0xd0, 0x83, 0xe8, 0xa2, 0x50, 0x3c, 0x4e,
    ];

    let got = block(&key, 1, &nonce);
    assert!(got == expected, "random selftest: the ChaCha20 block of RFC 8439 2.3.2 came out wrong");

    /* A different counter has to give a different block, or the counter is
     * not reaching the state at all. */
    assert!(block(&key, 2, &nonce) != got, "random selftest: the ChaCha20 counter changes nothing");
}
