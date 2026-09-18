extern "C" {
    pub fn kernel_softirq_raise(typ: usize);
    /// Whether that soft IRQ is already asked for.
    pub fn kernel_softirq_pending(typ: usize) -> i32;
    pub fn kernel_softirq_register(
        typ: usize,
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
    );
}
