//! The known traps, each played out step by step: the schedule that breaks
//! a Raft missing one rule, run against this one, which must come through
//! with nothing found. Each is aimed at a rule a mutant takes out
//! (`scripts/raft-mutants.py`), and the mutant must not come through.

use raft::{Body, ChangeKind, ConfChange, Entry, EntryKind, MemStorage, Message, NodeId, Role};

use crate::sim::{Disk, Params, Sim};

pub const ALL: &[(&str, fn() -> Result<(), String>)] = &[
    ("figure8", figure8),
    ("stale_leader_read", stale_leader_read),
    ("change_before_first_commit", change_before_first_commit),
    ("snapshot_overtakes_log", snapshot_overtakes_log),
    ("wiped_member_same_id", wiped_member_same_id),
    ("vote_persisted", vote_persisted),
    ("timer_reset_on_vote", timer_reset_on_vote),
    ("commit_past_match", commit_past_match),
    ("prevote_not_a_vote", prevote_not_a_vote),
    ("append_prev_term_mismatch", append_prev_term_mismatch),
    ("force_alone_after_loss", force_alone_after_loss),
];

fn votes(m: &Message) -> bool {
    matches!(m.body, Body::Vote { .. } | Body::VoteResp { .. })
}

fn between(m: &Message, a: NodeId, b: NodeId) -> bool {
    (m.from == a && m.to == b) || (m.from == b && m.to == a)
}

fn carries(m: &Message, index: u64) -> bool {
    matches!(&m.body, Body::Append { entries, .. } if entries.iter().any(|e| e.index == index))
}

fn involves(m: &Message, id: NodeId) -> bool {
    m.from == id || m.to == id
}

/// Nothing found, and every member back to agreeing: the scenario's end.
fn finish(mut s: Sim) -> Result<(), String> {
    if s.failures.is_empty() {
        s.check_logs();
    }
    if s.failures.is_empty() && !s.converge(400) {
        let d = s.describe();
        s.fail(format!("no progress once healed: {}", d));
    }
    if s.failures.is_empty() { Ok(()) } else { Err(s.failures.join("; ")) }
}

fn leader_is(s: &Sim, id: NodeId) -> Result<(), String> {
    match s.node(id).map(|n| n.role()) {
        Some(Role::Leader) => Ok(()),
        r => Err(format!("member {} was to lead, and is {:?} (failures so far: {:?})", id, r, s.failures)),
    }
}

/// Member `id` elected with every message delivered.
fn elect(s: &mut Sim, id: NodeId) -> Result<(), String> {
    s.campaign(id);
    s.flush(100);
    leader_is(s, id)
}

/// The paper's figure 8: an entry of an earlier term on a majority, which
/// a leader must not count as committed -- a later leader may still write
/// over it.
fn figure8() -> Result<(), String> {
    let mut p = Params::plain();
    p.max_entries_per_msg = 1;
    let mut s = Sim::new(p, 8, &[1, 2, 3, 4, 5], &[]);
    elect(&mut s, 1)?;
    /* (a) An entry, A, of S1's term on S1 and S2 alone; then S1 goes down. */
    s.propose_data(1, b"a").map_err(|e| e.to_string())?;
    s.flush_only(50, |m| between(m, 1, 2));
    s.crash(1);
    s.net.clear();
    /* (b) S5 elected in the next term by S3, S4 and itself; its own entry at
     * A's index goes nowhere, and it goes down. */
    s.campaign(5);
    s.flush_only(20, |m| votes(m) && !involves(m, 1));
    leader_is(&s, 5)?;
    s.process(5);
    s.net.clear();
    s.crash(5);
    /* (c) S1 back and elected in the term after -- by S2 at once and then by
     * S3 and S4 too -- and A replicated to S3 and S4, but not the entry
     * after it, its new term's: A is on a majority, and is not committed. */
    s.restart(1);
    for _ in 0..3 {
        if s.node(1).is_some_and(|n| n.role() == Role::Leader) {
            break;
        }
        s.campaign(1);
        s.flush_only(20, |m| votes(m) && !involves(m, 5));
    }
    leader_is(&s, 1)?;
    let own = s.node(1).map_or(0, |n| n.last_index());
    for _ in 0..60 {
        /* A heartbeat a round: its first appends went with the votes. */
        s.tick(1);
        s.process_all();
        let msgs = std::mem::take(&mut s.net);
        for m in msgs {
            if !(between(&m, 1, 3) || between(&m, 1, 4)) {
                continue;
            }
            /* S1's own entry goes only where it is refused: to a member that
             * lacks the entry before it. */
            if carries(&m, own) {
                let prev = if let Body::Append { prev_index, .. } = &m.body { *prev_index } else { 0 };
                if s.node(m.to).is_some_and(|n| n.last_index() >= prev) {
                    continue;
                }
            }
            s.deliver(m);
        }
    }
    s.crash(1);
    s.net.clear();
    /* (d) S5 back and elected by S2, S3 and S4: it writes its entry over A.
     * Had S1 committed A, a committed entry would change. */
    s.restart(5);
    for _ in 0..4 {
        if s.node(5).is_some_and(|n| n.role() == Role::Leader) {
            break;
        }
        s.campaign(5);
        s.flush_only(20, |m| votes(m) && !involves(m, 1));
    }
    leader_is(&s, 5)?;
    s.flush_only(100, |m| !involves(m, 1));
    s.restart(1);
    finish(s)
}

/// A leader cut off and replaced, asked for a read: it must not confirm one
/// from what it knew -- the new leader has committed more since.
fn stale_leader_read() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 9, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    for _ in 0..3 {
        s.propose(1);
    }
    s.flush(50);
    s.isolate(1);
    elect(&mut s, 2)?;
    for _ in 0..3 {
        s.propose(2);
    }
    s.flush(50);
    /* Member 1 still thinks it leads (no CheckQuorum): a read asked of it
     * must wait for a round no majority answers. */
    s.read(1);
    s.flush(50);
    /* A read through a follower of the real leader is confirmed. */
    s.read(3);
    s.flush(50);
    s.heal();
    s.flush(50);
    finish(s)
}

/// The 2015 erratum: a leader that makes a membership change before it has
/// committed an entry of its term can lose to a change an earlier leader
/// left in the log -- two majorities that do not meet, two leaders that
/// commit.
fn change_before_first_commit() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 15, &[1, 2, 3, 4], &[]);
    s.retire_on_removal = false;
    s.add(5, MemStorage::new());
    elect(&mut s, 1)?;
    /* S1 adds S5: the change reaches S5 alone. */
    s.change(1, ConfChange { kind: ChangeKind::AddVoter, node: 5 }).map_err(|e| e.to_string())?;
    s.flush_only(50, |m| between(m, 1, 5));
    s.net.clear();
    s.split(&[1, 5]);
    /* S2 elected by S2, S3 and S4 under the old membership; its first entry
     * reaches S3 alone, not committed. */
    s.campaign(2);
    s.flush_only(20, |m| votes(m) || between(m, 2, 3));
    leader_is(&s, 2)?;
    s.cut_link(2, 4);
    /* At once -- before its term's first entry is committed, which S4 now
     * cannot help with -- S2 is asked to take S4 out. */
    let early = s.change(2, ConfChange { kind: ChangeKind::Remove, node: 4 });
    if early.is_ok() {
        s.note_refusal_missing();
    }
    let _ = s.propose_data(2, b"x");
    s.flush_only(50, |m| between(m, 2, 3));
    s.net.clear();
    /* S1 comes back with S4 and S5 -- a majority of the membership it has --
     * no longer the leader it was, and stands. */
    s.crash(1);
    s.restart(1);
    s.heal();
    s.split(&[1, 4, 5]);
    for _ in 0..4 {
        if s.node(1).is_some_and(|n| n.role() == Role::Leader) {
            break;
        }
        s.campaign(1);
        s.flush_only(20, |m| votes(m));
    }
    leader_is(&s, 1)?;
    s.flush(50);
    s.heal();
    s.flush(100);
    finish(s)
}

/// A follower far behind is sent snapshots in pieces -- lost, doubled,
/// reordered -- while the leader moves on and takes newer ones; a stale
/// snapshot coming late changes nothing.
fn snapshot_overtakes_log() -> Result<(), String> {
    let mut p = Params::plain();
    p.snapshot_piece_bytes = 64;
    let mut s = Sim::new(p, 22, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    s.isolate(3);
    for _ in 0..30 {
        s.propose(1);
    }
    s.flush(80);
    s.snapshot(1);
    s.snapshot(2);
    s.heal();
    s.p.dup_pct = 20;
    s.p.drop_pct = 10;
    let mut old: Vec<Message> = Vec::new();
    for round in 0..400u64 {
        if round % 5 == 0 {
            s.tick(1);
        }
        if round == 40 {
            for _ in 0..10 {
                s.propose(1);
            }
        }
        if round == 80 {
            s.snapshot(1);
            s.snapshot(2);
        }
        s.process_all();
        if !s.net.is_empty() {
            let i = s.rng.below(s.net.len() as u64) as usize;
            if matches!(s.net[i].body, Body::Snapshot { .. }) && old.len() < 32 {
                old.push(s.net[i].clone());
            }
            s.deliver_at(i, true);
        }
    }
    s.p.dup_pct = 0;
    s.p.drop_pct = 0;
    s.flush(200);
    /* The pieces of an older snapshot, late: taken for nothing. */
    for m in old {
        s.deliver(m);
    }
    s.flush(50);
    finish(s)
}

/// A member that lost its disk and came back under its old id forgets the
/// vote it gave: two leaders in one term. The checks must see it -- and
/// the harness refuses to do it, as a deployment gives a new incarnation a
/// new member.
fn wiped_member_same_id() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 31, &[1, 2, 3], &[]);
    if s.wipe(1, MemStorage::bootstrap(&[1, 2, 3], &[])).is_ok() {
        return Err("a member wiped and back under its old id was let in".into());
    }
    s.allow_wipe = true;
    s.campaign(2);
    s.flush_only(20, |m| votes(m) && between(m, 1, 2));
    leader_is(&s, 2)?;
    s.wipe(1, MemStorage::bootstrap(&[1, 2, 3], &[]))?;
    s.campaign(3);
    s.flush_only(20, |m| votes(m) && between(m, 1, 3));
    if s.failures.iter().any(|f| f.contains("two leaders")) {
        Ok(())
    } else {
        Err(format!("a wiped member voted twice and the checks saw nothing: {:?}", s.failures))
    }
}

/// A vote is on the disk before it is answered: a member that crashes after
/// voting must not vote for another in the same term.
fn vote_persisted() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 41, &[1, 2, 3], &[]);
    s.campaign(2);
    s.flush_only(20, |m| votes(m) && between(m, 1, 2));
    leader_is(&s, 2)?;
    s.net.clear();
    s.crash(1);
    s.restart(1);
    s.campaign(3);
    s.flush_only(20, |m| votes(m) && between(m, 1, 3));
    s.net.clear();
    finish(s)
}

/// A vote given starts the election timer over, even when the term does
/// not change: a voter that stood a tick later would depose the leader it
/// just elected.
fn timer_reset_on_vote() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 53, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    s.isolate(3);
    for _ in 0..3 {
        s.propose(1);
    }
    s.flush(50);
    /* Member 1 back as a follower of its term, its log up to date. */
    s.crash(1);
    s.restart(1);
    s.isolate(1);
    s.net.clear();
    /* Member 3, behind, stands in the next term: member 2 takes the term
     * and turns it down. */
    s.cut.remove(&(3, 2));
    s.campaign(3);
    s.process(3);
    s.deliver_where(|m| m.from == 3 && m.to == 2);
    s.process(2);
    s.net.clear();
    let term = s.node(2).map_or(0, |n| n.term());
    /* Member 2 waits to a tick short of its timeout ... */
    let wait = s.node(2).map_or(0, |n| n.randomized_election_timeout().saturating_sub(1));
    for _ in 0..wait {
        s.tick(2);
    }
    /* ... and votes for member 1, standing in that same term. */
    s.cut.remove(&(1, 2));
    s.campaign(1);
    s.process(1);
    if s.node(1).map(|n| n.term()) != Some(term) {
        return Err(format!("member 1 stood in term {:?}, not {}", s.node(1).map(|n| n.term()), term));
    }
    s.deliver_where(|m| m.from == 1 && m.to == 2 && votes(m));
    if s.node(2).map(|n| n.vote()) != Some(1) {
        return Err("member 2 did not vote for member 1".into());
    }
    s.process(2);
    s.net.clear();
    /* A tick later it must not stand. */
    s.tick(2);
    s.heal();
    finish(s)
}

/// A follower commits no further than the append in hand has shown to
/// match: what follows in its log may be an older leader's, never
/// committed.
fn commit_past_match() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 67, &[1, 2, 3, 4, 5], &[]);
    elect(&mut s, 1)?;
    /* An entry, A, on 1, 2 and 3: committed, and known so by all three. */
    let a = s.propose_data(1, b"a").map_err(|e| e.to_string())?;
    s.flush_only(50, |m| between(m, 1, 2) || between(m, 1, 3));
    for _ in 0..2 {
        s.tick(1);
        s.flush_only(50, |m| between(m, 1, 2) || between(m, 1, 3));
    }
    /* B after it on 1, 2 and 3 -- their answers lost, so not known to be
     * committed -- and C after B on 1 and 3 alone. */
    let b = s.propose_data(1, b"b").map_err(|e| e.to_string())?;
    s.flush_only(20, |m| m.from == 1 && (m.to == 2 || m.to == 3) && matches!(m.body, Body::Append { .. }));
    s.propose_data(1, b"c").map_err(|e| e.to_string())?;
    s.flush_only(20, |m| m.from == 1 && m.to == 3 && matches!(m.body, Body::Append { .. }));
    s.net.clear();
    s.split(&[1, 3]);
    /* Member 2, which has B, elected by 2, 4 and 5: its own entry after B
     * committed, and B with it. */
    s.campaign(2);
    s.flush(50);
    leader_is(&s, 2)?;
    let term = s.node(2).map_or(0, |n| n.term());
    let commit = s.node(2).map_or(0, |n| n.committed());
    if commit <= b {
        return Err(format!("member 2 committed {} only", commit));
    }
    if s.node(3).is_none_or(|n| n.committed() > a || n.last_index() <= b) {
        return Err("member 3 is not as the scenario needs it: committed to A, holding B and C".into());
    }
    /* An append member 2 could have sent member 3 -- B after A, its commit
     * past B -- arriving now: member 3 has B, and after it C, member 1's,
     * which is not the entry committed there. */
    let (entries, prev_term) = match s.node(2) {
        Some(n) => (n.entries(b, b + 1, u64::MAX).unwrap_or_default(), n.term_at(a).unwrap_or(0)),
        None => return Err("member 2 is gone".into()),
    };
    s.heal();
    s.deliver(Message { from: 2, to: 3, term, body: Body::Append { prev_index: a, prev_term, commit, entries } });
    s.process(3);
    s.flush(100);
    finish(s)
}

/// A PreVote is a question, not a vote: two members that both could win
/// may not both lead the next term.
fn prevote_not_a_vote() -> Result<(), String> {
    let mut p = Params::plain();
    p.pre_vote = true;
    let mut s = Sim::new(p, 71, &[1, 2, 3], &[]);
    elect(&mut s, 1)?;
    s.crash(1);
    s.net.clear();
    s.campaign(2);
    s.campaign(3);
    s.process(2);
    s.process(3);
    /* Each asks the other, and each says it could win. */
    s.deliver_where(|m| between(m, 2, 3) && matches!(m.body, Body::Vote { pre: true, .. }));
    s.process(2);
    s.process(3);
    s.deliver_where(|m| between(m, 2, 3) && matches!(m.body, Body::VoteResp { pre: true, .. }));
    s.flush_only(50, |m| !involves(m, 1));
    s.restart(1);
    finish(s)
}

/// An append is taken only after an entry of the term it names: a follower
/// whose entry there is another term's has a log that went another way.
fn append_prev_term_mismatch() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 83, &[1, 2, 3, 4, 5], &[]);
    elect(&mut s, 1)?;
    /* An entry, A, of member 1's term on 1 and 3 alone. */
    s.propose_data(1, b"a").map_err(|e| e.to_string())?;
    s.flush_only(50, |m| between(m, 1, 3));
    s.net.clear();
    s.split(&[1, 3]);
    /* Member 2 elected in the next term by 2, 4 and 5: its own entry at A's
     * index reaches member 4 alone, and it goes down. */
    s.campaign(2);
    s.flush_only(20, |m| votes(m) || between(m, 2, 4));
    leader_is(&s, 2)?;
    if s.node(4).map(|n| n.last_index()) != s.node(2).map(|n| n.last_index()) {
        return Err("member 4 did not get member 2's entry".into());
    }
    s.crash(2);
    s.net.clear();
    /* Member 4 elected by 4, 5 and 3: its log, with member 2's entry there,
     * is newer than member 3's. Its first append to member 3 names that
     * entry, which member 3 has of another term. */
    s.heal();
    s.isolate(1);
    s.campaign(4);
    s.flush_only(20, |m| votes(m));
    leader_is(&s, 4)?;
    s.flush(100);
    s.restart(2);
    s.heal();
    finish(s)
}

impl Sim {
    /// A membership change taken before the leader's first commit: the 2015
    /// rule broken, which the run that follows will show.
    fn note_refusal_missing(&mut self) {
        if self.trace {
            eprintln!("a change was taken before the leader's first commit of its term");
        }
    }
}

/// A Normal entry: what the unit checks append by hand.
pub fn entry(index: u64, term: u64, data: &[u8]) -> Entry {
    Entry { index, term, kind: EntryKind::Normal, data: data.to_vec() }
}

/// Recover (plans/cluster.md, "When the majority is gone"): a group of
/// three voters and a learner whose other members are lost for good, the
/// third voter made a group
/// of itself alone by `raft::recover::force_alone` -- every other member
/// taken out by changes it appends, its log committed to its end. It
/// elects itself and commits on its own; what it held is kept, and what it
/// had appended that the group never committed is committed now; the two
/// taken out never come back. A learner is no group's last voter, and is
/// refused.
fn force_alone_after_loss() -> Result<(), String> {
    let mut s = Sim::new(Params::plain(), 97, &[1, 2, 3], &[4]);
    elect(&mut s, 1)?;
    for _ in 0..4 {
        s.propose(1);
    }
    s.flush(50);
    /* One more, on members 1 and 3 alone, and never committed */
    s.propose_data(1, b"late").map_err(|e| e.to_string())?;
    s.flush_only(10, |m| m.from == 1 && m.to == 3 && matches!(m.body, Body::Append { .. }));
    s.net.clear();
    let (last, committed) = s.node(3).map(|n| (n.last_index(), n.committed())).ok_or("member 3 down")?;
    if last <= committed {
        return Err(format!("the late entry not on member 3 uncommitted: last {}, committed {}", last, committed));
    }
    for id in [1, 2, 3, 4] {
        s.crash(id);
    }
    for id in [1, 2, 4] {
        if let Some(m) = s.members.get_mut(&id) {
            m.retired = true;
        }
    }
    let m = s.members.get_mut(&3).ok_or("member 3 gone")?;
    let disk = m.disk.take().ok_or("member 3's disk gone")?;
    let (mem, forced) = raft::recover::force_alone(disk.mem, 3).map_err(|e| format!("{:?}", e))?;
    if forced.removed != [1, 2, 4] || forced.committed_before != committed || forced.last != last + 3 {
        return Err(format!("forced {:?}, the log's last entry {} and its commit {}", forced, last, committed));
    }
    m.disk = Some(Disk::new(mem, 0, 97));
    s.restart(3);
    let n = s.node(3).ok_or("member 3 did not start again")?;
    let conf = n.membership();
    if conf.voters != [3] || !conf.learners.is_empty() || n.committed() != forced.last {
        return Err(format!("member 3 forced alone: {:?}, committed {}", conf, n.committed()));
    }
    let late = n.entries(last, last + 1, u64::MAX).map_err(|e| e.to_string())?;
    if late.first().map(|e| e.data.as_slice()) != Some(&b"late"[..]) {
        return Err(String::from("the late entry not kept"));
    }
    elect(&mut s, 3)?;
    let before = s.node(3).map_or(0, |n| n.committed());
    s.propose(3);
    s.flush(20);
    if s.node(3).map_or(0, |n| n.committed()) <= before {
        return Err(String::from("member 3, alone, commits nothing"));
    }
    match raft::recover::force_alone(MemStorage::bootstrap(&[1, 2], &[3]), 3) {
        Err(raft::recover::ForceError::NotAVoter) => {}
        other => return Err(format!("a learner forced alone: {:?}", other.map(|(_, f)| f))),
    }
    finish(s)
}
