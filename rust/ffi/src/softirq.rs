extern "C" {
    pub fn kernel_softirq_raise(typ: usize);
    pub fn kernel_softirq_register(
        typ: usize,
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
    );
}
