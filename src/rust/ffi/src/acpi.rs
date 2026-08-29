extern "C" {
    pub fn kernel_hpet_read_ns() -> u64;
    pub fn kernel_hpet_is_available() -> bool;
    pub fn kernel_acpi_has_firmware_watchdog() -> bool;
}
