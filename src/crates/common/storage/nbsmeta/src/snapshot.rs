//! The state as a snapshot holds it, written a record at a time and read
//! back off a stream (plans/nbs.md, "The mds's state on disk"): a state of
//! tens of MiB never in one buffer, where the kernel's heap gives no block
//! past 512 KiB. Its bytes are the state's in a fixed order, the same on
//! every member with the same state, so their SHA-256 is the digest the
//! members hold each other's to.
//!
//! Every table read back is held to the rules applying keeps (`check`): a
//! snapshot that says anything applying could not have made is refused.

use alloc::vec::Vec;

use codec::{Source, Stream, Writer};
use raft::rsm::{Frozen, RestoreError, SnapshotReader};
use sha2::{Digest as _, Sha256};

use crate::pieces::Pieces;
use crate::storage::*;
use crate::{Cluster, Member, Removed, State, MEMBERS_MAX, NAME_MAX, REMOVED_MAX};

/* NBM3: chunks carry their incarnation; NBM4: a writer its client's token;
   NBM5: a cs its state, a set its joiner and its members' last active
   versions (N9) */
const MAGIC: [u8; 4] = *b"NBM5";
/// Chunks or map entries a record of the stream carries at most.
const BATCH: usize = 256;
/// How much a digest asks of the stream at a time.
const DIGEST_STEP: usize = 64 * 1024;
/// Checkpoints a member keeps the digest of: a leader's for an older one
/// cannot be held to.
const RECENT: usize = 4;

/// Where the stream is.
#[derive(Clone, Copy)]
enum At {
    Header,
    Members(usize),
    Removed(usize),
    Cs(usize),
    Sets(usize),
    Chunks(usize),
    Volume(usize),
    Map(usize, usize),
    Done,
}

/// A state frozen -- its tables' pieces held as they were -- and written out
/// a record at a time.
pub struct FrozenState {
    state: State,
    at: At,
    /// The record under way, and how much of it is out.
    pending: Vec<u8>,
    sent: usize,
}

impl FrozenState {
    pub(crate) fn new(state: State) -> FrozenState {
        FrozenState { state, at: At::Header, pending: Vec::new(), sent: 0 }
    }

    /// The next record's bytes into `pending`: false once there are none,
    /// and `pending` then empty.
    fn next(&mut self) -> bool {
        self.sent = 0;
        let s = &self.state;
        let mut w = Writer { buf: core::mem::take(&mut self.pending) };
        w.buf.clear();
        self.at = match self.at {
            At::Header => {
                w.fixed(&MAGIC);
                match &s.cluster {
                    Some(c) => {
                        w.u8(1);
                        w.fixed(&c.id);
                        w.str(&c.name);
                    }
                    None => w.u8(0),
                }
                w.u32(s.next_device);
                w.u32(s.next_volume);
                w.u64(s.next_check);
                w.u32(s.free_head);
                w.u8(s.members.len() as u8);
                At::Members(0)
            }
            At::Members(i) => match s.members.get(i) {
                Some(m) => {
                    w.str(&m.host);
                    w.u64(m.id);
                    w.u64(m.since);
                    At::Members(i + 1)
                }
                None => {
                    w.u32(s.removed.len() as u32);
                    At::Removed(0)
                }
            },
            At::Removed(i) => match s.removed.get(i) {
                Some(r) => {
                    w.str(&r.host);
                    w.u64(r.id);
                    w.u64(r.at);
                    At::Removed(i + 1)
                }
                None => {
                    w.u8(s.cs.len() as u8);
                    At::Cs(0)
                }
            },
            At::Cs(i) => match s.cs.get(i) {
                Some(c) => {
                    w.str(&c.host);
                    w.u64(c.reserved);
                    w.u8(cs_state_code(c.state));
                    w.u8(c.devices.len() as u8);
                    for d in &c.devices {
                        w.u32(d.id);
                        w.str(&d.ident);
                        w.u32(d.slots);
                        w.u8(device_state_code(d.state));
                    }
                    At::Cs(i + 1)
                }
                None => {
                    w.u32(s.sets.len() as u32);
                    At::Sets(0)
                }
            },
            At::Sets(i) => match s.sets.get(i) {
                Some(set) => {
                    for c in set.cs {
                        w.u32(c);
                    }
                    for r in set.replicas {
                        w.u8(replica_code(r));
                    }
                    for a in set.active_until {
                        w.u32(a);
                    }
                    match set.joiner {
                        Some(j) => {
                            w.u8(1);
                            w.u32(j.cs);
                            w.u8(j.place);
                            w.u32(j.since);
                        }
                        None => w.u8(0),
                    }
                    w.u32(set.version);
                    w.u32(set.chunks);
                    At::Sets(i + 1)
                }
                None => {
                    w.u32(s.chunks.len() as u32);
                    At::Chunks(0)
                }
            },
            At::Chunks(i) if i < s.chunks.len() => {
                let end = (i + BATCH).min(s.chunks.len());
                for ch in (i..end).filter_map(|k| s.chunks.get(k)) {
                    w.u32(ch.set);
                    w.u16(ch.count);
                    w.u8(ch.flags);
                    w.u32(ch.incarnation);
                }
                At::Chunks(end)
            }
            At::Chunks(_) => {
                w.u32(s.volumes.len() as u32);
                At::Volume(0)
            }
            At::Volume(i) => match s.volumes.get(i) {
                Some(v) => {
                    w.u32(v.id);
                    w.str(&v.name);
                    w.u64(v.size_mib);
                    w.u8(kind_code(v.kind));
                    w.u32(v.origin);
                    w.str(&v.host);
                    w.u64(v.generation);
                    match &v.writer {
                        Some(a) => {
                            w.u8(1);
                            w.str(&a.host);
                            w.u64(a.generation);
                            w.u64(a.token);
                        }
                        None => w.u8(0),
                    }
                    w.u8(v.readers.len() as u8);
                    for r in &v.readers {
                        w.str(r);
                    }
                    w.u32(v.map.len() as u32);
                    At::Map(i, 0)
                }
                None => return false,
            },
            At::Map(i, j) => match s.volumes.get(i) {
                Some(v) if j < v.map.len() => {
                    let end = (j + BATCH).min(v.map.len());
                    for &c in (j..end).filter_map(|k| v.map.get(k)) {
                        w.u32(c);
                    }
                    At::Map(i, end)
                }
                _ => At::Volume(i + 1),
            },
            At::Done => return false,
        };
        self.pending = w.buf;
        self.sent = 0;
        true
    }
}

impl Frozen for FrozenState {
    fn write(&mut self, max: usize, out: &mut Vec<u8>) {
        let mut room = max;
        while room > 0 {
            if self.sent == self.pending.len() && !self.next() {
                self.at = At::Done;
                return;
            }
            let n = room.min(self.pending.len() - self.sent);
            out.extend_from_slice(&self.pending[self.sent..self.sent + n]);
            self.sent += n;
            room -= n;
        }
    }
}

fn device_state_code(s: DeviceState) -> u8 {
    match s {
        DeviceState::Active => 0,
        DeviceState::Draining => 1,
        DeviceState::Failed => 2,
    }
}

fn device_state(c: u8) -> Option<DeviceState> {
    Some(match c {
        0 => DeviceState::Active,
        1 => DeviceState::Draining,
        2 => DeviceState::Failed,
        _ => return None,
    })
}

fn replica_code(r: Replica) -> u8 {
    match r {
        Replica::Active => 0,
        Replica::Down => 2,
    }
}

fn replica(c: u8) -> Option<Replica> {
    Some(match c {
        0 => Replica::Active,
        2 => Replica::Down,
        _ => return None,
    })
}

fn cs_state_code(s: CsState) -> u8 {
    match s {
        CsState::Up => 0,
        CsState::Down => 1,
        CsState::Out => 2,
    }
}

fn cs_state(c: u8) -> Option<CsState> {
    Some(match c {
        0 => CsState::Up,
        1 => CsState::Down,
        2 => CsState::Out,
        _ => return None,
    })
}

fn kind_code(k: Kind) -> u8 {
    match k {
        Kind::Volume => 0,
        Kind::Image => 1,
        Kind::Snapshot => 2,
    }
}

fn kind(c: u8) -> Option<Kind> {
    Some(match c {
        0 => Kind::Volume,
        1 => Kind::Image,
        2 => Kind::Snapshot,
        _ => return None,
    })
}

/// A snapshot's reader, as a stream reads it.
pub(crate) struct FromSnapshot<'r, 'a>(pub(crate) &'r mut SnapshotReader<'a>);

impl Source for FromSnapshot<'_, '_> {
    fn fill(&mut self, buf: &mut [u8]) -> bool {
        self.0.read_exact(buf)
    }
}

const BAD: RestoreError = RestoreError("not NBS's metadata");

/// A state read off `r`, as `FrozenState` writes it, every count bounded
/// before anything is made of it -- not yet held to the rules.
pub(crate) fn read<S: Source>(r: &mut Stream<S>) -> Result<State, RestoreError> {
    if r.fixed::<4>().ok_or(BAD)? != MAGIC {
        return Err(BAD);
    }
    let mut s = State::new();
    s.cluster = match r.u8().ok_or(BAD)? {
        0 => None,
        1 => {
            let id = r.fixed().ok_or(BAD)?;
            Some(Cluster { id, name: r.str(NAME_MAX).ok_or(BAD)? })
        }
        _ => return Err(BAD),
    };
    s.next_device = r.u32().ok_or(BAD)?;
    s.next_volume = r.u32().ok_or(BAD)?;
    s.next_check = r.u64().ok_or(BAD)?;
    s.free_head = r.u32().ok_or(BAD)?;
    let n = usize::from(r.u8().ok_or(BAD)?);
    if n > MEMBERS_MAX {
        return Err(RestoreError("more members than a cluster takes"));
    }
    for _ in 0..n {
        let host = r.str(ipcwire::HOST_MAX).ok_or(BAD)?;
        let id = r.u64().ok_or(BAD)?;
        let since = r.u64().ok_or(BAD)?;
        s.members.push(Member { host, id, since });
    }
    let n = r.u32().ok_or(BAD)? as usize;
    if n > REMOVED_MAX {
        return Err(RestoreError("more members removed than are kept"));
    }
    for _ in 0..n {
        let host = r.str(ipcwire::HOST_MAX).ok_or(BAD)?;
        let id = r.u64().ok_or(BAD)?;
        let at = r.u64().ok_or(BAD)?;
        s.removed.push(Removed { host, id, at });
    }
    let n = usize::from(r.u8().ok_or(BAD)?);
    if n > CS_MAX {
        return Err(RestoreError("more chunk servers than are kept"));
    }
    for id in 0..n as u32 {
        let host = r.str(ipcwire::HOST_MAX).ok_or(BAD)?;
        let reserved = r.u64().ok_or(BAD)?;
        let state = cs_state(r.u8().ok_or(BAD)?).ok_or(BAD)?;
        let k = usize::from(r.u8().ok_or(BAD)?);
        if k > DEVICES_MAX {
            return Err(RestoreError("more devices than a cs has"));
        }
        let mut devices = Vec::with_capacity(k);
        for _ in 0..k {
            let id = r.u32().ok_or(BAD)?;
            let ident = r.str(IDENT_MAX).ok_or(BAD)?;
            let slots = r.u32().ok_or(BAD)?;
            let state = device_state(r.u8().ok_or(BAD)?).ok_or(BAD)?;
            devices.push(Device { id, ident, slots, state });
        }
        s.cs.push(Cs { id, host, devices, reserved, state });
    }
    let n = r.u32().ok_or(BAD)? as usize;
    if n > SETS_MAX {
        return Err(RestoreError("more sets than are kept"));
    }
    for _ in 0..n {
        let mut cs = [0u32; REPLICAS];
        for c in cs.iter_mut() {
            *c = r.u32().ok_or(BAD)?;
        }
        let mut replicas = [Replica::Active; REPLICAS];
        for x in replicas.iter_mut() {
            *x = replica(r.u8().ok_or(BAD)?).ok_or(BAD)?;
        }
        let mut active_until = [0u32; REPLICAS];
        for a in active_until.iter_mut() {
            *a = r.u32().ok_or(BAD)?;
        }
        let joiner = match r.u8().ok_or(BAD)? {
            0 => None,
            1 => {
                let cs = r.u32().ok_or(BAD)?;
                let place = r.u8().ok_or(BAD)?;
                Some(Joiner { cs, place, since: r.u32().ok_or(BAD)? })
            }
            _ => return Err(BAD),
        };
        let version = r.u32().ok_or(BAD)?;
        let chunks = r.u32().ok_or(BAD)?;
        s.sets.push(Set { cs, replicas, active_until, joiner, version, chunks });
    }
    let n = r.u32().ok_or(BAD)? as usize;
    if n > CHUNKS_MAX {
        return Err(RestoreError("more chunks than are kept"));
    }
    for _ in 0..n {
        let set = r.u32().ok_or(BAD)?;
        let count = r.u16().ok_or(BAD)?;
        let flags = r.u8().ok_or(BAD)?;
        let incarnation = r.u32().ok_or(BAD)?;
        s.chunks.push(Chunk { set, count, flags, incarnation });
    }
    let n = r.u32().ok_or(BAD)? as usize;
    if n > VOLUMES_MAX {
        return Err(RestoreError("more volumes than are kept"));
    }
    for _ in 0..n {
        let id = r.u32().ok_or(BAD)?;
        let name = r.str(VOLUME_NAME_MAX).ok_or(BAD)?;
        let size_mib = r.u64().ok_or(BAD)?;
        let kind = kind(r.u8().ok_or(BAD)?).ok_or(BAD)?;
        let origin = r.u32().ok_or(BAD)?;
        let host = r.str(ipcwire::HOST_MAX).ok_or(BAD)?;
        let generation = r.u64().ok_or(BAD)?;
        let writer = match r.u8().ok_or(BAD)? {
            0 => None,
            1 => {
                let host = r.str(ipcwire::HOST_MAX).ok_or(BAD)?;
                let generation = r.u64().ok_or(BAD)?;
                Some(Attach { host, generation, token: r.u64().ok_or(BAD)? })
            }
            _ => return Err(BAD),
        };
        let k = usize::from(r.u8().ok_or(BAD)?);
        if k > READERS_MAX {
            return Err(RestoreError("more readers than a volume has"));
        }
        let mut readers = Vec::with_capacity(k);
        for _ in 0..k {
            readers.push(r.str(ipcwire::HOST_MAX).ok_or(BAD)?);
        }
        let k = r.u32().ok_or(BAD)? as u64;
        if k != chunks_for(size_mib) || k > CHUNKS_MAX as u64 {
            return Err(RestoreError("a volume's map not of its size"));
        }
        let mut map = Pieces::new();
        for _ in 0..k {
            map.push(r.u32().ok_or(BAD)?);
        }
        s.volumes.push(Volume { id, name, size_mib, kind, origin, host, generation, writer, readers, map });
    }
    Ok(s)
}

impl State {
    /// The rules of what is stored a state read back is held to: what
    /// applying never makes, refused. Each count is made over the tables
    /// once, in pieces of their own -- no table of the state's size is made.
    pub(crate) fn check_storage(&self) -> Result<(), RestoreError> {
        if self.cluster.is_none()
            && (!self.cs.is_empty() || !self.sets.is_empty() || !self.chunks.is_empty() || !self.volumes.is_empty())
        {
            return Err(RestoreError("what is stored of no cluster"));
        }
        /* The counters as applying keeps them: devices and volumes numbered
           from 1, a digest no sooner than CHECK_EVERY. Without this a state
           of no volumes and a next number of 0 passed, and its next volume
           was numbered 0 (found by fuzz/nbs's meta target) */
        if self.next_device == 0 || self.next_volume == 0 || self.next_check < crate::CHECK_EVERY {
            return Err(RestoreError("a counter of the state's at a value applying never leaves it at"));
        }
        let mut idents: Vec<&str> = Vec::new();
        let mut devices: Vec<u32> = Vec::new();
        for (i, c) in self.cs.iter().enumerate() {
            if c.id as usize != i || !crate::valid_name(&c.host) || self.cs[..i].iter().any(|o| o.host == c.host) {
                return Err(RestoreError("a chunk server out of place, of no host or of a host twice"));
            }
            for d in &c.devices {
                if !valid_ident(&d.ident) || d.slots == 0 || d.slots > SLOTS_MAX || d.id >= self.next_device {
                    return Err(RestoreError("a device of no id or size, or of a number not yet given"));
                }
                idents.push(&d.ident);
                devices.push(d.id);
            }
        }
        idents.sort_unstable();
        devices.sort_unstable();
        if idents.windows(2).any(|w| w[0] == w[1]) || devices.windows(2).any(|w| w[0] == w[1]) {
            return Err(RestoreError("a device twice"));
        }
        for set in self.sets.iter() {
            if set.version == 0 || !set.cs.windows(2).all(|w| w[0] < w[1]) || set.cs[REPLICAS - 1] as usize >= self.cs.len() {
                return Err(RestoreError("a set not of three chunk servers in order"));
            }
            /* A replica taken out never takes the last active one with it */
            if !set.replicas.contains(&Replica::Active) {
                return Err(RestoreError("a set with no active replica"));
            }
            /* A member down was active until a version before this one; an
               active one has none */
            for k in 0..REPLICAS {
                let ok = match set.replicas[k] {
                    Replica::Active => set.active_until[k] == 0,
                    Replica::Down => set.active_until[k] != 0 && set.active_until[k] < set.version,
                };
                if !ok {
                    return Err(RestoreError("a set's member down at no version before its own, or active with one"));
                }
            }
            /* A joiner in one of its places: the member's own, down, or
               another cs's no member is; joined at a version of the set's */
            if let Some(j) = set.joiner {
                let k = usize::from(j.place);
                let ok = k < REPLICAS && (j.cs as usize) < self.cs.len() && j.since != 0 && j.since <= set.version
                    && if set.cs[k] == j.cs { set.replicas[k] == Replica::Down } else { !set.cs.contains(&j.cs) };
                if !ok {
                    return Err(RestoreError("a set's joiner of no place, or a member already, or of a version to come"));
                }
            }
        }
        /* Every chunk in use: in a set, counted by as many maps as name it;
           every free one on the free list, which ends */
        let mut named: Pieces<u16> = Pieces::new();
        for _ in 0..self.chunks.len() {
            named.push(0);
        }
        for v in self.volumes.iter() {
            for &c in v.map.iter() {
                let Some(n) = named.get_mut(c as usize) else {
                    return Err(RestoreError("a volume's map naming no chunk"));
                };
                *n = n.checked_add(1).ok_or(RestoreError("a chunk named too often"))?;
            }
        }
        let mut in_set: Vec<u32> = alloc::vec![0; self.sets.len()];
        let mut free = 0u64;
        for (i, ch) in self.chunks.iter().enumerate() {
            let n = named.get(i).copied().unwrap_or(0);
            if ch.incarnation == 0 {
                return Err(RestoreError("a chunk of incarnation 0, which none is"));
            }
            if ch.is_free() {
                if ch.count != 0 || n != 0 || ch.flags != CHUNK_FREE {
                    return Err(RestoreError("a free chunk in use"));
                }
                free += 1;
                continue;
            }
            if ch.count == 0 || ch.count != n || ch.flags & !CHUNK_WRITTEN != 0 {
                return Err(RestoreError("a chunk counted as other than the maps that name it"));
            }
            let Some(c) = in_set.get_mut(ch.set as usize) else {
                return Err(RestoreError("a chunk in no set"));
            };
            *c += 1;
        }
        if self.sets.iter().zip(in_set.iter()).any(|(s, &n)| s.chunks != n) {
            return Err(RestoreError("a set's count of chunks not its chunks'"));
        }
        if u64::from(self.free_count) != free {
            return Err(RestoreError("the free list's length not its chunks'"));
        }
        let mut at = self.free_head;
        let mut walked = 0u64;
        while at != NO_CHUNK {
            let Some(ch) = self.chunks.get(at as usize).filter(|c| c.is_free()) else {
                return Err(RestoreError("the free list through a chunk in use"));
            };
            walked += 1;
            if walked > free {
                return Err(RestoreError("the free list in a ring"));
            }
            at = ch.set;
        }
        if walked != free {
            return Err(RestoreError("a free chunk off the free list"));
        }
        /* Reserved: every chunk of a set it is a member of, and of one it
           joins in another's place */
        let mut held = alloc::vec![0u64; self.cs.len()];
        for set in self.sets.iter() {
            for &c in &set.cs {
                if let Some(h) = held.get_mut(c as usize) {
                    *h += u64::from(set.chunks);
                }
            }
            if let Some(j) = set.joiner.filter(|j| set.joins_elsewhere(j.cs)) {
                if let Some(h) = held.get_mut(j.cs as usize) {
                    *h += u64::from(set.chunks);
                }
            }
        }
        if self.cs.iter().zip(held.iter()).any(|(c, &h)| c.reserved != h) {
            return Err(RestoreError("a chunk server's reserved not its replicas'"));
        }
        let mut ids: Vec<u32> = Vec::new();
        let mut names: Vec<&str> = Vec::new();
        for v in self.volumes.iter() {
            if !valid_volume(&v.name) || v.size_mib == 0 || v.size_mib > VOLUME_MIB_MAX || v.id == 0
                || v.id >= self.next_volume || !crate::valid_name(&v.host)
            {
                return Err(RestoreError("a volume of no name, size, number or host"));
            }
            if v.writer.as_ref().is_some_and(|w| w.generation == 0 || w.generation > v.generation)
                || (v.writer.is_some() && !v.readers.is_empty())
            {
                return Err(RestoreError("a volume's writer of a generation it never gave, or with readers"));
            }
            for (k, r) in v.readers.iter().enumerate() {
                if v.readers[..k].contains(r) {
                    return Err(RestoreError("a volume's reader twice"));
                }
            }
            ids.push(v.id);
            names.push(&v.name);
        }
        ids.sort_unstable();
        names.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) || names.windows(2).any(|w| w[0] == w[1]) {
            return Err(RestoreError("a volume twice"));
        }
        Ok(())
    }

    /// The state's digest: the SHA-256 of its snapshot's bytes.
    pub fn digest(&self) -> [u8; 32] {
        let mut f = FrozenState::new(self.clone());
        let mut h = Sha256::new();
        let mut buf = Vec::with_capacity(DIGEST_STEP);
        loop {
            buf.clear();
            f.write(DIGEST_STEP, &mut buf);
            if buf.is_empty() {
                break;
            }
            h.update(&buf);
        }
        h.finalize().into()
    }
}

/// A member's own record of its state's digests, kept out of the state:
/// the last few checkpoints', and the first leader's digest that differed.
#[derive(Clone, Debug, Default)]
pub struct Checks {
    recent: Vec<(u64, [u8; 32])>,
    diverged: Option<Diverged>,
    /// The checkpoints held to the leader's, and how many it had no digest of
    /// to hold to.
    pub held: u64,
    pub unheld: u64,
}

/// Where this member's state stopped being the leader's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Diverged {
    /// The checkpoint's entry.
    pub at: u64,
    pub mine: [u8; 32],
    pub theirs: [u8; 32],
    /// The entry the leader's digest came in.
    pub seen: u64,
}

impl Checks {
    pub(crate) fn push(&mut self, at: u64, digest: [u8; 32]) {
        if self.recent.len() >= RECENT {
            self.recent.remove(0);
        }
        self.recent.push((at, digest));
    }

    /// The leader's digest of checkpoint `at`, which came in at entry
    /// `seen`, held to this member's own where it has one.
    pub(crate) fn compare(&mut self, at: u64, theirs: &[u8; 32], seen: u64) {
        match self.recent.iter().find(|(i, _)| *i == at) {
            Some((_, mine)) if mine == theirs => self.held += 1,
            Some((_, mine)) => {
                if self.diverged.is_none() {
                    self.diverged = Some(Diverged { at, mine: *mine, theirs: *theirs, seen });
                }
            }
            None => self.unheld += 1,
        }
    }

    /// The newest checkpoint, and its digest.
    pub fn latest(&self) -> Option<(u64, [u8; 32])> {
        self.recent.last().copied()
    }

    /// This member's digest of checkpoint `at`, where it still has it.
    pub fn digest_at(&self, at: u64) -> Option<[u8; 32]> {
        self.recent.iter().find(|(i, _)| *i == at).map(|(_, d)| *d)
    }

    pub fn diverged(&self) -> Option<&Diverged> {
        self.diverged.as_ref()
    }
}

