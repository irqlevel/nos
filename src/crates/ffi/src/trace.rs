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
    /// offset in the payload, its size, and how many names its values have
    /// -- an enumeration's; 0 for any other field -- 1, or 0 past the
    /// event's last field or for no such event.
    pub fn kernel_tp_field(id: u16, index: usize, name: *mut u8, name_len: usize, offset: *mut usize,
        size: *mut usize, names: *mut usize) -> i32;
    /// What value `value` of that field is called, into `name` (`name_len`
    /// bytes, terminated, cut to fit): 1, or 0 for a field that is no
    /// enumeration or a value it has no name for.
    pub fn kernel_tp_field_name(id: u16, index: usize, value: usize, name: *mut u8, name_len: usize) -> i32;
    /// The pid a record says the running task by -- its own, or the kernel's
    /// none (`tp::NO_TASK_PID`) off a task's stack. Values only.
    pub safe fn kernel_tp_pid() -> u32;
}
