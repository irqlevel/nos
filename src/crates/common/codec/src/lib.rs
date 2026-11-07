//! Bytes in order: little-endian integers, and strings with their lengths
//! in front -- every read checked, none past the end, and a length never
//! taken on trust. What a replicated state's commands, answers and snapshots
//! are made of: the cluster's registry's and NBS's metadata's.
//!
//! [`Reader`] reads a slice; [`Stream`] reads the same encoding off a
//! [`Source`], for bytes never in memory at once -- a snapshot of tens of
//! MiB read a piece at a time, where the kernel's heap gives no block past
//! 512 KiB.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Bytes written in order.
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Default for Writer {
    fn default() -> Writer {
        Writer::new()
    }
}

impl Writer {
    pub fn new() -> Writer {
        Writer { buf: Vec::new() }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Bytes whose length the reader knows.
    pub fn fixed(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// A string, its length in front: at most what a `u8` counts -- the
    /// writer keeps none longer.
    pub fn str(&mut self, s: &str) {
        debug_assert!(s.len() <= usize::from(u8::MAX));
        self.buf.push(s.len().min(usize::from(u8::MAX)) as u8);
        self.buf.extend_from_slice(&s.as_bytes()[..s.len().min(usize::from(u8::MAX))]);
    }
}

/// Bytes read in order: None for any read past the end.
pub struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, at: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let got = self.buf.get(self.at..end)?;
        self.at = end;
        Some(got)
    }

    pub fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    pub fn u16(&mut self) -> Option<u16> {
        let mut b = [0u8; 2];
        b.copy_from_slice(self.take(2)?);
        Some(u16::from_le_bytes(b))
    }

    pub fn u32(&mut self) -> Option<u32> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.take(4)?);
        Some(u32::from_le_bytes(b))
    }

    pub fn u64(&mut self) -> Option<u64> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8)?);
        Some(u64::from_le_bytes(b))
    }

    pub fn fixed<const N: usize>(&mut self) -> Option<[u8; N]> {
        let mut b = [0u8; N];
        b.copy_from_slice(self.take(N)?);
        Some(b)
    }

    /// A string of at most `max` bytes, UTF-8.
    pub fn str(&mut self, max: usize) -> Option<&'a str> {
        let n = usize::from(self.u8()?);
        if n > max {
            return None;
        }
        core::str::from_utf8(self.take(n)?).ok()
    }

    /// Whether everything was read.
    pub fn done(&self) -> bool {
        self.at == self.buf.len()
    }
}

/// Where a [`Stream`] reads from: bytes in order, handed out as asked.
pub trait Source {
    /// `buf` filled whole with what comes next: false where fewer bytes are
    /// left, and then nothing is promised of `buf`.
    fn fill(&mut self, buf: &mut [u8]) -> bool;
}

impl Source for Reader<'_> {
    fn fill(&mut self, buf: &mut [u8]) -> bool {
        match self.take(buf.len()) {
            Some(b) => {
                buf.copy_from_slice(b);
                true
            }
            None => false,
        }
    }
}

/// [`Reader`]'s encoding read off a [`Source`]: None for any read past its
/// end. A string comes out owned, there being no slice to borrow from.
pub struct Stream<S: Source> {
    src: S,
}

impl<S: Source> Stream<S> {
    pub fn new(src: S) -> Stream<S> {
        Stream { src }
    }

    /// What it reads from, the bytes read taken off it.
    pub fn into_inner(self) -> S {
        self.src
    }

    pub fn u8(&mut self) -> Option<u8> {
        Some(self.fixed::<1>()?[0])
    }

    pub fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.fixed()?))
    }

    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.fixed()?))
    }

    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.fixed()?))
    }

    pub fn fixed<const N: usize>(&mut self) -> Option<[u8; N]> {
        let mut b = [0u8; N];
        self.src.fill(&mut b).then_some(b)
    }

    /// A string of at most `max` bytes, UTF-8.
    pub fn str(&mut self, max: usize) -> Option<String> {
        let n = usize::from(self.u8()?);
        if n > max {
            return None;
        }
        let mut b = [0u8; u8::MAX as usize];
        if !self.src.fill(&mut b[..n]) {
            return None;
        }
        core::str::from_utf8(&b[..n]).ok().map(String::from)
    }
}

/// Bytes as lowercase hex, two digits a byte: how ids and keys are written in
/// the files and the shell.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0xf)]));
    }
    s
}

/// `N` bytes from their hex, exactly `2 * N` digits of either case: None for
/// anything else.
pub fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let b = text.as_bytes();
    if b.len() != 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    for (i, pair) in b.chunks(2).enumerate() {
        let digit = |x: u8| (x as char).to_digit(16);
        out[i] = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(out)
}
