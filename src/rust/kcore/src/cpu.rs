/* The most CPUs the kernel tracks is `consts::MAX_CPUS`, which is what a
 * per-CPU array is sized by. There used to be a second one here, at 8,
 * which no longer matched the kernel and which nothing used. */

/// Returns the logical index of the current CPU.
#[inline]
pub fn id() -> u32 {
    ffi::cpu::kernel_get_cpu_id()
}

/// Returns the number of CPUs currently in the running state.
pub fn count() -> u32 {
    ffi::cpu::kernel_cpu_count()
}

/// Returns a bitmask of running CPUs (bit i = CPU i is running).
pub fn online_mask() -> u64 {
    ffi::cpu::kernel_cpu_online_mask() as u64
}

/// Run `handler(ctx)` synchronously on the given CPU via IPI.
/// Blocks the calling CPU until the remote CPU completes the call.
/// Has no effect if `cpu` is out of range or `cpu` is not running.
pub fn run_on(cpu: u32, handler: extern "C" fn(*mut u8), ctx: *mut u8) {
    unsafe { ffi::cpu::kernel_cpu_run_on(cpu, handler, ctx) }
}

/// Run `f(arg)` on `cpu` and return once it has: `run_on` with the context a
/// reference the compiler checks rather than a word every call site casts
/// back by hand.
///
/// `arg` may be the caller's own stack, because `run_on` does not return
/// until the handler has run -- so the borrow outlives the call by
/// construction, and there is nothing to keep alive afterwards. `f` runs in
/// interrupt context on the far CPU: registers, atomics and IRQ-safe locks,
/// no sleeping and no allocating. Whatever it has to say comes back through
/// an atomic in `arg`.
pub fn run_on_with<T: Sync>(cpu: u32, arg: &T, f: fn(&T)) {
    struct Call<'a, T> {
        arg: &'a T,
        f: fn(&T),
    }

    extern "C" fn trampoline<T: Sync>(raw: *mut u8) {
        /* The one made below, and reached only through the `run_on` this
         * function makes: it is alive for as long as that call. */
        let call = unsafe { &*(raw as *const Call<'_, T>) };
        (call.f)(call.arg);
    }

    let call = Call { arg, f };
    run_on(cpu, trampoline::<T>, &call as *const Call<'_, T> as *mut u8);
}

/// An interrupt to `cpu`, now, waiting for nothing -- `run_on`'s opposite:
/// from any context, interrupts off included, and nothing runs there but
/// the interrupt's own handler. What has a vCPU running a guest on that CPU
/// leave it, a physical interrupt ending the guest's turn. Nothing for a
/// `cpu` out of range.
#[inline]
pub fn kick(cpu: u32) {
    ffi::cpu::kernel_cpu_kick(cpu)
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
    ffi::cpu::kernel_irq_save()
}

/// # Safety
/// `flags` came from `irq_save` on this CPU, and nothing since has restored
/// them.
pub unsafe fn irq_restore(flags: usize) {
    unsafe { ffi::cpu::kernel_irq_restore(flags) }
}

/// `work`, with interrupts and preemption off on this CPU for the whole of
/// it, handed the CPU that is.
///
/// The scoped form of `irq_save` and `irq_restore`, and the reason it is safe
/// where the pair is not: the flags never leave this function, so they
/// cannot be given back twice, on another CPU, or out of order with another
/// pair. For a step that has to find a CPU's state as it needs it and then
/// act on it with nothing in between -- no IPI to change the state, no
/// migration to change the CPU -- such as checking that a CPU's
/// virtualization extension is on and entering a guest there.
pub fn with_interrupts_off<R>(work: impl FnOnce(u32) -> R) -> R {
    let flags = irq_save();
    let result = work(id());
    /* The flags `irq_save` returned above, on this CPU: with interrupts and
     * preemption off, nothing moved this task elsewhere in between. */
    unsafe { irq_restore(flags) };
    result
}

/// Whether interrupts are on for this CPU.
///
/// What tells a caller it may release something whose free waits for every
/// other CPU to answer: with interrupts off it could not answer one itself,
/// and two such CPUs would wait on each other for good.
pub fn interrupts_enabled() -> bool {
    ffi::cpu::kernel_interrupts_enabled() != 0
}

/// Preemption off for the calling task, and the task it was taken on -- or
/// 0, when the stack is not a task's and there was nothing to raise. The
/// answer goes back to `preempt_enable_task`, because the count belongs to
/// the task that took it. Unlike the plain pair this is safe from the
/// tracer's side and from an AP on its way up.
#[inline]
pub fn preempt_disable_task() -> usize {
    ffi::cpu::kernel_preempt_disable_task()
}

#[inline]
pub fn preempt_enable_task(task: usize) {
    unsafe { ffi::cpu::kernel_preempt_enable_task(task) }
}

/// Whether preemption is on at all yet. Before it is, no task will ever be
/// scheduled: work cannot be handed to one, and whoever has it does it.
#[inline]
pub fn preempt_is_on() -> bool {
    ffi::cpu::kernel_preempt_is_on() != 0
}

/// Whether the caller may wait -- for a completion, or to be scheduled away.
/// Not with interrupts off, not off a task stack, and not under a spinlock.
#[inline]
pub fn preempt_can_block() -> bool {
    ffi::cpu::kernel_preempt_can_block() != 0
}
