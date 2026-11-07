//! The registry's commands -- what an entry of its log says to do -- and
//! the answers applying them gives, as bytes.
//!
//! Every command carries `now`: the wall clock of the member that proposed
//! it, in seconds -- the leader's. Applying one first lets go of every
//! invitation whose time ran out by then, so that every member does it at
//! the same entry, whatever its own clock says.

use alloc::string::String;
use alloc::vec::Vec;

use codec::{Reader, Writer};
use crate::{Fingerprint, Role};

/// The longest string a command carries: what its length byte counts. A
/// name is only read here -- whether it is a name is applying's to say, and
/// one longer than a name is answered `BadName` as any other that is none.
const STR_MAX: usize = u8::MAX as usize;

/// What the bytes of an entry begin with: this format's.
const VERSION: u8 = 1;

const TAG_INIT: u8 = 1;
const TAG_INVITE: u8 = 2;
const TAG_JOIN: u8 = 3;
const TAG_REMOVE: u8 = 4;
const TAG_REVOKE: u8 = 5;
const TAG_MAINTENANCE: u8 = 6;
const TAG_ADDRESS: u8 = 7;
const TAG_ADD_KEY: u8 = 8;
const TAG_DROP_KEY: u8 = 9;

/// A change of the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// The cluster made, with its first host -- the one entry an empty
    /// registry takes.
    Init { now: u64, cluster: [u8; 16], name: String, host: String, key: Fingerprint, ip: u32 },
    /// A host invited: `host` may join from `ip`, with the key `key` --
    /// pinned by every member's ipcnet from now on, as its own pins the
    /// inviting member's from the token -- until `expires`, proving the
    /// secret whose hash is `secret_hash`. `id` is the proposer's, a random
    /// number the token carries.
    Invite { now: u64, id: u64, host: String, ip: u32, key: Fingerprint, secret_hash: [u8; 32], expires: u64 },
    /// The host invitation `invitation` named, joined: it proved the secret
    /// over a connection pinned by the invitation's key.
    Join { now: u64, invitation: u64 },
    /// A host out, for good: its key and its node never taken again.
    Remove { now: u64, host: String },
    /// An invitation taken back before it was used.
    Revoke { now: u64, invitation: u64 },
    /// A host being worked on, or back: what every service of the cluster
    /// reads alike -- held off counting it out, nothing new placed on it.
    Maintenance { now: u64, host: String, on: bool },
    /// A host's address for `role` made `ip` -- a host that moved, or one
    /// with an address for another role -- or, for a role but `Ipc`, taken
    /// away with `ip` 0.
    Address { now: u64, host: String, role: Role, ip: u32 },
    /// A key added to a host's: the one it moves to, pinned beside the one
    /// it has until that one is dropped. Two at most.
    AddKey { now: u64, host: String, key: Fingerprint },
    /// A key of a host's taken off it -- never its last -- and retired:
    /// never taken again, by it or by anyone.
    DropKey { now: u64, host: String, key: Fingerprint },
}

impl Command {
    /// The proposer's clock, in seconds.
    pub fn now(&self) -> u64 {
        match self {
            Command::Init { now, .. }
            | Command::Invite { now, .. }
            | Command::Join { now, .. }
            | Command::Remove { now, .. }
            | Command::Revoke { now, .. }
            | Command::Maintenance { now, .. }
            | Command::Address { now, .. }
            | Command::AddKey { now, .. }
            | Command::DropKey { now, .. } => *now,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(VERSION);
        match self {
            Command::Init { now, cluster, name, host, key, ip } => {
                w.u8(TAG_INIT);
                w.u64(*now);
                w.fixed(cluster);
                w.str(name);
                w.str(host);
                w.fixed(key);
                w.u32(*ip);
            }
            Command::Invite { now, id, host, ip, key, secret_hash, expires } => {
                w.u8(TAG_INVITE);
                w.u64(*now);
                w.u64(*id);
                w.str(host);
                w.u32(*ip);
                w.fixed(key);
                w.fixed(secret_hash);
                w.u64(*expires);
            }
            Command::Join { now, invitation } => {
                w.u8(TAG_JOIN);
                w.u64(*now);
                w.u64(*invitation);
            }
            Command::Remove { now, host } => {
                w.u8(TAG_REMOVE);
                w.u64(*now);
                w.str(host);
            }
            Command::Revoke { now, invitation } => {
                w.u8(TAG_REVOKE);
                w.u64(*now);
                w.u64(*invitation);
            }
            Command::Maintenance { now, host, on } => {
                w.u8(TAG_MAINTENANCE);
                w.u64(*now);
                w.str(host);
                w.u8(u8::from(*on));
            }
            Command::Address { now, host, role, ip } => {
                w.u8(TAG_ADDRESS);
                w.u64(*now);
                w.str(host);
                w.u8(role.code());
                w.u32(*ip);
            }
            Command::AddKey { now, host, key } => {
                w.u8(TAG_ADD_KEY);
                w.u64(*now);
                w.str(host);
                w.fixed(key);
            }
            Command::DropKey { now, host, key } => {
                w.u8(TAG_DROP_KEY);
                w.u64(*now);
                w.str(host);
                w.fixed(key);
            }
        }
        w.buf
    }

    /// The command `bytes` are, or None: bytes no proposer wrote.
    pub fn decode(bytes: &[u8]) -> Option<Command> {
        let mut r = Reader::new(bytes);
        if r.u8()? != VERSION {
            return None;
        }
        let tag = r.u8()?;
        let now = r.u64()?;
        let cmd = match tag {
            TAG_INIT => Command::Init {
                now,
                cluster: r.fixed()?,
                name: String::from(r.str(STR_MAX)?),
                host: String::from(r.str(STR_MAX)?),
                key: r.fixed()?,
                ip: r.u32()?,
            },
            TAG_INVITE => Command::Invite {
                now,
                id: r.u64()?,
                host: String::from(r.str(STR_MAX)?),
                ip: r.u32()?,
                key: r.fixed()?,
                secret_hash: r.fixed()?,
                expires: r.u64()?,
            },
            TAG_JOIN => Command::Join { now, invitation: r.u64()? },
            TAG_REMOVE => Command::Remove { now, host: String::from(r.str(STR_MAX)?) },
            TAG_REVOKE => Command::Revoke { now, invitation: r.u64()? },
            TAG_MAINTENANCE => Command::Maintenance {
                now,
                host: String::from(r.str(STR_MAX)?),
                on: match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                },
            },
            TAG_ADDRESS => Command::Address {
                now,
                host: String::from(r.str(STR_MAX)?),
                role: Role::from_code(r.u8()?)?,
                ip: r.u32()?,
            },
            TAG_ADD_KEY => Command::AddKey { now, host: String::from(r.str(STR_MAX)?), key: r.fixed()? },
            TAG_DROP_KEY => Command::DropKey { now, host: String::from(r.str(STR_MAX)?), key: r.fixed()? },
            _ => return None,
        };
        r.done().then_some(cmd)
    }
}

/// Why a command changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Bytes that are no command.
    Malformed,
    /// A registry with no cluster yet takes `Init` alone.
    NotInitialized,
    /// `Init` once there is a cluster.
    Initialized,
    /// A cluster's or a host's name that is none.
    BadName,
    /// An address that is no host's: 0.0.0.0, or one of 255.x.x.x and the
    /// like is refused by the shell long before; here, 0 alone.
    BadAddress,
    /// A host of that name is in the cluster, or invited.
    HostExists,
    NoSuchHost,
    /// Another host's address, or an open invitation's.
    AddressInUse,
    /// An invitation of that number is open.
    InvitationExists,
    /// No open invitation of that number: used, taken back, never made, or
    /// past its time.
    InvitationGone,
    /// An invitation whose time is up by the proposer's clock already.
    Expired,
    /// A key a host of the cluster has, or an open invitation.
    KeyInUse,
    /// A key of a host removed: never taken again.
    KeyRemoved,
    /// As many hosts and invitations as the cluster takes, or as many
    /// removed hosts as it keeps.
    Full,
    /// The cluster's last host: a cluster is never left with none.
    LastHost,
    /// A host with two keys already: one is dropped before another comes.
    KeysFull,
    /// Not a key of that host's.
    NotItsKey,
    /// A host's last key: a host is never left with none.
    LastKey,
}

const REFUSALS: [Refusal; 18] = [
    Refusal::Malformed,
    Refusal::NotInitialized,
    Refusal::Initialized,
    Refusal::BadName,
    Refusal::BadAddress,
    Refusal::HostExists,
    Refusal::NoSuchHost,
    Refusal::AddressInUse,
    Refusal::InvitationExists,
    Refusal::InvitationGone,
    Refusal::Expired,
    Refusal::KeyInUse,
    Refusal::KeyRemoved,
    Refusal::Full,
    Refusal::LastHost,
    Refusal::KeysFull,
    Refusal::NotItsKey,
    Refusal::LastKey,
];

impl Refusal {
    fn code(self) -> u8 {
        /* Its place in REFUSALS, plus one: in the table, by construction */
        REFUSALS.iter().position(|&r| r == self).map_or(0, |i| i as u8 + 1)
    }

    fn from_code(code: u8) -> Option<Refusal> {
        REFUSALS.get(usize::from(code).checked_sub(1)?).copied()
    }

    /// What it says, for a person.
    pub fn says(self) -> &'static str {
        match self {
            Refusal::Malformed => "bytes that are no command",
            Refusal::NotInitialized => "no cluster yet -- `cluster init` makes one",
            Refusal::Initialized => "the cluster is made already",
            Refusal::BadName => "not a name: lowercase letters, digits and -, up to 63",
            Refusal::BadAddress => "not a host's address",
            Refusal::HostExists => "a host of that name is in the cluster or invited",
            Refusal::NoSuchHost => "no host of that name",
            Refusal::AddressInUse => "another host's address, or an open invitation's",
            Refusal::InvitationExists => "an invitation of that number is open",
            Refusal::InvitationGone => "no such invitation: used, taken back, past its time or never made",
            Refusal::Expired => "its time is up already",
            Refusal::KeyInUse => "a key another host of the cluster has, or an invitation",
            Refusal::KeyRemoved => "the key of a host removed from the cluster, or one a host dropped",
            Refusal::Full => "as many hosts and invitations as the cluster takes",
            Refusal::LastHost => "the cluster's last host",
            Refusal::KeysFull => "the host has two keys already: one is dropped before another comes",
            Refusal::NotItsKey => "not a key of that host's",
            Refusal::LastKey => "the host's last key",
        }
    }
}

/// What applying a command gave: done -- with the node of the host it made,
/// removed or changed, 0 for an invitation's -- or refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Done { node: u64 },
    Refused(Refusal),
}

impl Answer {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Answer::Done { node } => {
                w.u8(0);
                w.u64(*node);
            }
            Answer::Refused(why) => {
                w.u8(why.code());
            }
        }
        w.buf
    }

    pub fn decode(bytes: &[u8]) -> Option<Answer> {
        let mut r = Reader::new(bytes);
        let answer = match r.u8()? {
            0 => Answer::Done { node: r.u64()? },
            code => Answer::Refused(Refusal::from_code(code)?),
        };
        r.done().then_some(answer)
    }
}
