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
//!   of the leader's held, and a false one said as a divergence;
//! - snapshots, images and clones (N10): each chunk known by an identity of
//!   the model's, shared by the maps a snapshot or a clone copied, counted by
//!   them and by a copy-up pending on it -- the metadata's number for it
//!   learned as it first shows, and held from then on, one number one chunk;
//!   a copy-up's new chunk in the old one's set, its room taken where every
//!   cs of the set has a slot left, named by the map once done.

use std::collections::{BTreeMap, BTreeSet};

use nbsmeta::repair::{self, Seen};
use nbsmeta::{Answer, Command, Kind, Refusal, State, CHECK_EVERY};
use raft::rsm::{Frozen, StateMachine};

use crate::heap::Exempt;
use crate::rng::{mix, Rng};

/// The hosts a run's members are drawn from.
const HOSTS: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
/// The volumes' names, and those snapshots and clones are drawn from (N10):
/// a command of one names any of them.
const VOLUMES: [&str; 10] = ["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "web.root", "db-1"];
const SNAPS: [&str; 4] = ["s0", "s1", "s2", "s3"];
const CLONES: [&str; 4] = ["k0", "k1", "k2", "k3"];
/// Copy-ups pending at once, at most.
const COPYING_MAX: usize = 4096;
/// The answer the model gives for a copy-up's new chunk, whose number the
/// metadata's free list chooses: taken from the metadata's own answer.
const FRESH: u32 = u32::MAX;
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

/// A cs's being there as the leader committed it (N9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MState {
    Up,
    Down,
    Out,
}

#[derive(Clone)]
struct MCs {
    host: String,
    devices: Vec<MDevice>,
    reserved: u64,
    state: MState,
}

impl MCs {
    fn slots(&self) -> u64 {
        self.devices.iter().filter(|d| d.active).map(|d| u64::from(d.slots)).sum()
    }

    /// Devices, none taking new chunks: its places to be taken by others.
    fn drained(&self) -> bool {
        !self.devices.is_empty() && self.slots() == 0
    }
}

/// A replica set: its three cs's, which of them are taken out and the last
/// version each was active at, its joiner (cs, place, the version it joined
/// at), its version and how many chunks are in it.
#[derive(Clone)]
struct MSet {
    cs: [u32; REPLICAS],
    down: [bool; REPLICAS],
    until: [u32; REPLICAS],
    joiner: Option<(u32, usize, u32)>,
    version: u32,
    chunks: u64,
}

impl MSet {
    fn clean(&self) -> bool {
        self.joiner.is_none() && !self.down.contains(&true)
    }

    fn active(&self) -> usize {
        self.down.iter().filter(|&&d| !d).count()
    }

    /// The cs's that hold room for each of its chunks: its three, and a
    /// joiner of a place not its own.
    fn holders(&self) -> Vec<u32> {
        let mut h = self.cs.to_vec();
        if let Some((j, place, _)) = self.joiner {
            if self.cs[place] != j {
                h.push(j);
            }
        }
        h
    }
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
    /// Each chunk's set, by its number.
    chunks: Vec<usize>,
    /// What it is (N10), and the volume it was made of: 0 for none.
    kind: Kind,
    origin: u32,
    /// Each chunk's identity in the model: the same where maps share it.
    keys: Vec<u64>,
}

/// A chunk in use, by the model's identity (N10).
#[derive(Clone)]
struct MChunk {
    set: usize,
    /// The maps that name it, and a copy-up pending on it as its new chunk.
    count: u32,
    /// The metadata's number and incarnation for it, once seen.
    real: Option<(u32, u32)>,
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
    /// Every set: made where three cs's have no clean one, the lowest empty
    /// one made again first (N9).
    sets: Vec<MSet>,
    /// The chunks in use, by identity (N10), and the next identity.
    mchunks: BTreeMap<u64, MChunk>,
    next_key: u64,
    /// The copy-ups pending: by volume and place, the new chunk and the old.
    pending: BTreeMap<(u32, u32), (u64, u64)>,
    /// A copy-up's new chunk just made: its number the metadata's answer.
    fresh: Option<u64>,
}

/// The sets the metadata keeps at most.
const SETS_MAX: usize = 1 << 16;

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
        self.mchunks.len() as u64
    }

    /// A chunk in set `set`, placed already -- the set's count and its
    /// holders' room taken: its identity.
    fn key_for(&mut self, set: usize) -> u64 {
        let k = self.next_key;
        self.next_key += 1;
        self.mchunks.insert(k, MChunk { set, count: 1, real: None });
        k
    }

    /// One of chunk `key`'s counts given back: at none, the chunk freed, its
    /// set's room given back.
    fn release(&mut self, key: u64) {
        let c = self.mchunks.get_mut(&key).expect("a chunk in use");
        c.count -= 1;
        if c.count == 0 {
            let set = c.set;
            self.mchunks.remove(&key);
            self.give_back(&[set]);
        }
    }

    /// Volume `id`'s copy-ups pending given up: their new chunks freed.
    fn give_up(&mut self, id: u32) {
        let gone: Vec<u64> = self.pending.range((id, 0)..=(id, u32::MAX)).map(|(_, &(to, _))| to).collect();
        self.pending.retain(|&(v, _), _| v != id);
        for to in gone {
            self.release(to);
        }
    }

    /// Chunk `key`'s answer: its number and incarnation, as seen.
    fn answer_of(&self, key: u64) -> Answer {
        match self.mchunks.get(&key).and_then(|c| c.real) {
            Some((chunk, incarnation)) => Answer::Chunk { chunk, incarnation },
            None => Answer::Chunk { chunk: FRESH, incarnation: 0 },
        }
    }

    /// The chunk the metadata numbers `chunk`, if one of the model's is.
    fn key_of(&self, chunk: u32) -> Option<u64> {
        self.mchunks.iter().find(|(_, c)| c.real.is_some_and(|(n, _)| n == chunk)).map(|(&k, _)| k)
    }

    /// Volume `name`'s number and the chunk its map has at `place`, as its
    /// writer at `generation` asks: the crate's order of refusals.
    fn writers_place(&self, name: &str, place: u32, generation: u64) -> Result<(u32, u64), Refusal> {
        if !self.init {
            return Err(Refusal::NotInitialized);
        }
        let v = self.vols.get(name).ok_or(Refusal::NoSuchVolume)?;
        if v.kind != Kind::Volume {
            return Err(Refusal::ReadOnly);
        }
        if generation == 0 || v.writer.as_ref().map(|(_, g, _)| *g) != Some(generation) {
            return Err(Refusal::NotTheWriter);
        }
        let from = *v.keys.get(place as usize).ok_or(Refusal::BadVolume)?;
        Ok((v.id, from))
    }

    /// A volume `name` of kind `kind` made of `of`: its chunks shared, each
    /// counted once more, no room taken.
    fn share(&mut self, of: &MVol, name: &str, host: &str, kind: Kind) -> Result<Answer, Refusal> {
        if self.vols.len() >= VOLUMES_MAX {
            return Err(Refusal::TooMany);
        }
        for k in &of.keys {
            self.mchunks.get_mut(k).expect("a map's").count += 1;
        }
        let id = self.next_volume;
        self.next_volume += 1;
        self.vols.insert(String::from(name), MVol { id, size_mib: of.size_mib, host: String::from(host), generation: 0,
                                                   writer: None, readers: Vec::new(), chunks: of.chunks.clone(),
                                                   kind, origin: of.id, keys: of.keys.clone() });
        Ok(Answer::Volume { id })
    }

    /// The copy-up just made numbered as the metadata answered it: the answer
    /// the model gives then, or None where that number is another chunk's.
    fn bind_fresh(&mut self, chunk: u32, incarnation: u32) -> Option<Answer> {
        let k = self.fresh.take()?;
        if self.key_of(chunk).is_some() {
            return None;
        }
        self.mchunks.get_mut(&k)?.real = Some((chunk, incarnation));
        Some(Answer::Chunk { chunk, incarnation })
    }

    /// The metadata's numbers for the model's chunks learned, map by map and
    /// copy-up by copy-up -- and every one already learned held: one number
    /// one chunk, counted as the model counts it.
    fn learn(&mut self, s: &State) -> Option<String> {
        let mut seen: BTreeMap<u32, u64> = BTreeMap::new();
        let mut pairs: Vec<(u64, u32)> = Vec::new();
        for (name, mv) in &self.vols {
            let v = s.volume(name)?;
            for (place, (&c, &k)) in v.map.iter().zip(mv.keys.iter()).enumerate() {
                pairs.push((k, c));
                if let Some(&other) = seen.get(&c).filter(|&&o| o != k) {
                    return Some(format!("{}'s chunk {} at {} is the model's {} and {} both", name, c, place, k, other));
                }
                seen.insert(c, k);
            }
        }
        let real: Vec<((u32, u32), (u32, u32))> = s.copying().iter().map(|(&a, &b)| (a, b)).collect();
        if real.len() != self.pending.len() {
            return Some(format!("{} copy-ups pending, the model {}", real.len(), self.pending.len()));
        }
        for ((at, (to, from)), (mat, &(mto, mfrom))) in real.iter().zip(self.pending.iter()) {
            if at != mat {
                return Some(format!("a copy-up pending at {:?}, the model's at {:?}", at, mat));
            }
            pairs.push((mto, *to));
            pairs.push((mfrom, *from));
        }
        for (k, c) in pairs {
            let ch = s.chunks().get(c as usize)?;
            let mc = self.mchunks.get_mut(&k)?;
            match mc.real {
                None => mc.real = Some((c, ch.incarnation)),
                Some(r) if r != (c, ch.incarnation) => {
                    return Some(format!("the model's chunk {} is the metadata's {:?}, now {} of incarnation {}", k, r,
                                        c, ch.incarnation));
                }
                Some(_) => {}
            }
            if u32::from(ch.count) != mc.count || ch.set as usize != mc.set {
                return Some(format!("chunk {} counted {} in set {}, the model {} in {}", c, ch.count, ch.set, mc.count,
                                    mc.set));
            }
        }
        None
    }

    /// `n` chunks of volume `volume` from its `from`th, by the plan's rules.
    fn place(&self, volume: u32, home: &str, from: u64, n: u64) -> Placed {
        /* (cs, slots, reserved, free) of the cs's that take new chunks */
        let mut c: Vec<(u32, u64, u64, u64)> = Vec::new();
        for (i, x) in self.cs.iter().enumerate() {
            let slots = x.slots();
            if slots > 0 && self.is_member(&x.host) && x.state == MState::Up {
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

    /// The clean set -- no member down, no joiner -- of three cs's: the
    /// lowest-numbered.
    fn clean_set(&self, cs: &[u32; REPLICAS]) -> Option<usize> {
        (0..self.sets.len()).find(|&i| self.sets[i].cs == *cs && self.sets[i].clean())
    }

    /// Whether set `i` may be made again for a placement onto the three
    /// cs's of `used`: no chunk, no joiner, a version that can move on, and
    /// not the clean set of three the placement fills.
    fn spare(&self, i: usize, used: &BTreeSet<[u32; REPLICAS]>) -> bool {
        let s = &self.sets[i];
        s.chunks == 0 && s.joiner.is_none() && s.version < u32::MAX && !(s.clean() && used.contains(&s.cs))
    }

    /// The empty set made again first: the lowest-numbered spare one.
    fn empty_set(&self, used: &BTreeSet<[u32; REPLICAS]>) -> Option<usize> {
        (0..self.sets.len()).find(|&i| self.spare(i, used))
    }

    /// Whether the sets have room for the three cs's of `chunks` that have
    /// no clean set: a set each, made again or new.
    fn sets_for(&self, chunks: &[[u32; REPLICAS]]) -> Result<(), Refusal> {
        let used: BTreeSet<[u32; REPLICAS]> = chunks.iter().copied().collect();
        let fresh = used.iter().filter(|t| self.clean_set(t).is_none()).count();
        let empty = (0..self.sets.len()).filter(|&i| self.spare(i, &used)).count();
        if fresh > SETS_MAX.saturating_sub(self.sets.len()) + empty {
            return Err(Refusal::TooMany);
        }
        Ok(())
    }

    /// The chunks placed on `chunks`' three cs's, each into its set: the
    /// clean one, else an empty one made again on them -- its version on --
    /// else a new one.
    fn take(&mut self, chunks: &[[u32; REPLICAS]]) -> Vec<usize> {
        let used: BTreeSet<[u32; REPLICAS]> = chunks.iter().copied().collect();
        let mut out = Vec::new();
        for t in chunks {
            let id = match self.clean_set(t) {
                Some(i) => i,
                None => match self.empty_set(&used) {
                    Some(i) => {
                        let version = self.sets[i].version + 1;
                        self.sets[i] = MSet { cs: *t, down: [false; REPLICAS], until: [0; REPLICAS], joiner: None,
                                              version, chunks: 0 };
                        i
                    }
                    None => {
                        self.sets.push(MSet { cs: *t, down: [false; REPLICAS], until: [0; REPLICAS], joiner: None,
                                              version: 1, chunks: 0 });
                        self.sets.len() - 1
                    }
                },
            };
            self.sets[id].chunks += 1;
            for c in self.sets[id].holders() {
                self.cs[c as usize].reserved += 1;
            }
            out.push(id);
        }
        out
    }

    fn give_back(&mut self, chunks: &[usize]) {
        for &id in chunks {
            self.sets[id].chunks -= 1;
            for c in self.sets[id].holders() {
                self.cs[c as usize].reserved -= 1;
            }
        }
    }

    /// A cs down or out, from up: down in its sets but where it is the last
    /// active member, no joiner of any; from down to out nothing more.
    fn gone(&mut self, cs: u32, to: MState) -> Result<Answer, Refusal> {
        if !self.init {
            return Err(Refusal::NotInitialized);
        }
        let was = self.cs.get(cs as usize).ok_or(Refusal::NoSuchCs)?.state;
        if was == to || was == MState::Out {
            return Ok(Answer::Changed { sets: 0 });
        }
        if was == MState::Down {
            self.cs[cs as usize].state = to;
            return Ok(Answer::Changed { sets: 0 });
        }
        let touched = |s: &MSet| {
            s.joiner.is_some_and(|(j, _, _)| j == cs)
                || (s.active() > 1 && (0..REPLICAS).any(|k| s.cs[k] == cs && !s.down[k]))
        };
        if self.sets.iter().any(|s| touched(s) && s.version == u32::MAX) {
            return Err(Refusal::TooMany);
        }
        let (mut changed, mut released) = (0u32, 0u64);
        for s in self.sets.iter_mut() {
            if !touched(s) {
                continue;
            }
            if let Some((j, place, _)) = s.joiner.filter(|(j, _, _)| *j == cs) {
                if s.cs[place] != j {
                    released += s.chunks;
                }
                s.joiner = None;
            }
            if let Some(k) = (0..REPLICAS).find(|&k| s.cs[k] == cs && !s.down[k]) {
                if s.active() > 1 {
                    s.down[k] = true;
                    s.until[k] = s.version;
                }
            }
            s.version += 1;
            changed += 1;
        }
        let c = &mut self.cs[cs as usize];
        c.state = to;
        c.reserved -= released;
        Ok(Answer::Changed { sets: changed })
    }

    /// The join of set `set` by `cs` of `since` given up.
    fn unjoin(&mut self, set: usize, cs: u32, since: u32) -> Result<Answer, Refusal> {
        let s = self.sets.get_mut(set).ok_or(Refusal::NoSuchSet)?;
        let Some((_, place, _)) = s.joiner.filter(|&(j, _, si)| j == cs && si == since) else {
            return Err(Refusal::Stale);
        };
        if s.version == u32::MAX {
            return Err(Refusal::TooMany);
        }
        let elsewhere = s.cs[place] != cs;
        s.joiner = None;
        s.version += 1;
        let (version, chunks) = (s.version, s.chunks);
        if elsewhere {
            self.cs[cs as usize].reserved -= chunks;
        }
        Ok(Answer::Set { version })
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
                        self.cs.push(MCs { host: host.clone(), devices: Vec::new(), reserved: 0, state: MState::Up });
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
                    return if v.kind == Kind::Volume && v.size_mib == *size_mib {
                        Ok(Answer::Volume { id: v.id })
                    } else {
                        Err(NameTaken)
                    };
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
                let placed = p.result?;
                self.sets_for(&placed)?;
                let chunks = self.take(&placed);
                let keys = chunks.iter().map(|&set| self.key_for(set)).collect();
                self.next_volume += 1;
                self.vols.insert(name.clone(), MVol { id, size_mib: *size_mib, host: host.clone(), generation: 0,
                                                      writer: None, readers: Vec::new(), chunks, kind: Kind::Volume,
                                                      origin: 0, keys });
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
                for &k in &v.keys {
                    self.release(k);
                }
                self.give_up(v.id);
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
                if v.kind != Kind::Volume {
                    return Err(ReadOnly);
                }
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
                    let placed = p.result?;
                    self.sets_for(&placed)?;
                    let more = self.take(&placed);
                    let keys: Vec<u64> = more.iter().map(|&set| self.key_for(set)).collect();
                    let mv = self.vols.get_mut(name).expect("there");
                    mv.chunks.extend(more);
                    mv.keys.extend(keys);
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
                    if v.kind != Kind::Volume {
                        return Err(ReadOnly);
                    }
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
                if !v.chunks.contains(&(*set as usize)) {
                    return Err(NoSuchSet);
                }
                let s = self.sets.get(*set as usize).ok_or(NoSuchSet)?;
                /* The joiner silent to the writer: its join dropped */
                if let Some((_, _, since)) = s.joiner.filter(|(j, _, _)| j == cs) {
                    return self.unjoin(*set as usize, *cs, since);
                }
                let s = self.sets.get_mut(*set as usize).ok_or(NoSuchSet)?;
                let at = s.cs.iter().position(|c| c == cs).ok_or(NoSuchSet)?;
                if s.down[at] {
                    return Ok(Answer::Set { version: s.version });
                }
                if (0..REPLICAS).all(|i| i == at || s.down[i]) {
                    return Err(LastReplica);
                }
                if s.version == u32::MAX {
                    return Err(TooMany);
                }
                s.down[at] = true;
                s.until[at] = s.version;
                s.version += 1;
                Ok(Answer::Set { version: s.version })
            }
            Command::CsDown { cs } => self.gone(*cs, MState::Down),
            Command::CsOut { cs } => self.gone(*cs, MState::Out),
            Command::CsUp { cs } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                let c = self.cs.get(*cs as usize).ok_or(NoSuchCs)?;
                if c.state == MState::Up {
                    return Ok(Answer::Changed { sets: 0 });
                }
                if !self.is_member(&c.host) {
                    return Err(NotAMember);
                }
                self.cs[*cs as usize].state = MState::Up;
                Ok(Answer::Changed { sets: 0 })
            }
            Command::Join { set, place, cs, version } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                let s = self.sets.get(*set as usize).ok_or(NoSuchSet)?;
                if s.version != *version {
                    return Err(Stale);
                }
                if s.joiner.is_some() {
                    return Err(Joining);
                }
                let k = usize::from(*place);
                if k >= REPLICAS {
                    return Err(NoSuchSet);
                }
                let c = self.cs.get(*cs as usize).ok_or(NoSuchCs)?;
                if c.state != MState::Up {
                    return Err(NotUp);
                }
                if !self.is_member(&c.host) {
                    return Err(NotAMember);
                }
                let back = s.cs[k] == *cs;
                if back {
                    if !s.down[k] {
                        return Err(NotDown);
                    }
                } else {
                    if s.cs.contains(cs) {
                        return Err(AlreadyMember);
                    }
                    let m = &self.cs[s.cs[k] as usize];
                    let gone = m.state == MState::Out || !self.is_member(&m.host);
                    if !(s.down[k] && gone) && !m.drained() {
                        return Err(InPlace);
                    }
                    if c.slots().saturating_sub(c.reserved) < s.chunks {
                        return Err(NoRoom);
                    }
                }
                if *version == u32::MAX {
                    return Err(TooMany);
                }
                let chunks = s.chunks;
                let s = &mut self.sets[*set as usize];
                s.version += 1;
                s.joiner = Some((*cs, k, s.version));
                let since = s.version;
                if !back {
                    self.cs[*cs as usize].reserved += chunks;
                }
                Ok(Answer::Set { version: since })
            }
            Command::Joined { set, cs, since } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                let s = self.sets.get(*set as usize).ok_or(NoSuchSet)?;
                let Some((_, k, _)) = s.joiner.filter(|&(j, _, si)| j == *cs && si == *since) else {
                    return Err(Stale);
                };
                if s.version == u32::MAX {
                    return Err(TooMany);
                }
                let (old, chunks) = (s.cs[k], s.chunks);
                let mut members: Vec<(u32, bool, u32)> =
                    (0..REPLICAS).map(|i| if i == k { (*cs, false, 0) } else { (s.cs[i], s.down[i], s.until[i]) })
                        .collect();
                members.sort_by_key(|m| m.0);
                let s = &mut self.sets[*set as usize];
                for (i, m) in members.iter().enumerate() {
                    s.cs[i] = m.0;
                    s.down[i] = m.1;
                    s.until[i] = m.2;
                }
                s.joiner = None;
                s.version += 1;
                let version = s.version;
                if old != *cs {
                    self.cs[old as usize].reserved -= chunks;
                }
                Ok(Answer::Set { version })
            }
            Command::Unjoin { set, cs, since } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                self.unjoin(*set as usize, *cs, *since)
            }
            Command::ReplicaBad { set, cs, version } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                let s = self.sets.get(*set as usize).ok_or(NoSuchSet)?;
                if s.version != *version {
                    return Err(Stale);
                }
                if s.joiner.is_some() {
                    return Err(Joining);
                }
                let at = s.cs.iter().position(|c| c == cs).ok_or(NoSuchSet)?;
                if s.down[at] {
                    return Err(AlreadyDown);
                }
                if (0..REPLICAS).all(|i| i == at || s.down[i]) {
                    return Err(LastReplica);
                }
                if s.version == u32::MAX {
                    return Err(TooMany);
                }
                let s = &mut self.sets[*set as usize];
                s.down[at] = true;
                s.until[at] = s.version;
                s.version += 1;
                Ok(Answer::Set { version: s.version })
            }
            Command::Snapshot { volume, name, generation } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                if !m_volume_name(name) {
                    return Err(BadVolume);
                }
                let v = self.vols.get(volume).ok_or(NoSuchVolume)?.clone();
                if v.kind != Kind::Volume {
                    return Err(ReadOnly);
                }
                if let Some(e) = self.vols.get(name) {
                    return if e.kind == Kind::Snapshot && e.origin == v.id {
                        Ok(Answer::Volume { id: e.id })
                    } else {
                        Err(NameTaken)
                    };
                }
                if v.writer.as_ref().map_or(0, |(_, g, _)| *g) != *generation {
                    return Err(NotTheWriter);
                }
                self.share(&v, name, &v.host, Kind::Snapshot)
            }
            Command::Freeze { name } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                let v = self.vols.get(name).ok_or(NoSuchVolume)?;
                match v.kind {
                    Kind::Image => return Ok(Answer::Volume { id: v.id }),
                    Kind::Snapshot => return Err(ReadOnly),
                    Kind::Volume => {}
                }
                if v.writer.is_some() || !v.readers.is_empty() {
                    return Err(Attached);
                }
                let id = v.id;
                self.give_up(id);
                self.vols.get_mut(name).expect("there").kind = Kind::Image;
                Ok(Answer::Volume { id })
            }
            Command::Clone { from, name, host } => {
                if !self.init {
                    return Err(NotInitialized);
                }
                if !m_volume_name(name) {
                    return Err(BadVolume);
                }
                if !self.is_member(host) {
                    return Err(NotAMember);
                }
                let v = self.vols.get(from).ok_or(NoSuchVolume)?.clone();
                if v.kind == Kind::Volume {
                    return Err(NotFrozen);
                }
                if let Some(e) = self.vols.get(name) {
                    return if e.kind == Kind::Volume && e.origin == v.id {
                        Ok(Answer::Volume { id: e.id })
                    } else {
                        Err(NameTaken)
                    };
                }
                self.share(&v, name, host, Kind::Volume)
            }
            Command::CopyUp { name, place, generation } => {
                let (id, from) = self.writers_place(name, *place, *generation)?;
                if let Some(&(to, _)) = self.pending.get(&(id, *place)) {
                    return Ok(self.answer_of(to));
                }
                let ch = self.mchunks.get(&from).expect("a map's").clone();
                if ch.count <= 1 {
                    return Ok(self.answer_of(from));
                }
                if self.pending.len() >= COPYING_MAX || self.live() >= CHUNKS_MAX {
                    return Err(TooMany);
                }
                /* Thin: a slot of every cs of the set, the room kept to rebuild
                   a host's included */
                let holders = self.sets[ch.set].holders();
                if holders.iter().any(|&c| self.cs[c as usize].slots().saturating_sub(self.cs[c as usize].reserved) == 0) {
                    return Err(NoRoom);
                }
                self.sets[ch.set].chunks += 1;
                for c in holders {
                    self.cs[c as usize].reserved += 1;
                }
                let to = self.key_for(ch.set);
                self.pending.insert((id, *place), (to, from));
                self.fresh = Some(to);
                Ok(Answer::Chunk { chunk: FRESH, incarnation: 0 })
            }
            Command::CopyDone { name, place, generation, chunk } => {
                let (id, from) = self.writers_place(name, *place, *generation)?;
                if self.mchunks.get(&from).and_then(|c| c.real).map(|r| r.0) == Some(*chunk) {
                    return Ok(self.answer_of(from));
                }
                let to = match self.pending.get(&(id, *place)) {
                    Some(&(to, was)) if was == from
                        && self.mchunks.get(&to).and_then(|c| c.real).map(|r| r.0) == Some(*chunk) => to,
                    _ => return Err(Stale),
                };
                self.pending.remove(&(id, *place));
                let v = self.vols.values_mut().find(|v| v.id == id).expect("there");
                v.keys[*place as usize] = to;
                self.release(from);
                Ok(self.answer_of(to))
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
        let state = match c.state {
            nbsmeta::CsState::Up => MState::Up,
            nbsmeta::CsState::Down => MState::Down,
            nbsmeta::CsState::Out => MState::Out,
        };
        if c.id as usize != i || c.host != mc.host || c.reserved != mc.reserved || c.devices.len() != mc.devices.len()
            || state != mc.state
        {
            return Some(format!("cs {}: {} {:?} reserved {} of {} devices, the model {} {:?} reserved {} of {}", i,
                                c.host, state, c.reserved, c.devices.len(), mc.host, mc.state, mc.reserved,
                                mc.devices.len()));
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
        let joiner = set.joiner.map(|j| (j.cs, usize::from(j.place), j.since));
        if set.cs != ms.cs || down != ms.down || set.active_until != ms.until || joiner != ms.joiner
            || set.version != ms.version || u64::from(set.chunks) != ms.chunks
        {
            return Some(format!("set {}: cs's {:?}, out {:?} until {:?}, joiner {:?}, version {}, {} chunks; the model \
                                 {:?}, {:?} until {:?}, {:?}, {}, {}", i, set.cs, down, set.active_until, joiner,
                                set.version, set.chunks, ms.cs, ms.down, ms.until, ms.joiner, ms.version, ms.chunks));
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
            || v.kind != mv.kind || v.origin != mv.origin
        {
            return Some(format!("volume {}: {} {} MiB on {} gen {} writer {:?} readers {:?} {} chunks; the model {} {} \
                                 MiB on {} gen {} writer {:?} readers {:?} {} chunks", name, v.id, v.size_mib, v.host,
                                v.generation, writer, v.readers, v.map.len(), mv.id, mv.size_mib, mv.host,
                                mv.generation, mv.writer, mv.readers, mv.chunks.len()));
        }
        for (k, (&c, &want)) in v.map.iter().zip(mv.chunks.iter()).enumerate() {
            let Some(ch) = s.chunks().get(c as usize) else { return Some(format!("{}'s chunk {} is none", name, k)) };
            let Some(set) = s.sets().get(ch.set as usize) else {
                return Some(format!("{}'s chunk {} in no set", name, k));
            };
            if ch.is_free() || ch.set as usize != want {
                return Some(format!("{}'s chunk {} in set {}, the model's {}", name, k, ch.set, want));
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
    /* An image and a snapshot written by none, a snapshot made of a volume,
       none made of itself (N10) */
    for v in s.volumes().iter() {
        if (v.kind != Kind::Volume && v.writer.is_some()) || v.origin == v.id || (v.kind == Kind::Snapshot && v.origin == 0) {
            return Some(format!("volume {} a {:?} of {} with writer {:?}", v.name, v.kind, v.origin, v.writer));
        }
    }
    /* A copy-up pending (N10): its new chunk counted by it alone, in the old
       one's set, the old one where the volume's map has it */
    for (&(vol, place), &(to, from)) in s.copying() {
        let at = s.volumes().iter().find(|v| v.id == vol).and_then(|v| v.map.get(place as usize).copied());
        let sets = (chunks.get(to as usize).map(|c| c.set), chunks.get(from as usize).map(|c| c.set));
        if at != Some(from) || sets.0.is_none() || sets.0 != sets.1 {
            return Some(format!("copy-up of volume {}'s place {}: {} from {}, its map has {:?}, sets {:?}", vol, place,
                                to, from, at, sets));
        }
        match named.get_mut(to as usize) {
            Some(n) => *n += 1,
            None => return Some(format!("copy-up of volume {}'s place {} into chunk {}, past the table", vol, place, to)),
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
        let me = i as u32;
        /* A member's chunks, and those of a set it joins in a place not its
           own -- a member back holds its room already */
        let replicas: u64 = s.sets().iter()
            .filter(|x| x.cs.contains(&me)
                || x.joiner.is_some_and(|j| j.cs == me && x.cs.get(usize::from(j.place)) != Some(&me)))
            .map(|x| u64::from(x.chunks)).sum();
        if replicas != c.reserved {
            return Some(format!("cs {} reserved {}, its sets have {} chunks", i, c.reserved, replicas));
        }
    }
    for (i, set) in s.sets().iter().enumerate() {
        if !set.replicas.contains(&nbsmeta::Replica::Active) {
            return Some(format!("set {} with no active member", i));
        }
        /* A member down was active until an earlier version; an active one
           has none */
        for k in 0..REPLICAS {
            let until = set.active_until[k];
            let ok = if set.replicas[k] == nbsmeta::Replica::Down { until != 0 && until < set.version } else { until == 0 };
            if !ok {
                return Some(format!("set {}'s member {} {:?} active until {} at version {}", i, k, set.replicas[k],
                                    until, set.version));
            }
        }
        if let Some(j) = set.joiner {
            let k = usize::from(j.place);
            if k >= REPLICAS || (set.cs[k] == j.cs) != (set.cs.contains(&j.cs))
                || (set.cs[k] == j.cs && set.replicas[k] != nbsmeta::Replica::Down)
            {
                return Some(format!("set {}'s joiner {:?}: an active member's own place, or another member's", i, j));
            }
            if j.cs as usize >= s.cs().len() || j.since == 0 || j.since > set.version {
                return Some(format!("set {}'s joiner {:?}: of no cs, or joined at no version of the set's {}", i, j,
                                    set.version));
            }
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

/// A volume with copy-ups pending, by name (N10).
fn pending_volume(r: &mut Rng, s: &State, m: &Model) -> Option<String> {
    let ids: Vec<u32> = s.copying().keys().map(|&(v, _)| v).collect();
    let id = r.pick(&ids)?;
    m.vols.iter().find(|(_, v)| v.id == id).map(|(n, _)| n.clone())
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
    let volume = |r: &mut Rng| {
        let k = r.below((VOLUMES.len() + SNAPS.len() + CLONES.len()) as u64) as usize;
        VOLUMES.iter().chain(SNAPS.iter()).chain(CLONES.iter()).nth(k).copied().unwrap_or("v0").to_string()
    };
    if !m.init {
        *fresh += 1;
        return Command::Init { cluster: [7; 16], name: String::from("nbs"), host: host(r), member: *fresh };
    }
    match r.below(150) {
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
            let name = if r.chance(3) {
                String::from("Bad_Name")
            } else if r.chance(90) {
                VOLUMES[r.below(VOLUMES.len() as u64) as usize].to_string()
            } else {
                volume(r)
            };
            Command::VolumeCreate { name, size_mib, host: member(r) }
        }
        50..=59 => {
            let name = match pending_volume(r, s, m) {
                Some(n) if r.chance(30) => n,
                _ => volume(r),
            };
            Command::VolumeDelete { name }
        }
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
            let name = match pending_volume(r, s, m) {
                Some(n) if r.chance(30) => n,
                _ => volume(r),
            };
            let (h, generation) = match m.vols.get(&name).and_then(|v| v.writer.clone()) {
                Some((h, g, _)) if r.chance(70) => (h, g),
                _ => (member(r), r.pick(&[0u64, 1, 2, 7]).unwrap_or(0)),
            };
            Command::Detach { name, host: h, generation }
        }
        /* A replica taken out: mostly of a set the volume has chunks in, and
           of one of its members or now and then its joiner -- a writer finds
           a joiner silent as it finds a member; else of any set there is or
           one past the table, and a cs that is none of its */
        87..=89 => {
            let name = match m.vols.keys().nth(r.below(m.vols.len() as u64) as usize) {
                Some(n) if r.chance(75) => n.clone(),
                _ => volume(r),
            };
            let vol = m.vols.get(&name);
            let sets = m.sets.len() as u64;
            let set = match vol.and_then(|v| r.pick(&v.chunks)) {
                Some(s) if r.chance(70) => s as u32,
                _ if sets == 0 || r.chance(10) => r.below(sets + 2) as u32,
                _ => r.below(sets) as u32,
            };
            let cs = match m.sets.get(set as usize) {
                Some(ms) if ms.joiner.is_some() && r.chance(30) => ms.joiner.map_or(0, |(j, _, _)| j),
                Some(ms) if r.chance(85) => ms.cs[r.below(REPLICAS as u64) as usize],
                _ => r.below(m.cs.len() as u64 + 1) as u32,
            };
            /* As the volume's writer mostly, or one fenced since */
            let generation = match vol.and_then(|v| v.writer.clone()) {
                Some((_, g, _)) if r.chance(80) => g,
                _ => r.below(4),
            };
            Command::ReplicaDown { set, cs, refused: r.chance(30), name, generation }
        }
        90..=99 => {
            /* The leader's digest: the true one of the newest checkpoint, or
               of none the state has */
            match s.checks().latest() {
                Some((at, digest)) if r.chance(80) => Command::Digest { at, digest },
                _ => Command::Digest { at: r.below(10_000), digest: [r.below(256) as u8; 32] },
            }
        }
        /* A cs down, up or out (N9): one there is, or none */
        100..=103 => {
            let cs = r.below(m.cs.len() as u64 + 1) as u32;
            match r.below(3) {
                0 => Command::CsDown { cs },
                1 => Command::CsUp { cs },
                _ => Command::CsOut { cs },
            }
        }
        /* A join: mostly of a set with a place to fill, at its version, by
           its own member back or another cs -- else anything */
        104..=111 => {
            let sets = m.sets.len() as u64;
            let wanting: Vec<usize> = (0..m.sets.len()).filter(|&i| {
                let ms = &m.sets[i];
                ms.joiner.is_none() && (ms.down.contains(&true) || ms.cs.iter().any(|&c| m.cs[c as usize].drained()))
            }).collect();
            let set = match r.pick(&wanting) {
                Some(i) if r.chance(85) => i,
                _ => r.below(sets + 2) as usize,
            };
            let ms = m.sets.get(set);
            let place = match ms {
                Some(ms) if r.chance(80) => (0..REPLICAS)
                    .find(|&k| ms.down[k] || m.cs[ms.cs[k] as usize].drained()).unwrap_or(0) as u8,
                _ => r.below(4) as u8,
            };
            let cs = match ms {
                Some(ms) if r.chance(40) && usize::from(place) < REPLICAS => ms.cs[usize::from(place)],
                _ => r.below(m.cs.len() as u64 + 1) as u32,
            };
            let version = match ms {
                Some(ms) if r.chance(85) => ms.version,
                Some(ms) => ms.version.saturating_sub(1),
                None => 1,
            };
            Command::Join { set: set as u32, place, cs, version }
        }
        /* A join's end or give-up: of one under way, or not */
        112..=115 => {
            let joining: Vec<usize> = (0..m.sets.len()).filter(|&i| m.sets[i].joiner.is_some()).collect();
            let (set, cs, since) = match r.pick(&joining) {
                Some(i) if r.chance(85) => {
                    let (j, _, since) = m.sets[i].joiner.expect("joining");
                    (i as u32, j, if r.chance(90) { since } else { since + 1 })
                }
                _ => (r.below(m.sets.len() as u64 + 1) as u32, r.below(m.cs.len() as u64 + 1) as u32, r.below(5) as u32),
            };
            if r.chance(70) {
                Command::Joined { set, cs, since }
            } else {
                Command::Unjoin { set, cs, since }
            }
        }
        /* A snapshot (N10): mostly of a volume, at its writer's generation or
           none, named as snapshots are -- else anything */
        120..=123 => {
            let of: Vec<&String> = m.vols.iter().filter(|(_, v)| v.kind == Kind::Volume).map(|(n, _)| n).collect();
            let volume_name = match r.pick(&of) {
                Some(n) if r.chance(85) => n.clone(),
                _ => volume(r),
            };
            let generation = match m.vols.get(&volume_name).and_then(|v| v.writer.as_ref()) {
                Some((_, g, _)) if r.chance(85) => *g,
                Some(_) => 0,
                None if r.chance(90) => 0,
                None => r.below(4),
            };
            let name = if r.chance(3) { String::from("Bad_Name") } else { SNAPS[r.below(SNAPS.len() as u64) as usize].to_string() };
            Command::Snapshot { volume: volume_name, name, generation }
        }
        124..=125 => {
            let name = match pending_volume(r, s, m) {
                Some(n) if r.chance(50) => n,
                _ => volume(r),
            };
            Command::Freeze { name }
        }
        /* A clone: mostly of an image or a snapshot, named as clones are */
        126..=129 => {
            let of: Vec<&String> = m.vols.iter().filter(|(_, v)| v.kind != Kind::Volume).map(|(n, _)| n).collect();
            let from = match r.pick(&of) {
                Some(n) if r.chance(85) => n.clone(),
                _ => volume(r),
            };
            let name = if r.chance(3) { String::from("Bad_Name") } else { CLONES[r.below(CLONES.len() as u64) as usize].to_string() };
            Command::Clone { from, name, host: member(r) }
        }
        /* A copy-up's first step: mostly of a volume with a writer, at its
           generation, of a place of its map -- mostly one it shares */
        130..=135 => {
            let writing: Vec<(&String, &MVol)> = m.vols.iter().filter(|(_, v)| v.writer.is_some()).collect();
            let (name, keys, generation) = match r.pick(&writing) {
                Some((n, v)) if r.chance(85) => (n.clone(), v.keys.clone(), v.writer.as_ref().map_or(0, |w| w.1)),
                _ => (volume(r), Vec::new(), r.below(4)),
            };
            let len = keys.len() as u64;
            let shared: Vec<u64> = (0..len).filter(|&p| m.mchunks.get(&keys[p as usize]).is_some_and(|c| c.count > 1))
                .collect();
            let place = match r.pick(&shared) {
                Some(p) if r.chance(70) => p,
                _ if r.chance(95) => r.below(len.max(1)),
                _ => len + r.below(3),
            } as u32;
            let generation = if r.chance(90) { generation } else { generation + 1 + r.below(2) };
            Command::CopyUp { name, place, generation }
        }
        /* Its second: mostly of a copy-up pending, named by its chunk -- else
           of any place, any chunk */
        136..=141 => {
            let pending: Vec<((u32, u32), (u32, u32))> = s.copying().iter().map(|(&a, &b)| (a, b)).collect();
            match r.pick(&pending) {
                Some(((vol, place), (to, from))) if r.chance(80) => {
                    let (name, generation) = m.vols.iter().find(|(_, v)| v.id == vol)
                        .map_or((volume(r), 0), |(n, v)| (n.clone(), v.writer.as_ref().map_or(0, |w| w.1)));
                    let chunk = match r.below(10) {
                        0 => from,
                        1 => r.below(s.chunks().len() as u64 + 1) as u32,
                        _ => to,
                    };
                    let generation = if r.chance(90) { generation } else { generation + 1 };
                    Command::CopyDone { name, place, generation, chunk }
                }
                _ => Command::CopyDone { name: volume(r), place: r.below(4) as u32, generation: r.below(4),
                                         chunk: r.below(s.chunks().len() as u64 + 2) as u32 },
            }
        }
        /* A member the scrub found unlike the others (N9e): mostly of a set
           with three active members and no joiner, at its version -- else
           anything */
        116..=119 => {
            let sets = m.sets.len() as u64;
            let whole: Vec<usize> = (0..m.sets.len())
                .filter(|&i| m.sets[i].joiner.is_none() && !m.sets[i].down.contains(&true)).collect();
            let set = match r.pick(&whole) {
                Some(i) if r.chance(85) => i,
                _ => r.below(sets + 2) as usize,
            };
            let ms = m.sets.get(set);
            let cs = match ms {
                Some(ms) if r.chance(85) => ms.cs[r.below(REPLICAS as u64) as usize],
                _ => r.below(m.cs.len() as u64 + 1) as u32,
            };
            let version = match ms {
                Some(ms) if r.chance(85) => ms.version,
                Some(ms) => ms.version.saturating_sub(1),
                None => 1,
            };
            Command::ReplicaBad { set: set as u32, cs, version }
        }
        /* What the controller proposes, as a leader seeing the cs's so */
        _ => {
            let seen: Vec<Seen> = (0..s.cs().len() as u32).map(|cs| {
                let answering = r.chance(70);
                let silent_ms = if answering { 0 } else { r.pick(&[1_000u64, 6_000, 700_000]).unwrap_or(6_000) };
                Seen { cs, answering, silent_ms, maintenance: r.chance(10) }
            }).collect();
            let mut plan = repair::plan(s, &seen, repair::After::default());
            if plan.is_empty() {
                Command::CsUp { cs: r.below(m.cs.len() as u64 + 1) as u32 }
            } else {
                let at = r.below(plan.len() as u64) as usize;
                plan.swap_remove(at)
            }
        }
    }
}

/// The controller's progress (N9): from state `s`, every member's cs
/// answering but those `gone` for good, the controller's plans applied turn
/// after turn and every join under way ended at the next -- its copy done --
/// until it proposes nothing: within so many turns, and then every set with
/// a place to fill has none it could fill -- no member back that was not
/// joined, no cs with room that could have taken another's place.
fn settle(s: &State, gone: &[bool]) -> Option<String> {
    let mut t = s.clone();
    let seen: Vec<Seen> = (0..t.cs().len() as u32).map(|cs| {
        let g = gone.get(cs as usize).copied().unwrap_or(false);
        Seen { cs, answering: !g, silent_ms: if g { repair::OUT_AFTER_MS } else { 0 }, maintenance: false }
    }).collect();
    let turns = 4 * (t.sets().len() + 4);
    let mut index = 2_000_000u64;
    for _ in 0..turns {
        let cmds = repair::plan(&t, &seen, repair::After::default());
        for c in &cmds {
            index += 1;
            if let Some(Answer::Refused(why)) = Answer::decode(&t.apply(index, &c.encode())) {
                return Some(format!("the controller proposed {:?}, refused: {:?}", c, why));
            }
        }
        /* Every join under way ends: its copy done */
        let ends: Vec<Command> = t.sets().iter().enumerate()
            .filter_map(|(id, set)| set.joiner.map(|j| Command::Joined { set: id as u32, cs: j.cs, since: j.since }))
            .collect();
        for c in &ends {
            index += 1;
            if let Some(Answer::Refused(why)) = Answer::decode(&t.apply(index, &c.encode())) {
                return Some(format!("a join's end {:?} refused: {:?}", c, why));
            }
        }
        if cmds.is_empty() && ends.is_empty() {
            return settled(&t, gone).or_else(|| fsck(&t)).or_else(|| {
                State::decode(&t.encode()).err().map(|e| format!("the settled state's snapshot refused: {:?}", e))
            });
        }
    }
    Some(format!("the controller still proposing after {} turns", turns))
}

/// What the controller left undone, said: a member down whose cs answers,
/// up, a member host's and not drained, never joined back; a place of a
/// member gone or drained that a cs with room -- up, answering, a member
/// host's, not drained, no member of the set -- could have taken; a join
/// still under way; a member host's cs answering but not up.
fn settled(t: &State, gone: &[bool]) -> Option<String> {
    let g = |c: u32| gone.get(c as usize).copied().unwrap_or(false);
    let usable = |c: &nbsmeta::Cs| c.state == nbsmeta::CsState::Up && t.member(&c.host).is_some() && !g(c.id) && !c.draining();
    for c in t.cs() {
        if t.member(&c.host).is_some() && !g(c.id) && c.state != nbsmeta::CsState::Up {
            return Some(format!("cs {} answering but {:?} at the end", c.id, c.state));
        }
        /* Silent past out_after, or of a host no member: out */
        if (g(c.id) || t.member(&c.host).is_none()) && c.state != nbsmeta::CsState::Out {
            return Some(format!("cs {} gone for good but {:?} at the end", c.id, c.state));
        }
    }
    for (id, set) in t.sets().iter().enumerate() {
        if set.chunks == 0 {
            continue;
        }
        if set.joiner.is_some() {
            return Some(format!("set {} still joining at the end", id));
        }
        for k in 0..REPLICAS {
            let Some(c) = t.cs().get(set.cs[k] as usize) else { return Some(format!("set {} of no cs", id)) };
            let down = set.replicas[k] == nbsmeta::Replica::Down;
            if down && usable(c) {
                return Some(format!("set {}'s member {} down, its cs back, never joined again", id, c.id));
            }
            let gone_for_good = c.state == nbsmeta::CsState::Out || t.member(&c.host).is_none();
            if (down && gone_for_good) || c.draining() {
                let taker = t.cs().iter().find(|o| usable(o) && !set.cs.contains(&o.id) && o.free() >= u64::from(set.chunks));
                if let Some(o) = taker {
                    return Some(format!("set {}'s place {} ({}) left to fill, which cs {} had the room for", id, k,
                                        if down { "gone" } else { "drained" }, o.id));
                }
            }
        }
    }
    None
}

/// A set's joiner in the snapshot `bytes` of `s` made one no set can have:
/// of a place past the three, or another member's; the cs of a member
/// active; joined at 0, or at a version to come. The joiner is found by its
/// own values, as NBM5 writes them -- 1, its cs, its place and the version
/// it joined at, then the set's version and count of chunks -- and the rest
/// of the snapshot is left whole, so that the check of joiners is the one
/// to refuse it: bytes spoiled at random reach it once in a long while. A
/// format that writes a joiner otherwise finds none here, and the
/// `joiner_checked` mutant then says so. False where no set has a joiner.
fn spoil_joiner(r: &mut Rng, s: &State, bytes: &mut [u8]) -> bool {
    let joining: Vec<usize> = s.sets().iter().enumerate().filter(|(_, x)| x.joiner.is_some()).map(|(i, _)| i).collect();
    let Some(set) = r.pick(&joining).and_then(|i| s.sets().get(i)) else { return false };
    let Some(j) = set.joiner else { return false };
    let mut needle = vec![1u8];
    needle.extend_from_slice(&j.cs.to_le_bytes());
    needle.push(j.place);
    needle.extend_from_slice(&j.since.to_le_bytes());
    needle.extend_from_slice(&set.version.to_le_bytes());
    needle.extend_from_slice(&set.chunks.to_le_bytes());
    let Some(at) = bytes.windows(needle.len()).position(|w| w == needle.as_slice()) else { return false };
    let (cs_at, place_at, since_at) = (at + 1, at + 5, at + 6);
    let k = usize::from(j.place);
    match r.below(4) {
        0 => bytes[place_at] = 3 + r.below(253) as u8,
        /* A member back at another member's place */
        1 if set.cs[k] == j.cs => bytes[place_at] = ((k + 1 + r.below(2) as usize) % REPLICAS) as u8,
        2 => {
            let since = if r.chance(50) { 0 } else { set.version.saturating_add(1 + r.below(8) as u32) };
            bytes[since_at..since_at + 4].copy_from_slice(&since.to_le_bytes());
        }
        _ => {
            let active: Vec<u32> = (0..REPLICAS)
                .filter(|&m| set.replicas[m] == nbsmeta::Replica::Active && set.cs[m] != j.cs)
                .map(|m| set.cs[m])
                .collect();
            let Some(c) = r.pick(&active) else { return false };
            bytes[cs_at..cs_at + 4].copy_from_slice(&c.to_le_bytes());
        }
    }
    true
}

/// The state's counters spoiled in its snapshot: the next device's number,
/// or the next volume's, made 0 -- which applying never leaves them at, so
/// that a device or a volume added after would be numbered 0. Found by their
/// values, as the snapshot's header writes them -- the next device's number,
/// the next volume's, the next check's index, the head of the free chunks
/// -- the model's and the state's own; the rest left whole, so that the
/// check of the counters is the one to refuse it. A format that writes them
/// otherwise finds none here, and the `counters_checked` mutant then says
/// so. False where they are not found.
fn spoil_counters(r: &mut Rng, s: &State, m: &Model, bytes: &mut [u8]) -> bool {
    let mut head = m.next_device.to_le_bytes().to_vec();
    head.extend_from_slice(&m.next_volume.to_le_bytes());
    let free = s.free_head().to_le_bytes();
    let Some(at) = bytes.windows(20).position(|w| w[..8] == head[..] && w[16..] == free) else { return false };
    let k = if r.chance(50) { at } else { at + 4 };
    bytes[k..k + 4].fill(0);
    true
}

/// A copy-up pending spoiled in the snapshot (N10): its new chunk made the
/// old one, its place another, or its old chunk one of another set. Found by
/// its values as NBM6 writes it -- the volume's number, the place, the new
/// chunk, the old, each four bytes -- the rest left whole, so that the check
/// of copy-ups pending is the one to refuse it. A format that writes them
/// otherwise finds none here, and the `copying_checked` mutant then says so.
fn spoil_copying(r: &mut Rng, s: &State, bytes: &mut [u8]) -> bool {
    let pending: Vec<((u32, u32), (u32, u32))> = s.copying().iter().map(|(&a, &b)| (a, b)).collect();
    let Some(((vol, place), (to, from))) = r.pick(&pending) else { return false };
    let mut needle = Vec::new();
    for x in [vol, place, to, from] {
        needle.extend_from_slice(&x.to_le_bytes());
    }
    let Some(at) = bytes.windows(16).position(|w| w == needle.as_slice()) else { return false };
    let other = (0..s.chunks().len() as u32)
        .find(|&c| s.chunks().get(c as usize).is_some_and(|x| !x.is_free() && Some(x.set) != s.chunks().get(from as usize).map(|f| f.set)));
    match r.below(3) {
        0 => bytes[at + 8..at + 12].copy_from_slice(&from.to_le_bytes()),
        1 => bytes[at + 4..at + 8].copy_from_slice(&place.wrapping_add(1).to_le_bytes()),
        _ => match other {
            Some(o) => bytes[at + 12..at + 16].copy_from_slice(&o.to_le_bytes()),
            None => return false,
        },
    }
    true
}

/// A volume's kind or origin spoiled in the snapshot (N10): one with a
/// writer made an image, a snapshot made of nothing, or one made of itself.
/// Found by its values as the snapshot writes it -- its number, its name,
/// its size, its kind -- the rest left whole, so that the check of
/// read-only volumes is the one to refuse it. A format that writes them
/// otherwise finds none here, and the `read_only_checked` mutant then says
/// so.
fn spoil_read_only(r: &mut Rng, s: &State, bytes: &mut [u8]) -> bool {
    let vols: Vec<usize> = (0..s.volumes().len()).collect();
    let Some(v) = r.pick(&vols).and_then(|i| s.volumes().get(i)) else { return false };
    let code = match v.kind {
        Kind::Volume => 0u8,
        Kind::Image => 1,
        Kind::Snapshot => 2,
    };
    let mut needle = v.id.to_le_bytes().to_vec();
    needle.push(v.name.len() as u8);
    needle.extend_from_slice(v.name.as_bytes());
    needle.extend_from_slice(&v.size_mib.to_le_bytes());
    needle.push(code);
    let Some(at) = bytes.windows(needle.len()).position(|w| w == needle.as_slice()) else { return false };
    let (kind_at, origin_at) = (at + needle.len() - 1, at + needle.len());
    if v.writer.is_some() {
        bytes[kind_at] = 1 + r.below(2) as u8;
    } else if v.kind == Kind::Snapshot && r.chance(50) {
        bytes[origin_at..origin_at + 4].copy_from_slice(&0u32.to_le_bytes());
    } else {
        bytes[origin_at..origin_at + 4].copy_from_slice(&v.id.to_le_bytes());
    }
    true
}

/// A free chunk's link spoiled in the snapshot: the next free chunk made one
/// in use, itself, or the list's end. Found by its values as the snapshot
/// writes a chunk -- its link, a count of none, the free flag, its
/// incarnation -- the rest left whole, so that the check of the free list is
/// the one to refuse it. A format that writes them otherwise finds none
/// here, and the `free_list_checked` mutant then says so.
fn spoil_free_list(r: &mut Rng, s: &State, bytes: &mut [u8]) -> bool {
    let free_at = |c: u32| s.chunks().get(c as usize).is_some_and(|x| x.is_free());
    let free: Vec<u32> = (0..s.chunks().len() as u32).filter(|&c| free_at(c)).collect();
    let used: Vec<u32> = (0..s.chunks().len() as u32).filter(|&c| !free_at(c)).collect();
    let Some(c) = r.pick(&free) else { return false };
    let Some(&ch) = s.chunks().get(c as usize) else { return false };
    let mut needle = ch.set.to_le_bytes().to_vec();
    needle.extend_from_slice(&0u16.to_le_bytes());
    needle.push(nbsmeta::CHUNK_FREE);
    needle.extend_from_slice(&ch.incarnation.to_le_bytes());
    let Some(at) = bytes.windows(needle.len()).position(|w| w == needle.as_slice()) else { return false };
    let next = match r.below(3) {
        0 => match r.pick(&used) {
            Some(u) => u,
            None => return false,
        },
        1 => c,
        _ => nbsmeta::NO_CHUNK,
    };
    if next == ch.set {
        return false;
    }
    bytes[at..at + 4].copy_from_slice(&next.to_le_bytes());
    true
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
            let (want, unfair) = m.apply(decoded.as_ref());
            /* A copy-up's new chunk numbered as the metadata answers: the
               number held from then on */
            let want = match (want, got) {
                (Answer::Chunk { chunk: FRESH, .. }, Answer::Chunk { chunk, incarnation }) => {
                    m.bind_fresh(chunk, incarnation).unwrap_or(want)
                }
                _ => want,
            };
            m.fresh = None;
            (want, unfair)
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
        let what = decoded.as_ref().map_or(String::from("bytes"), |c| {
            format!("{:?}", c).split([' ', '(', '{']).next().unwrap_or("").to_string()
        });
        let kind = match got {
            Answer::Refused(why) => format!("{} {:?}", what, why),
            other => format!("{} {}", what, format!("{:?}", other).split([' ', '(', '{']).next().unwrap_or("")),
        };
        *answers.entry(kind).or_default() += 1;
        let differ = {
            let _model = Exempt::new();
            differs(&s, &m).or_else(|| m.learn(&s)).or_else(|| fsck(&s)).or_else(|| incarnations(&s, &mut lives))
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
        /* The controller's progress from here, some hosts gone for good */
        if r.chance(2) {
            let _model = Exempt::new();
            let gone: Vec<bool> = (0..s.cs().len()).map(|_| r.chance(20)).collect();
            if let Some(d) = settle(&s, &gone) {
                return Err(format!("step {}: {}", step, d));
            }
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
            /* Spoiled: refused, or a state that reads back the same -- a
               joiner now and then, aimed at, the rest at random */
            let mut spoiled = whole.clone();
            let aimed = r.chance(25) && spoil_joiner(&mut r, &s, &mut spoiled);
            for _ in 0..if aimed { 0 } else { r.range(1, 4) } {
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
            if !aimed && r.chance(20) {
                spoiled.truncate(r.below(spoiled.len() as u64 + 1) as usize);
            }
            /* Its counters aimed at, of a randomness of their own so that the
               run goes as it would without: refused -- or, taken, a device
               and a volume added to it numbered as the state numbers them.
               Spoiled at random they are reached once in a long while, and
               a counter of 0 breaks no rule fsck can see until something
               is numbered by it */
            let mut aim = Rng::new(mix(mix(seed, 0x636f_756e), step));
            let mut counters = whole.clone();
            if aim.chance(25) && spoil_counters(&mut aim, &s, &m, &mut counters) {
                if let (Ok(t), Some(host)) = (State::decode(&counters), s.cs().first().map(|c| c.host.clone())) {
                    let probes = [Command::DeviceAdd { host: host.clone(), ident: String::from("disk:probe"), slots: 64 },
                                  Command::VolumeCreate { name: String::from("probe"), size_mib: 1, host }];
                    let (mut a, mut b) = (s.clone(), t);
                    for (k, p) in probes.iter().enumerate() {
                        let index = 2_000_000 + step * 4 + k as u64;
                        let (x, y) = (a.apply_command(index, p), b.apply_command(index, p));
                        if x != y {
                            return Err(format!("step {}: a snapshot whose counters were spoiled taken: {:?} answered \
                                                {:?} of it, {:?} of the state", step, p, y, x));
                        }
                    }
                }
            }
            /* A copy-up pending and a read-only volume aimed at (N10), as the
               counters are: refused, or -- taken -- held to fsck */
            let mut aim = Rng::new(mix(mix(seed, 0x636f_7079), step));
            let mut aimed_at = whole.clone();
            let spoiled_one = match aim.below(4) {
                0 => spoil_copying(&mut aim, &s, &mut aimed_at),
                1 => spoil_read_only(&mut aim, &s, &mut aimed_at),
                2 => spoil_free_list(&mut aim, &s, &mut aimed_at),
                _ => false,
            };
            if spoiled_one {
                if let Ok(t) = State::decode(&aimed_at) {
                    if let Some(d) = fsck(&t) {
                        return Err(format!("step {}: a snapshot spoiled where it holds a copy-up, a read-only volume or \
                                            the free list taken that breaks a rule: {}", step, d));
                    }
                }
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
