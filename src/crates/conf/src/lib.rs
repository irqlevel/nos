//! A configuration file's text as a strict subset of TOML: `key = value` a
//! line, `#` to the end of a line a comment, and for values a string --
//! "basic", with TOML's escapes, or 'literal' -- a whole number, `true` or
//! `false`, and a list of strings in brackets, which may run over several
//! lines. No tables, no dotted or quoted keys, nothing else: what this cannot
//! read it refuses, saying the line, rather than read it some other way. So
//! is a key given twice, and -- as the file's own `Keys` says -- one it does
//! not have: `memroy = 4096` would otherwise boot a guest of the default size
//! without a word.
//!
//! What a VM's vm.toml (modules/hv) and the host API's api.toml
//! (modules/api) are written in. A crate with no kernel in it and no unsafe:
//! each module is linked on its own and cannot share a layer, and the
//! fuzzer builds it as it is (`fuzz/hv`, the `config` target), holding the
//! reader to never panicking and to giving back what a file's writer wrote.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

/// A value as the file writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(u64),
    Bool(bool),
    List(Vec<String>),
}

impl Value {
    /// What the value is, for a message that says it is not what was wanted.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Str(_) => "a string",
            Value::Int(_) => "a number",
            Value::Bool(_) => "true or false",
            Value::List(_) => "a list",
        }
    }
}

/// What is wrong with the file, and on which line.
pub fn error(line: usize, what: &str) -> String {
    alloc::format!("line {}: {}", line, what)
}

/// What a file has: its keys, and what it does with each one's value.
pub trait Keys {
    /// A key as it is read, before its value: an error for one the file
    /// does not have.
    fn known(&mut self, line: usize, key: &str) -> Result<(), String>;

    /// The key's value, once read and its line ended.
    fn assign(&mut self, line: usize, key: &str, value: Value) -> Result<(), String>;
}

/// `text` read, each key and its value handed to `keys` in the order they
/// come -- or what is wrong with it, the first thing, with its line. `what`
/// is what the file is called in that: "a vm.toml".
pub fn parse(text: &str, what: &str, keys: &mut dyn Keys) -> Result<(), String> {
    let mut p = Parser { s: text.as_bytes(), at: 0, line: 1, what };
    let mut seen: Vec<&str> = Vec::new();
    loop {
        p.blank()?;
        if p.at >= p.s.len() {
            break;
        }
        let line = p.line;
        let name = p.key()?;
        keys.known(line, name)?;
        if seen.contains(&name) {
            return Err(error(line, &alloc::format!("{} is given twice", name)));
        }
        seen.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        seen.push(name);
        p.spaces();
        if !p.eat(b'=') {
            return Err(error(line, &alloc::format!("{} wants = and a value", name)));
        }
        p.spaces();
        let value = p.value()?;
        p.end()?;
        keys.assign(line, name, value)?;
    }
    Ok(())
}

/* ---- the reader ---- */

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
    line: usize,
    /// What the file is called in what is said of it: "a vm.toml"
    what: &'a str,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.at).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    /// Spaces and tabs.
    fn spaces(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.at += 1;
        }
    }

    /// A comment, from its `#` to the end of the line, the newline left.
    fn comment(&mut self) {
        if self.peek() == Some(b'#') {
            while !matches!(self.peek(), None | Some(b'\n' | b'\r')) {
                self.at += 1;
            }
        }
    }

    /// A newline -- `\n` or `\r\n` -- if one is here.
    fn newline(&mut self) -> Result<bool, String> {
        match self.peek() {
            Some(b'\n') => {}
            Some(b'\r') if self.s.get(self.at + 1) == Some(&b'\n') => self.at += 1,
            Some(b'\r') => return Err(error(self.line, "a carriage return with no newline after it")),
            _ => return Ok(false),
        }
        self.at += 1;
        self.line += 1;
        Ok(true)
    }

    /// Blank lines and comments, up to the next thing that is neither.
    fn blank(&mut self) -> Result<(), String> {
        loop {
            self.spaces();
            self.comment();
            if !self.newline()? {
                return Ok(());
            }
        }
    }

    /// What may follow a value: spaces, a comment, the end of the line.
    fn end(&mut self) -> Result<(), String> {
        self.spaces();
        self.comment();
        if self.at >= self.s.len() || self.newline()? {
            return Ok(());
        }
        Err(error(self.line, "more after the value than a comment"))
    }

    /// A bare key: letters, digits, `_` and `-`.
    fn key(&mut self) -> Result<&'a str, String> {
        let start = self.at;
        while matches!(self.peek(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-')) {
            self.at += 1;
        }
        if self.at == start {
            return Err(match self.peek() {
                Some(b'[') => error(self.line, &alloc::format!("a table: {} has keys alone", self.what)),
                Some(b'"' | b'\'') => error(self.line, &alloc::format!("a quoted key: {}'s keys are bare words", self.what)),
                _ => error(self.line, "a key was expected here"),
            });
        }
        if self.peek() == Some(b'.') {
            return Err(error(self.line, &alloc::format!("a dotted key: {}'s keys are bare words", self.what)));
        }
        /* ASCII alone, so this is on a character's boundary. */
        core::str::from_utf8(&self.s[start..self.at]).map_err(|_| error(self.line, "a key was expected here"))
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some(b'"' | b'\'') => Ok(Value::Str(self.string()?)),
            Some(b'[') => self.list(),
            Some(b'0'..=b'9') => Ok(Value::Int(self.number()?)),
            Some(b't' | b'f') => self.boolean(),
            _ => Err(error(self.line, "a value was expected: a string, a number, true or false, or a list")),
        }
    }

    fn boolean(&mut self) -> Result<Value, String> {
        let rest = &self.s[self.at..];
        let (value, len) = if rest.starts_with(b"true") {
            (true, 4)
        } else if rest.starts_with(b"false") {
            (false, 5)
        } else {
            return Err(error(self.line, "a value was expected: a string, a number, true or false, or a list"));
        };
        self.at += len;
        if matches!(self.peek(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-')) {
            return Err(error(self.line, "true or false, then the end of the value"));
        }
        Ok(Value::Bool(value))
    }

    /// A whole number: digits, a `_` between two of them, and no leading 0
    /// but in 0 itself.
    fn number(&mut self) -> Result<u64, String> {
        let start = self.at;
        let mut n: u64 = 0;
        let mut digits = 0usize;
        loop {
            match self.peek() {
                Some(b @ b'0'..=b'9') => {
                    if digits == 1 && self.s[start] == b'0' {
                        return Err(error(self.line, "a number with a leading 0"));
                    }
                    n = n.checked_mul(10).and_then(|n| n.checked_add(u64::from(b - b'0')))
                        .ok_or_else(|| error(self.line, "a number too big"))?;
                    digits += 1;
                    self.at += 1;
                }
                Some(b'_') if matches!(self.s.get(self.at + 1), Some(b'0'..=b'9'))
                    && matches!(self.s.get(self.at.wrapping_sub(1)), Some(b'0'..=b'9')) && self.at > start =>
                {
                    self.at += 1;
                }
                Some(b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'.' | b'+' | b'-' | b':') => {
                    return Err(error(self.line, "a whole number of 0 or more, in decimal, was expected"));
                }
                _ => break,
            }
        }
        Ok(n)
    }

    /// A string, basic or literal, on one line.
    fn string(&mut self) -> Result<String, String> {
        let quote = self.peek().ok_or_else(|| error(self.line, "a string was expected"))?;
        self.at += 1;
        if self.s[self.at..].starts_with(&[quote, quote]) {
            return Err(error(self.line, &alloc::format!("a multi-line string: {}'s strings are on one line", self.what)));
        }
        let mut out: Vec<u8> = Vec::new();
        loop {
            let b = self.peek().ok_or_else(|| error(self.line, "a string with no end"))?;
            self.at += 1;
            match b {
                _ if b == quote => break,
                b'\n' | b'\r' => return Err(error(self.line, "a string with no end on its line")),
                b'\\' if quote == b'"' => self.escape(&mut out)?,
                0x00..=0x08 | 0x0A..=0x1F | 0x7F => {
                    return Err(error(self.line, "a control character in a string -- \\u0000 and the like for one"));
                }
                _ => {
                    out.try_reserve(1).map_err(|_| String::from("out of memory"))?;
                    out.push(b);
                }
            }
        }
        /* The text was UTF-8, and it was copied whole between quotes, with
         * escapes made characters: still UTF-8. */
        String::from_utf8(out).map_err(|_| error(self.line, "a string that is not UTF-8"))
    }

    /// What follows a backslash in a basic string, into `out`.
    fn escape(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        let b = self.peek().ok_or_else(|| error(self.line, "a string with no end"))?;
        self.at += 1;
        let c = match b {
            b'b' => '\u{8}',
            b't' => '\t',
            b'n' => '\n',
            b'f' => '\u{c}',
            b'r' => '\r',
            b'"' => '"',
            b'\\' => '\\',
            b'u' | b'U' => {
                let len = if b == b'u' { 4 } else { 8 };
                let hex = self.s.get(self.at..self.at + len)
                    .ok_or_else(|| error(self.line, "a \\u escape cut short"))?;
                let mut v: u32 = 0;
                for &h in hex {
                    let d = (h as char).to_digit(16).ok_or_else(|| error(self.line, "a \\u escape that is not hex"))?;
                    v = v * 16 + d;
                }
                self.at += len;
                char::from_u32(v).ok_or_else(|| error(self.line, "a \\u escape that is no character"))?
            }
            _ => return Err(error(self.line, "an escape a TOML string does not have")),
        };
        let mut buf = [0u8; 4];
        let bytes = c.encode_utf8(&mut buf).as_bytes();
        out.try_reserve(bytes.len()).map_err(|_| String::from("out of memory"))?;
        out.extend_from_slice(bytes);
        Ok(())
    }

    /// A list of strings, over as many lines as it takes, a comma after the
    /// last allowed.
    fn list(&mut self) -> Result<Value, String> {
        self.at += 1;
        let mut items = Vec::new();
        loop {
            self.blank()?;
            if self.eat(b']') {
                return Ok(Value::List(items));
            }
            match self.peek() {
                Some(b'"' | b'\'') => {}
                None => return Err(error(self.line, "a list with no end")),
                _ => return Err(error(self.line, "a list of strings: each in quotes")),
            }
            let item = self.string()?;
            items.try_reserve(1).map_err(|_| String::from("out of memory"))?;
            items.push(item);
            self.blank()?;
            if self.eat(b']') {
                return Ok(Value::List(items));
            }
            if !self.eat(b',') {
                return Err(error(self.line, "a comma or ] after a list's string"));
            }
        }
    }
}

/* ---- the writer's ---- */

/// `key = "value"` and a newline.
pub fn put(s: &mut String, key: &str, value: &str) {
    s.push_str(key);
    s.push_str(" = ");
    quoted(s, value);
    s.push('\n');
}

/// `value` as a basic string: in quotes, with what may not stand in one
/// escaped.
pub fn quoted(s: &mut String, value: &str) {
    s.push('"');
    for c in value.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\t' => s.push_str("\\t"),
            '\r' => s.push_str("\\r"),
            '\u{0}'..='\u{1f}' | '\u{7f}' => {
                let _ = write!(s, "\\u{:04X}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
}
