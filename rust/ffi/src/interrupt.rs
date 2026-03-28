extern "C" {
    pub fn kernel_interrupt_register_level(
        irq_line: u8,
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
        out_vector: *mut u8,
    ) -> usize;
    pub fn kernel_interrupt_unregister(handle: usize);
}
