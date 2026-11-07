//! What changes the metadata, and what it is answered: each as bytes, read
//! whole and every count bounded.

use alloc::string::String;
use alloc::vec::Vec;

use codec::{Reader, Writer};

/// A change to the metadata, as a log entry carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// The NBS cluster made, `host`'s mds -- as member `member` -- its first.
    Init { cluster: [u8; 16], name: String, host: String, member: u64 },
    /// `host` a member, as `member`: the incarnation its mds made when it was
    /// offered the cluster. Taken again as done where it is that member
    /// already.
    Add { host: String, member: u64 },
    /// `host`'s member out, for good: its incarnation never taken again. Done
    /// again where it is out already.
    Remove { host: String },
}

const INIT: u8 = 1;
const ADD: u8 = 2;
const REMOVE: u8 = 3;

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Command::Init { cluster, name, host, member } => {
                w.u8(INIT);
                w.fixed(cluster);
                w.str(name);
                w.str(host);
                w.u64(*member);
            }
            Command::Add { host, member } => {
                w.u8(ADD);
                w.str(host);
                w.u64(*member);
            }
            Command::Remove { host } => {
                w.u8(REMOVE);
                w.str(host);
            }
        }
        w.buf
    }

    /// The command `bytes` are, read whole: None for anything else.
    pub fn decode(bytes: &[u8]) -> Option<Command> {
        let mut r = Reader::new(bytes);
        let cmd = match r.u8()? {
            INIT => {
                let cluster = r.fixed()?;
                let name = String::from(r.str(crate::NAME_MAX)?);
                let host = String::from(r.str(ipcwire::HOST_MAX)?);
                let member = r.u64()?;
                Command::Init { cluster, name, host, member }
            }
            ADD => {
                let host = String::from(r.str(ipcwire::HOST_MAX)?);
                let member = r.u64()?;
                Command::Add { host, member }
            }
            REMOVE => Command::Remove { host: String::from(r.str(ipcwire::HOST_MAX)?) },
            _ => return None,
        };
        r.done().then_some(cmd)
    }
}

/// Why a command was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Bytes that are no command.
    Malformed,
    /// No NBS cluster yet: `Init` alone is taken.
    NotInitialized,
    /// `Init` once there is a cluster.
    Initialized,
    /// A cluster's or a host's name that is none.
    BadName,
    /// Member 0, which names none.
    BadMember,
    /// The host is a member already, as another incarnation: one whose state
    /// was lost, if it says it is a new one -- `nbs host replace`.
    HostIsMember,
    /// An incarnation another host's member is.
    MemberInUse,
    /// An incarnation removed: never taken again, as a member that forgot
    /// its log must never vote as the one that had it.
    MemberRemoved,
    /// As many members as a cluster takes, or as many removed as it keeps.
    Full,
    /// No member of that host, now or before.
    NoSuchHost,
    /// The cluster's last member.
    LastMember,
}

impl Refusal {
    pub fn says(self) -> &'static str {
        match self {
            Refusal::Malformed => "bytes that are no command",
            Refusal::NotInitialized => "no NBS cluster yet -- `nbs init` makes one",
            Refusal::Initialized => "the NBS cluster is made already",
            Refusal::BadName => "not a name: lowercase letters, digits and -, up to 63",
            Refusal::BadMember => "member 0 is none",
            Refusal::HostIsMember => "the host is a member already, as another incarnation -- if its state was lost, \
                                      `nbs host replace` it",
            Refusal::MemberInUse => "another host's member is that incarnation",
            Refusal::MemberRemoved => "that incarnation was removed: never taken again",
            Refusal::Full => "as many members as an NBS cluster takes, or as many removed as it keeps",
            Refusal::NoSuchHost => "no member of that host",
            Refusal::LastMember => "the NBS cluster's last member",
        }
    }

    fn code(self) -> u8 {
        match self {
            Refusal::Malformed => 1,
            Refusal::NotInitialized => 2,
            Refusal::Initialized => 3,
            Refusal::BadName => 4,
            Refusal::BadMember => 5,
            Refusal::HostIsMember => 6,
            Refusal::MemberInUse => 7,
            Refusal::MemberRemoved => 8,
            Refusal::Full => 9,
            Refusal::NoSuchHost => 10,
            Refusal::LastMember => 11,
        }
    }

    fn from_code(c: u8) -> Option<Refusal> {
        Some(match c {
            1 => Refusal::Malformed,
            2 => Refusal::NotInitialized,
            3 => Refusal::Initialized,
            4 => Refusal::BadName,
            5 => Refusal::BadMember,
            6 => Refusal::HostIsMember,
            7 => Refusal::MemberInUse,
            8 => Refusal::MemberRemoved,
            9 => Refusal::Full,
            10 => Refusal::NoSuchHost,
            11 => Refusal::LastMember,
            _ => return None,
        })
    }
}

/// What applying a command gave: done -- with the member it made, took or
/// removed -- or refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Done { member: u64 },
    Refused(Refusal),
}

impl Answer {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Answer::Done { member } => {
                w.u8(0);
                w.u64(*member);
            }
            Answer::Refused(why) => {
                w.u8(1);
                w.u8(why.code());
            }
        }
        w.buf
    }

    pub fn decode(bytes: &[u8]) -> Option<Answer> {
        let mut r = Reader::new(bytes);
        let a = match r.u8()? {
            0 => Answer::Done { member: r.u64()? },
            1 => Answer::Refused(Refusal::from_code(r.u8()?)?),
            _ => return None,
        };
        r.done().then_some(a)
    }
}
