extern "C" {
    pub fn kernel_timer_start(
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
        period_ns: u64,
    ) -> usize;
    pub fn kernel_timer_stop(handle: usize);
}
