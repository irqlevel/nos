//! JSON written as it goes: no tree built first, nothing allocated -- each
//! piece handed to a sink as it is made. The sink may take no more (a reply
//! past what its caller asked for at most, a socket gone): from then on
//! nothing is written and `ok` says the text is not whole.
//!
//! It keeps to what [`crate::json`] reads -- whole numbers, strings escaped
//! where JSON needs it -- and to JavaScript's numbers: a count past
//! [`crate::SAFE_INT`] is written as its digits in a string, which
//! [`crate::Value::as_u64`] reads back. A misuse (a key outside an object,
//! a value in one without its key, an end with nothing open, nesting past
//! `MAX_DEPTH`) makes the text not whole too, rather than malformed.

use crate::SAFE_INT;

/// Lists and objects within each other, at most.
pub const MAX_DEPTH: usize = 32;

pub struct Writer<'a> {
    sink: &'a mut dyn FnMut(&[u8]) -> bool,
    ok: bool,
    /// For each list or object open, whether it is an object, and whether
    /// nothing has been written in it yet (no comma before the next).
    object: [bool; MAX_DEPTH],
    empty: [bool; MAX_DEPTH],
    depth: usize,
    /// A key was the last thing written: its value comes next.
    keyed: bool,
    /// The one value a text is has been begun: a second is a misuse.
    top: bool,
}

impl<'a> Writer<'a> {
    pub fn new(sink: &'a mut dyn FnMut(&[u8]) -> bool) -> Writer<'a> {
        Writer { sink, ok: true, object: [false; MAX_DEPTH], empty: [false; MAX_DEPTH], depth: 0, keyed: false, top: false }
    }

    /// Whether everything was taken and written as it should be, and every
    /// list and object opened was closed.
    pub fn ok(&self) -> bool {
        self.ok && self.top && self.depth == 0 && !self.keyed
    }

    fn raw(&mut self, b: &[u8]) {
        if self.ok && !(self.sink)(b) {
            self.ok = false;
        }
    }

    /// What goes before a value: a comma after another, and in an object
    /// its key first.
    fn before_value(&mut self) {
        if self.keyed {
            self.keyed = false;
            return;
        }
        if self.depth == 0 {
            if self.top {
                self.ok = false;
            }
            self.top = true;
            return;
        }
        if self.object[self.depth - 1] {
            /* A value in an object with no key */
            self.ok = false;
            return;
        }
        self.comma();
    }

    fn comma(&mut self) {
        let d = self.depth - 1;
        if self.empty[d] {
            self.empty[d] = false;
        } else {
            self.raw(b",");
        }
    }

    fn open(&mut self, object: bool) {
        self.before_value();
        if self.depth == MAX_DEPTH {
            self.ok = false;
            return;
        }
        self.raw(if object { b"{" } else { b"[" });
        self.object[self.depth] = object;
        self.empty[self.depth] = true;
        self.depth += 1;
    }

    fn close(&mut self, object: bool) {
        if self.depth == 0 || self.object[self.depth - 1] != object || self.keyed {
            self.ok = false;
            return;
        }
        self.depth -= 1;
        self.raw(if object { b"}" } else { b"]" });
    }

    pub fn begin_object(&mut self) {
        self.open(true);
    }

    pub fn end_object(&mut self) {
        self.close(true);
    }

    pub fn begin_list(&mut self) {
        self.open(false);
    }

    pub fn end_list(&mut self) {
        self.close(false);
    }

    /// The key of the next value, in the object open.
    pub fn key(&mut self, key: &str) {
        if self.depth == 0 || !self.object[self.depth - 1] || self.keyed {
            self.ok = false;
            return;
        }
        self.comma();
        self.string(key);
        self.raw(b":");
        self.keyed = true;
    }

    pub fn str(&mut self, s: &str) {
        self.before_value();
        self.string(s);
    }

    pub fn bool(&mut self, b: bool) {
        self.before_value();
        self.raw(if b { b"true" } else { b"false" });
    }

    pub fn null(&mut self) {
        self.before_value();
        self.raw(b"null");
    }

    /// A count: a number up to SAFE_INT, its digits in a string past it.
    pub fn uint(&mut self, n: u64) {
        self.before_value();
        let mut buf = [0u8; 20];
        let digits = decimal(n, &mut buf);
        if n <= SAFE_INT {
            self.raw(digits);
        } else {
            self.raw(b"\"");
            self.raw(digits);
            self.raw(b"\"");
        }
    }

    /// A signed number, which has to be one JavaScript holds.
    pub fn int(&mut self, n: i64) {
        if n.unsigned_abs() > SAFE_INT {
            self.ok = false;
            return;
        }
        self.before_value();
        if n < 0 {
            self.raw(b"-");
        }
        let mut buf = [0u8; 20];
        let digits = decimal(n.unsigned_abs(), &mut buf);
        self.raw(digits);
    }

    /// `key` and a string, in the object open.
    pub fn field_str(&mut self, key: &str, s: &str) {
        self.key(key);
        self.str(s);
    }

    /// `key` and a count, in the object open.
    pub fn field_uint(&mut self, key: &str, n: u64) {
        self.key(key);
        self.uint(n);
    }

    /// `key` and a truth, in the object open.
    pub fn field_bool(&mut self, key: &str, b: bool) {
        self.key(key);
        self.bool(b);
    }

    /// A string, quoted and escaped where JSON needs it: a quote, a
    /// backslash, a control character. The rest goes as it is, UTF-8.
    fn string(&mut self, s: &str) {
        self.raw(b"\"");
        let b = s.as_bytes();
        let mut start = 0;
        for (i, &c) in b.iter().enumerate() {
            let short: Option<&[u8]> = match c {
                b'"' => Some(b"\\\""),
                b'\\' => Some(b"\\\\"),
                b'\n' => Some(b"\\n"),
                b'\r' => Some(b"\\r"),
                b'\t' => Some(b"\\t"),
                _ => None,
            };
            if short.is_none() && c >= 0x20 {
                continue;
            }
            self.raw(&b[start..i]);
            match short {
                Some(e) => self.raw(e),
                None => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    self.raw(&[b'\\', b'u', b'0', b'0', HEX[usize::from(c >> 4)], HEX[usize::from(c & 15)]]);
                }
            }
            start = i + 1;
        }
        self.raw(&b[start..]);
        self.raw(b"\"");
    }
}

/// `n`'s decimal digits, at the end of `buf`.
fn decimal(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &buf[i..];
        }
    }
}
