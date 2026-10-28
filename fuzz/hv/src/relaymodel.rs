//! `hv forward`'s relay (modules/hv/src/relay.rs) between two sides a script
//! drives: what each side sends and when, whether each has room for what it
//! is sent -- a window shut for a while, then open again -- when one ends or
//! goes, when the clock jumps, and when the relay is told to stop. Held to:
//! - every byte one side sent reaches the other in order, once, and nothing
//!   else does: what the relay offers a side is exactly what it holds for it;
//! - a side is read from only while the relay holds none of its bytes, and
//!   then every round: a direction with no room never holds the other back;
//! - an end or a failure ends the relay at once, a stop within a round;
//! - a direction that has moved nothing for the stall time ends the relay
//!   the moment its receiver is offered it and takes nothing -- never before.

use std::cell::RefCell;

use crate::relay::{self, Ended, Read, Side};
use crate::targets::Input;

const WAIT_MS: u64 = 5;
const NS_PER_MS: u64 = 1_000_000;
/// The stall, short enough for a script to reach: the logic is the one the
/// kernel's five minutes go through.
const STALL_NS: u64 = 24 * WAIT_MS * NS_PER_MS;
/// The calls of one round of the relay's: a read and a send each way.
const ROUND: u64 = 4;
/// Calls a script may take at most: a relay that makes more has not ended
/// when it should have.
const MAX_CALLS: u64 = 100_000;

struct World<'a> {
    input: Input<'a>,
    now_ns: u64,
    calls: u64,
    /// What each side sent: all the relay read of it, in order.
    sent: [Vec<u8>; 2],
    /// What the relay gave each side.
    got: [Vec<u8>; 2],
    /// When each direction -- d is side d's bytes, to side 1 - d -- last
    /// moved as the relay times it: read in, or taken in part.
    moved_ns: [u64; 2],
    /// Calls since each side could be read from: since it was, or since the
    /// relay last gave away all it held of it.
    since_read: [u64; 2],
    /// Each side's window: shut, it takes nothing it is offered.
    shut: [bool; 2],
    /// A side ended or went: the relay must not call again.
    closed: bool,
    /// A send that took nothing was answered with its direction overdue: the
    /// relay must return Stalled, calling nothing more.
    stall_due: bool,
    stop: bool,
    /// Calls since the stop was asked for.
    since_stop: u64,
}

impl World<'_> {
    /// What the relay holds of direction `d`, for side 1 - d.
    fn pending(&self, d: usize) -> usize {
        self.sent[d].len() - self.got[1 - d].len()
    }

    /// Every call's checks, before it is answered; and maybe the stop.
    fn enter(&mut self) {
        self.calls += 1;
        invariant!(self.calls <= MAX_CALLS, "the relay made {} calls without ending", self.calls);
        invariant!(!self.closed, "the relay went on after a side ended or went");
        invariant!(!self.stall_due, "the relay went on with a side that took nothing for {} ns", STALL_NS);
        if self.stop {
            self.since_stop += 1;
            invariant!(self.since_stop <= ROUND, "the relay went on {} calls after its stop", self.since_stop);
        }
        for d in 0..2 {
            self.since_read[d] += 1;
            if self.pending(d) == 0 {
                invariant!(self.since_read[d] <= ROUND,
                           "side {} not read from for {} calls, with nothing of its held", d, self.since_read[d]);
            }
        }
        if !self.stop && self.input.rare() {
            self.stop = true;
        }
    }

    /// A wait that ran out: the clock moves by it -- now and then by much
    /// more, the task not run for a while.
    fn wait(&mut self, wait_ms: u64) {
        self.now_ns += wait_ms * NS_PER_MS;
        if self.input.rare() {
            self.now_ns += self.input.below(4 * STALL_NS);
        }
    }

    /// The byte at `at` of what side `side` sends.
    fn byte(side: usize, at: usize) -> u8 {
        (at as u8).wrapping_mul(31) ^ ((at >> 8) as u8) ^ (side as u8).wrapping_mul(0x5A)
    }
}

/// A side of the relay, its answers the script's.
struct Model<'w, 'a> {
    side: usize,
    world: &'w RefCell<World<'a>>,
}

impl Side for Model<'_, '_> {
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Read {
        let mut w = self.world.borrow_mut();
        w.enter();
        let side = self.side;
        invariant!(w.pending(side) == 0, "side {} read from with {} of its bytes still held", side, w.pending(side));
        invariant!(wait_ms > 0 && !buf.is_empty(), "a read into {} bytes, waiting {} ms", buf.len(), wait_ms);
        w.since_read[side] = 0;
        match w.input.op(16) {
            /* Past the end of the script, the sides end. */
            None => {
                w.closed = true;
                Read::End
            }
            Some(15) if w.input.rare() => {
                w.closed = true;
                Read::End
            }
            Some(0..=6) | Some(15) => {
                w.wait(wait_ms);
                Read::Idle
            }
            Some(_) => {
                let most = if w.input.u8() % 4 == 0 { buf.len() } else { 64.min(buf.len()) };
                let k = 1 + w.input.below(most as u64) as usize;
                let at = w.sent[side].len();
                for (i, b) in buf[..k].iter_mut().enumerate() {
                    *b = World::byte(side, at + i);
                }
                w.sent[side].extend_from_slice(&buf[..k]);
                w.moved_ns[side] = w.now_ns;
                Read::Data(k)
            }
        }
    }

    fn send(&mut self, buf: &[u8], wait_ms: u64) -> Option<usize> {
        let mut w = self.world.borrow_mut();
        w.enter();
        let to = self.side;
        let d = 1 - to;
        let held = {
            let from = w.got[to].len();
            buf == &w.sent[d][from..]
        };
        invariant!(!buf.is_empty() && held, "side {} offered {} bytes, not the {} the relay holds for it",
                   to, buf.len(), w.pending(d));
        invariant!(wait_ms > 0, "a send waiting 0 ms");
        if w.input.below(16) == 0 {
            w.shut[to] = !w.shut[to];
        }
        match w.input.op(16) {
            None => {
                w.closed = true;
                None
            }
            Some(15) if w.input.rare() => {
                w.closed = true;
                None
            }
            _ if w.shut[to] => {
                w.wait(wait_ms);
                if w.now_ns.saturating_sub(w.moved_ns[d]) >= STALL_NS {
                    w.stall_due = true;
                }
                Some(0)
            }
            _ => {
                let n = 1 + w.input.below(buf.len() as u64) as usize;
                w.got[to].extend_from_slice(&buf[..n]);
                w.moved_ns[d] = w.now_ns;
                if w.pending(d) == 0 {
                    w.since_read[d] = 0;
                }
                Some(n)
            }
        }
    }
}

pub fn relay_target(input: &mut Input) {
    let world = RefCell::new(World {
        input: Input::new(input.rest()),
        now_ns: 0,
        calls: 0,
        sent: [Vec::new(), Vec::new()],
        got: [Vec::new(), Vec::new()],
        moved_ns: [0; 2],
        since_read: [0; 2],
        shut: [false; 2],
        closed: false,
        stall_due: false,
        stop: false,
        since_stop: 0,
    });
    let mut a = Model { side: 0, world: &world };
    let mut b = Model { side: 1, world: &world };
    let now = || world.borrow().now_ns;
    let stop = || world.borrow().stop;
    let ended = relay::relay(&mut a, &mut b, WAIT_MS, STALL_NS, &now, &stop);
    let w = world.borrow();
    match ended {
        Ended::Closed => invariant!(w.closed, "the relay said a side ended when none had"),
        Ended::Stalled => invariant!(w.stall_due, "the relay gave a side up before it took nothing for {} ns", STALL_NS),
        Ended::Stopped => invariant!(w.stop, "the relay stopped unasked"),
    }
    invariant!(!w.stall_due || ended == Ended::Stalled, "a side overdue, and the relay ended {:?}", ended);
    for to in 0..2 {
        invariant!(w.got[to][..] == w.sent[1 - to][..w.got[to].len()], "side {} got what side {} did not send",
                   to, 1 - to);
    }
}
