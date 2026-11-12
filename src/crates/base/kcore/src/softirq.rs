pub const TYPE_NET_RX: usize = 0;
pub const TYPE_BLK_IO: usize = 1;
pub const TYPE_NET_TX: usize = 2;
pub const MAX_TYPES: usize = 8;

/// Raise a soft IRQ type. Safe to call from a hard IRQ handler. One past
/// the types is none.
pub fn raise(typ: usize) {
    if typ < MAX_TYPES {
        raise_mask(1 << typ);
    }
}

/// Raise every type `mask` has a bit of (`1 << TYPE_*`) at once: one wake
/// and at most one kick of this CPU's softirq task for all of them, where a
/// `raise` each kicks it once each -- an IPI to this CPU each. What an
/// interrupt with work for more than one type -- a NIC's, for both its
/// queues -- raises them by. Safe to call from a hard IRQ handler.
pub fn raise_mask(mask: usize) {
    ffi::softirq::kernel_softirq_raise_mask(mask)
}

/// Have `handler(target)` run for a soft IRQ type: in task context, on one
/// CPU at a time for a given type. False, and nothing changed, for a type
/// past the types or one that has a handler already -- the kernel publishes
/// the pair whole, once, so no pass runs one's function with another's
/// target. See `MsixInterrupt::register_for` for what a target and a
/// handler are.
#[must_use]
pub fn register_for<T, F>(typ: usize, target: &'static T, handler: F) -> bool
where
    T: Sync + 'static,
    F: Fn(&'static T) + Copy + 'static,
{
    const { crate::callback::assert_stateless::<F>() };
    let _shown = handler;

    unsafe {
        ffi::softirq::kernel_softirq_register(
            typ, crate::callback::trampoline::<T, F>, crate::callback::ctx_of(target)) == 1
    }
}

/// Whether that soft IRQ is already asked for. What lets a poll tell a pass
/// it caused from one an interrupt caused.
pub fn is_pending(typ: usize) -> bool {
    ffi::softirq::kernel_softirq_pending(typ) != 0
}
