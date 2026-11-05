//! The review of the first version (a84d4e2f), each finding a control that
//! failed on it: a storage's failed read taken as no entry, a message's
//! buffer grown past a block of the heap, a read whose round was lost never
//! answered, a snapshot that copied the log after it, an append dropped
//! whole where part of it fit, and what a storage or a configuration says
//! taken unbounded -- and a snapshot given up mid-way held for good. Run as
//! scenarios, in the gate; each uses nothing the first version did not
//! have, so that it can be run against it.

use std::collections::VecDeque;

use raft::wire;
use raft::{
    Body, ConfState, Config, Entry, EntryKind, HardState, MemStorage, Message, RaftState, RawNode, ReadState, Role,
    Snapshot, SnapshotMeta, Storage, StorageError, PIECE_MAX,
};

pub const ALL: &[(&str, fn() -> Result<(), String>)] = &[
    ("storage_fault_vote", storage_fault_vote),
    ("storage_fault_spin", storage_fault_spin),
    ("encode_reserves", encode_reserves),
    ("read_round_lost", read_round_lost),
    ("snapshot_long_tail", snapshot_long_tail),
    ("partial_append", partial_append),
    ("config_pair", config_pair),
    ("storage_values_bounded", storage_values_bounded),
    ("snapshot_abandoned", snapshot_abandoned),
];

fn ensure(ok: bool, what: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what.to_string()) }
}

/// A storage over a `MemStorage` that fails to read one entry's term, or
/// any entries at all.
struct Failing {
    inner: MemStorage,
    term_at: Option<u64>,
    entries: bool,
}

impl Storage for Failing {
    fn initial_state(&self) -> Result<RaftState, StorageError> {
        self.inner.initial_state()
    }
    fn entries(&self, lo: u64, hi: u64, max_bytes: u64, out: &mut Vec<Entry>) -> Result<(), StorageError> {
        if self.entries {
            return Err(StorageError::Corrupt);
        }
        self.inner.entries(lo, hi, max_bytes, out)
    }
    fn term(&self, index: u64) -> Result<u64, StorageError> {
        if Some(index) == self.term_at {
            return Err(StorageError::Corrupt);
        }
        self.inner.term(index)
    }
    fn first_index(&self) -> u64 {
        self.inner.first_index()
    }
    fn last_index(&self) -> u64 {
        self.inner.last_index()
    }
    fn snapshot(&self) -> Result<Snapshot, StorageError> {
        self.inner.snapshot()
    }
}

/// A voter whose storage cannot read its last entry's term, asked for its
/// vote by a candidate missing seven committed entries: no vote given --
/// none sent, none written. (The first version read the failure as a last
/// term of 0 and voted.)
fn storage_fault_vote() -> Result<(), String> {
    let mut s = MemStorage::bootstrap(&[1, 2, 3], &[]);
    let ents: Vec<Entry> = (4..=10u64).map(|i| Entry { term: 2, index: i, kind: EntryKind::Normal, data: vec![1] }).collect();
    s.append(&ents).map_err(|e| format!("{:?}", e))?;
    s.set_hard_state(HardState { term: 2, vote: 0, commit: 10 });
    let mut n = RawNode::new(Config::new(1), Failing { inner: s, term_at: Some(10), entries: false })
        .map_err(|e| e.to_string())?;
    let _ = n.step(Message { from: 2, to: 1, term: 3, body: Body::Vote { pre: false, transfer: false, last_index: 3, last_term: 1 } });
    for _ in 0..4 {
        if !n.has_ready() {
            break;
        }
        let rd = n.ready();
        let granted = rd.messages.iter().any(|m| matches!(m.body, Body::VoteResp { granted: true, pre: false }));
        ensure(!granted, "a vote given on a last term the storage failed to read")?;
        ensure(rd.hard_state.is_none_or(|hs| hs.vote != 2), "a vote written on a last term the storage failed to read")?;
        n.advance();
    }
    Ok(())
}

/// A storage that stops reading entries once one is committed: the member
/// does not hand out empty `Ready`s for good, saying nothing. (The first
/// version did, 100,000 of them.)
fn storage_fault_spin() -> Result<(), String> {
    let mut n = RawNode::new(Config::new(1), Failing { inner: MemStorage::bootstrap(&[1], &[]), term_at: None, entries: false })
        .map_err(|e| e.to_string())?;
    let drain = |n: &mut RawNode<Failing>, limit: usize| -> usize {
        let mut k = 0;
        while n.has_ready() && k < limit {
            let rd = n.ready();
            n.storage_mut().inner.append(&rd.entries).expect("written");
            if let Some(hs) = rd.hard_state {
                n.storage_mut().inner.set_hard_state(hs);
            }
            n.advance();
            k += 1;
        }
        k
    };
    for _ in 0..30 {
        n.tick();
    }
    drain(&mut n, 100);
    ensure(n.role() == Role::Leader, "a lone voter not leading")?;
    n.propose(b"x".to_vec()).map_err(|e| e.to_string())?;
    let rd = n.ready();
    n.storage_mut().inner.append(&rd.entries).expect("written");
    if let Some(hs) = rd.hard_state {
        n.storage_mut().inner.set_hard_state(hs);
    }
    n.advance();
    n.storage_mut().entries = true;
    let rounds = drain(&mut n, 1000);
    ensure(rounds < 1000, "a member whose storage fails to read hands out empty Readies for good")?;
    /* And whatever it is asked now says why. */
    ensure(n.propose(b"y".to_vec()).is_err(), "a proposal taken by a member whose storage failed a read")
}

/// An append of ordinary sizes, within the default limits, written into an
/// empty buffer: no block past the kernel heap's. (The first version grew
/// the buffer as it wrote: 527,264 bytes asked for one of 262,000.)
fn encode_reserves() -> Result<(), String> {
    let append = |entries: Vec<Entry>| Message {
        from: 1,
        to: 2,
        term: 2,
        body: Body::Append { prev_index: 100, prev_term: 2, commit: 100, entries },
    };
    let mut mixed = vec![Entry { term: 2, index: 101, kind: EntryKind::Normal, data: vec![1; 16_400] }];
    for k in 0..70u64 {
        mixed.push(Entry { term: 2, index: 102 + k, kind: EntryKind::Normal, data: vec![2; 3_510] });
    }
    let mut catch_up: Vec<Entry> =
        (0..4u64).map(|k| Entry { term: 2, index: 101 + k, kind: EntryKind::Normal, data: vec![3; 64 * 1024] }).collect();
    for k in 0..13u64 {
        catch_up.push(Entry { term: 2, index: 105 + k, kind: EntryKind::Normal, data: Vec::new() });
    }
    for m in [append(mixed), append(catch_up)] {
        let mut out = Vec::new();
        wire::encode(&m, &mut out).map_err(|e| format!("{:?}", e))?;
        ensure(wire::decode(&out).is_ok_and(|back| back == m), "an append not read back as written")?;
    }
    /* The heap's ceiling is held by `guarded`, which runs this. */
    Ok(())
}

/// Members over a network in order, a `keep` deciding what arrives.
struct Net {
    nodes: Vec<RawNode<MemStorage>>,
    q: VecDeque<Message>,
}

impl Net {
    fn new(n: u64, cfg: impl Fn(&mut Config)) -> Result<Net, String> {
        let ids: Vec<u64> = (1..=n).collect();
        let mut nodes = Vec::new();
        for &id in &ids {
            let mut c = Config::new(id);
            cfg(&mut c);
            nodes.push(RawNode::new(c, MemStorage::bootstrap(&ids, &[])).map_err(|e| e.to_string())?);
        }
        Ok(Net { nodes, q: VecDeque::new() })
    }

    fn node(&mut self, id: u64) -> &mut RawNode<MemStorage> {
        &mut self.nodes[(id - 1) as usize]
    }

    fn process(&mut self) -> Vec<ReadState> {
        let mut reads = Vec::new();
        for n in &mut self.nodes {
            while n.has_ready() {
                let rd = n.ready();
                let s = n.storage_mut();
                if let Some(snap) = &rd.snapshot {
                    s.apply_snapshot(snap.clone()).expect("snapshot written");
                }
                s.append(&rd.entries).expect("entries written");
                if let Some(hs) = rd.hard_state {
                    s.set_hard_state(hs);
                }
                reads.extend(rd.read_states);
                self.q.extend(rd.messages);
                n.advance();
            }
        }
        reads
    }

    fn flush(&mut self, keep: &dyn Fn(&Message) -> bool) -> Vec<ReadState> {
        let mut reads = Vec::new();
        for _ in 0..30 {
            reads.extend(self.process());
            if self.q.is_empty() {
                break;
            }
            let msgs: Vec<Message> = self.q.drain(..).collect();
            for m in msgs {
                if keep(&m) {
                    let to = m.to;
                    let _ = self.node(to).step(m);
                }
            }
        }
        reads
    }
}

fn all(_: &Message) -> bool {
    true
}

/// A read asked of a leader of a healthy group, the heartbeats of its own
/// round lost: answered by the next heartbeats, within an election's time.
/// (The first version asked a round by its own heartbeats alone, and the
/// read waited for good.)
fn read_round_lost() -> Result<(), String> {
    let mut net = Net::new(3, |_| {})?;
    net.node(1).campaign().map_err(|e| e.to_string())?;
    net.flush(&all);
    ensure(net.node(1).role() == Role::Leader, "no leader")?;
    net.node(1).propose(b"x".to_vec()).map_err(|e| e.to_string())?;
    net.flush(&all);
    net.node(1).read_index(1).map_err(|e| e.to_string())?;
    let mut got = net.flush(&|m: &Message| !matches!(m.body, Body::Heartbeat { ctx, .. } if ctx != 0)).iter().any(|r| r.ctx == 1);
    ensure(!got, "a read confirmed with its round lost")?;
    let ticks = 2 * Config::new(1).election_ticks;
    for _ in 0..ticks {
        for id in 1..=3 {
            net.node(id).tick();
        }
        got |= net.flush(&all).iter().any(|r| r.ctx == 1);
    }
    ensure(net.node(1).role() == Role::Leader, "the leader lost its leadership in a healthy group")?;
    ensure(got, "a read whose round was lost not answered in a healthy group within two election timeouts")
}

/// A snapshot taken at 100 of a log of 20,000 entries: no block past the
/// heap's (the first version copied the 19,900 after it into one list),
/// and the log after it the same.
fn snapshot_long_tail() -> Result<(), String> {
    let mut s = MemStorage::new();
    for start in (1..=20_000u64).step_by(1000) {
        let c: Vec<Entry> =
            (start..start + 1000).map(|i| Entry { term: 1 + i / 5000, index: i, kind: EntryKind::Normal, data: vec![i as u8] }).collect();
        s.append(&c).map_err(|e| format!("{:?}", e))?;
    }
    let data = raft::SnapshotBuilder::new().finish();
    s.create_snapshot(100, ConfState { voters: vec![1], learners: vec![] }, data.clone()).map_err(|e| format!("{:?}", e))?;
    ensure(s.first_index() == 101 && s.last_index() == 20_000, "the log's ends after a snapshot")?;
    s.create_snapshot(15_555, ConfState { voters: vec![1], learners: vec![] }, data).map_err(|e| format!("{:?}", e))?;
    let more: Vec<Entry> = (15_600..=21_000u64).map(|i| Entry { term: 9, index: i, kind: EntryKind::Normal, data: vec![9] }).collect();
    s.append(&more).map_err(|e| format!("{:?}", e))?;
    for i in [15_556u64, 15_599, 15_600, 21_000] {
        let want = if i < 15_600 { 1 + i / 5000 } else { 9 };
        ensure(s.term(i) == Ok(want), "a term after a snapshot and an append over the log's end")?;
    }
    let mut out = Vec::new();
    s.entries(15_556, 15_560, u64::MAX, &mut out).map_err(|e| format!("{:?}", e))?;
    ensure(out.iter().map(|e| e.index).eq(15_556..15_560) && out[0].data == vec![15_556u64 as u8], "entries after a snapshot")
}

/// A follower with entries not yet written given an append of more than
/// room is left for: it takes what fits and says how far. (The first
/// version dropped the whole append and said nothing.)
fn partial_append() -> Result<(), String> {
    let mut c = Config::new(2);
    c.max_entries_per_msg = 16;
    c.max_uncommitted_entries = 16;
    let mut n = RawNode::new(c, MemStorage::bootstrap(&[1, 2], &[])).map_err(|e| e.to_string())?;
    let append = |prev_index: u64, prev_term: u64, n: u64| Message {
        from: 1,
        to: 2,
        term: 2,
        body: Body::Append {
            prev_index,
            prev_term,
            commit: 2,
            entries: (1..=n).map(|k| Entry { term: 2, index: prev_index + k, kind: EntryKind::Normal, data: vec![] }).collect(),
        },
    };
    n.step(append(2, 1, 10)).map_err(|e| e.to_string())?;
    n.step(append(12, 2, 16)).map_err(|e| e.to_string())?;
    let rd = n.ready();
    let answers: Vec<u64> = rd
        .messages
        .iter()
        .filter_map(|m| match m.body {
            Body::AppendResp { reject: false, index, .. } => Some(index),
            _ => None,
        })
        .collect();
    ensure(answers == vec![12, 18], "an append past the room left not taken in part, or not answered")
}

/// A configuration whose appends are larger than what may wait to be
/// written: refused, or a follower that missed 150 entries still catches
/// up. (The first version took it, and the follower stayed behind.)
fn config_pair() -> Result<(), String> {
    let pair = |c: &mut Config| {
        c.max_entries_per_msg = 64;
        c.max_uncommitted_entries = 16;
    };
    let mut c = Config::new(1);
    pair(&mut c);
    if c.validate().is_err() {
        return Ok(());
    }
    let mut net = Net::new(3, pair)?;
    net.node(1).campaign().map_err(|e| e.to_string())?;
    net.flush(&all);
    for _ in 0..10 {
        for _ in 0..15 {
            let _ = net.node(1).propose(b"a".to_vec());
        }
        net.flush(&|m: &Message| m.to != 3 && m.from != 3);
    }
    for _ in 0..300 {
        for id in 1..=3 {
            net.node(id).tick();
        }
        net.flush(&all);
    }
    let committed = net.node(1).committed();
    ensure(net.node(3).last_index() >= committed, "a follower that cannot take a whole append left behind for good")
}

/// What a storage or a configuration says, at the edge: a term of
/// u64::MAX, an index of u64::MAX asked about, an election timeout of
/// three billion ticks -- refused, or run without a panic. (The first
/// version took each and overflowed.)
fn storage_values_bounded() -> Result<(), String> {
    let mut s = MemStorage::bootstrap(&[1, 2, 3], &[]);
    let mut hs = s.hard_state();
    hs.term = u64::MAX;
    s.set_hard_state(hs);
    if let Ok(mut n) = RawNode::new(Config::new(1), s) {
        for _ in 0..30 {
            n.tick();
        }
    }
    let n = RawNode::new(Config::new(1), MemStorage::bootstrap(&[1, 2, 3], &[])).map_err(|e| e.to_string())?;
    ensure(n.conf_state_at(u64::MAX).is_none(), "a membership at u64::MAX")?;
    let mut c = Config::new(1);
    c.election_ticks = 3_000_000_000;
    if c.validate().is_ok() {
        let mut n = RawNode::new(c, MemStorage::bootstrap(&[1, 2, 3], &[])).map_err(|e| e.to_string())?;
        n.tick();
    }
    Ok(())
}

/// A follower sent the first piece of a snapshot and then nothing of it
/// for three election timeouts, its leader's heartbeats coming meanwhile:
/// the piece is dropped, and the next one is answered by asking for the
/// first again. (The first version held it until the term changed.)
fn snapshot_abandoned() -> Result<(), String> {
    let mut n = RawNode::new(Config::new(2), MemStorage::bootstrap(&[1, 2], &[])).map_err(|e| e.to_string())?;
    let meta = SnapshotMeta { index: 50, term: 2, conf: ConfState { voters: vec![1, 2], learners: vec![] } };
    let piece = |offset: u64| Message {
        from: 1,
        to: 2,
        term: 2,
        body: Body::Snapshot { meta: meta.clone(), offset, total: 2 * PIECE_MAX as u64, data: vec![5; PIECE_MAX] },
    };
    n.step(piece(0)).map_err(|e| e.to_string())?;
    let drain = |n: &mut RawNode<MemStorage>| -> Vec<Message> {
        let mut out = Vec::new();
        while n.has_ready() {
            let rd = n.ready();
            n.storage_mut().append(&rd.entries).expect("written");
            if let Some(hs) = rd.hard_state {
                n.storage_mut().set_hard_state(hs);
            }
            out.extend(rd.messages);
            n.advance();
        }
        out
    };
    drain(&mut n);
    for _ in 0..3 * Config::new(2).election_ticks {
        n.tick();
        n.step(Message { from: 1, to: 2, term: 2, body: Body::Heartbeat { commit: 2, ctx: 0 } }).map_err(|e| e.to_string())?;
        drain(&mut n);
    }
    n.step(piece(PIECE_MAX as u64)).map_err(|e| e.to_string())?;
    let answers = drain(&mut n);
    let restart = answers.iter().any(|m| matches!(m.body, Body::SnapshotResp { next: 0, reject: false, .. }));
    ensure(restart, "a snapshot given up for three election timeouts still held, its next piece taken")
}
