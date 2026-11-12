//! The events tasks wait on: one a task, which the kernel's objects made on
//! it signal -- a mailbox (`ipc::Mailbox::new_on`), a device's queue and an
//! image's (the `block::queue::Queue` under both) -- so that a task that
//! waits for several things sleeps on one and wakes for any of them
//! (plans/ipc.md, I2; docs/ipc.md). The image's tasks use the crate; a
//! module holds its events in the table below, through the C ABI
//! (base/ffi/src/events.rs, kcore::events).
//!
//! What keeps it sound:
//!
//! - **Counted.** A `SharedEvent` is held as an `Arc`: by its task, and by
//!   every object made on it, each of which signals an event it holds. It
//!   is freed when the last of them lets go, whichever that is -- so none is
//!   ever signalled once it is freed, and nobody has to know who goes last.
//! - **One waiter.** A wait holds the event's flag for as long as it waits
//!   (`hold`): a second task waiting on it -- or receiving from a mailbox
//!   made on it -- is refused and said, never the kernel `Event`'s panic at
//!   a second waiter. The objects only signal.
//! - **A module's are looked up.** A handle is a slot of the table and the
//!   slot's generation, so one that is no event's is refused, never
//!   followed; the table holds a count of the event for the handle, which
//!   a close lets go of -- an object made on it keeps it from there on.
//! - **Nothing freed with interrupts off.** A signal by handle is made under
//!   its slot's interrupt-safe lock and takes no count; whatever lets go of
//!   one -- a close, a wait done -- takes it out under the lock and drops it
//!   after, in task context.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use kcore::klog;
use kcore::owner::Owner;
use kcore::sync::{Event, IrqSpinLock};
use kcore::time::Duration;

/// Events the modules hold at once through the table: every task of every
/// module that waits for several things, and room to spare.
pub const MAX_EVENTS: usize = 256;

/// The events there are, each from `new` until its last holder lets go:
/// what the tests hold an event's life to.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// An event one task waits on and anyone signals -- from anywhere, a hard
/// interrupt's handler included -- held as an `Arc` by the task and by every
/// object made on it, and freed with the last of them.
pub struct SharedEvent {
    event: Event,
    /// A task holds it for a wait (`hold`): one at a time.
    waiting: AtomicBool,
}

impl SharedEvent {
    /// An event, the caller its one holder: None when the kernel has no
    /// event to give. The `Arc`'s few bytes are the heap's own, and a heap
    /// that has not even those panics, as every infallible allocation of
    /// the image's does.
    pub fn new() -> Option<Arc<SharedEvent>> {
        let event = Event::new()?;
        LIVE.fetch_add(1, Ordering::Relaxed);
        Some(Arc::new(SharedEvent { event, waiting: AtomicBool::new(false) }))
    }

    /// The task waiting on it woken -- or, none waiting, its next wait
    /// ended at once. From anywhere.
    #[inline]
    pub fn signal(&self) {
        self.event.signal();
    }

    /// The event held for a wait -- a task's, or a mailbox's receive that
    /// looks at its queue between its sleeps -- until the `Hold` goes: None
    /// while another task holds it, which is the caller's to say.
    pub fn hold(&self) -> Option<Hold<'_>> {
        if self.waiting.swap(true, Ordering::Acquire) {
            return None;
        }
        Some(Hold { event: self, _task: PhantomData })
    }

    /// A wait of up to `timeout` for a signal: true when one came, false
    /// when the time ran out -- or at once, and said, while another task
    /// waits on it. Task context.
    pub fn wait_for(&self, timeout: Duration) -> bool {
        match self.hold() {
            Some(held) => held.wait_for(timeout),
            None => {
                klog!(0, "events: two tasks waiting on one event at once -- the second is refused");
                false
            }
        }
    }

    /// A signal looked for for up to `poll`, this CPU spinning rather than
    /// sleeping: whether one came -- taken, as a wait takes it. What a task
    /// that expects its signal within microseconds does before `wait_for`.
    /// Refused, and said, as `wait_for` is.
    pub fn poll(&self, poll: Duration) -> bool {
        match self.hold() {
            Some(held) => held.poll(poll),
            None => {
                klog!(0, "events: two tasks waiting on one event at once -- the second is refused");
                false
            }
        }
    }
}

impl Drop for SharedEvent {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A task's hold of an event for its wait (`SharedEvent::hold`): the event
/// is waited on through this alone, and by nobody else until it goes.
pub struct Hold<'a> {
    event: &'a SharedEvent,
    /// The task's that took it: not to be handed to another.
    _task: PhantomData<*const ()>,
}

impl Hold<'_> {
    /// As `SharedEvent::wait_for`, held already.
    pub fn wait_for(&self, timeout: Duration) -> bool {
        self.event.event.wait_for(timeout)
    }

    /// As `SharedEvent::poll`, held already.
    pub fn poll(&self, poll: Duration) -> bool {
        self.event.event.poll(poll)
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        self.event.waiting.store(false, Ordering::Release);
    }
}

/// How many events there are: made, and not yet let go of by all that held
/// them.
pub fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/* ---- the events the modules hold (`kcore::events::SharedEvent`) ----
 *
 * An event crosses as a word: a slot of this table, plus one, below, and the
 * generation of what is in it above -- looked up on every call, so a word
 * that is no event's names nothing, which is why every call here is sound
 * for any word. Each slot under a lock of its own, interrupt-safe, as a
 * signal may come from an interrupt: held for a look and a signal, never
 * across an allocation, a free or a wait. */

struct Slot {
    generation: u32,
    /// The table's count of its event, for as long as the handle is open.
    event: Option<Arc<SharedEvent>>,
    /// The module that opened it: what `rust_events_module_gone` closes by.
    owner: Owner,
}

static SLOTS: [IrqSpinLock<Slot>; MAX_EVENTS] =
    [const { IrqSpinLock::new(Slot { generation: 0, event: None, owner: Owner::IMAGE }) }; MAX_EVENTS];

/* The slot, plus one, below; the generation above. */
const SLOT_BITS: u32 = 32;
const SLOT_MASK: u64 = (1 << SLOT_BITS) - 1;

fn word(slot: usize, generation: u32) -> u64 {
    (u64::from(generation) << SLOT_BITS) | (slot as u64 + 1)
}

/// The slot `event` names, and the generation it names there: None for 0,
/// or a slot past the table.
fn slot_of(event: u64) -> Option<(&'static IrqSpinLock<Slot>, u32)> {
    let at = usize::try_from(event & SLOT_MASK).ok()?.checked_sub(1)?;
    Some((SLOTS.get(at)?, (event >> SLOT_BITS) as u32))
}

/// The event a module's handle names, with a count of it the caller's:
/// None for a word that is no event's. What a mailbox or a queue made on a
/// module's event holds it by. Task context: the count's drop may be the
/// last, which frees the event.
pub fn lookup(event: u64) -> Option<Arc<SharedEvent>> {
    let (slot, generation) = slot_of(event)?;
    let s = slot.lock();
    if s.generation != generation {
        return None;
    }
    s.event.clone()
}

/// An event for a module -- the one whose code `owner` is in: its handle,
/// held by the table until `kernel_shared_event_close`, or until the
/// module goes (`rust_events_module_gone`) -- or 0 when there is no memory
/// for one, or every slot is taken.
#[no_mangle]
pub extern "C" fn kernel_shared_event_open(owner: usize) -> u64 {
    let owner = Owner::at(owner);
    /* Made before a slot is looked for: nothing is allocated under a slot's
     * lock */
    let Some(event) = SharedEvent::new() else { return 0 };
    for (at, slot) in SLOTS.iter().enumerate() {
        let mut s = slot.lock();
        if s.event.is_some() {
            continue;
        }
        s.generation = s.generation.wrapping_add(1);
        s.event = Some(event);
        s.owner = owner;
        return word(at, s.generation);
    }
    klog!(0, "events: all {} events the modules may hold are taken", MAX_EVENTS);
    /* Off every slot's lock: the only count, and so the event freed */
    drop(event);
    0
}

/// The handle let go of: the event goes once no object made on it holds it
/// either. Nothing for a word that is no event's. Task context.
#[no_mangle]
pub extern "C" fn kernel_shared_event_close(event: u64) {
    let Some((slot, generation)) = slot_of(event) else { return };
    let taken = {
        let mut s = slot.lock();
        if s.generation != generation {
            return;
        }
        s.event.take()
    };
    /* Off the lock: the table's count let go of, perhaps the last */
    drop(taken);
}

/// What a module that went left open, closed: each event its code opened
/// and never closed -- an event that stays as long as whatever was made on
/// it holds it, but a slot of the table's 256 that was kept for good -- and
/// said, as the module's bug. The loader's, at the module's take-down
/// (`ModuleTable::take_down`). Task context.
///
/// # Safety
/// `name` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_events_module_gone(name: *const u8, len: usize) {
    if name.is_null() || len == 0 || len > kcore::owner::NAME_MAX {
        return;
    }
    let name = unsafe { core::slice::from_raw_parts(name, len) };
    let mut left = 0usize;
    for slot in SLOTS.iter() {
        let taken = {
            let mut s = slot.lock();
            if !s.owner.is_module(name) || s.event.is_none() {
                continue;
            }
            s.owner = Owner::IMAGE;
            s.event.take()
        };
        /* Off the lock: the table's count let go of, perhaps the last */
        drop(taken);
        left += 1;
    }
    if left != 0 {
        klog!(0, "module: {} left {} event(s) open: closed", core::str::from_utf8(name).unwrap_or("?"), left);
    }
}

/// A wait of up to `timeout_ns` for a signal: 1 when one came, 0 when the
/// time ran out, -1 at once for a word that is no event's or while another
/// task waits on it. Task context.
#[no_mangle]
pub extern "C" fn kernel_shared_event_wait(event: u64, timeout_ns: u64) -> i32 {
    let Some(e) = lookup(event) else { return -1 };
    let Some(held) = e.hold() else {
        klog!(0, "events: two tasks waiting on one event at once -- the second is refused");
        return -1;
    };
    let got = held.wait_for(Duration::from_nanos(timeout_ns));
    drop(held);
    /* The count, after the wait: the event outlives it whoever let go */
    drop(e);
    i32::from(got)
}

/// What `kernel_shared_event_wait` sleeps for, spun for for up to
/// `poll_ns`: 1, 0 or -1 as it answers.
#[no_mangle]
pub extern "C" fn kernel_shared_event_poll(event: u64, poll_ns: u64) -> i32 {
    let Some(e) = lookup(event) else { return -1 };
    let Some(held) = e.hold() else {
        klog!(0, "events: two tasks waiting on one event at once -- the second is refused");
        return -1;
    };
    let got = held.poll(Duration::from_nanos(poll_ns));
    drop(held);
    drop(e);
    i32::from(got)
}

/// The event's waiter woken, from any context; nothing for a word that is
/// no event's. Made under the slot's lock, so the event is the table's
/// while it is signalled, and no count is taken or dropped here.
#[no_mangle]
pub extern "C" fn kernel_shared_event_signal(event: u64) {
    let Some((slot, generation)) = slot_of(event) else { return };
    let s = slot.lock();
    if s.generation == generation {
        if let Some(e) = s.event.as_ref() {
            e.signal();
        }
    }
}
