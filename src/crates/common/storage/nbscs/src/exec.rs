//! Driving the engine's futures (N12): the one way a future of the
//! engine's is polled, so that how is said once.

use alloc::boxed::Box;
use core::future::Future;
use core::task::{Context, Poll, Waker};

/// `f` driven to its end now: an operation of the engine's over devices
/// that answer at once -- a device's queue whose requests are waited out
/// (nbs-cs.ko until N12c), the fuzzer's disks but the cs target's -- is
/// ready at its first poll. Boxed: an operation's state is no burden for a
/// kernel task's stack. One that is not ready is waiting on a device that
/// answers later, or on another operation, which a caller that drives one
/// at a time never has: an invariant broken, said as such.
pub fn now<F: Future>(f: F) -> F::Output {
    let mut f = Box::pin(f);
    let mut cx = Context::from_waker(Waker::noop());
    match f.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("invariant: an operation of the cs's engine driven now waits on something"),
    }
}
