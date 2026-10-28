//! A VM's configuration, `vm.toml` in its folder: what `hv start <name>`
//! boots it with, written by `hv create` and by hand. And `Options`, what a
//! command line or the file says of a guest before anything is filled in --
//! which `hv start`'s words are read into too (`guest::words`), so that the
//! two say a guest the same way.
//!
//! The file is the strict subset of TOML the `conf` crate reads: what it
//! cannot read it refuses, saying the line, rather than read it some other
//! way. So is a key vm.toml does not have, and one given twice -- `memroy =
//! 4096` would otherwise boot a guest of the default size without a word.
//!
//! No kernel in it: the fuzzer builds it as it is (`fuzz/hv`, the `config`
//! target), and holds `parse` to never panicking, and `parse` and `render`
//! to giving each other back what they were given.

use alloc::string::String;
use alloc::vec::Vec;
use conf::{error, put, quoted, Value};
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
    /// Kept from what is done to it by mistake: the host API stops it and
    /// deletes it only when told `force`, and `hv delete` only with
    /// `force` -- the management panel's own VM has it.
    pub protected: bool,
}

impl Config {
    pub fn new(options: Options) -> Config {
        Config { options, console_log: true, protected: false }
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
    Protected,
}

/// The keys there are, in the order `render` writes them.
const KEYS: [(&str, Key); 22] = [
    ("kernel", Key::Kernel),
    ("initrd", Key::Initrd),
    ("cmdline", Key::Cmdline),
    ("memory", Key::Memory),
    ("cpus", Key::Cpus),
    ("cpu", Key::Cpu),
    ("disks", Key::Disks),
    ("net", Key::Net),
    ("network", Key::Network),
    ("port", Key::Port),
    ("forward", Key::Forward),
    ("restart", Key::Restart),
    ("protected", Key::Protected),
    ("console_log", Key::ConsoleLog),
    ("log", Key::Log),
    ("input", Key::Input),
    ("ioapic", Key::Ioapic),
    ("xapic", Key::Xapic),
    ("apicv", Key::Apicv),
    ("rxmod", Key::Rxmod),
    ("offload", Key::Offload),
    ("uuid", Key::Uuid),
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

/// vm.toml's keys, as `conf` reads the file: a key the table does not have
/// is refused before its value is read.
struct Reader {
    c: Config,
}

impl conf::Keys for Reader {
    fn known(&mut self, line: usize, name: &str) -> Result<(), String> {
        match KEYS.iter().any(|(n, _)| *n == name) {
            true => Ok(()),
            false => Err(error(line, &alloc::format!("\"{}\" is not a key a vm.toml has", name))),
        }
    }

    fn assign(&mut self, line: usize, name: &str, value: Value) -> Result<(), String> {
        let key = KEYS.iter().find(|(n, _)| *n == name).map(|&(_, k)| k)
            .ok_or_else(|| error(line, &alloc::format!("\"{}\" is not a key a vm.toml has", name)))?;
        assign(&mut self.c, key, name, value, line)
    }
}

/// What a vm.toml is called in what is said of one.
const WHAT: &str = "a vm.toml";

/// The file's text read as the configuration it is, or what is wrong with
/// it -- the first thing, with its line.
pub fn parse(text: &str) -> Result<Config, String> {
    let mut reader = Reader { c: Config::new(Options::default()) };
    conf::parse(text, WHAT, &mut reader)?;
    let c = reader.c;
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
        (Key::Protected, Value::Bool(b)) => c.protected = b,
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

/// What a command line says without its `ip=`: the switch gives a guest
/// its address at every start, and a clone's source's would be another's.
/// The line as it was when it has none; its other words in their order,
/// a space apart, when it has.
pub fn without_ip(cmdline: &str) -> String {
    if !cmdline.split_ascii_whitespace().any(|w| w.starts_with("ip=")) {
        return String::from(cmdline);
    }
    let mut out = String::new();
    for word in cmdline.split_ascii_whitespace().filter(|w| !w.starts_with("ip=")) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
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

/* ---- the writer ---- */

/// What every vm.toml `render` writes begins with.
const HEADER: &str = "# A VM of nos's hypervisor: hv start <this folder's name> boots it (docs/hypervisor.md)\n";

/// What a vm.toml says of `key` -- named `name` -- for `c`: its line, the
/// newline with it, or None where the file says nothing of it, `c` having
/// it as a file without the key does. Err out of memory.
fn line(c: &Config, key: Key, name: &str) -> Result<Option<String>, String> {
    let o = &c.options;
    /* Room for the line: a string's every character may be written as an
     * escape of six. */
    let text = |v: &str| v.len() * 6 + 2;
    let room = name.len() + 4 + match key {
        Key::Kernel => text(&o.kernel),
        Key::Initrd => o.initrd.as_deref().map_or(0, text),
        Key::Cmdline => o.cmdline.as_deref().map_or(0, text),
        Key::Input => text(&o.input),
        Key::Network => o.network.as_deref().map_or(0, text),
        Key::Disks => o.disks.iter().map(|(d, _)| text(d) + 5).sum::<usize>() + 2,
        Key::Forward => o.forwards.len() * 16 + 2,
        _ => 48,
    };
    let mut s = String::new();
    s.try_reserve(room).map_err(|_| String::from("out of memory"))?;
    let said = |s: &mut String, value: &dyn core::fmt::Display| {
        let _ = writeln!(s, "{} = {}", name, value);
    };
    match key {
        Key::Kernel => put(&mut s, name, &o.kernel),
        Key::Initrd => match &o.initrd {
            Some(v) => put(&mut s, name, v),
            None => return Ok(None),
        },
        Key::Cmdline => match &o.cmdline {
            Some(v) => put(&mut s, name, v),
            None => return Ok(None),
        },
        Key::Memory => match o.mem_mib {
            Some(v) => said(&mut s, &v),
            None => return Ok(None),
        },
        Key::Cpus => match o.cpus {
            Some(v) => said(&mut s, &v),
            None => return Ok(None),
        },
        Key::Cpu => match o.cpu {
            Some(v) => said(&mut s, &v),
            None => return Ok(None),
        },
        Key::Disks => {
            if o.disks.is_empty() {
                return Ok(None);
            }
            s.push_str(name);
            s.push_str(" = [");
            for (i, (path, ro)) in o.disks.iter().enumerate() {
                if i != 0 {
                    s.push_str(", ");
                }
                let mut word = String::new();
                word.try_reserve(path.len() + 3).map_err(|_| String::from("out of memory"))?;
                word.push_str(path);
                if *ro {
                    word.push_str(":ro");
                }
                quoted(&mut s, &word);
            }
            s.push_str("]\n");
        }
        Key::Net if o.net => said(&mut s, &true),
        Key::Network => match &o.network {
            Some(v) => put(&mut s, name, v),
            None => return Ok(None),
        },
        Key::Port => match o.port {
            Some(v) => said(&mut s, &v),
            None => return Ok(None),
        },
        Key::Forward => {
            if o.forwards.is_empty() {
                return Ok(None);
            }
            s.push_str(name);
            s.push_str(" = [");
            for (i, (host, guest)) in o.forwards.iter().enumerate() {
                let _ = write!(s, "{}\"{}:{}\"", if i == 0 { "" } else { ", " }, host, guest);
            }
            s.push_str("]\n");
        }
        Key::Restart if o.restart => said(&mut s, &true),
        Key::Protected if c.protected => said(&mut s, &true),
        Key::ConsoleLog => said(&mut s, &c.console_log),
        Key::Log if o.log => said(&mut s, &true),
        Key::Input if !o.input.is_empty() => put(&mut s, name, &o.input),
        Key::Ioapic if o.ioapic => said(&mut s, &true),
        Key::Xapic if o.xapic => said(&mut s, &true),
        Key::Apicv => match o.apicv {
            Some(v) => said(&mut s, &v),
            None => return Ok(None),
        },
        Key::Rxmod => match o.rxmod_us {
            Some(v) => said(&mut s, &v),
            None => return Ok(None),
        },
        Key::Offload if o.nooffload => said(&mut s, &false),
        Key::Uuid => match &o.uuid {
            Some(u) => put(&mut s, name, &uuid_text(u)),
            None => return Ok(None),
        },
        Key::Net | Key::Restart | Key::Protected | Key::Log | Key::Input | Key::Ioapic | Key::Xapic
        | Key::Offload => return Ok(None),
    }
    Ok(Some(s))
}

/// The configuration as a vm.toml says it: what `parse` reads back as the
/// same configuration. None out of memory.
pub fn render(c: &Config) -> Option<String> {
    let mut s = String::new();
    s.try_reserve(HEADER.len() + 512).ok()?;
    s.push_str(HEADER);
    for &(name, key) in KEYS.iter() {
        if let Some(l) = line(c, key, name).ok()? {
            s.try_reserve(l.len()).ok()?;
            s.push_str(&l);
        }
    }
    Some(s)
}

/// `text`, a vm.toml, made to say `new`: the line of every key whose value
/// differs from the text's replaced, taken out or added at the end
/// (`conf::edit`), and every other line -- the comments, the blank lines,
/// the keys `new` leaves as they were -- as the operator wrote it. What
/// `text` cannot be read as is refused as `parse` refuses it.
pub fn rewrite(text: &str, new: &Config) -> Result<String, String> {
    let old = parse(text)?;
    let mut edits = Vec::new();
    edits.try_reserve_exact(KEYS.len()).map_err(|_| String::from("out of memory"))?;
    for &(name, key) in KEYS.iter() {
        let now = line(new, key, name)?;
        if line(&old, key, name)? != now {
            edits.push(conf::Edit { key: name, line: now });
        }
    }
    let mut reader = Reader { c: Config::new(Options::default()) };
    conf::edit(text, WHAT, &mut reader, &edits)
}
