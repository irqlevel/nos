//! NBS's metadata (plans/nbs.md): the state every host's mds keeps through
//! raft's replicated state machine, and the commands that change it.
//!
//! [`State`] is raft's [`StateMachine`]: its log's entries are
//! [`Command`]s, which applying answers with an [`Answer`]. It holds the NBS
//! cluster's identity and its members (step N5), and what is stored and
//! where (N6, `storage.rs`): each host's chunk server and its devices, the
//! volumes, their chunks and the replica sets the chunks are in -- placed
//! by `place.rs`, the same on every member.
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
//!
//! **A determinism bug is a report, not a silent fork**: every
//! [`CHECK_EVERY`] entries a member takes its state's digest -- the SHA-256
//! of its snapshot's bytes, which are the state's in a fixed order -- and the
//! leader proposes its own (`Command::Digest`), which every member holds its
//! own to. A member whose differs says so ([`State::checks`]), and its module
//! stops it.
//!
//! The big tables are kept in pieces shared copy-on-write (`pieces.rs`): the
//! member's view, copied at every turn, and a snapshot being written cost a
//! handle a piece, and the snapshot is written and read back as a stream
//! (`snapshot.rs`) -- never in one buffer, where the kernel's heap gives no
//! block past 512 KiB.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod command;
mod pieces;
mod place;
pub mod repair;
mod snapshot;
mod storage;

pub use command::{Answer, Command, Refusal};
pub use pieces::{Pieces, PIECE};
pub use snapshot::{Checks, Diverged, FrozenState};
pub use storage::*;

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use codec::{Reader, Stream};
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
/// How many entries apart a member takes its state's digest, which the
/// leader proposes for every member to hold its own to.
pub const CHECK_EVERY: u64 = 64;

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

#[derive(Clone, Debug)]
pub struct State {
    cluster: Option<Cluster>,
    members: Vec<Member>,
    removed: Vec<Removed>,
    /* What is stored (storage.rs) */
    cs: Vec<Cs>,
    sets: Pieces<Set>,
    chunks: Pieces<Chunk>,
    volumes: Pieces<Volume>,
    /// The next device's number and the next volume's: never taken again.
    next_device: u32,
    next_volume: u32,
    /// The first entry from which the next digest is taken.
    next_check: u64,
    /// The free list's first chunk, `NO_CHUNK` for none.
    free_head: u32,
    /* Of the tables above, made again from them: never in a snapshot */
    /// The clean sets -- every member active, no joiner -- by their three
    /// cs's and number: where new chunks on those three go, the lowest first.
    set_of: BTreeSet<([u32; REPLICAS], u32)>,
    /// The free list's length.
    free_count: u32,
    /* This member's own: never in a snapshot, never compared */
    checks: Checks,
}

impl Default for State {
    fn default() -> State {
        State {
            cluster: None,
            members: Vec::new(),
            removed: Vec::new(),
            cs: Vec::new(),
            sets: Pieces::new(),
            chunks: Pieces::new(),
            volumes: Pieces::new(),
            next_device: 1,
            next_volume: 1,
            next_check: CHECK_EVERY,
            free_head: NO_CHUNK,
            set_of: BTreeSet::new(),
            free_count: 0,
            checks: Checks::default(),
        }
    }
}

/// Two states are the same where every table a snapshot holds is: what a
/// member keeps of its own -- its digests -- is not compared.
impl PartialEq for State {
    fn eq(&self, o: &State) -> bool {
        self.cluster == o.cluster && self.members == o.members && self.removed == o.removed && self.cs == o.cs
            && self.sets == o.sets && self.chunks == o.chunks && self.volumes == o.volumes
            && self.next_device == o.next_device && self.next_volume == o.next_volume
            && self.next_check == o.next_check && self.free_head == o.free_head
    }
}

impl Eq for State {}

impl State {
    pub fn new() -> State {
        State::default()
    }

    /// Every host's chunk server, by number.
    pub fn cs(&self) -> &[Cs] {
        &self.cs
    }

    pub fn sets(&self) -> &Pieces<Set> {
        &self.sets
    }

    pub fn chunks(&self) -> &Pieces<Chunk> {
        &self.chunks
    }

    pub fn volumes(&self) -> &Pieces<Volume> {
        &self.volumes
    }

    /// The chunks in use.
    pub fn live_chunks(&self) -> u64 {
        self.chunks.len() as u64 - u64::from(self.free_count)
    }

    /// The free list's first chunk, `NO_CHUNK` for none: the rest chained
    /// through each free chunk's `set`.
    pub fn free_head(&self) -> u32 {
        self.free_head
    }

    /// This member's digests: the newest, and whether the leader's ever
    /// differed.
    pub fn checks(&self) -> &Checks {
        &self.checks
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
        match command {
            Command::Init { .. } | Command::Add { .. } | Command::Remove { .. } => match self.change(index, command) {
                Ok(member) => Answer::Done { member },
                Err(why) => Answer::Refused(why),
            },
            other => self.apply_storage(index, other),
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
            /* What is stored: storage.rs's */
            _ => Err(Refusal::Malformed),
        }
    }

    /// The state as a snapshot holds it, in one buffer: for a host's tests --
    /// the member writes it a piece at a time (`freeze`).
    pub fn encode(&self) -> Vec<u8> {
        let mut f = self.freeze();
        let mut out = Vec::new();
        loop {
            let before = out.len();
            f.write(64 * 1024, &mut out);
            if out.len() == before {
                return out;
            }
        }
    }

    /// The state `bytes` are, read whole and held to every rule: an error
    /// where they are no state's.
    pub fn decode(bytes: &[u8]) -> Result<State, RestoreError> {
        let mut r = Stream::new(Reader::new(bytes));
        let s = State::read(&mut r)?;
        if !r.into_inner().done() {
            return Err(RestoreError("not NBS's metadata"));
        }
        Ok(s)
    }

    /// A state read off `r` and held to every rule, the tables made again
    /// from it made.
    fn read<S: codec::Source>(r: &mut Stream<S>) -> Result<State, RestoreError> {
        let mut s = snapshot::read(r)?;
        s.free_count = s.chunks.iter().filter(|c| c.is_free()).count() as u32;
        for id in 0..s.sets.len() as u32 {
            s.index(id);
        }
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
            None => return self.check_storage(),
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
        self.check_storage()
    }
}

impl StateMachine for State {
    type Frozen = FrozenState;

    /// A command applied -- and at every `CHECK_EVERY` entries, the state's
    /// digest taken, for the leader's to be held to.
    fn apply(&mut self, index: u64, command: &[u8]) -> Vec<u8> {
        let answer = match Command::decode(command) {
            Some(c) => self.apply_command(index, &c),
            None => Answer::Refused(Refusal::Malformed),
        };
        if index >= self.next_check {
            self.next_check = index.saturating_add(CHECK_EVERY);
            let digest = self.digest();
            self.checks.push(index, digest);
        }
        answer.encode()
    }

    /// No reads go through raft: a member's shell and provider read its own
    /// view, at the index it has applied.
    fn query(&self, _query: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    /// About its snapshot's bytes: the tables at the size a record of each
    /// takes there.
    fn size(&self) -> u64 {
        let maps: u64 = self.volumes.iter().map(|v| v.map.len() as u64 * 4 + 160).sum();
        let cs: u64 = self.cs.iter().map(|c| 80 + c.devices.len() as u64 * 80).sum();
        128 + (self.members.len() + self.removed.len()) as u64 * 96 + cs + self.sets.len() as u64 * 45
            + self.chunks.len() as u64 * 11 + maps
    }

    fn freeze(&self) -> FrozenState {
        FrozenState::new(self.clone())
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>) -> Result<(), RestoreError> {
        let mut stream = Stream::new(snapshot::FromSnapshot(r));
        let s = State::read(&mut stream)?;
        if stream.into_inner().0.remaining() != 0 {
            return Err(RestoreError("a snapshot longer than its state"));
        }
        *self = s;
        Ok(())
    }
}
