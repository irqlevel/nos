//! What a host's mds tells its cs and its clients, from its applied state
//! (plans/nbs.md, "The cs" and "The client, in hv"): the one copy that
//! nbs-mds.ko answers with and fuzz/nbs's cluster target's mds does.

use alloc::string::String;
use alloc::vec::Vec;

use nbscs::{ChunkRef, Epoch};
use nbsmeta::{DeviceState, Replica, State};

use crate::{Device, Identity, JoinView, Place, Question, Refusal, ReplicaState, Said, Told, LIST_MAX, MAP_MAX};

/// Question `q` of host `host`'s cs, answered from state `s`, applied as far
/// as entry `applied`.
pub fn told(s: &State, host: &str, applied: u64, q: &Question) -> Told {
    let Some(cluster) = s.cluster() else { return Told::Refused(Refusal::NoMds) };
    let mine = s.cs_of(host);
    match q {
        Question::Identity => Told::Identity(mine.map(|cs| Identity {
            cluster: cluster.id,
            cs: cs.id,
            devices: cs.devices.iter().map(|d| Device { ident: d.ident.clone(), device: d.id, slots: d.slots,
                                                          draining: d.state != DeviceState::Active })
                .collect(),
        })),
        Question::Verify { volume, generation, chunk, position, index, writing } => {
            if applied < *index {
                return Told::Refused(Refusal::Behind { applied });
            }
            let Some(v) = s.volumes().iter().find(|v| v.id == *volume) else {
                return Told::Refused(Refusal::NotTheMap);
            };
            if v.map.get(*position as usize) != Some(&chunk.chunk) {
                return Told::Refused(Refusal::NotTheMap);
            }
            let Some(ch) = s.chunks().get(chunk.chunk as usize).filter(|c| !c.is_free()) else {
                return Told::Refused(Refusal::NotTheMap);
            };
            if ch.incarnation != chunk.incarnation {
                return Told::Refused(Refusal::NotTheMap);
            }
            let (Some(set), Some(me)) = (s.sets().get(ch.set as usize), mine) else {
                return Told::Refused(Refusal::NotHere);
            };
            /* A member taken out serves nothing; the joiner -- being copied
               onto, its own place's member back or another cs -- is written,
               not read */
            let joining = set.joiner.is_some_and(|j| j.cs == me.id);
            let active = set.cs.iter().position(|&c| c == me.id).is_some_and(|at| set.replicas[at] == Replica::Active);
            if !(active || (joining && *writing)) {
                return Told::Refused(Refusal::NotHere);
            }
            let known = Epoch { set: ch.set, version: set.version };
            if known != chunk.epoch {
                return Told::Refused(Refusal::Epoch { known });
            }
            /* A write, or a read of a writer's, at the volume's attach for
               writing; a read of generation 0 is a read-only one's */
            if (*writing || *generation != 0) && v.writer.as_ref().is_none_or(|w| w.generation != *generation) {
                return Told::Refused(Refusal::NotAttached);
            }
            /* The joiner told so: it marks what clients write from now on,
               which its copy is not to write over */
            match set.joiner.filter(|_| !active) {
                Some(j) => Told::VerifiedJoining { since: j.since },
                None => Told::Verified,
            }
        }
        /* A source's check of a joiner's ask: the joiner the set's at the
           version it names, this host's cs an active member of the set */
        Question::VerifyJoin { set, joiner, version, index } => {
            if applied < *index {
                return Told::Refused(Refusal::Behind { applied });
            }
            let Some(st) = s.sets().get(*set as usize) else { return Told::Refused(Refusal::NotTheJoiner) };
            if st.joiner.is_none_or(|j| j.cs != *joiner) {
                return Told::Refused(Refusal::NotTheJoiner);
            }
            if st.version != *version {
                return Told::Refused(Refusal::Epoch { known: Epoch { set: *set, version: st.version } });
            }
            let Some(me) = mine else { return Told::Refused(Refusal::NotHere) };
            if !st.cs.iter().zip(st.replicas.iter()).any(|(&c, &r)| c == me.id && r == Replica::Active) {
                return Told::Refused(Refusal::NotHere);
            }
            Told::Verified
        }
        Question::Joins => {
            let Some(me) = mine else { return Told::Joins(Vec::new()) };
            Told::Joins(s.sets().iter().enumerate().filter_map(|(id, st)| {
                let j = st.joiner.filter(|j| j.cs == me.id)?;
                let k = usize::from(j.place);
                let sources = (0..nbsmeta::REPLICAS).filter(|&m| st.replicas[m] == Replica::Active)
                    .filter_map(|m| s.cs().get(st.cs[m] as usize).map(|c| (c.id, c.host.clone()))).collect();
                Some(JoinView { set: id as u32, place: j.place, since: j.since, version: st.version,
                                own: st.cs.get(k) == Some(&me.id), until: st.active_until.get(k).copied().unwrap_or(0),
                                sources, index: applied })
            }).take(LIST_MAX).collect())
        }
        /* A copy-up's copy on this host's cs (N10): pending at that place of
           that volume, from that chunk to that one, asked by the volume's
           writer of now, this host's cs an active member of their set at its
           version now */
        Question::VerifyCopy { volume, generation, position, from, to, index } => {
            if applied < *index {
                return Told::Refused(Refusal::Behind { applied });
            }
            let Some(v) = s.volumes().iter().find(|v| v.id == *volume) else {
                return Told::Refused(Refusal::NotTheMap);
            };
            if s.copying().get(&(*volume, *position)) != Some(&(to.chunk, from.chunk)) {
                return Told::Refused(Refusal::NotTheMap);
            }
            let live = |c: &ChunkRef| s.chunks().get(c.chunk as usize)
                .filter(|ch| !ch.is_free() && ch.incarnation == c.incarnation).copied();
            let (Some(ch), Some(_)) = (live(from), live(to)) else { return Told::Refused(Refusal::NotTheMap) };
            let (Some(set), Some(me)) = (s.sets().get(ch.set as usize), mine) else {
                return Told::Refused(Refusal::NotHere);
            };
            if !set.cs.iter().zip(set.replicas.iter()).any(|(&c, &r)| c == me.id && r == Replica::Active) {
                return Told::Refused(Refusal::NotHere);
            }
            let known = Epoch { set: ch.set, version: set.version };
            if from.epoch != known || to.epoch != known {
                return Told::Refused(Refusal::Epoch { known });
            }
            if *generation == 0 || v.writer.as_ref().is_none_or(|w| w.generation != *generation) {
                return Told::Refused(Refusal::NotAttached);
            }
            Told::Verified
        }
        /* Proposals: nbs-mds's, through raft, not answered from the state */
        Question::JoinDone { .. } | Question::GiveUp { .. } => Told::Refused(Refusal::NoMds),
        Question::Live { chunks } => Told::Gone(chunks.iter()
            .filter(|(c, inc)| s.chunks().get(*c as usize).is_none_or(|ch| ch.is_free() || ch.incarnation != *inc))
            .map(|&(c, _)| c).collect()),
        Question::Volumes { volumes } => Told::Gone(volumes.iter()
            .filter(|&&v| !s.volumes().iter().any(|x| x.id == v)).copied().collect()),
    }
}

fn replica_state(r: Replica) -> ReplicaState {
    match r {
        Replica::Active => ReplicaState::Active,
        Replica::Down => ReplicaState::Down,
    }
}

/// `count` places of volume `name`'s map from `from` -- MAP_MAX at most --
/// as state `s` has them, once it has applied as far as entry `index`
/// (`applied`, how far it has); and the hosts of the cs's they name.
pub fn map(s: &State, applied: u64, name: &str, from: u32, count: u32, index: u64) -> Said {
    if applied < index {
        return Said::Behind { applied };
    }
    if s.cluster().is_none() {
        return Said::Refused(String::from("this host's member has no NBS cluster yet"));
    }
    let Some(v) = s.volume(name) else { return Said::Refused(alloc::format!("no volume {}", name)) };
    let places = v.map.len() as u32;
    let end = from.saturating_add(count.min(MAP_MAX)).min(places);
    let mut map = Vec::new();
    let mut named: Vec<u32> = Vec::new();
    for p in from.min(end)..end {
        let unknown = || Said::Refused(alloc::format!("volume {}'s place {} names a chunk or a set the metadata has \
                                                       not", name, p));
        let Some(&c) = v.map.get(p as usize) else { return unknown() };
        let Some(ch) = s.chunks().get(c as usize) else { return unknown() };
        let Some(set) = s.sets().get(ch.set as usize) else { return unknown() };
        map.push(Place {
            chunk: ChunkRef { chunk: c, incarnation: ch.incarnation, epoch: Epoch { set: ch.set, version: set.version } },
            cs: set.cs,
            replicas: [replica_state(set.replicas[0]), replica_state(set.replicas[1]), replica_state(set.replicas[2])],
            joiner: set.joiner.map(|j| j.cs),
        });
        for cs in set.cs.iter().copied().chain(set.joiner.map(|j| j.cs)) {
            if !named.contains(&cs) {
                named.push(cs);
            }
        }
    }
    let hosts = named.iter().filter_map(|&cs| s.cs().get(cs as usize).map(|c| (cs, c.host.clone()))).collect();
    Said::Map { volume: v.id, size_mib: v.size_mib, places, index: applied, from, map, hosts }
}
