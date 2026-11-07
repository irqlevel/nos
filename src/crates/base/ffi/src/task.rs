unsafe extern "C" {
    pub fn kernel_task_spawn(
        name: *const u8,
        name_len: usize,
        func: extern "C" fn(*mut u8),
        ctx: *mut u8,
    ) -> usize;
    pub fn kernel_task_spawn_on(
        name: *const u8,
        name_len: usize,
        func: extern "C" fn(*mut u8),
        ctx: *mut u8,
        affinity_mask: usize,
    ) -> usize;
    pub fn kernel_task_wait(handle: usize);
    pub fn kernel_task_set_stopping(handle: usize);
    pub fn kernel_task_put(handle: usize);
    /// Whether the calling task has been asked to stop.
    pub safe fn kernel_task_stopping() -> i32;
    pub safe fn kernel_sleep_ns(ns: u64);
    pub safe fn kernel_task_yield_to_runnable();
    /// The calling task, as a handle from the spawn calls names it.
    pub safe fn kernel_task_current() -> usize;
    /// The same, but 0 rather than a complaint when the stack is not a
    /// task's -- for code that runs wherever it is called from.
    pub safe fn kernel_task_current_or_none() -> usize;
}
