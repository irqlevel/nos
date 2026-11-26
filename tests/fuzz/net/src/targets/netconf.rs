//! `netconf`: every interface's settings (`net::settings`) -- `/etc/net.toml`,
//! read off the root at boot, and the kernel command line's `ip=`, `gw=`,
//! `dns=` and `dhcp=` for eth0 in its place where there is no such file or
//! it is refused. Both
//! are somebody else's to write: an installer that got a field wrong, a
//! hand that edited the file, a line typed at GRUB's prompt -- and a wrong
//! one is a server nobody can reach.
//!
//! The input makes a file of lines a person might write, well and badly --
//! keys, values of every kind, comments, a table, a key twice -- or bytes
//! of any kind, and a command line of words. What the module makes of each
//! is held to a model of its rules written from its comments and from RFC
//! 1122's of an address, with nothing of the module's in it: an address is
//! made from its parts here and judged by them, never parsed back. What the
//! model takes is taken, exactly; what it refuses is refused, at the line
//! it names. Then the boot's own step is run on a ramfs root holding the
//! file -- or a directory in its place, one too big, bytes that are not
//! text, nothing -- and what it decided must be the file's where the model
//! takes the file, and the command line's otherwise, the refusal kept for
//! `net` to say where there was one.

use std::fmt::Write as _;

use net::settings::{self, Address, Dhcp, Settings, Source};

use crate::machine;
use crate::world::{App, Link, Net, Peers, World};
use crate::Input;

const SEC: u64 = 1_000_000_000;

/* ---- addresses, made from their parts and judged by them ---- */

/// Whether a host may have `a` (RFC 1122 3.2.1.3): not "this network",
/// not loopback, not multicast or reserved.
fn may_have(a: u32) -> bool {
    let first = a >> 24;
    first != 0 && first != 127 && first < 224
}

/// The mask of a prefix of `bits`, 1 to 32.
fn mask_of(bits: u32) -> u32 {
    if bits == 32 { u32::MAX } else { !(u32::MAX >> bits) }
}

/// Whether `a` is its subnet's network or broadcast address -- which a
/// subnet of /31 or /32 has neither of.
fn network_or_broadcast(a: u32, bits: u32) -> bool {
    bits <= 30 && (a & !mask_of(bits) == 0 || a & !mask_of(bits) == !mask_of(bits))
}

/// An address as it is written, with what it is: its value, and whether it
/// is four numbers of at most 255 at all.
#[derive(Clone)]
struct Quad {
    text: String,
    /// Its value, where it is one.
    value: Option<u32>,
}

const OCTETS: [u32; 14] = [0, 1, 2, 5, 10, 113, 127, 192, 203, 223, 224, 240, 255, 256];

fn octet(r: &mut Input) -> u32 {
    if r.bool() { r.pick(&OCTETS) } else { r.u8() as u32 }
}

/// Four numbers, now and then written with a leading zero, or something
/// that is not four numbers at all.
fn quad(r: &mut Input, near: Option<u32>) -> Quad {
    if r.chance(12) {
        let text = r.pick(&["10.0.0", "10.0.0.1.2", "a.b.c.d", "10..0.1", "", " 10.0.0.1", "10.0.0.1 ", "-1.0.0.1",
                            "1e1.0.0.1", "10.0.0.1:53"]);
        return Quad { text: text.to_string(), value: None };
    }
    let mut parts = [octet(r), octet(r), octet(r), octet(r)];
    if let Some(base) = near {
        /* An address of the same network, mostly: a gateway, a resolver --
         * now and then the address itself */
        if r.chance(24) {
            parts[3] = base & 0xFF;
        }
        if r.chance(160) {
            parts = [base >> 24, (base >> 16) & 0xFF, (base >> 8) & 0xFF, parts[3]];
        }
    }
    let mut text = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            text.push('.');
        }
        if r.chance(10) {
            text.push('0');
        }
        let _ = write!(text, "{}", p);
    }
    let value = parts.iter().all(|p| *p <= 255).then(|| parts.iter().fold(0u32, |a, p| (a << 8) | p));
    Quad { text, value }
}

/// An address and its prefix length as written, and what it is: its value
/// and mask where it is one eth0 may have.
struct Cidr {
    text: String,
    taken: Option<(u32, u32)>,
}

fn cidr(r: &mut Input) -> Cidr {
    let q = quad(r, None);
    let bits = if r.bool() { r.pick(&[0u32, 1, 8, 16, 24, 26, 29, 30, 31, 32, 33]) } else { r.below(40) as u32 };
    /* The prefix length is one or two digits: a third, even a leading
     * zero's, is not one */
    let (suffix, bits_ok) = match r.below(16) {
        0 => (String::new(), None),
        1 => ("/".to_string(), None),
        2 => (format!("/{}x", bits), None),
        3 => (format!("/0{}", bits % 10), Some(bits % 10)),
        4 => (format!("/0{}", bits), if bits < 10 { Some(bits) } else { None }),
        5 => (format!("/+{}", bits), None),
        _ => (format!("/{}", bits), Some(bits)),
    };
    let taken = match (q.value, bits_ok) {
        (Some(a), Some(b)) if (1..=32).contains(&b) && may_have(a) && !network_or_broadcast(a, b) => {
            Some((a, mask_of(b)))
        }
        _ => None,
    };
    Cidr { text: format!("{}{}", q.text, suffix), taken }
}

/// A gateway's or a resolver's address: what it is, where it is one a host
/// may have.
fn host(r: &mut Input, near: Option<u32>) -> (String, Option<u32>) {
    let q = quad(r, near);
    let taken = q.value.filter(|a| may_have(*a));
    (q.text, taken)
}

/// Whether `gw` is a gateway for `ip` on its subnet of `mask`.
fn gateway_for(gw: u32, ip: u32, mask: u32) -> bool {
    let bits = mask.count_ones();
    gw != ip && (gw ^ ip) & mask == 0 && !network_or_broadcast(gw, bits)
}

/* ---- the file ---- */

/// A line of the file, and what the model makes of it.
enum Line {
    /// A comment or nothing: skipped.
    Blank,
    /// A key with a value: what the value is to the key, where it is one.
    Entry { key: &'static str, value: Value },
    /// Something the file's syntax refuses: a table, a key with no value,
    /// a string not closed, more after the value.
    Junk,
}

#[derive(Clone)]
enum Value {
    /// An address, a gateway's or a resolver's, as `taken` -- None where
    /// none: refused.
    Ip(Option<(u32, u32)>),
    Host(Option<u32>),
    Bool(bool),
    /// A value of the wrong kind for its key.
    Wrong,
}

fn file(r: &mut Input) -> (String, Vec<Line>) {
    let mut text = String::new();
    let mut lines = Vec::new();
    let mut ip_near = None;
    let n = r.below(8);
    for _ in 0..n {
        let indent = if r.chance(20) { "  " } else { "" };
        let tail = match r.below(8) {
            0 => "  # a comment",
            1 => " ",
            _ => "",
        };
        let (written, line) = match r.below(12) {
            0 => ("# eth0's settings".to_string(), Line::Blank),
            1 => (String::new(), Line::Blank),
            2 | 3 => {
                let c = cidr(r);
                if let Some((ip, _)) = c.taken {
                    ip_near = Some(ip);
                }
                (format!("ip = \"{}\"", c.text), Line::Entry { key: "ip", value: Value::Ip(c.taken) })
            }
            4 | 5 => {
                let (t, v) = host(r, ip_near);
                (format!("gw = \"{}\"", t), Line::Entry { key: "gw", value: Value::Host(v) })
            }
            6 => {
                let (t, v) = host(r, ip_near);
                (format!("dns = '{}'", t), Line::Entry { key: "dns", value: Value::Host(v) })
            }
            7 | 8 => {
                let on = r.bool();
                (format!("dhcp = {}", on), Line::Entry { key: "dhcp", value: Value::Bool(on) })
            }
            9 => {
                /* A value of the wrong kind for its key */
                let key = r.pick(&["ip", "gw", "dns", "dhcp"]);
                let written = match r.below(3) {
                    0 => "5",
                    1 => "[\"10.0.0.1/24\"]",
                    _ if key == "dhcp" => "\"yes\"",
                    _ => "true",
                };
                (format!("{} = {}", key, written), Line::Entry { key, value: Value::Wrong })
            }
            10 => {
                let key = r.pick(&["netmask", "gateway", "address", "eth0_ip", "IP"]);
                (format!("{} = \"10.0.0.1\"", key), Line::Entry { key: "?", value: Value::Wrong })
            }
            _ => {
                let junk = r.pick(&["[eth0]", "ip", "ip = ", "ip = \"10.0.0.5/24", "= 5", "gw = \"a\" \"b\"",
                                    "dhcp = true false", "dns.server = \"1.1.1.1\""]);
                (junk.to_string(), Line::Junk)
            }
        };
        if !written.is_empty() {
            text.push_str(indent);
            text.push_str(&written);
            if !matches!(line, Line::Junk) || tail.is_empty() {
                text.push_str(tail);
            }
        }
        text.push_str(if r.chance(16) { "\r\n" } else { "\n" });
        lines.push(line);
    }
    (text, lines)
}

/// What the model makes of the file: its settings, or the line it is
/// refused at.
fn judge_file(lines: &[Line]) -> Result<Settings, usize> {
    let mut ip: Option<(u32, u32)> = None;
    let mut gw: Option<(u32, usize)> = None;
    let mut dns = None;
    let mut dhcp: Option<(bool, usize)> = None;
    let mut seen: Vec<&str> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let at = i + 1;
        let (key, value) = match line {
            Line::Blank => continue,
            Line::Junk => return Err(at),
            Line::Entry { key, value } => (*key, value),
        };
        if key == "?" || seen.contains(&key) {
            return Err(at);
        }
        seen.push(key);
        match (key, value) {
            ("ip", Value::Ip(Some(a))) => ip = Some(*a),
            ("gw", Value::Host(Some(g))) => gw = Some((*g, at)),
            ("dns", Value::Host(Some(d))) => dns = Some(*d),
            ("dhcp", Value::Bool(on)) => dhcp = Some((*on, at)),
            _ => return Err(at),
        }
    }
    let address = match (ip, gw) {
        (None, Some((_, at))) => return Err(at),
        (None, None) => None,
        (Some((ip, mask)), gw) => {
            if let Some((true, at)) = dhcp {
                return Err(at);
            }
            let mut a = Address { ip, mask, gw: 0 };
            if let Some((g, at)) = gw {
                if !gateway_for(g, ip, mask) {
                    return Err(at);
                }
                a.gw = g;
            }
            Some(a)
        }
    };
    let by_dhcp = address.is_none() && dhcp.map_or(true, |(on, _)| on);
    Ok(Settings {
        address,
        dns: dns.unwrap_or(0),
        lease_dns: true,
        dhcp: if by_dhcp { Dhcp::Boot } else { Dhcp::Command },
        source: Source::File,
        ..Settings::NONE
    })
}

/* ---- the command line ---- */

/// A command line's words, and what the model makes of them: the settings
/// and how many of its words of the four are named as not taken.
fn command_line(r: &mut Input) -> (String, Settings, usize) {
    let mut words = Vec::new();
    let mut ip: Option<(u32, u32)> = None;
    let mut gw: Option<u32> = None;
    let mut s = Settings { address: None, dns: 0, lease_dns: false, dhcp: Dhcp::Command, source: Source::CommandLine,
        ..Settings::NONE };
    let mut refused = 0;
    /* At most four words, so that the line is never cut short: a cut is the
     * C++'s to make, at a whole word, which the machine's line here is not */
    let n = r.below(5);
    for _ in 0..n {
        match r.below(10) {
            0 | 1 => {
                let c = cidr(r);
                /* A space would make two words of it */
                if c.text.contains(' ') {
                    continue;
                }
                match c.taken {
                    Some(a) => ip = Some(a),
                    /* `ip=` with nothing after it is no parameter at all */
                    None if c.text.is_empty() => {}
                    None => refused += 1,
                }
                words.push(format!("ip={}", c.text));
            }
            2 => {
                let (t, v) = host(r, ip.map(|(a, _)| a));
                if t.contains(' ') {
                    continue;
                }
                match v {
                    Some(g) => gw = Some(g),
                    None if t.is_empty() => {}
                    None => refused += 1,
                }
                words.push(format!("gw={}", t));
            }
            3 => {
                let (t, v) = host(r, None);
                if t.contains(' ') {
                    continue;
                }
                match v {
                    Some(d) => {
                        s.dns = d;
                        s.lease_dns = true;
                    }
                    None if t.is_empty() => {}
                    None => refused += 1,
                }
                words.push(format!("dns={}", t));
            }
            4 => {
                s.lease_dns = true;
                words.push("dns=on".to_string());
            }
            5 => {
                let (word, mode) = r.pick(&[("auto", Some(Dhcp::Boot)), ("on", Some(Dhcp::Command)),
                                            ("off", Some(Dhcp::Off)), ("yes", None), ("Auto", None)]);
                match mode {
                    Some(m) => s.dhcp = m,
                    None => refused += 1,
                }
                words.push(format!("dhcp={}", word));
            }
            6 => {
                /* Past the longest parameter the C++ takes: skipped, though
                 * its address is a good one */
                words.push(format!("ip={}10.0.0.5/24", "0".repeat(40)));
            }
            _ => words.push(r.pick(&["root=auto", "quiet", "fstest=on", "ip=", "gw", "=10.0.0.1", "dhcp",
                                     "udpshell=9000"]).to_string()),
        }
    }
    /* The last good address, and its gateway -- the last good one -- held to
     * it once every word is read */
    s.address = ip.map(|(ip, mask)| Address { ip, mask, gw: 0 });
    if let Some(g) = gw {
        match &mut s.address {
            Some(a) if gateway_for(g, a.ip, a.mask) => a.gw = g,
            _ => refused += 1,
        }
    }
    let sep = if r.chance(20) { "  " } else { " " };
    (words.join(sep), s, refused)
}

/* ---- what anything taken must be ---- */

/// What the module may give, whatever it was handed.
fn sane(s: &Settings, what: &str) {
    if let Some(a) = s.address {
        let bits = a.mask.count_ones();
        invariant!((1..=32).contains(&bits) && a.mask == mask_of(bits), "{}: a mask of {:#x}", what, a.mask);
        invariant!(may_have(a.ip) && !network_or_broadcast(a.ip, bits), "{}: eth0 given {:#x}/{}", what, a.ip, bits);
        invariant!(a.gw == 0 || (may_have(a.gw) && gateway_for(a.gw, a.ip, a.mask)),
                   "{}: a gateway {:#x} for {:#x}/{}", what, a.gw, a.ip, bits);
    }
    invariant!(s.dns == 0 || may_have(s.dns), "{}: a resolver {:#x}", what, s.dns);
    if s.source == Source::File {
        invariant!(s.lease_dns && s.dhcp != Dhcp::Off && !(s.dhcp == Dhcp::Boot && s.address.is_some()),
                   "{}: a file's settings {:?}", what, s);
    }
}

/* ---- two links, as the dual-homed server has them ---- */

fn two_links() {
    let text = "dhcp_eth0 = true\n\
                ip_eth1 = \"94.126.205.237/31\"\n\
                gw_eth1 = \"94.126.205.236\"\n\
                dns = \"77.88.8.8\"\n\
                default = \"eth1\"\n\
                route_eth0 = [\"10.0.0.0/8 via 10.0.0.1\", \"172.16.0.0/12 via 10.0.0.1\", \
                              \"192.168.0.0/16 via 10.0.0.1\"]\n";
    let got = settings::from_file(text).expect("two links");
    invariant!(got.dhcp == Dhcp::Boot && got.address.is_none() && got.default == 1 && got.dns == 0x4D58_0808,
               "the inside's DHCP and the default outside: {:?}", got);
    let outside = got.interface(1);
    invariant!(outside.dhcp == Dhcp::Command
        && outside.address == Some(Address { ip: 0x5E7E_CDED, mask: 0xFFFF_FFFE, gw: 0x5E7E_CDEC }),
        "the static /31 outside: {:?}", outside);
    let nets: Vec<(u32, u32)> = got.routes[..got.route_count].iter().flatten().map(|r| (r.net, r.mask)).collect();
    invariant!(nets == [(0x0A00_0000, 0xFF00_0000), (0xAC10_0000, 0xFFF0_0000), (0xC0A8_0000, 0xFFFF_0000)]
        && got.routes[..got.route_count].iter().flatten().all(|r| r.dev == 0 && r.gw == 0x0A00_0001),
        "the private routes: {:?}", got.routes);
    for bad in [
        "default = \"eth1\"",
        "ip = \"10.0.0.2/24\"\nip_eth1 = \"10.0.0.3/24\"",
        "ip = \"10.0.0.2/24\"\nroute_eth0 = [\"192.168.0.0/16 via 10.0.1.1\"]",
    ] {
        invariant!(settings::from_file(bad).is_err(), "refused: {}", bad);
    }
}

/* ---- the boot's own step ---- */

/// Nobody on the wire: this target sends nothing.
struct Nobody;

impl Peers for Nobody {
    fn on_frame(&mut self, _net: &mut Net, _frame: &[u8]) {}
}

/// What the root holds at the file's path, at boot.
enum Root {
    /// No root mounted at all.
    None,
    Absent,
    File(Vec<u8>),
    Directory,
}

fn boot(root: Root, words: &'static str) -> (Settings, Option<String>) {
    machine::set_params(machine::Params { words, ..machine::params() });
    let app = App::spawn("netconf", 0, move || {
        let mut ready = true;
        if !matches!(root, Root::None) {
            ready &= fs::ramfs::mount_at("/", false);
        }
        match &root {
            Root::File(bytes) => ready &= kcore::fs::create_dir("/etc").is_ok()
                && kcore::fs::create(settings::PATH, bytes).is_ok(),
            Root::Directory => ready &= kcore::fs::create_dir("/etc").is_ok()
                && kcore::fs::create_dir(settings::PATH).is_ok(),
            Root::None | Root::Absent => {}
        }
        invariant!(ready, "the root could not be made");
        settings::rust_net_settings_from_root();
        (settings::current(), settings::refused().map(String::from))
    });
    let mut w = World::new(Link::perfect(), Nobody);
    invariant!(w.wait_app(&app, 10 * SEC), "the boot's step never finished");
    app.take().expect("done")
}

pub fn netconf(r: &mut Input) {
    two_links();
    let (line, line_settings, line_refused) = command_line(r);
    let mut named = 0;
    let got = settings::from_command_line(line.as_bytes(), &mut |_, _, _| named += 1);
    invariant!(got == line_settings && named == line_refused,
               "the command line {:?}: {:?} with {} named, the model {:?} with {}", line, got, named,
               line_settings, line_refused);
    sane(&got, "the command line");

    /* The file: lines a person might write, or bytes of any kind */
    let bytes = if r.chance(200) {
        let (text, lines) = file(r);
        match (settings::from_file(&text), judge_file(&lines)) {
            (Ok(s), Ok(want)) => invariant!(s == want, "{:?}: {:?}, the model {:?}", text, s, want),
            (Err(why), Err(at)) => invariant!(why.starts_with(&format!("line {}:", at)),
                                              "{:?}: refused \"{}\", the model at line {}", text, why, at),
            (got, want) => panic!("invariant: {:?}: {:?}, the model {:?}", text, got, want),
        }
        text.into_bytes()
    } else {
        let n = r.below(160) as usize;
        let mut bytes = r.bytes(n);
        if r.chance(128) {
            bytes.retain(|b| b.is_ascii_graphic() || *b == b' ' || *b == b'\n');
        }
        if let Ok(text) = std::str::from_utf8(&bytes) {
            if let Ok(s) = settings::from_file(text) {
                sane(&s, "a file of bytes");
            }
        }
        bytes
    };

    /* The boot's step: the root as the input has it, the command line too */
    let root = match r.below(16) {
        0 => Root::None,
        1 => Root::Absent,
        2 => Root::Directory,
        3 => Root::File(vec![b'#'; settings::MAX_BYTES + 1 + r.below(64) as usize]),
        _ => Root::File(bytes.clone()),
    };
    /* What the boot is to decide: the file's settings -- judged above --
     * where it is there to be taken, the command line's otherwise */
    let file_taken = match &root {
        Root::File(b) if b.len() <= settings::MAX_BYTES => match std::str::from_utf8(b) {
            Ok(text) => Some(settings::from_file(text).ok()),
            Err(_) => Some(None),
        },
        Root::File(_) | Root::Directory => Some(None),
        Root::None | Root::Absent => None,
    };
    let words: &'static str = Box::leak(line.clone().into_boxed_str());
    let (decided, refused) = boot(root, words);
    match file_taken {
        Some(Some(s)) => invariant!(decided == s && refused.is_none(), "a file taken: {:?}, refused {:?}, the file's {:?}",
                                    decided, refused, s),
        Some(None) => invariant!(decided == line_settings && refused.is_some(),
                                 "a file refused: {:?}, refused {:?}, the command line's {:?}", decided, refused,
                                 line_settings),
        None => invariant!(decided == line_settings && refused.is_none(),
                           "no file: {:?}, refused {:?}, the command line's {:?}", decided, refused, line_settings),
    }
}
