//! The `meta` target: NBS's metadata (src/crates/common/storage/nbsmeta)
//! held, through random histories of every command, to a model of its own
//! -- written from plans/nbs.md's rules, sharing no line with the crate:
//!
//! - every answer the model's, refusals in the crate's order;
//! - where every chunk of every volume is: the model places them by the
//!   plan's rules ("Where replicas go") on its own count of room, and each
//!   chunk's set must name the same three hosts' cs's;
//! - and that the placement places all it can: a volume refused for room
//!   must not fit by counting -- its chunks fit three distinct hosts exactly
//!   when the hosts' free room, each counted up to the chunks, adds up to
//!   three times them;
//! - every cs's reserved its replicas', every chunk counted by its maps;
//! - every replica set the model's, made in the order the placement first
//!   names its three cs's, its replicas taken out one at a time (N8) -- the
//!   set's version up at each, never its last active one;
//! - its snapshot read back as itself, written in pieces of any size as in
//!   one, and bytes spoiled read without a panic -- refused, or read as a
//!   state that writes and reads back the same;
//! - a replica applying the same entries the same state, with the same
//!   digests, and one restored from a snapshot part way the same at the end;
//! - a checkpoint's digest the state's own at that entry, every true digest
//!   of the leader's held, and a false one said as a divergence.

use std::collections::BTreeMap;

use nbsmeta::{Answer, Command, Refusal, State, CHECK_EVERY};
use raft::rsm::{Frozen, StateMachine};

use crate::heap::Exempt;
use crate::rng::{mix, Rng};

/// The hosts a run's members are drawn from.
const HOSTS: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
/// The volumes' names.
const VOLUMES: [&str; 10] = ["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "web.root", "db-1"];
/// What the model has of the crate's limits: the plan's.
const MEMBERS_MAX: usize = 16;
const REMOVED_MAX: usize = 1024;
const CS_MAX: usize = 64;
const DEVICES_MAX: usize = 16;
const VOLUMES_MAX: usize = 4096;
const CHUNKS_MAX: u64 = 1 << 22;
const VOLUME_MIB_MAX: u64 = 16 << 20;
const SLOTS_MAX: u32 = 1 << 28;
const READERS_MAX: usize = 16;
const CHUNK_MIB: u64 = 4;
const REPLICAS: usize = 3;

#[derive(Clone)]
struct MDevice {
    id: u32,
    ident: String,
    slots: u32,
    active: bool,
}

#[derive(Clone)]
struct MCs {
    host: String,
    devices: Vec<MDevice>,
    reserved: u64,
}

impl MCs {
    fn slots(&self) -> u64 {
        self.devices.iter().filter(|d| d.active).map(|d| u64::from(d.slots)).sum()
    }
}

/// A replica set: its three cs's, which of them are taken out, its version.
#[derive(Clone)]
struct MSet {
    cs: [u32; REPLICAS],
    down: [bool; REPLICAS],
    version: u32,
}

#[derive(Clone)]
struct MVol {
    id: u32,
    size_mib: u64,
    host: String,
    generation: u64,
    /// The writer: its host, generation and token.
    writer: Option<(String, u64, u64)>,
    readers: Vec<String>,
    /// Each chunk's three cs's, by number, in increasing order.
    chunks: Vec<[u32; REPLICAS]>,
}

#[derive(Default, Clone)]
struct Model {
    init: bool,
    members: Vec<(String, u64)>,
    removed: Vec<(String, u64)>,
    cs: Vec<MCs>,
    vols: BTreeMap<String, MVol>,
    next_device: u32,
    next_volume: u32,
    /// Every set, in the order its three cs's were first placed on.
    sets: Vec<MSet>,
    set_of: BTreeMap<[u32; REPLICAS], usize>,
}

fn m_name(s: &str) -> bool {
    (1..=63).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

fn m_volume_name(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=63).contains(&b.len()) && b[0].is_ascii_alphanumeric()
        && b.iter().all(|&c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

fn m_ident(s: &str) -> bool {
    (s.starts_with("part:") || s.starts_with("disk:")) && s.len() > 5 && s.len() <= 64
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b':'))
}

/// The plan's tie, as its formula has it.
fn m_tie(volume: u32, index: u64, cs: u32) -> u64 {
    let mut z = ((u64::from(volume) << 32) ^ index).wrapping_add(u64::from(cs).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A placement's outcome by the model: where each chunk goes, or why not --
/// and for a refusal of room, whether the chunks would have fitted by
/// counting.
struct Placed {
    result: Result<Vec<[u32; REPLICAS]>, Refusal>,
    fits: bool,
}

impl Model {
    fn is_member(&self, host: &str) -> bool {
        self.members.iter().any(|(h, _)| h == host)
    }

    fn live(&self) -> u64 {
        self.vols.values().map(|v| v.chunks.len() as u64).sum()
    }

    /// `n` chunks of volume `volume` from its `from`th, by the plan's rules.
    fn place(&self, volume: u32, home: &str, from: u64, n: u64) -> Placed {
        /* (cs, slots, reserved, free) of the cs's that take new chunks */
        let mut c: Vec<(u32, u64, u64, u64)> = Vec::new();
        for (i, x) in self.cs.iter().enumerate() {
            let slots = x.slots();
            if slots > 0 && self.is_member(&x.host) {
                c.push((i as u32, slots, x.reserved, slots.saturating_sub(x.reserved)));
            }
        }
        let fits = c.iter().map(|x| x.3.min(n)).sum::<u64>() >= 3 * n;
        /* Fewer than three that take chunks at all; three full is NoRoom */
        if c.len() < REPLICAS {
            return Placed { result: Err(Refusal::TooFewHosts), fits };
        }
        let home = self.cs.iter().position(|x| x.host == home).and_then(|h| c.iter().position(|x| x.0 == h as u32));
        let mut out = Vec::new();
        for k in 0..n {
            let mut chosen: Vec<usize> = Vec::new();
            if let Some(h) = home {
                if c[h].3 > 0 {
                    chosen.push(h);
                }
            }
            while chosen.len() < REPLICAS {
                let best = (0..c.len()).filter(|&i| c[i].3 > 0 && !chosen.contains(&i))
                    .max_by_key(|&i| (c[i].3, m_tie(volume, from + k, c[i].0)));
                match best {
                    Some(i) => chosen.push(i),
                    None => return Placed { result: Err(Refusal::NoRoom), fits },
                }
            }
            let mut cs = [0u32; REPLICAS];
            for (slot, &i) in cs.iter_mut().zip(chosen.iter()) {
                c[i].3 -= 1;
                c[i].2 += 1;
                *slot = c[i].0;
            }
            cs.sort();
            out.push(cs);
        }
        if c.len() >= REPLICAS + 1 {
            let mut biggest = 0;
            for i in 1..c.len() {
                if c[i].2 > c[biggest].2 {
                    biggest = i;
                }
            }
            let others: u64 = (0..c.len()).filter(|&i| i != biggest).map(|i| c[i].1.saturating_sub(c[i].2)).sum();
            if others < c[biggest].2 {
                return Placed { result: Err(Refusal::NoRoomToRebuild), fits };
            }
        }
        Placed { result: Ok(out), fits }
    }

    fn take(&mut self, chunks: &[[u32; REPLICAS]]) {
        for set in chunks {
            for &c in set {
                self.cs[c as usize].reserved += 1;
            }
            if !self.set_of.contains_key(set) {
                self.set_of.insert(*set, self.sets.len());
                self.sets.push(MSet { cs: *set, down: [false; REPLICAS], version: 1 });
            }
        }
    }

    fn give_back(&mut self, chunks: &[[u32; REPLICAS]]) {
        for set in chunks {
            for &c in set {
                self.cs[c as usize].reserved -= 1;
            }
        }
    }

    /// The model's answer to `cmd`, and whether a refusal for room was one the
    /// chunks would have fitted.
    fn apply(&mut self, cmd: Option<&Command>) -> (Answer, bool) {
        let Some(cmd) = cmd else { return (Answer::Refused(Refusal::Malformed), false) };
        let mut unfair = false;
        let a = match self.change(cmd, &mut unfair) {
            Ok(a) => a,
            Err(why) => Answer::Refused(why),
        };
        (a, unfair)
    }

    fn change(&mut self, cmd: &Command, unfair: &mut bool) -> Result<Answer, Refusal> {
        use Refusal::*;
        match cmd {
            Command::Init { name, host, member, .. } => {
                if self.init {
                    return Err(Initialized);
                }
                if !m_name(name) || !m_name(host) {
                    return Err(BadName);
                }
                if *member == 0 {
                    return Err(BadMember);
                }
                self.init = true;
                self.members.push((host.clone(), *member));
                Ok(Answer::Done { member: *member })
            }
            Command::Add { host, member } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                if !m_name(host) {
                    return Err(BadName);
                }
                if *member == 0 {
                    return Err(BadMember);
                }
                if let Some((_, id)) = self.members.iter().find(|(h, _)| h == host) {
                    return if id == member { Ok(Answer::Done { member: *id }) } else { Err(HostIsMember) };
                }
                if self.members.iter().any(|(_, id)| id == member) {
                    return Err(MemberInUse);
                }
                if self.removed.iter().any(|(_, id)| id == member) {
                    return Err(MemberRemoved);
                }
                if self.members.len() >= MEMBERS_MAX {
                    return Err(Full);
                }
                self.members.push((host.clone(), *member));
                Ok(Answer::Done { member: *member })
            }
            Command::Remove { host } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                let Some(at) = self.members.iter().position(|(h, _)| h == host) else {
                    return self.removed.iter().rev().find(|(h, _)| h == host).map(|(_, id)| Answer::Done { member: *id })
                        .ok_or(NoSuchHost);
                };
                if self.members.len() == 1 {
                    return Err(LastMember);
                }
                if self.removed.len() >= REMOVED_MAX {
                    return Err(Full);
                }
                let m = self.members.remove(at);
                let id = m.1;
                self.removed.push(m);
                Ok(Answer::Done { member: id })
            }
            Command::DeviceAdd { host, ident, slots } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                if !self.is_member(host) {
                    return Err(NotAMember);
                }
                if !m_ident(ident) || *slots == 0 || *slots > SLOTS_MAX {
                    return Err(BadDevice);
                }
                for c in &self.cs {
                    if let Some(d) = c.devices.iter().find(|d| &d.ident == ident) {
                        return match (&c.host == host, d.slots == *slots) {
                            (true, true) => Ok(Answer::Device { id: d.id }),
                            (true, false) => Err(DeviceChanged),
                            (false, _) => Err(DeviceElsewhere),
                        };
                    }
                }
                let at = match self.cs.iter().position(|c| &c.host == host) {
                    Some(at) => at,
                    None => {
                        if self.cs.len() >= CS_MAX {
                            return Err(TooMany);
                        }
                        self.cs.push(MCs { host: host.clone(), devices: Vec::new(), reserved: 0 });
                        self.cs.len() - 1
                    }
                };
                if self.cs[at].devices.len() >= DEVICES_MAX {
                    return Err(TooMany);
                }
                let id = self.next_device;
                self.next_device += 1;
                self.cs[at].devices.push(MDevice { id, ident: ident.clone(), slots: *slots, active: true });
                Ok(Answer::Device { id })
            }
            Command::DeviceDrain { host, ident } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                let c = self.cs.iter_mut().find(|c| &c.host == host).ok_or(NoSuchDevice)?;
                let d = c.devices.iter_mut().find(|d| &d.ident == ident).ok_or(NoSuchDevice)?;
                d.active = false;
                Ok(Answer::Device { id: d.id })
            }
            Command::VolumeCreate { name, size_mib, host } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                if !m_volume_name(name) || *size_mib == 0 || *size_mib > VOLUME_MIB_MAX {
                    return Err(BadVolume);
                }
                if !self.is_member(host) {
                    return Err(NotAMember);
                }
                if let Some(v) = self.vols.get(name) {
                    return if v.size_mib == *size_mib { Ok(Answer::Volume { id: v.id }) } else { Err(NameTaken) };
                }
                if self.vols.len() >= VOLUMES_MAX {
                    return Err(TooMany);
                }
                let n = size_mib.div_ceil(CHUNK_MIB);
                if self.live() + n > CHUNKS_MAX {
                    return Err(TooMany);
                }
                let id = self.next_volume;
                let p = self.place(id, host, 0, n);
                *unfair = p.result == Err(NoRoom) && p.fits;
                let chunks = p.result?;
                self.take(&chunks);
                self.next_volume += 1;
                self.vols.insert(name.clone(), MVol { id, size_mib: *size_mib, host: host.clone(), generation: 0,
                                                      writer: None, readers: Vec::new(), chunks });
                Ok(Answer::Volume { id })
            }
            Command::VolumeDelete { name } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                let Some(v) = self.vols.get(name) else { return Ok(Answer::Volume { id: 0 }) };
                if v.writer.is_some() || !v.readers.is_empty() {
                    return Err(Attached);
                }
                let v = self.vols.remove(name).expect("there");
                self.give_back(&v.chunks);
                Ok(Answer::Volume { id: v.id })
            }
            Command::VolumeResize { name, size_mib } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                if *size_mib == 0 || *size_mib > VOLUME_MIB_MAX {
                    return Err(BadVolume);
                }
                let v = self.vols.get(name).ok_or(NoSuchVolume)?.clone();
                if *size_mib < v.size_mib {
                    return Err(Shrink);
                }
                let want = size_mib.div_ceil(CHUNK_MIB);
                let had = v.chunks.len() as u64;
                if want > had {
                    if self.live() + (want - had) > CHUNKS_MAX {
                        return Err(TooMany);
                    }
                    let p = self.place(v.id, &v.host, had, want - had);
                    *unfair = p.result == Err(NoRoom) && p.fits;
                    let more = p.result?;
                    self.take(&more);
                    self.vols.get_mut(name).expect("there").chunks.extend(more);
                }
                self.vols.get_mut(name).expect("there").size_mib = *size_mib;
                Ok(Answer::Volume { id: v.id })
            }
            Command::Attach { name, host, writable, force, token } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                if !self.is_member(host) {
                    return Err(NotAMember);
                }
                let v = self.vols.get_mut(name).ok_or(NoSuchVolume)?;
                if *writable {
                    if let Some((_, g, _)) = v.writer.as_ref().filter(|(h, _, t)| *token != 0 && t == token && h == host) {
                        return Ok(Answer::Attached { generation: *g });
                    }
                    if !*force {
                        if v.writer.as_ref().is_some_and(|(h, _, _)| h != host) {
                            return Err(AttachedElsewhere);
                        }
                        if !v.readers.is_empty() {
                            return Err(ReadersAttached);
                        }
                    }
                    v.readers.clear();
                    v.generation += 1;
                    v.writer = Some((host.clone(), v.generation, *token));
                    return Ok(Answer::Attached { generation: v.generation });
                }
                if v.writer.is_some() {
                    return Err(Writing);
                }
                if !v.readers.contains(host) {
                    if v.readers.len() >= READERS_MAX {
                        return Err(TooMany);
                    }
                    v.readers.push(host.clone());
                }
                Ok(Answer::Attached { generation: 0 })
            }
            Command::Detach { name, host, generation } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                let Some(v) = self.vols.get_mut(name) else { return Ok(Answer::Attached { generation: 0 }) };
                if *generation == 0 {
                    v.readers.retain(|r| r != host);
                    return Ok(Answer::Attached { generation: 0 });
                }
                if v.writer.as_ref().is_some_and(|(h, g, _)| h == host && g == generation) {
                    v.writer = None;
                    return Ok(Answer::Attached { generation: *generation });
                }
                Ok(Answer::Attached { generation: 0 })
            }
            Command::Digest { .. } => Ok(Answer::Checked),
            Command::ReplicaDown { set, cs, name, generation, .. } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                let v = self.vols.get(name).ok_or(NoSuchVolume)?;
                if v.writer.as_ref().is_none_or(|(_, g, _)| g != generation) {
                    return Err(NotTheWriter);
                }
                if !v.chunks.iter().any(|t| self.set_of.get(t) == Some(&(*set as usize))) {
                    return Err(NoSuchSet);
                }
                let s = self.sets.get_mut(*set as usize).ok_or(NoSuchSet)?;
                let at = s.cs.iter().position(|c| c == cs).ok_or(NoSuchSet)?;
                if s.down[at] {
                    return Ok(Answer::Set { version: s.version });
                }
                if (0..REPLICAS).all(|i| i == at || s.down[i]) {
                    return Err(LastReplica);
                }
                s.down[at] = true;
                s.version += 1;
                Ok(Answer::Set { version: s.version })
            }
        }
    }
}

/// Where the metadata differs from the model, said.
fn differs(s: &State, m: &Model) -> Option<String> {
    let members: Vec<(String, u64)> = s.members().iter().map(|x| (x.host.clone(), x.id)).collect();
    if members != m.members {
        return Some(format!("members {:?}, the model {:?}", members, m.members));
    }
    if s.cs().len() != m.cs.len() {
        return Some(format!("{} chunk servers, the model {}", s.cs().len(), m.cs.len()));
    }
    for (i, (c, mc)) in s.cs().iter().zip(m.cs.iter()).enumerate() {
        if c.id as usize != i || c.host != mc.host || c.reserved != mc.reserved || c.devices.len() != mc.devices.len() {
            return Some(format!("cs {}: {} reserved {} of {} devices, the model {} reserved {} of {}", i, c.host,
                                c.reserved, c.devices.len(), mc.host, mc.reserved, mc.devices.len()));
        }
        for (d, md) in c.devices.iter().zip(mc.devices.iter()) {
            let active = d.state == nbsmeta::DeviceState::Active;
            if d.id != md.id || d.ident != md.ident || d.slots != md.slots || active != md.active {
                return Some(format!("cs {}'s device {:?}, the model {} {} {} {}", i, d, md.id, md.ident, md.slots,
                                    md.active));
            }
        }
    }
    if s.sets().len() != m.sets.len() {
        return Some(format!("{} replica sets, the model {}", s.sets().len(), m.sets.len()));
    }
    for (i, (set, ms)) in s.sets().iter().zip(m.sets.iter()).enumerate() {
        let down: Vec<bool> = set.replicas.iter().map(|r| *r == nbsmeta::Replica::Down).collect();
        if set.cs != ms.cs || down != ms.down || set.version != ms.version {
            return Some(format!("set {}: cs's {:?}, out {:?}, version {}; the model {:?}, {:?}, {}", i, set.cs, down,
                                set.version, ms.cs, ms.down, ms.version));
        }
    }
    if s.volumes().len() != m.vols.len() {
        return Some(format!("{} volumes, the model {}", s.volumes().len(), m.vols.len()));
    }
    if s.live_chunks() != m.live() {
        return Some(format!("{} chunks in use, the model {}", s.live_chunks(), m.live()));
    }
    for (name, mv) in &m.vols {
        let Some(v) = s.volume(name) else { return Some(format!("no volume {}", name)) };
        let writer = v.writer.as_ref().map(|w| (w.host.clone(), w.generation, w.token));
        if v.id != mv.id || v.size_mib != mv.size_mib || v.host != mv.host || v.generation != mv.generation
            || writer != mv.writer || v.readers != mv.readers || v.map.len() != mv.chunks.len()
        {
            return Some(format!("volume {}: {} {} MiB on {} gen {} writer {:?} readers {:?} {} chunks; the model {} {} \
                                 MiB on {} gen {} writer {:?} readers {:?} {} chunks", name, v.id, v.size_mib, v.host,
                                v.generation, writer, v.readers, v.map.len(), mv.id, mv.size_mib, mv.host,
                                mv.generation, mv.writer, mv.readers, mv.chunks.len()));
        }
        for (k, (&c, want)) in v.map.iter().zip(mv.chunks.iter()).enumerate() {
            let Some(ch) = s.chunks().get(c as usize) else { return Some(format!("{}'s chunk {} is none", name, k)) };
            let Some(set) = s.sets().get(ch.set as usize) else {
                return Some(format!("{}'s chunk {} in no set", name, k));
            };
            if ch.is_free() || set.cs != *want {
                return Some(format!("{}'s chunk {} on cs's {:?}, the model's {:?}", name, k, set.cs, want));
            }
            let hosts: Vec<&str> = set.cs.iter().map(|&c| s.cs()[c as usize].host.as_str()).collect();
            if hosts[0] == hosts[1] || hosts[1] == hosts[2] || hosts[0] == hosts[2] {
                return Some(format!("{}'s chunk {} on two replicas of one host: {:?}", name, k, hosts));
            }
        }
    }
    None
}

/// The rules of what is stored, held by the fuzzer's own counting -- not
/// the crate's check, which a snapshot read back has passed already: each
/// cs's reserved its sets' chunks, each set's chunks those in it, each chunk
/// in use counted by the maps that name it, and the free list every free
/// chunk once, through free ones only.
/// The chunks' numbers through a run: each number's newest incarnation, and
/// whether that chunk is in use. A number's first chunk is incarnation 1; it
/// keeps its incarnation while in use and while free, and the next chunk
/// taking the number has one more -- so a cs never takes one for another.
fn incarnations(s: &State, lives: &mut Vec<(u32, bool)>) -> Option<String> {
    let chunks = s.chunks();
    if chunks.len() < lives.len() {
        return Some(format!("the chunk table went from {} to {}", lives.len(), chunks.len()));
    }
    for (id, ch) in chunks.iter().enumerate() {
        let live = !ch.is_free();
        match lives.get_mut(id) {
            None => {
                if ch.incarnation != 1 {
                    return Some(format!("chunk {}, a number new, at incarnation {}", id, ch.incarnation));
                }
                lives.push((ch.incarnation, live));
            }
            Some((inc, was_live)) => {
                let want = if !*was_live && live { inc.wrapping_add(1).max(1) } else { *inc };
                if ch.incarnation != want {
                    return Some(format!("chunk {} at incarnation {}, {} expected (it was {} and {})", id,
                                        ch.incarnation, want, inc, if *was_live { "in use" } else { "free" }));
                }
                *inc = ch.incarnation;
                *was_live = live;
            }
        }
    }
    None
}

fn fsck(s: &State) -> Option<String> {
    let chunks = s.chunks();
    if let Some(i) = chunks.iter().position(|c| c.incarnation == 0) {
        return Some(format!("chunk {} of incarnation 0", i));
    }
    let mut named = vec![0u64; chunks.len()];
    for v in s.volumes().iter() {
        for &c in v.map.iter() {
            match named.get_mut(c as usize) {
                Some(n) => *n += 1,
                None => return Some(format!("volume {}'s map names chunk {}, past the table", v.name, c)),
            }
        }
    }
    let mut in_set = vec![0u64; s.sets().len()];
    let mut free = 0u64;
    for (i, ch) in chunks.iter().enumerate() {
        if ch.is_free() {
            if ch.count != 0 || named[i] != 0 {
                return Some(format!("free chunk {} counted {} and named {} times", i, ch.count, named[i]));
            }
            free += 1;
        } else {
            if u64::from(ch.count) != named[i] || ch.count == 0 {
                return Some(format!("chunk {} counted {}, named {} times", i, ch.count, named[i]));
            }
            match in_set.get_mut(ch.set as usize) {
                Some(n) => *n += 1,
                None => return Some(format!("chunk {} in set {}, past the table", i, ch.set)),
            }
        }
    }
    for (i, set) in s.sets().iter().enumerate() {
        if u64::from(set.chunks) != in_set[i] {
            return Some(format!("set {} says {} chunks, {} are in it", i, set.chunks, in_set[i]));
        }
    }
    for (i, c) in s.cs().iter().enumerate() {
        let replicas: u64 = s.sets().iter().filter(|x| x.cs.contains(&(i as u32))).map(|x| u64::from(x.chunks)).sum();
        if replicas != c.reserved {
            return Some(format!("cs {} reserved {}, its sets have {} chunks", i, c.reserved, replicas));
        }
    }
    let mut seen = vec![false; chunks.len()];
    let mut at = s.free_head();
    let mut walked = 0u64;
    while at != nbsmeta::NO_CHUNK {
        let Some(ch) = chunks.get(at as usize) else { return Some(format!("the free list through chunk {}, past the table", at)) };
        if !ch.is_free() || seen[at as usize] {
            return Some(format!("the free list through chunk {}, in use or twice", at));
        }
        seen[at as usize] = true;
        walked += 1;
        at = ch.set;
    }
    if walked != free {
        return Some(format!("the free list has {} chunks, {} are free", walked, free));
    }
    None
}

/// A command for the history: of every kind, valid and not.
fn command(r: &mut Rng, s: &State, m: &Model, fresh: &mut u64) -> Command {
    let host = |r: &mut Rng| HOSTS[r.below(HOSTS.len() as u64) as usize].to_string();
    let member = |r: &mut Rng| -> String {
        if m.members.is_empty() || r.chance(10) {
            HOSTS[r.below(HOSTS.len() as u64) as usize].to_string()
        } else {
            m.members[r.below(m.members.len() as u64) as usize].0.clone()
        }
    };
    let volume = |r: &mut Rng| VOLUMES[r.below(VOLUMES.len() as u64) as usize].to_string();
    if !m.init {
        *fresh += 1;
        return Command::Init { cluster: [7; 16], name: String::from("nbs"), host: host(r), member: *fresh };
    }
    match r.below(100) {
        0..=7 => {
            *fresh += 1;
            Command::Add { host: host(r), member: if r.chance(5) { 0 } else { *fresh } }
        }
        8..=9 => Command::Remove { host: member(r) },
        10..=24 => {
            let h = member(r);
            let ident = match r.below(10) {
                0 => String::from("nonsense"),
                1 => format!("disk:{}{}", HOSTS[r.below(HOSTS.len() as u64) as usize], r.below(3)),
                _ => format!("disk:{}{}", h, r.below(3)),
            };
            let slots = r.pick(&[0u32, 1, 2, 4, 8, 16, 40, 64, 300, 50_000, SLOTS_MAX + 1]).unwrap_or(8);
            Command::DeviceAdd { host: h, ident, slots }
        }
        25..=27 => {
            let h = member(r);
            Command::DeviceDrain { ident: format!("disk:{}{}", h, r.below(3)), host: h }
        }
        28..=49 => {
            let size_mib = r.pick(&[0u64, 1, 3, 4, 5, 8, 16, 33, 64, 100, 256, 1024, VOLUME_MIB_MAX + 1]).unwrap_or(4);
            Command::VolumeCreate { name: if r.chance(3) { String::from("Bad_Name") } else { volume(r) }, size_mib,
                                    host: member(r) }
        }
        50..=59 => Command::VolumeDelete { name: volume(r) },
        60..=67 => {
            let name = volume(r);
            let base = m.vols.get(&name).map_or(4, |v| v.size_mib);
            let size_mib = match r.below(4) {
                0 => base.saturating_sub(1).max(1),
                1 => base,
                _ => base + r.pick(&[1u64, 3, 4, 16, 128]).unwrap_or(4),
            };
            Command::VolumeResize { name, size_mib }
        }
        68..=79 => Command::Attach { name: volume(r), host: member(r), writable: r.chance(60), force: r.chance(15),
                                     token: r.below(4) },
        80..=86 => {
            let name = volume(r);
            let (h, generation) = match m.vols.get(&name).and_then(|v| v.writer.clone()) {
                Some((h, g, _)) if r.chance(70) => (h, g),
                _ => (member(r), r.pick(&[0u64, 1, 2, 7]).unwrap_or(0)),
            };
            Command::Detach { name, host: h, generation }
        }
        /* A replica taken out: of a set there is, one of its own -- or a set
           past the table, a cs that is none of its */
        87..=89 => {
            let sets = m.sets.len() as u64;
            let set = if sets == 0 || r.chance(10) { r.below(sets + 2) as u32 } else { r.below(sets) as u32 };
            let cs = match m.sets.get(set as usize) {
                Some(ms) if r.chance(85) => ms.cs[r.below(REPLICAS as u64) as usize],
                _ => r.below(m.cs.len() as u64 + 1) as u32,
            };
            /* As the volume's writer mostly, or one fenced since */
            let name = volume(r);
            let generation = match m.vols.get(&name).and_then(|v| v.writer.clone()) {
                Some((_, g, _)) if r.chance(80) => g,
                _ => r.below(4),
            };
            Command::ReplicaDown { set, cs, refused: r.chance(30), name, generation }
        }
        _ => {
            /* The leader's digest: the true one of the newest checkpoint, or
               of none the state has */
            match s.checks().latest() {
                Some((at, digest)) if r.chance(80) => Command::Digest { at, digest },
                _ => Command::Digest { at: r.below(10_000), digest: [r.below(256) as u8; 32] },
            }
        }
    }
}

/// `cmd`'s bytes, now and then spoiled.
fn entry(r: &mut Rng, cmd: &Command) -> Vec<u8> {
    let mut b = cmd.encode();
    if r.chance(3) && !b.is_empty() {
        let at = r.below(b.len() as u64) as usize;
        b[at] = r.below(256) as u8;
    }
    if r.chance(1) {
        b.truncate(r.below(b.len() as u64 + 1) as usize);
    }
    b
}

/// The state's snapshot, written in pieces of `piece` bytes at most.
fn written(s: &State, piece: usize) -> Vec<u8> {
    let mut f = s.freeze();
    let mut out = Vec::new();
    loop {
        let before = out.len();
        f.write(piece, &mut out);
        if out.len() == before {
            return out;
        }
    }
}

pub fn run(seed: u64, steps: u64, trace: bool) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x6e62_7366));
    let mut fresh = 0x1000;
    let mut s = State::new();
    let mut m = Model::default();
    m.next_device = 1;
    m.next_volume = 1;
    let mut replica = State::new();
    let mut restored: Option<State> = None;
    let mut answers: BTreeMap<String, u64> = BTreeMap::new();
    let mut lives: Vec<(u32, bool)> = Vec::new();
    let false_digest_at = if r.chance(50) { Some(r.range(steps / 2, steps)) } else { None };
    let mut diverged_expected: Option<u64> = None;
    for step in 1..=steps {
        let index = step;
        let mut cmd = command(&mut r, &s, &m, &mut fresh);
        if Some(step) == false_digest_at {
            if let Some((at, mut digest)) = s.checks().latest() {
                digest[0] ^= 1;
                cmd = Command::Digest { at, digest };
            }
        }
        /* The planned false digest goes in as it is; anything else may be
           spoiled */
        let bytes = if Some(step) == false_digest_at { cmd.encode() } else { entry(&mut r, &cmd) };
        let decoded = Command::decode(&bytes);
        /* A digest of a checkpoint the state still has, not its own -- the
           planned one, or one spoiling made -- is the first divergence */
        if let Some(Command::Digest { at, digest }) = &decoded {
            if diverged_expected.is_none() && s.checks().digest_at(*at).is_some_and(|mine| mine != *digest) {
                diverged_expected = Some(*at);
            }
        }
        if bytes == cmd.encode() && decoded.as_ref() != Some(&cmd) {
            return Err(format!("step {}: {:?} does not read back from its bytes", step, cmd));
        }
        let got = Answer::decode(&s.apply(index, &bytes))
            .ok_or_else(|| format!("step {}: an answer that does not read back", step))?;
        /* The model's own lists -- a volume of a TiB is megabytes of them --
           are the fuzzer's, not the crate's: what the state allocates is held
           to the kernel's heap, the model's is not */
        let (want, unfair) = {
            let _model = Exempt::new();
            m.apply(decoded.as_ref())
        };
        if trace {
            eprintln!("step {}: {:?} -> {:?}", step, decoded, got);
        }
        if got != want {
            return Err(format!("step {}: {:?} answered {:?}, the model {:?}", step, decoded, got, want));
        }
        if unfair {
            return Err(format!("step {}: {:?} refused for room its chunks fit by counting", step, decoded));
        }
        let kind = match got {
            Answer::Refused(why) => format!("{:?}", why),
            other => format!("{:?}", other).split([' ', '(', '{']).next().unwrap_or("").to_string(),
        };
        *answers.entry(kind).or_default() += 1;
        let differ = {
            let _model = Exempt::new();
            differs(&s, &m).or_else(|| fsck(&s)).or_else(|| incarnations(&s, &mut lives))
        };
        if let Some(d) = differ {
            return Err(format!("step {} after {:?}: {}", step, decoded, d));
        }
        /* The same entries on a replica: the same state, the same digests */
        replica.apply(index, &bytes);
        if let Some(rs) = restored.as_mut() {
            rs.apply(index, &bytes);
        }
        if s.checks().latest() != replica.checks().latest() {
            return Err(format!("step {}: the replica's newest digest is not the state's", step));
        }
        if let Some((at, digest)) = s.checks().latest() {
            if at == index && digest != s.digest() {
                return Err(format!("step {}: the checkpoint's digest is not the state's own", step));
            }
        }
        match (s.checks().diverged(), diverged_expected) {
            (None, None) => {}
            (Some(d), Some(at)) if d.at == at => {}
            (d, want) => return Err(format!("step {}: diverged {:?}, expected at {:?}", step, d, want)),
        }
        if r.chance(5) && s != replica {
            return Err(format!("step {}: the replica's state is not the state", step));
        }
        /* Its snapshot read back as itself, in pieces as in one */
        if r.chance(6) {
            let _big = Exempt::new();
            let whole = s.encode();
            let pieced = written(&s, r.range(1, 700) as usize);
            if pieced != whole {
                return Err(format!("step {}: the snapshot written in pieces is not the snapshot", step));
            }
            let back = State::decode(&whole).map_err(|e| format!("step {}: its own snapshot refused: {:?}", step, e))?;
            if back != s || back.digest() != s.digest() {
                return Err(format!("step {}: its snapshot read back as another state", step));
            }
            if restored.is_none() && r.chance(30) {
                restored = Some(back);
            }
            /* Spoiled: refused, or a state that reads back the same */
            let mut spoiled = whole.clone();
            for _ in 0..r.range(1, 4) {
                if spoiled.is_empty() {
                    break;
                }
                let at = r.below(spoiled.len() as u64) as usize;
                if r.chance(15) {
                    /* A number made 0: eight bytes that read as a small one
                       -- a counter, a count, an id -- which a random spot
                       seldom is */
                    let small: Vec<usize> = (0..spoiled.len().saturating_sub(8)).filter(|&i| {
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&spoiled[i..i + 8]);
                        let v = u64::from_le_bytes(b);
                        v > 0 && v < 1 << 20
                    }).collect();
                    if let Some(i) = r.pick(&small) {
                        spoiled[i..i + 8].fill(0);
                    }
                } else if r.chance(30) {
                    /* A short run of zeros: how a count, an incarnation or
                       a number becomes 0, which random bytes seldom make */
                    let end = (at + 1 + r.below(4) as usize).min(spoiled.len());
                    spoiled[at..end].fill(0);
                } else {
                    spoiled[at] = r.below(256) as u8;
                }
            }
            if r.chance(20) {
                spoiled.truncate(r.below(spoiled.len() as u64 + 1) as usize);
            }
            if let Ok(mut t) = State::decode(&spoiled) {
                if let Some(d) = fsck(&t) {
                    return Err(format!("step {}: a spoiled snapshot taken that breaks a rule: {}", step, d));
                }
                let again = State::decode(&t.encode())
                    .map_err(|e| format!("step {}: a spoiled snapshot taken, written again and refused: {:?}", step, e))?;
                if again != t {
                    return Err(format!("step {}: a spoiled snapshot taken reads back as another", step));
                }
                /* Taken, it is a state like any: it keeps every rule through
                   more commands -- a rule its check let pass would break one */
                for k in 0..r.range(1, 20) {
                    let c = command(&mut r, &t, &m, &mut fresh);
                    t.apply(1_000_000 + step * 32 + k, &c.encode());
                }
                State::decode(&t.encode()).map_err(|e| {
                    format!("step {}: a spoiled snapshot taken broke a rule a few commands on: {:?}", step, e)
                })?;
                if let Some(d) = fsck(&t) {
                    return Err(format!("step {}: a spoiled snapshot taken broke a rule a few commands on: {}", step, d));
                }
            }
        }
    }
    if let Some(rs) = &restored {
        if *rs != s {
            return Err(String::from("the state restored part way is not the state at the end"));
        }
    }
    if s != replica {
        return Err(String::from("the replica's state is not the state at the end"));
    }
    if std::env::var_os("NBS_META_STATS").is_some() {
        eprintln!("meta seed {}: {} volumes, {} chunks, {} cs, checks held {} unheld {}, answers {:?}", seed,
                  s.volumes().len(), s.live_chunks(), s.cs().len(), s.checks().held, s.checks().unheld, answers);
    }
    let _ = CHECK_EVERY;
    Ok(())
}
