//! The state's bytes in and out: `codec`'s little-endian integers, a part's
//! kind and length in front of it, and every read failing with the name of
//! what it was reading.

use alloc::vec::Vec;

use codec::{Reader, Writer};

use crate::{Error, Result, PART_MAX};

/// Bytes written in order.
pub struct Out {
    w: Writer,
    /// A part's length did not fit, or memory ran out: the whole is lost.
    failed: bool,
}

impl Out {
    pub fn new() -> Out {
        Out { w: Writer::new(), failed: false }
    }

    /// Room for `n` bytes, taken now, so that a state of the size expected
    /// is written without the vector growing -- None when there is none.
    pub fn reserve(&mut self, n: usize) -> Option<()> {
        self.w.buf.try_reserve(n).ok()
    }

    pub fn u8(&mut self, v: u8) {
        self.w.u8(v);
    }

    pub fn bool(&mut self, v: bool) {
        self.w.u8(u8::from(v));
    }

    pub fn u16(&mut self, v: u16) {
        self.w.u16(v);
    }

    pub fn u32(&mut self, v: u32) {
        self.w.u32(v);
    }

    pub fn u64(&mut self, v: u64) {
        self.w.u64(v);
    }

    pub fn fixed(&mut self, b: &[u8]) {
        self.w.fixed(b);
    }

    /// An optional value: a byte saying whether, then the value.
    pub fn opt<T>(&mut self, v: Option<T>, put: impl FnOnce(&mut Out, T)) {
        match v {
            None => self.u8(0),
            Some(v) => {
                self.u8(1);
                put(self, v);
            }
        }
    }

    /// A part: its kind, its length, then what `body` writes -- the length
    /// filled in after.
    pub fn part(&mut self, kind: u8, body: impl FnOnce(&mut Out)) -> Option<()> {
        self.u8(kind);
        let at = self.w.buf.len();
        self.u32(0);
        body(self);
        let len = self.w.buf.len() - at - 4;
        if len > PART_MAX {
            self.failed = true;
            return None;
        }
        self.w.buf[at..at + 4].copy_from_slice(&(len as u32).to_le_bytes());
        Some(())
    }

    pub fn finish(self) -> Option<Vec<u8>> {
        (!self.failed).then_some(self.w.buf)
    }
}

/// Bytes read in order, each read named for the error it fails with -- and
/// the version of the format they are in, which says what fields there are.
pub struct In<'a> {
    r: Reader<'a>,
    pub version: u16,
}

impl<'a> In<'a> {
    pub fn new(buf: &'a [u8]) -> In<'a> {
        In { r: Reader::new(buf), version: crate::VERSION }
    }

    pub fn u8(&mut self, what: &'static str) -> Result<u8> {
        self.r.u8().ok_or(Error(what))
    }

    /// A byte that is 0 or 1, nothing else.
    pub fn bool(&mut self, what: &'static str) -> Result<bool> {
        match self.r.u8() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(Error(what)),
        }
    }

    pub fn u16(&mut self, what: &'static str) -> Result<u16> {
        self.r.u16().ok_or(Error(what))
    }

    pub fn u32(&mut self, what: &'static str) -> Result<u32> {
        self.r.u32().ok_or(Error(what))
    }

    pub fn u64(&mut self, what: &'static str) -> Result<u64> {
        self.r.u64().ok_or(Error(what))
    }

    pub fn fixed<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N]> {
        self.r.fixed::<N>().ok_or(Error(what))
    }

    pub fn bytes(&mut self, n: usize, what: &'static str) -> Result<&'a [u8]> {
        self.r.bytes(n).ok_or(Error(what))
    }

    /// An optional value, as [`Out::opt`] wrote it.
    pub fn opt<T>(&mut self, what: &'static str, get: impl FnOnce(&mut In<'a>) -> Result<T>) -> Result<Option<T>> {
        if self.bool(what)? { get(self).map(Some) } else { Ok(None) }
    }

    /// A part of kind `kind`, read whole by `body` -- which must take every
    /// byte its length says and no more.
    pub fn part<T>(&mut self, kind: u8, what: &'static str, body: impl FnOnce(&mut In<'a>) -> Result<T>) -> Result<T> {
        if self.u8(what)? != kind {
            return Err(Error(what));
        }
        let len = self.u32(what)? as usize;
        if len > PART_MAX {
            return Err(Error(what));
        }
        let version = self.version;
        let mut inner = In { r: Reader::new(self.bytes(len, what)?), version };
        let v = body(&mut inner)?;
        if !inner.done() {
            return Err(Error("a part longer than what it holds"));
        }
        Ok(v)
    }

    pub fn done(&self) -> bool {
        self.r.done()
    }
}

/// `n` items read by `get` into a vector, `n` bounded by `max` before
/// anything is taken for them.
pub fn list<'a, T>(r: &mut In<'a>, n: usize, max: usize, what: &'static str,
                   mut get: impl FnMut(&mut In<'a>) -> Result<T>) -> Result<Vec<T>> {
    if n > max {
        return Err(Error(what));
    }
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| Error("no memory"))?;
    for _ in 0..n {
        v.push(get(r)?);
    }
    Ok(v)
}
