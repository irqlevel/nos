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
//! - **A set** is three cs's on three hosts, each replica's state, and a
//!   version. Chunks placed on the same three cs's name the same set, so a
//!   host lost changes the few sets it is in rather than every chunk. A set
//!   and its version are a chunk's epoch.
//! - **A chunk** -- 4 MiB of a volume -- is 8 bytes: its set, how many maps
//!   name it, its flags. A free one is on the free list, chained through
//!   its own record, so no other table keeps it.
//! - **A volume**: its name and size, its kind, its own host, its attach,
//!   and its map, a chunk's number for each 4 MiB of it.

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

/// A host's chunk server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cs {
    /// Its number: its place in the table, which only grows.
    pub id: u32,
    pub host: String,
    pub devices: Vec<Device>,
    /// The slots its replicas take: one for every chunk whose set it is in.
    pub reserved: u64,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replica {
    Active,
    /// Being copied onto: written to, not read from (N9).
    Rebuilding,
    /// Not answering: written to by nobody until it is back (N9).
    Down,
}

/// Three cs's on three hosts, by number, in increasing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Set {
    pub cs: [u32; REPLICAS],
    pub replicas: [Replica; REPLICAS],
    /// Raised by every change of its members: a chunk's epoch is its set and
    /// that.
    pub version: u32,
    /// The chunks in it.
    pub chunks: u32,
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

/// A volume's writer: the host, and the generation it attached at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attach {
    pub host: String,
    pub generation: u64,
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
            Command::Attach { name, host, writable, force } => {
                self.attach(name, host, *writable, *force).map(|generation| Answer::Attached { generation })
            }
            Command::Detach { name, host, generation } => {
                self.detach(name, host, *generation).map(|generation| Answer::Attached { generation })
            }
            Command::Digest { at, digest } => {
                self.checks.compare(*at, digest, index);
                Ok(Answer::Checked)
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
                self.cs.push(Cs { id, host: String::from(host), devices: Vec::new(), reserved: 0 });
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

    fn attach(&mut self, name: &str, host: &str, writable: bool, force: bool) -> Result<u64, Refusal> {
        self.ready()?;
        self.member_host(host)?;
        let Some(at) = self.volume_at(name) else { return Err(Refusal::NoSuchVolume) };
        let v = self.volumes.get_mut(at).ok_or(Refusal::NoSuchVolume)?;
        if writable {
            if v.kind != Kind::Volume {
                return Err(Refusal::ReadOnly);
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
            v.writer = Some(Attach { host: String::from(host), generation: v.generation });
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
        for k in 0..n {
            trial.pick(volume, from + k).ok_or(Refusal::NoRoom)?;
        }
        if !trial.leaves_rebuild_room() {
            return Err(Refusal::NoRoomToRebuild);
        }
        let mut real = placer;
        for k in 0..n {
            /* The same picks as the trial's, which found room for every one */
            let Some(cs) = real.pick(volume, from + k) else { unreachable!("a placement the trial made") };
            let set = self.set_for(cs);
            map.push(self.new_chunk(set));
        }
        Ok(())
    }

    /// The set of cs's `cs`, made where there is none.
    fn set_for(&mut self, cs: [u32; REPLICAS]) -> u32 {
        if let Some(&id) = self.set_of.get(&cs) {
            return id;
        }
        let id = self.sets.len() as u32;
        self.sets.push(Set { cs, replicas: [Replica::Active; REPLICAS], version: 1, chunks: 0 });
        self.set_of.insert(cs, id);
        id
    }

    /// A chunk in set `set`, counted once: off the free list where it has
    /// one.
    fn new_chunk(&mut self, set: u32) -> u32 {
        let cs = match self.sets.get_mut(set as usize) {
            Some(s) => {
                s.chunks += 1;
                s.cs
            }
            None => unreachable!("a set made for it"),
        };
        for c in cs {
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
        let cs = match self.sets.get_mut(set as usize) {
            Some(s) => {
                s.chunks -= 1;
                s.cs
            }
            None => return,
        };
        for c in cs {
            if let Some(c) = self.cs.get_mut(c as usize) {
                c.reserved -= 1;
            }
        }
    }
}
