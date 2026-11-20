//! eth0's settings -- its address, its gateway, its resolver, and whether it
//! asks DHCP for them -- from `/etc/net.toml` on the root filesystem: what
//! nos-install writes as it installs, and what is edited afterwards (`edit
//! /etc/net.toml`, then a reboot). In the strict TOML `conf` reads:
//!
//!     # A network with no DHCP server
//!     ip = "203.0.113.5/24"
//!     gw = "203.0.113.1"
//!     dns = "203.0.113.53"
//!
//! or `dhcp = true`, which is also what a file without `ip` means: an
//! address asked of DHCP as the boot starts, and the lease's resolver --
//! or, with `dns` beside it, a resolver on that server from the start.
//! `dhcp = false` without an `ip` leaves eth0 with no address until the
//! `dhcp` command is run. The file is taken whole or not at all: a key it
//! does not have, a value that is not one, a gateway off the address's
//! subnet, `dhcp = true` beside an `ip` -- each is refused with its line, and
//! the boot says so.
//!
//! A boot with no such file -- an ISO's, QEMU's, a test's, one whose root
//! was not found -- or with one refused goes by the kernel command line's
//! `ip=`, `gw=`, `dns=` and `dhcp=`, as every boot did before the file: a
//! kernel from before it, which a rollback boots under the same GRUB entry,
//! still does. Where the file is taken the command line's are not used, and
//! the boot names them.
//!
//! Both are read here, through the same checks, once the root is mounted
//! and before anything sends -- the shell's DHCP, the netconsole, the shell
//! over UDP (`rust_net_settings_from_root`); what the boot decided is what
//! every one of them asks after (`current`).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use conf::{error, Value};
use fs::vfs::{Open, Vfs, OPEN_READ};
use fs::vnode::Kind;
use kcore::klog;
use kcore::once::OnceBox;

use crate::wire::{self, Ipv4 as Ip};

/// Where the settings are. One literal, kept whole: nos-install looks for
/// it in a kernel's image to tell a kernel that reads the file from one
/// from before it, which still needs the command line's words.
pub const PATH: &str = "/etc/net.toml";

/// The most the file may be: it has four keys.
pub const MAX_BYTES: usize = 4096;

/// The longest parameter the C++ takes off the command line
/// (`Parameters::MAX_PARAM_LEN`): a longer word it has named and skipped.
const PARAM_MAX: usize = 48;
/// The longest command line there is, its NUL among it
/// (`Parameters::CMDLINE_LEN`).
const CMDLINE_MAX: usize = 256;

const PREFIX_MAX: u32 = 32;

/// The four keys, on the command line as in the file.
const KEYS: [&str; 4] = ["ip", "gw", "dns", "dhcp"];

/// When eth0 asks DHCP for its address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dhcp {
    /// As the boot starts: `dhcp=auto`, the file's `dhcp = true` -- and a
    /// file that says nothing of an address.
    Boot,
    /// When the `dhcp` command is run: `dhcp=on`, the command line's
    /// default; the file's `dhcp = false`, or its `ip`.
    Command,
    /// Never, the command refused too: `dhcp=off`.
    Off,
}

/// A static address: eth0's, its subnet's mask and its gateway -- 0 for
/// none, the subnet alone reached. Host byte order, as everything here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Address {
    pub ip: u32,
    pub mask: u32,
    pub gw: u32,
}

/// Where the settings came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    CommandLine,
    File,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Settings {
    pub address: Option<Address>,
    /// A resolver's server, given: on it from the start, a lease's not
    /// taken in its place. 0 for none.
    pub dns: u32,
    /// Whether a lease's DNS server is taken where none was given: the
    /// command line's `dns=on`, and always the file's.
    pub lease_dns: bool,
    pub dhcp: Dhcp,
    pub source: Source,
}

impl Settings {
    /// A command line with none of the four: DHCP only when asked, by the
    /// command, and no resolver unless one is.
    const NONE: Settings = Settings {
        address: None, dns: 0, lease_dns: false, dhcp: Dhcp::Command, source: Source::CommandLine,
    };
}

/// What the settings come to, as the boot and `net` say it.
impl fmt::Display for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.address, self.dhcp) {
            (Some(a), _) => {
                write!(f, "{}/{}", Ip(a.ip), a.mask.count_ones())?;
                if a.gw != 0 {
                    write!(f, ", gateway {}", Ip(a.gw))?;
                }
            }
            (None, Dhcp::Boot) => f.write_str("DHCP")?,
            (None, Dhcp::Command) => f.write_str("no address until the dhcp command")?,
            (None, Dhcp::Off) => f.write_str("no address, DHCP off")?,
        }
        if self.dns != 0 {
            write!(f, ", resolver {}", Ip(self.dns))
        } else if self.lease_dns && self.dhcp != Dhcp::Off {
            f.write_str(", the lease's resolver")
        } else {
            Ok(())
        }
    }
}

/* ---- what a value may be ---- */

const NOT_ADDRESS_PREFIX: &str = "is not an address and its prefix length, A.B.C.D/N";
const NOT_ADDRESS: &str = "is not an address, A.B.C.D";
const NOT_HOST: &str = "is not an address a host may have";
const NOT_OWN: &str = "is its subnet's network or broadcast address";

/// Whether `addr` is neither the network nor the broadcast address of its
/// subnet of `mask` -- which a subnet of one or two addresses has none of
/// (RFC 3021).
fn own_address(addr: u32, mask: u32) -> bool {
    let host = addr & !mask;
    mask.count_zeros() < 2 || (host != 0 && host != !mask)
}

/// `A.B.C.D/N`: an address a host may have, and its subnet's mask.
fn address(text: &str) -> Result<(u32, u32), &'static str> {
    let (ip, prefix) = text.split_once('/').ok_or(NOT_ADDRESS_PREFIX)?;
    let ip = wire::parse_ipv4(ip.as_bytes()).ok_or(NOT_ADDRESS_PREFIX)?;
    if prefix.is_empty() || prefix.len() > 2 || !prefix.bytes().all(|b| b.is_ascii_digit()) {
        return Err(NOT_ADDRESS_PREFIX);
    }
    let bits: u32 = prefix.parse().map_err(|_| NOT_ADDRESS_PREFIX)?;
    if bits == 0 || bits > PREFIX_MAX {
        return Err("has a prefix length that is not 1 to 32");
    }
    if !wire::host_address(ip, 0) {
        return Err(NOT_HOST);
    }
    /* 32 - bits is 0 to 31: a shift a u32 takes */
    let mask = u32::MAX << (PREFIX_MAX - bits);
    if !own_address(ip, mask) {
        return Err(NOT_OWN);
    }
    Ok((ip, mask))
}

/// `A.B.C.D`: an address a host may have -- a gateway's, a resolver's.
fn host(text: &str) -> Result<u32, &'static str> {
    let ip = wire::parse_ipv4(text.as_bytes()).ok_or(NOT_ADDRESS)?;
    if wire::host_address(ip, 0) { Ok(ip) } else { Err(NOT_HOST) }
}

/// Whether `gw` can be the gateway of `ip` on its subnet of `mask`: another
/// address of that subnet, and not its network or broadcast address.
fn gateway_of(gw: u32, ip: u32, mask: u32) -> Result<(), &'static str> {
    if gw == ip || (gw ^ ip) & mask != 0 {
        return Err("is not another address of ip's subnet");
    }
    if !own_address(gw, mask) {
        return Err(NOT_OWN);
    }
    Ok(())
}

/* ---- the command line ---- */

/// The command line's words that are the four's, as the C++ splits it: at
/// spaces, `key=value`, a value to it, no longer than it takes a parameter.
fn network_words(line: &[u8]) -> impl Iterator<Item = (&str, &str)> {
    line.split(|b| *b == b' ').filter_map(|word| {
        if word.len() > PARAM_MAX {
            return None;
        }
        let (key, value) = core::str::from_utf8(word).ok()?.split_once('=')?;
        (KEYS.contains(&key) && !value.is_empty()).then_some((key, value))
    })
}

/// What the command line's `ip=`, `gw=`, `dns=` and `dhcp=` say. Each is
/// taken where it can be, and handed to `refuse` -- its key, its value and
/// what is wrong with it -- where it cannot: nothing on the command line
/// stops the boot. The gateway is checked once every word is read, since it
/// is checked against the address; a word given twice is its last good one.
pub fn from_command_line(line: &[u8], refuse: &mut dyn FnMut(&str, &str, &str)) -> Settings {
    let mut s = Settings::NONE;
    let mut ip = None;
    let mut gw = None;
    for (key, value) in network_words(line) {
        match key {
            "dhcp" => match value {
                "auto" => s.dhcp = Dhcp::Boot,
                "on" => s.dhcp = Dhcp::Command,
                "off" => s.dhcp = Dhcp::Off,
                _ => refuse(key, value, "is not auto, on or off"),
            },
            "dns" if value == "on" => s.lease_dns = true,
            "dns" => match host(value) {
                Ok(server) => {
                    s.dns = server;
                    s.lease_dns = true;
                }
                Err(why) => refuse(key, value, why),
            },
            "ip" => match address(value) {
                Ok(given) => ip = Some(given),
                Err(why) => refuse(key, value, why),
            },
            _ => match host(value) {
                Ok(g) => gw = Some((g, value)),
                Err(why) => refuse(key, value, why),
            },
        }
    }

    s.address = ip.map(|(ip, mask)| Address { ip, mask, gw: 0 });
    if let Some((g, text)) = gw {
        match &mut s.address {
            None => refuse("gw", text, "is given without ip="),
            Some(a) => match gateway_of(g, a.ip, a.mask) {
                Ok(()) => a.gw = g,
                Err(why) => refuse("gw", text, why),
            },
        }
    }
    s
}

/// The kernel's command line, as the C++ keeps it.
fn command_line_text(buf: &mut [u8; CMDLINE_MAX]) -> &[u8] {
    let len = kcore::procinfo::cmdline(buf);
    &buf[..len]
}

fn command_line(refuse: &mut dyn FnMut(&str, &str, &str)) -> Settings {
    let mut buf = [0u8; CMDLINE_MAX];
    from_command_line(command_line_text(&mut buf), refuse)
}

/* ---- the file ---- */

/// The file's keys as they are read; checked against each other once all
/// of them are.
#[derive(Default)]
struct Reader {
    ip: Option<(u32, u32)>,
    /// The gateway, and its line.
    gw: Option<(u32, usize)>,
    dns: Option<u32>,
    /// `dhcp`, and its line.
    dhcp: Option<(bool, usize)>,
}

impl conf::Keys for Reader {
    fn known(&mut self, line: usize, key: &str) -> Result<(), String> {
        if KEYS.contains(&key) {
            Ok(())
        } else {
            Err(error(line, &alloc::format!("\"{}\" is not a key net.toml has: ip, gw, dns, dhcp", key)))
        }
    }

    fn assign(&mut self, line: usize, key: &str, value: Value) -> Result<(), String> {
        let text = match value {
            Value::Bool(on) if key == "dhcp" => {
                self.dhcp = Some((on, line));
                return Ok(());
            }
            Value::Str(text) if key != "dhcp" => text,
            other => {
                let want = match key {
                    "dhcp" => "true or false",
                    "ip" => "a string, \"A.B.C.D/N\"",
                    _ => "a string, \"A.B.C.D\"",
                };
                return Err(error(line, &alloc::format!("{} is {}, not {}", key, want, other.kind())));
            }
        };
        let wrong = |why: &str| error(line, &alloc::format!("{} \"{}\" {}", key, text, why));
        match key {
            "ip" => self.ip = Some(address(&text).map_err(wrong)?),
            "gw" => self.gw = Some((host(&text).map_err(wrong)?, line)),
            _ => self.dns = Some(host(&text).map_err(wrong)?),
        }
        Ok(())
    }
}

/// The file's text read: the settings it gives, or what is wrong with it,
/// its line said.
pub fn from_file(text: &str) -> Result<Settings, String> {
    let mut r = Reader::default();
    conf::parse(text, "net.toml", &mut r)?;

    let address = match (r.ip, r.gw) {
        (None, Some((_, line))) => {
            return Err(error(line, "gw without an ip: a gateway is the way off ip's subnet"));
        }
        (None, None) => None,
        (Some((ip, mask)), gw) => {
            if let Some((true, line)) = r.dhcp {
                return Err(error(line, "dhcp = true beside an ip: eth0's address is the one, or DHCP's"));
            }
            let mut a = Address { ip, mask, gw: 0 };
            if let Some((g, line)) = gw {
                gateway_of(g, ip, mask).map_err(|why| error(line, &alloc::format!("gw \"{}\" {}", Ip(g), why)))?;
                a.gw = g;
            }
            Some(a)
        }
    };
    let dhcp = match (address, r.dhcp) {
        (Some(_), _) | (None, Some((false, _))) => Dhcp::Command,
        (None, _) => Dhcp::Boot,
    };
    Ok(Settings { address, dns: r.dns.unwrap_or(0), lease_dns: true, dhcp, source: Source::File })
}

/// What the root holds at `PATH`.
enum Found {
    Absent,
    Text(String),
    /// There, and not to be read as settings: why.
    Unreadable(String),
}

fn read(vfs: &Vfs) -> Found {
    let Some(stat) = vfs.stat(PATH.as_bytes()) else { return Found::Absent };
    if stat.kind != Kind::File {
        return Found::Unreadable(String::from("is not a file"));
    }
    if stat.size > MAX_BYTES {
        return Found::Unreadable(alloc::format!("is {} bytes, past the {} it may be", stat.size, MAX_BYTES));
    }
    let Some(file) = Open::new(vfs, PATH.as_bytes(), OPEN_READ) else {
        return Found::Unreadable(String::from("could not be opened"));
    };

    /* A byte past the most it may be: a file grown since its size was
     * looked at is not read short */
    let mut buf = Vec::new();
    if buf.try_reserve_exact(MAX_BYTES + 1).is_err() {
        return Found::Unreadable(String::from("could not be read: no memory"));
    }
    buf.resize(MAX_BYTES + 1, 0u8);
    let mut len = 0;
    while len < buf.len() {
        match file.read(&mut buf[len..]) {
            Some(0) => break,
            Some(n) => len += n,
            None => return Found::Unreadable(String::from("could not be read")),
        }
    }
    if len > MAX_BYTES {
        return Found::Unreadable(alloc::format!("is past the {} bytes it may be", MAX_BYTES));
    }
    buf.truncate(len);
    match String::from_utf8(buf) {
        Ok(text) => Found::Text(text),
        Err(_) => Found::Unreadable(String::from("is not UTF-8 text")),
    }
}

/* ---- what the boot decides ---- */

/// What the boot decided, once: `rust_net_settings_from_root`.
static DECIDED: OnceBox<Settings> = OnceBox::new();
/// Why the root's file was not taken, where it was there and was not: for
/// `net` to say.
static REFUSED: OnceBox<String> = OnceBox::new();

/// eth0's settings: what the boot decided -- or, before it has, and where
/// nothing boots (a fuzzer), the command line's.
pub fn current() -> Settings {
    match DECIDED.get() {
        Some(settings) => *settings,
        None => command_line(&mut |_, _, _| {}),
    }
}

/// Why the root's `PATH` was not taken, if it was there.
pub fn refused() -> Option<&'static str> {
    REFUSED.get().map(String::as_str)
}

/// A word of the command line's not taken, said: the boot goes on without it.
fn say_refused(key: &str, value: &str, why: &str) {
    klog!(0, "net: {}={} on the command line {}: not taken", key, value, why);
}

/// Boot, once the root filesystem is mounted and before the shell, the
/// netconsole or the shell over UDP start: eth0's settings, from the root's
/// `PATH` or else the command line, decided for the rest of the boot and
/// said. Called once, by each architecture's boot.
#[no_mangle]
pub extern "C" fn rust_net_settings_from_root() {
    let found = match fs::vfs_instance() {
        Some(vfs) => read(vfs),
        None => Found::Absent,
    };
    let parsed = match found {
        Found::Absent => None,
        Found::Text(text) => Some(from_file(&text)),
        Found::Unreadable(why) => Some(Err(why)),
    };

    let settings = match parsed {
        Some(Ok(settings)) => {
            klog!(0, "net: eth0's settings from {}: {}", PATH, settings);
            /* The command line's words are not used: said, since the system
             * image's GRUB entries pass dhcp=auto dns=on to every boot, for
             * a root with no file and for a kernel from before it */
            let mut buf = [0u8; CMDLINE_MAX];
            let mut unused = String::new();
            for (key, value) in network_words(command_line_text(&mut buf)) {
                if unused.try_reserve(key.len() + value.len() + 2).is_ok() {
                    if !unused.is_empty() {
                        unused.push(' ');
                    }
                    unused.push_str(key);
                    unused.push('=');
                    unused.push_str(value);
                }
            }
            if !unused.is_empty() {
                klog!(0, "net: the command line's {} not used: {} is eth0's settings", unused, PATH);
            }
            settings
        }
        Some(Err(why)) => {
            /* Two lines: a reason long enough to be cut at the log's line
             * length would take what is taken in the file's place with it */
            klog!(0, "net: {} refused, {}", PATH, why);
            let settings = command_line(&mut say_refused);
            klog!(0, "net: eth0's settings from the command line: {}", settings);
            let _ = REFUSED.get_or_try_init(|| Some(Box::new(why)));
            settings
        }
        None => {
            let settings = command_line(&mut say_refused);
            klog!(0, "net: no {}: eth0's settings from the command line: {}", PATH, settings);
            settings
        }
    };

    if DECIDED.get_or_try_init(|| Some(Box::new(settings))).copied() != Some(settings) {
        klog!(0, "net: eth0's settings were decided before the root was read: these are not taken");
    }
}

/* ---- the boot's self-test ---- */

fn check(what: &str, ok: bool) -> bool {
    if !ok {
        klog!(0, "net selftest: settings: {} FAILED", what);
    }
    ok
}

/// The command line's words and the file, each through what it is to
/// take and what it is to refuse: the rules the C++ had for `ip=` and
/// `gw=`, and the file's own.
pub fn selftest() -> bool {
    let mut ok = true;
    let line = |text: &str| {
        let mut refusals = 0usize;
        let s = from_command_line(text.as_bytes(), &mut |_, _, _| refusals += 1);
        (s, refusals)
    };

    /* A static address, its gateway and a resolver given -- a network with
     * no DHCP server, as the ASUS box's -- in host byte order, the gateway
     * given before the address it is checked against */
    let (fixed, n) = line("dhcp=auto gw=213.232.225.1 ip=213.232.225.136/24 dns=213.148.0.221 root=auto");
    ok &= check("a static address off the command line", n == 0 && fixed == Settings {
        address: Some(Address { ip: 0xD5E8_E188, mask: 0xFFFF_FF00, gw: 0xD5E8_E101 }),
        dns: 0xD594_00DD, lease_dns: true, dhcp: Dhcp::Boot, source: Source::CommandLine,
    });
    /* /31 and /32 have no network or broadcast address of their own */
    let (host_route, n) = line("ip=10.0.0.0/31");
    ok &= check("a /31", n == 0 && host_route.address == Some(Address { ip: 0x0A00_0000, mask: 0xFFFF_FFFE, gw: 0 }));
    let (plain, n) = line("dhcp=off quiet dns=on");
    ok &= check("dhcp=off, dns=on", n == 0 && plain.dhcp == Dhcp::Off && plain.lease_dns && plain.address.is_none());
    let (nothing, n) = line("ip= gw dhcp root=auto");
    ok &= check("none of them, and words that are not one", n == 0 && nothing == Settings::NONE);

    /* Refused, and nothing of them kept: no prefix, a prefix out of range,
     * the subnet's own network and broadcast addresses, addresses no host
     * has; a gateway off the subnet, or given with no address at all */
    for bad in ["ip=10.0.0.5", "ip=10.0.0.5/0", "ip=10.0.0.5/33", "ip=10.0.0.5/", "ip=10.0.0.0/24",
                "ip=10.0.0.255/24", "ip=127.0.0.1/8", "ip=224.0.0.1/4", "ip=0.0.0.0/8", "ip=10.0.0.5/24x",
                "ip=10.0.0.256/24", "dhcp=yes", "dns=255.255.255.255", "gw=10.0.0.1"] {
        let (s, n) = line(bad);
        ok &= check(bad, n == 1 && s == Settings::NONE);
    }
    for bad in ["ip=10.0.0.5/24 gw=10.0.1.1", "ip=10.0.0.5/24 gw=10.0.0.5", "ip=10.0.0.5/24 gw=10.0.0.255"] {
        let (s, n) = line(bad);
        ok &= check(bad, n == 1 && s.address == Some(Address { ip: 0x0A00_0005, mask: 0xFFFF_FF00, gw: 0 }));
    }

    /* The file: a static address, DHCP said or left unsaid, no address */
    let file = from_file("# a network with no DHCP\nip = \"203.0.113.5/24\"\ngw = \"203.0.113.1\"\n\
                          dns = \"203.0.113.53\"  # its resolver\n");
    ok &= check("a file's static address", file == Ok(Settings {
        address: Some(Address { ip: 0xCB00_7105, mask: 0xFFFF_FF00, gw: 0xCB00_7101 }),
        dns: 0xCB00_7135, lease_dns: true, dhcp: Dhcp::Command, source: Source::File,
    }));
    for dhcp in ["dhcp = true\n", "", "# nothing said\n"] {
        ok &= check(dhcp, from_file(dhcp).map(|s| (s.dhcp, s.address, s.lease_dns)) == Ok((Dhcp::Boot, None, true)));
    }
    ok &= check("dhcp = false", from_file("dhcp = false").map(|s| (s.dhcp, s.address)) == Ok((Dhcp::Command, None)));

    /* Refused whole, with the line of what is wrong */
    for (bad, at) in [
        ("ip = \"10.0.0.5\"", "line 1:"),
        ("ip = \"10.0.0.0/24\"", "line 1:"),
        ("\ngw = \"10.0.0.1\"", "line 2:"),
        ("ip = \"10.0.0.5/24\"\ngw = \"10.0.1.1\"", "line 2:"),
        ("ip = \"10.0.0.5/24\"\n\ndhcp = true", "line 3:"),
        ("dhcp = \"yes\"", "line 1:"),
        ("ip = 5", "line 1:"),
        ("netmask = \"255.0.0.0\"", "line 1:"),
        ("ip = \"10.0.0.5/24\"\nip = \"10.0.0.6/24\"", "line 2:"),
        ("dns = \"255.255.255.255\"", "line 1:"),
        ("[eth0]", "line 1:"),
    ] {
        ok &= check(bad, matches!(from_file(bad), Err(why) if why.starts_with(at)));
    }
    ok
}
