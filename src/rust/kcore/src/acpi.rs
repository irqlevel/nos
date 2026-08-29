/// Returns true if the firmware describes a watchdog in the ACPI WDAT table.
/// The platform then expects the OS to drive that watchdog through the WDAT
/// instruction set, and native drivers must leave the hardware alone -- WDAT
/// usually describes the very same block they would grab.  Mirrors Linux's
/// acpi_has_watchdog(); see tco_wdt and the WDAT parser in drivers/acpi.cpp.
pub fn has_firmware_watchdog() -> bool {
    unsafe { ffi::acpi::kernel_acpi_has_firmware_watchdog() }
}
