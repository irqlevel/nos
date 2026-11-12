//! raft::asks -- a member's own commands asked of its group, a session each
//! -- as raftmember runs it: groups of three or five over raft's rsm on a
//! network that delays, loses, doubles and cuts, every member with asks of
//! its own. A member that does not lead hands what it is told to send to
//! whom it thinks leads, over the same network; the leader refuses a
//! command now and then, as raftmember's `before_submit` may, and says so
//! back, and tells the member what it did for it as it applies it
//! (`asks::Tell`). Snapshots are taken often, so that a member cut off for
//! long comes back by one, the applications it skipped never seen.
//!
//! The state machine counts each ask's applications; its answer is the
//! count, for some asks too long for a session to keep. The judge: no ask
//! applied twice, on any member; one answered `Applied` applied once, with
//! the answer of that one application; `Lost` applied once; `NotAsked`
//! never; and, the network calm, every ask asked then answered `Applied` or
//! `Lost`, and every member's state the same.

use std::collections::BTreeMap;

use raft::asks::{Asks, Done, Outcome, Send, Tell, Timing, Told, SEQ};
use raft::{
    Body, Config, Event, Frozen, MemStorage, Message, NodeId, RestoreError, Role, Rsm, SnapshotReader, StateMachine,
};

use crate::rng::{mix, Rng};

/// An answer past what a session keeps (`raft::rsm::MAX_ANSWER`): the
/// long asks', whose retry after an application not seen is `Lost`.
const LONG: usize = 1100;
/// The log's bytes since the last snapshot before the next: small, so that
/// a member cut off comes back by one.
const FLOOR: u64 = 600;
/// Steps of a run that are calm at its end: no loss, no cut, no doubles;
/// asks made only in its first part, each to be answered by its end.
const CALM: u64 = 1500;
const CALM_ASKS: u64 = 700;
/// An ask's life, its resend and its retry, in steps: a step is a tick.
const TIMING: Timing = Timing { total: 300, resend: 25, retry: 3 };
/// What the leader says when it refuses a command.
const BUSY: &str = "busy -- ask again in a moment";

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/* ---- the state machine: each ask's applications counted ---- */

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    counts: BTreeMap<u64, u32>,
}

pub struct FrozenTally {
    bytes: Vec<u8>,
    at: usize,
}

impl Frozen for FrozenTally {
    fn write(&mut self, max: usize, out: &mut Vec<u8>) {
        let n = (self.bytes.len() - self.at).min(max).min(256);
        out.extend_from_slice(&self.bytes[self.at..self.at + n]);
        self.at += n;
    }
}

/// An ask's command: its id, and whether its answer is long.
fn command(id: u64, long: bool) -> Vec<u8> {
    let mut c = id.to_le_bytes().to_vec();
    c.push(u8::from(long));
    c
}

impl StateMachine for Tally {
    type Frozen = FrozenTally;

    fn apply(&mut self, _index: u64, command: &[u8]) -> Vec<u8> {
        if command.len() != 9 {
            return vec![0xFF];
        }
        let c = self.counts.entry(le64(&command[..8])).or_insert(0);
        *c += 1;
        let mut answer = c.to_le_bytes().to_vec();
        if command[8] == 1 {
            answer.resize(4 + LONG, 0xA5);
        }
        answer
    }

    fn query(&self, _query: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    fn size(&self) -> u64 {
        8 + 12 * self.counts.len() as u64
    }

    fn freeze(&self) -> FrozenTally {
        let mut bytes = Vec::with_capacity(8 + 12 * self.counts.len());
        bytes.extend_from_slice(&(self.counts.len() as u64).to_le_bytes());
        for (id, c) in &self.counts {
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.extend_from_slice(&c.to_le_bytes());
        }
        FrozenTally { bytes, at: 0 }
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>) -> Result<(), RestoreError> {
        let n = r.u64().ok_or(RestoreError("a tally's snapshot cut short"))?;
        if n.checked_mul(12) != Some(r.remaining()) {
            return Err(RestoreError("a tally's snapshot of another length"));
        }
        let mut counts = BTreeMap::new();
        for _ in 0..n {
            let mut b = [0u8; 12];
            if !r.read_exact(&mut b) {
                return Err(RestoreError("a tally's snapshot cut short"));
            }
            counts.insert(le64(&b[..8]), u32::from_le_bytes([b[8], b[9], b[10], b[11]]));
        }
        self.counts = counts;
        Ok(())
    }
}

/* ---- the world ---- */

/// What goes between members.
#[derive(Clone, Debug)]
pub enum Msg {
    Raft(Message),
    /// What an ask's member hands the leader.
    Session(Send),
    /// The leader's refusal of a command.
    Refused { client: u64, why: String },
    /// What the leader that applied an entry of an ask's tells its member.
    Told(Told),
}

struct Member {
    id: NodeId,
    rsm: Rsm<MemStorage, Tally>,
    asks: Asks<u64>,
    /// As the leader, the members owed word of what it did for their asks.
    tell: Tell<NodeId>,
    /// The leader as last seen: a change is said to the asks.
    leader: NodeId,
}

struct World {
    r: Rng,
    now: u64,
    members: Vec<Member>,
    /// In flight: when due, from, to.
    wire: Vec<(u64, NodeId, NodeId, Msg)>,
    /// Losses, doubles and delays; and the leader's refusals, in a hundred.
    lossy: bool,
    veto: u64,
    /// Every ask: its member, when, whether long; and what became of it.
    asked: BTreeMap<u64, (NodeId, u64, bool)>,
    outcomes: BTreeMap<u64, Outcome>,
    /// Asks refused at once, too many in flight.
    full: u64,
    /// Commands submitted by a leader: of an ask's session, at an index.
    submitted: Vec<(NodeId, u64, u64)>,
    /// Applications of one session's command seen by one member: past the
    /// first, a try asked again answered from the session.
    applied_seen: BTreeMap<(NodeId, u64), u32>,
    dups: u64,
    expired: u64,
    leader_changes: u64,
    /// The leader's words that reached their asker.
    told: u64,
    /// Sessions made, as each member saw them: the member, the origin, the
    /// session.
    registered: Vec<(NodeId, NodeId, u64)>,
}

impl World {
    fn new(seed: u64, n: u64, timing: Timing) -> Result<World, String> {
        let voters: Vec<NodeId> = (1..=n).collect();
        let mut r = Rng::new(mix(seed, 0x6173_6b73));
        let mut members = Vec::new();
        for id in 1..=n {
            let mut cfg = Config::new(id);
            cfg.seed = mix(seed, id);
            let mut rsm = Rsm::new(cfg, MemStorage::bootstrap(&voters, &[]), Tally::default(), r.next())
                .map_err(|e| format!("member {}: {:?}", id, e))?;
            rsm.set_snapshot_floor(FLOOR);
            /* Each member's request numbers from a start of its own */
            let asks = Asks::new(id, r.next() | 1, timing);
            members.push(Member { id, rsm, asks, tell: Tell::new(), leader: raft::NONE });
        }
        Ok(World { r, now: 0, members, wire: Vec::new(), lossy: false, veto: 0, asked: BTreeMap::new(),
                   outcomes: BTreeMap::new(), full: 0, submitted: Vec::new(), applied_seen: BTreeMap::new(), dups: 0,
                   expired: 0, leader_changes: 0, told: 0, registered: Vec::new() })
    }

    fn at(&self, id: NodeId) -> usize {
        (id - 1) as usize
    }

    fn send(&mut self, from: NodeId, to: NodeId, m: Msg) {
        if self.lossy && self.r.chance(5) {
            return;
        }
        let copies = if self.lossy && self.r.chance(2) { 2 } else { 1 };
        for _ in 0..copies {
            let delay = if self.lossy { self.r.range(1, 4) } else { 1 };
            self.wire.push((self.now + delay, from, to, m.clone()));
        }
    }

    /// Ask `id` made on member `on`.
    fn ask(&mut self, on: NodeId, id: u64, long: bool) {
        let now = self.now;
        let i = self.at(on);
        self.asked.insert(id, (on, now, long));
        if self.members[i].asks.ask(command(id, long), id, now).is_err() {
            self.full += 1;
        }
    }

    fn record(&mut self, done: Vec<Done<u64>>) -> Result<(), String> {
        for d in done {
            if let Some(o) = self.outcomes.insert(d.waiter, d.outcome) {
                return Err(format!("ask {} told twice: first {:?}", d.waiter, o));
            }
        }
        Ok(())
    }

    /// As the leader `i`, what an ask's member sent -- `from`, or itself.
    fn lead(&mut self, i: usize, from: NodeId, s: Send) {
        if self.members[i].rsm.role() != Role::Leader {
            return;
        }
        let me = self.members[i].id;
        match s {
            Send::Register { request } => {
                if self.members[i].rsm.register_for(from, request).is_ok() && from != me {
                    self.members[i].tell.registering(from, request, from);
                }
            }
            Send::Command { client, command } => {
                if self.veto > 0 && self.r.chance(self.veto) {
                    if from == me {
                        let now = self.now;
                        self.members[i].asks.refused(client, String::from(BUSY), now);
                    } else {
                        self.send(me, from, Msg::Refused { client, why: String::from(BUSY) });
                    }
                    return;
                }
                if let Ok(index) = self.members[i].rsm.submit(client, SEQ, &command) {
                    self.submitted.push((me, client, index));
                    if from != me {
                        self.members[i].tell.submitted(client, from);
                    }
                }
            }
            Send::Unregister { client } => {
                let _ = self.members[i].rsm.unregister(client);
            }
        }
    }

    /// A step: arrivals -- those `pass` lets through -- then a tick and a
    /// `process` of every member, their events to their asks, and then
    /// their asks' turns.
    fn step(&mut self, pass: &mut dyn FnMut(NodeId, NodeId, &Msg) -> bool) -> Result<(), String> {
        self.now += 1;
        let now = self.now;
        let mut due = Vec::new();
        let mut k = 0;
        while k < self.wire.len() {
            if self.wire[k].0 <= now {
                due.push(self.wire.swap_remove(k));
            } else {
                k += 1;
            }
        }
        while !due.is_empty() {
            let at = if self.lossy { self.r.below(due.len() as u64) as usize } else { 0 };
            let (_, from, to, m) = due.remove(at);
            if !pass(from, to, &m) {
                continue;
            }
            let i = self.at(to);
            match m {
                Msg::Raft(rm) => {
                    let _ = self.members[i].rsm.step(rm);
                }
                Msg::Session(s) => self.lead(i, from, s),
                Msg::Refused { client, why } => {
                    /* From the leader this member knows, and no other */
                    if self.members[i].rsm.leader() == from {
                        self.members[i].asks.refused(client, why, now);
                    }
                }
                Msg::Told(told) => {
                    self.told += 1;
                    let mut done = Vec::new();
                    self.members[i].asks.told(told, &mut done);
                    self.record(done)?;
                }
            }
        }
        for i in 0..self.members.len() {
            let id = self.members[i].id;
            self.members[i].rsm.tick();
            let out = self.members[i].rsm.process().map_err(|e| format!("member {} stopped: {:?}", id, e))?;
            for m in out.messages {
                let to = m.to;
                self.send(id, to, Msg::Raft(m));
            }
            for e in out.events {
                match e {
                    Event::Applied { client, seq: SEQ, .. } => {
                        let n = self.applied_seen.entry((id, client)).or_insert(0);
                        *n += 1;
                        if *n > 1 {
                            self.dups += 1;
                        }
                    }
                    Event::Expired { .. } => self.expired += 1,
                    Event::Registered { origin, client, .. } => self.registered.push((id, origin, client)),
                    _ => {}
                }
                let mut done = Vec::new();
                self.members[i].asks.on_event(&e, &mut done);
                self.record(done)?;
                if let Some((to, told)) = self.members[i].tell.on_event(&e) {
                    self.send(id, to, Msg::Told(told));
                }
            }
            let leader = self.members[i].rsm.leader();
            if leader != self.members[i].leader {
                self.members[i].leader = leader;
                self.members[i].asks.leader_changed();
                self.leader_changes += 1;
            }
            if self.members[i].rsm.role() != Role::Leader {
                self.members[i].tell.clear();
            }
        }
        for i in 0..self.members.len() {
            let id = self.members[i].id;
            let leading = self.members[i].rsm.role() == Role::Leader;
            let leader = self.members[i].rsm.leader();
            let mut out = Vec::new();
            let mut done = Vec::new();
            self.members[i].asks.poll(now, leading || leader != raft::NONE, &mut out, &mut done);
            for s in out {
                if leading {
                    self.lead(i, id, s);
                } else {
                    self.send(id, leader, Msg::Session(s));
                }
            }
            self.record(done)?;
        }
        Ok(())
    }

    /// Steps, every message let through, until `until` holds: false where
    /// it never did in `max`.
    fn run(&mut self, max: u64, until: &dyn Fn(&World) -> bool) -> Result<bool, String> {
        for _ in 0..max {
            if until(self) {
                return Ok(true);
            }
            self.step(&mut |_, _, _| true)?;
        }
        Ok(until(self))
    }

    fn count(&self, i: usize, id: u64) -> u32 {
        self.members[i].rsm.state().counts.get(&id).copied().unwrap_or(0)
    }

    /// The judge: no ask applied twice anywhere; each told what became of
    /// it truly.
    fn judge(&self) -> Result<(), String> {
        for (i, m) in self.members.iter().enumerate() {
            if let Some((id, c)) = m.rsm.state().counts.iter().find(|(_, &c)| c > 1) {
                return Err(format!("ask {} applied {} times, as member {} has it", id, c, i + 1));
            }
        }
        for (&id, &(on, when, long)) in &self.asked {
            let c = self.count(0, id).max(self.count(self.members.len() - 1, id));
            match self.outcomes.get(&id) {
                Some(Outcome::Applied { answer, .. }) => {
                    if c != 1 || answer.len() < 4 || answer[..4] != 1u32.to_le_bytes()
                        || (answer.len() == 4 + LONG) != long
                    {
                        return Err(format!("ask {} (member {}, step {}) answered Applied with {} bytes beginning \
                                            {:?}, and applied {} times", id, on, when, answer.len(),
                                           &answer[..answer.len().min(4)], c));
                    }
                }
                Some(Outcome::Lost) if c != 1 || !long => {
                    return Err(format!("ask {} (member {}, step {}) told Lost, applied {} times, its answer {}",
                                       id, on, when, c, if long { "long" } else { "short" }));
                }
                Some(Outcome::NotAsked) if c != 0 => {
                    return Err(format!("ask {} (member {}, step {}) told it was not asked, and applied", id, on,
                                       when));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// A run: asks on every member under losses, doubles, cuts and the
/// leader's refusals; then calm, asks answered. On every seed.
pub fn run(seed: u64, steps: u64) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x7275_6e));
    let n: u64 = if r.chance(50) { 3 } else { 5 };
    let mut w = World::new(seed, n, TIMING)?;
    w.veto = if r.chance(50) { 3 } else { 0 };
    let mut cut: Option<NodeId> = None;
    let mut cut_until = 0;
    let mut next_id = 1u64;
    let total = steps + CALM;
    for step in 0..total {
        let calm = step >= steps;
        w.lossy = !calm;
        if calm {
            cut = None;
        } else if cut.is_none() && r.chance(1) {
            /* Short, or long enough for the others' snapshots to pass it by */
            cut = Some(r.range(1, n));
            cut_until = step + if r.chance(30) { r.range(150, 400) } else { r.range(5, 60) };
        } else if cut.is_some() && step >= cut_until {
            cut = None;
        }
        if (!calm || step < steps + CALM_ASKS) && r.chance(6) {
            let on = r.range(1, n);
            let long = r.chance(15);
            w.ask(on, next_id, long);
            next_id += 1;
        }
        let c = cut;
        w.step(&mut |from, to, _| c.is_none_or(|c| (from == c) == (to == c)))?;
    }
    w.judge()?;
    /* Calm: every ask made in it answered, applied once */
    for (&id, &(on, when, _)) in &w.asked {
        if when < steps {
            continue;
        }
        match w.outcomes.get(&id) {
            Some(Outcome::Applied { .. }) | Some(Outcome::Lost) => {}
            other => {
                return Err(format!("ask {} made on member {} at step {}, the network calm: {:?}", id, on, when,
                                   other));
            }
        }
    }
    let first = w.members[0].rsm.state().clone();
    let at = w.members[0].rsm.applied();
    for m in &w.members {
        if m.rsm.applied() != at || *m.rsm.state() != first {
            return Err(format!("member {} applied to {}, member 1 to {}: their tallies differ", m.id, m.rsm.applied(),
                               at));
        }
    }
    if std::env::var_os("RAFT_ASKS_STATS").is_some() {
        let mut kinds: BTreeMap<&str, u64> = BTreeMap::new();
        for o in w.outcomes.values() {
            let k = match o {
                Outcome::Applied { .. } => "applied",
                Outcome::Lost => "lost",
                Outcome::NotAsked => "not asked",
                Outcome::Unknown { .. } => "unknown",
            };
            *kinds.entry(k).or_default() += 1;
        }
        eprintln!("asks seed {}: {} members, {} asks, {:?}, {} refused at once, {} retries answered from a session, \
                   {} expired, {} leader changes seen, {} words of the leader's", seed, n, w.asked.len(), kinds,
                  w.full, w.dups, w.expired, w.leader_changes, w.told);
    }
    Ok(())
}

/* ---- scenarios, a rule of the asks' each ---- */

pub const SCENARIOS: &[(&str, fn() -> Result<(), String>)] = &[
    ("asks_once", once),
    ("asks_expired", expired),
    ("asks_unknown", unknown),
    ("asks_told", told),
];

/// Whether an append carries an entry of `index`.
fn carries(m: &Msg, index: u64) -> bool {
    matches!(m, Msg::Raft(Message { body: Body::Append { entries, .. }, .. })
             if entries.iter().any(|e| e.index == index))
}

/// Three members, member 1 leading, its ask's command in its log and in
/// member 2's alone, member 1 then cut off: members 2 and 3 elect 2, which
/// commits the command. The index the command is at.
fn in_two_logs(w: &mut World) -> Result<u64, String> {
    w.members[0].rsm.campaign().map_err(|e| format!("{:?}", e))?;
    if !w.run(100, &|w| w.members[0].rsm.role() == Role::Leader)? {
        return Err(String::from("member 1 never led"));
    }
    w.ask(1, 7, false);
    if !w.run(100, &|w| !w.submitted.is_empty())? {
        return Err(String::from("member 1 never submitted its ask's command"));
    }
    let index = w.submitted[0].2;
    /* The append that carries it to member 2 alone, and nothing back to 1 */
    for _ in 0..20 {
        w.step(&mut |from, to, m| from == 1 && to == 2 && carries(m, index) || from != 1 && to != 1)?;
        if w.members[1].rsm.node().last_index() >= index {
            break;
        }
    }
    if w.members[1].rsm.node().last_index() < index || w.members[2].rsm.node().last_index() >= index {
        return Err(String::from("the command not in member 2's log alone"));
    }
    /* Member 1 cut off: 2 and 3 elect 2, the one with the command */
    for _ in 0..400 {
        w.step(&mut |from, to, _| from != 1 && to != 1)?;
        if w.count(1, 7) == 1 && w.count(2, 7) == 1 {
            break;
        }
    }
    if w.members[1].rsm.role() != Role::Leader || w.count(1, 7) != 1 {
        return Err(format!("member 2 did not lead and apply the command: {:?}, applied it {} times",
                           w.members[1].rsm.role(), w.count(1, 7)));
    }
    Ok(index)
}

/// A command in a log as its leader is cut off: asked again of the next
/// leader under its session and number, and answered from the session --
/// applied once, the ask told so.
fn once() -> Result<(), String> {
    let mut w = World::new(1, 3, Timing { total: 2000, resend: 1000, retry: 3 })?;
    in_two_logs(&mut w)?;
    /* Back: member 1 hears of the new leader and asks again */
    if !w.run(600, &|w| w.outcomes.contains_key(&7) && w.members.iter().all(|m| m.rsm.applied() == w.members[1]
                                                                              .rsm.applied()))? {
        return Err(format!("the ask never answered: {:?}", w.outcomes.get(&7)));
    }
    w.judge()?;
    if !matches!(w.outcomes.get(&7), Some(Outcome::Applied { .. })) || w.count(0, 7) != 1 {
        return Err(format!("told {:?}, applied {} times", w.outcomes.get(&7), w.count(0, 7)));
    }
    if w.dups == 0 {
        return Err(String::from("the command was never asked again: the scenario did not play out"));
    }
    Ok(())
}

/// A session gone before its command is applied: the command not applied
/// under it, and asked again under a new one -- applied once.
fn expired() -> Result<(), String> {
    let mut w = World::new(2, 3, Timing { total: 2000, resend: 20, retry: 3 })?;
    w.members[0].rsm.campaign().map_err(|e| format!("{:?}", e))?;
    if !w.run(100, &|w| w.members[0].rsm.role() == Role::Leader)? {
        return Err(String::from("member 1 never led"));
    }
    /* Member 2's ask: its session made, its command held back */
    w.ask(2, 9, false);
    let held = |from: NodeId, _: NodeId, m: &Msg| !(from == 2 && matches!(m, Msg::Session(Send::Command { .. })));
    let mut client = None;
    for _ in 0..100 {
        w.step(&mut |f, t, m| held(f, t, m))?;
        client = w.registered.iter().find(|&&(at, origin, _)| at == 2 && origin == 2).map(|&(_, _, c)| c);
        if client.is_some() {
            break;
        }
    }
    let client = client.ok_or("member 2's ask got no session")?;
    /* Its session ended by the leader before the command comes */
    w.members[0].rsm.unregister(client).map_err(|e| format!("{:?}", e))?;
    for _ in 0..30 {
        w.step(&mut |f, t, m| held(f, t, m))?;
    }
    if w.members.iter().any(|m| m.rsm.sessions() != 0) {
        return Err(String::from("the ask's session not ended"));
    }
    if !w.run(600, &|w| w.outcomes.contains_key(&9))? {
        return Err(format!("the ask never answered: {:?}", w.outcomes.get(&9)));
    }
    w.judge()?;
    if !matches!(w.outcomes.get(&9), Some(Outcome::Applied { .. })) || w.count(0, 9) != 1 {
        return Err(format!("told {:?}, applied {} times", w.outcomes.get(&9), w.count(0, 9)));
    }
    if w.expired == 0 {
        return Err(String::from("no try of the command expired: the scenario did not play out"));
    }
    Ok(())
}

/// An ask whose time is up with its command in a log, not heard of: told
/// it may yet be applied, not that it was never asked -- and then applied.
fn unknown() -> Result<(), String> {
    let mut w = World::new(3, 3, Timing { total: 150, resend: 1000, retry: 3 })?;
    in_two_logs(&mut w)?;
    /* Member 1 still cut off past its ask's time */
    for _ in 0..200 {
        w.step(&mut |from, to, _| from != 1 && to != 1)?;
    }
    if !w.run(600, &|w| w.members.iter().all(|m| m.rsm.applied() == w.members[1].rsm.applied()))? {
        return Err(String::from("member 1 never caught up"));
    }
    w.judge()?;
    if !matches!(w.outcomes.get(&7), Some(Outcome::Unknown { .. })) || w.count(0, 7) != 1 {
        return Err(format!("told {:?}, applied {} times", w.outcomes.get(&7), w.count(0, 7)));
    }
    Ok(())
}

/// A member that gets no entry from the moment it asks -- as one far
/// behind in the log, or a host the command removes from the group, which
/// never gets its entry -- the leader's heartbeats alone reaching it, which
/// commit nothing it has not: its session and its command's outcome told
/// it by the leader that applied them.
fn told() -> Result<(), String> {
    let mut w = World::new(4, 3, Timing { total: 400, resend: 1000, retry: 3 })?;
    w.members[0].rsm.campaign().map_err(|e| format!("{:?}", e))?;
    if !w.run(100, &|w| w.members[0].rsm.role() == Role::Leader
                       && w.members.iter().all(|m| m.rsm.applied() == w.members[0].rsm.applied()))? {
        return Err(String::from("member 1 never led"));
    }
    let before = w.members[1].rsm.applied();
    w.ask(2, 11, false);
    for _ in 0..200 {
        w.step(&mut |_, to, m| {
            !(to == 2 && matches!(m, Msg::Raft(Message { body: Body::Append { .. } | Body::Snapshot { .. }, .. })))
        })?;
        if w.outcomes.contains_key(&11) {
            break;
        }
    }
    if w.members[1].rsm.applied() != before {
        return Err(String::from("member 2 applied entries: the scenario did not play out"));
    }
    match w.outcomes.get(&11) {
        Some(Outcome::Applied { answer, .. }) if answer[..4] == 1u32.to_le_bytes() => {}
        other => return Err(format!("member 2, which applies nothing, told {:?}", other)),
    }
    if !w.run(600, &|w| w.members.iter().all(|m| m.rsm.applied() == w.members[0].rsm.applied()))? {
        return Err(String::from("member 2 never caught up"));
    }
    w.judge()
}
