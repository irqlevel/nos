//! NBS's records (`trace on nbs`, docs/tracing.md): a volume's request as
//! its client sends it -- `nbs_req`, and back, `nbs_ans`, or bounced,
//! `nbs_bounce` -- and as a chunk server takes it and answers it,
//! `nbs_cs_req` and `nbs_cs_done`, each naming it by its session's id;
//! storage's stages (`nbs_cs_step`) and turns (`nbs_turn`) within it.
//!
//! The one place nbswire's requests and answers are said as the kernel's
//! events' values: what a request asks (`NbsOp`, nbswire's `Op`) and what an
//! answer says (`NbsStatus`, done or nbswire's `Refusal`) -- each `match`
//! whole, so that a kind nbswire gains is a build that fails here until it
//! has its name, in this list and the kernel's (kernel/tracepoint.cpp), which
//! every boot holds to each other. Each site records only while its event
//! is on: a request's bytes are read again for its record then, and the
//! chunk server's clock read for its time. Off, a site is the flag's load and
//! a branch where it is called: each test `#[inline]`, what a record takes
//! out of line, `#[cold]`.

#![no_std]
#![forbid(unsafe_code)]

use kcore::ipc::Bounce;
use kcore::time::boot_time_ns;
use kcore::trace::{self, BounceWhy, NbsAns, NbsBounce, NbsCsDone, NbsCsReq, NbsCsStep,
                   NbsCsStepStage as KernelStepStage, NbsOp, NbsReq, NbsStatus, NbsTurn,
                   NbsTurnStage as KernelTurnStage};
use nbscs::{NbsCsStepStage, NbsTurnStage};
use nbswire::{Answer, Op, Refusal, Reply, Request};

/// A count in a record's 32 bits: what is past them said as their most.
fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn elapsed(since: u64) -> u32 {
    let ns = if since == 0 { 0 } else { boot_time_ns().saturating_sub(since) };
    u32::try_from(ns).unwrap_or(u32::MAX)
}

/// What a request asks, of which chunk, where in it and how many bytes --
/// a write's, a read's, a pull's or a sum's; a list's first chunk number.
fn op_of(op: &Op<'_>) -> (NbsOp, u32, u32, u32) {
    match op {
        Op::Fence { .. } => (NbsOp::Fence, 0, 0, 0),
        Op::Write { chunk, offset, data, .. } => (NbsOp::Write, chunk.chunk, *offset, u32_of(data.len())),
        Op::Read { chunk, offset, len, .. } => (NbsOp::Read, chunk.chunk, *offset, *len),
        Op::Flush => (NbsOp::Flush, 0, 0, 0),
        Op::Close { chunk, .. } => (NbsOp::Close, chunk.chunk, 0, 0),
        Op::Held { from } => (NbsOp::Held, *from, 0, 0),
        Op::Ping => (NbsOp::Ping, 0, 0, 0),
        Op::Holdings { from, .. } => (NbsOp::Holdings, *from, 0, 0),
        Op::Pull { chunk, offset, len, .. } => (NbsOp::Pull, chunk.chunk, *offset, *len),
        Op::Sums { chunk, offset, len, .. } => (NbsOp::Sums, chunk.chunk, *offset, *len),
        Op::Scrub { chunk } => (NbsOp::Scrub, chunk.chunk, 0, 0),
        /* The chunk a copy-up makes */
        Op::CopyUp { to, .. } => (NbsOp::CopyUp, to.chunk, 0, 0),
    }
}

/// What an answer says: done, whatever it carries, or why refused.
fn status_of(reply: &Reply<'_>) -> NbsStatus {
    match reply {
        Reply::Refused(why) => refusal(*why),
        _ => NbsStatus::Ok,
    }
}

/// A refusal, by its kind.
fn refusal(why: Refusal) -> NbsStatus {
    match why {
        Refusal::Fenced { .. } => NbsStatus::Fenced,
        Refusal::NotFenced { .. } => NbsStatus::NotFenced,
        Refusal::StaleChunk { .. } => NbsStatus::StaleChunk,
        Refusal::Epoch { .. } => NbsStatus::Epoch,
        Refusal::NoRoom => NbsStatus::NoRoom,
        Refusal::Bad => NbsStatus::Bad,
        Refusal::Failed => NbsStatus::Failed,
        Refusal::Io => NbsStatus::Io,
        Refusal::TooManyFences => NbsStatus::TooManyFences,
        Refusal::NotTheMap => NbsStatus::NotTheMap,
        Refusal::NotHere => NbsStatus::NotHere,
        Refusal::NotAttached => NbsStatus::NotAttached,
        Refusal::Behind { .. } => NbsStatus::Behind,
        Refusal::NoMds => NbsStatus::NoMds,
        Refusal::NotReady => NbsStatus::NotReady,
        Refusal::NotHeld => NbsStatus::NotHeld,
        Refusal::NotTheJoiner => NbsStatus::NotTheJoiner,
    }
}

/// Why ipc gave a message back, as `nbs_bounce` says it.
#[inline]
pub fn bounce_why(why: Bounce) -> BounceWhy {
    match why {
        Bounce::Full => BounceWhy::Full,
        Bounce::Gone => BounceWhy::Gone,
        Bounce::Unsent => BounceWhy::Unsent,
    }
}

/// `nbs_req`: a request sent to chunk server `cs`, its bytes as they go.
#[inline]
pub fn sent(cs: u32, bytes: &[u8]) {
    if trace::on::<NbsReq>() {
        record_sent(cs, bytes);
    }
}

#[cold]
#[inline(never)]
fn record_sent(cs: u32, bytes: &[u8]) {
    let Some((req, _)) = Request::decode_relayed(bytes) else { return };
    let (op, chunk, offset, len) = op_of(&req.op);
    trace::emit(&NbsReq {
        id: req.id,
        volume: req.volume,
        chunk,
        offset,
        len,
        pid: trace::pid(),
        cs: u16::try_from(cs).unwrap_or(u16::MAX),
        op: op as u8,
        _pad: 0,
    });
}

/// `nbs_ans`: an answer back from host `from` -- 0, this one -- its bytes
/// as they came.
#[inline]
pub fn answered(from: u16, bytes: &[u8]) {
    if trace::on::<NbsAns>() {
        record_answered(from, bytes);
    }
}

#[cold]
#[inline(never)]
fn record_answered(from: u16, bytes: &[u8]) {
    let Some(a) = Answer::decode(bytes) else { return };
    trace::emit(&NbsAns {
        id: a.id,
        bytes: u32_of(bytes.len()),
        pid: trace::pid(),
        from,
        status: status_of(&a.reply) as u8,
        _pad: [0; 5],
    });
}

/// `nbs_bounce`: request `id` that never reached its chunk server.
#[inline]
pub fn bounced(id: u64, why: BounceWhy) {
    if trace::on::<NbsBounce>() {
        record_bounced(id, why);
    }
}

#[cold]
#[inline(never)]
fn record_bounced(id: u64, why: BounceWhy) {
    trace::emit(&NbsBounce { id, pid: trace::pid(), why: why as u8, _pad: [0; 3] });
}

/// When a chunk server took a request, for its `nbs_cs_done`'s time: the
/// clock read only while that event is on.
#[inline]
pub fn taking() -> u64 {
    if trace::on::<NbsCsDone>() {
        boot_time_ns()
    } else {
        0
    }
}

/// `nbs_cs_req`: request `req` a chunk server took from host `from`.
#[inline]
pub fn taken(from: u16, req: &Request<'_>) {
    if trace::on::<NbsCsReq>() {
        record_taken(from, req);
    }
}

#[cold]
#[inline(never)]
fn record_taken(from: u16, req: &Request<'_>) {
    let (op, chunk, offset, len) = op_of(&req.op);
    trace::emit(&NbsCsReq {
        id: req.id,
        volume: req.volume,
        chunk,
        offset,
        len,
        pid: trace::pid(),
        from,
        op: op as u8,
        _pad: 0,
    });
}

/// `nbs_cs_done`: the chunk server's answer `reply` to request `req` of
/// host `from`, taken at `since` (`taking`).
#[inline]
pub fn done(from: u16, req: &Request<'_>, reply: &Reply<'_>, since: u64) {
    if trace::on::<NbsCsDone>() {
        record_done(from, req, reply, since);
    }
}

#[cold]
#[inline(never)]
fn record_done(from: u16, req: &Request<'_>, reply: &Reply<'_>, since: u64) {
    let (op, ..) = op_of(&req.op);
    /* Taken before the event was on: no time to say */
    trace::emit(&NbsCsDone {
        id: req.id,
        ns: elapsed(since),
        pid: trace::pid(),
        from,
        op: op as u8,
        status: status_of(reply) as u8,
        _pad: [0; 4],
    });
}

/// The chunk server engine's operations, as the kernel's events.
pub struct Tracer;

impl nbscs::Trace for Tracer {
    fn now(&self) -> u64 {
        boot_time_ns()
    }

    fn step_enabled(&self) -> bool {
        trace::on::<NbsCsStep>()
    }

    #[cold]
    fn step(&self, volume: u32, chunk: u32, offset: u32, len: u32, stage: NbsCsStepStage, since: u64) {
        let stage = match stage {
            NbsCsStepStage::ZeroStart => KernelStepStage::ZeroStart,
            NbsCsStepStage::ZeroDone => KernelStepStage::ZeroDone,
            NbsCsStepStage::PutStart => KernelStepStage::PutStart,
            NbsCsStepStage::PutDone => KernelStepStage::PutDone,
            NbsCsStepStage::WriteStart => KernelStepStage::WriteStart,
            NbsCsStepStage::WriteDone => KernelStepStage::WriteDone,
            NbsCsStepStage::FlushStart => KernelStepStage::FlushStart,
            NbsCsStepStage::FlushDone => KernelStepStage::FlushDone,
            NbsCsStepStage::CommitStart => KernelStepStage::CommitStart,
            NbsCsStepStage::CommitDone => KernelStepStage::CommitDone,
        };
        trace::emit(&NbsCsStep { volume, chunk, offset, len, ns: elapsed(since), pid: trace::pid(), stage: stage as u8,
                                 _pad: [0; 3] });
    }

    fn turn_enabled(&self) -> bool {
        trace::on::<NbsTurn>()
    }

    #[cold]
    fn turn(&self, id: u64, key: u32, alone: bool, stage: NbsTurnStage, since: u64) {
        let stage = match stage {
            NbsTurnStage::Wait => KernelTurnStage::Wait,
            NbsTurnStage::Acquire => KernelTurnStage::Acquire,
            NbsTurnStage::Release => KernelTurnStage::Release,
        };
        trace::emit(&NbsTurn { id, key, alone: u8::from(alone), stage: stage as u8, ns: elapsed(since),
                               pid: trace::pid(), _pad: [0; 2] });
    }
}
