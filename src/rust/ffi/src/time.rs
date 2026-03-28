extern "C" {
    pub fn kernel_get_boot_time(secs: *mut u64, usecs: *mut u64);
    pub fn kernel_get_wall_time_secs() -> u64;
}
