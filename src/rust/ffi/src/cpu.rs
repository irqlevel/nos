unsafe extern "C" {
    pub safe fn kernel_get_cpu_id() -> u32;
    pub safe fn kernel_cpu_count() -> u32;
    pub safe fn kernel_cpu_online_mask() -> usize;
    pub fn kernel_cpu_run_on(
        cpu: u32,
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
    );
}

/* Interrupts and preemption off, and back on, for code that holds a lock of
   its own because it cannot allocate one. */
unsafe extern "C" {
    pub safe fn kernel_irq_save() -> usize;
    pub fn kernel_irq_restore(flags: usize);
}

/* Preemption off and back on, for a spin lock of the kernel's own kind. */
unsafe extern "C" {
    pub safe fn kernel_preempt_disable();
    pub fn kernel_preempt_enable();
    /// The same, for code that may run on a stack that is not a task's. The
    /// answer -- the task whose count went up, or 0 -- goes back to
    /// `kernel_preempt_enable_task`.
    pub safe fn kernel_preempt_disable_task() -> usize;
    pub fn kernel_preempt_enable_task(task: usize);
    /// Whether preemption is on at all yet: before it is, no task will ever
    /// be scheduled and work cannot be handed to one.
    pub safe fn kernel_preempt_is_on() -> i32;
    /// Whether the caller may wait: not with interrupts off, not off a task
    /// stack, and not with preemption disabled.
    pub safe fn kernel_preempt_can_block() -> i32;
    /// Whether interrupts are on for this CPU.
    pub safe fn kernel_interrupts_enabled() -> i32;
}
