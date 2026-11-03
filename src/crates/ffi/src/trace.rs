//! The tracepoints (kernel/tracepoint.h): what Rust's sites -- the image's and
//! a module's -- record into the same rings, by the same `emit()`, as the
//! kernel's C++ ones. `kcore::trace` is the safe side.

unsafe extern "C" {
    /// The events' flags, a byte an event and `kernel_tp_count()` of them:
    /// the kernel's static array, for the kernel's life, read and written
    /// only with atomics. Values only.
    pub safe fn kernel_tp_flags() -> *const u8;
    /// How many events the kernel describes: the flags' length. Values only.
    pub safe fn kernel_tp_count() -> usize;
    /// A record of event `id`: `size` bytes at `payload`, the event's
    /// description's size exactly -- anything else is the kernel's BUG_ON.
    pub fn kernel_tp_emit(id: u16, payload: *const u8, size: usize);
    /// Event `id`'s name into `name` (`name_len` bytes, terminated, cut to
    /// fit) and its payload's size into `size`: 1, or 0 for no such event.
    pub fn kernel_tp_event(id: u16, name: *mut u8, name_len: usize, size: *mut usize) -> i32;
    /// Field `index` of event `id`, as its description has it: its name, its
    /// offset in the payload and its size -- 1, or 0 past the event's last
    /// field or for no such event.
    pub fn kernel_tp_field(id: u16, index: usize, name: *mut u8, name_len: usize, offset: *mut usize,
        size: *mut usize) -> i32;
}
