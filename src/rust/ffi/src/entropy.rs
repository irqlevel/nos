/* A source of raw entropy for the kernel's pool (kernel/entropy.h): a device
   the pool asks for bytes when it reseeds. Registration is for good -- a
   source cannot be taken back -- so everything handed over here has to
   outlive the kernel's use of it. */
extern "C" {
    /// Register a source. `name` is NUL-terminated and kept; `get_random`
    /// fills `len` bytes and answers 0, anything else on failure, and runs
    /// in task context (it may be slow: virtio-rng polls its device).
    pub fn kernel_entropy_source_register(
        name: *const u8,
        get_random: extern "C" fn(ctx: *mut u8, buf: *mut u8, len: usize) -> i32,
        ctx: *mut u8,
    ) -> usize;
}
