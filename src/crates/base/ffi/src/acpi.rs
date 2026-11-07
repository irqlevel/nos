unsafe extern "C" {
    pub safe fn kernel_hpet_read_ns() -> u64;
    pub safe fn kernel_hpet_is_available() -> bool;
    pub safe fn kernel_acpi_has_firmware_watchdog() -> bool;
}
