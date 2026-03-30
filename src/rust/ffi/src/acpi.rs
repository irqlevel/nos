extern "C" {
    pub fn kernel_hpet_read_ns() -> u64;
    pub fn kernel_hpet_is_available() -> bool;
}
