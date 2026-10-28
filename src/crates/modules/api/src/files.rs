//! What the API keeps in its folder -- /etc/api unless `insmod api.ko
//! conf=` names another -- each file the machine's own:
//!
//! - `api.toml`, where it listens and whom it takes (config.rs);
//! - `tls/key.pem` and `tls/cert.pem`, the host's key and its certificate,
//!   made on the first start and never written over: a client pins the
//!   certificate's key, and a new one would lock every client out;
//! - `host`, the host's id, a UUID made with them: what a cluster's panel
//!   knows the host by, whatever its addresses;
//! - `clients`, the keys let in, a line each: `<fingerprint> <role> <name>`,
//!   the fingerprint the SHA-256 of the key's SubjectPublicKeyInfo in hex;
//! - `audit.log`, a line for every call that changes something, kept to a
//!   megabyte and the one before it in `audit.log.1`.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;
use tls::server::Fingerprint;
use zeroize::Zeroize;

pub const DEFAULT_DIR: &str = "/etc/api";
/// What a key, a certificate and the clients file come to, at most.
const PEM_MAX: usize = 16 * 1024;
const CLIENTS_MAX: usize = 64 * 1024;
/// Clients the file may name.
pub const CLIENTS_LIMIT: usize = 64;
/// A client's name: what the audit log and `api clients` say it as.
const NAME_MAX: usize = 32;
/// How long the host's certificate is good for: what browsers take, should
/// one ever be shown it (CA/B's limit for a server's certificate).
const CERT_DAYS: u64 = 825;
const SECS_PER_DAY: u64 = 86_400;
/// The audit log's size before it is turned over.
const AUDIT_MAX: u64 = 1024 * 1024;
/// The name the certificate is for besides the host's addresses.
const DNS_NAME: &str = "nos";

/// What a client may do: `read`, the read methods alone and the streams;
/// `admin`, everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Admin,
    Read,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Read => "read",
        }
    }

    pub fn parse(text: &str) -> Option<Role> {
        match text {
            "admin" => Some(Role::Admin),
            "read" => Some(Role::Read),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Client {
    pub key: Fingerprint,
    pub role: Role,
    pub name: String,
}

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

/// A fingerprint as the clients file and `api status` write it: 64 hex
/// digits.
pub fn hex(key: &Fingerprint) -> String {
    let mut s = String::with_capacity(64);
    for b in key {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

pub fn parse_hex(text: &str) -> Option<Fingerprint> {
    let b = text.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, pair) in b.chunks(2).enumerate() {
        let digit = |c: u8| (c as char).to_digit(16);
        key[i] = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(key)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= NAME_MAX
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@'))
}

/// The clients file read: strictly -- a line it cannot read, a fingerprint
/// or a name twice, is refused with its line, and nothing is let in by it.
pub fn parse_clients(text: &str) -> Result<Vec<Client>, String> {
    let mut out: Vec<Client> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(key), Some(role), Some(name), None) = (words.next(), words.next(), words.next(), words.next()) else {
            return Err(alloc::format!("line {}: <fingerprint> <admin|read> <name>", i + 1));
        };
        let key = parse_hex(key).ok_or_else(|| alloc::format!("line {}: a fingerprint is 64 hex digits", i + 1))?;
        let role = Role::parse(role).ok_or_else(|| alloc::format!("line {}: the role is admin or read", i + 1))?;
        if !valid_name(name) {
            return Err(alloc::format!("line {}: a name is 1 to {} letters, digits, _ - . @", i + 1, NAME_MAX));
        }
        if out.iter().any(|c| c.key == key || c.name == name) {
            return Err(alloc::format!("line {}: a key or a name given twice", i + 1));
        }
        if out.len() == CLIENTS_LIMIT {
            return Err(alloc::format!("more than {} clients", CLIENTS_LIMIT));
        }
        out.push(Client { key, role, name: name.to_string() });
    }
    Ok(out)
}

pub fn render_clients(clients: &[Client]) -> String {
    let mut s = String::from("# The keys the host API lets in: <fingerprint> <admin|read> <name> -- api client add, api client del\n");
    for c in clients {
        let _ = writeln!(s, "{} {} {}", hex(&c.key), c.role.name(), c.name);
    }
    s
}

pub fn read_clients(dir: &Dir) -> Result<Vec<Client>, String> {
    match kcore::fs::read(&dir.path("clients"), CLIENTS_MAX) {
        Ok(bytes) => {
            let text = core::str::from_utf8(&bytes).map_err(|_| String::from("clients is not UTF-8"))?;
            parse_clients(text).map_err(|e| alloc::format!("{}: {}", dir.path("clients"), e))
        }
        /* None given yet: nobody is let in */
        Err(kcore::error::Error::NotFound) => Ok(Vec::new()),
        Err(_) => Err(alloc::format!("cannot read {}", dir.path("clients"))),
    }
}

/// The clients file written whole: the file ABI's replace, through
/// `<path>.new`, synced and then put in place -- so a cut-short write never
/// leaves half a list, but the old one or the new.
pub fn write_clients(dir: &Dir, clients: &[Client]) -> Result<(), String> {
    let text = render_clients(clients);
    let path = dir.path("clients");
    kcore::fs::write(&path, text.as_bytes()).map_err(|_| alloc::format!("cannot write {}", path))
}

/// The host's key and certificate, and its id.
pub struct Identity {
    /// PKCS#8, DER.
    pub key: zeroize::Zeroizing<Vec<u8>>,
    pub cert: Vec<u8>,
    pub cert_pem: String,
    pub key_fingerprint: Fingerprint,
    pub id: String,
}

fn read_text(path: &str) -> Result<Option<String>, String> {
    match kcore::fs::read(path, PEM_MAX) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| alloc::format!("{} is not text", path)),
        Err(kcore::error::Error::NotFound) => Ok(None),
        Err(_) => Err(alloc::format!("cannot read {}", path)),
    }
}

/// The identity in the folder, made first if it has none -- refused while
/// the wall clock is not set, since the certificate would start in 1970.
/// `ips` are the addresses the certificate is for, with the name `nos`.
pub fn identity(dir: &Dir, ips: &[[u8; 4]]) -> Result<(Identity, bool), String> {
    let key_path = dir.path("tls/key.pem");
    let cert_path = dir.path("tls/cert.pem");
    let id_path = dir.path("host");
    let made = match (read_text(&key_path)?, read_text(&cert_path)?) {
        (Some(_), Some(_)) => false,
        (None, None) => {
            make(dir, ips)?;
            true
        }
        _ => return Err(alloc::format!("{} or {} is there without the other -- remove the other too, and a new \
                                        identity is made (every client then pins its key again)", key_path, cert_path)),
    };
    let key_text = zeroize::Zeroizing::new(read_text(&key_path)?.unwrap_or_default());
    let cert_pem = read_text(&cert_path)?.unwrap_or_default();
    let key = zeroize::Zeroizing::new(web::x509::pem_decode("PRIVATE KEY", &key_text)
        .ok_or_else(|| alloc::format!("{} has no PRIVATE KEY", key_path))?);
    let cert = web::x509::pem_decode("CERTIFICATE", &cert_pem)
        .ok_or_else(|| alloc::format!("{} has no CERTIFICATE", cert_path))?;
    let key_fingerprint = tls::server::fingerprint(&cert).ok_or_else(|| alloc::format!("{} is not a certificate", cert_path))?;
    let id = match read_text(&id_path)? {
        Some(t) => t.trim().to_string(),
        None => return Err(alloc::format!("{} is missing -- the host's id is made with its key", id_path)),
    };
    Ok((Identity { key, cert, cert_pem, key_fingerprint, id }, made))
}

/// A UUID of the random kind (RFC 9562's version 4).
fn uuid() -> Option<String> {
    let mut u = [0u8; 16];
    if !kcore::random::fill_random(&mut u) {
        return None;
    }
    u[6] = (u[6] & 0x0F) | 0x40;
    u[8] = (u[8] & 0x3F) | 0x80;
    let mut s = String::with_capacity(36);
    for (i, b) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{:02x}", b);
    }
    Some(s)
}

/// A new key, and a certificate for it of `name`, good for `days` from a
/// day ago (a peer's clock a day behind ours) -- refused while the wall
/// clock is not set, since it would start in 1970, and while the kernel's
/// pool has no randomness for the key.
pub fn new_key(name: &str, ips: &[[u8; 4]], dns: &[&str], days: u64, usage: web::x509::Usage)
               -> Result<web::x509::Identity, String> {
    let now = kcore::time::wall_clock_secs();
    if now == 0 {
        return Err(alloc::format!("the wall clock is not set: {}'s certificate would start in 1970 -- set it (an \
                                   RTC, NTP) and try again", name));
    }
    let mut serial = [0u8; 16];
    if !kcore::random::fill_random(&mut serial) {
        return Err(alloc::format!("no randomness yet for {}'s key: the kernel's pool has no source -- no RDRAND, \
                                   RNDR or virtio-rng (`random` says) -- and no TLS handshake can be done without \
                                   one either", name));
    }
    serial[0] &= 0x7F;
    let req = web::x509::Request {
        name,
        ips,
        dns,
        serial,
        not_before: now.saturating_sub(SECS_PER_DAY),
        not_after: now + days * SECS_PER_DAY,
        usage,
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
                return Err(alloc::format!("{}'s certificate could not be made: {:?}", name, e));
            }
        }
    }
    secret.zeroize();
    made.ok_or_else(|| alloc::format!("no randomness for {}'s key", name))
}

fn make(dir: &Dir, ips: &[[u8; 4]]) -> Result<(), String> {
    let id = new_key("nos host API", ips, &[DNS_NAME], CERT_DAYS, web::x509::Usage::Server)?;
    let key_pem = zeroize::Zeroizing::new(web::x509::pem("PRIVATE KEY", &id.key).map_err(|_| String::from("out of memory"))?);
    let cert_pem = web::x509::pem("CERTIFICATE", &id.cert).map_err(|_| String::from("out of memory"))?;
    let host = uuid().ok_or_else(|| String::from("no randomness for the host's id"))?;
    let _ = kcore::fs::create_dir(&dir.root);
    let _ = kcore::fs::create_dir(&dir.path("tls"));
    kcore::fs::create(&dir.path("tls/key.pem"), key_pem.as_bytes())
        .map_err(|_| alloc::format!("cannot write {}", dir.path("tls/key.pem")))?;
    kcore::fs::create(&dir.path("tls/cert.pem"), cert_pem.as_bytes())
        .map_err(|_| alloc::format!("cannot write {}", dir.path("tls/cert.pem")))?;
    match kcore::fs::create(&dir.path("host"), alloc::format!("{}\n", host).as_bytes()) {
        Ok(()) | Err(kcore::error::Error::Busy) => Ok(()),
        Err(_) => Err(alloc::format!("cannot write {}", dir.path("host"))),
    }
}

/// A line of the audit log: the wall clock, who, from where, what, and
/// how it ended -- the log turned over past its size.
pub fn audit(dir: &Dir, line: &str) {
    let path = dir.path("audit.log");
    if kcore::fs::size(&path).is_ok_and(|n| n >= AUDIT_MAX) {
        let old = dir.path("audit.log.1");
        let _ = kcore::fs::remove(&old);
        let _ = kcore::fs::rename(&path, &old);
    }
    let mut text = String::new();
    let _ = writeln!(text, "{} {}", kcore::time::wall_clock_secs(), line);
    if kcore::fs::append(&path, text.as_bytes()).is_err() {
        kcore::trace!(0, "api: cannot append to {}", path);
    }
}
