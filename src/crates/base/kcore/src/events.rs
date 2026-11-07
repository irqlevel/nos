//! One event for a module's task to wait on, which what it makes on the
//! event signals -- a mailbox (`ipc::Mailbox::new_on`), a device's queue
//! (`block::Disk::into_queue_on`), an image's (`fs::Image::into_queue_on`)
//! -- so that a task waiting for several things sleeps on one and wakes for
//! any of them (docs/ipc.md). Inside the kernel image it is the `events`
//! crate itself.
//!
//! The kernel holds the event for every object made on it as well as for
//! this handle, so this may be dropped first: they go on signalling an
//! event that is still there, which goes with the last of them. The handle
//! is looked up on every call, and one that is no event's names nothing.

use ffi::events;

use crate::time::Duration;

/// An event a module's task waits on: let go of when dropped.
pub struct SharedEvent {
    handle: u64,
}

impl SharedEvent {
    /// None when the kernel has no memory for one, or holds as many for the
    /// modules as it takes.
    pub fn new() -> Option<SharedEvent> {
        match events::kernel_shared_event_open() {
            0 => None,
            handle => Some(SharedEvent { handle }),
        }
    }

    /// Its waiter woken -- or, none waiting, its next wait ended at once.
    /// From anywhere, an interrupt's handler included.
    pub fn signal(&self) {
        events::kernel_shared_event_signal(self.handle);
    }

    /// A wait of up to `timeout` for a signal: true when one came, false
    /// when the time ran out -- or at once, the kernel saying so, while
    /// another task waits on it or receives from a mailbox made on it.
    /// Task context.
    pub fn wait_for(&self, timeout: Duration) -> bool {
        events::kernel_shared_event_wait(self.handle, timeout.as_nanos()) == 1
    }

    /// A signal looked for for up to `poll`, this CPU spinning rather than
    /// sleeping: whether one came -- taken, as `wait_for` takes it.
    pub fn poll(&self, poll: Duration) -> bool {
        events::kernel_shared_event_poll(self.handle, poll.as_nanos()) == 1
    }

    /// The handle, for an object to be made on it: looked up by the kernel.
    pub(crate) fn handle(&self) -> u64 {
        self.handle
    }
}

impl Drop for SharedEvent {
    fn drop(&mut self) {
        events::kernel_shared_event_close(self.handle);
    }
}
