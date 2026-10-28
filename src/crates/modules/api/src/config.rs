//! /etc/api/api.toml: where the API listens and whom it takes, in the strict
//! TOML `conf` reads.
//!
//!     port = 39234
//!     listen = ["hv0", "eth0"]
//!     allow_eth0 = ["203.0.113.0/24", "198.51.100.7"]
//!     max_connections = 8
//!
//! `listen` names the interfaces -- hv0, the guests' switch, alone unless
//! told -- and `allow_<interface>` the networks a connection on it may come
//! from: none said, any. A client still has to show a key the API was given
//! (`clients`); the allowlist is what keeps everyone else from even its
//! handshake. Every key is optional; a key the file does not have, or one
//! given twice, is refused with its line.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use conf::{error, Value};
use web::cidr::Cidr;

/// The API's own port: the panel's relay is the host's 39233.
pub const DEFAULT_PORT: u16 = 39234;
/// The guests' switch, where the panel's VM is.
pub const DEFAULT_LISTEN: &str = "hv0";

/// Connections at once, in all; of one client's key; and in their
/// handshake. Each is a task and two of the kernel's 64 TCP slots, and a
/// handshake costs a signature and two checks of one.
pub const DEFAULT_MAX_CONNECTIONS: u64 = 8;
pub const DEFAULT_MAX_PER_KEY: u64 = 4;
pub const DEFAULT_MAX_HANDSHAKES: u64 = 2;
/// What the file may raise them to.
pub const LIMIT_MAX: u64 = 32;
/// A connection that says nothing for this long is let go.
pub const DEFAULT_IDLE_SECS: u64 = 60;
pub const IDLE_SECS_MAX: u64 = 3600;
/// Interfaces listened on, and networks one allows, at most.
const LISTEN_MAX: usize = 4;
const ALLOW_MAX: usize = 64;
/// The most api.toml may be.
pub const MAX_BYTES: usize = 16 * 1024;
/// An interface's name, as `net` names them.
const NIC_NAME_MAX: usize = 15;

pub struct Config {
    pub port: u16,
    pub listen: Vec<String>,
    /// Each interface's allowlist, for those that have one.
    pub allow: Vec<(String, Vec<Cidr>)>,
    pub max_connections: usize,
    pub max_per_key: usize,
    pub max_handshakes: usize,
    pub idle_secs: u64,
}

impl Config {
    pub fn defaults() -> Config {
        Config {
            port: DEFAULT_PORT,
            listen: alloc::vec![DEFAULT_LISTEN.to_string()],
            allow: Vec::new(),
            max_connections: DEFAULT_MAX_CONNECTIONS as usize,
            max_per_key: DEFAULT_MAX_PER_KEY as usize,
            max_handshakes: DEFAULT_MAX_HANDSHAKES as usize,
            idle_secs: DEFAULT_IDLE_SECS,
        }
    }

    /// Whether a connection on `nic` from `ip` may be taken: its
    /// interface's allowlist has it, or the interface has none.
    pub fn allows(&self, nic: &str, ip: u32) -> bool {
        match self.allow.iter().find(|(n, _)| n == nic) {
            Some((_, nets)) => nets.iter().any(|c| c.contains(ip)),
            None => true,
        }
    }
}

fn nic_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= NIC_NAME_MAX && name.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// An address or a network: `a.b.c.d` alone is that address.
pub fn network(text: &str) -> Option<Cidr> {
    Cidr::parse(text)
}

struct Reader {
    c: Config,
}

const KEYS: [&str; 6] = ["port", "listen", "max_connections", "max_per_key", "max_handshakes", "idle_secs"];

impl conf::Keys for Reader {
    fn known(&mut self, line: usize, key: &str) -> Result<(), String> {
        match key.strip_prefix("allow_") {
            Some(nic) if nic_name(nic) => Ok(()),
            _ if KEYS.contains(&key) => Ok(()),
            _ => Err(error(line, &alloc::format!("\"{}\" is not a key api.toml has", key))),
        }
    }

    fn assign(&mut self, line: usize, key: &str, value: Value) -> Result<(), String> {
        let c = &mut self.c;
        let count = |v: &Value, max: u64| match v {
            Value::Int(n) if (1..=max).contains(n) => Ok(*n),
            _ => Err(error(line, &alloc::format!("{} is a number from 1 to {}", key, max))),
        };
        match key {
            "port" => c.port = u16::try_from(count(&value, u64::from(u16::MAX))?).unwrap_or(DEFAULT_PORT),
            "max_connections" => c.max_connections = count(&value, LIMIT_MAX)? as usize,
            "max_per_key" => c.max_per_key = count(&value, LIMIT_MAX)? as usize,
            "max_handshakes" => c.max_handshakes = count(&value, LIMIT_MAX)? as usize,
            "idle_secs" => c.idle_secs = count(&value, IDLE_SECS_MAX)?,
            "listen" => {
                let Value::List(names) = value else {
                    return Err(error(line, "listen is a list of interfaces: [\"hv0\", \"eth0\"]"));
                };
                if names.is_empty() || names.len() > LISTEN_MAX {
                    return Err(error(line, &alloc::format!("listen names 1 to {} interfaces", LISTEN_MAX)));
                }
                for (i, n) in names.iter().enumerate() {
                    if !nic_name(n) || names[..i].contains(n) {
                        return Err(error(line, &alloc::format!("\"{}\" is not an interface's name, or is given twice", n)));
                    }
                }
                c.listen = names;
            }
            _ => {
                /* allow_<interface>, which `known` let through */
                let nic = key.strip_prefix("allow_").unwrap_or(key);
                let Value::List(texts) = value else {
                    return Err(error(line, &alloc::format!("{} is a list of networks: [\"10.0.100.0/24\"]", key)));
                };
                if texts.len() > ALLOW_MAX {
                    return Err(error(line, &alloc::format!("{} has more than {} networks", key, ALLOW_MAX)));
                }
                let mut nets = Vec::new();
                nets.try_reserve_exact(texts.len()).map_err(|_| String::from("out of memory"))?;
                for t in &texts {
                    nets.push(network(t).ok_or_else(|| error(line, &alloc::format!("\"{}\" is not an address or a network", t)))?);
                }
                c.allow.try_reserve(1).map_err(|_| String::from("out of memory"))?;
                c.allow.push((nic.to_string(), nets));
            }
        }
        Ok(())
    }
}

/// api.toml's text read; what is not said is its default.
pub fn parse(text: &str) -> Result<Config, String> {
    let mut reader = Reader { c: Config::defaults() };
    conf::parse(text, "api.toml", &mut reader)?;
    Ok(reader.c)
}
