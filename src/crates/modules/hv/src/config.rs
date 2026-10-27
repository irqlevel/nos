//! A VM's configuration, `vm.toml` in its folder: what `hv start <name>`
//! boots it with, written by `hv create` and by hand. And `Options`, what a
//! command line or the file says of a guest before anything is filled in --
//! which `hv start`'s words are read into too (`guest::words`), so that the
//! two say a guest the same way.
//!
//! The file is a strict subset of TOML: `key = value` a line, `#` to the end
//! of a line a comment, and for values a string -- "basic", with TOML's
//! escapes, or 'literal' -- a whole number, `true` or `false`, and a list of
//! strings in brackets, which may run over several lines. No tables, no
//! dotted or quoted keys, nothing else: what this cannot read it refuses,
//! saying the line, rather than read it some other way. So is a key it does
//! not know, and one given twice -- `memroy = 4096` would otherwise boot a
//! guest of the default size without a word.
//!
//! No kernel in it: the fuzzer builds it as it is (`fuzz/hv`, the `config`
//! target), and holds `parse` to never panicking, and `parse` and `render`
//! to giving each other back what they were given.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

/// The most a vm.toml may be: a configuration, not a document.
pub const MAX_BYTES: usize = 64 * 1024;

/// What a command line or a vm.toml says of a guest, as said: what is not
/// said is `None`, `false` or empty, and `guest::spec` gives it its default
/// and checks the whole.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Its bzImage.
    pub kernel: String,
    pub initrd: Option<String>,
    /// Its disks, `vda` on: each one's path, and whether it is read-only.
    pub disks: Vec<(String, bool)>,
    pub mem_mib: Option<u64>,
    pub cmdline: Option<String>,
    /// What is typed at its console once it is at a prompt.
    pub input: String,
    /// How long `hv boot` runs it; no key in the file.
    pub secs: Option<u64>,
    /// The host CPU its first CPU runs on.
    pub cpu: Option<u32>,
    pub cpus: Option<u32>,
    pub xapic: bool,
    pub ioapic: bool,
    pub apicv: Option<bool>,
    pub rxmod_us: Option<u64>,
    pub nooffload: bool,
    /// Its console into the kernel log too.
    pub log: bool,
    pub restart: bool,
    pub net: bool,
    /// Its port on the guests' switch, for an address and a MAC that are the
    /// same at every start; the first free one when not said.
    pub port: Option<u32>,
    /// nos's ports relayed to its own once it runs: (nos's, the guest's).
    pub forwards: Vec<(u16, u16)>,
    /// The network on the guests' switch it is on, whose other guests it
    /// reaches; alone, reaching none, when not said.
    pub network: Option<String>,
    /// The UUID its SMBIOS gives it, as RFC 4122 writes one: what its OS
    /// knows the machine by, the same at every start. A VM with a folder is
    /// given one at its first start, written into its vm.toml.
    pub uuid: Option<[u8; 16]>,
}

/// A vm.toml: the options, and what only a VM with a folder has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub options: Options,
    /// Its console written to `console.log` in its folder: on unless told.
    pub console_log: bool,
}

impl Config {
    pub fn new(options: Options) -> Config {
        Config { options, console_log: true }
    }
}

/// The keys there are, and what each takes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Kernel,
    Initrd,
    Cmdline,
    Memory,
    Cpus,
    Cpu,
    Disks,
    Net,
    Port,
    Forward,
    Restart,
    ConsoleLog,
    Log,
    Input,
    Ioapic,
    Xapic,
    Apicv,
    Rxmod,
    Offload,
    Uuid,
    Network,
}

const KEYS: [(&str, Key); 21] = [
    ("kernel", Key::Kernel),
    ("initrd", Key::Initrd),
    ("cmdline", Key::Cmdline),
    ("memory", Key::Memory),
    ("cpus", Key::Cpus),
    ("cpu", Key::Cpu),
    ("disks", Key::Disks),
    ("net", Key::Net),
    ("port", Key::Port),
    ("forward", Key::Forward),
    ("restart", Key::Restart),
    ("console_log", Key::ConsoleLog),
    ("log", Key::Log),
    ("input", Key::Input),
    ("ioapic", Key::Ioapic),
    ("xapic", Key::Xapic),
    ("apicv", Key::Apicv),
    ("rxmod", Key::Rxmod),
    ("offload", Key::Offload),
    ("uuid", Key::Uuid),
    ("network", Key::Network),
];

/// The longest a network's name is.
pub const NETWORK_NAME_MAX: usize = 31;

/// Whether `name` may be a network's on the guests' switch: a letter or a
/// digit, then letters, digits, `.`, `_` and `-`, at most
/// `NETWORK_NAME_MAX` in all -- as a VM's name is.
pub fn valid_network(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= NETWORK_NAME_MAX && b[0].is_ascii_alphanumeric()
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// A value as the file writes it.
enum Value {
    Str(String),
    Int(u64),
    Bool(bool),
    List(Vec<String>),
}

impl Value {
    fn kind(&self) -> &'static str {
        match self {
            Value::Str(_) => "a string",
            Value::Int(_) => "a number",
            Value::Bool(_) => "true or false",
            Value::List(_) => "a list",
        }
    }
}

/// What is wrong with the file, and on which line.
fn error(line: usize, what: &str) -> String {
    alloc::format!("line {}: {}", line, what)
}

/// The file's text read as the configuration it is, or what is wrong with
/// it -- the first thing, with its line.
pub fn parse(text: &str) -> Result<Config, String> {
    let mut p = Parser { s: text.as_bytes(), at: 0, line: 1 };
    let mut c = Config::new(Options::default());
    let mut seen: Vec<Key> = Vec::new();
    loop {
        p.blank()?;
        if p.at >= p.s.len() {
            break;
        }
        let line = p.line;
        let name = p.key()?;
        let key = KEYS.iter().find(|(n, _)| *n == name).map(|&(_, k)| k)
            .ok_or_else(|| error(line, &alloc::format!("\"{}\" is not a key a vm.toml has", name)))?;
        if seen.contains(&key) {
            return Err(error(line, &alloc::format!("{} is given twice", name)));
        }
        seen.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        seen.push(key);
        p.spaces();
        if !p.eat(b'=') {
            return Err(error(line, &alloc::format!("{} wants = and a value", name)));
        }
        p.spaces();
        let value = p.value()?;
        p.end()?;
        assign(&mut c, key, name, value, line)?;
    }
    if c.options.kernel.is_empty() {
        return Err(String::from("no kernel: a vm.toml says kernel = \"<its bzImage>\""));
    }
    Ok(c)
}

/// `value` as `key` takes it, into `c`.
fn assign(c: &mut Config, key: Key, name: &str, value: Value, line: usize) -> Result<(), String> {
    let o = &mut c.options;
    let wrong = |want: &str, got: &Value| error(line, &alloc::format!("{} wants {}, not {}", name, want, got.kind()));
    match (key, value) {
        (Key::Kernel, Value::Str(s)) if !s.is_empty() => o.kernel = s,
        (Key::Initrd, Value::Str(s)) if !s.is_empty() => o.initrd = Some(s),
        (Key::Kernel | Key::Initrd, Value::Str(_)) => return Err(error(line, &alloc::format!("{} is an empty path", name))),
        (Key::Cmdline, Value::Str(s)) => o.cmdline = Some(s),
        (Key::Network, Value::Str(s)) => {
            if !valid_network(&s) {
                return Err(error(line, &alloc::format!("\"{}\" is not a network's name -- a letter or a digit, then \
                                                        letters, digits, . _ and -, at most {}", s, NETWORK_NAME_MAX)));
            }
            o.network = Some(s);
        }
        (Key::Uuid, Value::Str(s)) => {
            o.uuid = Some(uuid(&s).ok_or_else(|| error(line, &alloc::format!("\"{}\" is not a UUID -- \
                                                                              8-4-4-4-12 hex digits", s)))?);
        }
        (Key::Input, Value::Str(s)) => o.input = s,
        (Key::Memory, Value::Int(n)) => o.mem_mib = Some(n),
        (Key::Rxmod, Value::Int(n)) => o.rxmod_us = Some(n),
        (Key::Cpus | Key::Cpu | Key::Port, Value::Int(n)) => {
            let n = u32::try_from(n).map_err(|_| error(line, &alloc::format!("{} is too big", name)))?;
            match key {
                Key::Cpus => o.cpus = Some(n),
                Key::Cpu => o.cpu = Some(n),
                _ => o.port = Some(n),
            }
        }
        (Key::Net, Value::Bool(b)) => o.net = b,
        (Key::Restart, Value::Bool(b)) => o.restart = b,
        (Key::ConsoleLog, Value::Bool(b)) => c.console_log = b,
        (Key::Log, Value::Bool(b)) => o.log = b,
        (Key::Ioapic, Value::Bool(b)) => o.ioapic = b,
        (Key::Xapic, Value::Bool(b)) => o.xapic = b,
        (Key::Apicv, Value::Bool(b)) => o.apicv = Some(b),
        (Key::Offload, Value::Bool(b)) => o.nooffload = !b,
        (Key::Disks, Value::List(list)) => {
            o.disks.try_reserve_exact(list.len()).map_err(|_| String::from("out of memory"))?;
            for d in list {
                let disk = disk(&d).ok_or_else(|| error(line, &alloc::format!("\"{}\" is not a disk -- a path, \
                                                                              and :ro after it for one read-only", d)))?;
                o.disks.push(disk);
            }
        }
        (Key::Forward, Value::List(list)) => {
            o.forwards.try_reserve_exact(list.len()).map_err(|_| String::from("out of memory"))?;
            for f in list {
                let pair = forward(&f).ok_or_else(|| error(line, &alloc::format!("\"{}\" is not a forward -- nos's \
                                                                                 port:the guest's, each 1..65535", f)))?;
                o.forwards.push(pair);
            }
        }
        (Key::Kernel | Key::Initrd | Key::Cmdline | Key::Input | Key::Uuid | Key::Network, v) => {
            return Err(wrong("a string", &v));
        }
        (Key::Memory | Key::Rxmod | Key::Cpus | Key::Cpu | Key::Port, v) => return Err(wrong("a number", &v)),
        (Key::Disks | Key::Forward, v) => return Err(wrong("a list of strings", &v)),
        (_, v) => return Err(wrong("true or false", &v)),
    }
    Ok(())
}

/// A UUID as RFC 4122 writes one -- 8-4-4-4-12 hex digits, either case --
/// its bytes most significant first.
pub fn uuid(text: &str) -> Option<[u8; 16]> {
    let b = text.as_bytes();
    if b.len() != UUID_TEXT || UUID_DASHES.iter().any(|&at| b[at] != b'-') {
        return None;
    }
    let mut digits = b.iter().enumerate().filter(|(i, _)| !UUID_DASHES.contains(i)).map(|(_, &c)| c);
    let mut out = [0u8; 16];
    for byte in out.iter_mut() {
        let hi = char::from(digits.next()?).to_digit(16)?;
        let lo = char::from(digits.next()?).to_digit(16)?;
        *byte = (hi << 4 | lo) as u8;
    }
    Some(out)
}

/// A UUID as RFC 4122 writes one, lower case.
pub fn uuid_text(u: &[u8; 16]) -> String {
    let mut s = String::new();
    for (i, byte) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{:02x}", byte);
    }
    s
}

/// A UUID's text: its length, and where its dashes are.
const UUID_TEXT: usize = 36;
const UUID_DASHES: [usize; 4] = [8, 13, 18, 23];

/// A disk as a word and the file say it: a path, `:ro` after it for one the
/// guest cannot write.
pub fn disk(word: &str) -> Option<(String, bool)> {
    let (path, ro) = match word.strip_suffix(":ro") {
        Some(path) => (path, true),
        None => (word, false),
    };
    if path.is_empty() {
        return None;
    }
    let mut s = String::new();
    s.try_reserve_exact(path.len()).ok()?;
    s.push_str(path);
    Some((s, ro))
}

/// A forward as a word and the file say it: `<nos's port>:<the guest's>`.
pub fn forward(word: &str) -> Option<(u16, u16)> {
    let (host, guest) = word.split_once(':')?;
    let port = |s: &str| -> Option<u16> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse::<u16>().ok().filter(|p| *p != 0)
    };
    Some((port(host)?, port(guest)?))
}

/* ---- the reader ---- */

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
    line: usize,
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
            return Err(error(self.line, match self.peek() {
                Some(b'[') => "a table: a vm.toml has keys alone",
                Some(b'"' | b'\'') => "a quoted key: a vm.toml's keys are bare words",
                _ => "a key was expected here",
            }));
        }
        if self.peek() == Some(b'.') {
            return Err(error(self.line, "a dotted key: a vm.toml's keys are bare words"));
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
            return Err(error(self.line, "a multi-line string: a vm.toml's strings are on one line"));
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

/* ---- the writer ---- */

/// The configuration as a vm.toml says it: what `parse` reads back as the
/// same configuration. None out of memory.
pub fn render(c: &Config) -> Option<String> {
    let o = &c.options;
    let mut s = String::new();
    s.try_reserve(512 + o.kernel.len() + o.cmdline.as_ref().map_or(0, |l| l.len()) * 2 + o.input.len() * 6
                  + o.disks.iter().map(|(d, _)| d.len() * 2 + 8).sum::<usize>() + o.forwards.len() * 16).ok()?;
    s.push_str("# A VM of nos's hypervisor: hv start <this folder's name> boots it (docs/hypervisor.md)\n");
    put(&mut s, "kernel", &o.kernel);
    if let Some(v) = &o.initrd {
        put(&mut s, "initrd", v);
    }
    if let Some(v) = &o.cmdline {
        put(&mut s, "cmdline", v);
    }
    if let Some(v) = o.mem_mib {
        let _ = writeln!(s, "memory = {}", v);
    }
    if let Some(v) = o.cpus {
        let _ = writeln!(s, "cpus = {}", v);
    }
    if let Some(v) = o.cpu {
        let _ = writeln!(s, "cpu = {}", v);
    }
    if !o.disks.is_empty() {
        s.push_str("disks = [");
        for (i, (path, ro)) in o.disks.iter().enumerate() {
            if i != 0 {
                s.push_str(", ");
            }
            let mut word = String::new();
            word.try_reserve(path.len() + 3).ok()?;
            word.push_str(path);
            if *ro {
                word.push_str(":ro");
            }
            quoted(&mut s, &word);
        }
        s.push_str("]\n");
    }
    if o.net {
        s.push_str("net = true\n");
    }
    if let Some(v) = &o.network {
        put(&mut s, "network", v);
    }
    if let Some(v) = o.port {
        let _ = writeln!(s, "port = {}", v);
    }
    if !o.forwards.is_empty() {
        s.push_str("forward = [");
        for (i, (host, guest)) in o.forwards.iter().enumerate() {
            let _ = write!(s, "{}\"{}:{}\"", if i == 0 { "" } else { ", " }, host, guest);
        }
        s.push_str("]\n");
    }
    if o.restart {
        s.push_str("restart = true\n");
    }
    let _ = writeln!(s, "console_log = {}", c.console_log);
    if o.log {
        s.push_str("log = true\n");
    }
    if !o.input.is_empty() {
        put(&mut s, "input", &o.input);
    }
    if o.ioapic {
        s.push_str("ioapic = true\n");
    }
    if o.xapic {
        s.push_str("xapic = true\n");
    }
    if let Some(v) = o.apicv {
        let _ = writeln!(s, "apicv = {}", v);
    }
    if let Some(v) = o.rxmod_us {
        let _ = writeln!(s, "rxmod = {}", v);
    }
    if o.nooffload {
        s.push_str("offload = false\n");
    }
    if let Some(u) = &o.uuid {
        put(&mut s, "uuid", &uuid_text(u));
    }
    Some(s)
}

/// `key = "value"` and a newline.
fn put(s: &mut String, key: &str, value: &str) {
    s.push_str(key);
    s.push_str(" = ");
    quoted(s, value);
    s.push('\n');
}

/// `value` as a basic string: in quotes, with what may not stand in one
/// escaped.
fn quoted(s: &mut String, value: &str) {
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
