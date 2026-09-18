pub const TYPE_NET_RX: usize = 0;
pub const TYPE_BLK_IO: usize = 1;
pub const TYPE_NET_TX: usize = 2;
pub const TYPE_TCP_TIMER: usize = 3;
pub const MAX_TYPES: usize = 8;

/// Raise a soft IRQ type. Safe to call from a hard IRQ handler.
pub fn raise(typ: usize) {
    unsafe { ffi::softirq::kernel_softirq_raise(typ) }
}

/// Register a handler for a soft IRQ type.
/// Called once during driver init, before the softirq task starts processing.
pub fn register(typ: usize, handler: extern "C" fn(*mut u8), ctx: *mut u8) {
    unsafe { ffi::softirq::kernel_softirq_register(typ, handler, ctx) }
}

/// Whether that soft IRQ is already asked for. What lets a poll tell a pass
/// it caused from one an interrupt caused.
pub fn is_pending(typ: usize) -> bool {
    unsafe { ffi::softirq::kernel_softirq_pending(typ) != 0 }
}
