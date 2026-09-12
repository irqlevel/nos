pub const MAX_CPUS: usize = 8;

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
