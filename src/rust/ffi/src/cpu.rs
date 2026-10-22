unsafe extern "C" {
    pub safe fn kernel_get_cpu_id() -> u32;
    pub safe fn kernel_cpu_count() -> u32;
    pub safe fn kernel_cpu_online_mask() -> usize;
    pub fn kernel_cpu_run_on(
        cpu: u32,
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
    );
    /// An interrupt to `cpu`, now, waiting for nothing: from any context,
    /// interrupts off included. What makes a vCPU running a guest there
    /// leave it; any other CPU takes an interrupt with nothing to do.
    pub safe fn kernel_cpu_kick(cpu: u32);
    /// The posted-interrupt notification to `cpu`, from any context: a vCPU
    /// in its guest there takes what was posted for it without an exit;
    /// out of one it is an interrupt with nothing to do.
    pub safe fn kernel_cpu_notify(cpu: u32);
    /// The vector `kernel_cpu_notify` sends.
    pub safe fn kernel_posted_interrupt_vector() -> u8;
    /// The CPU this runs on to take an interrupt from its own timer by
    /// `deadline_ns` on the boot clock, if it is not armed to fire sooner;
    /// nothing for a deadline past. From any context.
    pub safe fn kernel_cpu_timer_at(deadline_ns: u64);
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
