//! NBS's metadata (plans/nbs.md): the state every host's mds keeps through
//! raft's replicated state machine, and the commands that change it.
//!
//! [`State`] is raft's [`StateMachine`]: its log's entries are
//! [`Command`]s, which applying answers with an [`Answer`]. For now it holds
//! the NBS cluster's identity and its members (step N5); volumes, devices
//! and placement come with N6.
//!
//! - **The cluster**: its id, 128 random bits made at `nbs init`, and a
//!   name. Every NBS message carries the id, and an mds takes none of
//!   another cluster's.
//! - **Its members**, in the order they joined: a host's ipc name and the
//!   incarnation its mds made when it was offered the cluster -- 64 random
//!   bits, never 0, the member's raft node. A host whose mds lost its state
//!   comes back with a new incarnation, and is not the old member: Raft must
//!   never count the vote of a member that forgot its log. So a host is a
//!   member as one incarnation at a time, and another is refused until the
//!   old one is removed (`nbs host replace`).
//! - **The members removed**, kept so that an incarnation removed is never
//!   taken again, whatever an old copy of the list says.
//!
//! The policy the module keeps raft's configuration to (`voters`): the
//! first five members to join vote, the rest learn -- they hold everything
//! and do not vote.
//!
//! Applying is the same on every member: it reads the entry's index and
//! nothing else of the world -- no clock, no member's id, no randomness of
//! its own. Every byte string it is handed is read whole and every count
//! bounded: an entry that is no command is answered `Malformed`, a snapshot
//! that is no state's refused.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod command;

pub use command::{Answer, Command, Refusal};

use alloc::string::String;
use alloc::vec::Vec;

use codec::{Reader, Writer};
use raft::rsm::{Frozen, RestoreError, SnapshotReader, StateMachine};

/// Members a cluster takes at once: each is a host of the registry, every
/// one a peer of every other's ipcnet, which takes 16.
pub const MEMBERS_MAX: usize = 16;
/// Members that vote: the first to join.
pub const VOTERS_MAX: usize = 5;
/// Members removed the state keeps: past this, a remove is refused rather
/// than a removed incarnation forgotten.
pub const REMOVED_MAX: usize = 1024;
/// A cluster's name at most.
pub const NAME_MAX: usize = 63;
/// A snapshot's most bytes: every member and every removed at their most.
const SNAPSHOT_MAX: u64 = 64 * 1024 + (MEMBERS_MAX as u64 + REMOVED_MAX as u64) * 96;
const MAGIC: [u8; 4] = *b"NBM1";

/// Whether `name` is a cluster's or a host's name: a DNS label's letters,
/// as ipc's hosts have them.
pub fn valid_name(name: &str) -> bool {
    ipcwire::valid_host(name)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cluster {
    pub id: [u8; 16],
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The host's ipc name.
    pub host: String,
    /// Its incarnation: the member's raft node.
    pub id: u64,
    /// The entry it joined at.
    pub since: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removed {
    pub host: String,
    pub id: u64,
    /// The entry it was removed at.
    pub at: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    cluster: Option<Cluster>,
    members: Vec<Member>,
    removed: Vec<Removed>,
}

impl State {
    pub fn new() -> State {
        State::default()
    }

    pub fn cluster(&self) -> Option<&Cluster> {
        self.cluster.as_ref()
    }

    pub fn members(&self) -> &[Member] {
        &self.members
    }

    pub fn removed(&self) -> &[Removed] {
        &self.removed
    }

    /// Host `host`'s member.
    pub fn member(&self, host: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.host == host)
    }

    /// The member incarnation `id` is.
    pub fn member_of_id(&self, id: u64) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    /// Whether incarnation `id` was removed: never taken again.
    pub fn id_removed(&self, id: u64) -> bool {
        self.removed.iter().any(|r| r.id == id)
    }

    /// The members that vote: the first `VOTERS_MAX` to join.
    pub fn voters(&self) -> Vec<u64> {
        self.members.iter().take(VOTERS_MAX).map(|m| m.id).collect()
    }

    /// The members that learn: every one past the voters.
    pub fn learners(&self) -> Vec<u64> {
        self.members.iter().skip(VOTERS_MAX).map(|m| m.id).collect()
    }

    /// `command`, applied at entry `index`: what its proposer is answered.
    pub fn apply_command(&mut self, index: u64, command: &Command) -> Answer {
        match self.change(index, command) {
            Ok(member) => Answer::Done { member },
            Err(why) => Answer::Refused(why),
        }
    }

    fn change(&mut self, index: u64, command: &Command) -> Result<u64, Refusal> {
        match command {
            Command::Init { cluster, name, host, member } => {
                if self.cluster.is_some() {
                    return Err(Refusal::Initialized);
                }
                if !valid_name(name) || !valid_name(host) {
                    return Err(Refusal::BadName);
                }
                if *member == 0 {
                    return Err(Refusal::BadMember);
                }
                self.cluster = Some(Cluster { id: *cluster, name: name.clone() });
                self.members.push(Member { host: host.clone(), id: *member, since: index });
                Ok(*member)
            }
            Command::Add { host, member } => {
                if self.cluster.is_none() {
                    return Err(Refusal::NotInitialized);
                }
                if !valid_name(host) {
                    return Err(Refusal::BadName);
                }
                if *member == 0 {
                    return Err(Refusal::BadMember);
                }
                if let Some(m) = self.member(host) {
                    /* Asked again: the same incarnation is done; another one
                       is a host that lost its state, or an older offer's */
                    return if m.id == *member { Ok(m.id) } else { Err(Refusal::HostIsMember) };
                }
                if self.member_of_id(*member).is_some() {
                    return Err(Refusal::MemberInUse);
                }
                if self.id_removed(*member) {
                    return Err(Refusal::MemberRemoved);
                }
                if self.members.len() >= MEMBERS_MAX {
                    return Err(Refusal::Full);
                }
                self.members.push(Member { host: host.clone(), id: *member, since: index });
                Ok(*member)
            }
            Command::Remove { host } => {
                if self.cluster.is_none() {
                    return Err(Refusal::NotInitialized);
                }
                let Some(at) = self.members.iter().position(|m| m.host == *host) else {
                    /* Asked again once done: the incarnation it took out */
                    return self.removed.iter().rev().find(|r| r.host == *host).map(|r| r.id)
                        .ok_or(Refusal::NoSuchHost);
                };
                if self.members.len() == 1 {
                    return Err(Refusal::LastMember);
                }
                if self.removed.len() >= REMOVED_MAX {
                    return Err(Refusal::Full);
                }
                let m = self.members.remove(at);
                self.removed.push(Removed { host: m.host, id: m.id, at: index });
                Ok(m.id)
            }
        }
    }

    /// The state as a snapshot holds it.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.fixed(&MAGIC);
        match &self.cluster {
            Some(c) => {
                w.u8(1);
                w.fixed(&c.id);
                w.str(&c.name);
            }
            None => w.u8(0),
        }
        w.u8(self.members.len() as u8);
        for m in &self.members {
            w.str(&m.host);
            w.u64(m.id);
            w.u64(m.since);
        }
        w.u32(self.removed.len() as u32);
        for r in &self.removed {
            w.str(&r.host);
            w.u64(r.id);
            w.u64(r.at);
        }
        w.buf
    }

    /// The state `bytes` are, read whole and held to every rule: an error
    /// where they are no state's.
    pub fn decode(bytes: &[u8]) -> Result<State, RestoreError> {
        let bad = RestoreError("not NBS's metadata");
        let mut r = Reader::new(bytes);
        if r.fixed::<4>().ok_or(bad)? != MAGIC {
            return Err(bad);
        }
        let cluster = match r.u8().ok_or(bad)? {
            0 => None,
            1 => {
                let id = r.fixed().ok_or(bad)?;
                let name = String::from(r.str(NAME_MAX).ok_or(bad)?);
                Some(Cluster { id, name })
            }
            _ => return Err(bad),
        };
        let n = usize::from(r.u8().ok_or(bad)?);
        if n > MEMBERS_MAX {
            return Err(RestoreError("more members than a cluster takes"));
        }
        let mut members = Vec::with_capacity(n);
        for _ in 0..n {
            let host = String::from(r.str(ipcwire::HOST_MAX).ok_or(bad)?);
            let id = r.u64().ok_or(bad)?;
            let since = r.u64().ok_or(bad)?;
            members.push(Member { host, id, since });
        }
        let n = r.u32().ok_or(bad)? as usize;
        if n > REMOVED_MAX {
            return Err(RestoreError("more members removed than are kept"));
        }
        let mut removed = Vec::with_capacity(n);
        for _ in 0..n {
            let host = String::from(r.str(ipcwire::HOST_MAX).ok_or(bad)?);
            let id = r.u64().ok_or(bad)?;
            let at = r.u64().ok_or(bad)?;
            removed.push(Removed { host, id, at });
        }
        if !r.done() {
            return Err(bad);
        }
        let s = State { cluster, members, removed };
        s.check()?;
        Ok(s)
    }

    /// The rules a state read back is held to: what applying never makes,
    /// refused.
    fn check(&self) -> Result<(), RestoreError> {
        match &self.cluster {
            None if !self.members.is_empty() || !self.removed.is_empty() => {
                return Err(RestoreError("members of no cluster"));
            }
            None => return Ok(()),
            Some(c) if !valid_name(&c.name) => return Err(RestoreError("a cluster's name is none")),
            Some(_) if self.members.is_empty() => return Err(RestoreError("a cluster of no member")),
            Some(_) => {}
        }
        for (i, m) in self.members.iter().enumerate() {
            if !valid_name(&m.host) || m.id == 0 {
                return Err(RestoreError("a member of no name or of incarnation 0"));
            }
            if self.members[..i].iter().any(|o| o.host == m.host || o.id == m.id) {
                return Err(RestoreError("a host or an incarnation a member twice"));
            }
            if self.id_removed(m.id) {
                return Err(RestoreError("a member removed"));
            }
        }
        for r in &self.removed {
            if !valid_name(&r.host) || r.id == 0 {
                return Err(RestoreError("a member removed of no name or of incarnation 0"));
            }
        }
        Ok(())
    }
}

/// A state frozen: its bytes, made at the freeze, handed out a piece at a
/// time.
pub struct FrozenState {
    bytes: Vec<u8>,
    at: usize,
}

impl Frozen for FrozenState {
    fn write(&mut self, max: usize, out: &mut Vec<u8>) {
        let n = max.min(self.bytes.len() - self.at);
        out.extend_from_slice(&self.bytes[self.at..self.at + n]);
        self.at += n;
    }
}

impl StateMachine for State {
    type Frozen = FrozenState;

    fn apply(&mut self, index: u64, command: &[u8]) -> Vec<u8> {
        let answer = match Command::decode(command) {
            Some(c) => self.apply_command(index, &c),
            None => Answer::Refused(Refusal::Malformed),
        };
        answer.encode()
    }

    /// Any query: the state as it is, as a snapshot holds it.
    fn query(&self, _query: &[u8]) -> Vec<u8> {
        self.encode()
    }

    fn size(&self) -> u64 {
        (self.members.len() * 96 + self.removed.len() * 96 + 128) as u64
    }

    fn freeze(&self) -> FrozenState {
        FrozenState { bytes: self.encode(), at: 0 }
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>) -> Result<(), RestoreError> {
        let len = r.remaining();
        if len > SNAPSHOT_MAX {
            return Err(RestoreError("a snapshot larger than any NBS metadata"));
        }
        let mut bytes = alloc::vec![0u8; len as usize];
        if !r.read_exact(&mut bytes) {
            return Err(RestoreError("a snapshot cut short"));
        }
        *self = State::decode(&bytes)?;
        Ok(())
    }
}
