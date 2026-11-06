//! One event to wait on (plans/ipc.md, I2), held to what it says at every
//! boot: a mailbox and a block queue made on one event, and a task asleep on
//! it -- woken by a message, by a completion made in another CPU's interrupt
//! through the queue's own completion path, and by the queue's wake; a
//! second waiter refused while it waits; the task and the event's owner
//! letting go first, the mailbox and the queue keeping the event, a message
//! and a completion after that harmless; and the event freed with the last
//! of them. A failure panics, as the boot's selftests do.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use events::SharedEvent;
use ipc::{Limits, Mailbox, Payload, To};
use kcore::klog;
use kcore::time::Duration;

/// How long the task sleeps at a time: far past what a signal takes to wake
/// it, so that a wake inside it is a signal's.
const NAP_SECS: u64 = 30;
/// How long the test waits for the task to have woken -- or to be found
/// waiting -- a millisecond at a time.
const WITHIN_MS: u64 = 5000;
const LIMITS: Limits = Limits { messages: 4, bytes: 64 };

/// What the test and the task asleep on the event share.
struct Waiter {
    event: Arc<SharedEvent>,
    /// How many of its waits a signal ended.
    woke: AtomicU32,
    stop: AtomicBool,
}

/// The task: asleep on the event until told to stop, counting the waits a
/// signal ended. The event held by the test now and then -- a second
/// waiter, which must be refused while this one waits -- is a moment's.
fn wait(w: Arc<Waiter>) {
    while !w.stop.load(Ordering::Acquire) {
        match w.event.hold() {
            Some(held) => {
                if held.wait_for(Duration::from_secs(NAP_SECS)) {
                    w.woke.fetch_add(1, Ordering::AcqRel);
                }
            }
            None => kcore::task::sleep_ms(1),
        }
    }
}

fn check(ok: bool, what: &str) {
    if !ok {
        panic!("one event selftest: {}", what);
    }
}

/// Whether the task's waits ended past `before` within WITHIN_MS.
fn woken(w: &Waiter, before: u32) -> bool {
    for _ in 0..WITHIN_MS {
        if w.woke.load(Ordering::Acquire) != before {
            return true;
        }
        kcore::task::sleep_ms(1);
    }
    false
}

/// Whether a second waiter is refused while the task waits: looked for
/// again while the task is found between two of its waits.
fn second_refused(event: &SharedEvent) -> bool {
    for _ in 0..WITHIN_MS {
        match event.hold() {
            None => return true,
            Some(held) => {
                drop(held);
                kcore::task::sleep_ms(1);
            }
        }
    }
    false
}

/// A CPU other than this one, where there is one: where a completion's
/// interrupt is made, as a device's comes wherever it is routed.
fn other_cpu() -> u32 {
    let me = kcore::cpu::id();
    let online = kcore::cpu::online_mask();
    (0..u64::BITS).find(|&c| c != me && online & (1u64 << c) != 0).unwrap_or(me)
}

/// A command of buffer 0's finished in another CPU's interrupt, as the
/// queue's device would finish one: through the queue's own completion.
fn complete(q: &mut block::queue::Queue) {
    let cmd = q.test_command(0).unwrap_or_else(|| panic!("one event selftest: a command of the queue's"));
    kcore::cpu::run_on_with(other_cpu(), &cmd, |c| c.finish(true));
}

pub fn selftest() {
    let event = SharedEvent::new().unwrap_or_else(|| panic!("one event selftest: an event"));
    let mb = Mailbox::new_on(LIMITS, &event)
        .unwrap_or_else(|e| panic!("one event selftest: a mailbox on the event: {}", e.name()));
    /* A queue of buffers alone: its completions are the test's */
    let mut q = block::queue::Queue::new(None, 1, 1, Some(event.clone()))
        .unwrap_or_else(|| panic!("one event selftest: a queue on the event"));
    let w = Arc::new(Waiter { event: event.clone(), woke: AtomicU32::new(0), stop: AtomicBool::new(false) });
    let task = kcore::task::spawn_with("one-event", w.clone(), wait)
        .unwrap_or_else(|| panic!("one event selftest: a task to wait"));

    /* A message to the mailbox */
    let before = w.woke.load(Ordering::Acquire);
    check(mb.send(To::Addr(mb.addr()), 1, 0, Payload::empty()).is_ok(), "a message to the mailbox refused");
    check(woken(&w, before), "a message did not wake the task waiting on its mailbox's event");
    check(mb.try_recv().is_some_and(|m| m.tag() == 1), "the message not in the mailbox");

    /* A completion of the queue's */
    let before = w.woke.load(Ordering::Acquire);
    complete(&mut q);
    check(woken(&w, before), "a completion did not wake the task waiting on its queue's event");
    check(q.reap() == Some((0, true)), "the completion not in the queue");

    /* The queue's wake */
    let before = w.woke.load(Ordering::Acquire);
    block::queue::wake(q.entry());
    check(woken(&w, before), "the queue's wake did not wake the task waiting on its event");

    /* One waiter: the task's */
    check(second_refused(&event), "a second waiter let wait on an event a task waits on");

    /* The task and the event's owner let go first: the mailbox and the queue
     * keep it, and are as they were */
    w.stop.store(true, Ordering::Release);
    event.signal();
    drop(task);
    drop(w);
    let weak = Arc::downgrade(&event);
    drop(event);
    check(weak.upgrade().is_some(), "the event freed while a mailbox and a queue made on it were open");
    check(mb.send(To::Addr(mb.addr()), 2, 0, Payload::empty()).is_ok() && mb.try_recv().is_some_and(|m| m.tag() == 2),
          "a message once the event's owner let go");
    drop(mb);
    check(weak.upgrade().is_some(), "the event freed while a queue made on it was open");
    complete(&mut q);
    block::queue::wake(q.entry());
    check(q.reap() == Some((0, true)), "a completion once the event's owner let go");
    drop(q);
    check(weak.upgrade().is_none(), "the event outlived the last of what held it");
    klog!(0, "one event: selftest passed");
}
