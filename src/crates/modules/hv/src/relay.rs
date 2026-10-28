//! A relay between two byte streams: `hv forward`'s, between a client of
//! nos's port and a guest's server. It is kept apart from the TCP it runs
//! over so that the host can drive it with a script (fuzz/hv's `relay`).
//!
//! Each direction has a buffer of what one side sent that the other has not
//! taken yet, and a side is read from only when its buffer is empty: a side
//! that takes nothing holds back no more than it is sent. What waits is
//! offered to its receiver a short wait at a time, never in a wait that lasts
//! until it takes some, so
//! - while one direction waits the other is carried: two full windows, each
//!   side waiting to send before it reads, cannot hold the relay;
//! - a stop is seen within one round of short waits;
//! - a side that takes nothing for `stall_ns` ends the relay, rather than
//!   keeping it -- and whoever joins its task -- for good.

/// What a side's read gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// This many bytes, at the head of the buffer.
    Data(usize),
    /// None within the wait.
    Idle,
    /// The end of its stream, or a failure.
    End,
}

/// One side of a relay: a TCP connection, in the kernel.
pub trait Side {
    /// Bytes read into `buf` within `wait_ms`.
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Read;
    /// How many of `buf`'s bytes it took within `wait_ms` -- 0 when it had
    /// no room -- or None once it is gone.
    fn send(&mut self, buf: &[u8], wait_ms: u64) -> Option<usize>;
}

/// Why a relay ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// A side ended its stream, failed or went.
    Closed,
    /// A side took none of what waited for it for `stall_ns`.
    Stalled,
    /// `stop` said so.
    Stopped,
}

/// How much one direction holds while its receiver has no room.
pub const BUF: usize = 4096;

/// One direction: what was read from one side and not yet taken by the
/// other, `buf[start..end]`, and when it last moved -- read in, or taken in
/// part.
struct Leg {
    buf: [u8; BUF],
    start: usize,
    end: usize,
    moved_ns: u64,
}

impl Leg {
    const fn new() -> Leg {
        Leg { buf: [0; BUF], start: 0, end: 0, moved_ns: 0 }
    }
}

/// Carry what each of `a` and `b` sends to the other until one ends or goes,
/// one takes nothing of what waits for it for `stall_ns`, or `stop` says so.
/// Each read and send waits at most `wait_ms`; `now_ns` is the clock a stall
/// is timed by.
pub fn relay<S: Side>(a: &mut S, b: &mut S, wait_ms: u64, stall_ns: u64, now_ns: &dyn Fn() -> u64,
                      stop: &dyn Fn() -> bool) -> Ended {
    let mut legs = [Leg::new(), Leg::new()];
    loop {
        if stop() {
            return Ended::Stopped;
        }
        for (dir, leg) in legs.iter_mut().enumerate() {
            let (from, to) = if dir == 0 { (&mut *a, &mut *b) } else { (&mut *b, &mut *a) };
            if leg.start == leg.end {
                match from.recv(&mut leg.buf, wait_ms) {
                    /* Nothing came, or a side that broke its word and said
                     * nothing came: the other direction next. */
                    Read::Idle | Read::Data(0) => continue,
                    Read::End => return Ended::Closed,
                    Read::Data(n) => {
                        leg.start = 0;
                        leg.end = n.min(BUF);
                        leg.moved_ns = now_ns();
                    }
                }
            }
            match to.send(&leg.buf[leg.start..leg.end], wait_ms) {
                None => return Ended::Closed,
                Some(0) => {
                    if now_ns().saturating_sub(leg.moved_ns) >= stall_ns {
                        return Ended::Stalled;
                    }
                }
                Some(n) => {
                    leg.start += n.min(leg.end - leg.start);
                    leg.moved_ns = now_ns();
                }
            }
        }
    }
}
