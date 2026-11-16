//! A group's members, as a group whose hosts are offered it keeps them:
//! NBS's metadata (plans/nbs.md, "Who is in the cluster") and vnet's state
//! (plans/vnet.md). Written once, here, for every such group:
//!
//! - **the group**: its id and its name, made by its first host's `init`;
//! - **its members**, in the order they came, each a host of the cluster
//!   registry and the incarnation its member made as the host was offered
//!   the group -- 64 random bits, never made again, the member's raft node.
//!   A host whose state was lost comes back offered the group as a new
//!   incarnation, never the one it was: raft never counts the vote of a
//!   member that forgot its log;
//! - **the members removed**, kept so that an incarnation removed is never
//!   taken again, whatever an old copy of the list says;
//! - **who votes**: the first `VOTERS_MAX` to join, the rest learn -- the
//!   registry's hosts too, which join by invitation and keep a table of
//!   their own, vote by this rule ([`voters`], [`learners`]).
//!
//! The commands that change them -- `Init`, `Add`, `Remove` -- are each
//! group's own on its wire and applied here ([`Members::init`],
//! [`Members::add`], [`Members::remove`]); a state read back is held to the
//! rules applying keeps ([`Members::check`]). No clock, no member's id and no
//! randomness is read here: applying is the same on every member.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Members a group takes at once: each is a host of the registry, every one
/// a peer of every other's ipcnet, which takes 16.
pub const MEMBERS_MAX: usize = 16;
/// Members that vote: the first to join.
pub const VOTERS_MAX: usize = 5;
/// Members removed a group keeps: past this, a remove is refused rather
/// than a removed incarnation forgotten.
pub const REMOVED_MAX: usize = 1024;
/// A group's name at most.
pub const NAME_MAX: usize = 63;

/// Whether `name` is a group's or a host's name: a DNS label's letters, as
/// ipc's hosts have them.
pub fn valid_name(name: &str) -> bool {
    ipcwire::valid_host(name)
}

/// Of the members in the order they joined, by node, those that vote.
pub fn voters(order: impl Iterator<Item = u64>) -> Vec<u64> {
    order.take(VOTERS_MAX).collect()
}

/// Of the members in the order they joined, by node, those that learn:
/// every one past the voters.
pub fn learners(order: impl Iterator<Item = u64>) -> Vec<u64> {
    order.skip(VOTERS_MAX).collect()
}

/// The group: made once, by its first host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
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

/// Why a change of the members was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `Init` of a group made already.
    Initialized,
    /// Anything else before the group is made.
    NotInitialized,
    BadName,
    /// Incarnation 0, which is none.
    BadMember,
    /// The host is a member as another incarnation.
    HostIsMember,
    /// Another host's member is that incarnation.
    MemberInUse,
    /// That incarnation was removed: never taken again.
    MemberRemoved,
    /// As many members as a group takes, or as many removed as it keeps.
    Full,
    NoSuchHost,
    /// The group's last member.
    LastMember,
}

/// A group's members: what its log's `Init`, `Add` and `Remove` make.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Members {
    group: Option<Group>,
    /// In the order they came.
    members: Vec<Member>,
    /// In the order they were removed.
    removed: Vec<Removed>,
}

impl Members {
    pub fn new() -> Members {
        Members::default()
    }

    /// The tables as a snapshot held them, every count bounded by its
    /// reader: not yet held to the rules (`check`).
    pub fn from_parts(group: Option<Group>, members: Vec<Member>, removed: Vec<Removed>) -> Members {
        Members { group, members, removed }
    }

    pub fn group(&self) -> Option<&Group> {
        self.group.as_ref()
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
        voters(self.members.iter().map(|m| m.id))
    }

    /// The members that learn: every one past the voters.
    pub fn learners(&self) -> Vec<u64> {
        learners(self.members.iter().map(|m| m.id))
    }

    /// About the bytes a snapshot takes of them, each record at its most.
    pub fn size(&self) -> u64 {
        (self.members.len() + self.removed.len()) as u64 * 96
    }

    /// The group made, `host` its first member as incarnation `member`, at
    /// entry `index`: the incarnation.
    pub fn init(&mut self, index: u64, id: [u8; 16], name: &str, host: &str, member: u64) -> Result<u64, Refusal> {
        if self.group.is_some() {
            return Err(Refusal::Initialized);
        }
        if !valid_name(name) || !valid_name(host) {
            return Err(Refusal::BadName);
        }
        if member == 0 {
            return Err(Refusal::BadMember);
        }
        self.group = Some(Group { id, name: String::from(name) });
        self.members.push(Member { host: String::from(host), id: member, since: index });
        Ok(member)
    }

    /// `host` a member as incarnation `member`, at entry `index`: the
    /// incarnation. Asked again, the same incarnation is done.
    pub fn add(&mut self, index: u64, host: &str, member: u64) -> Result<u64, Refusal> {
        if self.group.is_none() {
            return Err(Refusal::NotInitialized);
        }
        if !valid_name(host) {
            return Err(Refusal::BadName);
        }
        if member == 0 {
            return Err(Refusal::BadMember);
        }
        if let Some(m) = self.member(host) {
            /* Asked again: the same incarnation is done; another one is a
               host that lost its state, or an older offer's */
            return if m.id == member { Ok(m.id) } else { Err(Refusal::HostIsMember) };
        }
        if self.member_of_id(member).is_some() {
            return Err(Refusal::MemberInUse);
        }
        if self.id_removed(member) {
            return Err(Refusal::MemberRemoved);
        }
        if self.members.len() >= MEMBERS_MAX {
            return Err(Refusal::Full);
        }
        self.members.push(Member { host: String::from(host), id: member, since: index });
        Ok(member)
    }

    /// `host`'s member out, for good, at entry `index`: the incarnation it
    /// took out. Asked again once done, the same.
    pub fn remove(&mut self, index: u64, host: &str) -> Result<u64, Refusal> {
        if self.group.is_none() {
            return Err(Refusal::NotInitialized);
        }
        let Some(at) = self.members.iter().position(|m| m.host == host) else {
            /* Asked again once done: the incarnation it took out */
            return self.removed.iter().rev().find(|r| r.host == host).map(|r| r.id).ok_or(Refusal::NoSuchHost);
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

    /// The rules a state read back is held to -- what applying never makes,
    /// refused, and said.
    pub fn check(&self) -> Result<(), &'static str> {
        match &self.group {
            None if !self.members.is_empty() || !self.removed.is_empty() => return Err("members of no cluster"),
            None => return Ok(()),
            Some(g) if !valid_name(&g.name) => return Err("a cluster's name is none"),
            Some(_) if self.members.is_empty() => return Err("a cluster of no member"),
            Some(_) => {}
        }
        if self.members.len() > MEMBERS_MAX || self.removed.len() > REMOVED_MAX {
            return Err("more members, or members removed, than are kept");
        }
        for (i, m) in self.members.iter().enumerate() {
            if !valid_name(&m.host) || m.id == 0 {
                return Err("a member of no name or of incarnation 0");
            }
            if self.members[..i].iter().any(|o| o.host == m.host || o.id == m.id) {
                return Err("a host or an incarnation a member twice");
            }
            if self.id_removed(m.id) {
                return Err("a member removed");
            }
        }
        for r in &self.removed {
            if !valid_name(&r.host) || r.id == 0 {
                return Err("a member removed of no name or of incarnation 0");
            }
        }
        Ok(())
    }
}
