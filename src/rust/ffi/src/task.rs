extern "C" {
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
    pub fn kernel_sleep_ns(ns: u64);
    pub fn kernel_task_yield_to_runnable();
}
