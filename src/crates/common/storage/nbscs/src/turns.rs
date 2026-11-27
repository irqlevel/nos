//! Turns (N12b): what orders the engine's operations on its one task -- a
//! chunk's requests one at a time, in the order they came; a volume's
//! requests together, and its fence alone, after every request before it
//! and before every one after. A turn is a ticket in its key's queue,
//! waited for by being polled (no waker is kept anywhere: the executor
//! polls every operation that may have moved on), and let go when its
//! guard drops -- however the operation ends, a power cut's drop of every
//! future in flight among them. Each let go is counted (`released`): what
//! the executor polls again for.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::Rc;
use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::task::Poll;

use crate::trace::{self, NbsTurnStage, Trace};

/// The queues of one kind of key: a chunk's number, a volume's.
#[derive(Default)]
pub(crate) struct Turns {
    queues: RefCell<BTreeMap<u32, VecDeque<(u64, bool)>>>,
    next: Cell<u64>,
    released: Cell<u64>,
    trace: Option<Rc<dyn Trace>>,
}

/// A turn held, or waited for: let go when dropped.
pub(crate) struct Turn<'a> {
    turns: &'a Turns,
    key: u32,
    ticket: u64,
    alone: bool,
    since: u64,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        trace::turn(self.turns.trace.as_ref(), self.ticket, self.key, self.alone, NbsTurnStage::Release, self.since);
        self.turns.released.set(self.turns.released.get().wrapping_add(1));
        let mut q = self.turns.queues.borrow_mut();
        if let Some(waiting) = q.get_mut(&self.key) {
            waiting.retain(|&(t, _)| t != self.ticket);
            if waiting.is_empty() {
                q.remove(&self.key);
            }
        }
    }
}

impl Turns {
    pub(crate) fn set_tracer(&mut self, tracer: Rc<dyn Trace>) {
        self.trace = Some(tracer);
    }

    /// Whether `ticket` may go: alone, the first in its queue; shared, with
    /// nobody alone before it.
    fn may_go(&self, key: u32, ticket: u64) -> bool {
        let q = self.queues.borrow();
        let Some(waiting) = q.get(&key) else { return false };
        let Some(at) = waiting.iter().position(|&(t, _)| t == ticket) else { return false };
        if waiting[at].1 {
            at == 0
        } else {
            waiting.iter().take(at).all(|&(_, alone)| !alone)
        }
    }

    /// `key`'s turn, `alone` or shared, once every turn before it that it
    /// cannot share has been let go.
    pub(crate) async fn take(&self, key: u32, alone: bool) -> Turn<'_> {
        let ticket = self.next.get();
        self.next.set(ticket + 1);
        self.queues.borrow_mut().entry(key).or_default().push_back((ticket, alone));
        let since = trace::turn_since(self.trace.as_ref());
        trace::turn(self.trace.as_ref(), ticket, key, alone, NbsTurnStage::Wait, 0);
        /* Made first, so that a wait given up -- the future dropped --
           takes its ticket out of the queue */
        let turn = Turn { turns: self, key, ticket, alone, since };
        poll_fn(|_| if self.may_go(key, ticket) { Poll::Ready(()) } else { Poll::Pending }).await;
        trace::turn(self.trace.as_ref(), ticket, key, alone, NbsTurnStage::Acquire, since);
        turn
    }

    /// Turns let go, counted.
    pub(crate) fn released(&self) -> u64 {
        self.released.get()
    }

    /// Turns held or waited for: none once every operation is over.
    pub(crate) fn in_use(&self) -> usize {
        self.queues.borrow().values().map(|q| q.len()).sum()
    }

    /// Every key's queue, in words.
    pub(crate) fn held(&self) -> alloc::string::String {
        alloc::format!("{:?}", self.queues.borrow())
    }
}
