//! CRYPTOGAMS' ChaCha20 and Poly1305 (`asm/`, made by
//! `scripts/cryptogams.sh` from `../cryptogams/`, whose README says where
//! they come from and under what license), and the one place each is
//! called from: [`chacha20`] and the [`Poly1305`] state.
//!
//! Which code path a call takes is the [`Unit`]'s: a vector path only with
//! a SIMD section open -- the unit borrows it -- and the integer entry
//! points otherwise, which use no vector register: `ChaCha20_ctr32`,
//! `poly1305_init`, `poly1305_blocks`, `poly1305_emit`. Between the two the
//! choice is the one OpenSSL's own dispatch makes on the same CPU, in the
//! same places.
//!
//! What the assembly needs of the memory it is given, and that every call
//! here keeps: the key as eight words and the counter block as four, each
//! aligned to 16; the Poly1305 state its 192 bytes and more, aligned to 64;
//! input and output the same buffer or apart, at any alignment, of any
//! length -- ChaCha20's -- or of a length a multiple of 16 -- Poly1305's.

use core::marker::PhantomData;

use kcore::simd::Section;

#[cfg(all(target_arch = "x86_64", any(target_os = "none", target_os = "linux")))]
core::arch::global_asm!(
    include_str!("asm/chacha-x86_64-elf.s"),
    include_str!("asm/poly1305-x86_64-elf.s"),
    // Nothing of these is anything a loadable module exports.
    ".hidden ChaCha20_ctr32",
    ".hidden ChaCha20_ssse3",
    ".hidden ChaCha20_4xop",
    ".hidden ChaCha20_avx2",
    ".hidden poly1305_blocks_avx",
    ".hidden poly1305_blocks_avx2",
    ".hidden xor128_encrypt_n_pad",
    ".hidden xor128_decrypt_n_pad",
    options(att_syntax, raw)
);

// The C++ around the kernel's Rust is built with -mgeneral-regs-only and
// the Rust soft-float, so the assembler takes Advanced SIMD only once told.
#[cfg(all(target_arch = "aarch64", any(target_os = "none", target_os = "linux")))]
core::arch::global_asm!(
    ".arch_extension simd",
    include_str!("asm/chacha-armv8-linux64.s"),
    include_str!("asm/poly1305-armv8-linux64.s"),
    ".hidden ChaCha20_ctr32",
    ".hidden ChaCha20_neon",
    ".hidden poly1305_init",
    ".hidden poly1305_blocks",
    ".hidden poly1305_blocks_neon",
    ".hidden poly1305_emit",
    options(raw)
);

// A Mac's fuzzers, Mach-O.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
core::arch::global_asm!(
    include_str!("asm/chacha-armv8-ios64.s"),
    include_str!("asm/poly1305-armv8-ios64.s"),
    options(raw)
);

#[cfg(not(any(
    all(target_arch = "x86_64", any(target_os = "none", target_os = "linux")),
    all(target_arch = "aarch64", any(target_os = "none", target_os = "linux", target_os = "macos")),
)))]
compile_error!("no CRYPTOGAMS assembly for this target: scripts/cryptogams.sh makes x86-64 ELF and arm64 ELF and Mach-O");

unsafe extern "C" {
    /// ChaCha20 over `len` bytes of `inp` into `out`, with the key's eight
    /// words and the counter block -- the 32-bit block counter, then the
    /// nonce's three words. Integer code only.
    fn ChaCha20_ctr32(out: *mut u8, inp: *const u8, len: usize, key: *const u32, counter: *const u32);
    /// The same with SSSE3; `atom` is what OpenSSL's dispatch read from its
    /// capability word for its Atom check, and 0 says no Atom.
    #[cfg(target_arch = "x86_64")]
    fn ChaCha20_ssse3(out: *mut u8, inp: *const u8, len: usize, key: *const u32, counter: *const u32, atom: usize);
    /// The same with AVX2, eight blocks at a time; for more than 128 bytes.
    #[cfg(target_arch = "x86_64")]
    fn ChaCha20_avx2(out: *mut u8, inp: *const u8, len: usize, key: *const u32, counter: *const u32);
    /// The same with Advanced SIMD; for 192 bytes and more.
    #[cfg(target_arch = "aarch64")]
    fn ChaCha20_neon(out: *mut u8, inp: *const u8, len: usize, key: *const u32, counter: *const u32);

    /// A Poly1305 state started from the key's first half, r, clamped.
    fn poly1305_init(state: *mut u8, key: *const u8) -> i32;
    /// `len` -- a multiple of 16 -- bytes in, each block with its 2^128 bit
    /// when `pad_bit` is 1. Integer code only.
    fn poly1305_blocks(state: *mut u8, inp: *const u8, len: usize, pad_bit: usize);
    #[cfg(target_arch = "x86_64")]
    fn poly1305_blocks_avx(state: *mut u8, inp: *const u8, len: usize, pad_bit: usize);
    #[cfg(target_arch = "x86_64")]
    fn poly1305_blocks_avx2(state: *mut u8, inp: *const u8, len: usize, pad_bit: usize);
    #[cfg(target_arch = "aarch64")]
    fn poly1305_blocks_neon(state: *mut u8, inp: *const u8, len: usize, pad_bit: usize);
    /// The tag: the state's sum, plus the key's second half, s. Integer
    /// code only, whichever path the blocks took.
    fn poly1305_emit(state: *const u8, mac: *mut u8, s: *const u32);
}

/// The code paths a stretch of work may take: a vector path only while
/// the section it was made from is open, which it borrows.
#[derive(Clone, Copy)]
pub struct Unit<'s> {
    features: u32,
    _section: PhantomData<&'s Section>,
}

impl Unit<'static> {
    /// The integer entry points alone: for where no section can be had.
    pub const INTEGER: Unit<'static> = Unit { features: 0, _section: PhantomData };
}

impl<'s> Unit<'s> {
    /// What every CPU's section may use, for as long as `section` is open.
    #[inline]
    pub fn of(_section: &'s Section) -> Self {
        Unit { features: kcore::simd::features(), _section: PhantomData }
    }

    /// The same, held to `mask`'s features: what the self-test drives each
    /// path with.
    pub fn of_only(_section: &'s Section, mask: u32) -> Self {
        Unit { features: kcore::simd::features() & mask, _section: PhantomData }
    }

    #[inline]
    fn has(&self, feature: u32) -> bool {
        self.features & feature != 0
    }
}

/// The key as the assembly reads it: eight little-endian words, wiped
/// when dropped.
#[repr(C, align(16))]
pub struct Key(pub [u32; 8]);

impl Drop for Key {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0[..]);
    }
}

/// The counter block, the state's last four words: RFC 8439's 32-bit block
/// counter and the nonce's three words, or the original ChaCha20's 64-bit
/// counter and 64-bit nonce, two words each. The code counts in the first
/// word alone.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct Counter(pub [u32; 4]);

/// `buf` XORed with ChaCha20's key stream from `counter` on, in place.
pub fn chacha20(unit: &Unit, buf: &mut [u8], key: &Key, counter: &Counter) {
    let len = buf.len();
    if len == 0 {
        return;
    }
    let p = buf.as_mut_ptr();
    let k = key.0.as_ptr();
    let c = counter.0.as_ptr();
    /* Each call: `buf` is `len` bytes, read and written in place, which the
     * code does a block at a time, reading each before writing it; the key
     * and the counter are 32 and 16 bytes, aligned to 16. A vector path is
     * taken only while `unit`'s section is open. */
    #[cfg(target_arch = "x86_64")]
    if unit.has(kcore::simd::SSSE3) {
        if unit.has(kcore::simd::AVX2) && len > 128 {
            unsafe { ChaCha20_avx2(p, p, len, k, c) };
        } else {
            unsafe { ChaCha20_ssse3(p, p, len, k, c, 0) };
        }
        return;
    }
    #[cfg(target_arch = "aarch64")]
    if unit.has(kcore::simd::NEON) && len >= 192 {
        unsafe { ChaCha20_neon(p, p, len, k, c) };
        return;
    }
    unsafe { ChaCha20_ctr32(p, p, len, k, c) };
}

/// The assembly's Poly1305 state: 192 bytes used, 64-byte aligned.
#[repr(C, align(64))]
struct State([u8; 256]);

/// A Poly1305 MAC in progress.
pub struct Poly1305 {
    state: State,
    /// s, the key's second half, as `poly1305_emit` reads it.
    s: Counter,
}

impl Poly1305 {
    /// A MAC under the one-time key `key`.
    pub fn new(key: &[u8; 32]) -> Self {
        let mut mac = Poly1305 { state: State([0; 256]), s: Counter([0; 4]) };
        for (i, word) in mac.s.0.iter_mut().enumerate() {
            let at = 16 + 4 * i;
            *word = u32::from_le_bytes([key[at], key[at + 1], key[at + 2], key[at + 3]]);
        }
        /* The state is ours and 256 bytes; the key 32, of which init reads
         * the first 16. */
        unsafe { poly1305_init(mac.state.0.as_mut_ptr(), key.as_ptr()) };
        mac
    }

    /// Whole blocks in: `data`'s length a multiple of 16.
    pub fn blocks(&mut self, unit: &Unit, data: &[u8]) {
        assert!(data.len() % 16 == 0, "poly1305: a block cut short");
        let len = data.len();
        if len == 0 {
            return;
        }
        let st = self.state.0.as_mut_ptr();
        let p = data.as_ptr();
        /* Each call: the state is this MAC's, `data` is `len` bytes, a
         * multiple of 16; the vector paths are taken only while `unit`'s
         * section is open, and each turns the state to the radix it uses
         * and back as the next call needs. */
        #[cfg(target_arch = "x86_64")]
        {
            if unit.has(kcore::simd::AVX2) {
                unsafe { poly1305_blocks_avx2(st, p, len, 1) };
                return;
            }
            if unit.has(kcore::simd::AVX) {
                unsafe { poly1305_blocks_avx(st, p, len, 1) };
                return;
            }
        }
        #[cfg(target_arch = "aarch64")]
        if unit.has(kcore::simd::NEON) {
            unsafe { poly1305_blocks_neon(st, p, len, 1) };
            return;
        }
        unsafe { poly1305_blocks(st, p, len, 1) };
    }

    /// A message's last bytes, fewer than a block, as RFC 8439 2.5 pads
    /// them: a 1 after them, zeros to the block's end, and no 2^128 bit --
    /// Poly1305's own padding, which the AEAD's ([`Poly1305::padded`]) is
    /// not. Integer code: one block, which takes the state from whichever
    /// radix the blocks before it left it in.
    pub fn last(&mut self, tail: &[u8]) {
        assert!(tail.len() < 16, "poly1305: a tail of a block or more");
        if tail.is_empty() {
            return;
        }
        let mut block = [0u8; 16];
        block[..tail.len()].copy_from_slice(tail);
        block[tail.len()] = 1;
        /* The state is this MAC's, the block 16 bytes, its pad bit 0. */
        unsafe { poly1305_blocks(self.state.0.as_mut_ptr(), block.as_ptr(), 16, 0) };
        zeroize::Zeroize::zeroize(&mut block);
    }

    /// `data` in, its last block filled out with zeros to 16 bytes -- the
    /// AEAD's padding.
    pub fn padded(&mut self, unit: &Unit, data: &[u8]) {
        let whole = data.len() & !15;
        self.blocks(unit, &data[..whole]);
        if whole < data.len() {
            let mut block = [0u8; 16];
            block[..data.len() - whole].copy_from_slice(&data[whole..]);
            self.blocks(unit, &block);
            zeroize::Zeroize::zeroize(&mut block);
        }
    }

    /// The tag.
    pub fn finish(mut self) -> [u8; 16] {
        let mut tag = [0u8; 16];
        /* The state is this MAC's, whatever path its blocks took; the tag
         * 16 bytes; s four words, aligned to 16. */
        unsafe { poly1305_emit(self.state.0.as_ptr(), tag.as_mut_ptr(), self.s.0.as_ptr()) };
        self.wipe();
        tag
    }

    fn wipe(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.state.0[..]);
        zeroize::Zeroize::zeroize(&mut self.s.0[..]);
    }
}

impl Drop for Poly1305 {
    fn drop(&mut self) {
        self.wipe();
    }
}
