//! Rules of the node's that the review of 2026-11 took out one by one and
//! found the gate silent on -- nothing in the fuzzer looked at them -- each
//! held by a scenario now, and by a mutant in `mutants.py` that takes it out
//! and must fail it: a change written over undone, a snapshot's piece taken
//! at its own place alone, a leader that does not change itself, the lease
//! of a leader heard from, a leader with no majority stepping down, a
//! hand-over's TimeoutNow to a member that holds the log, and a refused
//! append's hint. (The eighth, a session evicted least recently used, is
//! the replicated state machine's: `rsm_session_evicted`.) Run as
//! scenarios, in the gate.

use std::mem;

use raft::{Body, ChangeKind, ConfChange, ConfState, Config, MemStorage, Message, RawNode, Role, SnapshotMeta};

use crate::scenarios::{elect, finish, leader_is};
use crate::sim::{Params, Sim};

pub const ALL: &[(&str, fn() -> Result<(), String>)] = &[
    ("change_written_over_undone", change_written_over_undone),
    ("piece_at_its_place", piece_at_its_place),
    ("leader_changes_not_itself", leader_changes_not_itself),
    ("lease_holds", lease_holds),
    ("quorum_lost_steps_down", quorum_lost_steps_down),
    ("timeout_now_when_caught_up", timeout_now_when_caught_up),
    ("hint_skips_a_term", hint_skips_a_term),
];

fn ensure(ok: bool, what: impl Into<String>) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what.into()) }
}

fn tick_all(s: &mut Sim) {
    for id in s.live() {
        s.tick(id);
    }
}

/// Every member's work done and what is on the wire delivered, once, `see`
/// shown each message first.
fn round(s: &mut Sim, see: &mut dyn FnMut(&Sim, &Message)) {
    s.process_all();
    for m in mem::take(&mut s.net) {
        see(s, &m);
        s.deliver(m);
    }
}

/// A change appended on a leader cut off -- in force there as it is
/// appended -- and written over by the next leader's log is no longer in
/// force: the member's membership goes back to the one before it. (No run
/// compared a member's membership with what its log says, and taking the
/// undoing out failed nothing.)
fn change_written_over_undone() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 301, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    s.propose(1);
    s.flush(40);
    s.isolate(1);
    s.change(1, ConfChange { kind: ChangeKind::Remove, node: 3 }).map_err(|e| e.to_string())?;
    s.flush(20);
    ensure(s.node(1).is_some_and(|n| !n.membership().voters.contains(&3)),
           "member 3's removal not in force on member 1 as it is appended")?;
    elect(&mut s, 2)?;
    s.propose(2);
    s.flush(40);
    s.heal();
    /* Member 2's heartbeats reach member 1, which is then sent the log */
    let election = u64::from(Params::plain().election_ticks);
    for _ in 0..election {
        s.tick(2);
        round(&mut s, &mut |_, _| {});
    }
    let voters = s.node(1).map(|n| n.membership().voters).unwrap_or_default();
    ensure(voters == [1, 2, 3], format!("member 1's removal of 3, written over by member 2's log: voters {:?}", voters))?;
    finish(s)
}

/// A snapshot's piece is taken at its own place alone -- one past what the
/// member has, or one it has, is not added -- so that what it takes in is
/// the leader's bytes in their order. (The simulator's snapshots were
/// filler past their first sixteen bytes, and taking a piece wherever it
/// said it went failed nothing.)
fn piece_at_its_place() -> Result<(), String> {
    let mut n = RawNode::new(Config::new(2), MemStorage::bootstrap(&[1, 2], &[])).map_err(|e| e.to_string())?;
    let bytes: Vec<u8> = (0..48u32).map(|i| (i * 7 + 3) as u8).collect();
    let meta = SnapshotMeta { index: 10, term: 2, conf: ConfState { voters: vec![1, 2], learners: vec![] } };
    let piece = |offset: usize| Message {
        from: 1,
        to: 2,
        term: 2,
        body: Body::Snapshot { meta: meta.clone(), offset: offset as u64, total: 48, data: bytes[offset..offset + 16].to_vec() },
    };
    /* The first; the third before the second; the first again; then the
     * second and the third */
    for at in [0, 32, 0, 16, 32] {
        n.step(piece(at)).map_err(|e| format!("a piece at {}: {}", at, e))?;
    }
    let rd = n.ready();
    let Some(snap) = rd.snapshot else { return Err("the snapshot's three pieces taken in, and no snapshot".into()) };
    let got: Vec<u8> = snap.data.pieces().concat();
    ensure(got == bytes, format!("pieces sent out of order taken in as {:?}, not the leader's {:?}", got, bytes))
}

/// A leader proposes no change of its own membership but its being made a
/// voter: its removal, or its being made a learner, would leave a leader
/// that is not a voter. (Taking the refusal out failed nothing: the
/// fuzzer's changes never touched the leader.)
fn leader_changes_not_itself() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 311, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    s.propose(1);
    s.flush(40);
    for kind in [ChangeKind::Remove, ChangeKind::AddLearner] {
        let r = s.change(1, ConfChange { kind, node: 1 });
        ensure(matches!(r, Err(raft::Error::ChangeRefused(raft::Refusal::ChangesLeader))),
               format!("the leader's {:?} of itself: {:?}", kind, r))?;
    }
    finish(s)
}

/// CheckQuorum's lease: a member that hears from its leader takes no vote
/// request but a hand-over's -- a member cut off from the leader alone, its
/// elections run out, deposes nobody, and the term goes on. (Taking the
/// lease out failed nothing: no run held a working leader to staying one.)
fn lease_holds() -> Result<(), String> {
    let mut p = Params::plain();
    p.check_quorum = true;
    let mut s = Sim::new(p, 321, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    s.propose(1);
    s.flush(40);
    let term = s.node(1).map_or(0, |n| n.term());
    s.cut_link(1, 3);
    let election = u64::from(Params::plain().election_ticks);
    for _ in 0..4 * election {
        tick_all(&mut s);
        round(&mut s, &mut |_, _| {});
    }
    leader_is(&s, 1)?;
    let (t1, t2) = (s.node(1).map_or(0, |n| n.term()), s.node(2).map_or(0, |n| n.term()));
    ensure(t1 == term && t2 == term, format!("member 3 cut off from the leader alone: the leader's term {} -> {}, \
                                              member 2's {}", term, t1, t2))?;
    let ignored = s.node(2).map_or(0, |n| n.stats().ignored_in_lease);
    ensure(ignored > 0, "member 3's vote requests never reached member 2 to be refused")?;
    s.heal();
    finish(s)
}

/// CheckQuorum: a leader that hears from no majority in an election's time
/// steps down. (Taking that out failed nothing either.)
fn quorum_lost_steps_down() -> Result<(), String> {
    let mut p = Params::plain();
    p.check_quorum = true;
    let mut s = Sim::new(p, 331, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    s.isolate(1);
    let election = u64::from(Params::plain().election_ticks);
    for _ in 0..2 * election + 1 {
        tick_all(&mut s);
        round(&mut s, &mut |_, _| {});
    }
    let role = s.node(1).map(|n| n.role());
    ensure(role != Some(Role::Leader), format!("a leader cut off from all for two elections' time: {:?}", role))?;
    s.heal();
    finish(s)
}

/// A hand-over's TimeoutNow goes to its member once it holds all of the
/// leader's log -- sent before, it stands with a log shorter than the
/// others' and loses, and the hand-over is for nothing. (Sending it at
/// once failed nothing.)
fn timeout_now_when_caught_up() -> Result<(), String> {
    let mut p = Params::plain();
    p.max_entries_per_msg = 2;
    let mut s = Sim::new(p, 341, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    s.cut_link(1, 3);
    for _ in 0..12 {
        s.propose(1);
    }
    s.flush(40);
    s.heal();
    /* Heard from again, then asked to take the lead while far behind */
    let mut none = |_: &Sim, _: &Message| {};
    tick_all(&mut s);
    round(&mut s, &mut none);
    s.transfer(1, 3);
    let mut early = None;
    let mut sent = false;
    for _ in 0..60 {
        round(&mut s, &mut |s, m| {
            if m.from == 1 && m.to == 3 && matches!(m.body, Body::TimeoutNow) {
                sent = true;
                let (held, last) = s.node(1).map_or((0, 0), |n| (n.matched(3).unwrap_or(0), n.last_index()));
                if held < last && early.is_none() {
                    early = Some((held, last));
                }
            }
        });
    }
    ensure(early.is_none(), format!("TimeoutNow sent to member 3 holding {:?} (held, last) of the log", early))?;
    ensure(sent, "no TimeoutNow sent to member 3 once it held the log")?;
    leader_is(&s, 3)?;
    finish(s)
}

/// A follower whose log took another turn -- many entries of a term the
/// leader has none of -- is found where it agrees in a few refused appends,
/// by its hint, not one entry a round trip. (Nothing counted the round
/// trips: decrementing by one failed nothing.)
fn hint_skips_a_term() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 351, &[1, 2, 3, 4, 5], &[]);
    elect(&mut s, 1)?;
    s.propose(1);
    s.flush(40);
    /* Member 1, cut off with member 3, appends 40 entries no majority has */
    s.split(&[1, 3]);
    for _ in 0..40 {
        s.propose(1);
    }
    s.flush(40);
    /* The other side elects member 2, which commits 40 of its own; then
     * member 4, which probes every follower from the end of that log --
     * past all of member 3's own 40 */
    elect(&mut s, 2)?;
    for _ in 0..40 {
        s.propose(2);
    }
    s.flush(80);
    elect(&mut s, 4)?;
    s.heal();
    let mut refused = 0u64;
    let election = u64::from(Params::plain().election_ticks);
    for r in 0..4 * election * 8 {
        if r % 8 == 0 {
            tick_all(&mut s);
        }
        round(&mut s, &mut |_, m| {
            if m.from == 3 && m.to == 4 && matches!(m.body, Body::AppendResp { reject: true, .. }) {
                refused += 1;
            }
        });
    }
    leader_is(&s, 4)?;
    let (held, last) = s.node(4).map_or((0, 0), |n| (n.matched(3).unwrap_or(0), n.last_index()));
    ensure(held == last, format!("member 3 holds {} of the leader's {} entries", held, last))?;
    ensure(refused <= 4, format!("member 3 brought on by {} refused appends", refused))?;
    finish(s)
}
