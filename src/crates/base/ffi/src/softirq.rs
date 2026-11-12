unsafe extern "C" {
    /// Every type `mask` has a bit of (1 << type) raised at once: one wake
    /// and at most one kick of this CPU's softirq task. A bit past the types
    /// is none.
    pub safe fn kernel_softirq_raise_mask(mask: usize);
    /// Whether that soft IRQ is already asked for.
    pub safe fn kernel_softirq_pending(typ: usize) -> i32;
    /// 1 once `handler(ctx)` is the type's, 0 -- nothing changed -- for a
    /// type past the types or one that has a handler already.
    pub fn kernel_softirq_register(
        typ: usize,
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
    ) -> i32;
}
