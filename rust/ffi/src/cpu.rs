extern "C" {
    pub fn kernel_get_cpu_id() -> u32;
    pub fn kernel_cpu_count() -> u32;
    pub fn kernel_cpu_online_mask() -> usize;
    pub fn kernel_cpu_run_on(
        cpu: u32,
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
    );
}
