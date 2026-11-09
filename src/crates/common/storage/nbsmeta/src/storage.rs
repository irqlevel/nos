//! What NBS stores and where (plans/nbs.md, "What is stored"): each host's
//! chunk server and its devices, the replica sets, the chunks and the
//! volumes -- and the commands that change them (N6). Placement is
//! `place.rs`'s.
//!
//! - **A cs** is a host's, one a host, kept by the host's name: a member
//!   replaced keeps its cs, whose data is on its devices, not in the mds.
//!   Its room is its devices' slots that take new data; what its replicas
//!   take is counted as reserved, never as used (rule 5 of "Where replicas
//!   go"): one slot for every chunk whose set it is in.
//! - **A set** is three cs's on three hosts, each member Active or Down, at
//!   most one joiner -- a cs being copied onto, which takes a member's place
//!   once its copy is done (N9) -- and a version. Chunks placed on the same
//!   three cs's name the same set, so a host lost changes the few sets it is
//!   in rather than every chunk. A set and its version are a chunk's epoch.
//!   A join can leave two sets on the same three cs's; placement takes the
//!   lowest-numbered of them.
//! - **A chunk** -- 4 MiB of a volume -- is 8 bytes: its set, how many maps
//!   name it, its flags. A free one is on the free list, chained through
//!   its own record, so no other table keeps it.
//! - **A volume**: its name and size, its kind, its own host, its attach,
//!   and its map, a chunk's number for each 4 MiB of it.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use crate::pieces::Pieces;
use crate::place::Placer;
use crate::{Answer, Command, Refusal, State};

/// Replicas a chunk has: three hosts' (decision 7 of plans/nbs.md has it so).
pub const REPLICAS: usize = 3;
/// A chunk's size, fixed when the cluster is made (decision 5).
pub const CHUNK_MIB: u32 = 4;
/// Chunk servers the metadata keeps: one a host, those of hosts removed
/// included.
pub const CS_MAX: usize = 64;
/// Devices a cs has, at most.
pub const DEVICES_MAX: usize = 16;
/// Volumes, at most.
pub const VOLUMES_MAX: usize = 4096;
/// Chunks, at most: 16 TiB of volumes, some 48 MiB of state.
pub const CHUNKS_MAX: usize = 1 << 22;
/// A volume's size at most, in MiB: 16 TiB.
pub const VOLUME_MIB_MAX: u64 = 16 << 20;
/// A device's slots at most: a PiB in 4 MiB.
pub const SLOTS_MAX: u32 = 1 << 28;
/// Readers a volume has at once, at most: a host each.
pub const READERS_MAX: usize = crate::MEMBERS_MAX;
/// A device's id at most: `part:` and a GUID, `disk:` and a serial.
pub const IDENT_MAX: usize = 64;
/// A volume's name at most.
pub const VOLUME_NAME_MAX: usize = 63;
/// Sets, at most: every three of `CS_MAX` cs's is fewer.
pub const SETS_MAX: usize = 1 << 16;
/// What a free chunk's record has for the end of the free list.
pub const NO_CHUNK: u32 = u32::MAX;

/// A chunk on the free list.
pub const CHUNK_FREE: u8 = 0x80;
/// A chunk written to at least once (the cs's say so, from N7 on).
pub const CHUNK_WRITTEN: u8 = 0x01;

/// The chunks a volume of `size_mib` MiB has.
pub fn chunks_for(size_mib: u64) -> u64 {
    size_mib.div_ceil(u64::from(CHUNK_MIB))
}

/// Whether `name` is a volume's: lowercase letters, digits, `.`, `_` and
/// `-`, a letter or digit first, up to 63 of them.
pub fn valid_volume(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= VOLUME_NAME_MAX && b[0].is_ascii_alphanumeric()
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
}

/// Whether `ident` is a device's id as nos has one: `part:...` or
/// `disk:...`, letters, digits and `.`, `_`, `-`, `:`, up to 64 of them.
pub fn valid_ident(ident: &str) -> bool {
    (ident.starts_with("part:") || ident.starts_with("disk:")) && ident.len() > 5 && ident.len() <= IDENT_MAX
        && ident.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b':'))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceState {
    /// Takes new chunks.
    Active,
    /// Takes no new chunks; what it has is to move (N9).
    Draining,
    /// Lost: what it had is to be rebuilt (N9).
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: u32,
    pub ident: String,
    pub slots: u32,
    pub state: DeviceState,
}

/// What the leader has committed of a cs's being there (N9): what the
/// controller (`repair.rs`) moves it by, from what the leader sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsState {
    /// Answering: a member of its sets, taking new chunks where it has room.
    Up,
    /// Silent past the controller's 5 s: down in every set it is a member of
    /// -- the last active member of one apart -- and the joiner of none; it
    /// takes no new chunks, and is back in a set only by joining it again
    /// once it answers.
    Down,
    /// Silent past `out_after`, or its host no member: down in its sets as
    /// a down one is, its places in them to be taken by others.
    Out,
}

/// A host's chunk server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cs {
    /// Its number: its place in the table, which only grows.
    pub id: u32,
    pub host: String,
    pub devices: Vec<Device>,
    /// The slots its replicas take: one for every chunk whose set it is a
    /// member of, and for every chunk of a set it joins in another's place.
    pub reserved: u64,
    pub state: CsState,
}

impl Cs {
    /// Its room: the slots of its devices that take new chunks.
    pub fn slots(&self) -> u64 {
        self.devices.iter().filter(|d| d.state == DeviceState::Active).map(|d| u64::from(d.slots)).sum()
    }

    /// What of its room no replica has.
    pub fn free(&self) -> u64 {
        self.slots().saturating_sub(self.reserved)
    }

    /// Being drained: devices, none of which takes new chunks -- every set
    /// it is a member of to be given a joiner in its place.
    pub fn draining(&self) -> bool {
        !self.devices.is_empty() && self.slots() == 0
    }
}

/// A member of a set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replica {
    /// Written and read.
    Active,
    /// Taken out: written and read by nobody until it joins again, or
    /// another joins in its place (N9).
    Down,
}

/// A cs being copied onto (N9): written, not read, until its copy is done
/// and it takes a member's place -- the member's own, when the member
/// itself is back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Joiner {
    pub cs: u32,
    /// The member whose place it takes: its index in the set's three.
    pub place: u8,
    /// The version the join raised the set to: what its end names it by.
    pub since: u32,
}

/// Three cs's on three hosts, by number, in increasing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Set {
    pub cs: [u32; REPLICAS],
    pub replicas: [Replica; REPLICAS],
    /// For each member down, the last version it was active at: the writes
    /// it may have missed were made at that version or after. 0 for an
    /// active one.
    pub active_until: [u32; REPLICAS],
    pub joiner: Option<Joiner>,
    /// Raised by every change of its members: a chunk's epoch is its set and
    /// that.
    pub version: u32,
    /// The chunks in it.
    pub chunks: u32,
}

impl Set {
    /// How many of its members are active.
    pub fn active(&self) -> usize {
        self.replicas.iter().filter(|&&r| r == Replica::Active).count()
    }

    /// Whether `cs` is the joiner of a place another's is.
    pub fn joins_elsewhere(&self, cs: u32) -> bool {
        self.joiner.is_some_and(|j| j.cs == cs && self.cs.get(usize::from(j.place)) != Some(&cs))
    }
}

/// A chunk: in use, or on the free list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Chunk {
    /// Its set; on the free list, the next free chunk (`NO_CHUNK` at the end).
    pub set: u32,
    /// How many maps name it: a volume's, and from N10 its snapshots' and
    /// clones'.
    pub count: u16,
    pub flags: u8,
    /// Which of the chunks its number has been this is: 1 for the first, one
    /// more each time the number is taken off the free list again, kept while
    /// it is free. A cs holds a chunk by its number and incarnation, so that
    /// a slot of the chunk the number was before -- one a cs that missed its
    /// free still has -- is never taken for the chunk it is now.
    pub incarnation: u32,
}

impl Chunk {
    pub fn is_free(&self) -> bool {
        self.flags & CHUNK_FREE != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Volume,
    /// A volume frozen read-only (N10).
    Image,
    /// A volume's map at a moment (N10).
    Snapshot,
}

/// A volume's writer: the host, the generation it attached at, and the
/// token of the client's session that attached it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attach {
    pub host: String,
    pub generation: u64,
    pub token: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// Its number, never taken again.
    pub id: u32,
    pub name: String,
    pub size_mib: u64,
    pub kind: Kind,
    /// The image or snapshot it was cloned from: 0 for none (N10).
    pub origin: u32,
    /// Its own host: where it was made, where one replica of each chunk goes
    /// that has room, and where it is attached from.
    pub host: String,
    /// The newest generation of a writer's: what the cs's fence an older
    /// one by.
    pub generation: u64,
    pub writer: Option<Attach>,
    /// The hosts that have it read-only.
    pub readers: Vec<String>,
    /// A chunk's number for each 4 MiB of it.
    pub map: Pieces<u32>,
}

impl State {
    /// A command of what is stored (N6), applied.
    pub(crate) fn apply_storage(&mut self, index: u64, command: &Command) -> Answer {
        let done = match command {
            Command::DeviceAdd { host, ident, slots } => self.device_add(host, ident, *slots).map(|id| Answer::Device { id }),
            Command::DeviceDrain { host, ident } => self.device_drain(host, ident).map(|id| Answer::Device { id }),
            Command::VolumeCreate { name, size_mib, host } => {
                self.volume_create(name, *size_mib, host).map(|id| Answer::Volume { id })
            }
            Command::VolumeDelete { name } => self.volume_delete(name).map(|id| Answer::Volume { id }),
            Command::VolumeResize { name, size_mib } => {
                self.volume_resize(name, *size_mib).map(|id| Answer::Volume { id })
            }
            Command::Attach { name, host, writable, force, token } => {
                self.attach(name, host, *writable, *force, *token).map(|generation| Answer::Attached { generation })
            }
            Command::Detach { name, host, generation } => {
                self.detach(name, host, *generation).map(|generation| Answer::Attached { generation })
            }
            Command::Digest { at, digest } => {
                self.checks.compare(*at, digest, index);
                Ok(Answer::Checked)
            }
            Command::ReplicaDown { set, cs, name, generation, .. } => {
                self.replica_down(*set, *cs, name, *generation).map(|version| Answer::Set { version })
            }
            Command::CsDown { cs } => self.cs_gone(*cs, CsState::Down).map(|sets| Answer::Changed { sets }),
            Command::CsOut { cs } => self.cs_gone(*cs, CsState::Out).map(|sets| Answer::Changed { sets }),
            Command::CsUp { cs } => self.cs_up(*cs).map(|sets| Answer::Changed { sets }),
            Command::Join { set, place, cs, version } => {
                self.join(*set, *place, *cs, *version).map(|version| Answer::Set { version })
            }
            Command::Joined { set, cs, since } => self.joined(*set, *cs, *since).map(|version| Answer::Set { version }),
            Command::Unjoin { set, cs, since } => self.unjoin(*set, *cs, *since).map(|version| Answer::Set { version }),
            Command::ReplicaBad { set, cs, version } => {
                self.replica_bad(*set, *cs, *version).map(|version| Answer::Set { version })
            }
            /* The members' are lib.rs's */
            Command::Init { .. } | Command::Add { .. } | Command::Remove { .. } => Err(Refusal::Malformed),
        };
        done.unwrap_or_else(Answer::Refused)
    }

    fn ready(&self) -> Result<(), Refusal> {
        if self.cluster.is_none() {
            return Err(Refusal::NotInitialized);
        }
        Ok(())
    }

    fn member_host(&self, host: &str) -> Result<(), Refusal> {
        if self.member(host).is_none() {
            return Err(Refusal::NotAMember);
        }
        Ok(())
    }

    /// The cs of host `host`, by its place in the table.
    pub fn cs_of(&self, host: &str) -> Option<&Cs> {
        self.cs.iter().find(|c| c.host == host)
    }

    /// The cs that has device `ident`, and the device.
    pub fn device(&self, ident: &str) -> Option<(&Cs, &Device)> {
        self.cs.iter().find_map(|c| c.devices.iter().find(|d| d.ident == ident).map(|d| (c, d)))
    }

    /// Volume `name`, and where it is in the table.
    pub fn volume(&self, name: &str) -> Option<&Volume> {
        self.volumes.iter().find(|v| v.name == name)
    }

    fn volume_at(&self, name: &str) -> Option<usize> {
        self.volumes.iter().position(|v| v.name == name)
    }

    fn device_add(&mut self, host: &str, ident: &str, slots: u32) -> Result<u32, Refusal> {
        self.ready()?;
        self.member_host(host)?;
        if !valid_ident(ident) || slots == 0 || slots > SLOTS_MAX {
            return Err(Refusal::BadDevice);
        }
        if let Some((cs, d)) = self.device(ident) {
            return match (cs.host == host, d.slots == slots) {
                (true, true) => Ok(d.id),
                (true, false) => Err(Refusal::DeviceChanged),
                (false, _) => Err(Refusal::DeviceElsewhere),
            };
        }
        let at = match self.cs.iter().position(|c| c.host == host) {
            Some(at) => at,
            None => {
                if self.cs.len() >= CS_MAX {
                    return Err(Refusal::TooMany);
                }
                let id = self.cs.len() as u32;
                self.cs.push(Cs { id, host: String::from(host), devices: Vec::new(), reserved: 0, state: CsState::Up });
                self.cs.len() - 1
            }
        };
        if self.cs[at].devices.len() >= DEVICES_MAX {
            return Err(Refusal::TooMany);
        }
        let id = self.next_device;
        self.next_device += 1;
        self.cs[at].devices.push(Device { id, ident: String::from(ident), slots, state: DeviceState::Active });
        Ok(id)
    }

    fn device_drain(&mut self, host: &str, ident: &str) -> Result<u32, Refusal> {
        self.ready()?;
        let Some(cs) = self.cs.iter_mut().find(|c| c.host == host) else { return Err(Refusal::NoSuchDevice) };
        let Some(d) = cs.devices.iter_mut().find(|d| d.ident == ident) else { return Err(Refusal::NoSuchDevice) };
        if d.state == DeviceState::Active {
            d.state = DeviceState::Draining;
        }
        Ok(d.id)
    }

    fn volume_create(&mut self, name: &str, size_mib: u64, host: &str) -> Result<u32, Refusal> {
        self.ready()?;
        if !valid_volume(name) || size_mib == 0 || size_mib > VOLUME_MIB_MAX {
            return Err(Refusal::BadVolume);
        }
        self.member_host(host)?;
        if let Some(v) = self.volume(name) {
            /* Made again with the same size: answered as it is */
            return if v.kind == Kind::Volume && v.size_mib == size_mib { Ok(v.id) } else { Err(Refusal::NameTaken) };
        }
        if self.volumes.len() >= VOLUMES_MAX {
            return Err(Refusal::TooMany);
        }
        let id = self.next_volume;
        let n = chunks_for(size_mib);
        let mut map = Pieces::new();
        self.place_chunks(id, host, 0, n, &mut map)?;
        self.next_volume += 1;
        self.volumes.push(Volume {
            id,
            name: String::from(name),
            size_mib,
            kind: Kind::Volume,
            origin: 0,
            host: String::from(host),
            generation: 0,
            writer: None,
            readers: Vec::new(),
            map,
        });
        Ok(id)
    }

    fn volume_resize(&mut self, name: &str, size_mib: u64) -> Result<u32, Refusal> {
        self.ready()?;
        if size_mib == 0 || size_mib > VOLUME_MIB_MAX {
            return Err(Refusal::BadVolume);
        }
        let Some(at) = self.volume_at(name) else { return Err(Refusal::NoSuchVolume) };
        let (id, host, old, kind, had) = {
            let v = &self.volumes.get(at).ok_or(Refusal::NoSuchVolume)?;
            (v.id, v.host.clone(), v.size_mib, v.kind, v.map.len() as u64)
        };
        if kind != Kind::Volume {
            return Err(Refusal::ReadOnly);
        }
        if size_mib < old {
            return Err(Refusal::Shrink);
        }
        let want = chunks_for(size_mib);
        if want > had {
            let mut map = core::mem::take(&mut self.volumes.get_mut(at).ok_or(Refusal::NoSuchVolume)?.map);
            let placed = self.place_chunks(id, &host, had, want - had, &mut map);
            if let Some(v) = self.volumes.get_mut(at) {
                v.map = map;
            }
            placed?;
        }
        if let Some(v) = self.volumes.get_mut(at) {
            v.size_mib = size_mib;
        }
        Ok(id)
    }

    fn volume_delete(&mut self, name: &str) -> Result<u32, Refusal> {
        self.ready()?;
        let Some(at) = self.volume_at(name) else { return Ok(0) };
        {
            let v = self.volumes.get(at).ok_or(Refusal::NoSuchVolume)?;
            if v.writer.is_some() || !v.readers.is_empty() {
                return Err(Refusal::Attached);
            }
        }
        let v = self.volumes.swap_remove(at).ok_or(Refusal::NoSuchVolume)?;
        for &chunk in v.map.iter() {
            self.release_chunk(chunk);
        }
        Ok(v.id)
    }

    fn attach(&mut self, name: &str, host: &str, writable: bool, force: bool, token: u64) -> Result<u64, Refusal> {
        self.ready()?;
        self.member_host(host)?;
        let Some(at) = self.volume_at(name) else { return Err(Refusal::NoSuchVolume) };
        let v = self.volumes.get_mut(at).ok_or(Refusal::NoSuchVolume)?;
        if writable {
            if v.kind != Kind::Volume {
                return Err(Refusal::ReadOnly);
            }
            /* The writer's own attach asked again: answered as it was */
            if let Some(w) = v.writer.as_ref().filter(|w| token != 0 && w.token == token && w.host == host) {
                return Ok(w.generation);
            }
            if !force {
                if v.writer.as_ref().is_some_and(|w| w.host != host) {
                    return Err(Refusal::AttachedElsewhere);
                }
                if !v.readers.is_empty() {
                    return Err(Refusal::ReadersAttached);
                }
            }
            /* A new generation for every attach for writing, the same host's
               again included: what its own older one still has in flight is
               fenced by it too */
            v.readers.clear();
            v.generation += 1;
            v.writer = Some(Attach { host: String::from(host), generation: v.generation, token });
            return Ok(v.generation);
        }
        if v.writer.is_some() {
            return Err(Refusal::Writing);
        }
        if !v.readers.iter().any(|r| r == host) {
            if v.readers.len() >= READERS_MAX {
                return Err(Refusal::TooMany);
            }
            v.readers.push(String::from(host));
        }
        Ok(0)
    }

    fn detach(&mut self, name: &str, host: &str, generation: u64) -> Result<u64, Refusal> {
        self.ready()?;
        let Some(at) = self.volume_at(name) else { return Ok(0) };
        let v = self.volumes.get_mut(at).ok_or(Refusal::NoSuchVolume)?;
        if generation == 0 {
            v.readers.retain(|r| r != host);
            return Ok(0);
        }
        if v.writer.as_ref().is_some_and(|w| w.host == host && w.generation == generation) {
            v.writer = None;
            return Ok(generation);
        }
        Ok(0)
    }

    /// `n` new chunks for volume `volume`, its `from`th on, placed and
    /// pushed onto `map` -- or none at all, and why: the placement is found
    /// whole on a copy of the room first, then made the same way again, so
    /// that a volume that does not fit takes nothing and no list of where
    /// every chunk goes is ever held at once.
    fn place_chunks(&mut self, volume: u32, host: &str, from: u64, n: u64, map: &mut Pieces<u32>)
        -> Result<(), Refusal> {
        if n == 0 {
            return Ok(());
        }
        let live = self.chunks.len() as u64 - u64::from(self.free_count);
        if live + n > CHUNKS_MAX as u64 {
            return Err(Refusal::TooMany);
        }
        let placer = Placer::new(self, host);
        if placer.eligible() < REPLICAS {
            return Err(Refusal::TooFewHosts);
        }
        let mut trial = placer.clone();
        /* Every three cs's the picks take */
        let mut used: BTreeSet<[u32; REPLICAS]> = BTreeSet::new();
        for k in 0..n {
            used.insert(trial.pick(volume, from + k).ok_or(Refusal::NoRoom)?);
        }
        if !trial.leaves_rebuild_room() {
            return Err(Refusal::NoRoomToRebuild);
        }
        /* Those of no clean set take a set each, and only those: a set made
           again for them is none of the others' clean sets (`spare`), and
           is clean for its own from then on */
        let fresh = used.iter().filter(|&&cs| self.clean_set(cs).is_none()).count();
        if fresh > self.sets_to_spare(&used) {
            return Err(Refusal::TooMany);
        }
        let mut real = placer;
        for k in 0..n {
            /* The same picks as the trial's, which found room for every one,
               and a set for each new three */
            let Some(cs) = real.pick(volume, from + k) else { unreachable!("a placement the trial made") };
            let Some(set) = self.set_for(cs, &used) else { unreachable!("a set the trial counted") };
            map.push(self.new_chunk(set));
        }
        Ok(())
    }

    /// Sets a placement onto the three cs's of `used` can have for those of
    /// them with no clean set: the table's room left, and the empty sets it
    /// may make again.
    fn sets_to_spare(&self, used: &BTreeSet<[u32; REPLICAS]>) -> usize {
        SETS_MAX.saturating_sub(self.sets.len()) + self.sets.iter().filter(|s| State::spare(s, used)).count()
    }

    /// Whether set `s` may be made again on other cs's by a placement onto
    /// those of `used`: empty, no joiner, a version that can move on, and
    /// not the clean set of three cs's the placement fills -- which, made
    /// again for others, would leave those three to take a set of their own
    /// that no count had for them.
    fn spare(s: &Set, used: &BTreeSet<[u32; REPLICAS]>) -> bool {
        s.chunks == 0 && s.joiner.is_none() && s.version < u32::MAX && !(State::is_clean(s) && used.contains(&s.cs))
    }

    /// Member `cs` of set `set` taken down as a person finds its copy bad
    /// -- the scrub's finding (N9e) -- at `version`: down as a replica taken
    /// out is, the set's version now. A set with a joiner is the copy's, a
    /// member down already joins its place again, and the last active
    /// member is never taken down.
    fn replica_bad(&mut self, set: u32, cs: u32, version: u32) -> Result<u32, Refusal> {
        let s = self.set_at(set, version)?;
        let at = s.cs.iter().position(|&c| c == cs).ok_or(Refusal::NoSuchSet)?;
        if s.replicas[at] == Replica::Down {
            return Err(Refusal::AlreadyDown);
        }
        self.member_down(set, at)
    }

    /// Set `set` as a change planned at its `version` finds it: refused as
    /// stale once the set has moved on, and while a join of it is under way.
    fn set_at(&self, set: u32, version: u32) -> Result<&Set, Refusal> {
        self.ready()?;
        let s = self.sets.get(set as usize).ok_or(Refusal::NoSuchSet)?;
        if s.version != version {
            return Err(Refusal::Stale);
        }
        if s.joiner.is_some() {
            return Err(Refusal::Joining);
        }
        Ok(s)
    }

    /// Member `at` of set `set` down -- never the set's last active member:
    /// the version it was active until kept, and the set's raised by one,
    /// what it is now answered.
    fn member_down(&mut self, set: u32, at: usize) -> Result<u32, Refusal> {
        let s = self.sets.get(set as usize).ok_or(Refusal::NoSuchSet)?;
        if s.active() <= 1 {
            return Err(Refusal::LastReplica);
        }
        let version = s.version.checked_add(1).ok_or(Refusal::TooMany)?;
        self.unindex(set);
        if let Some(s) = self.sets.get_mut(set as usize) {
            s.replicas[at] = Replica::Down;
            s.active_until[at] = s.version;
            s.version = version;
        }
        Ok(version)
    }

    /// Replica `cs` of set `set` taken out: its version now. The set keeps
    /// it among its three, for repair to bring back or replace (N9), and
    /// the room it holds; never the last active one.
    fn replica_down(&mut self, set: u32, cs: u32, name: &str, generation: u64) -> Result<u32, Refusal> {
        self.ready()?;
        let v = self.volume(name).ok_or(Refusal::NoSuchVolume)?;
        /* Asked by the volume's writer now: one fenced since would have its
           write done by taking out the replicas that refuse it (found by
           fuzz/nbs's cluster target) */
        if v.writer.as_ref().is_none_or(|w| w.generation != generation) {
            return Err(Refusal::NotTheWriter);
        }
        if !v.map.iter().any(|&c| self.chunks.get(c as usize).is_some_and(|ch| ch.set == set)) {
            return Err(Refusal::NoSuchSet);
        }
        let s = self.sets.get(set as usize).ok_or(Refusal::NoSuchSet)?;
        /* The joiner silent to the writer: its join dropped, as its going
           down would drop it -- the writer's writes go on to the members */
        if let Some(j) = s.joiner.filter(|j| j.cs == cs) {
            return self.unjoin(set, cs, j.since);
        }
        let at = s.cs.iter().position(|&c| c == cs).ok_or(Refusal::NoSuchSet)?;
        if s.replicas[at] == Replica::Down {
            return Ok(s.version);
        }
        self.member_down(set, at)
    }

    /// cs `cs` down, or out (`to`): down in every set it is an active member
    /// of but the last -- the version it was active at kept -- and dropped
    /// as the joiner of any, its room for that given back; given no new
    /// chunks. From up only: down to out changes no set, and out again or
    /// down again nothing. How many sets changed.
    fn cs_gone(&mut self, cs: u32, to: CsState) -> Result<u32, Refusal> {
        self.ready()?;
        let was = self.cs.get(cs as usize).ok_or(Refusal::NoSuchCs)?.state;
        if was == to || was == CsState::Out {
            return Ok(0);
        }
        if was == CsState::Down {
            if let Some(c) = self.cs.get_mut(cs as usize) {
                c.state = to;
            }
            return Ok(0);
        }
        /* The sets it changes: those it is joining, and those it is an
           active member of but not the last -- a set is never left with no
           member active, and its writes go on at the one it has. Every
           version to be raised checked first: a refusal half way would
           leave half of the sets changed */
        let touches = |s: &Set| {
            s.joiner.is_some_and(|j| j.cs == cs)
                || (s.active() > 1 && (0..REPLICAS).any(|k| s.cs[k] == cs && s.replicas[k] == Replica::Active))
        };
        if self.sets.iter().any(|s| touches(s) && s.version == u32::MAX) {
            return Err(Refusal::TooMany);
        }
        let mut changed = 0u32;
        let mut released = 0u64;
        for id in 0..self.sets.len() as u32 {
            let Some(s) = self.sets.get(id as usize) else { break };
            if !touches(s) {
                continue;
            }
            self.unindex(id);
            let Some(s) = self.sets.get_mut(id as usize) else { break };
            if let Some(j) = s.joiner.filter(|j| j.cs == cs) {
                if s.cs[usize::from(j.place)] != cs {
                    released += u64::from(s.chunks);
                }
                s.joiner = None;
            }
            /* `touches` has left out a set it is the last member active of */
            for k in 0..REPLICAS {
                if s.cs[k] == cs && s.replicas[k] == Replica::Active {
                    s.replicas[k] = Replica::Down;
                    s.active_until[k] = s.version;
                }
            }
            s.version += 1;
            changed += 1;
            self.index(id);
        }
        if let Some(c) = self.cs.get_mut(cs as usize) {
            c.state = to;
            c.reserved -= released;
        }
        Ok(changed)
    }

    /// cs `cs` answering again: up, given new chunks. Its sets are as they
    /// were: in one it is down in, it is back by joining it. Done again
    /// where it is up.
    fn cs_up(&mut self, cs: u32) -> Result<u32, Refusal> {
        self.ready()?;
        let c = self.cs.get(cs as usize).ok_or(Refusal::NoSuchCs)?;
        if c.state == CsState::Up {
            return Ok(0);
        }
        self.member_host(&c.host.clone())?;
        if let Some(c) = self.cs.get_mut(cs as usize) {
            c.state = CsState::Up;
        }
        Ok(0)
    }

    /// cs `cs` joining set `set`, planned at its `version`, to take member
    /// `place`'s place once its copy is done: the member itself, down and
    /// back -- its room its own already -- or another cs, with room for every
    /// chunk of the set, in the place of a member down, out or drained.
    fn join(&mut self, set: u32, place: u8, cs: u32, version: u32) -> Result<u32, Refusal> {
        let s = self.set_at(set, version)?;
        let k = usize::from(place);
        if k >= REPLICAS {
            return Err(Refusal::NoSuchSet);
        }
        let c = self.cs.get(cs as usize).ok_or(Refusal::NoSuchCs)?;
        if c.state != CsState::Up {
            return Err(Refusal::NotUp);
        }
        self.member_host(&c.host)?;
        let member = s.cs[k];
        let back = member == cs;
        if back {
            if s.replicas[k] != Replica::Down {
                return Err(Refusal::NotDown);
            }
        } else {
            if s.cs.contains(&cs) {
                return Err(Refusal::AlreadyMember);
            }
            let m = self.cs.get(member as usize).ok_or(Refusal::NoSuchCs)?;
            let gone = m.state == CsState::Out || self.member(&m.host).is_none();
            if !(s.replicas[k] == Replica::Down && gone) && !m.draining() {
                return Err(Refusal::InPlace);
            }
            if c.free() < u64::from(s.chunks) {
                return Err(Refusal::NoRoom);
            }
        }
        let since = version.checked_add(1).ok_or(Refusal::TooMany)?;
        let chunks = s.chunks;
        self.unindex(set);
        if let Some(s) = self.sets.get_mut(set as usize) {
            s.joiner = Some(Joiner { cs, place, since });
            s.version = since;
        }
        if !back {
            if let Some(c) = self.cs.get_mut(cs as usize) {
                c.reserved += u64::from(chunks);
            }
        }
        Ok(since)
    }

    /// Set `set`'s join by `cs` of `since` done: the joiner the member in
    /// its place, active, the room of the member it replaces given back,
    /// the three in order again.
    fn joined(&mut self, set: u32, cs: u32, since: u32) -> Result<u32, Refusal> {
        self.ready()?;
        let s = self.sets.get(set as usize).ok_or(Refusal::NoSuchSet)?;
        let Some(j) = s.joiner.filter(|j| j.cs == cs && j.since == since) else { return Err(Refusal::Stale) };
        let version = s.version.checked_add(1).ok_or(Refusal::TooMany)?;
        let k = usize::from(j.place);
        let (old, chunks) = (s.cs[k], s.chunks);
        let mut members: [(u32, Replica, u32); REPLICAS] = [(0, Replica::Active, 0); REPLICAS];
        for (i, m) in members.iter_mut().enumerate() {
            *m = if i == k { (cs, Replica::Active, 0) } else { (s.cs[i], s.replicas[i], s.active_until[i]) };
        }
        members.sort_unstable_by_key(|m| m.0);
        self.unindex(set);
        if let Some(s) = self.sets.get_mut(set as usize) {
            for (i, m) in members.iter().enumerate() {
                s.cs[i] = m.0;
                s.replicas[i] = m.1;
                s.active_until[i] = m.2;
            }
            s.joiner = None;
            s.version = version;
        }
        if old != cs {
            if let Some(c) = self.cs.get_mut(old as usize) {
                c.reserved -= u64::from(chunks);
            }
        }
        self.index(set);
        Ok(version)
    }

    /// Set `set`'s join by `cs` of `since` given up: the room it took given
    /// back.
    fn unjoin(&mut self, set: u32, cs: u32, since: u32) -> Result<u32, Refusal> {
        self.ready()?;
        let s = self.sets.get(set as usize).ok_or(Refusal::NoSuchSet)?;
        let Some(j) = s.joiner.filter(|j| j.cs == cs && j.since == since) else { return Err(Refusal::Stale) };
        let version = s.version.checked_add(1).ok_or(Refusal::TooMany)?;
        let elsewhere = s.cs[usize::from(j.place)] != cs;
        let chunks = s.chunks;
        if let Some(s) = self.sets.get_mut(set as usize) {
            s.joiner = None;
            s.version = version;
        }
        if elsewhere {
            if let Some(c) = self.cs.get_mut(cs as usize) {
                c.reserved -= u64::from(chunks);
            }
        }
        self.index(set);
        Ok(version)
    }

    /// The clean set of cs's `cs` -- every member active, no joiner -- where
    /// new chunks on them go: the lowest-numbered.
    pub(crate) fn clean_set(&self, cs: [u32; REPLICAS]) -> Option<u32> {
        self.set_of.range((cs, 0)..=(cs, u32::MAX)).next().map(|&(_, id)| id)
    }

    /// Whether set `id` is clean: what placement puts new chunks into.
    fn is_clean(s: &Set) -> bool {
        s.joiner.is_none() && s.replicas.iter().all(|&r| r == Replica::Active)
    }

    /// Set `id` out of the index of clean sets: before it changes.
    fn unindex(&mut self, id: u32) {
        if let Some(s) = self.sets.get(id as usize) {
            self.set_of.remove(&(s.cs, id));
        }
    }

    /// Set `id` in the index of clean sets, where it is one: once it has
    /// changed.
    pub(crate) fn index(&mut self, id: u32) {
        if let Some(s) = self.sets.get(id as usize).filter(|s| State::is_clean(s)) {
            self.set_of.insert((s.cs, id));
        }
    }

    /// The set new chunks on cs's `cs` go into, in a placement onto those of
    /// `used`: their clean set; else the lowest-numbered spare one (`spare`),
    /// made again on them -- its version moved on, so that no epoch of what
    /// it was is one of what it is -- or a new one; None where the table is
    /// full.
    fn set_for(&mut self, cs: [u32; REPLICAS], used: &BTreeSet<[u32; REPLICAS]>) -> Option<u32> {
        if let Some(id) = self.clean_set(cs) {
            return Some(id);
        }
        let empty = self.sets.iter().position(|s| State::spare(s, used));
        if let Some(at) = empty {
            let id = at as u32;
            self.unindex(id);
            if let Some(s) = self.sets.get_mut(at) {
                *s = Set { cs, replicas: [Replica::Active; REPLICAS], active_until: [0; REPLICAS], joiner: None,
                           version: s.version + 1, chunks: 0 };
            }
            self.index(id);
            return Some(id);
        }
        if self.sets.len() >= SETS_MAX {
            return None;
        }
        let id = self.sets.len() as u32;
        self.sets.push(Set { cs, replicas: [Replica::Active; REPLICAS], active_until: [0; REPLICAS], joiner: None,
                             version: 1, chunks: 0 });
        self.index(id);
        Some(id)
    }

    /// The cs's that hold room for every chunk of set `s`: its three, and a
    /// joiner in another's place.
    fn holders(s: &Set) -> ([u32; REPLICAS], Option<u32>) {
        (s.cs, s.joiner.filter(|j| s.cs.get(usize::from(j.place)) != Some(&j.cs)).map(|j| j.cs))
    }

    /// A chunk in set `set`, counted once: off the free list where it has
    /// one.
    fn new_chunk(&mut self, set: u32) -> u32 {
        let (cs, joiner) = match self.sets.get_mut(set as usize) {
            Some(s) => {
                s.chunks += 1;
                State::holders(s)
            }
            None => unreachable!("a set made for it"),
        };
        for c in cs.into_iter().chain(joiner) {
            if let Some(c) = self.cs.get_mut(c as usize) {
                c.reserved += 1;
            }
        }
        if self.free_head != NO_CHUNK {
            let id = self.free_head;
            if let Some(ch) = self.chunks.get_mut(id as usize) {
                self.free_head = ch.set;
                /* The number's next chunk: never 0, which no chunk is */
                let incarnation = ch.incarnation.wrapping_add(1).max(1);
                *ch = Chunk { set, count: 1, flags: 0, incarnation };
            }
            self.free_count -= 1;
            return id;
        }
        let id = self.chunks.len() as u32;
        self.chunks.push(Chunk { set, count: 1, flags: 0, incarnation: 1 });
        id
    }

    /// One of chunk `id`'s counts given back: at none, the chunk freed --
    /// its slots given back on its set's cs's, the chunk onto the free list.
    fn release_chunk(&mut self, id: u32) {
        let Some(ch) = self.chunks.get_mut(id as usize) else { return };
        if ch.is_free() || ch.count == 0 {
            return;
        }
        ch.count -= 1;
        if ch.count > 0 {
            return;
        }
        let set = ch.set;
        *ch = Chunk { set: self.free_head, count: 0, flags: CHUNK_FREE, incarnation: ch.incarnation };
        self.free_head = id;
        self.free_count += 1;
        let (cs, joiner) = match self.sets.get_mut(set as usize) {
            Some(s) => {
                s.chunks -= 1;
                State::holders(s)
            }
            None => return,
        };
        for c in cs.into_iter().chain(joiner) {
            if let Some(c) = self.cs.get_mut(c as usize) {
                c.reserved -= 1;
            }
        }
    }
}
