//! Driving the engine's futures one at a time (N12): how the fuzzers'
//! targets whose disks answer at once drive an operation, said once.
//! nbs-cs.ko drives its own many at once on its task (N12c), and fuzz/nbs's
//! inflight target its own.

use alloc::boxed::Box;
use core::future::Future;
use core::task::{Context, Poll, Waker};

/// `f` driven to its end now: an operation of the engine's over devices
/// that answer at once -- the fuzzer's disks but the inflight target's -- is
/// ready at its first poll. Boxed: an operation's state is no burden for a
/// stack. One that is not ready is waiting on a device that answers later,
/// or on another operation, which a caller that drives one at a time never
/// has: an invariant broken, said as such.
pub fn now<F: Future>(f: F) -> F::Output {
    let mut f = Box::pin(f);
    let mut cx = Context::from_waker(Waker::noop());
    match f.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("invariant: an operation of the cs's engine driven now waits on something"),
    }
}
