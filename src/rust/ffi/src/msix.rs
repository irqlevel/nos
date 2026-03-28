extern "C" {
    pub fn kernel_msix_create(
        bus: u16, slot: u16, func: u16,
        mapped_bars: *const u64,
    ) -> usize;
    pub fn kernel_msix_destroy(handle: usize);
    pub fn kernel_msix_enable_vector(
        handle: usize, index: u16,
        isr_fn: unsafe extern "C" fn(),
    ) -> u8;
    pub fn kernel_msix_mask(handle: usize, index: u16);
    pub fn kernel_msix_unmask(handle: usize, index: u16);
    pub fn kernel_msix_table_size(handle: usize) -> u16;
    pub fn kernel_msix_is_ready(handle: usize) -> i32;
    pub fn kernel_msix_register_handler(
        msix_handle: usize, msix_index: u16,
        handler: extern "C" fn(*mut u8), ctx: *mut u8,
        out_vector: *mut u8,
    ) -> usize;
    pub fn kernel_msix_unregister_handler(handle: usize);
}
