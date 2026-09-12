//! The SSH wire encodings (RFC 4251 section 5): read out of a payload with
//! a `Reader`, appended to one with the `put_` functions.

use alloc::vec::Vec;

/// Reads a payload front to back. A read that would run past the end gives
/// None instead, and a message that does not parse is the protocol error of
/// whoever reads it.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    pub fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }

    /// Any value but 0 is true (RFC 4251 5).
    pub fn bool(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }

    pub fn u32(&mut self) -> Option<u32> {
        let b = self.bytes(4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn string(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.bytes(n)
    }

    /// A string that has to be UTF-8: a user name, a command line.
    pub fn utf8(&mut self) -> Option<&'a str> {
        core::str::from_utf8(self.string()?).ok()
    }

    /// Whether everything has been read.
    pub fn done(&self) -> bool {
        self.pos == self.buf.len()
    }
}

pub fn put_bool(out: &mut Vec<u8>, v: bool) {
    out.push(v as u8);
}

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}

pub fn put_name_list(out: &mut Vec<u8>, names: &[&str]) {
    let len = names.iter().map(|n| n.len()).sum::<usize>() + names.len().saturating_sub(1);
    put_u32(out, len as u32);
    for (i, name) in names.iter().enumerate() {
        if i != 0 {
            out.push(b',');
        }
        out.extend_from_slice(name.as_bytes());
    }
}

/// An unsigned big-endian number the way an mpint carries it: no leading
/// zero bytes, but one 0 in front when the top bit would otherwise read as
/// a sign. The bytes to write, and whether that 0 goes first.
pub fn mpint_parts(magnitude: &[u8]) -> (bool, &[u8]) {
    let first = magnitude.iter().position(|&b| b != 0).unwrap_or(magnitude.len());
    let digits = &magnitude[first..];
    let sign_pad = digits.first().map_or(false, |&b| b & 0x80 != 0);
    (sign_pad, digits)
}

/// The names in a name-list.
pub fn names(list: &[u8]) -> impl Iterator<Item = &[u8]> {
    list.split(|&b| b == b',').filter(|n| !n.is_empty())
}

pub fn has_name(list: &[u8], name: &str) -> bool {
    names(list).any(|n| n == name.as_bytes())
}

/// The first name of the client's list that is one of ours: the client's
/// order decides (RFC 4253 7.1).
pub fn choose(client: &[u8], ours: &[&'static str]) -> Option<&'static str> {
    names(client).find_map(|n| ours.iter().copied().find(|o| o.as_bytes() == n))
}
