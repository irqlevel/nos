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
//! - A cs silent `DOWN_AFTER_MS` is down (`CsDown`); silent `out_after` --
//!   held off while its host is in maintenance -- or its host no member, out
//!   (`CsOut`); answering again, up (`CsUp`).
//! - A set with a member down whose cs is up again has it join in its own
//!   place: it copies only what it missed. One whose member's cs is out, or
//!   drained, has another join in that place: the most room first, ties by
//!   a hash of the set's number and the cs's. A member down whose cs is down
//!   yet is waited for.
//! - The sets with one active member left first; at most `JOINS_PER_CS`
//!   joins with one joiner, and `JOINS_PER_TURN` proposed at a turn.

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
/// Joins one cs is the joiner of at once, at most.
pub const JOINS_PER_CS: usize = 2;
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
/// sees them -- or where none is to change, joins. `out_after_ms` is the
/// cluster's `out_after`.
pub fn plan(s: &State, seen: &[Seen], out_after_ms: u64) -> Vec<Command> {
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
        let out_now = !v.answering && v.silent_ms >= out_after_ms && !v.maintenance;
        match c.state {
            CsState::Up if !v.answering && v.silent_ms >= DOWN_AFTER_MS => {
                out.push(if out_now { Command::CsOut { cs: c.id } } else { Command::CsDown { cs: c.id } });
            }
            CsState::Down | CsState::Out if v.answering => out.push(Command::CsUp { cs: c.id }),
            CsState::Down if out_now => out.push(Command::CsOut { cs: c.id }),
            _ => {}
        }
    }
    /* A cs's change changes its sets and what a join of them may be: the
       joins wait for the next turn, planned on the state it leaves (found
       by fuzz/nbs's meta target: a join planned in the same turn as its
       cs's coming back was refused as in place) */
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
    let mut order: Vec<(usize, u32)> = s.sets().iter().enumerate()
        .filter(|(_, set)| set.joiner.is_none() && set.chunks > 0)
        .map(|(id, set)| (set.active(), id as u32)).collect();
    order.sort_unstable();
    let mut proposed = 0;
    for (_, id) in order {
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
