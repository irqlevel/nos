//! Every interface's settings from `/etc/net.toml` on the root filesystem:
//! what nos-install writes as it installs, and what is edited afterwards
//! (`edit /etc/net.toml`, then a reboot). The original keys are eth0's:
//!
//!     ip = "203.0.113.5/24"
//!     gw = "203.0.113.1"
//!     dns = "203.0.113.53"
//!
//! or `dhcp = true`, which is also what a file without `ip` means. The
//! per-interface form names the NIC in each key, the default route and any
//! additional routes:
//!
//!     dhcp_eth0 = true
//!     ip_eth1 = "94.126.205.237/31"
//!     gw_eth1 = "94.126.205.236"
//!     default = "eth1"
//!     dns = "77.88.8.8"
//!     route_eth0 = ["10.0.0.0/8 via 10.0.0.1"]
//!
//! The file is taken whole or not at all: a key it does not have, a value
//! that is not one, a gateway off the address's subnet, overlapping static
//! subnets, `dhcp = true` beside an `ip` -- each is refused, and the boot
//! says so.
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

/// The most the file may be, comments and every interface's routes among it.
pub const MAX_BYTES: usize = 4096;

/// The longest parameter the C++ takes off the command line
/// (`Parameters::MAX_PARAM_LEN`): a longer word it has named and skipped.
const PARAM_MAX: usize = 48;
/// The longest command line there is, its NUL among it
/// (`Parameters::CMDLINE_LEN`).
const CMDLINE_MAX: usize = 256;

const PREFIX_MAX: u32 = 32;

/// The original keys, on the command line and in the file.
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

pub const MAX_INTERFACES: usize = 16;
pub const MAX_ROUTES: usize = 32;
/// Also in the kernel image: nos-install checks it before allowing a
/// multi-interface rollback to an older kernel.
pub const FORMAT_MARKER: &str = "net.toml multi-NIC v2";

/// An interface beside eth0. `others[0]` is eth1: eth0's fields stay where
/// the command line's format and the original file put them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Interface {
    pub address: Option<Address>,
    pub dhcp: Dhcp,
}

impl Interface {
    const NONE: Interface = Interface { address: None, dhcp: Dhcp::Command };

    fn configured(&self) -> bool {
        self.address.is_some() || self.dhcp == Dhcp::Boot
    }
}

/// An additional route through `dev` (0 is eth0), longest prefix wins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Route {
    pub dev: usize,
    pub net: u32,
    pub mask: u32,
    pub gw: u32,
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
    pub others: [Interface; MAX_INTERFACES - 1],
    /// The interface of the default route, 0 for eth0.
    pub default: usize,
    pub routes: [Option<Route>; MAX_ROUTES],
    pub route_count: usize,
}

impl Settings {
    /// A command line with none of the four: DHCP only when asked, by the
    /// command, and no resolver unless one is.
    pub const NONE: Settings = Settings {
        address: None, dns: 0, lease_dns: false, dhcp: Dhcp::Command, source: Source::CommandLine,
        others: [Interface::NONE; MAX_INTERFACES - 1], default: 0,
        routes: [None; MAX_ROUTES], route_count: 0,
    };

    pub fn interface(&self, index: usize) -> Interface {
        if index == 0 {
            Interface { address: self.address, dhcp: self.dhcp }
        } else {
            self.others.get(index - 1).copied().unwrap_or(Interface::NONE)
        }
    }

    pub fn is_multi(&self) -> bool {
        self.default != 0 || self.route_count != 0 || self.others.iter().any(Interface::configured)
    }
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

impl fmt::Display for Interface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.address, self.dhcp) {
            (Some(a), _) => {
                write!(f, "{}/{}", Ip(a.ip), a.mask.count_ones())?;
                if a.gw != 0 {
                    write!(f, ", gateway {}", Ip(a.gw))?;
                }
                Ok(())
            }
            (None, Dhcp::Boot) => f.write_str("DHCP"),
            (None, Dhcp::Command) => f.write_str("no address until the dhcp command"),
            (None, Dhcp::Off) => f.write_str("no address, DHCP off"),
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

#[derive(Clone, Copy)]
struct NicReader {
    ip: Option<(u32, u32, usize)>,
    gw: Option<(u32, usize)>,
    dhcp: Option<(bool, usize)>,
}

impl NicReader {
    const NONE: NicReader = NicReader { ip: None, gw: None, dhcp: None };
}

struct Reader {
    nics: [NicReader; MAX_INTERFACES],
    dns: Option<u32>,
    default: Option<(usize, usize)>,
    routes: [Option<Route>; MAX_ROUTES],
    route_lines: [usize; MAX_ROUTES],
    route_count: usize,
}

impl Reader {
    fn new() -> Reader {
        Reader {
            nics: [NicReader::NONE; MAX_INTERFACES], dns: None, default: None,
            routes: [None; MAX_ROUTES], route_lines: [0; MAX_ROUTES], route_count: 0,
        }
    }
}

fn eth_number(text: &str) -> Option<usize> {
    let n = text.strip_prefix("eth")?.parse::<usize>().ok()?;
    (n < MAX_INTERFACES).then_some(n)
}

/// `ip_ethN`, `gw_ethN` or `dhcp_ethN`: the interface and the bare key.
fn nic_key(key: &str) -> Option<(usize, &'static str)> {
    for kind in ["ip", "gw", "dhcp"] {
        let prefix = alloc::format!("{}_eth", kind);
        if let Some(number) = key.strip_prefix(&prefix) {
            let n = number.parse::<usize>().ok().filter(|n| *n < MAX_INTERFACES)?;
            return Some((n, kind));
        }
    }
    None
}

fn route_key(key: &str) -> Option<usize> {
    key.strip_prefix("route_eth")?.parse::<usize>().ok().filter(|n| *n < MAX_INTERFACES)
}

/// A route's network, A.B.C.D/N with no host bits set.
fn network(text: &str) -> Result<(u32, u32), &'static str> {
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
    let mask = u32::MAX << (PREFIX_MAX - bits);
    if ip & !mask != 0 {
        return Err("has host bits set in its network address");
    }
    Ok((ip, mask))
}

fn route(text: &str, dev: usize) -> Result<Route, &'static str> {
    let mut words = text.split_whitespace();
    let (Some(net), Some(via), Some(gateway), None) = (words.next(), words.next(), words.next(), words.next()) else {
        return Err("is not A.B.C.D/N via A.B.C.D");
    };
    if via != "via" {
        return Err("is not A.B.C.D/N via A.B.C.D");
    }
    let (net, mask) = network(net)?;
    let gw = host(gateway)?;
    Ok(Route { dev, net, mask, gw })
}

impl conf::Keys for Reader {
    fn known(&mut self, line: usize, key: &str) -> Result<(), String> {
        if KEYS.contains(&key) || key == "default" || nic_key(key).is_some() || route_key(key).is_some() {
            Ok(())
        } else {
            Err(error(line, &alloc::format!("\"{}\" is not a key net.toml has", key)))
        }
    }

    fn assign(&mut self, line: usize, key: &str, value: Value) -> Result<(), String> {
        let (index, bare) = match key {
            "ip" | "gw" | "dhcp" => (0, key),
            _ => nic_key(key).map_or((0, key), |(n, bare)| (n, bare)),
        };
        if let Some(dev) = route_key(key) {
            let Value::List(lines) = value else {
                return Err(error(line, &alloc::format!("{} is a list of \"A.B.C.D/N via A.B.C.D\"", key)));
            };
            for text in lines {
                let parsed = route(&text, dev).map_err(|why| error(line, &alloc::format!("{} \"{}\" {}", key, text, why)))?;
                if self.route_count == MAX_ROUTES {
                    return Err(error(line, &alloc::format!("{} takes the routes past {}", key, MAX_ROUTES)));
                }
                if self.routes[..self.route_count].iter().flatten().any(|r| r.dev == dev && r.net == parsed.net && r.mask == parsed.mask) {
                    return Err(error(line, &alloc::format!("{} \"{}\" gives that network's route twice", key, text)));
                }
                self.routes[self.route_count] = Some(parsed);
                self.route_lines[self.route_count] = line;
                self.route_count += 1;
            }
            return Ok(());
        }
        if key == "default" {
            let Value::Str(text) = value else {
                return Err(error(line, "default is an interface name, \"eth0\""));
            };
            let n = eth_number(&text).ok_or_else(|| error(line, &alloc::format!("default \"{}\" is not an interface eth0 to eth{}", text, MAX_INTERFACES - 1)))?;
            self.default = Some((n, line));
            return Ok(());
        }
        let text = match value {
            Value::Bool(on) if bare == "dhcp" => {
                self.nics[index].dhcp = Some((on, line));
                return Ok(());
            }
            Value::Str(text) if bare != "dhcp" => text,
            other => {
                let want = match bare {
                    "dhcp" => "true or false",
                    "ip" => "a string, \"A.B.C.D/N\"",
                    _ => "a string, \"A.B.C.D\"",
                };
                return Err(error(line, &alloc::format!("{} is {}, not {}", key, want, other.kind())));
            }
        };
        let wrong = |why: &str| error(line, &alloc::format!("{} \"{}\" {}", key, text, why));
        match bare {
            "ip" => {
                let (ip, mask) = address(&text).map_err(wrong)?;
                self.nics[index].ip = Some((ip, mask, line));
            }
            "gw" => self.nics[index].gw = Some((host(&text).map_err(wrong)?, line)),
            _ => self.dns = Some(host(&text).map_err(wrong)?),
        }
        Ok(())
    }
}

/// The file's text read: the settings it gives, or what is wrong with it,
/// its line said.
pub fn from_file(text: &str) -> Result<Settings, String> {
    let mut r = Reader::new();
    conf::parse(text, "net.toml", &mut r)?;

    let mut s = Settings {
        dns: r.dns.unwrap_or(0), lease_dns: true, dhcp: Dhcp::Boot, source: Source::File, ..Settings::NONE
    };
    for index in 0..MAX_INTERFACES {
        let given = r.nics[index];
        let address = match (given.ip, given.gw) {
            (None, Some((_, line))) => {
                return Err(error(line, "gw without an ip: a gateway is the way off ip's subnet"));
            }
            (None, None) => None,
            (Some((ip, mask, _)), gw) => {
                if let Some((true, line)) = given.dhcp {
                    return Err(error(line, &alloc::format!("dhcp = true beside an ip: eth{}'s address is the one, or DHCP's", index)));
                }
                let mut a = Address { ip, mask, gw: 0 };
                if let Some((g, line)) = gw {
                    gateway_of(g, ip, mask).map_err(|why| error(line, &alloc::format!("gw \"{}\" {}", Ip(g), why)))?;
                    a.gw = g;
                }
                Some(a)
            }
        };
        let dhcp = match (address, given.dhcp, index) {
            (Some(_), _, _) | (None, Some((false, _)), _) => Dhcp::Command,
            (None, None, 0) | (None, Some((true, _)), _) => Dhcp::Boot,
            (None, None, _) => Dhcp::Command,
        };
        if index == 0 {
            s.address = address;
            s.dhcp = dhcp;
        } else {
            s.others[index - 1] = Interface { address, dhcp };
        }
    }

    if let Some((n, line)) = r.default {
        let i = s.interface(n);
        if i.address.is_none() && i.dhcp != Dhcp::Boot {
            return Err(error(line, &alloc::format!("default = \"eth{}\", which has no address or DHCP", n)));
        }
        if i.address.is_some_and(|a| a.gw == 0) {
            return Err(error(line, &alloc::format!("default = \"eth{}\", whose static address has no gateway", n)));
        }
        s.default = n;
    }
    for (index, a) in (0..MAX_INTERFACES).filter_map(|n| s.interface(n).address.map(|a| (n, a))) {
        for (other, b) in (index + 1..MAX_INTERFACES).filter_map(|n| s.interface(n).address.map(|a| (n, a))) {
            if a.ip & b.mask == b.ip & b.mask || b.ip & a.mask == a.ip & a.mask {
                let line = r.nics[other].ip.map_or(1, |(_, _, line)| line);
                return Err(error(line, &alloc::format!("eth{} and eth{} have overlapping subnets", index, other)));
            }
        }
    }
    for (at, route) in r.routes[..r.route_count].iter().enumerate().filter_map(|(at, r)| r.map(|r| (at, r))) {
        let line = r.route_lines[at];
        let i = s.interface(route.dev);
        if i.address.is_none() && i.dhcp != Dhcp::Boot {
            return Err(error(line, &alloc::format!("a route through eth{}, which has no address or DHCP", route.dev)));
        }
        if let Some(a) = i.address {
            gateway_of(route.gw, a.ip, a.mask)
                .map_err(|why| error(line, &alloc::format!("route gateway {} on eth{} {}", Ip(route.gw), route.dev, why)))?;
        }
    }
    s.routes = r.routes;
    s.route_count = r.route_count;
    Ok(s)
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

/// Every interface's settings: what the boot decided -- or, before it has,
/// and where nothing boots (a fuzzer), the command line's.
pub fn current() -> Settings {
    match DECIDED.get() {
        Some(settings) => *settings,
        None => command_line(&mut |_, _, _| {}),
    }
}

pub fn default_index() -> usize {
    DECIDED.get().map_or(0, |s| s.default)
}

pub fn is_multi() -> bool {
    DECIDED.get().is_some_and(|s| s.is_multi())
}

/// The most specific additional route to `dst`.
pub fn route_for(dst: u32) -> Option<Route> {
    let s = DECIDED.get()?;
    s.routes[..s.route_count].iter().flatten()
        .filter(|r| r.mask != 0 && dst & r.mask == r.net)
        .max_by_key(|r| r.mask.count_ones()).copied()
}

/// The most specific additional route to `dst` through one interface. A
/// caller pinned to another interface never takes it.
pub fn route_for_dev(index: usize, dst: u32) -> Option<Route> {
    let s = DECIDED.get()?;
    s.routes[..s.route_count].iter().flatten()
        .filter(|r| r.dev == index && r.mask != 0 && dst & r.mask == r.net)
        .max_by_key(|r| r.mask.count_ones()).copied()
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
/// netconsole or the shell over UDP start: every interface's settings, from
/// the root's `PATH` or else the command line, decided for the rest of the
/// boot and said. Called once, by each architecture's boot.
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
            if settings.is_multi() {
                klog!(0, "net: {}: {}", PATH, FORMAT_MARKER);
            }
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
        ..Settings::NONE
    });
    /* /31 and /32 have no network or broadcast address of their own */
    let (host_route, n) = line("ip=10.0.0.0/31");
    ok &= check("a /31", n == 0 && host_route.address == Some(Address { ip: 0x0A00_0000, mask: 0xFFFF_FFFE, gw: 0 }));
    let (plain, n) = line("dhcp=off quiet dns=on");
    ok &= check("dhcp=off, dns=on", n == 0 && plain.dhcp == Dhcp::Off && plain.lease_dns && plain.address.is_none());
    let (nothing, n) = line("ip= gw dhcp root=auto");
    ok &= check("none of them, and words that are not one", n == 0 && nothing == Settings::NONE);
    ok &= check("an unmentioned NIC still allows the dhcp command",
        nothing.interface(1).dhcp == Dhcp::Command && !nothing.is_multi());

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
        ..Settings::NONE
    }));
    for dhcp in ["dhcp = true\n", "", "# nothing said\n"] {
        ok &= check(dhcp, from_file(dhcp).map(|s| (s.dhcp, s.address, s.lease_dns)) == Ok((Dhcp::Boot, None, true)));
    }
    ok &= check("dhcp = false", from_file("dhcp = false").map(|s| (s.dhcp, s.address)) == Ok((Dhcp::Command, None)));

    let mut multi = Settings {
        address: None, dns: 0x4D58_0808, lease_dns: true, dhcp: Dhcp::Boot, source: Source::File,
        ..Settings::NONE
    };
    multi.others[0] = Interface {
        address: Some(Address { ip: 0x5E7E_CDED, mask: 0xFFFF_FFFE, gw: 0x5E7E_CDEC }), dhcp: Dhcp::Command,
    };
    multi.default = 1;
    multi.routes[0] = Some(Route { dev: 0, net: 0x0A00_0000, mask: 0xFF00_0000, gw: 0x0A00_0001 });
    multi.route_count = 1;
    ok &= check("DHCP inside, a static /31 outside and a private route",
        from_file("dhcp_eth0 = true\nip_eth1 = \"94.126.205.237/31\"\ngw_eth1 = \"94.126.205.236\"\n\
                   default = \"eth1\"\ndns = \"77.88.8.8\"\nroute_eth0 = [\"10.0.0.0/8 via 10.0.0.1\"]\n") == Ok(multi));

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
        ("default = \"eth1\"", "line 1:"),
        ("ip_eth1 = \"203.0.113.5/24\"\ndefault = \"eth1\"", "line 2:"),
    ] {
        ok &= check(bad, matches!(from_file(bad), Err(why) if why.starts_with(at)));
    }
    for bad in [
        "ip = \"10.0.0.2/24\"\nip_eth1 = \"10.0.0.3/24\"",
        "ip = \"10.0.0.2/24\"\nroute_eth0 = [\"192.168.0.0/16 via 10.0.1.1\"]",
        "dhcp_eth0 = true\nroute_eth0 = [\"10.0.0.0/8\", \"10.0.0.0/8 via 10.0.0.1 via 10.0.0.2\"]",
    ] {
        ok &= check(bad, from_file(bad).is_err());
    }
    ok
}
