pub use ffi::trace::trace;
pub use ffi::trace::__TraceBuf;

/// Whether a panic has started. What tells code to stop waiting for locks:
/// another CPU may hold one and is on its way to a halt.
pub fn panic_active() -> bool {
    unsafe { ffi::panic::kernel_panic_active() != 0 }
}
