/* The most CPUs the kernel tracks is `consts::MAX_CPUS`, which is what a
 * per-CPU array is sized by. There used to be a second one here, at 8,
 * which no longer matched the kernel and which nothing used. */

/// Returns the logical index of the current CPU.
pub fn id() -> u32 {
    unsafe { ffi::cpu::kernel_get_cpu_id() }
}

/// Returns the number of CPUs currently in the running state.
pub fn count() -> u32 {
    unsafe { ffi::cpu::kernel_cpu_count() }
}

/// Returns a bitmask of running CPUs (bit i = CPU i is running).
pub fn online_mask() -> u64 {
    unsafe { ffi::cpu::kernel_cpu_online_mask() as u64 }
}

/// Run `handler(ctx)` synchronously on the given CPU via IPI.
/// Blocks the calling CPU until the remote CPU completes the call.
/// Has no effect if `cpu` is out of range or `cpu` is not running.
pub fn run_on(cpu: u32, handler: extern "C" fn(*mut u8), ctx: *mut u8) {
    unsafe { ffi::cpu::kernel_cpu_run_on(cpu, handler, ctx) }
}

extern "C" fn nothing(_ctx: *mut u8) {}

/// Returns once every running CPU has taken an interrupt since the call, so
/// an interrupt handler running anywhere when it was called has returned by
/// then: a handler runs with interrupts off, and the IPI this sends each CPU
/// is only taken after it. What makes it safe to free what a device's
/// completion callback may still have been touching on its way out. Task
/// context: it waits.
pub fn synchronize() {
    let mask = online_mask();
    for cpu in 0..u64::BITS {
        if mask & (1u64 << cpu) != 0 {
            run_on(cpu, nothing, core::ptr::null_mut());
        }
    }
}

/// Interrupts and preemption off until `irq_restore`, and the flags to give
/// it back with.
///
/// For code that owns a per-CPU structure and needs nothing else: there is
/// no lock to take, because nothing else touches that CPU's slot. The order
/// matters -- reading the CPU id first and disabling after leaves a window
/// in which this task is preempted onto another CPU, and then two CPUs are
/// inside one per-CPU structure, which is not a per-CPU structure at all.
pub fn irq_save() -> usize {
    unsafe { ffi::cpu::kernel_irq_save() }
}

/// # Safety
/// `flags` came from `irq_save` on this CPU, and nothing since has restored
/// them.
pub unsafe fn irq_restore(flags: usize) {
    unsafe { ffi::cpu::kernel_irq_restore(flags) }
}

/// Whether interrupts are on for this CPU.
///
/// What tells a caller it may release something whose free waits for every
/// other CPU to answer: with interrupts off it could not answer one itself,
/// and two such CPUs would wait on each other for good.
pub fn interrupts_enabled() -> bool {
    unsafe { ffi::cpu::kernel_interrupts_enabled() != 0 }
}
