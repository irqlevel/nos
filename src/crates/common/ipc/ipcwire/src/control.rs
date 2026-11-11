//! What a module of this host says to ipcnet through its control mailbox,
//! [`NAME`] -- a `Host` name, so that nothing from another host reaches it
//! -- and what ipcnet answers: who this host is in its cluster, and the
//! peers it is to have, which the cluster's registry keeps
//! (plans/cluster.md). ipcnet writes hosts.toml from what it is told, and
//! nobody else writes it.
//!
//! Each is a message's payload, its kind the message's tag. Numbers are
//! little-endian; a string has its length in a byte in front. Everything is
//! read checked -- nothing past the end, every count bounded, every name a
//! host's -- and a payload that is none is refused, never taken in part.

use alloc::string::String;
use alloc::vec::Vec;

/// ipcnet's control mailbox.
pub const NAME: &str = "ipcnet.ctl";

/// Asked with no payload; answered under the same tag with an [`Identity`].
pub const TAG_ID: u32 = 1;
/// Asked with [`Peers`]; answered [`TAG_OK`] once hosts.toml says them and
/// ipcnet has them, or [`TAG_REFUSED`].
pub const TAG_PEERS: u32 = 2;
pub const TAG_OK: u32 = 3;
/// The payload: why, in words.
pub const TAG_REFUSED: u32 = 4;
/// Asked with a cluster's id, 16 bytes: this host's cluster made that one --
/// a host joining it, which has no peers yet. Answered [`TAG_OK`], or
/// [`TAG_REFUSED`] for a host with peers in another.
pub const TAG_JOIN: u32 = 5;
/// Asked with no payload: a key made and staged -- or the one staged
/// already -- not yet this host's. Answered under the same tag with its
/// fingerprint, 32 bytes, or [`TAG_REFUSED`].
pub const TAG_KEY_STAGE: u32 = 6;
/// Asked with no payload: the staged key made this host's -- every
/// connection from now on made with it, the connections up kept. Answered
/// under the same tag with its fingerprint, or [`TAG_REFUSED`] where none
/// is staged.
pub const TAG_KEY_USE: u32 = 7;

/// Peers a host has at most, as ipcnet takes them.
pub const PEERS_MAX: usize = 16;
/// Keys a peer is pinned by: its own, and the one it moves to.
pub const KEYS_MAX: usize = 2;
/// An interface's name, as `net` names them.
pub const NIC_MAX: usize = 15;

/// 2 since a peer's port is said (#99).
const VERSION: u8 = 2;

/// This host, as ipcnet has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub cluster: [u8; 16],
    /// Its key's fingerprint, which every peer pins.
    pub key: [u8; 32],
    /// The interface it serves on, and that interface's IPv4 address --
    /// host order, 0 while it has none.
    pub nic: String,
    pub ip: u32,
    pub port: u16,
    /// Whether it serves: `ipcnet start` done, and not stopped.
    pub serving: bool,
    /// A key staged and not yet this host's, if there is one.
    pub next: Option<[u8; 32]>,
}

/// A peer of this host's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub ip: u32,
    /// The port its ipcnet listens on, which this host dials -- 0 for
    /// ipcnet's own, `crate::PORT`, as before a peer's port was said: two
    /// hosts behind one address, a NAT's, are told apart by their ports
    /// (#99).
    pub port: u16,
    pub keys: Vec<[u8; 32]>,
    /// The one name the peer may reach here, by that name alone -- a host
    /// invited, which reaches the cluster's member and nothing else until
    /// it has joined; None for a host of the cluster.
    pub only: Option<String>,
}

/// The peers this host is to have -- every one, in place of those it has --
/// in cluster `cluster`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peers {
    pub cluster: [u8; 16],
    pub peers: Vec<Peer>,
}

struct W(Vec<u8>);

impl W {
    fn str(&mut self, s: &str) {
        /* Every string here is a name or an interface's: far shorter */
        let n = s.len().min(usize::from(u8::MAX));
        self.0.push(n as u8);
        self.0.extend_from_slice(&s.as_bytes()[..n]);
    }
}

struct R<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let got = self.buf.get(self.at..end)?;
        self.at = end;
        Some(got)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn fixed<const N: usize>(&mut self) -> Option<[u8; N]> {
        let mut b = [0u8; N];
        b.copy_from_slice(self.take(N)?);
        Some(b)
    }

    fn u16(&mut self) -> Option<u16> {
        self.fixed().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Option<u32> {
        self.fixed().map(u32::from_le_bytes)
    }

    fn str(&mut self, max: usize) -> Option<&'a str> {
        let n = usize::from(self.u8()?);
        if n > max {
            return None;
        }
        core::str::from_utf8(self.take(n)?).ok()
    }

    fn done(&self) -> bool {
        self.at == self.buf.len()
    }
}

impl Identity {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        w.0.push(VERSION);
        w.str(&self.name);
        w.0.extend_from_slice(&self.cluster);
        w.0.extend_from_slice(&self.key);
        w.str(&self.nic);
        w.0.extend_from_slice(&self.ip.to_le_bytes());
        w.0.extend_from_slice(&self.port.to_le_bytes());
        w.0.push(u8::from(self.serving));
        match &self.next {
            Some(k) => {
                w.0.push(1);
                w.0.extend_from_slice(k);
            }
            None => w.0.push(0),
        }
        w.0
    }

    pub fn decode(bytes: &[u8]) -> Option<Identity> {
        let mut r = R { buf: bytes, at: 0 };
        if r.u8()? != VERSION {
            return None;
        }
        let name = r.str(crate::HOST_MAX)?;
        let cluster = r.fixed()?;
        let key = r.fixed()?;
        let nic = r.str(NIC_MAX)?;
        let ip = r.u32()?;
        let port = r.u16()?;
        let serving = match r.u8()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        let next = match r.u8()? {
            0 => None,
            1 => Some(r.fixed()?),
            _ => return None,
        };
        if !r.done() || !crate::valid_host(name) {
            return None;
        }
        Some(Identity { name: String::from(name), cluster, key, nic: String::from(nic), ip, port, serving, next })
    }
}

impl Peers {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        w.0.push(VERSION);
        w.0.extend_from_slice(&self.cluster);
        w.0.push(self.peers.len().min(usize::from(u8::MAX)) as u8);
        for p in &self.peers {
            w.str(&p.name);
            w.0.extend_from_slice(&p.ip.to_le_bytes());
            w.0.extend_from_slice(&p.port.to_le_bytes());
            w.0.push(p.keys.len().min(usize::from(u8::MAX)) as u8);
            for k in &p.keys {
                w.0.extend_from_slice(k);
            }
            w.str(p.only.as_deref().unwrap_or(""));
        }
        w.0
    }

    /// The peers `bytes` say: each a host's name, an address that is not 0
    /// and its port, one key or two and at most a mailbox's name it alone
    /// may reach, no name twice, at most `PEERS_MAX` -- or None.
    pub fn decode(bytes: &[u8]) -> Option<Peers> {
        let mut r = R { buf: bytes, at: 0 };
        if r.u8()? != VERSION {
            return None;
        }
        let cluster = r.fixed()?;
        let n = usize::from(r.u8()?);
        if n > PEERS_MAX {
            return None;
        }
        let mut peers: Vec<Peer> = Vec::with_capacity(n);
        for _ in 0..n {
            let name = r.str(crate::HOST_MAX)?;
            let ip = r.u32()?;
            let port = r.u16()?;
            let k = usize::from(r.u8()?);
            if !crate::valid_host(name) || ip == 0 || k == 0 || k > KEYS_MAX || peers.iter().any(|p| p.name == name) {
                return None;
            }
            let mut keys = Vec::with_capacity(k);
            for _ in 0..k {
                keys.push(r.fixed()?);
            }
            let only = match r.str(crate::NAME_MAX)? {
                "" => None,
                n if crate::valid_name(n) => Some(String::from(n)),
                _ => return None,
            };
            peers.push(Peer { name: String::from(name), ip, port, keys, only });
        }
        r.done().then_some(Peers { cluster, peers })
    }
}
