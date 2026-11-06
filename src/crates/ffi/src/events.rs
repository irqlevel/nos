/* The events a module's tasks wait on (src/crates/events defines these,
   kcore::events wraps them): one a task, which what the module makes on it
   signals -- a mailbox (kernel_ipc_open), a device's queue
   (kernel_blockq_open), an image's (kernel_image_queue), each given the
   event's handle -- so that a task waiting for several things sleeps on one.
   The kernel holds the event for every object made on it, and frees it with
   the last of them and the handle. A handle is a slot of the kernel's table
   and its generation, looked up on every call, so one that is no event's is
   nothing: every call is sound for any handle. */
unsafe extern "C" {
    /// An event, held for its handle until kernel_shared_event_close: the
    /// handle, or 0 when there is no memory or every slot is taken.
    pub safe fn kernel_shared_event_open() -> u64;

    /// The handle let go of: the event goes once nothing made on it holds it
    /// either. Task context.
    pub safe fn kernel_shared_event_close(event: u64);

    /// A wait of up to `timeout_ns` for a signal: 1 when one came, 0 when
    /// the time ran out, -1 at once for a handle that is no event's or while
    /// another task waits on it. Task context, one task at a time.
    pub safe fn kernel_shared_event_wait(event: u64, timeout_ns: u64) -> i32;

    /// The same, spun for for up to `poll_ns`.
    pub safe fn kernel_shared_event_poll(event: u64, poll_ns: u64) -> i32;

    /// The event's waiter woken, from any context.
    pub safe fn kernel_shared_event_signal(event: u64);
}
