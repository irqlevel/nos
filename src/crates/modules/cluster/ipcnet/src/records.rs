//! ipcnet's records (`trace on ipcnet`, docs/tracing.md): a peer's session
//! up or lost (`ipcnet_link`), a message queued for a peer (`ipcnet_queue`)
//! and one bounced back to its sender (`ipcnet_bounce`) -- a message by its
//! reference, which an NBS request's is its session's id, as `nbs_req` says
//! it. Each records only while its event is on: off, a site is the flag's
//! load and a branch where it is called -- each test `#[inline]`, what a
//! record takes out of line, `#[cold]`.

use kcore::trace::{self, IpcnetBounce, IpcnetLink, IpcnetQueue, IpcnetWhy};

fn u32_of(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `ipcnet_link`: peer `peer`'s session up, or lost, and its sessions up and
/// lost so far.
#[inline]
pub fn link(peer: u16, up: bool, ups: u64, losses: u64) {
    if trace::on::<IpcnetLink>() {
        record_link(peer, up, ups, losses);
    }
}

#[cold]
#[inline(never)]
fn record_link(peer: u16, up: bool, ups: u64, losses: u64) {
    trace::emit(&IpcnetLink {
        ups: u32_of(ups),
        losses: u32_of(losses),
        pid: trace::pid(),
        peer,
        up: u8::from(up),
        _pad: 0,
    });
}

/// `ipcnet_queue`: a message of `bytes` queued for peer `peer` -- in its
/// bulk class's queue or its control one's -- and that queue's bytes with it.
#[inline]
pub fn queued(peer: u16, bulk: bool, reference: u64, bytes: usize, queue: usize) {
    if trace::on::<IpcnetQueue>() {
        record_queued(peer, bulk, reference, bytes, queue);
    }
}

#[cold]
#[inline(never)]
fn record_queued(peer: u16, bulk: bool, reference: u64, bytes: usize, queue: usize) {
    trace::emit(&IpcnetQueue {
        reference,
        bytes: u32_of(bytes as u64),
        queued: u32_of(queue as u64),
        pid: trace::pid(),
        peer,
        bulk: u8::from(bulk),
        _pad: 0,
    });
}

/// `ipcnet_bounce`: a message for peer `peer` bounced to its sender, for
/// `why`, its queue's bytes then.
#[inline]
pub fn bounced(peer: u16, bulk: bool, reference: u64, bytes: usize, queue: usize, why: IpcnetWhy) {
    if trace::on::<IpcnetBounce>() {
        record_bounced(peer, bulk, reference, bytes, queue, why);
    }
}

#[cold]
#[inline(never)]
fn record_bounced(peer: u16, bulk: bool, reference: u64, bytes: usize, queue: usize, why: IpcnetWhy) {
    trace::emit(&IpcnetBounce {
        reference,
        bytes: u32_of(bytes as u64),
        queued: u32_of(queue as u64),
        pid: trace::pid(),
        peer,
        bulk: u8::from(bulk),
        why: why as u8,
    });
}
