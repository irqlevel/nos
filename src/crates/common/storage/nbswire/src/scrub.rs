//! The scrub (plans/nbs.md, N9e): the leader's look at every chunk closed on
//! its set's active members, held alike by the CRC-32C of each granule --
//! the one scrub nbs-mds's leader runs and fuzz/nbs's cluster target's does.
//! It has no I/O: what it asks is handed back, what is answered handed in,
//! and the state it plans from lent to each call.
//!
//! - **A chunk at a time**, every volume's in turn, of a set with no joiner
//!   and two active members at least: each member asked for the chunk's
//!   record and the sums of all of it (`Op::Scrub`), which moves no epoch on
//!   and so refuses a writer nothing.
//! - **Compared** only where every answer is of a closed chunk and the set
//!   is at the version it was asked at: an open chunk is in flight, and is
//!   looked at on a later round. A member that does not hold the chunk
//!   answers so (`Refusal::NotHeld`), and its copy is compared as what it
//!   reads as: zeros (`nbscs::zero_sums`) -- one that lost a chunk its set's
//!   others hold differs from them as a rotted one does. A chunk no member
//!   holds was never written: nothing is read of it, and the next chunk is
//!   looked at a hundredth of the pace on (`EMPTY_PART`) -- a volume mostly
//!   unwritten takes a round little longer than what is written of it, and
//!   its empty chunks are asked about no faster than that.
//! - **A difference** is acted on only when a second look, `CONFIRM_NS` on,
//!   finds every member's answer as the first did: two members' answers are
//!   not taken at one instant, and a write and a close between them would
//!   show a difference that is none -- a chunk written meanwhile changes its
//!   answers, and is let be.
//! - **What it finds it says**, and takes nothing down: of three, the one
//!   unlike the two that agree, or that no two agree. A cs keeps no checksum
//!   of its own data, and two members alike are no proof -- a joiner that
//!   copied a rotted source makes two alike in rot, and a scrub that took
//!   the odd one down would have taken the one good copy down, its join back
//!   copying the rot over it (found by fuzz/nbs's cluster target, N9e). A
//!   person takes a member down (`Command::ReplicaBad`), for the controller
//!   to have it join its own place again -- a member back, whose copy holds
//!   every chunk to a source's by sums and pulls what differs. Each finding
//!   is said once for the set's version it was found at.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use nbscs::{ChunkRef, Epoch};
use nbsmeta::{Replica, State};

use crate::{Op, Refusal, Reply, Request};

/// How long a look's answers are waited for: one that does not come in time
/// leaves the chunk to a later round.
pub const ANSWER_NS: u64 = 5_000_000_000;
/// How long after a difference is first seen it is looked at again.
pub const CONFIRM_NS: u64 = 2_000_000_000;
/// A look at a chunk no member holds reads nothing, and takes this part of
/// a look's turn: the messages a look costs, at most a hundred a pace.
pub const EMPTY_PART: u64 = 100;

/// A member's answer: whether it holds the chunk at all, whether the chunk
/// is open there, the version it was last written at, and its sums -- of
/// zeros where it holds none.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Answer {
    held: bool,
    open: bool,
    written: u32,
    sums: Vec<u32>,
}

/// A look at a chunk under way: the members asked, each by its request's
/// id, and what each answered.
struct Look {
    chunk: ChunkRef,
    asked: BTreeMap<u64, u32>,
    got: BTreeMap<u32, Answer>,
    started: u64,
    /// The second look at a difference seen.
    again: bool,
}

/// A difference seen at a first look, to be looked at again: the chunk, its
/// set's version, and every member's answer then.
struct Seen {
    chunk: ChunkRef,
    got: BTreeMap<u32, Answer>,
}

/// What the scrub asks of its driver.
#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    /// A request for the cs of `host`.
    Send { host: String, bytes: Vec<u8> },
    /// Chunk `chunk` of set `set`, at the set's `version`: its active
    /// members' copies differ, as two looks found them -- `odd` the member
    /// unlike the two that agree, where two do.
    Found { chunk: u32, set: u32, version: u32, odd: Option<u32> },
    /// A line for a person.
    Say(String),
}

/// What the scrub has done, for `nbs status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Chunks whose every active member answered alike.
    pub alike: u64,
    /// Chunks no active member holds: never written, nothing read.
    pub empty: u64,
    /// Looks let be: a chunk open on a member, a set moved on, an answer
    /// late or refused, a chunk written between two looks.
    pub skipped: u64,
    /// Chunks found with a member unlike the two others: said.
    pub odd: u64,
    /// Chunks found whose members differ with no two alike: said.
    pub differ: u64,
}

/// The leader's scrub.
pub struct Scrub {
    /// How long between one look's end and the next's start.
    every_ns: u64,
    /// The next chunk in turn: a volume's place in the volume table, and a
    /// place of its map.
    volume: usize,
    place: usize,
    look: Option<Look>,
    seen: Option<Seen>,
    /// When the next look may begin.
    at: u64,
    /// What was said: each chunk by its number and incarnation, and the
    /// epoch -- its set, at the version -- it was found at.
    said: BTreeSet<(u32, u32, Epoch)>,
    /// The sums of a chunk of zeros: a member's that does not hold it.
    zeros: Vec<u32>,
    pub stats: Stats,
}

impl Scrub {
    /// A scrub that looks at a chunk every `every_ns` at most.
    pub fn new(every_ns: u64) -> Scrub {
        Scrub { every_ns, volume: 0, place: 0, look: None, seen: None, at: 0, said: BTreeSet::new(),
                zeros: nbscs::zero_sums(), stats: Stats::default() }
    }

    /// Whether the answer to request `id` is one this scrub waits for.
    pub fn asked(&self, id: u64) -> bool {
        self.look.as_ref().is_some_and(|l| l.asked.contains_key(&id))
    }

    /// The scrub's turn at `now`, by state `s`: a look whose answers did not
    /// all come in time let be, and the next begun once the pace allows --
    /// the chunk a difference was seen in, or the next in turn. Its requests'
    /// ids come from `next_id`.
    pub fn tick(&mut self, s: &State, now: u64, next_id: &mut u64, out: &mut Vec<Out>) {
        if let Some(l) = &self.look {
            if now < l.started + ANSWER_NS {
                return;
            }
            self.look = None;
            self.seen = None;
            self.stats.skipped += 1;
            self.at = now + self.every_ns;
        }
        if now < self.at {
            return;
        }
        let (chunk, again) = match self.seen.as_ref() {
            Some(seen) => {
                let volume = s.volumes().iter().find(|v| v.map.iter().any(|&c| c == seen.chunk.chunk)).map(|v| v.id);
                (Some((seen.chunk, volume)), true)
            }
            None => (self.next(s), false),
        };
        let Some((chunk, Some(volume))) = chunk else {
            self.seen = None;
            self.at = now + self.every_ns;
            return;
        };
        let Some(set) = s.sets().get(chunk.epoch.set as usize) else {
            self.seen = None;
            return;
        };
        let mut look = Look { chunk, asked: BTreeMap::new(), got: BTreeMap::new(), started: now, again };
        for k in 0..nbsmeta::REPLICAS {
            if set.replicas[k] != Replica::Active {
                continue;
            }
            let Some(host) = s.cs().get(set.cs[k] as usize).map(|c| c.host.clone()) else { continue };
            let id = *next_id;
            *next_id += 1;
            let bytes = Request { id, volume, generation: 0, op: Op::Scrub { chunk } }.encode();
            look.asked.insert(id, set.cs[k]);
            out.push(Out::Send { host, bytes });
        }
        self.look = Some(look);
    }

    /// The next chunk in turn that can be looked at -- its set with no
    /// joiner and two active members at least -- and its volume.
    fn next(&mut self, s: &State) -> Option<(ChunkRef, Option<u32>)> {
        let volumes = s.volumes();
        /* Every place once, and a step from each volume's end to the next's */
        let total: usize = volumes.iter().map(|v| v.map.len() + 1).sum::<usize>() + 1;
        for _ in 0..total {
            let Some(v) = volumes.get(self.volume) else {
                self.volume = 0;
                self.place = 0;
                continue;
            };
            let Some(&c) = v.map.get(self.place) else {
                self.volume = (self.volume + 1) % volumes.len().max(1);
                self.place = 0;
                continue;
            };
            self.place += 1;
            let Some(ch) = s.chunks().get(c as usize).filter(|ch| !ch.is_free()) else { continue };
            let Some(set) = s.sets().get(ch.set as usize) else { continue };
            if set.joiner.is_some() || set.active() < 2 {
                continue;
            }
            let chunk = ChunkRef { chunk: c, incarnation: ch.incarnation, epoch: Epoch { set: ch.set, version: set.version } };
            return Some((chunk, Some(v.id)));
        }
        None
    }

    /// Request `id`'s answer `reply`, with state `s` at `now`: the look
    /// judged once every member asked has answered.
    pub fn answer(&mut self, id: u64, reply: &Reply, s: &State, now: u64, out: &mut Vec<Out>) {
        let Some(l) = self.look.as_mut() else { return };
        let Some(cs) = l.asked.remove(&id) else { return };
        match reply {
            Reply::Scrub { open, written, sums } => {
                l.got.insert(cs, Answer { held: true, open: *open, written: *written, sums: sums.clone() });
            }
            /* Not held there: what it reads as -- zeros -- compared with the
               others' copies */
            Reply::Refused(Refusal::NotHeld) => {
                l.got.insert(cs, Answer { held: false, open: false, written: 0, sums: self.zeros.clone() });
            }
            /* Refused otherwise -- a device failed, the chunk newer there than
               the state says -- or not an answer to this: the chunk left to a
               later round */
            _ => {
                self.look = None;
                self.seen = None;
                self.stats.skipped += 1;
                self.at = now + self.every_ns;
                return;
            }
        }
        if !l.asked.is_empty() {
            return;
        }
        let Some(l) = self.look.take() else { return };
        self.at = now + self.every_ns;
        self.judge(l, s, now, out);
    }

    /// A look whose every answer is in.
    fn judge(&mut self, l: Look, s: &State, now: u64, out: &mut Vec<Out>) {
        let c = l.chunk;
        /* The set as it was asked at, and the chunk the same: else moved on */
        let same = s.chunks().get(c.chunk as usize).is_some_and(|ch| ch.incarnation == c.incarnation && ch.set == c.epoch.set)
            && s.sets().get(c.epoch.set as usize).is_some_and(|set| set.version == c.epoch.version && set.joiner.is_none());
        /* Closed on every member: one open is in flight. The versions they
           were last written at may differ -- a member back's copy writes at
           its join's -- and are no test of the data: a write between two
           members' answers is the second look's to find, each member's
           answer then held to its first */
        let comparable = same && l.got.len() >= 2 && l.got.values().all(|a| !a.open);
        if !comparable {
            self.seen = None;
            self.stats.skipped += 1;
            return;
        }
        /* Held by none: never written. Nothing was read, and the pace is the
           reads': the next chunk a part of it on */
        if l.got.values().all(|a| !a.held) {
            self.seen = None;
            self.stats.empty += 1;
            self.at = now + self.every_ns / EMPTY_PART;
            return;
        }
        let sums: Vec<&Vec<u32>> = l.got.values().map(|a| &a.sums).collect();
        if sums.iter().all(|x| *x == sums[0]) {
            self.seen = None;
            self.stats.alike += 1;
            return;
        }
        if !l.again {
            self.seen = Some(Seen { chunk: c, got: l.got });
            self.at = now + CONFIRM_NS;
            return;
        }
        /* The second look: acted on only where every member answered as at
           the first -- written meanwhile, the chunk is let be */
        let Some(seen) = self.seen.take() else { return };
        if seen.chunk != c || seen.got != l.got {
            self.stats.skipped += 1;
            return;
        }
        /* Said once for the chunk at its set's version: found again at the
           next round as it was, it is the same finding. What was said of a
           chunk freed since, or of a set moved on, is forgotten */
        self.said.retain(|&(chunk, incarnation, epoch)| {
            s.chunks().get(chunk as usize).is_some_and(|ch| ch.incarnation == incarnation && ch.set == epoch.set)
                && s.sets().get(epoch.set as usize).is_some_and(|set| set.version == epoch.version)
        });
        if !self.said.insert((c.chunk, c.incarnation, c.epoch)) {
            return;
        }
        let unlike: Vec<u32> = l.got.iter()
            .filter(|(_, a)| l.got.values().filter(|b| b.sums == a.sums).count() == 1).map(|(&cs, _)| cs).collect();
        /* One unlike: the two others agree -- two of two, or three each
           unlike, are no two alike */
        let odd = (unlike.len() == 1).then(|| unlike[0]);
        match odd {
            Some(cs) => {
                self.stats.odd += 1;
                let host = s.cs().get(cs as usize).map_or_else(|| alloc::format!("cs {}", cs), |x| x.host.clone());
                let what = if l.got.get(&cs).is_some_and(|a| !a.held) {
                    alloc::format!("{} holds none of it, the two others alike", host)
                } else {
                    alloc::format!("{}'s copy is unlike the two others' -- no checksum says which holds the data", host)
                };
                out.push(Out::Say(alloc::format!("nbs: scrub: chunk {} of set {} (version {}): {}; `nbs replica bad {} \
                                                  {}` has it copied again", c.chunk, c.epoch.set, c.epoch.version,
                                                 what, c.epoch.set, host)));
            }
            None => {
                self.stats.differ += 1;
                out.push(Out::Say(alloc::format!("nbs: scrub: chunk {} of set {} (version {}): its {} active \
                                                  members' copies differ, no two alike", c.chunk, c.epoch.set,
                                                 c.epoch.version, l.got.len())));
            }
        }
        out.push(Out::Found { chunk: c.chunk, set: c.epoch.set, version: c.epoch.version, odd });
    }
}
