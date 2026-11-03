pub use ffi::klog::klog;
pub use ffi::klog::__KlogBuf;

/// Whether a panic has started. What tells code to stop waiting for locks:
/// another CPU may hold one and is on its way to a halt.
pub fn panic_active() -> bool {
    ffi::panic::kernel_panic_active() != 0
}
