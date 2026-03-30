/// Returns true if the HPET is available (ACPI HPET table found and MMIO mapped).
pub fn is_available() -> bool {
    unsafe { ffi::acpi::kernel_hpet_is_available() }
}

/// Read the HPET monotonic counter converted to nanoseconds.
/// Returns 0 if the HPET is not available.
pub fn read_ns() -> u64 {
    unsafe { ffi::acpi::kernel_hpet_read_ns() }
}
