//! The controller (plans/nbs.md, "How three are kept"; N9): what the leader
//! proposes to keep every set at three active members, from the committed
//! state and what the leader sees of each cs -- a pure function of the two,
//! the same in nbs-mds as in fuzz/nbs, so that the fuzzer holds the leader's
//! plans to their rules. It keeps no plan of its own between turns: each is
//! made from the state as it is, so a new leader goes on where the old one
//! was and nothing heard twice or missed once misleads it; and a join made
//! of a view gone stale is refused where it is applied, as it names the
//! version it was planned at.
//!
//! - A cs silent `down_ms` (`DOWN_AFTER_MS` unless the cluster says
//!   otherwise) is down (`CsDown`); silent `out_ms` -- the cluster's
//!   `out_after`, held off while its host is in maintenance -- or its host no
//!   member, out (`CsOut`); answering again, up (`CsUp`).
//! - A set with a member down whose cs is up again has it join in its own
//!   place: it copies only what it missed. One whose member's cs is out, or
//!   drained, has another join in that place: the most room first, ties by
//!   a hash of the set's number and the cs's. A member down whose cs is down
//!   yet is waited for.
//! - The sets with one active member left first; at most `JOINS_PER_CS`
//!   joins with one joiner, and `JOINS_PER_TURN` proposed at a turn.
//! - A join the leader has seen stand past `join_limit_ms` given up
//!   (`Unjoin`), its set planned again at the next turn: one whose joiner
//!   answers the leader but never ends it held its set from any other join
//!   and every copy-up of its chunks for good (the review of 2026-11, its
//!   question 2).

use alloc::vec;
use alloc::vec::Vec;

use crate::place::tie;
use crate::storage::{Cs, CsState, Replica, Set, REPLICAS};
use crate::{Command, State};

/// A cs silent this long is down: writes go on to its sets' other members.
pub const DOWN_AFTER_MS: u64 = 5_000;
/// A cs silent this long is out, and its places are taken by others: the
/// cluster's `out_after` unless it says otherwise.
pub const OUT_AFTER_MS: u64 = 10 * 60 * 1000;
/// When a silent cs is taken for down, and for out: the cluster's. The
/// default is `DOWN_AFTER_MS` and `OUT_AFTER_MS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct After {
    pub down_ms: u64,
    pub out_ms: u64,
}

impl Default for After {
    fn default() -> After {
        After { down_ms: DOWN_AFTER_MS, out_ms: OUT_AFTER_MS }
    }
}

/// Joins one cs is the joiner of at once, at most.
pub const JOINS_PER_CS: usize = 2;
/// How long a join may stand, as the leader has seen it, before it is
/// given up: half an hour, and a second more for each chunk of its set --
/// 4 MiB copied at 4 MiB/s, a slow link's. A copy that moves, however
/// slowly, ends well inside it; one stuck gives itself up in five minutes
/// (nbswire's `copy::STUCK_NS`) where its joiner can say so.
pub const JOIN_MIN_MS: u64 = 30 * 60 * 1000;
pub const JOIN_PER_CHUNK_MS: u64 = 1000;

/// How long a join of a set of `chunks` chunks may stand.
pub fn join_limit_ms(chunks: u32) -> u64 {
    JOIN_MIN_MS + u64::from(chunks) * JOIN_PER_CHUNK_MS
}

/// A join as the leader has seen it: its set, its `since`, and for how long
/// it has stood -- from when this leader first saw it, so that a new one
/// gives it its whole time again, never less.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Joining {
    pub set: u32,
    pub since: u32,
    pub age_ms: u64,
}
/// Joins the leader proposes at a turn, at most.
pub const JOINS_PER_TURN: usize = 16;

/// What the leader sees of a cs at a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    pub cs: u32,
    /// Answering the leader now.
    pub answering: bool,
    /// For how long it has not answered, in ms: 0 when it does.
    pub silent_ms: u64,
    /// Its host in maintenance (the registry's flag), which holds off
    /// `out_after`.
    pub maintenance: bool,
}

/// What a set needs: the place to fill, and whether its own member fills
/// it, back, or another cs.
#[derive(Clone, Copy)]
enum Need {
    Back(usize),
    Replace(usize),
}

/// What the leader proposes at a turn of its: cs's down, up and out as it
/// sees them, silent past the cluster's `after`, and the joins it has seen
/// stand past their time (`joining`) given up -- or where none is to change,
/// joins.
pub fn plan(s: &State, seen: &[Seen], joining: &[Joining], after: After) -> Vec<Command> {
    let mut out = Vec::new();
    let view = |c: &Cs| seen.iter().find(|v| v.cs == c.id);
    for c in s.cs() {
        if s.member(&c.host).is_none() {
            if c.state != CsState::Out {
                out.push(Command::CsOut { cs: c.id });
            }
            continue;
        }
        let Some(v) = view(c) else { continue };
        let out_now = !v.answering && v.silent_ms >= after.out_ms && !v.maintenance;
        match c.state {
            CsState::Up if !v.answering && v.silent_ms >= after.down_ms => {
                out.push(if out_now { Command::CsOut { cs: c.id } } else { Command::CsDown { cs: c.id } });
            }
            CsState::Down | CsState::Out if v.answering => out.push(Command::CsUp { cs: c.id }),
            CsState::Down if out_now => out.push(Command::CsOut { cs: c.id }),
            _ => {}
        }
    }
    /* A join stood past its time, given up: its joiner answers the leader,
       or it would be down and the join over, but the copy is never ended --
       its host's mds gone, say, which a copy asks its end of */
    for (id, set) in s.sets().iter().enumerate() {
        let Some(j) = set.joiner else { continue };
        let age = joining.iter().find(|x| x.set == id as u32 && x.since == j.since).map_or(0, |x| x.age_ms);
        if age >= join_limit_ms(set.chunks) {
            out.push(Command::Unjoin { set: id as u32, cs: j.cs, since: j.since });
        }
    }
    /* A cs's change changes its sets and what a join of them may be: the
       joins wait for the next turn, planned on the state it leaves (found
       by fuzz/nbs's meta target: a join planned in the same turn as its
       cs's coming back was refused as in place); and a join given up
       frees its set for the next */
    if !out.is_empty() {
        return out;
    }

    /* A cs a join can have: up, its host a member, answering the leader now,
       not drained */
    let usable = |c: &Cs| {
        c.state == CsState::Up && s.member(&c.host).is_some() && view(c).is_some_and(|v| v.answering) && !c.draining()
    };
    let mut joins = vec![0usize; s.cs().len()];
    for set in s.sets().iter() {
        if let Some(n) = set.joiner.and_then(|j| joins.get_mut(j.cs as usize)) {
            *n += 1;
        }
    }
    /* The room this turn's joins take, which the state does not count yet */
    let mut taken = vec![0u64; s.cs().len()];
    /* The sets with the fewest members active first, the lowest-numbered
       first among them: a pass for each count, no list of every set made --
       one of 16 bytes a set was past what the kernel's heap gives at 32,768
       sets (the review of 2026-11) */
    let order = (1..=REPLICAS).flat_map(|active| {
        s.sets().iter().enumerate()
            .filter(move |(_, set)| set.joiner.is_none() && set.chunks > 0 && set.active() == active)
            .map(|(id, _)| id as u32)
    });
    let mut proposed = 0;
    for id in order {
        if proposed >= JOINS_PER_TURN {
            break;
        }
        let Some(set) = s.sets().get(id as usize) else { continue };
        let (place, cs) = match need(s, set, &usable) {
            Some(Need::Back(k)) => (k, set.cs[k]),
            Some(Need::Replace(k)) => match pick(s, id, set, &usable, &joins, &taken) {
                Some(c) => (k, c),
                None => continue,
            },
            None => continue,
        };
        let Some(n) = joins.get_mut(cs as usize).filter(|n| **n < JOINS_PER_CS) else { continue };
        *n += 1;
        if set.cs[place] != cs {
            if let Some(t) = taken.get_mut(cs as usize) {
                *t += u64::from(set.chunks);
            }
        }
        out.push(Command::Join { set: id, place: place as u8, cs, version: set.version });
        proposed += 1;
    }
    out
}

/// What set `set` needs, if anything: a member down whose cs is usable joins
/// in its own place -- the cheapest copy, of what it missed; else a member
/// down whose cs is out or of no member host, or a member whose cs is
/// drained, is replaced.
fn need(s: &State, set: &Set, usable: &dyn Fn(&Cs) -> bool) -> Option<Need> {
    let cs_of = |k: usize| s.cs().get(set.cs[k] as usize);
    for k in 0..REPLICAS {
        if set.replicas[k] == Replica::Down && cs_of(k).is_some_and(usable) {
            return Some(Need::Back(k));
        }
    }
    for k in 0..REPLICAS {
        let Some(c) = cs_of(k) else { continue };
        let gone = c.state == CsState::Out || s.member(&c.host).is_none();
        if (set.replicas[k] == Replica::Down && gone) || c.draining() {
            return Some(Need::Replace(k));
        }
    }
    None
}

/// The cs to join set `id` in another's place: usable, no member of it,
/// with fewer than JOINS_PER_CS joins and room for every chunk of the set
/// beyond what this turn's joins take; the most such room first, ties by a
/// hash of the set's number and the cs's.
fn pick(s: &State, id: u32, set: &Set, usable: &dyn Fn(&Cs) -> bool, joins: &[usize], taken: &[u64]) -> Option<u32> {
    let mut best: Option<(u64, u64, u32)> = None;
    for c in s.cs() {
        if !usable(c) || set.cs.contains(&c.id) || joins.get(c.id as usize).copied().unwrap_or(0) >= JOINS_PER_CS {
            continue;
        }
        let room = c.free().saturating_sub(taken.get(c.id as usize).copied().unwrap_or(0));
        if room < u64::from(set.chunks) {
            continue;
        }
        let key = (room, tie(id, 0, c.id));
        if best.is_none_or(|(r, t, _)| key > (r, t)) {
            best = Some((key.0, key.1, c.id));
        }
    }
    best.map(|(_, _, cs)| cs)
}

/// How far from three the chunks are: what `nbs status` counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Health {
    /// The chunks in use by how many of their set's members are active:
    /// `by_active[3]` at three, ..., `by_active[1]` at one.
    pub by_active: [u64; REPLICAS + 1],
    /// The chunks being copied onto a joiner.
    pub joining: u64,
    /// The cs's down, and out.
    pub down: u32,
    pub out: u32,
}

pub fn health(s: &State) -> Health {
    let mut h = Health::default();
    for set in s.sets().iter() {
        h.by_active[set.active()] += u64::from(set.chunks);
        if set.joiner.is_some() {
            h.joining += u64::from(set.chunks);
        }
    }
    for c in s.cs() {
        match c.state {
            CsState::Down => h.down += 1,
            CsState::Out => h.out += 1,
            CsState::Up => {}
        }
    }
    h
}
