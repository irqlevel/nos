pub const TYPE_NET_RX: usize = 0;
pub const TYPE_BLK_IO: usize = 1;
pub const TYPE_NET_TX: usize = 2;
pub const TYPE_TCP_TIMER: usize = 3;
pub const MAX_TYPES: usize = 8;

/// Raise a soft IRQ type. Safe to call from a hard IRQ handler.
pub fn raise(typ: usize) {
    ffi::softirq::kernel_softirq_raise(typ)
}

/// Have `handler(target)` run for a soft IRQ type: in task context, on one
/// CPU at a time for a given type. Called once, before anything raises it.
/// See `MsixInterrupt::register_for` for what a target and a handler are.
pub fn register_for<T, F>(typ: usize, target: &'static T, handler: F)
where
    T: Sync + 'static,
    F: Fn(&'static T) + Copy + 'static,
{
    const { crate::callback::assert_stateless::<F>() };
    let _shown = handler;

    unsafe {
        ffi::softirq::kernel_softirq_register(
            typ, crate::callback::trampoline::<T, F>, crate::callback::ctx_of(target))
    }
}

/// Whether that soft IRQ is already asked for. What lets a poll tell a pass
/// it caused from one an interrupt caused.
pub fn is_pending(typ: usize) -> bool {
    ffi::softirq::kernel_softirq_pending(typ) != 0
}
