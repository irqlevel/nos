//! One member fed what no correct member sends: every kind of message, from
//! the group's members and from strangers, of terms and indexes about its
//! own -- where the deep paths are -- and of any value at all, and noise
//! the wire takes; answers to what it sent, some true and some not, so that
//! it gets to lead and to send snapshots too. Raft trusts its members, so
//! what the group agrees is not checked here -- a liar can make it agree to
//! anything -- but the member must not panic, must ask for no block the
//! kernel's heap would not give (`heap`), and must keep what it promises of
//! itself: its term never goes back, nor its vote within a term, nor its
//! commit; it commits nothing past what it holds, hands out in order what it
//! wrote, answers a vote only once the vote is on its disk, and sends
//! nothing the wire would not carry back as it was.

use raft::wire;
use raft::{
    Body, ChangeKind, ConfChange, ConfState, Config, Entry, EntryKind, MemStorage, Message, NodeId, RawNode, Role,
    SnapshotBuilder, SnapshotMeta, Storage, NONE,
};

use crate::rng::Rng;
use crate::wirefuzz;

/// The member under test is always this one.
const ME: NodeId = 1;
/// Ids a message may come from: the group's are the low ones.
const IDS: u64 = 9;
/// Messages it sent, kept to answer.
const SENT_KEPT: usize = 32;
/// Iterations between walks over the whole log.
const LOG_CHECK_EVERY: u64 = 64;

struct Run {
    r: Rng,
    cfg: Config,
    node: Option<RawNode<MemStorage>>,
    sent: Vec<Message>,
    /// What it said of itself so far, to hold it to.
    term: u64,
    vote: NodeId,
    committed: u64,
    applied: u64,
    i: u64,
    /// How far it got, for `RAFT_STATS`: steps it led in, snapshots taken
    /// in and pieces of its own sent, entries applied.
    led: u64,
    snaps_in: u64,
    pieces_out: u64,
    applied_n: u64,
    /// Messages stepped, and those taken.
    steps: u64,
    taken: u64,
}

fn fail<T>(i: u64, what: String) -> Result<T, String> {
    Err(format!("iteration {}: {}", i, what))
}

impl Run {
    fn n(&mut self) -> &mut RawNode<MemStorage> {
        self.node.as_mut().expect("the member is up between steps")
    }

    /// A term about the member's own, or one no group reaches. The last
    /// one taken leaves the member a term from which its next election
    /// is past them all, and nothing it hears is of its term again: rare,
    /// or the rest of a run would be refusals.
    fn term_near(&mut self) -> u64 {
        let t = self.n().term();
        match self.r.below(10) {
            0..=3 => t,
            4 | 5 => t + 1,
            6 => t.saturating_sub(1),
            7 => t + self.r.range(2, 4),
            8 => self.r.below(t + 2),
            _ if self.r.below(1000) == 0 => (1 << 62) - 1,
            _ => [0, u64::MAX, 1 << 62][self.r.below(3) as usize],
        }
    }

    /// An index about the member's log, or any.
    fn index_near(&mut self) -> u64 {
        let (first, last, committed) = {
            let n = self.n();
            (n.first_index(), n.last_index(), n.committed())
        };
        match self.r.below(12) {
            0 | 1 => last,
            2 => last + 1,
            3 => last.saturating_sub(1),
            4 => committed,
            5 => committed + 1,
            6 => first.saturating_sub(1),
            7 => first,
            8 | 9 => self.r.below(last + 4),
            10 => last + self.r.range(2, 64),
            /* The last index a group may reach: a snapshot there leaves
             * the member a log past it. */
            _ if self.r.below(1000) == 0 => (1 << 62) - 1,
            _ => [0, u64::MAX, 1 << 62][self.r.below(3) as usize],
        }
    }

    fn from(&mut self) -> NodeId {
        match self.r.below(20) {
            0 => ME,
            1 => NONE,
            2 => self.r.next(),
            _ => self.r.range(2, IDS),
        }
    }

    fn change(&mut self) -> ConfChange {
        let kind = match self.r.below(3) {
            0 => ChangeKind::AddVoter,
            1 => ChangeKind::AddLearner,
            _ => ChangeKind::Remove,
        };
        ConfChange { kind, node: self.r.below(IDS + 2) }
    }

    fn entries(&mut self, prev_index: u64, prev_term: u64, term: u64) -> Vec<Entry> {
        let n = match self.r.below(10) {
            0 => 0,
            1 => self.r.range(20, 300),
            _ => self.r.range(1, 6),
        };
        let mut t = prev_term;
        let mut out = Vec::new();
        for k in 0..n {
            if self.r.chance(15) && t < term {
                t += 1;
            }
            let kind = match self.r.below(8) {
                0 => EntryKind::Noop,
                1 => EntryKind::Change,
                _ => EntryKind::Normal,
            };
            let data = match kind {
                EntryKind::Change if self.r.chance(90) => {
                    let cc = self.change();
                    wire::encode_change(&cc)
                }
                _ => (0..self.r.below(48)).map(|_| self.r.next() as u8).collect(),
            };
            let mut e = Entry { term: t, index: prev_index.wrapping_add(k + 1), kind, data };
            if self.r.chance(2) {
                /* Out of order, or of a term past the message's. */
                match self.r.below(2) {
                    0 => e.index = self.r.next(),
                    _ => e.term = term.wrapping_add(1),
                }
            }
            out.push(e);
        }
        out
    }

    /// A message made to look like one of the group's.
    fn crafted(&mut self) -> Message {
        let from = self.from();
        let term = self.term_near();
        let body = match self.r.below(12) {
            0 | 1 => {
                let (last_index, last_term) = (self.index_near(), self.term_near());
                Body::Vote { pre: self.r.chance(50), transfer: self.r.chance(10), last_index, last_term }
            }
            2 => Body::VoteResp { pre: self.r.chance(50), granted: self.r.chance(60) },
            3 | 4 => {
                let prev_index = self.index_near();
                let prev_term = match self.r.below(3) {
                    0 => self.n().term_at(prev_index).unwrap_or(0),
                    1 => self.r.below(term.saturating_add(1)),
                    _ => self.term_near(),
                };
                let entries = self.entries(prev_index, prev_term, term);
                Body::Append { prev_index, prev_term, commit: self.index_near(), entries }
            }
            5 => Body::AppendResp {
                reject: self.r.chance(30),
                index: self.index_near(),
                hint_index: self.index_near(),
                hint_term: self.term_near(),
            },
            6 => Body::Heartbeat { commit: self.index_near(), ctx: self.r.below(8) },
            7 => Body::HeartbeatResp { ctx: self.r.below(8) },
            8 => {
                let total = match self.r.below(4) {
                    0 => self.r.below(64),
                    1 => self.r.range(64, 4096),
                    2 => self.cfg.max_snapshot_bytes + self.r.below(2),
                    _ => self.r.next(),
                };
                let offset = if self.r.chance(70) { 0 } else { self.r.below(total.saturating_add(1)) };
                let len = total.saturating_sub(offset).min(self.r.below(512) + 1);
                let data = (0..len).map(|_| self.r.next() as u8).collect();
                let voters: Vec<NodeId> = (1..=self.r.range(1, 5)).collect();
                let learners = if self.r.chance(20) { vec![7] } else { vec![] };
                let meta = SnapshotMeta {
                    index: self.index_near(),
                    term: self.r.below(term.saturating_add(1)),
                    conf: ConfState { voters, learners },
                };
                Body::Snapshot { meta, offset, total, data }
            }
            9 => Body::SnapshotResp { index: self.index_near(), next: self.r.below(8192), reject: self.r.chance(10) },
            10 => {
                if self.r.chance(50) {
                    Body::TimeoutNow
                } else {
                    Body::ReadIndex { ctx: self.r.below(16) }
                }
            }
            _ => Body::ReadIndexResp { ctx: self.r.below(16), index: self.index_near(), ok: self.r.chance(70) },
        };
        let to = if self.r.chance(95) { ME } else { self.r.below(IDS) };
        Message { from, to, term, body }
    }

    /// An answer to something it sent: true most of the time.
    fn answer(&mut self) -> Option<Message> {
        let at = self.r.below(self.sent.len() as u64) as usize;
        let m = self.sent.get(at)?.clone();
        let lie = self.r.chance(15);
        let body = match m.body {
            Body::Vote { pre, .. } => Body::VoteResp { pre, granted: self.r.chance(80) },
            Body::Append { prev_index, entries, .. } => {
                if self.r.chance(15) {
                    let hint_index = self.r.below(prev_index + 1);
                    let hint_term = self.n().term_at(hint_index).unwrap_or(0);
                    Body::AppendResp { reject: true, index: prev_index, hint_index, hint_term }
                } else {
                    Body::AppendResp { reject: false, index: prev_index + entries.len() as u64, hint_index: 0, hint_term: 0 }
                }
            }
            Body::Heartbeat { ctx, .. } => Body::HeartbeatResp { ctx },
            Body::Snapshot { meta, offset, data, .. } => {
                if self.r.chance(20) {
                    /* Taken in whole: the member says so as an append. */
                    Body::AppendResp { reject: false, index: meta.index, hint_index: 0, hint_term: 0 }
                } else {
                    Body::SnapshotResp { index: meta.index, next: offset + data.len() as u64, reject: self.r.chance(5) }
                }
            }
            Body::ReadIndex { ctx } => {
                let index = self.n().committed();
                Body::ReadIndexResp { ctx, index, ok: self.r.chance(80) }
            }
            _ => return None,
        };
        let mut a = Message { from: m.to, to: ME, term: m.term, body };
        if lie {
            match self.r.below(3) {
                0 => a.term = self.term_near(),
                1 => a.from = self.from(),
                _ => {
                    if let Body::AppendResp { index, .. } = &mut a.body {
                        *index = self.index_near();
                    }
                }
            }
        }
        Some(a)
    }

    /// A message from the wire: one of any values, written and broken.
    fn noise(&mut self) -> Option<Message> {
        let mut m = wirefuzz::message(&mut self.r);
        if self.r.chance(80) {
            m.to = ME;
        }
        if self.r.chance(80) {
            /* The body's values any, the term one that is listened to. */
            m.term = self.term_near();
        }
        if self.r.chance(50) {
            return Some(m);
        }
        let mut b = Vec::new();
        wire::encode(&m, &mut b).ok()?;
        if !b.is_empty() {
            let at = self.r.below(b.len() as u64) as usize;
            b[at] ^= 1 << self.r.below(8);
        }
        wire::decode(&b).ok()
    }

    fn step(&mut self, m: Message) {
        self.steps += 1;
        if self.n().step(m).is_ok() {
            self.taken += 1;
        }
    }

    /// What it asked done: written, checked, and its messages kept to answer.
    fn process(&mut self) -> Result<(), String> {
        let i = self.i;
        while self.n().has_ready() {
            let rd = self.n().ready();
            if let Some(snap) = &rd.snapshot {
                if snap.meta.index <= self.applied {
                    return fail(i, format!("a snapshot at {} handed out after {} was applied", snap.meta.index, self.applied));
                }
                if let Err(e) = self.n().storage_mut().apply_snapshot(snap.clone()) {
                    return fail(i, format!("the snapshot at {} could not be written: {:?}", snap.meta.index, e));
                }
                self.applied = snap.meta.index;
                self.snaps_in += 1;
            }
            if let Err(e) = self.n().storage_mut().append(&rd.entries) {
                return fail(i, format!("its entries could not be written: {:?}", e));
            }
            if let Some(hs) = rd.hard_state {
                self.n().storage_mut().set_hard_state(hs);
            }
            let hs = self.n().storage().hard_state();
            for m in &rd.messages {
                if m.from != ME || m.to == ME || m.to == NONE {
                    return fail(i, format!("sent {:?}", m));
                }
                /* Of its own term, which is on its disk -- but a PreVote's,
                 * of the term it would stand in, and the answer to one, of
                 * the term it was asked about. */
                let bound = match m.body {
                    Body::VoteResp { pre: true, .. } => u64::MAX,
                    Body::Vote { pre: true, .. } => hs.term + 1,
                    _ => hs.term,
                };
                if m.term > bound {
                    return fail(i, format!("sent a message of term {} with term {} on its disk: {:?}", m.term, hs.term, m));
                }
                if let Body::VoteResp { pre: false, granted: true } = m.body {
                    if hs.term != m.term || hs.vote != m.to {
                        return fail(i, format!("gave its vote to {} in term {} with {:?} on its disk", m.to, m.term, hs));
                    }
                }
                if let Body::Snapshot { .. } = m.body {
                    self.pieces_out += 1;
                }
                let mut b = Vec::new();
                match wire::encode(m, &mut b) {
                    Ok(()) => match wire::decode(&b) {
                        Ok(back) if &back == m => {}
                        other => return fail(i, format!("sent {:?}, which the wire reads back as {:?}", m, other)),
                    },
                    Err(e) => return fail(i, format!("sent {:?}, which the wire does not write: {:?}", m, e)),
                }
            }
            let last = self.n().storage().last_index();
            for e in &rd.committed {
                if e.index != self.applied + 1 {
                    return fail(i, format!("handed out entry {} to apply after {}", e.index, self.applied));
                }
                if e.index > last || self.n().storage().term(e.index).ok() != Some(e.term) {
                    return fail(i, format!("handed out entry {} of term {} to apply, which it has not written", e.index, e.term));
                }
                self.applied = e.index;
                self.applied_n += 1;
            }
            self.sent.extend(rd.messages);
            if self.sent.len() > SENT_KEPT {
                let drop = self.sent.len() - SENT_KEPT;
                self.sent.drain(..drop);
            }
            self.n().advance();
        }
        self.check()
    }

    /// What it says of itself, held to what it said before.
    fn check(&mut self) -> Result<(), String> {
        let i = self.i;
        let n = self.node.as_ref().expect("up");
        let (term, vote, committed) = (n.term(), n.vote(), n.committed());
        if term < self.term {
            return fail(i, format!("its term went back from {} to {}", self.term, term));
        }
        if term == self.term && self.vote != NONE && vote != self.vote {
            return fail(i, format!("its vote in term {} went from {} to {}", term, self.vote, vote));
        }
        if committed < self.committed {
            return fail(i, format!("its commit went back from {} to {}", self.committed, committed));
        }
        if n.applied() > committed || committed > n.last_index() || n.first_index() > n.last_index() + 1 {
            return fail(i, format!("applied {} committed {} first {} last {}", n.applied(), committed, n.first_index(),
                                   n.last_index()));
        }
        if n.role() == Role::Leader && n.leader() != ME {
            return fail(i, format!("leads, following {}", n.leader()));
        }
        (self.term, self.vote, self.committed) = (term, vote, committed);
        if self.i.is_multiple_of(LOG_CHECK_EVERY) {
            self.check_log()?;
        }
        Ok(())
    }

    /// Its log's terms never go down, and none is past its own.
    fn check_log(&self) -> Result<(), String> {
        let n = self.node.as_ref().expect("up");
        let mut prev = n.term_at(n.first_index() - 1).unwrap_or(0);
        for at in n.first_index()..=n.last_index() {
            let Some(t) = n.term_at(at) else { return fail(self.i, format!("no term for entry {}", at)) };
            if t < prev || t > n.term() {
                return fail(self.i, format!("entry {} of term {} after one of {}, in term {}", at, t, prev, n.term()));
            }
            prev = t;
        }
        Ok(())
    }

    /// Its applied state made a snapshot, the log before it dropped.
    fn compact(&mut self) {
        let applied = self.applied;
        let filler = self.r.below(3000) as usize;
        let n = self.n();
        if applied <= n.storage().snapshot_meta().index || applied > n.storage().last_index() {
            return;
        }
        let Some(conf) = n.conf_state_at(applied) else { return };
        let mut b = SnapshotBuilder::new();
        assert!(b.push(&applied.to_le_bytes()) && b.push(&vec![0x5A; filler]), "a snapshot's bytes refused");
        let _ = n.storage_mut().create_snapshot(applied, conf, b.finish());
    }

    /// Down and up again over what it wrote.
    fn restart(&mut self) -> Result<(), String> {
        let storage = self.node.take().expect("up").into_storage();
        let mut cfg = self.cfg.clone();
        cfg.seed = self.r.next();
        match RawNode::new(cfg, storage) {
            Ok(n) => {
                /* Its state starts again from its snapshot, and what it has
                 * committed past it is handed out again. */
                self.applied = n.applied();
                self.node = Some(n);
                self.sent.clear();
                Ok(())
            }
            Err(e) => fail(self.i, format!("does not start again over what it wrote: {:?}", e)),
        }
    }
}

/// `iterations` steps of one member from `seed`.
pub fn run(seed: u64, iterations: u64) -> Result<(), String> {
    let mut r = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xBAD);
    let mut cfg = Config::new(ME);
    cfg.election_ticks = r.range(3, 10) as u32;
    cfg.heartbeat_ticks = 1;
    cfg.pre_vote = r.chance(70);
    cfg.check_quorum = r.chance(70);
    cfg.max_entries_per_msg = r.range(1, 16) as usize;
    cfg.max_bytes_per_msg = r.range(64, 4096);
    cfg.max_inflight_msgs = r.range(1, 8) as usize;
    cfg.max_uncommitted_entries = r.range(cfg.max_entries_per_msg as u64, 256);
    cfg.snapshot_piece_bytes = r.range(16, 1024) as usize;
    cfg.max_snapshot_bytes = r.range(1024, 64 * 1024);
    cfg.seed = seed;
    let nvoters = r.range(1, 5);
    let voters: Vec<NodeId> = (1..=nvoters).collect();
    let learners: Vec<NodeId> = if r.chance(30) { vec![nvoters + 1] } else { vec![] };
    let storage = if r.chance(85) { MemStorage::bootstrap(&voters, &learners) } else { MemStorage::new() };
    let node = RawNode::new(cfg.clone(), storage).map_err(|e| format!("does not start: {:?}", e))?;
    let mut run = Run {
        r,
        cfg,
        node: Some(node),
        sent: Vec::new(),
        term: 0,
        vote: NONE,
        committed: 0,
        applied: 0,
        i: 0,
        led: 0,
        snaps_in: 0,
        pieces_out: 0,
        applied_n: 0,
        steps: 0,
        taken: 0,
    };
    run.applied = run.n().applied();
    run.check()?;
    for i in 0..iterations {
        run.i = i;
        match run.r.below(100) {
            0..=19 => run.n().tick(),
            20..=44 => {
                let m = run.crafted();
                run.step(m);
            }
            45..=64 => {
                if let Some(m) = run.answer() {
                    run.step(m);
                }
            }
            65..=71 => {
                if let Some(m) = run.noise() {
                    run.step(m);
                }
            }
            72..=79 => {
                let len = if run.r.chance(3) { run.cfg.max_entry_bytes + 1 } else { run.r.below(64) as usize };
                let data = vec![run.r.next() as u8; len];
                let _ = run.n().propose(data);
            }
            80..=83 => {
                let cc = run.change();
                let _ = run.n().propose_change(cc);
            }
            84..=86 => {
                let ctx = run.r.below(16);
                let _ = run.n().read_index(ctx);
            }
            87 | 88 => {
                let to = run.r.below(IDS + 1);
                let _ = run.n().transfer_leader(to);
            }
            89..=91 => {
                let _ = run.n().campaign();
            }
            92..=96 => run.compact(),
            _ if run.r.chance(30) => run.restart()?,
            _ => run.n().tick(),
        }
        run.process()?;
        if run.n().role() == Role::Leader {
            run.led += 1;
        }
    }
    if std::env::var_os("RAFT_STATS").is_some() {
        let n = run.node.as_ref().expect("up");
        eprintln!("hostile seed {}: term {} log {}..={} committed {} led {} snapshots in {} pieces out {} applied {} \
                   messages {} taken {} {:?}",
                  seed, n.term(), n.first_index(), n.last_index(), n.committed(), run.led, run.snaps_in, run.pieces_out,
                  run.applied_n, run.steps, run.taken, n.stats());
    }
    run.check_log()
}
