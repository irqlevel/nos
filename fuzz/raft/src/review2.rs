//! The review of 2026-11 (at 73283772), its findings in the protocol each a
//! scenario that failed on it -- or, where the rule's API is new, on the
//! mutant that takes the rule out (`mutants.py`): a snapshot's pieces sent
//! twice and more for every heartbeat, a member whose disk kept a term
//! below its own log's and could never be elected, a probe answered and
//! never left, a vote's "no" to a request gone by counted as the answer to
//! the present one, a member that lost what it had acknowledged never
//! brought on, a voter added in one step costing its leader the term, a
//! hand-over armed to a voter nobody hears, and reads that were neither
//! confirmed nor refused. Run as scenarios, in the gate.

use std::mem;

use raft::{Body, ChangeKind, ConfChange, Entry, EntryKind, HardState, MemStorage, Message, Storage};

use crate::scenarios::{elect, finish, leader_is};
use crate::sim::{Disk, Params, Sim};

pub const ALL: &[(&str, fn() -> Result<(), String>)] = &[
    ("snapshot_pieces_bounded", snapshot_pieces_bounded),
    ("snapshot_refused_counted", snapshot_refused_counted),
    ("successors_heard", successors_heard),
    ("membership_goes_on", membership_goes_on),
    ("term_behind_log", term_behind_log),
    ("probe_leaves_on_match", probe_leaves_on_match),
    ("stale_vote_not_counted", stale_vote_not_counted),
    ("regressed_follower_repaired", regressed_follower_repaired),
    ("added_voter_heard", added_voter_heard),
    ("transfer_unreachable_refused", transfer_unreachable_refused),
    ("follower_read_refused", follower_read_refused),
    ("leader_reads_bounded", leader_reads_bounded),
];

fn ensure(ok: bool, what: impl Into<String>) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what.into()) }
}

/// Every member up ticks once.
fn tick_all(s: &mut Sim) {
    for id in s.live() {
        s.tick(id);
    }
}

/// Every member's work done and every message there is delivered, once:
/// what `count` says of each counted first.
fn round(s: &mut Sim, count: &mut u64, counted: &dyn Fn(&Message) -> bool) {
    s.process_all();
    for m in mem::take(&mut s.net) {
        if counted(&m) {
            *count += 1;
        }
        s.deliver(m);
    }
}

/// A group of three, member 1 leading, member 3 cut off while member 1
/// commits and then takes a snapshot of its state and `filler` bytes more:
/// healed, member 3 can be brought on by the snapshot alone. And the
/// snapshot's index.
fn behind_a_snapshot(seed: u64, piece: usize, filler: usize) -> Result<(Sim, u64), String> {
    let mut p = Params::plain();
    p.snapshot_piece_bytes = piece;
    behind_a_snapshot_with(p, seed, filler)
}

fn behind_a_snapshot_with(p: Params, seed: u64, filler: usize) -> Result<(Sim, u64), String> {
    let mut s = Sim::new(p, seed, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    s.isolate(3);
    for _ in 0..20 {
        s.propose(1);
    }
    s.flush(80);
    for _ in 0..2 {
        s.tick(1);
        s.tick(2);
        s.flush(20);
    }
    s.snapshot_sized(1, filler);
    let index = s.node(1).map_or(0, |n| n.storage().snapshot_meta().index);
    ensure(index > 0 && s.node(3).is_some_and(|n| n.last_index() < index), "member 3 is not behind member 1's snapshot")?;
    s.heal();
    Ok((s, index))
}

fn snapshot_in(s: &Sim, id: u64, index: u64) -> bool {
    s.node(id).is_some_and(|n| n.storage().snapshot_meta().index >= index)
}

/// A snapshot of a hundred and two pieces sent to a member while the
/// leader's heartbeats go -- one every four round trips -- nothing lost,
/// and with `doubled` its first pieces each delivered twice, as a network
/// may: each piece is sent once. (Every heartbeat sent the piece on its way
/// once more, and every answer -- a repeat too -- the piece after it: a
/// piece more on its way at each heartbeat and each piece doubled, for
/// good.)
fn pieces_sent(seed: u64, doubled: usize) -> Result<(), String> {
    const PIECE: usize = 16;
    const FILLER: usize = 1616;
    let (mut s, index) = behind_a_snapshot(seed, PIECE, FILLER)?;
    let pieces = (16 + FILLER).div_ceil(PIECE) as u64;
    let mut sent = 0u64;
    for r in 0..4000u64 {
        if r % 8 == 0 {
            tick_all(&mut s);
        }
        s.process_all();
        for m in mem::take(&mut s.net) {
            if m.from == 1 && m.to == 3 && matches!(m.body, Body::Snapshot { .. }) {
                sent += 1;
                if sent <= doubled as u64 {
                    s.deliver(m.clone());
                }
            }
            s.deliver(m);
        }
        if snapshot_in(&s, 3, index) {
            break;
        }
    }
    ensure(snapshot_in(&s, 3, index), "member 3 never took the snapshot in")?;
    ensure(sent <= pieces + 2, format!("a snapshot of {} pieces sent as {}, the first {} delivered twice, nothing lost",
                                       pieces, sent, doubled))?;
    finish(s)
}

fn snapshot_pieces_bounded() -> Result<(), String> {
    pieces_sent(201, 0)?;
    pieces_sent(211, 3)
}

/// The voters a leader hands the lead to as it leaves (`successors`) are
/// those it heard from within an election's time: of five, one cut off is
/// not among them. (raftmember took the first voter by number, heard from
/// or not: a dead one's hand-over armed again at every turn, and the group
/// took almost no proposal while it was.)
fn successors_heard() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 231, &[1, 2, 3, 4, 5], &[]);
    elect(&mut s, 1)?;
    for _ in 0..5 {
        s.propose(1);
    }
    s.flush(80);
    let all = s.node(1).map(|n| n.successors(&[2, 3, 4, 5])).unwrap_or_default();
    ensure(all.len() == 4, format!("every voter heard from, and the successors {:?}", all))?;
    s.isolate(2);
    let election = u64::from(Params::plain().election_ticks);
    let mut none = 0;
    for _ in 0..2 * election {
        tick_all(&mut s);
        round(&mut s, &mut none, &|_| false);
    }
    leader_is(&s, 1)?;
    let heard = s.node(1).map(|n| n.successors(&[2, 3, 4, 5])).unwrap_or_default();
    ensure(!heard.contains(&2) && heard.len() == 3,
           format!("member 2 cut off for two elections' time: the successors {:?}", heard))?;
    let staying = s.node(1).map(|n| n.successors(&[3, 4])).unwrap_or_default();
    ensure(staying.len() == 2 && !staying.contains(&5), format!("of 3 and 4 staying: {:?}", staying))
}

/// The leader keeps raft's membership to what its state names a step at a
/// time (`keep::next_change`): a member removed taken out before the
/// leader hands the lead on; the leader to leave hands it on where it can;
/// where it cannot, the other changes go on -- a voter that stays made.
/// (Removed with a voter-to-be still a learner, the leader stayed for
/// good: it could neither hand the lead on nor make the learner a voter.)
fn membership_goes_on() -> Result<(), String> {
    use raft::keep::{next_change, Next};
    use raft::{ChangeKind, ConfChange, ConfState};
    let conf = |voters: &[u64], learners: &[u64]| ConfState { voters: voters.to_vec(), learners: learners.to_vec() };
    let all = |_: u64| 100u64;
    let change = |kind, node| Next::Change(ConfChange { kind, node });
    /* [1], 2 added as a learner and 1 removed at once: 2 made a voter */
    let got = next_change(&conf(&[1], &[2]), 1, &[2], &[], false, &all, 100, 8);
    ensure(got == change(ChangeKind::AddVoter, 2), format!("1 removed, 2 a learner to be a voter: {:?}", got))?;
    /* ... and once 2 votes and is heard from, the lead handed to it */
    let got = next_change(&conf(&[1, 2], &[]), 1, &[2], &[], true, &all, 100, 8);
    ensure(got == Next::HandLead, format!("1 removed, 2 a voter heard from: {:?}", got))?;
    /* A member removed taken out first -- never the leader itself */
    let got = next_change(&conf(&[1, 2, 3, 4], &[]), 1, &[2, 3], &[], true, &all, 100, 8);
    ensure(got == change(ChangeKind::Remove, 4), format!("1 and 4 removed: {:?}", got))?;
    let got = next_change(&conf(&[1, 2, 3], &[]), 1, &[2, 3], &[], true, &all, 100, 8);
    ensure(got == Next::HandLead, format!("1 removed of three: {:?}", got))?;
    /* The leader named a learner hands the lead on; its own demotion is
     * never proposed by it */
    let got = next_change(&conf(&[1, 2, 3], &[]), 1, &[2, 3], &[1], false, &all, 100, 8);
    ensure(got == Next::Nothing, format!("1 to be a learner, no voter heard from: {:?}", got))?;
    /* A learner far behind is not made a voter yet */
    let got = next_change(&conf(&[1], &[2]), 1, &[1, 2], &[], false, &|n| if n == 2 { 10 } else { 100 }, 100, 8);
    ensure(got == Next::Nothing, format!("2 far behind: {:?}", got))
}

/// A follower sent a snapshot larger than it may hold refuses it at its
/// first piece -- counted, as its leader counts the refusal, for nothing
/// else would say why it stays behind -- and is sent it again no more than
/// once a heartbeat. (It took the pieces in until the heap refused one: in
/// a kernel, a panic of the machine.)
fn snapshot_refused_counted() -> Result<(), String> {
    let mut p = Params::plain();
    p.snapshot_piece_bytes = 16;
    p.max_snapshot_bytes = 1000;
    let (mut s, index) = behind_a_snapshot_with(p, 221, 1616)?;
    const TICKS: u64 = 20;
    let mut sent = 0u64;
    for r in 0..8 * TICKS {
        if r % 8 == 0 {
            tick_all(&mut s);
        }
        round(&mut s, &mut sent, &|m| m.from == 1 && m.to == 3 && matches!(m.body, Body::Snapshot { .. }));
    }
    let refused = s.node(3).map_or(0, |n| n.stats().snapshots_refused);
    let rejected = s.node(1).map_or(0, |n| n.stats().snapshots_rejected);
    ensure(!snapshot_in(&s, 3, index), "member 3 took in a snapshot larger than it may hold")?;
    ensure(refused >= 1 && rejected >= 1, format!("refused {} times, the leader told of {}", refused, rejected))?;
    ensure(refused == sent, format!("{} pieces sent, {} refusals: not refused at the first piece", sent, refused))?;
    ensure(sent <= TICKS + 2, format!("a snapshot refused sent {} times in {} heartbeats", sent, TICKS))
}

/// Members 2 and 3 of three, the leader of term 3 gone for good: member 2
/// holds its first entry and kept term 1 on its disk -- a crash between the
/// entry and the hard state written after it -- member 3 has term 3 and not
/// the entry. With PreVote they elect a leader. (Member 2's requests named
/// a last term past their own, which member 3 refused as ones that cannot
/// be, and nothing raised its term: no leader, ever.)
fn term_behind_log() -> Result<(), String> {
    let mut p = Params::plain();
    p.pre_vote = true;
    let election = u64::from(p.election_ticks);
    let mut s = Sim::new(p, 202, &[1, 2, 3], &[]);
    for id in [1, 2, 3] {
        s.crash(id);
    }
    if let Some(m) = s.members.get_mut(&1) {
        m.retired = true;
    }
    {
        let d = s.members.get_mut(&2).and_then(|m| m.disk.as_mut()).ok_or("member 2's disk")?;
        d.mem
            .append(&[Entry { term: 3, index: 4, kind: EntryKind::Noop, data: Vec::new() }])
            .map_err(|e| format!("{:?}", e))?;
        d.mem.set_hard_state(HardState { term: 1, vote: 0, commit: 3 });
    }
    {
        let d = s.members.get_mut(&3).and_then(|m| m.disk.as_mut()).ok_or("member 3's disk")?;
        d.mem.set_hard_state(HardState { term: 3, vote: 1, commit: 3 });
    }
    s.restart(2);
    s.restart(3);
    for _ in 0..40 * election {
        s.tick(2);
        s.tick(3);
        s.flush(20);
        if s.leader().is_some() {
            break;
        }
    }
    ensure(s.leader().is_some(), format!("two members of three up, one with the newest log, and no leader: {}", s.describe()))?;
    finish(s)
}

/// A member brought on by a snapshot, with nothing after it in the
/// leader's log, idle: the leader sends it nothing between heartbeats.
/// (Its probe answered with what the leader knew, it was left probing: an
/// empty append and its answer at every heartbeat, and the next entry only
/// after one.)
fn probe_leaves_on_match() -> Result<(), String> {
    let (mut s, index) = behind_a_snapshot(203, 64, 200)?;
    let mut none = 0;
    for r in 0..2000u64 {
        if r % 8 == 0 {
            tick_all(&mut s);
        }
        round(&mut s, &mut none, &|_| false);
        if snapshot_in(&s, 3, index) {
            break;
        }
    }
    ensure(snapshot_in(&s, 3, index), "member 3 never took the snapshot in")?;
    for _ in 0..3 {
        tick_all(&mut s);
        s.flush(20);
    }
    let mut appends = 0;
    let append_to_3 = |m: &Message| m.from == 1 && m.to == 3 && matches!(m.body, Body::Append { .. });
    for _ in 0..20 {
        tick_all(&mut s);
        for _ in 0..10 {
            round(&mut s, &mut appends, &append_to_3);
        }
    }
    ensure(appends <= 1, format!("{} appends to a member that holds the log, in 20 idle heartbeats", appends))?;
    finish(s)
}

/// A candidate that stood twice, its first request to a voter late: the
/// voter grants the second and refuses the first, and the candidate takes
/// the refusal first. It counts the vote it was given. (The refusal was a
/// vote's "no" of the candidate's present term, counted as the voter's
/// answer -- the first is the one counted -- and the vote given lost.)
fn stale_vote_not_counted() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 204, &[1, 2, 3], &[]);
    s.crash(1);
    let to_3 = |s: &mut Sim| -> Vec<Message> {
        let all = mem::take(&mut s.net);
        all.into_iter().filter(|m| m.to == 3 && matches!(m.body, Body::Vote { .. })).collect()
    };
    s.campaign(2);
    s.process(2);
    let first = to_3(&mut s);
    s.campaign(2);
    s.process(2);
    let second = to_3(&mut s);
    for m in second {
        s.deliver(m);
    }
    s.process(3);
    let yes = mem::take(&mut s.net);
    for m in first {
        s.deliver(m);
    }
    s.process(3);
    let no = mem::take(&mut s.net);
    ensure(s.node(3).map(|n| n.vote()) == Some(2), "member 3 did not vote for member 2")?;
    for m in no.into_iter().chain(yes) {
        s.deliver(m);
    }
    s.process(2);
    leader_is(&s, 2)?;
    s.net.clear();
    finish(s)
}

/// A follower back holding three entries fewer than it acknowledged -- its
/// disk cut where it went bad, or put back from an older copy -- brought
/// on by the next entries, with no heartbeat: a few appends. (Its refusal
/// of the entry it had acknowledged sent it the same probe again, at once,
/// for good -- and it never caught up.)
fn regressed_follower_repaired() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 205, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    for _ in 0..10 {
        s.propose(1);
    }
    s.flush(80);
    for _ in 0..3 {
        tick_all(&mut s);
        s.flush(20);
    }
    let last = s.node(1).map_or(0, |n| n.last_index());
    ensure(
        s.node(3).is_some_and(|n| n.last_index() == last) && s.node(1).and_then(|n| n.matched(3)) == Some(last),
        "member 3 does not hold the leader's log",
    )?;
    s.crash(3);
    let keep = last - 3;
    {
        let m = s.members.get_mut(&3).ok_or("member 3")?;
        let old = m.disk.take().ok_or("member 3's disk")?;
        let hs = old.mem.hard_state();
        let mut mem = MemStorage::bootstrap(&[1, 2, 3], &[]);
        let mut ents = Vec::new();
        old.mem.entries(4, keep + 1, u64::MAX, &mut ents).map_err(|e| format!("{:?}", e))?;
        mem.append(&ents).map_err(|e| format!("{:?}", e))?;
        mem.set_hard_state(HardState { commit: hs.commit.min(keep), ..hs });
        m.disk = Some(Disk::new(mem, 0, 205));
    }
    s.restart(3);
    let mut appends = 0u64;
    let append_to_3 = |m: &Message| m.from == 1 && m.to == 3 && matches!(m.body, Body::Append { .. });
    for _ in 0..3 {
        s.propose(1);
        for _ in 0..50 {
            round(&mut s, &mut appends, &append_to_3);
        }
    }
    let want = s.node(1).map_or(0, |n| n.last_index());
    let got = s.node(3).map_or(0, |n| n.last_index());
    ensure(got == want, format!("member 3, back with {} of the {} entries it acknowledged, holds {} of {} after {} appends",
                                keep, last, got, want, appends))?;
    ensure(appends <= 12, format!("member 3 brought on by {} appends", appends))?;
    ensure(s.node(1).is_some_and(|n| n.stats().regressed >= 1), "the leader did not count member 3 as regressed")?;
    finish(s)
}

/// A lone voter with CheckQuorum makes a new member a voter a tick before
/// it looks at its quorum: it leads on. (The new voter's progress was
/// "not heard from", and the look that followed cost the leader its term
/// and its change.)
fn added_voter_heard() -> Result<(), String> {
    let mut p = Params::plain();
    p.check_quorum = true;
    let election = p.election_ticks;
    let mut s = Sim::new(p, 206, &[1], &[]);
    let two = s.add_fresh();
    elect(&mut s, 1)?;
    s.propose(1);
    s.flush(20);
    while s.node(1).is_some_and(|n| n.election_elapsed() + 1 < election) {
        s.tick(1);
        s.flush(5);
    }
    s.change(1, ConfChange { kind: ChangeKind::AddVoter, node: two }).map_err(|e| e.to_string())?;
    s.tick(1);
    leader_is(&s, 1)?;
    s.flush(50);
    finish(s)
}

/// A hand-over asked to a voter not heard from in an election's time is
/// refused, and proposals go on; one to a voter that is there is taken.
/// (It was armed, refused every proposal for an election's time, given up
/// with nothing said -- and raftmember armed it again at once.)
fn transfer_unreachable_refused() -> Result<(), String> {
    let p = Params::plain();
    let election = u64::from(p.election_ticks);
    let mut s = Sim::new(p, 207, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    s.propose(1);
    s.flush(20);
    s.isolate(3);
    for _ in 0..election + 2 {
        s.tick(1);
        s.tick(2);
        s.flush(10);
    }
    match s.node_mut(1).map(|n| n.transfer_leader(3)) {
        Some(Err(raft::Error::Unreachable)) => {}
        r => return Err(format!("a hand-over to a voter not heard from in an election's time answered {:?}", r)),
    }
    ensure(s.node(1).is_some_and(|n| n.transferring() == raft::NONE), "a hand-over armed to a voter nobody hears")?;
    ensure(s.propose(1).is_some(), "a proposal refused after a hand-over to a voter nobody hears")?;
    s.flush(20);
    s.node_mut(1).ok_or("member 1 down")?.transfer_leader(2).map_err(|e| e.to_string())?;
    s.flush(50);
    leader_is(&s, 2)?;
    s.heal();
    finish(s)
}

/// A leader whose room for reads is full refuses a follower's: the
/// follower hands the refusal out, for its asker to ask again. (It dropped
/// it: the read was neither confirmed nor refused, for good.)
fn follower_read_refused() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 208, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    s.propose(1);
    s.flush(20);
    let mut taken = 0;
    while taken <= 5000 && s.read(1).is_some() {
        taken += 1;
    }
    ensure(taken > 0 && taken <= 1024, format!("the leader took {} reads", taken))?;
    s.net.clear();
    let ctx = s.read(2).ok_or("member 2 did not ask")?;
    s.flush_only(10, |m| matches!(m.body, Body::ReadIndex { .. } | Body::ReadIndexResp { .. }));
    ensure(s.reads_failed.contains(&(2, ctx)), "member 2's read, refused by the leader, was not handed out failed")?;
    finish(s)
}

/// A lone voter asked more reads before its next `Ready` than one hands
/// out: those it takes are all confirmed, and the rest refused as they are
/// asked. (Each was taken, and those past the room dropped -- neither
/// confirmed nor refused.)
fn leader_reads_bounded() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 209, &[1], &[]);
    elect(&mut s, 1)?;
    s.propose(1);
    s.flush(10);
    let mut taken = 0;
    for _ in 0..1100 {
        if s.read(1).is_some() {
            taken += 1;
        }
    }
    s.process(1);
    ensure(s.reads_open(1) == 0, format!("{} of the {} reads a lone voter took neither confirmed nor refused",
                                         s.reads_open(1), taken))?;
    ensure(taken < 1100, "every read taken, past the room a Ready hands out")?;
    finish(s)
}
