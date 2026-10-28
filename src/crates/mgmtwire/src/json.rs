//! JSON read as the bus takes it (RFC 8259, and stricter): whole numbers
//! from i64::MIN to u64::MAX, no fraction and no exponent; no key twice in
//! an object; strings of valid UTF-8, a `\u` escape a whole code point (a
//! surrogate only in its pair); and a limit on the text's length, its
//! nesting, its values, an object's members and a string's length -- what a
//! text from outside could otherwise make as large as it likes.

use alloc::string::String;
use alloc::vec::Vec;

/// A value as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    /// A whole number, from i64::MIN to u64::MAX.
    Int(i128),
    Str(String),
    List(Vec<Value>),
    /// The members in the order they came, no key twice.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// The member `key` of an object.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// A count: a number from 0 to u64::MAX, or -- past what JavaScript
    /// holds -- its digits in a string.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Int(n) => u64::try_from(*n).ok(),
            Value::Str(s) => digits_u64(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(n) => i64::try_from(*n).ok(),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(members) => Some(members),
            _ => None,
        }
    }
}

/// `s` as a count, if it is one in decimal: 1 to 20 digits, no sign, no
/// leading zero, and no larger than u64::MAX.
pub(crate) fn digits_u64(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 20 || (b.len() > 1 && b[0] == b'0') || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut n: u64 = 0;
    for d in b {
        n = n.checked_mul(10)?.checked_add(u64::from(d - b'0'))?;
    }
    Some(n)
}

/// What a text may make, at most.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The text's length, in bytes.
    pub bytes: usize,
    /// Lists and objects within each other.
    pub depth: usize,
    /// Values in all, at every depth.
    pub values: usize,
    /// One object's members.
    pub members: usize,
    /// One string's length, in bytes, escapes undone.
    pub string: usize,
}

impl Limits {
    /// What a call's parameters are held to.
    pub const PARAMS: Limits = Limits { bytes: 64 * 1024, depth: 16, values: 4096, members: 256, string: 16 * 1024 };
}

/// Why a text was refused, and where: the offset of the byte it was
/// refused at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub at: usize,
    pub what: &'static str,
}

/// `text` read whole -- one value, and nothing but white space around it.
pub fn parse(text: &[u8], limits: &Limits) -> Result<Value, ParseError> {
    if text.len() > limits.bytes {
        return Err(ParseError { at: limits.bytes, what: "longer than it may be" });
    }
    let mut p = Parser { s: text, i: 0, limits, values: 0 };
    p.space();
    let v = p.value(0)?;
    p.space();
    if p.i != p.s.len() {
        return Err(p.err("more after the value"));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    /// Where it is: never past the end.
    i: usize,
    limits: &'a Limits,
    values: usize,
}

impl Parser<'_> {
    fn err(&self, what: &'static str) -> ParseError {
        ParseError { at: self.i, what }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn space(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.i += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.values += 1;
        if self.values > self.limits.values {
            return Err(self.err("more values than it may have"));
        }
        match self.peek() {
            None => Err(self.err("a value expected")),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.list(depth),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => self.word(b"true", Value::Bool(true)),
            Some(b'f') => self.word(b"false", Value::Bool(false)),
            Some(b'n') => self.word(b"null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("not a value")),
        }
    }

    fn word(&mut self, word: &[u8], v: Value) -> Result<Value, ParseError> {
        if self.s[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("not a value"))
        }
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        let negative = self.peek() == Some(b'-');
        if negative {
            self.i += 1;
        }
        let digits = self.i;
        /* Past 2^64 it is out of range whatever comes after: the digits
         * stop being added, and the length stays bounded by the text. */
        let mut magnitude: u128 = 0;
        while let Some(d @ b'0'..=b'9') = self.peek() {
            magnitude = magnitude.saturating_mul(10).saturating_add(u128::from(d - b'0')).min(1u128 << 65);
            self.i += 1;
        }
        let n = self.i - digits;
        if n == 0 {
            return Err(ParseError { at: start, what: "a digit expected" });
        }
        if n > 1 && self.s[digits] == b'0' {
            return Err(ParseError { at: digits, what: "a number with a leading zero" });
        }
        if let Some(b'.' | b'e' | b'E') = self.peek() {
            return Err(self.err("a fraction or an exponent: whole numbers only"));
        }
        let value = if negative { -(magnitude as i128) } else { magnitude as i128 };
        if value < i128::from(i64::MIN) || value > i128::from(u64::MAX) {
            return Err(ParseError { at: start, what: "a number out of range" });
        }
        Ok(Value::Int(value))
    }

    fn string(&mut self) -> Result<String, ParseError> {
        let start = self.i;
        /* The opening quote */
        self.i += 1;
        let mut out = String::new();
        loop {
            /* A run of bytes taken as they are: it ends at what is ASCII --
             * a quote, a backslash, a control character -- so it never ends
             * inside a UTF-8 sequence, and is valid UTF-8 or the text is
             * not. */
            let run_start = self.i;
            while let Some(b) = self.peek() {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.i += 1;
            }
            let run = core::str::from_utf8(&self.s[run_start..self.i])
                .map_err(|e| ParseError { at: run_start + e.valid_up_to(), what: "not UTF-8" })?;
            self.push(&mut out, run, start)?;
            match self.peek() {
                None => return Err(ParseError { at: start, what: "a string not ended" }),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = self.escape()?;
                    let mut buf = [0u8; 4];
                    self.push(&mut out, c.encode_utf8(&mut buf), start)?;
                }
                Some(_) => return Err(self.err("a control character in a string")),
            }
        }
    }

    fn push(&self, out: &mut String, s: &str, start: usize) -> Result<(), ParseError> {
        if out.len().saturating_add(s.len()) > self.limits.string {
            return Err(ParseError { at: start, what: "a string longer than it may be" });
        }
        out.try_reserve(s.len()).map_err(|_| ParseError { at: start, what: "no memory for the string" })?;
        out.push_str(s);
        Ok(())
    }

    fn escape(&mut self) -> Result<char, ParseError> {
        let at = self.i - 1;
        let Some(b) = self.peek() else {
            return Err(ParseError { at, what: "an escape not ended" });
        };
        self.i += 1;
        Ok(match b {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let high = self.hex4()?;
                let code = match high {
                    0xD800..=0xDBFF => {
                        if !self.s[self.i..].starts_with(b"\\u") {
                            return Err(ParseError { at, what: "a surrogate without its pair" });
                        }
                        self.i += 2;
                        let low = self.hex4()?;
                        if !(0xDC00..=0xDFFF).contains(&low) {
                            return Err(ParseError { at, what: "a surrogate without its pair" });
                        }
                        0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                    }
                    0xDC00..=0xDFFF => return Err(ParseError { at, what: "a surrogate without its pair" }),
                    _ => high,
                };
                char::from_u32(code).ok_or(ParseError { at, what: "not a character" })?
            }
            _ => return Err(ParseError { at, what: "an unknown escape" }),
        })
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let d = match self.peek() {
                Some(c @ b'0'..=b'9') => c - b'0',
                Some(c @ b'a'..=b'f') => c - b'a' + 10,
                Some(c @ b'A'..=b'F') => c - b'A' + 10,
                _ => return Err(self.err("four hex digits expected")),
            };
            v = v * 16 + u32::from(d);
            self.i += 1;
        }
        Ok(v)
    }

    fn list(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth >= self.limits.depth {
            return Err(self.err("nested deeper than it may be"));
        }
        self.i += 1;
        self.space();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::List(items));
        }
        loop {
            let v = self.value(depth + 1)?;
            items.try_reserve(1).map_err(|_| self.err("no memory for the list"))?;
            items.push(v);
            self.space();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    self.space();
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::List(items));
                }
                _ => return Err(self.err("',' or ']' expected")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth >= self.limits.depth {
            return Err(self.err("nested deeper than it may be"));
        }
        self.i += 1;
        self.space();
        let mut members: Vec<(String, Value)> = Vec::new();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Object(members));
        }
        loop {
            if self.peek() != Some(b'"') {
                return Err(self.err("a key expected"));
            }
            let key_at = self.i;
            let key = self.string()?;
            /* Bounded by `members`, so the search is too. */
            if members.iter().any(|(k, _)| *k == key) {
                return Err(ParseError { at: key_at, what: "a key twice" });
            }
            if members.len() >= self.limits.members {
                return Err(ParseError { at: key_at, what: "more members than it may have" });
            }
            self.space();
            if self.peek() != Some(b':') {
                return Err(self.err("':' expected"));
            }
            self.i += 1;
            self.space();
            let v = self.value(depth + 1)?;
            members.try_reserve(1).map_err(|_| self.err("no memory for the object"))?;
            members.push((key, v));
            self.space();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    self.space();
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(members));
                }
                _ => return Err(self.err("',' or '}' expected")),
            }
        }
    }
}
