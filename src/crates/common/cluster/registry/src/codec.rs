//! What the registry's commands, answers and snapshots are made of:
//! little-endian integers, and strings with their lengths in front -- every
//! read checked, none past the end, and a length never taken on trust.

use alloc::vec::Vec;

/// Bytes written in order.
pub(crate) struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Writer {
        Writer { buf: Vec::new() }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
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

    /// A string, its length in front: one the registry wrote, so at most
    /// what a `u8` counts -- every string it keeps is shorter than that.
    pub fn str(&mut self, s: &str) {
        debug_assert!(s.len() <= usize::from(u8::MAX));
        self.buf.push(s.len().min(usize::from(u8::MAX)) as u8);
        self.buf.extend_from_slice(&s.as_bytes()[..s.len().min(usize::from(u8::MAX))]);
    }
}

/// Bytes read in order: None for any read past the end.
pub(crate) struct Reader<'a> {
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
