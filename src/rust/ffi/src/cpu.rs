extern "C" {
    pub fn kernel_get_cpu_id() -> u32;
    pub fn kernel_cpu_count() -> u32;
    pub fn kernel_cpu_online_mask() -> usize;
    pub fn kernel_cpu_run_on(
        cpu: u32,
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
    );
}

/* Interrupts and preemption off, and back on, for code that holds a lock of
   its own because it cannot allocate one. */
extern "C" {
    pub fn kernel_irq_save() -> usize;
    pub fn kernel_irq_restore(flags: usize);
}

/* Preemption off and back on, for a spin lock of the kernel's own kind. */
extern "C" {
    pub fn kernel_preempt_disable();
    pub fn kernel_preempt_enable();
}
