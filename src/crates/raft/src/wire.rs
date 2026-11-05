//! Messages as bytes, and back.
//!
//! A message is a header -- `RF`, the version, its kind, the sender, the
//! receiver, the term -- and its body, every integer little-endian and fixed
//! in width. What comes from the network is the network's to choose, so a
//! decode checks everything: every length against what is left and against
//! its limit before anything is allocated for it, every flag and kind known,
//! a membership sorted and without repeats, nothing after the end. What it
//! cannot take it refuses with an error, for the caller to drop and count;
//! it never panics. What it takes, `encode` writes back to the same bytes.

use alloc::vec::Vec;

use crate::conf;
use crate::types::{
    Body, ChangeKind, ConfChange, ConfState, Entry, EntryKind, HardState, Message, NodeId, SnapshotMeta, NONE,
    PIECE_MAX,
};

const MAGIC: [u8; 2] = *b"RF";
pub const VERSION: u8 = 1;
/// The bytes of the header: magic, version, kind, from, to, term.
pub const HEADER_BYTES: usize = 2 + 1 + 1 + 8 + 8 + 8;
/// The largest message there is, in bytes: one block of the kernel's heap,
/// what a transport reads a message into.
pub const MAX_MESSAGE_BYTES: usize = 512 * 1024;
/// The most entries one append carries.
pub const MAX_ENTRIES: usize = 4096;
/// The largest entry's data.
pub const MAX_ENTRY_BYTES: usize = 256 * 1024;
/// The most bytes of entries' data one append carries: what a member's
/// appends are held to (`Config::max_bytes_per_msg`, and one entry past it
/// at most of [`MAX_ENTRY_BYTES`]), so that every append it sends fits a
/// message whatever its count of entries.
pub const MAX_APPEND_DATA: usize = MAX_ENTRY_BYTES;
/// An append's own fields: prev index, prev term, commit, the count.
const APPEND_BYTES: usize = 8 + 8 + 8 + 4;
const _: () = assert!(HEADER_BYTES + APPEND_BYTES + MAX_ENTRIES * ENTRY_HEADER_BYTES + MAX_APPEND_DATA <= MAX_MESSAGE_BYTES);

const KIND_VOTE: u8 = 1;
const KIND_VOTE_RESP: u8 = 2;
const KIND_APPEND: u8 = 3;
const KIND_APPEND_RESP: u8 = 4;
const KIND_HEARTBEAT: u8 = 5;
const KIND_HEARTBEAT_RESP: u8 = 6;
const KIND_SNAPSHOT: u8 = 7;
const KIND_SNAPSHOT_RESP: u8 = 8;
const KIND_TIMEOUT_NOW: u8 = 9;
const KIND_READ_INDEX: u8 = 10;
const KIND_READ_INDEX_RESP: u8 = 11;

/* A body's flags, a bit each. */
const FLAG_PRE: u8 = 1 << 0;
const FLAG_TRANSFER: u8 = 1 << 1;
const FLAG_GRANTED: u8 = 1 << 1;
const FLAG_REJECT: u8 = 1 << 0;
const FLAG_OK: u8 = 1 << 0;

/// Why bytes are not a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than the message says it has.
    Short,
    /// Bytes after the message's end.
    Trailing,
    BadMagic,
    BadVersion,
    BadKind,
    BadFlags,
    /// Past a limit: a message, an entry, a count.
    TooLarge,
    /// What a message cannot say: a member 0, a membership out of order, a
    /// piece past its snapshot's end.
    Invalid,
}

/// Why a message is not written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    TooLarge,
}

/* ---- writing ---- */

fn put8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put32(out, b.len() as u32);
    out.extend_from_slice(b);
}

fn put_ids(out: &mut Vec<u8>, ids: &[NodeId]) {
    put16(out, ids.len() as u16);
    for id in ids {
        put64(out, *id);
    }
}

fn put_conf(out: &mut Vec<u8>, c: &ConfState) {
    put_ids(out, &c.voters);
    put_ids(out, &c.learners);
}

fn put_entry(out: &mut Vec<u8>, e: &Entry) {
    put64(out, e.index);
    put64(out, e.term);
    put8(out, e.kind as u8);
    put_bytes(out, &e.data);
}

fn flag(on: bool, bit: u8) -> u8 {
    if on { bit } else { 0 }
}

/// The bytes `encode` writes for `m`, whatever they come to.
pub fn encoded_len(m: &Message) -> usize {
    let body = match &m.body {
        Body::Vote { .. } => 1 + 8 + 8,
        Body::VoteResp { .. } => 1,
        Body::Append { entries, .. } => entries
            .iter()
            .fold(APPEND_BYTES, |sum, e| sum.saturating_add(ENTRY_HEADER_BYTES).saturating_add(e.data.len())),
        Body::AppendResp { .. } => 1 + 8 + 8 + 8,
        Body::Heartbeat { .. } => 8 + 8,
        Body::HeartbeatResp { .. } => 8,
        Body::Snapshot { meta, data, .. } => {
            let ids = meta.conf.voters.len().saturating_add(meta.conf.learners.len());
            (8 + 8 + 2 + 2 + 8 + 8 + 4usize).saturating_add(ids.saturating_mul(8)).saturating_add(data.len())
        }
        Body::SnapshotResp { .. } => 1 + 8 + 8,
        Body::TimeoutNow => 0,
        Body::ReadIndex { .. } => 8,
        Body::ReadIndexResp { .. } => 1 + 8 + 8,
    };
    HEADER_BYTES.saturating_add(body)
}

/// `m` as bytes, appended to `out`.
pub fn encode(m: &Message, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    /* Measured first: refused before a byte is written, and its bytes
     * reserved at once -- a buffer grown as it is written doubles, and
     * from an odd size past the one block of the kernel's heap that holds
     * a message well within the limit. */
    let len = encoded_len(m);
    if len > MAX_MESSAGE_BYTES {
        return Err(EncodeError::TooLarge);
    }
    out.reserve_exact(len);
    let start = out.len();
    out.extend_from_slice(&MAGIC);
    put8(out, VERSION);
    let kind_at = out.len();
    put8(out, 0);
    put64(out, m.from);
    put64(out, m.to);
    put64(out, m.term);
    let kind = match &m.body {
        Body::Vote { pre, transfer, last_index, last_term } => {
            put8(out, flag(*pre, FLAG_PRE) | flag(*transfer, FLAG_TRANSFER));
            put64(out, *last_index);
            put64(out, *last_term);
            KIND_VOTE
        }
        Body::VoteResp { pre, granted } => {
            put8(out, flag(*pre, FLAG_PRE) | flag(*granted, FLAG_GRANTED));
            KIND_VOTE_RESP
        }
        Body::Append { prev_index, prev_term, commit, entries } => {
            if entries.len() > MAX_ENTRIES || entries.iter().any(|e| e.data.len() > MAX_ENTRY_BYTES) {
                out.truncate(start);
                return Err(EncodeError::TooLarge);
            }
            put64(out, *prev_index);
            put64(out, *prev_term);
            put64(out, *commit);
            put32(out, entries.len() as u32);
            for e in entries {
                put_entry(out, e);
            }
            KIND_APPEND
        }
        Body::AppendResp { reject, index, hint_index, hint_term } => {
            put8(out, flag(*reject, FLAG_REJECT));
            put64(out, *index);
            put64(out, *hint_index);
            put64(out, *hint_term);
            KIND_APPEND_RESP
        }
        Body::Heartbeat { commit, ctx } => {
            put64(out, *commit);
            put64(out, *ctx);
            KIND_HEARTBEAT
        }
        Body::HeartbeatResp { ctx } => {
            put64(out, *ctx);
            KIND_HEARTBEAT_RESP
        }
        Body::Snapshot { meta, offset, total, data } => {
            if data.len() > PIECE_MAX || meta.conf.voters.len() + meta.conf.learners.len() > conf::MAX_MEMBERS {
                out.truncate(start);
                return Err(EncodeError::TooLarge);
            }
            put64(out, meta.index);
            put64(out, meta.term);
            put_conf(out, &meta.conf);
            put64(out, *offset);
            put64(out, *total);
            put_bytes(out, data);
            KIND_SNAPSHOT
        }
        Body::SnapshotResp { index, next, reject } => {
            put8(out, flag(*reject, FLAG_REJECT));
            put64(out, *index);
            put64(out, *next);
            KIND_SNAPSHOT_RESP
        }
        Body::TimeoutNow => KIND_TIMEOUT_NOW,
        Body::ReadIndex { ctx } => {
            put64(out, *ctx);
            KIND_READ_INDEX
        }
        Body::ReadIndexResp { ctx, index, ok } => {
            put8(out, flag(*ok, FLAG_OK));
            put64(out, *ctx);
            put64(out, *index);
            KIND_READ_INDEX_RESP
        }
    };
    out[kind_at] = kind;
    if out.len() - start > MAX_MESSAGE_BYTES {
        out.truncate(start);
        return Err(EncodeError::TooLarge);
    }
    Ok(())
}

/// A membership change as an entry's data carries it.
pub fn encode_change(cc: &ConfChange) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    put8(&mut out, cc.kind as u8);
    put64(&mut out, cc.node);
    out
}

/// An entry as a log record's payload (and an append) carries it.
pub fn encode_entry(e: &Entry, out: &mut Vec<u8>) {
    put_entry(out, e);
}

pub fn encode_hard_state(hs: &HardState, out: &mut Vec<u8>) {
    put64(out, hs.term);
    put64(out, hs.vote);
    put64(out, hs.commit);
}

pub fn encode_conf_state(c: &ConfState, out: &mut Vec<u8>) {
    put_conf(out, c);
}

/* ---- reading ---- */

struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.b.len() < n {
            return Err(DecodeError::Short);
        }
        let (head, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    /// A length, then that many bytes -- the length held to `max` and to
    /// what is left before anything is allocated.
    fn bytes(&mut self, max: usize) -> Result<Vec<u8>, DecodeError> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(DecodeError::TooLarge);
        }
        Ok(self.take(n)?.to_vec())
    }

    fn flags(&mut self, known: u8) -> Result<u8, DecodeError> {
        let f = self.u8()?;
        if f & !known != 0 {
            return Err(DecodeError::BadFlags);
        }
        Ok(f)
    }

    fn ids(&mut self) -> Result<Vec<NodeId>, DecodeError> {
        let n = usize::from(self.u16()?);
        if n > conf::MAX_MEMBERS {
            return Err(DecodeError::TooLarge);
        }
        if self.b.len() < n * 8 {
            return Err(DecodeError::Short);
        }
        let mut ids = Vec::with_capacity(n);
        for _ in 0..n {
            ids.push(self.u64()?);
        }
        Ok(ids)
    }

    fn conf(&mut self) -> Result<ConfState, DecodeError> {
        let voters = self.ids()?;
        let learners = self.ids()?;
        let c = ConfState { voters, learners };
        if !conf::valid(&c) {
            return Err(DecodeError::Invalid);
        }
        Ok(c)
    }

    fn entry(&mut self) -> Result<Entry, DecodeError> {
        let index = self.u64()?;
        let term = self.u64()?;
        let kind = match self.u8()? {
            0 => EntryKind::Normal,
            1 => EntryKind::Change,
            2 => EntryKind::Noop,
            _ => return Err(DecodeError::BadKind),
        };
        let data = self.bytes(MAX_ENTRY_BYTES)?;
        Ok(Entry { term, index, kind, data })
    }

    fn end(&self) -> Result<(), DecodeError> {
        if self.b.is_empty() { Ok(()) } else { Err(DecodeError::Trailing) }
    }
}

/// The message `bytes` are, all of them.
pub fn decode(bytes: &[u8]) -> Result<Message, DecodeError> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(DecodeError::TooLarge);
    }
    let mut r = Reader { b: bytes };
    if r.take(2)? != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    if r.u8()? != VERSION {
        return Err(DecodeError::BadVersion);
    }
    let kind = r.u8()?;
    let from = r.u64()?;
    let to = r.u64()?;
    let term = r.u64()?;
    if from == NONE || to == NONE {
        return Err(DecodeError::Invalid);
    }
    let body = match kind {
        KIND_VOTE => {
            let f = r.flags(FLAG_PRE | FLAG_TRANSFER)?;
            Body::Vote { pre: f & FLAG_PRE != 0, transfer: f & FLAG_TRANSFER != 0, last_index: r.u64()?, last_term: r.u64()? }
        }
        KIND_VOTE_RESP => {
            let f = r.flags(FLAG_PRE | FLAG_GRANTED)?;
            Body::VoteResp { pre: f & FLAG_PRE != 0, granted: f & FLAG_GRANTED != 0 }
        }
        KIND_APPEND => {
            let prev_index = r.u64()?;
            let prev_term = r.u64()?;
            let commit = r.u64()?;
            let n = r.u32()? as usize;
            if n > MAX_ENTRIES {
                return Err(DecodeError::TooLarge);
            }
            /* An entry is at least its 21 bytes of header: a count the
             * bytes left cannot hold is refused before anything is made. */
            if r.b.len() / ENTRY_HEADER_BYTES < n {
                return Err(DecodeError::Short);
            }
            let mut entries = Vec::with_capacity(n);
            for _ in 0..n {
                entries.push(r.entry()?);
            }
            Body::Append { prev_index, prev_term, commit, entries }
        }
        KIND_APPEND_RESP => {
            let f = r.flags(FLAG_REJECT)?;
            Body::AppendResp { reject: f & FLAG_REJECT != 0, index: r.u64()?, hint_index: r.u64()?, hint_term: r.u64()? }
        }
        KIND_HEARTBEAT => Body::Heartbeat { commit: r.u64()?, ctx: r.u64()? },
        KIND_HEARTBEAT_RESP => Body::HeartbeatResp { ctx: r.u64()? },
        KIND_SNAPSHOT => {
            let index = r.u64()?;
            let term = r.u64()?;
            let conf = r.conf()?;
            let offset = r.u64()?;
            let total = r.u64()?;
            let data = r.bytes(PIECE_MAX)?;
            if offset.checked_add(data.len() as u64).is_none_or(|end| end > total) {
                return Err(DecodeError::Invalid);
            }
            Body::Snapshot { meta: SnapshotMeta { index, term, conf }, offset, total, data }
        }
        KIND_SNAPSHOT_RESP => {
            let f = r.flags(FLAG_REJECT)?;
            Body::SnapshotResp { reject: f & FLAG_REJECT != 0, index: r.u64()?, next: r.u64()? }
        }
        KIND_TIMEOUT_NOW => Body::TimeoutNow,
        KIND_READ_INDEX => Body::ReadIndex { ctx: r.u64()? },
        KIND_READ_INDEX_RESP => {
            let f = r.flags(FLAG_OK)?;
            Body::ReadIndexResp { ok: f & FLAG_OK != 0, ctx: r.u64()?, index: r.u64()? }
        }
        _ => return Err(DecodeError::BadKind),
    };
    r.end()?;
    Ok(Message { from, to, term, body })
}

/// An entry's header: index, term, kind and the data's length.
const ENTRY_HEADER_BYTES: usize = 8 + 8 + 1 + 4;

/// The membership change an entry's data is.
pub fn decode_change(bytes: &[u8]) -> Result<ConfChange, DecodeError> {
    let mut r = Reader { b: bytes };
    let kind = match r.u8()? {
        0 => ChangeKind::AddVoter,
        1 => ChangeKind::AddLearner,
        2 => ChangeKind::Remove,
        _ => return Err(DecodeError::BadKind),
    };
    let node = r.u64()?;
    r.end()?;
    if node == NONE {
        return Err(DecodeError::Invalid);
    }
    Ok(ConfChange { kind, node })
}

/// An entry as `encode_entry` wrote it, all of `bytes`.
pub fn decode_entry(bytes: &[u8]) -> Result<Entry, DecodeError> {
    let mut r = Reader { b: bytes };
    let e = r.entry()?;
    r.end()?;
    Ok(e)
}

pub fn decode_hard_state(bytes: &[u8]) -> Result<HardState, DecodeError> {
    let mut r = Reader { b: bytes };
    let hs = HardState { term: r.u64()?, vote: r.u64()?, commit: r.u64()? };
    r.end()?;
    Ok(hs)
}

pub fn decode_conf_state(bytes: &[u8]) -> Result<ConfState, DecodeError> {
    let mut r = Reader { b: bytes };
    let c = r.conf()?;
    r.end()?;
    Ok(c)
}
