//! The wire fuzzed: messages made at random written and read back as they
//! were; noise, and what was written then broken -- a byte changed, cut
//! short, grown -- read without a panic, and whatever is taken written back
//! to the very bytes it was read from.

use raft::wire;
use raft::{Body, ChangeKind, ConfChange, ConfState, Entry, EntryKind, Message, SnapshotMeta};

use crate::rng::Rng;

fn ids(r: &mut Rng, n: u64) -> Vec<u64> {
    let mut v: Vec<u64> = (0..n).map(|_| r.range(1, 40)).collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn value(r: &mut Rng) -> u64 {
    match r.below(4) {
        0 => 0,
        1 => r.below(100),
        2 => u64::MAX - r.below(3),
        _ => r.next(),
    }
}

fn entry(r: &mut Rng, index: u64) -> Entry {
    let kind = match r.below(3) {
        0 => EntryKind::Normal,
        1 => EntryKind::Noop,
        _ => EntryKind::Change,
    };
    let data = match kind {
        EntryKind::Change => wire::encode_change(&ConfChange {
            kind: match r.below(3) {
                0 => ChangeKind::AddVoter,
                1 => ChangeKind::AddLearner,
                _ => ChangeKind::Remove,
            },
            node: r.range(1, 99),
        }),
        _ => (0..r.below(64)).map(|_| r.next() as u8).collect(),
    };
    Entry { index, term: value(r), kind, data }
}

pub fn message(r: &mut Rng) -> Message {
    let body = match r.below(11) {
        0 => Body::Vote { pre: r.chance(50), transfer: r.chance(50), last_index: value(r), last_term: value(r) },
        1 => Body::VoteResp { pre: r.chance(50), granted: r.chance(50) },
        2 => {
            let prev = value(r) / 2;
            let n = r.below(6);
            Body::Append {
                prev_index: prev,
                prev_term: value(r),
                commit: value(r),
                entries: (0..n).map(|k| entry(r, prev.wrapping_add(k + 1))).collect(),
            }
        }
        3 => Body::AppendResp { reject: r.chance(50), index: value(r), hint_index: value(r), hint_term: value(r) },
        4 => Body::Heartbeat { commit: value(r), ctx: value(r) },
        5 => Body::HeartbeatResp { ctx: value(r) },
        6 => {
            let nv = r.below(6);
            let mut voters = ids(r, nv);
            let nl = r.below(3);
            let learners: Vec<u64> = ids(r, nl).into_iter().filter(|l| !voters.contains(l)).collect();
            voters.retain(|v| !learners.contains(v));
            let data: Vec<u8> = (0..r.below(300)).map(|_| r.next() as u8).collect();
            let offset = r.below(1000);
            let total = offset + data.len() as u64 + r.below(1000);
            Body::Snapshot { meta: SnapshotMeta { index: value(r), term: value(r), conf: ConfState { voters, learners } }, offset, total, data }
        }
        7 => Body::SnapshotResp { index: value(r), next: value(r), reject: r.chance(50) },
        8 => Body::TimeoutNow,
        9 => Body::ReadIndex { ctx: value(r) },
        _ => Body::ReadIndexResp { ctx: value(r), index: value(r), ok: r.chance(50) },
    };
    Message { from: r.range(1, u64::MAX - 1), to: r.range(1, 1000), term: value(r), body }
}

/// Bytes read: no panic, and if taken, written back to the same bytes.
fn read_back(bytes: &[u8]) -> Result<(), String> {
    if let Ok(m) = wire::decode(bytes) {
        let mut again = Vec::new();
        wire::encode(&m, &mut again).map_err(|e| format!("{:?} taken but not written: {:?}", m, e))?;
        if again != bytes {
            return Err(format!("{:02x?} taken as {:?} and written back as {:02x?}", bytes, m, again));
        }
    }
    let _ = wire::decode_change(bytes);
    let _ = wire::decode_entry(bytes);
    let _ = wire::decode_hard_state(bytes);
    let _ = wire::decode_conf_state(bytes);
    Ok(())
}

/// An append about as large as a message is -- just under, or just past --
/// in entries of odd sizes: written into an empty buffer, it must take no
/// block past the kernel heap's (held by `guarded`), and be refused past
/// the limit.
fn near_limit(r: &mut Rng) -> Result<(), String> {
    let n = r.range(2, 200);
    let fixed = wire::HEADER_BYTES + 28 + n as usize * 21;
    let target = (wire::MAX_MESSAGE_BYTES - fixed) as u64 + r.below(64) - 32;
    let mut left = target;
    let mut entries = Vec::new();
    for k in 0..n {
        let most = left.min(wire::MAX_ENTRY_BYTES as u64);
        let size = if k + 1 == n { most } else { r.below(most.min(2 * target / n) + 1) };
        left -= size;
        entries.push(Entry { term: 1, index: 1 + k, kind: EntryKind::Normal, data: vec![k as u8; size as usize] });
    }
    let m = Message { from: 1, to: 2, term: 1, body: Body::Append { prev_index: 0, prev_term: 0, commit: 0, entries } };
    let len = wire::encoded_len(&m);
    let mut b = Vec::new();
    match wire::encode(&m, &mut b) {
        Ok(()) if len <= wire::MAX_MESSAGE_BYTES && b.len() == len => {}
        Err(_) if len > wire::MAX_MESSAGE_BYTES && b.is_empty() => {}
        other => return Err(format!("an append of {} bytes: {:?}, {} written", len, other, b.len())),
    }
    Ok(())
}

/// `iterations` rounds from `seed`.
pub fn run(seed: u64, iterations: u64) -> Result<(), String> {
    let mut r = Rng::new(seed);
    for i in 0..iterations {
        if i % 50 == 0 {
            near_limit(&mut r).map_err(|e| format!("iteration {}: {}", i, e))?;
        }
        let m = message(&mut r);
        let mut b = Vec::new();
        wire::encode(&m, &mut b).map_err(|e| format!("iteration {}: {:?} not written: {:?}", i, m, e))?;
        if wire::encoded_len(&m) != b.len() {
            return Err(format!("iteration {}: {:?} measured {} and written in {}", i, m, wire::encoded_len(&m), b.len()));
        }
        match wire::decode(&b) {
            Ok(back) if back == m => {}
            other => return Err(format!("iteration {}: {:?} read back as {:?}", i, m, other)),
        }
        /* Broken: a byte changed, a cut, a byte more -- or noise. */
        let mut broken = b.clone();
        match r.below(4) {
            0 if !broken.is_empty() => {
                let at = r.below(broken.len() as u64) as usize;
                broken[at] ^= 1 << r.below(8);
            }
            1 => broken.truncate(r.below(broken.len() as u64 + 1) as usize),
            2 => broken.push(r.next() as u8),
            _ => broken = (0..r.below(80)).map(|_| r.next() as u8).collect(),
        }
        read_back(&broken).map_err(|e| format!("iteration {}: {}", i, e))?;
        if r.chance(10) {
            /* Noise that starts like a message. */
            let mut n = vec![b'R', b'F', wire::VERSION, r.range(0, 12) as u8];
            n.extend((0..r.below(120)).map(|_| r.next() as u8));
            read_back(&n).map_err(|e| format!("iteration {}: {}", i, e))?;
        }
    }
    Ok(())
}
