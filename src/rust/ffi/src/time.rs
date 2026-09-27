unsafe extern "C" {
    pub fn kernel_get_boot_time(secs: *mut u64, usecs: *mut u64);
    pub safe fn kernel_get_wall_time_secs() -> u64;
    /// Nanoseconds since boot, at the clock's full resolution
    pub safe fn kernel_get_boot_time_ns() -> u64;
    /// The cycle counter's rate in Hz -- the TSC's on x86-64, the generic
    /// timer's on arm64 -- or 0 while it is not known
    pub safe fn kernel_cycle_counter_hz() -> u64;
}
