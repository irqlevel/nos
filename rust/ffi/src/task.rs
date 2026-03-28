extern "C" {
    pub fn kernel_task_spawn(func: extern "C" fn(*mut u8), ctx: *mut u8) -> usize;
    pub fn kernel_task_wait(handle: usize);
    pub fn kernel_task_set_stopping(handle: usize);
    pub fn kernel_task_put(handle: usize);
    pub fn kernel_sleep_ns(ns: u64);
    pub fn kernel_get_cpu_id() -> u32;
}
