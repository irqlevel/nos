#[repr(C)]
pub struct PciDeviceInfo {
    pub bus: u16,
    pub slot: u16,
    pub func: u16,
    pub vendor: u16,
    pub device: u16,
    pub cls: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub irq_line: u8,
    pub irq_pin: u8,
}

unsafe extern "C" {
    pub fn kernel_pci_find_device(
        vendor: u16, device: u16, start_index: usize,
        out: *mut PciDeviceInfo,
    ) -> isize;
    pub safe fn kernel_pci_device_count() -> usize;
    pub fn kernel_pci_get_device(index: usize, out: *mut PciDeviceInfo) -> i32;
    pub fn kernel_pci_get_bar(bus: u16, slot: u16, func: u16, bar: u8) -> u32;
    pub fn kernel_pci_enable_bus_mastering(bus: u16, slot: u16, func: u16);
    pub fn kernel_pci_find_capability(
        bus: u16, slot: u16, func: u16,
        cap_id: u8, start_offset: u8,
    ) -> u8;
    pub fn kernel_pci_read_config8(bus: u16, slot: u16, func: u16, offset: u16) -> u8;
    pub fn kernel_pci_read_config16(bus: u16, slot: u16, func: u16, offset: u16) -> u16;
    pub fn kernel_pci_read_config32(bus: u16, slot: u16, func: u16, offset: u16) -> u32;
    pub fn kernel_pci_write_config8(bus: u16, slot: u16, func: u16, offset: u16, val: u8);
    pub fn kernel_pci_write_config16(bus: u16, slot: u16, func: u16, offset: u16, val: u16);
    pub fn kernel_pci_write_config32(bus: u16, slot: u16, func: u16, offset: u16, val: u32);
    /// The screen's drawing held (`on` non-zero) or taken up again (0) --
    /// a count, under the console's lock -- while a function about to be
    /// given to a guest has its BARs sized, its decoding off: the firmware's
    /// framebuffer may be in one. Sound for any value, at any time; kcore
    /// pairs the two.
    pub safe fn kernel_screen_hold(on: i32);
    /// The screen's framebuffer let go of for good where it lies in
    /// `[base, base + size)`, a BAR of such a function: 1 when it did.
    /// Values compared under the console's lock: sound for any, at any time.
    pub safe fn kernel_screen_release(base: u64, size: u64) -> i32;
}
