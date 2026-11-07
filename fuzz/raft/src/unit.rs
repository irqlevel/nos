//! Known answers: the crate's pieces held one at a time to what each must
//! do -- the wire's bytes, the storage's log, a single member's leadership,
//! the configuration's checks, the membership change rules.

use raft::wire::{self, DecodeError};
use raft::{
    Body, ChangeKind, ConfChange, ConfState, Config, Entry, EntryKind, MemStorage, Message, RawNode, Role,
    SnapshotBuilder, SnapshotMeta, Storage, StorageError, PIECE_MAX,
};

use crate::scenarios::entry;

pub const ALL: &[(&str, fn() -> Result<(), String>)] = &[
    ("wire_known", wire_known),
    ("wire_refusals", wire_refusals),
    ("storage_log", storage_log),
    ("storage_snapshot", storage_snapshot),
    ("snapshot_data", snapshot_data),
    ("config_checks", config_checks),
    ("single_voter", single_voter),
    ("change_rules", change_rules),
    ("bootstrap_membership", bootstrap_membership),
    ("long_log", long_log),
    ("config_extremes", config_extremes),
    ("crc32c_known", crc32c_known),
];

/// CRC-32C's known answers: the check value of "123456789", and RFC 3720's
/// vectors (B.4) -- 32 zeros, 32 ones, 32 bytes counting up and down -- each
/// whole and carried on in two parts across every split.
fn crc32c_known() -> Result<(), String> {
    use crc::crc32c::{checksum as crc32c, update};
    let up: Vec<u8> = (0u8..32).collect();
    let down: Vec<u8> = (0u8..32).rev().collect();
    let cases: [(&[u8], u32); 5] = [
        (b"123456789", 0xE306_9283),
        (&[0u8; 32], 0x8A91_36AA),
        (&[0xFFu8; 32], 0x62A8_AB43),
        (&up, 0x46DD_794E),
        (&down, 0x113F_DB5C),
    ];
    for (bytes, want) in cases {
        ensure(crc32c(bytes) == want, &format!("crc32c of {} bytes is {:08x}, not {:08x}", bytes.len(), crc32c(bytes), want))?;
        for k in 0..=bytes.len() {
            ensure(update(crc32c(&bytes[..k]), &bytes[k..]) == want, "crc32c carried on across a split")?;
        }
    }
    ensure(crc32c(&[]) == 0, "crc32c of nothing")
}

fn ensure(ok: bool, what: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what.to_string()) }
}

fn every_kind() -> Vec<Message> {
    let m = |body| Message { from: 7, to: 9, term: 42, body };
    vec![
        m(Body::Vote { pre: true, transfer: false, last_index: 10, last_term: 3 }),
        m(Body::Vote { pre: false, transfer: true, last_index: u64::MAX, last_term: 0 }),
        m(Body::VoteResp { pre: true, granted: false }),
        m(Body::VoteResp { pre: false, granted: true }),
        m(Body::Append {
            prev_index: 5,
            prev_term: 2,
            commit: 4,
            entries: vec![
                entry(6, 2, b"x"),
                Entry { index: 7, term: 3, kind: EntryKind::Noop, data: vec![] },
                Entry {
                    index: 8,
                    term: 3,
                    kind: EntryKind::Change,
                    data: wire::encode_change(&ConfChange { kind: ChangeKind::AddLearner, node: 11 }),
                },
            ],
        }),
        m(Body::AppendResp { reject: true, index: 9, hint_index: 4, hint_term: 2 }),
        m(Body::Heartbeat { commit: 3, ctx: 77 }),
        m(Body::HeartbeatResp { ctx: 77 }),
        m(Body::Snapshot {
            meta: SnapshotMeta { index: 30, term: 4, conf: ConfState { voters: vec![1, 2, 3], learners: vec![9] } },
            offset: 64,
            total: 200,
            data: vec![0xAB; 64],
        }),
        m(Body::SnapshotResp { index: 30, next: 128, reject: false }),
        m(Body::TimeoutNow),
        m(Body::ReadIndex { ctx: 5 }),
        m(Body::ReadIndexResp { ctx: 5, index: 12, ok: true }),
    ]
}

/// Every kind of message written and read back as it was, and a vote's
/// bytes as the format says they are.
fn wire_known() -> Result<(), String> {
    for m in every_kind() {
        let mut b = Vec::new();
        wire::encode(&m, &mut b).map_err(|e| format!("{:?} not written: {:?}", m.body, e))?;
        let back = wire::decode(&b).map_err(|e| format!("{:?} not read back: {:?}", m.body, e))?;
        ensure(back == m, &format!("{:?} read back as {:?}", m, back))?;
    }
    let mut b = Vec::new();
    let vote = Message { from: 1, to: 2, term: 3, body: Body::Vote { pre: true, transfer: true, last_index: 4, last_term: 5 } };
    wire::encode(&vote, &mut b).map_err(|e| format!("{:?}", e))?;
    let mut want = vec![b'R', b'F', wire::VERSION, 1];
    for v in [1u64, 2, 3] {
        want.extend_from_slice(&v.to_le_bytes());
    }
    want.push(0b11);
    for v in [4u64, 5] {
        want.extend_from_slice(&v.to_le_bytes());
    }
    ensure(b == want, &format!("a vote written as {:02x?}, not {:02x?}", b, want))
}

/// Bytes that are not a message refused, each for its reason.
fn wire_refusals() -> Result<(), String> {
    let base = Message { from: 1, to: 2, term: 3, body: Body::HeartbeatResp { ctx: 4 } };
    let mut good = Vec::new();
    wire::encode(&base, &mut good).map_err(|e| format!("{:?}", e))?;
    let cases: Vec<(Vec<u8>, DecodeError)> = vec![
        (good[..good.len() - 1].to_vec(), DecodeError::Short),
        ([good.clone(), vec![0]].concat(), DecodeError::Trailing),
        ([&b"XF"[..], &good[2..]].concat(), DecodeError::BadMagic),
        ({ let mut v = good.clone(); v[2] = 9; v }, DecodeError::BadVersion),
        ({ let mut v = good.clone(); v[3] = 99; v }, DecodeError::BadKind),
        ({ let mut v = good.clone(); v[4..12].copy_from_slice(&0u64.to_le_bytes()); v }, DecodeError::Invalid),
    ];
    for (bytes, want) in cases {
        match wire::decode(&bytes) {
            Err(e) if e == want => {}
            r => return Err(format!("{:02x?} decoded as {:?}, not refused as {:?}", bytes, r, want)),
        }
    }
    /* A vote's flags with a bit nobody knows. */
    let mut v = Vec::new();
    wire::encode(&Message { from: 1, to: 2, term: 3, body: Body::VoteResp { pre: true, granted: true } }, &mut v)
        .map_err(|e| format!("{:?}", e))?;
    v[wire::HEADER_BYTES] |= 0x80;
    ensure(wire::decode(&v) == Err(DecodeError::BadFlags), "a flag nobody knows taken")?;
    /* A membership out of order. */
    let snap = |voters: Vec<u64>| Message {
        from: 1,
        to: 2,
        term: 3,
        body: Body::Snapshot { meta: SnapshotMeta { index: 1, term: 1, conf: ConfState { voters, learners: vec![] } }, offset: 0, total: 0, data: vec![] },
    };
    let mut s = Vec::new();
    wire::encode(&snap(vec![2, 1]), &mut s).map_err(|e| format!("{:?}", e))?;
    ensure(wire::decode(&s) == Err(DecodeError::Invalid), "a membership out of order taken")?;
    /* An append claiming more entries than its bytes hold. */
    let mut a = Vec::new();
    wire::encode(&Message { from: 1, to: 2, term: 3, body: Body::Append { prev_index: 0, prev_term: 0, commit: 0, entries: vec![] } }, &mut a)
        .map_err(|e| format!("{:?}", e))?;
    let at = a.len() - 4;
    a[at..].copy_from_slice(&4000u32.to_le_bytes());
    ensure(wire::decode(&a) == Err(DecodeError::Short), "4000 entries promised by an empty append taken")?;
    a[at..].copy_from_slice(&u32::MAX.to_le_bytes());
    ensure(wire::decode(&a) == Err(DecodeError::TooLarge), "u32::MAX entries promised taken")?;
    /* A change that is not one. */
    ensure(wire::decode_change(&[7, 1, 0, 0, 0, 0, 0, 0, 0]).is_err(), "a change of kind 7 taken")?;
    ensure(wire::decode_change(&[0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err(), "a change of member 0 taken")?;
    Ok(())
}

fn storage_log() -> Result<(), String> {
    let mut s = MemStorage::new();
    let ents: Vec<Entry> = (1..=3000).map(|i| entry(i, 1 + i / 1000, &i.to_le_bytes())).collect();
    s.append(&ents).map_err(|e| format!("{:?}", e))?;
    ensure(s.first_index() == 1 && s.last_index() == 3000, "3000 entries not 1..3000")?;
    ensure(s.term(2500) == Ok(3), "entry 2500's term")?;
    let mut out = Vec::new();
    s.entries(1020, 1030, u64::MAX, &mut out).map_err(|e| format!("{:?}", e))?;
    ensure(out.len() == 10 && out[0].index == 1020 && out[9].index == 1029, "entries across a piece's edge")?;
    /* Written over from 1500 on: what came after is gone. */
    s.append(&[entry(1500, 9, b"new")]).map_err(|e| format!("{:?}", e))?;
    ensure(s.last_index() == 1500 && s.term(1500) == Ok(9), "an entry written over")?;
    ensure(s.term(1501) == Err(StorageError::Unavailable), "an entry past the end")?;
    /* A byte limit: at least one entry, then no more than it allows. */
    out.clear();
    s.entries(1, 1500, 10, &mut out).map_err(|e| format!("{:?}", e))?;
    ensure(out.len() == 1, "a byte limit under one entry gave other than one")?;
    /* Not one after another, or a gap past the end: refused. */
    ensure(s.append(&[entry(1502, 9, b"")]).is_err(), "an entry past the end and one taken")?;
    ensure(s.append(&[entry(10, 1, b""), entry(12, 1, b"")]).is_err(), "entries with a gap taken")?;
    Ok(())
}

fn storage_snapshot() -> Result<(), String> {
    let mut s = MemStorage::new();
    let ents: Vec<Entry> = (1..=100).map(|i| entry(i, 2, b"e")).collect();
    s.append(&ents).map_err(|e| format!("{:?}", e))?;
    let conf = ConfState { voters: vec![1, 2, 3], learners: vec![] };
    s.create_snapshot(60, conf.clone(), SnapshotBuilder::new().finish()).map_err(|e| format!("{:?}", e))?;
    ensure(s.first_index() == 61 && s.last_index() == 100, "the log after a snapshot at 60")?;
    ensure(s.term(60) == Ok(2) && s.term(59) == Err(StorageError::Compacted), "terms around a snapshot")?;
    let mut out = Vec::new();
    ensure(s.entries(50, 70, u64::MAX, &mut out) == Err(StorageError::Compacted), "entries before a snapshot")?;
    ensure(s.create_snapshot(50, conf.clone(), SnapshotBuilder::new().finish()).is_err(), "an older snapshot taken")?;
    /* Entries before the snapshot, given again, skipped. */
    s.append(&[entry(59, 2, b"e"), entry(60, 2, b"e"), entry(61, 2, b"e")]).map_err(|e| format!("{:?}", e))?;
    ensure(s.first_index() == 61 && s.last_index() == 61, "entries across the snapshot's edge")?;
    Ok(())
}

fn snapshot_data() -> Result<(), String> {
    let mut b = SnapshotBuilder::new();
    let bytes: Vec<u8> = (0..(PIECE_MAX * 2 + 100)).map(|i| i as u8).collect();
    ensure(b.push(&bytes[..1000]) && b.push(&bytes[1000..]), "a snapshot's bytes refused")?;
    let d = b.finish();
    ensure(d.len() == bytes.len() as u64 && d.pieces().len() == 3, "pieces of a snapshot's data")?;
    let at = PIECE_MAX as u64 - 10;
    ensure(d.read(at, 100) == &bytes[at as usize..PIECE_MAX], "a read up to a piece's end")?;
    ensure(d.read(d.len(), 10).is_empty(), "a read at the end")?;
    let mut whole = Vec::new();
    let mut off = 0;
    while off < d.len() {
        let piece = d.read(off, 333);
        whole.extend_from_slice(piece);
        off += piece.len() as u64;
    }
    ensure(whole == bytes, "a snapshot's data read back whole")
}

fn config_checks() -> Result<(), String> {
    let ok = Config::new(1);
    ensure(ok.validate().is_ok(), "the defaults refused")?;
    let mut c = ok.clone();
    c.id = 0;
    ensure(c.validate().is_err(), "an id of 0 taken")?;
    let mut c = ok.clone();
    c.election_ticks = c.heartbeat_ticks;
    ensure(c.validate().is_err(), "an election no longer than a heartbeat taken")?;
    let mut c = ok.clone();
    c.max_bytes_per_msg = wire::MAX_MESSAGE_BYTES as u64;
    ensure(c.validate().is_err(), "messages larger than the wire takes")?;
    let mut c = ok.clone();
    c.max_uncommitted_entries = wire::MAX_ENTRIES as u64 + 1;
    ensure(c.validate().is_err(), "more entries waiting to be written than a block of the heap lists")?;
    let mut c = ok.clone();
    c.max_snapshot_bytes = raft::MAX_SNAPSHOT_BYTES + 1;
    ensure(c.validate().is_err(), "a snapshot past the largest there is taken")?;
    /* An append of three entries each as large as an entry is: past a
     * message, refused before a byte of it is written. */
    let big = |index| Entry { index, term: 1, kind: EntryKind::Normal, data: vec![7; wire::MAX_ENTRY_BYTES] };
    let m = Message { from: 1, to: 2, term: 1, body: Body::Append { prev_index: 0, prev_term: 0, commit: 0,
                                                                     entries: vec![big(1), big(2), big(3)] } };
    let mut out = Vec::new();
    let refused = wire::encode(&m, &mut out).is_err();
    ensure(refused && out.is_empty() && out.capacity() < 4096, "an append past a message written")?;
    let mut c = ok;
    c.snapshot_piece_bytes = PIECE_MAX + 1;
    ensure(c.validate().is_err(), "a piece larger than a piece taken")
}

/// One voter: it leads at its first timeout, commits what it writes, and
/// confirms reads on its own.
fn single_voter() -> Result<(), String> {
    let mut n = RawNode::new(Config::new(1), MemStorage::bootstrap(&[1], &[])).map_err(|e| e.to_string())?;
    let drain = |n: &mut RawNode<MemStorage>| {
        let mut applied = Vec::new();
        let mut reads = Vec::new();
        while n.has_ready() {
            let rd = n.ready();
            let s = n.storage_mut();
            s.append(&rd.entries).expect("entries written");
            if let Some(hs) = rd.hard_state {
                s.set_hard_state(hs);
            }
            applied.extend(rd.committed.iter().map(|e| e.index));
            reads.extend(rd.read_states);
            n.advance();
        }
        (applied, reads)
    };
    drain(&mut n);
    for _ in 0..30 {
        n.tick();
    }
    drain(&mut n);
    ensure(n.role() == Role::Leader, "a lone voter not leading after its timeout")?;
    let i = n.propose(b"x".to_vec()).map_err(|e| e.to_string())?;
    let (applied, _) = drain(&mut n);
    ensure(applied.contains(&i), "a lone voter's entry not committed")?;
    n.read_index(9).map_err(|e| e.to_string())?;
    let (_, reads) = drain(&mut n);
    ensure(reads.len() == 1 && reads[0].ctx == 9 && reads[0].index >= i, "a lone voter's read not confirmed")
}

/// The 2015 rule and its fellows: no change before the leader's first
/// commit, one change at a time, the leader neither made a learner nor taken
/// out.
fn change_rules() -> Result<(), String> {
    let mut s = crate::sim::Sim::new(crate::sim::Params::plain(), 5, &[1, 2, 3], &[]);
    s.campaign(1);
    s.process_all();
    s.flush_only(10, |m| matches!(m.body, Body::Vote { .. } | Body::VoteResp { .. }));
    ensure(s.node(1).is_some_and(|n| n.role() == Role::Leader), "member 1 not leading")?;
    let add = ConfChange { kind: ChangeKind::AddLearner, node: 4 };
    match s.change(1, add) {
        Err(raft::Error::ChangeRefused(raft::Refusal::NotYetCommittedInTerm)) => {}
        r => return Err(format!("a change before the leader's first commit answered {:?}", r)),
    }
    /* Its appends were dropped with the votes: a heartbeat brings them on. */
    for _ in 0..3 {
        s.tick(1);
        s.flush(50);
    }
    s.change(1, add).map_err(|e| format!("a change after the first commit refused: {}", e))?;
    match s.change(1, ConfChange { kind: ChangeKind::AddLearner, node: 5 }) {
        Err(raft::Error::ChangeRefused(raft::Refusal::ChangePending)) => {}
        r => return Err(format!("a second change before the first committed answered {:?}", r)),
    }
    match s.change(1, ConfChange { kind: ChangeKind::Remove, node: 1 }) {
        Err(raft::Error::ChangeRefused(raft::Refusal::ChangesLeader)) => {}
        Err(raft::Error::ChangeRefused(raft::Refusal::ChangePending)) => {}
        r => return Err(format!("the leader taken out answered {:?}", r)),
    }
    Ok(())
}

/// A member added later, sent the log from its first entry, makes the same
/// membership of it as the first members.
fn bootstrap_membership() -> Result<(), String> {
    let mut s = crate::sim::Sim::new(crate::sim::Params::plain(), 6, &[1, 2, 3], &[4]);
    s.campaign(1);
    s.flush(50);
    let new = s.add_fresh();
    s.change(1, ConfChange { kind: ChangeKind::AddLearner, node: new }).map_err(|e| e.to_string())?;
    s.flush(80);
    let want = ConfState { voters: vec![1, 2, 3], learners: vec![4, new] };
    let got = s.node(new).map(|n| n.membership());
    ensure(got.as_ref() == Some(&want), &format!("the new member's membership is {:?}, not {:?}", got, want))?;
    ensure(s.failures.is_empty(), &s.failures.join("; "))
}

/// Empty entries a long log has, membership changes in it, and ticks a
/// caller lets go by without asking for its `Ready`.
const LONG_EMPTY: u64 = 30_000;
const LONG_CHANGES: u64 = 12_000;
const LONG_TICKS: u64 = 20_000;

/// A log longer than one block of the kernel's heap holds as a list --
/// empty entries by the ten thousand, membership changes by the thousand --
/// applied, started over and applied again from the start, and a leader
/// ticking on with nobody asking for its messages: nothing the crate
/// allocates for any of it larger than a block, which `guarded` holds
/// every test to (`heap`).
fn long_log() -> Result<(), String> {
    let mut cfg = Config::new(1);
    cfg.election_ticks = 2;
    let mut n = RawNode::new(cfg.clone(), MemStorage::bootstrap(&[1], &[])).map_err(|e| e.to_string())?;
    let drain = |n: &mut RawNode<MemStorage>| -> u64 {
        let mut last = 0;
        while n.has_ready() {
            let rd = n.ready();
            let s = n.storage_mut();
            s.append(&rd.entries).expect("entries written");
            if let Some(hs) = rd.hard_state {
                s.set_hard_state(hs);
            }
            if let Some(e) = rd.committed.last() {
                last = e.index;
            }
            n.advance();
        }
        last
    };
    for _ in 0..10 {
        n.tick();
    }
    drain(&mut n);
    ensure(n.role() == Role::Leader, "a lone voter not leading")?;
    for k in 0..LONG_EMPTY {
        n.propose(Vec::new()).map_err(|e| format!("empty entry {}: {}", k, e))?;
        if k % 1000 == 999 {
            drain(&mut n);
        }
    }
    drain(&mut n);
    for k in 0..LONG_CHANGES {
        let kind = if k % 2 == 0 { ChangeKind::AddLearner } else { ChangeKind::Remove };
        n.propose_change(ConfChange { kind, node: 2 }).map_err(|e| format!("change {}: {}", k, e))?;
        drain(&mut n);
    }
    let last = n.last_index();
    ensure(n.committed() == last && n.applied() == last, "a lone voter's log not all applied")?;
    ensure(n.conf_state_at(last / 2).is_some(), "no membership halfway along the log")?;
    /* Started over: every committed entry handed out again, from the first. */
    let mut n = RawNode::new(cfg, n.into_storage()).map_err(|e| e.to_string())?;
    ensure(drain(&mut n) == last, "a lone voter started over not applying its log again")?;
    /* A learner it sends to, and ticks nobody takes the messages of. */
    for _ in 0..10 {
        n.tick();
    }
    drain(&mut n);
    n.propose_change(ConfChange { kind: ChangeKind::AddLearner, node: 2 }).map_err(|e| e.to_string())?;
    drain(&mut n);
    for _ in 0..LONG_TICKS {
        n.tick();
    }
    drain(&mut n);
    ensure(n.role() == Role::Leader, "a lone voter no longer leading")
}

/// Configurations of every value at the edges: each refused, or a group's
/// member over it ticked, elected and asked to write without a panic.
fn config_extremes() -> Result<(), String> {
    let mut r = crate::rng::Rng::new(77);
    let pick32 = |r: &mut crate::rng::Rng| [0, 1, 2, 3, 10, 1 << 24, (1 << 24) + 1, u32::MAX / 2, u32::MAX][r.below(9) as usize];
    let pick64 = |r: &mut crate::rng::Rng| [0, 1, 7, 4096, 4097, 1 << 30, (1 << 30) + 1, u64::MAX][r.below(8) as usize];
    for _ in 0..3000 {
        let mut c = Config::new(1);
        c.election_ticks = pick32(&mut r);
        c.heartbeat_ticks = pick32(&mut r);
        c.max_entries_per_msg = pick64(&mut r) as usize;
        c.max_bytes_per_msg = pick64(&mut r);
        c.max_inflight_msgs = pick64(&mut r) as usize;
        c.max_inflight_bytes = pick64(&mut r);
        c.max_entry_bytes = pick64(&mut r) as usize;
        c.max_uncommitted_entries = pick64(&mut r);
        c.max_apply_bytes = pick64(&mut r);
        c.snapshot_piece_bytes = pick64(&mut r) as usize;
        c.max_snapshot_bytes = pick64(&mut r);
        if c.validate().is_err() {
            continue;
        }
        let mut n = RawNode::new(c, MemStorage::bootstrap(&[1], &[])).map_err(|e| e.to_string())?;
        for _ in 0..64 {
            n.tick();
            while n.has_ready() {
                let rd = n.ready();
                n.storage_mut().append(&rd.entries).expect("written");
                if let Some(hs) = rd.hard_state {
                    n.storage_mut().set_hard_state(hs);
                }
                n.advance();
            }
            let _ = n.propose(vec![1; 16]);
        }
    }
    Ok(())
}
