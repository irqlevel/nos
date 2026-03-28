extern "C" {
    pub fn kernel_alloc_dma_pages(
        count: usize,
        phys_out: *mut u64,
        actual_pages_out: *mut usize,
    ) -> *mut u8;
    pub fn kernel_free_dma_pages(ptr: *mut u8);
    pub fn kernel_map_phys(phys_base: u64, num_pages: usize) -> *mut u8;
    pub fn kernel_unmap_phys(virt_addr: *mut u8, num_pages: usize);
}
