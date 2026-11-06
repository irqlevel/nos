//! What ipcnet keeps in its folder -- /etc/ipc unless `insmod ipcnet.ko
//! conf=` names another:
//!
//! - `hosts.toml`, this host's name in its cluster, the cluster's id, where
//!   it listens, and the other hosts -- each one's name, address and key,
//!   pinned (plans/ipc.md, "Who may send to whom"):
//!
//!       name = "ex44"
//!       cluster = "0f1e2d3c4b5a69788796a5b4c3d2e1f0"
//!       peers = ["ax41 65.109.93.213 9c1e...64 hex", "box3 10.0.0.3 ab... cd..."]
//!       nic = "eth0"
//!       port = 39240
//!
//!   A peer's second key is the one it moves to while it changes its own.
//!   Every key but `name` and `cluster` is optional; one the file does not
//!   have, or one given twice, is refused with its line, and so is a peer
//!   named twice or given an address another has.
//! - `tls/key.pem` and `tls/cert.pem`, the host's key and its certificate,
//!   made at the first start and never written over: every peer pins the
//!   key, and a new one would leave the host out of its cluster.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use conf::{error, Value};
use core::fmt::Write;
use tls::pinned::Fingerprint;
use zeroize::Zeroize;

pub const DEFAULT_DIR: &str = "/etc/ipc";
/// ipcnet's port, beside the host API's 39234.
pub const DEFAULT_PORT: u16 = 39240;
/// The interface it listens on and dials from unless told: never hv0, so
/// that no guest reaches it.
pub const DEFAULT_NIC: &str = "eth0";
/// The most hosts.toml may be, and a key or a certificate.
const HOSTS_MAX_BYTES: usize = 16 * 1024;
const PEM_MAX: usize = 16 * 1024;
/// Peers at most: each is two connections, two tasks and their buffers.
pub const PEERS_MAX: usize = 16;
/// Keys a peer may be pinned by: its own, and the one it moves to.
const KEYS_MAX: usize = 2;
/// How long the host's certificate is good for. Nobody looks at its dates
/// -- a peer pins the key -- but a certificate has to have some.
const CERT_DAYS: u64 = 3650;
const SECS_PER_DAY: u64 = 86_400;
/// An interface's name, as `net` names them.
const NIC_NAME_MAX: usize = 15;

/// The folder's files, by path.
pub struct Dir {
    pub root: String,
}

impl Dir {
    pub fn new(root: &str) -> Dir {
        Dir { root: root.trim_end_matches('/').to_string() }
    }

    pub fn path(&self, name: &str) -> String {
        alloc::format!("{}/{}", self.root, name)
    }
}

/// A key's fingerprint as hosts.toml and `ipcnet id` write it: 64 hex
/// digits.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// `text` as `N` bytes written in hex.
fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let b = text.as_bytes();
    if b.len() != 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    for (i, pair) in b.chunks(2).enumerate() {
        let digit = |c: u8| (c as char).to_digit(16);
        out[i] = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(out)
}

pub fn parse_key(text: &str) -> Option<Fingerprint> {
    unhex::<32>(text)
}

/// A host's name, as the channel and the wire take one (`ipcwire::valid_host`).
pub fn valid_host(name: &str) -> bool {
    ipcwire::valid_host(name)
}

fn nic_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= NIC_NAME_MAX && name.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// An IPv4 address in dotted form, host order.
pub fn parse_ip(text: &str) -> Option<u32> {
    let mut parts = text.split('.');
    let mut ip = 0u32;
    for _ in 0..4 {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u32 = p.parse().ok()?;
        if v > 255 {
            return None;
        }
        ip = (ip << 8) | v;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(ip)
}

/// An address as `a.b.c.d`.
pub struct Ip(pub u32);

impl core::fmt::Display for Ip {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = self.0.to_be_bytes();
        write!(f, "{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    }
}

/// A host of the cluster, as hosts.toml has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerConf {
    pub name: String,
    pub ip: u32,
    pub keys: Vec<Fingerprint>,
    /// The one name of this host's it may reach, by that name alone: a host
    /// invited to the cluster, which reaches the cluster's member and
    /// nothing else until it has joined (`only=<name>` in hosts.toml).
    pub only: Option<String>,
}

impl PeerConf {
    /// The entry hosts.toml writes for it.
    pub fn line(&self) -> String {
        let mut s = alloc::format!("{} {}", self.name, Ip(self.ip));
        for k in &self.keys {
            s.push(' ');
            s.push_str(&hex(k));
        }
        if let Some(n) = &self.only {
            s.push_str(" only=");
            s.push_str(n);
        }
        s
    }

    /// Whether `other` is the same peer at the same address: what its
    /// connections are made of. Its keys and what it may reach are not, and
    /// change with no connection lost -- a connection keeps the key it was
    /// made with, and the next is made with a key the peer has then.
    pub fn same_link(&self, other: &PeerConf) -> bool {
        self.name == other.name && self.ip == other.ip
    }
}

/// hosts.toml, read.
#[derive(Clone, Debug)]
pub struct Config {
    pub name: String,
    pub cluster: [u8; 16],
    pub nic: String,
    pub port: u16,
    pub peers: Vec<PeerConf>,
}

/// A peer's entry: `name ip key [key] [only=<mailbox>]`.
fn parse_peer(line: usize, text: &str) -> Result<PeerConf, String> {
    let mut words = text.split_whitespace();
    let (Some(name), Some(ip)) = (words.next(), words.next()) else {
        return Err(error(line, &alloc::format!("\"{}\": a peer is \"<name> <address> <key> [<key>]\"", text)));
    };
    if !valid_host(name) {
        return Err(error(line, &alloc::format!("\"{}\" is not a host's name: lowercase letters, digits and -, up \
                                                to {}", name, ipcwire::HOST_MAX)));
    }
    let ip = parse_ip(ip).ok_or_else(|| error(line, &alloc::format!("\"{}\" is not an IPv4 address", ip)))?;
    let mut keys = Vec::new();
    let mut only = None;
    for w in words {
        if let Some(n) = w.strip_prefix("only=") {
            if only.is_some() || !ipcwire::valid_name(n) {
                return Err(error(line, &alloc::format!("\"{}\": `only=` names one mailbox, once", w)));
            }
            only = Some(n.to_string());
            continue;
        }
        let k = parse_key(w).ok_or_else(|| error(line, &alloc::format!("\"{}\" is not a key: 64 hex digits", w)))?;
        if keys.len() == KEYS_MAX || keys.contains(&k) {
            return Err(error(line, &alloc::format!("{} has more than {} keys, or one twice", name, KEYS_MAX)));
        }
        keys.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        keys.push(k);
    }
    if keys.is_empty() {
        return Err(error(line, &alloc::format!("{} has no key", name)));
    }
    Ok(PeerConf { name: name.to_string(), ip, keys, only })
}

struct Reader {
    name: Option<String>,
    cluster: Option<[u8; 16]>,
    nic: String,
    port: u16,
    peers: Vec<PeerConf>,
}

const KEYS: [&str; 5] = ["name", "cluster", "nic", "port", "peers"];

impl conf::Keys for Reader {
    fn known(&mut self, line: usize, key: &str) -> Result<(), String> {
        if KEYS.contains(&key) {
            Ok(())
        } else {
            Err(error(line, &alloc::format!("\"{}\" is not a key hosts.toml has", key)))
        }
    }

    fn assign(&mut self, line: usize, key: &str, value: Value) -> Result<(), String> {
        match (key, value) {
            ("name", Value::Str(n)) if valid_host(&n) => self.name = Some(n),
            ("name", _) => {
                return Err(error(line, "name is this host's: a string of lowercase letters, digits and -"));
            }
            ("cluster", Value::Str(c)) => {
                self.cluster = Some(unhex::<16>(&c).ok_or_else(|| error(line, "cluster is 32 hex digits"))?);
            }
            ("cluster", _) => return Err(error(line, "cluster is a string of 32 hex digits")),
            ("nic", Value::Str(n)) if nic_name(&n) => self.nic = n,
            ("nic", _) => return Err(error(line, "nic is an interface's name: \"eth0\"")),
            ("port", Value::Int(p)) if (1..=u64::from(u16::MAX)).contains(&p) => self.port = p as u16,
            ("port", _) => return Err(error(line, "port is a number from 1 to 65535")),
            ("peers", Value::List(entries)) => {
                if entries.len() > PEERS_MAX {
                    return Err(error(line, &alloc::format!("more than {} peers", PEERS_MAX)));
                }
                for e in &entries {
                    let p = parse_peer(line, e)?;
                    if self.peers.iter().any(|q| q.name == p.name || q.ip == p.ip) {
                        return Err(error(line, &alloc::format!("{}: a name or an address given twice", p.name)));
                    }
                    self.peers.try_reserve(1).map_err(|_| String::from("out of memory"))?;
                    self.peers.push(p);
                }
            }
            ("peers", _) => return Err(error(line, "peers is a list: [\"<name> <address> <key>\", ...]")),
            _ => return Err(error(line, &alloc::format!("\"{}\" is not a key hosts.toml has", key))),
        }
        Ok(())
    }
}

/// hosts.toml's text read: refused for anything it cannot read, for no
/// name or cluster, and for a peer under this host's own name.
pub fn parse(text: &str) -> Result<Config, String> {
    let mut r = Reader { name: None, cluster: None, nic: DEFAULT_NIC.to_string(), port: DEFAULT_PORT,
                         peers: Vec::new() };
    conf::parse(text, "hosts.toml", &mut r)?;
    let name = r.name.ok_or_else(|| String::from("hosts.toml has no name: this host's, in its cluster"))?;
    let cluster = r.cluster.ok_or_else(|| String::from("hosts.toml has no cluster: its id, 32 hex digits"))?;
    if r.peers.iter().any(|p| p.name == name) {
        return Err(alloc::format!("hosts.toml names {} among its peers, which is this host", name));
    }
    Ok(Config { name, cluster, nic: r.nic, port: r.port, peers: r.peers })
}

/// The text of a new hosts.toml: this host as `name` in the cluster
/// `cluster`, with no peers yet.
pub fn render_new(name: &str, cluster: &[u8; 16]) -> String {
    let mut s = String::from("# ipcnet's: this host in its cluster, and the cluster's other hosts -- \
                              ipcnet add, ipcnet remove (plans/ipc.md)\n");
    conf::put(&mut s, "name", name);
    conf::put(&mut s, "cluster", &hex(cluster));
    s.push_str("peers = []\n");
    s
}

/// hosts.toml's text with its cluster made `cluster`, every other line as
/// it was.
pub fn with_cluster(text: &str, cluster: &[u8; 16]) -> Result<String, String> {
    let mut line = String::new();
    conf::put(&mut line, "cluster", &hex(cluster));
    let mut r = Reader { name: None, cluster: None, nic: DEFAULT_NIC.to_string(), port: DEFAULT_PORT,
                         peers: Vec::new() };
    let text = conf::edit(text, "hosts.toml", &mut r, &[conf::Edit { key: "cluster", line: Some(line) }])?;
    if parse(&text)?.cluster != *cluster {
        return Err(String::from("hosts.toml would not read back as written"));
    }
    Ok(text)
}

/// hosts.toml's text with its peers made `peers`, every other line as it
/// was.
pub fn with_peers(text: &str, peers: &[PeerConf]) -> Result<String, String> {
    let mut line = String::from("peers = [");
    for (i, p) in peers.iter().enumerate() {
        line.push_str(if i == 0 { "\n  " } else { ",\n  " });
        conf::quoted(&mut line, &p.line());
    }
    line.push_str(if peers.is_empty() { "]\n" } else { ",\n]\n" });
    let mut r = Reader { name: None, cluster: None, nic: DEFAULT_NIC.to_string(), port: DEFAULT_PORT,
                         peers: Vec::new() };
    let text = conf::edit(text, "hosts.toml", &mut r, &[conf::Edit { key: "peers", line: Some(line) }])?;
    /* What is written must read back as what was meant */
    let back = parse(&text)?;
    if back.peers != peers {
        return Err(String::from("hosts.toml would not read back as written"));
    }
    Ok(text)
}

pub fn read_hosts(dir: &Dir) -> Result<Option<String>, String> {
    let path = dir.path("hosts.toml");
    match kcore::fs::read(&path, HOSTS_MAX_BYTES) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| alloc::format!("{} is not text", path)),
        Err(kcore::error::Error::NotFound) => Ok(None),
        Err(_) => Err(alloc::format!("cannot read {}", path)),
    }
}

/// hosts.toml written whole: through `<path>.new`, synced and put in
/// place, so that a cut-short write leaves the old file or the new.
pub fn write_hosts(dir: &Dir, text: &str) -> Result<(), String> {
    let path = dir.path("hosts.toml");
    let _ = kcore::fs::create_dir(&dir.root);
    kcore::fs::write(&path, text.as_bytes()).map_err(|_| alloc::format!("cannot write {}", path))
}

/// The host's key and certificate.
pub struct Identity {
    /// PKCS#8, DER.
    pub key: zeroize::Zeroizing<Vec<u8>>,
    pub cert: Vec<u8>,
    /// What every peer pins: the SHA-256 of the key as the certificate has
    /// it.
    pub fingerprint: Fingerprint,
}

fn read_text(path: &str) -> Result<Option<String>, String> {
    match kcore::fs::read(path, PEM_MAX) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| alloc::format!("{} is not text", path)),
        Err(kcore::error::Error::NotFound) => Ok(None),
        Err(_) => Err(alloc::format!("cannot read {}", path)),
    }
}

/// The host's key and certificate, one after the other in one file, so
/// that the two never go apart: written whole, by rename, as hosts.toml is.
const IDENTITY: &str = "tls/identity.pem";
/// A key made and not yet the host's: the one `cluster rekey` moves it to.
const STAGED: &str = "tls/next.pem";
/// The key and certificate as two files, as openssl makes them: read where
/// there is no identity.pem, and taken away when a staged key becomes the
/// host's.
const KEY: &str = "tls/key.pem";
const CERT: &str = "tls/cert.pem";

/// An identity from the PEM text of its key and of its certificate --
/// `what` says where they came from.
fn parse_identity(key_text: &str, cert_text: &str, what: &str) -> Result<Identity, String> {
    let key = zeroize::Zeroizing::new(web::x509::pem_decode("PRIVATE KEY", key_text)
        .ok_or_else(|| alloc::format!("{} has no PRIVATE KEY", what))?);
    let cert = web::x509::pem_decode("CERTIFICATE", cert_text)
        .ok_or_else(|| alloc::format!("{} has no CERTIFICATE", what))?;
    let fingerprint = tls::pinned::fingerprint(&cert).ok_or_else(|| alloc::format!("{} is not a certificate", what))?;
    Ok(Identity { key, cert, fingerprint })
}

/// The identity a file of both holds.
fn read_identity(path: &str) -> Result<Option<Identity>, String> {
    let Some(text) = read_text(path)? else { return Ok(None) };
    let text = zeroize::Zeroizing::new(text);
    parse_identity(&text, &text, path).map(Some)
}

/// The identity in the folder, made first if it has none -- for the host
/// `name`. Refused while the wall clock is not set, since the certificate
/// would start in 1970, and while the kernel's pool has no randomness.
pub fn identity(dir: &Dir, name: &str) -> Result<(Identity, bool), String> {
    if let Some(id) = read_identity(&dir.path(IDENTITY))? {
        return Ok((id, false));
    }
    let (key_path, cert_path) = (dir.path(KEY), dir.path(CERT));
    match (read_text(&key_path)?, read_text(&cert_path)?) {
        (Some(key_text), Some(cert_text)) => {
            let key_text = zeroize::Zeroizing::new(key_text);
            let what = alloc::format!("{} and {}", key_path, cert_path);
            Ok((parse_identity(&key_text, &cert_text, &what)?, false))
        }
        (None, None) => {
            let pem = make(name)?;
            let path = dir.path(IDENTITY);
            let _ = kcore::fs::create_dir(&dir.root);
            let _ = kcore::fs::create_dir(&dir.path("tls"));
            match kcore::fs::create(&path, pem.as_bytes()) {
                /* Made by another meanwhile: its, read as it is */
                Ok(()) | Err(kcore::error::Error::Busy) => {}
                Err(_) => return Err(alloc::format!("cannot write {}", path)),
            }
            let id = read_identity(&path)?.ok_or_else(|| alloc::format!("{} is gone", path))?;
            Ok((id, true))
        }
        _ => Err(alloc::format!("{} or {} is there without the other -- remove the other too, and a new key is \
                                 made (every peer then pins it anew)", key_path, cert_path)),
    }
}

/// The key staged and not yet the host's, if there is one: its fingerprint.
pub fn staged(dir: &Dir) -> Result<Option<Fingerprint>, String> {
    Ok(read_identity(&dir.path(STAGED))?.map(|id| id.fingerprint))
}

/// A new key staged -- or the one staged already -- for the host `name`,
/// whose key is `current`: its fingerprint, and whether it was made now. A
/// staged key that is the host's own already -- a switch cut short after
/// identity.pem took it -- is taken away first.
pub fn stage(dir: &Dir, name: &str, current: &Fingerprint) -> Result<(Fingerprint, bool), String> {
    let path = dir.path(STAGED);
    if let Some(id) = read_identity(&path)? {
        if id.fingerprint != *current {
            return Ok((id.fingerprint, false));
        }
        kcore::fs::remove(&path).map_err(|_| alloc::format!("cannot remove {}", path))?;
    }
    let pem = make(name)?;
    let _ = kcore::fs::create_dir(&dir.path("tls"));
    kcore::fs::create(&path, pem.as_bytes()).map_err(|_| alloc::format!("cannot write {}", path))?;
    let id = read_identity(&path)?.ok_or_else(|| alloc::format!("{} is gone", path))?;
    Ok((id.fingerprint, true))
}

/// The staged key made the host's: identity.pem written whole with it --
/// the one step that switches -- then next.pem, and the two files openssl
/// makes, taken away. The new identity.
pub fn promote(dir: &Dir) -> Result<Identity, String> {
    let path = dir.path(STAGED);
    let text = zeroize::Zeroizing::new(read_text(&path)?
        .ok_or_else(|| String::from("no key staged -- `cluster rekey` stages one"))?);
    let id = parse_identity(&text, &text, &path)?;
    kcore::fs::write(&dir.path(IDENTITY), text.as_bytes())
        .map_err(|_| alloc::format!("cannot write {}", dir.path(IDENTITY)))?;
    /* What is left of the old: read never again, identity.pem being there */
    for old in [STAGED, KEY, CERT] {
        let _ = kcore::fs::remove(&dir.path(old));
    }
    Ok(id)
}

/// A new key, and a certificate of it for the host `name`, as the PEM text
/// of both. Pinned TLS looks at the key alone, so its usage and dates are
/// for whoever reads it.
fn make(name: &str) -> Result<zeroize::Zeroizing<String>, String> {
    let now = kcore::time::wall_clock_secs();
    if now == 0 {
        return Err(String::from("the wall clock is not set: the host's certificate would start in 1970 -- set it \
                                 (an RTC, NTP) and try again"));
    }
    let mut serial = [0u8; 16];
    if !kcore::random::fill_random(&mut serial) {
        return Err(String::from("no randomness yet for the host's key: the kernel's pool has no source -- no \
                                 RDRAND, RNDR or virtio-rng (`random` says)"));
    }
    serial[0] &= 0x7F;
    let cn = alloc::format!("nos ipcnet {}", name);
    let dns = [name];
    let req = web::x509::Request {
        name: &cn,
        ips: &[],
        dns: &dns,
        serial,
        not_before: now.saturating_sub(SECS_PER_DAY),
        not_after: now + CERT_DAYS * SECS_PER_DAY,
        usage: web::x509::Usage::Server,
    };
    let mut secret = [0u8; 32];
    let mut made = None;
    for _ in 0..8 {
        if !kcore::random::fill_random(&mut secret) {
            break;
        }
        match web::x509::self_signed(&secret, &req) {
            Ok(id) => {
                made = Some(id);
                break;
            }
            /* Past the group's order, or zero: one in 2^32 -- draw again */
            Err(web::x509::Error::BadKey) => continue,
            Err(e) => {
                secret.zeroize();
                return Err(alloc::format!("the host's certificate could not be made: {:?}", e));
            }
        }
    }
    secret.zeroize();
    let id = made.ok_or_else(|| String::from("no randomness for the host's key"))?;
    let mut pem = zeroize::Zeroizing::new(web::x509::pem("PRIVATE KEY", &id.key)
        .map_err(|_| String::from("out of memory"))?);
    pem.push_str(&web::x509::pem("CERTIFICATE", &id.cert).map_err(|_| String::from("out of memory"))?);
    Ok(pem)
}
