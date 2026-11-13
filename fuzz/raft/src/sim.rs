//! A group's members in one process: each a `RawNode` over a `MemStorage`
//! that is its disk -- a crash keeps exactly what was written and nothing
//! else -- and a network that delays, loses, doubles, reorders and cuts.
//! Every step is checked against Raft's properties as it happens:
//!
//! - at most one leader a term (election safety);
//! - a new leader holds every entry committed before it (leader
//!   completeness);
//! - two logs that agree on an entry's term agree on everything up to it
//!   (log matching);
//! - an entry committed is never changed, and every member applies the
//!   same entries, one after another, to the same state (state machine
//!   safety) -- a snapshot taken in carrying that same state;
//! - a confirmed read is no older than what was committed before it was
//!   asked;
//! - a member stands only once its election timer has run out since it
//!   last started over -- a vote given, a leader heard, a term changed;
//! - no member takes a message from another as one that cannot be;
//! - a read its disk failed stops a member: nothing it decided on it handed
//!   out, no `Ready` after it -- the member is crashed and started again;
//! - and, once the network is healed and every member is up, a leader and
//!   new entries committed on every voter within so many ticks, and every
//!   read asked of a leader still leading in its term answered.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Deref, DerefMut};

use raft::{
    Body, ChangeKind, ConfChange, Config, Entry, EntryKind, MemStorage, Message, NodeId, RaftState, RawNode, Role,
    Snapshot, SnapshotBuilder, SnapshotData, Storage, StorageError,
};

/// A member's disk: a `MemStorage` that, in some runs, fails a read now and
/// then -- a term or entries asked for, answered `Corrupt` -- while armed,
/// which it is only inside the member's own work (a tick, a message, a
/// `Ready`): what the checks read of it never fails.
#[derive(Debug)]
pub struct Disk {
    pub mem: MemStorage,
    /// One read in this many fails; 0, none.
    fail_one_in: u64,
    rng: Cell<u64>,
    armed: Cell<bool>,
    /// Reads failed since the last look.
    failed: Cell<u64>,
}

impl Disk {
    pub fn new(mem: MemStorage, fail_one_in: u64, seed: u64) -> Disk {
        Disk { mem, fail_one_in, rng: Cell::new(seed), armed: Cell::new(false), failed: Cell::new(0) }
    }

    fn arm(&self, on: bool) {
        self.armed.set(on);
    }

    fn take_failed(&self) -> u64 {
        self.failed.replace(0)
    }

    fn fails(&self) -> bool {
        if !self.armed.get() || self.fail_one_in == 0 {
            return false;
        }
        let r = mix(self.rng.get(), 1);
        self.rng.set(r);
        if r % self.fail_one_in == 0 {
            self.failed.set(self.failed.get() + 1);
            return true;
        }
        false
    }
}

impl Deref for Disk {
    type Target = MemStorage;
    fn deref(&self) -> &MemStorage {
        &self.mem
    }
}

impl DerefMut for Disk {
    fn deref_mut(&mut self) -> &mut MemStorage {
        &mut self.mem
    }
}

impl Storage for Disk {
    fn initial_state(&self) -> Result<RaftState, StorageError> {
        self.mem.initial_state()
    }
    fn entries(&self, lo: u64, hi: u64, max_bytes: u64, out: &mut Vec<Entry>) -> Result<(), StorageError> {
        if self.fails() {
            return Err(StorageError::Corrupt);
        }
        self.mem.entries(lo, hi, max_bytes, out)
    }
    fn term(&self, index: u64) -> Result<u64, StorageError> {
        if self.fails() {
            return Err(StorageError::Corrupt);
        }
        self.mem.term(index)
    }
    fn first_index(&self) -> u64 {
        self.mem.first_index()
    }
    fn last_index(&self) -> u64 {
        self.mem.last_index()
    }
    fn snapshot(&self) -> Result<Snapshot, StorageError> {
        self.mem.snapshot()
    }
}

use crate::rng::{hash_bytes, mix, Rng};

/// How a run behaves: the members' configuration and the network's faults.
#[derive(Clone, Debug)]
pub struct Params {
    pub pre_vote: bool,
    pub check_quorum: bool,
    pub election_ticks: u32,
    pub heartbeat_ticks: u32,
    pub max_entries_per_msg: usize,
    pub max_bytes_per_msg: u64,
    pub max_inflight_msgs: usize,
    pub snapshot_piece_bytes: usize,
    pub max_uncommitted_entries: u64,
    /// In a hundred messages delivered at random, lost and doubled.
    pub drop_pct: u64,
    pub dup_pct: u64,
    /// One read of a disk in this many fails; 0, none.
    pub read_fail_one_in: u64,
}

impl Params {
    /// No PreVote, no CheckQuorum, no faults: what a scripted scenario
    /// starts from.
    pub fn plain() -> Params {
        Params {
            pre_vote: false,
            check_quorum: false,
            election_ticks: 10,
            heartbeat_ticks: 1,
            max_entries_per_msg: 64,
            max_bytes_per_msg: 64 * 1024,
            max_inflight_msgs: 16,
            snapshot_piece_bytes: 256,
            max_uncommitted_entries: 256,
            drop_pct: 0,
            dup_pct: 0,
            read_fail_one_in: 0,
        }
    }

    pub fn random(rng: &mut Rng) -> Params {
        let max_entries_per_msg = rng.range(1, 8) as usize;
        Params {
            pre_vote: rng.chance(60),
            check_quorum: rng.chance(60),
            election_ticks: rng.range(4, 15) as u32,
            heartbeat_ticks: rng.range(1, 2) as u32,
            max_entries_per_msg,
            max_bytes_per_msg: rng.range(16, 4096),
            max_inflight_msgs: rng.range(1, 8) as usize,
            snapshot_piece_bytes: rng.range(16, 600) as usize,
            /* Small at times: a follower then takes appends in part. */
            max_uncommitted_entries: if rng.chance(30) { rng.range(max_entries_per_msg as u64, 24) } else { 256 },
            drop_pct: rng.below(20),
            dup_pct: rng.below(10),
            read_fail_one_in: if rng.chance(20) { rng.range(30, 3000) } else { 0 },
        }
    }
}

/// A member's state machine, as far as the checks need it: how far it has
/// applied, and a hash of everything applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sm {
    pub applied: u64,
    pub hash: u64,
}

const SM_START: u64 = 0x5EED_0F_57A7E;
/// A snapshot's head: the applied index and the hash, before its filler.
const SM_BYTES: usize = 16;

pub struct Member {
    pub id: NodeId,
    /// None while it is down.
    pub node: Option<RawNode<Disk>>,
    /// Its disk while it is down.
    pub disk: Option<Disk>,
    pub sm: Sm,
    /// Ticks since its election timer last started over, as the checks
    /// count them.
    pub since_reset: u64,
    /// Taken out of the group, and stopped.
    pub retired: bool,
}

pub struct Sim {
    pub p: Params,
    pub seed: u64,
    pub members: BTreeMap<NodeId, Member>,
    pub net: Vec<Message>,
    /// Links cut, one way each.
    pub cut: BTreeSet<(NodeId, NodeId)>,
    pub rng: Rng,
    pub failures: Vec<String>,
    /// A member that loses its disk may come back under its old id: what
    /// a correct deployment never does (a new incarnation is a new member),
    /// and what one scenario does on purpose to show why.
    pub allow_wipe: bool,
    /// A member whose removal is committed stops, as a deployment stops it.
    /// Off where a scenario splits the group: a member taken out on one side
    /// has not heard of it on the other.
    pub retire_on_removal: bool,
    pub steps: u64,
    pub trace: bool,
    leaders: BTreeMap<u64, NodeId>,
    /// Each entry committed: its term, its hash, and the highest term any
    /// member had when it was first applied -- no earlier than its commit.
    committed: BTreeMap<u64, (u64, u64, u64)>,
    state_at: BTreeMap<u64, u64>,
    reads: BTreeMap<(NodeId, u64), u64>,
    /// Reads asked of a leader, by the term it led in: each to be answered
    /// while it leads in it and the group is healthy.
    leader_reads: BTreeMap<(NodeId, u64), u64>,
    /// Members stopped by a read their disk failed.
    pub faults: u64,
    /// Reads failed back, by member and context.
    pub reads_failed: BTreeSet<(NodeId, u64)>,
    /// Every read asked, confirmed or not: a doubled answer comes again.
    asked: BTreeSet<(NodeId, u64)>,
    max_committed: u64,
    to_retire: BTreeSet<NodeId>,
    next_data: u64,
    next_ctx: u64,
    next_id: NodeId,
}

/// The hash an entry is applied by: its kind and its data.
pub fn entry_hash(e: &Entry) -> u64 {
    mix(hash_bytes(&e.data), e.kind as u64)
}

fn sm_snapshot(sm: Sm, filler: usize) -> SnapshotData {
    let mut b = SnapshotBuilder::new();
    assert!(b.push(&sm.applied.to_le_bytes()) && b.push(&sm.hash.to_le_bytes()), "a snapshot's bytes refused");
    let mut x = sm.hash;
    let mut fill = Vec::with_capacity(filler);
    for _ in 0..filler {
        x = mix(x, 1);
        fill.push(x as u8);
    }
    assert!(b.push(&fill), "a snapshot's bytes refused");
    b.finish()
}

fn sm_of_snapshot(snap: &Snapshot) -> Option<Sm> {
    let mut head = [0u8; SM_BYTES];
    let mut at = 0;
    for piece in snap.data.pieces() {
        for &byte in piece {
            if at == SM_BYTES {
                break;
            }
            head[at] = byte;
            at += 1;
        }
    }
    if at < SM_BYTES {
        return None;
    }
    let mut a = [0u8; 8];
    a.copy_from_slice(&head[..8]);
    let applied = u64::from_le_bytes(a);
    a.copy_from_slice(&head[8..]);
    Some(Sm { applied, hash: u64::from_le_bytes(a) })
}

fn sm_of_disk(disk: &MemStorage) -> Sm {
    let snap = disk.snapshot().unwrap_or_default();
    if snap.meta.index == 0 {
        return Sm { applied: 0, hash: SM_START };
    }
    sm_of_snapshot(&snap).unwrap_or(Sm { applied: snap.meta.index, hash: 0 })
}

impl Sim {
    /// A group whose first members are `voters` and `learners`, all up.
    pub fn new(p: Params, seed: u64, voters: &[NodeId], learners: &[NodeId]) -> Sim {
        let next_id = voters.iter().chain(learners).copied().max().unwrap_or(0) + 1;
        let mut s = Sim {
            p,
            seed,
            members: BTreeMap::new(),
            net: Vec::new(),
            cut: BTreeSet::new(),
            rng: Rng::new(seed),
            failures: Vec::new(),
            allow_wipe: false,
            retire_on_removal: true,
            steps: 0,
            trace: std::env::var_os("RAFT_TRACE").is_some(),
            leaders: BTreeMap::new(),
            committed: BTreeMap::new(),
            state_at: BTreeMap::new(),
            reads: BTreeMap::new(),
            leader_reads: BTreeMap::new(),
            faults: 0,
            reads_failed: BTreeSet::new(),
            asked: BTreeSet::new(),
            max_committed: 0,
            to_retire: BTreeSet::new(),
            next_data: 1,
            next_ctx: 1,
            next_id,
        };
        for &id in voters.iter().chain(learners) {
            s.add(id, MemStorage::bootstrap(voters, learners));
        }
        s
    }

    pub fn fail(&mut self, what: String) {
        if self.failures.len() < 8 {
            self.failures.push(format!("step {}: {}", self.steps, what));
        }
    }

    pub fn config(&self, id: NodeId) -> Config {
        let mut c = Config::new(id);
        c.election_ticks = self.p.election_ticks;
        c.heartbeat_ticks = self.p.heartbeat_ticks;
        c.pre_vote = self.p.pre_vote;
        c.check_quorum = self.p.check_quorum;
        c.max_entries_per_msg = self.p.max_entries_per_msg;
        c.max_bytes_per_msg = self.p.max_bytes_per_msg;
        c.max_inflight_msgs = self.p.max_inflight_msgs;
        c.snapshot_piece_bytes = self.p.snapshot_piece_bytes;
        c.max_uncommitted_entries = self.p.max_uncommitted_entries;
        c.seed = mix(self.seed, id);
        c
    }

    /// A member over `disk`, started.
    pub fn add(&mut self, id: NodeId, disk: MemStorage) {
        let sm = sm_of_disk(&disk);
        let disk = Disk::new(disk, self.p.read_fail_one_in, mix(self.seed, id ^ 0xD15C));
        match RawNode::new(self.config(id), disk) {
            Ok(node) => {
                self.members.insert(id, Member { id, node: Some(node), disk: None, sm, since_reset: 0, retired: false });
            }
            Err(e) => self.fail(format!("member {} did not start: {}", id, e)),
        }
    }

    /// A new member with nothing, to be sent the group by its leader.
    pub fn add_fresh(&mut self) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        self.add(id, MemStorage::new());
        id
    }

    fn take(&mut self, id: NodeId) -> Option<Member> {
        self.members.remove(&id)
    }

    fn put(&mut self, m: Member) {
        self.members.insert(m.id, m);
    }

    pub fn node(&self, id: NodeId) -> Option<&RawNode<Disk>> {
        self.members.get(&id).and_then(|m| m.node.as_ref())
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut RawNode<Disk>> {
        self.members.get_mut(&id).and_then(|m| m.node.as_mut())
    }

    pub fn live(&self) -> Vec<NodeId> {
        self.members.values().filter(|m| m.node.is_some() && !m.retired).map(|m| m.id).collect()
    }

    pub fn crashed(&self) -> Vec<NodeId> {
        self.members.values().filter(|m| m.node.is_none() && !m.retired).map(|m| m.id).collect()
    }

    /// The live leader of the highest term, if there is one.
    pub fn leader(&self) -> Option<NodeId> {
        self.live()
            .into_iter()
            .filter_map(|id| self.node(id).map(|n| (n, id)))
            .filter(|(n, _)| n.role() == Role::Leader)
            .max_by_key(|(n, _)| n.term())
            .map(|(_, id)| id)
    }

    fn observe(&mut self, committed: u64) {
        if committed > self.max_committed {
            self.max_committed = committed;
        }
    }

    fn observe_all(&mut self) {
        let c: Vec<u64> = self.live().into_iter().filter_map(|id| self.node(id).map(|n| n.committed())).collect();
        for x in c {
            self.observe(x);
        }
    }

    fn note(&self, what: &str) {
        if self.trace {
            eprintln!("[{}] {}", self.steps, what);
        }
    }

    /* ---- what the harness does to a member ---- */

    pub fn tick(&mut self, id: NodeId) {
        let Some(mut m) = self.take(id) else { return };
        if let (Some(node), false) = (m.node.as_mut(), m.retired) {
            let before = node.stats();
            let (r0, t0, v0) = (node.role(), node.term(), node.vote());
            node.storage().arm(true);
            node.tick();
            node.storage().arm(false);
            m.since_reset += 1;
            let after = node.stats();
            let stood = after.pre_campaigns + after.campaigns > before.pre_campaigns + before.campaigns;
            if stood {
                if m.since_reset < u64::from(self.p.election_ticks) {
                    let since = m.since_reset;
                    self.fail(format!("member {} stood {} ticks after its election timer started over (it waits {} at least)",
                                      id, since, self.p.election_ticks));
                }
                m.since_reset = 0;
            } else if (node.role(), node.term(), node.vote()) != (r0, t0, v0) {
                m.since_reset = 0;
            }
            if after.invalid > before.invalid {
                self.fail(format!("member {} counted a message that cannot be", id));
            }
        }
        self.put(m);
        self.check_fault(id);
    }

    /// Message `i` of the network taken off it -- doubled or lost at the
    /// network's rate with `faults` -- and delivered.
    pub fn deliver_at(&mut self, i: usize, faults: bool) {
        if i >= self.net.len() {
            return;
        }
        let msg = self.net.remove(i);
        if faults && self.rng.chance(self.p.dup_pct) {
            self.net.push(msg.clone());
        }
        if faults && self.rng.chance(self.p.drop_pct) {
            return;
        }
        self.deliver(msg);
    }

    pub fn deliver(&mut self, msg: Message) {
        if self.cut.contains(&(msg.from, msg.to)) {
            return;
        }
        let to = msg.to;
        let Some(mut m) = self.take(to) else { return };
        if let (Some(node), false) = (m.node.as_mut(), m.retired) {
            let before = node.stats();
            let (r0, t0, v0) = (node.role(), node.term(), node.vote());
            let leaderish = matches!(msg.body, Body::Append { .. } | Body::Heartbeat { .. } | Body::Snapshot { .. });
            let mterm = msg.term;
            if self.trace {
                eprintln!("[{}] deliver {:?}", self.steps, msg);
            }
            node.storage().arm(true);
            let stepped = node.step(msg);
            node.storage().arm(false);
            if let Err(raft::Error::Invalid) = stepped {
                self.fail(format!("member {} refused a message as one that cannot be", to));
            }
            let (r1, t1, v1) = (node.role(), node.term(), node.vote());
            if (r1, t1, v1) != (r0, t0, v0) || (leaderish && t1 == mterm && r1 == Role::Follower) {
                m.since_reset = 0;
            }
            if node.stats().invalid > before.invalid {
                self.fail(format!("member {} counted a message that cannot be", to));
            }
        }
        self.put(m);
        self.check_fault(to);
    }

    /// What member `id` has to do, done as its program would: written,
    /// sent, applied, then advanced.
    pub fn process(&mut self, id: NodeId) {
        let Some(mut m) = self.take(id) else { return };
        let retired = m.retired;
        let Some(node) = m.node.as_mut() else {
            self.put(m);
            return;
        };
        if retired || !node.has_ready() {
            self.put(m);
            return;
        }
        node.storage().arm(true);
        let rd = node.ready();
        node.storage().arm(false);
        if node.fault().is_some() || node.storage().failed.get() > 0 {
            self.put(m);
            self.check_fault(id);
            return;
        }
        if let Some(ss) = rd.soft_state {
            if ss.role == Role::Leader {
                let term = node.term();
                self.check_leader(id, term, node);
            }
        }
        if let Some(snap) = &rd.snapshot {
            if let Err(e) = node.storage_mut().apply_snapshot(snap.clone()) {
                self.fail(format!("member {} could not write the snapshot at {}: {:?}", id, snap.meta.index, e));
            }
            match sm_of_snapshot(snap) {
                Some(sm) if sm.applied == snap.meta.index => {
                    match self.state_at.get(&sm.applied) {
                        Some(&h) if h != sm.hash => self.fail(format!(
                            "member {} took in a snapshot at {} whose state is not the state there", id, sm.applied)),
                        Some(_) => {}
                        None => {
                            self.state_at.insert(sm.applied, sm.hash);
                        }
                    }
                    m.sm = sm;
                }
                _ => self.fail(format!("member {} took in a snapshot at {} that is not one", id, snap.meta.index)),
            }
        }
        if let Err(e) = node.storage_mut().append(&rd.entries) {
            self.fail(format!("member {} could not write its entries: {:?}", id, e));
        }
        if let Some(hs) = rd.hard_state {
            node.storage_mut().set_hard_state(hs);
        }
        self.net.extend(rd.messages);
        let term_now = node.term();
        for e in &rd.committed {
            self.apply(id, term_now, &mut m.sm, e);
        }
        for rs in &rd.read_states {
            self.leader_reads.remove(&(id, rs.ctx));
            match self.reads.remove(&(id, rs.ctx)) {
                Some(need) if rs.index < need => self.fail(format!(
                    "member {}'s read {} confirmed at {}, behind {} committed before it was asked", id, rs.ctx, rs.index, need)),
                Some(_) => {}
                None if self.asked.contains(&(id, rs.ctx)) => {}
                None => self.fail(format!("member {} confirmed read {}, which nobody asked of it", id, rs.ctx)),
            }
        }
        /* A read failed back is answered: owed nothing more */
        for &ctx in &rd.reads_failed {
            self.leader_reads.remove(&(id, ctx));
            if self.reads.remove(&(id, ctx)).is_none() && !self.asked.contains(&(id, ctx)) {
                self.fail(format!("member {} failed read {}, which nobody asked of it", id, ctx));
            }
            self.reads_failed.insert((id, ctx));
        }
        if let Some(node) = m.node.as_mut() {
            node.advance();
            let c = node.committed();
            self.observe(c);
        }
        self.put(m);
        self.retire_removed();
    }

    /// The highest term any member has, up or down.
    fn highest_term(&self) -> u64 {
        self.members
            .values()
            .map(|m| match (&m.node, &m.disk) {
                (Some(n), _) => n.term(),
                (None, Some(d)) => d.hard_state().term,
                (None, None) => 0,
            })
            .max()
            .unwrap_or(0)
    }

    fn apply(&mut self, id: NodeId, term_now: u64, sm: &mut Sm, e: &Entry) {
        if e.index != sm.applied + 1 {
            self.fail(format!("member {} applied entry {} after {}", id, e.index, sm.applied));
        }
        let h = entry_hash(e);
        match self.committed.get(&e.index) {
            Some(&(t, eh, _)) if t != e.term || eh != h => self.fail(format!(
                "entry {} committed as one of term {} and applied by member {} as one of term {}", e.index, t, id, e.term)),
            Some(_) => {}
            None => {
                /* It was committed by a leader of a term no later than any
                 * member has now: what leaders of later terms must hold. */
                let seen = self.highest_term().max(term_now);
                self.committed.insert(e.index, (e.term, h, seen));
            }
        }
        sm.hash = mix(sm.hash, h);
        sm.applied = e.index;
        match self.state_at.get(&e.index) {
            Some(&s) if s != sm.hash => self.fail(format!("member {}'s state after entry {} is not the others'", id, e.index)),
            Some(_) => {}
            None => {
                self.state_at.insert(e.index, sm.hash);
            }
        }
        if e.kind == EntryKind::Change {
            if let Ok(cc) = raft::wire::decode_change(&e.data) {
                if cc.kind == ChangeKind::Remove && self.retire_on_removal {
                    self.to_retire.insert(cc.node);
                }
            }
        }
        self.observe(e.index);
    }

    /// Members whose removal is committed stop, as their programs would.
    fn retire_removed(&mut self) {
        let gone: Vec<NodeId> = std::mem::take(&mut self.to_retire).into_iter().collect();
        for id in gone {
            if let Some(m) = self.members.get_mut(&id) {
                m.retired = true;
            }
        }
    }

    fn check_leader(&mut self, id: NodeId, term: u64, node: &RawNode<Disk>) {
        match self.leaders.get(&term) {
            Some(&other) if other != id => {
                self.fail(format!("two leaders in term {}: members {} and {}", term, other, id));
            }
            Some(_) => {}
            None => {
                self.leaders.insert(term, id);
            }
        }
        /* Every entry committed in an earlier term: those it was first seen
         * committed with every term still before this one. */
        let mut lacking = Vec::new();
        for (&i, &(t, h, seen)) in &self.committed {
            if i < node.first_index() || seen >= term {
                continue;
            }
            match node.entries(i, i + 1, u64::MAX) {
                Ok(v) if v.len() == 1 && v[0].term == t && entry_hash(&v[0]) == h => {}
                _ => lacking.push(i),
            }
        }
        if let Some(i) = lacking.first() {
            self.fail(format!("member {}, leader of term {}, lacks committed entry {}", id, term, i));
        }
    }

    /// Two logs that agree on an entry's term agree on everything before it.
    pub fn check_logs(&mut self) {
        let ids = self.live();
        let mut found = None;
        'pairs: for (k, &a) in ids.iter().enumerate() {
            for &b in &ids[k + 1..] {
                let (Some(na), Some(nb)) = (self.node(a), self.node(b)) else { continue };
                let lo = na.first_index().max(nb.first_index());
                let hi = na.last_index().min(nb.last_index());
                if lo > hi {
                    continue;
                }
                let (Ok(ea), Ok(eb)) = (na.entries(lo, hi + 1, u64::MAX), nb.entries(lo, hi + 1, u64::MAX)) else {
                    continue;
                };
                let Some(top) = (0..ea.len().min(eb.len())).rev().find(|&j| ea[j].term == eb[j].term) else { continue };
                if let Some(j) = (0..=top).find(|&j| ea[j] != eb[j]) {
                    found = Some(format!("members {} and {} agree on entry {}'s term but differ at entry {}",
                                         a, b, ea[top].index, ea[j].index));
                    break 'pairs;
                }
            }
        }
        if let Some(f) = found {
            self.fail(f);
        }
    }

    pub fn crash(&mut self, id: NodeId) {
        if let Some(m) = self.members.get_mut(&id) {
            if let Some(node) = m.node.take() {
                m.disk = Some(node.into_storage());
            }
        }
        /* The reads asked of it are gone with what it had in memory: it may
         * lead again in the same term, a vote it had not written given
         * again, and owes them nothing. */
        self.leader_reads.retain(|(m, _), _| *m != id);
        self.note(&format!("crash {}", id));
    }

    pub fn restart(&mut self, id: NodeId) {
        let Some(m) = self.members.get_mut(&id) else { return };
        let Some(disk) = m.disk.take() else { return };
        let sm = sm_of_disk(&disk);
        let cfg = self.config(id);
        match RawNode::new(cfg, disk) {
            Ok(node) => {
                let m = self.members.get_mut(&id).expect("the member is there");
                m.node = Some(node);
                m.sm = sm;
                m.since_reset = 0;
            }
            Err(e) => self.fail(format!("member {} did not start again: {}", id, e)),
        }
        self.note(&format!("restart {}", id));
    }

    /// Member `id` back with a disk wiped clean, under the same id: refused
    /// unless `allow_wipe` -- a member that forgot its vote must never vote
    /// again as itself.
    pub fn wipe(&mut self, id: NodeId, disk: MemStorage) -> Result<(), String> {
        if !self.allow_wipe {
            return Err(format!("member {} back with nothing under its old id: refused, as a new incarnation is a new member", id));
        }
        self.crash(id);
        let disk = Disk::new(disk, self.p.read_fail_one_in, mix(self.seed, id ^ 0x5EED));
        if let Some(m) = self.members.get_mut(&id) {
            m.disk = Some(disk);
        }
        self.restart(id);
        Ok(())
    }

    /// Whether member `id`'s disk failed a read in what it just did: if so
    /// the member must have stopped on it -- `fault` said, no `Ready` left
    /// -- and it is crashed, to be started again over its disk.
    fn check_fault(&mut self, id: NodeId) {
        let Some(node) = self.members.get(&id).and_then(|m| m.node.as_ref()) else { return };
        let failed = node.storage().take_failed();
        let fault = node.fault();
        if failed > 0 && fault.is_none() {
            self.fail(format!("member {}'s disk failed {} reads and the member went on", id, failed));
            return;
        }
        if fault.is_none() {
            return;
        }
        if node.has_ready() {
            self.fail(format!("member {} stopped by a failed read still has a Ready", id));
        }
        self.faults += 1;
        self.crash(id);
        self.note(&format!("member {} stopped by a failed read: {:?}", id, fault));
    }

    /// Once healed and converged: an election's time and more of a healthy
    /// group, and then every read asked of a member that still leads in the
    /// term it was asked in answered -- a round whose own heartbeats were
    /// lost answered by later ones.
    fn reads_answered(&mut self) -> bool {
        for _ in 0..2 * u64::from(self.p.election_ticks) + 4 {
            for id in self.live() {
                self.tick(id);
            }
            self.flush(64);
        }
        let pending: Vec<((NodeId, u64), u64)> = self.leader_reads.iter().map(|(k, t)| (*k, *t)).collect();
        for ((id, ctx), term) in pending {
            let leading = self.node(id).is_some_and(|n| n.role() == Role::Leader && n.term() == term);
            if leading {
                self.fail(format!("read {} asked of member {}, leading in term {} since, not answered in a healthy group",
                                  ctx, id, term));
                return false;
            }
        }
        true
    }

    pub fn propose(&mut self, id: NodeId) -> Option<u64> {
        let data = self.next_data.to_le_bytes().to_vec();
        self.next_data += 1;
        let r = self.node_mut(id)?.propose(data).ok();
        self.note(&format!("propose on {}: {:?}", id, r));
        r
    }

    pub fn propose_data(&mut self, id: NodeId, data: &[u8]) -> Result<u64, raft::Error> {
        let r = match self.node_mut(id) {
            Some(n) => n.propose(data.to_vec()),
            None => Err(raft::Error::NotLeader(raft::NONE)),
        };
        self.note(&format!("propose {:?} on {}: {:?}", data, id, r));
        r
    }

    pub fn change(&mut self, id: NodeId, cc: ConfChange) -> Result<u64, raft::Error> {
        let r = match self.node_mut(id) {
            Some(n) => n.propose_change(cc),
            None => Err(raft::Error::NotLeader(raft::NONE)),
        };
        self.note(&format!("change {:?} on {}: {:?}", cc, id, r));
        r
    }

    pub fn read(&mut self, id: NodeId) -> Option<u64> {
        self.observe_all();
        let ctx = self.next_ctx;
        self.next_ctx += 1;
        let need = self.max_committed;
        let node = self.node_mut(id)?;
        let ok = node.read_index(ctx).is_ok();
        let leading = (node.role() == Role::Leader).then(|| node.term());
        if ok {
            self.reads.insert((id, ctx), need);
            self.asked.insert((id, ctx));
            if let Some(term) = leading {
                self.leader_reads.insert((id, ctx), term);
            }
        }
        ok.then_some(ctx)
    }

    pub fn campaign(&mut self, id: NodeId) {
        let Some(mut m) = self.take(id) else { return };
        if let Some(node) = m.node.as_mut() {
            let _ = node.campaign();
            m.since_reset = 0;
        }
        self.put(m);
        self.note(&format!("campaign {}", id));
    }

    pub fn transfer(&mut self, from: NodeId, to: NodeId) {
        if let Some(n) = self.node_mut(from) {
            let _ = n.transfer_leader(to);
        }
    }

    /// The member's applied state made its snapshot, the log before it
    /// dropped.
    pub fn snapshot(&mut self, id: NodeId) {
        let filler = self.rng.below(1500) as usize;
        self.snapshot_sized(id, filler);
    }

    /// Reads asked of member `id` and neither confirmed nor failed yet.
    pub fn reads_open(&self, id: NodeId) -> usize {
        self.reads.keys().filter(|(m, _)| *m == id).count()
    }

    /// `snapshot`, the state's bytes followed by `filler` bytes more.
    pub fn snapshot_sized(&mut self, id: NodeId, filler: usize) {
        let Some(m) = self.members.get_mut(&id) else { return };
        let sm = m.sm;
        let Some(node) = m.node.as_mut() else { return };
        let have = node.storage().snapshot_meta().index;
        if sm.applied <= have || sm.applied > node.storage().last_index() {
            return;
        }
        let Some(conf) = node.conf_state_at(sm.applied) else { return };
        if let Err(e) = node.storage_mut().create_snapshot(sm.applied, conf, sm_snapshot(sm, filler)) {
            let msg = format!("member {} could not take a snapshot at {}: {:?}", id, sm.applied, e);
            self.fail(msg);
        }
        self.note(&format!("snapshot {} at {}", id, sm.applied));
    }

    pub fn cut_link(&mut self, a: NodeId, b: NodeId) {
        self.cut.insert((a, b));
        self.cut.insert((b, a));
    }

    pub fn isolate(&mut self, id: NodeId) {
        let all: Vec<NodeId> = self.members.keys().copied().collect();
        for other in all {
            if other != id {
                self.cut_link(id, other);
            }
        }
    }

    /// The members split in two, nothing crossing.
    pub fn split(&mut self, side: &[NodeId]) {
        let all: Vec<NodeId> = self.members.keys().copied().collect();
        for &a in side {
            for &b in &all {
                if !side.contains(&b) {
                    self.cut_link(a, b);
                }
            }
        }
    }

    pub fn heal(&mut self) {
        self.cut.clear();
    }

    pub fn process_all(&mut self) {
        for id in self.live() {
            self.process(id);
        }
    }

    /// Every message delivered, in order, and every member's work done,
    /// until nothing is left to do -- at most `rounds` times over.
    pub fn flush(&mut self, rounds: usize) {
        for _ in 0..rounds {
            self.process_all();
            if self.net.is_empty() {
                return;
            }
            let msgs = std::mem::take(&mut self.net);
            for m in msgs {
                self.deliver(m);
            }
        }
    }

    /// `flush`, delivering only what `keep` says, and dropping the rest.
    pub fn flush_only(&mut self, rounds: usize, keep: impl Fn(&Message) -> bool) {
        for _ in 0..rounds {
            self.process_all();
            if self.net.is_empty() {
                return;
            }
            let msgs = std::mem::take(&mut self.net);
            for m in msgs {
                if keep(&m) {
                    self.deliver(m);
                }
            }
        }
    }

    /// The messages in the network that `keep` says, delivered in order;
    /// the rest left where they are.
    pub fn deliver_where(&mut self, keep: impl Fn(&Message) -> bool) {
        let msgs = std::mem::take(&mut self.net);
        let mut rest = Vec::new();
        for m in msgs {
            if keep(&m) {
                self.deliver(m);
            } else {
                rest.push(m);
            }
        }
        rest.append(&mut self.net);
        self.net = rest;
    }

    /* ---- the fuzzer's steps ---- */

    pub fn random_step(&mut self) {
        self.steps += 1;
        let live = self.live();
        let roll = self.rng.below(1000);
        match roll {
            0..=399 if !self.net.is_empty() => {
                let i = self.rng.below(self.net.len() as u64) as usize;
                self.deliver_at(i, true);
            }
            0..=599 => {
                if let Some(id) = self.rng.pick(&live) {
                    self.tick(id);
                }
            }
            600..=749 => {
                if let Some(id) = self.rng.pick(&live) {
                    self.process(id);
                }
            }
            750..=809 => {
                let id = match self.leader() {
                    Some(l) if self.rng.chance(80) => Some(l),
                    _ => self.rng.pick(&live),
                };
                if let Some(id) = id {
                    self.propose(id);
                }
            }
            810..=839 => {
                if let Some(id) = self.rng.pick(&live) {
                    self.read(id);
                }
            }
            840..=849 => {
                if live.len() > 1 {
                    if let Some(id) = self.rng.pick(&live) {
                        self.crash(id);
                    }
                }
            }
            850..=889 => {
                let down = self.crashed();
                if let Some(id) = self.rng.pick(&down) {
                    self.restart(id);
                }
            }
            890..=899 => {
                let all: Vec<NodeId> = self.members.keys().copied().collect();
                let side: Vec<NodeId> = all.iter().copied().filter(|_| self.rng.chance(50)).collect();
                if !side.is_empty() && side.len() < all.len() {
                    self.split(&side);
                    self.note(&format!("split {:?}", side));
                }
            }
            900..=929 => {
                self.heal();
                self.note("heal");
            }
            930..=944 => {
                if let Some(id) = self.rng.pick(&live) {
                    self.snapshot(id);
                }
            }
            945..=959 => self.random_change(),
            960..=969 => {
                if let Some(l) = self.leader() {
                    let voters = self.node(l).map(|n| n.membership().voters).unwrap_or_default();
                    if let Some(to) = self.rng.pick(&voters) {
                        self.transfer(l, to);
                        self.note(&format!("transfer {} -> {}", l, to));
                    }
                }
            }
            970..=979 => {
                if let Some(id) = self.rng.pick(&live) {
                    self.campaign(id);
                }
            }
            _ => self.process_all(),
        }
        if self.steps % 64 == 0 {
            self.check_logs();
        }
    }

    fn random_change(&mut self) {
        let Some(l) = self.leader() else { return };
        let Some(conf) = self.node(l).map(|n| n.membership()) else { return };
        let members: Vec<NodeId> = conf.voters.iter().chain(&conf.learners).copied().filter(|&x| x != l).collect();
        let cc = match self.rng.below(4) {
            0 if conf.voters.len() + conf.learners.len() < 7 => {
                /* The new member is made once its change is taken, before
                 * anything is delivered to it. */
                let id = self.next_id;
                if self.change(l, ConfChange { kind: ChangeKind::AddLearner, node: id }).is_ok() {
                    self.add_fresh();
                }
                return;
            }
            1 => match self.rng.pick(&conf.learners) {
                Some(x) => ConfChange { kind: ChangeKind::AddVoter, node: x },
                None => return,
            },
            2 => {
                let others: Vec<NodeId> = conf.voters.iter().copied().filter(|&x| x != l).collect();
                match self.rng.pick(&others) {
                    Some(x) => ConfChange { kind: ChangeKind::AddLearner, node: x },
                    None => return,
                }
            }
            _ => match self.rng.pick(&members) {
                Some(x) => ConfChange { kind: ChangeKind::Remove, node: x },
                None => return,
            },
        };
        let _ = self.change(l, cc);
    }

    /// Each member as it stands: for a finding's report.
    pub fn describe(&self) -> String {
        let mut out = Vec::new();
        for m in self.members.values() {
            let what = match &m.node {
                Some(n) => format!(
                    "{}: {:?} term {} vote {} leader {} commit {} applied {} log {}..{} snap {} sm {} voters {:?} learners {:?}{}",
                    m.id,
                    n.role(),
                    n.term(),
                    n.vote(),
                    n.leader(),
                    n.committed(),
                    n.applied(),
                    n.first_index(),
                    n.last_index(),
                    n.storage().snapshot_meta().index,
                    m.sm.applied,
                    n.membership().voters,
                    n.membership().learners,
                    if m.retired { " retired" } else { "" }
                ),
                None => format!("{}: down{}", m.id, if m.retired { " retired" } else { "" }),
            };
            out.push(what);
        }
        out.join(" | ")
    }

    /// The network healed, every member up, no faults: a leader, and an
    /// entry proposed now applied by every voter, within `rounds` rounds of
    /// a tick each.
    pub fn converge(&mut self, rounds: u64) -> bool {
        self.heal();
        self.p.drop_pct = 0;
        self.p.dup_pct = 0;
        /* Healthy disks too. */
        self.p.read_fail_one_in = 0;
        for m in self.members.values_mut() {
            if let Some(n) = m.node.as_mut() {
                n.storage_mut().fail_one_in = 0;
            }
            if let Some(d) = m.disk.as_mut() {
                d.fail_one_in = 0;
            }
        }
        for id in self.crashed() {
            self.restart(id);
        }
        let mut probe: Option<u64> = None;
        for round in 0..rounds {
            for id in self.live() {
                self.tick(id);
            }
            self.flush(64);
            if let Some(index) = probe {
                let voters: Vec<NodeId> = match self.leader().and_then(|l| self.node(l)) {
                    Some(n) => n.membership().voters,
                    None => Vec::new(),
                };
                let live = self.live();
                if !voters.is_empty()
                    && voters.iter().all(|v| !live.contains(v) || self.members.get(v).is_some_and(|m| m.sm.applied >= index))
                {
                    return self.reads_answered();
                }
            }
            if probe.is_none() || round % 64 == 63 {
                if let Some(l) = self.leader() {
                    if let Some(i) = self.propose(l) {
                        probe = Some(i);
                    }
                }
            }
        }
        false
    }
}
