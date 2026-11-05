//! The router: the mailbox the channel hands everything for another host
//! (docs/ipc.md, "Between hosts: the router"), and the task that takes it.
//!
//! A message for a peer -- `TAG_ROUTE`, its payload the route head and then
//! the message's own bytes -- goes into the peer's queue of its class, bulk
//! for a tag with TAG_BULK and control for any other, and is bounced back
//! to its sender (`Unsent`) when the peer is not up any more or its queue
//! is full: what waits to go to a host has its cap, and a sender hears when
//! it was met. A monitor of a mailbox there is asked of the peer -- a
//! `Monitor` frame -- and a Down of a monitor this host's router holds for
//! a peer goes back to it as a `Down` frame. Received in pieces, so that a
//! message of a megabyte costs no block of the heap larger than a piece.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use ipcwire::{route, Dest, Frame, To};
use kcore::ipc::{Addr, Bounce, Pieces, Reason, RemoteTo, PIECE_MAX, TAG_BULK, TAG_DOWN, TAG_ROUTE};
use kcore::time::Duration;

use super::link::{byte, push_frame};
use super::peer::{add, OutMsg, Peer, BULK, CONTROL, QUEUE_BYTES};
use super::server::Shared;

/// How long a receive waits at a time: how soon a stop is seen when the
/// wake is missed.
const WAIT_MS: u64 = 250;

/// The router's task.
pub fn router_main(shared: Arc<Shared>) {
    while !shared.stop.load(Ordering::Acquire) {
        let Some(m) = shared.router.recv_pieces(Duration::from_millis(WAIT_MS), PIECE_MAX) else { continue };
        match m.tag {
            TAG_ROUTE => route(&shared, m),
            TAG_DOWN => down(&shared, &m),
            _ => add(&shared.counters.stray, 1),
        }
    }
}

/// The peer whose number `number` is now.
fn peer_by_number(shared: &Shared, number: u16) -> Option<Arc<Peer>> {
    if number == 0 {
        return None;
    }
    shared.peers.with(|v| v.iter().find(|p| p.number_now.load(Ordering::Acquire) == number).cloned())
}

fn route(shared: &Shared, m: Pieces) {
    let Some(first) = m.pieces.first() else {
        add(&shared.counters.bad_route, 1);
        return;
    };
    /* The head is the channel's, whole in the first piece: a piece is far
     * larger than it */
    let head = match route::decode(first) {
        Ok(h) => h,
        Err(_) => {
            add(&shared.counters.bad_route, 1);
            return;
        }
    };
    let peer = peer_by_number(shared, head.peer);
    match head.kind {
        route::SEND => {
            let to = match head.to {
                To::Addr(a) => Dest::Addr(a),
                To::Name(n) => Dest::Name(n.to_vec()),
            };
            let (tag, reference, peer_number) = (head.tag, head.reference, head.peer);
            let class = if tag & TAG_BULK != 0 { BULK } else { CONTROL };
            let len = m.len.saturating_sub(route::HEAD);
            let msg = OutMsg { from: m.from.0, to, tag, reference, pieces: m.pieces, skip: route::HEAD, len };
            let left = match &peer {
                Some(p) => p.out[class].with(|o| {
                    if o.number != peer_number || o.bytes + len > QUEUE_BYTES[class] || o.msgs.try_reserve(1).is_err() {
                        return Some(msg);
                    }
                    o.bytes += len;
                    o.msgs.push_back(msg);
                    None
                }),
                None => Some(msg),
            };
            if let Some(msg) = left {
                unsent(shared, peer.as_deref(), peer_number, &msg);
            }
        }
        route::MONITOR => {
            let To::Addr(target) = head.to else { return };
            let asked = peer.as_ref().is_some_and(|p| push_frame(p, CONTROL, &Frame::Monitor { id: head.reference,
                                                                                              target }));
            if !asked {
                /* The monitor's record here ends now, rather than wait for
                 * a Down nobody was asked to send */
                let _ = shared.router.remote_down(head.peer, head.reference, Reason::HostLost);
            }
        }
        route::DEMONITOR => {
            if let Some(p) = &peer {
                push_frame(p, CONTROL, &Frame::Demonitor { id: head.reference });
            }
        }
        _ => add(&shared.counters.bad_route, 1),
    }
}

/// A message for the peer `number` that will not go: bounced to its sender.
fn unsent(shared: &Shared, peer: Option<&Peer>, number: u16, msg: &OutMsg) {
    let to = match &msg.to {
        Dest::Addr(a) => RemoteTo::Addr(Addr(*a)),
        Dest::Name(n) => match core::str::from_utf8(n) {
            Ok(n) => RemoteTo::Name(n),
            Err(_) => return,
        },
    };
    let told = shared.router.bounced(number, Addr(msg.from), to, msg.tag, msg.reference, Bounce::Unsent).is_ok();
    match peer {
        Some(p) if told => add(&p.counts.bounced_out, 1),
        Some(p) => add(&p.counts.dropped, 1),
        None => add(&shared.counters.unrouted, 1),
    }
}

/// A Down of a monitor the router holds for a peer: told to the peer, under
/// the id it gave.
fn down(shared: &Shared, m: &Pieces) {
    let mut reason = [0u8; 4];
    let mut at = 0;
    for p in &m.pieces {
        for &b in p {
            if at < reason.len() {
                reason[at] = b;
            }
            at += 1;
        }
    }
    let reason = Reason::from_code(u32::from_le_bytes(reason)).unwrap_or(Reason::HostLost);
    let peers: Vec<Arc<Peer>> = shared.peers.with(|v| v.clone());
    for p in peers {
        let remote = p.st.with(|st| {
            let i = st.monitors.iter().position(|&(_, local)| local == m.reference)?;
            Some(st.monitors.swap_remove(i).0)
        });
        if let Some(id) = remote {
            push_frame(&p, CONTROL, &Frame::Down { id, reason: byte(reason.code()) });
            return;
        }
    }
}
