extern "C" {
    fn kernel_panic(msg: *const u8, len: usize) -> !;
    /// Whether a panic has started -- so code writes without taking a lock
    /// another CPU may hold on its way to a halt.
    pub fn kernel_panic_active() -> i32;
}

pub fn panic_handler(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let mut buf = crate::trace::__TraceBuf::new();
    let _ = write!(buf, "{}", info);
    unsafe { kernel_panic(buf.as_str().as_ptr(), buf.as_str().len()) }
}

pub fn alloc_error() -> ! {
    unsafe { kernel_panic(b"alloc error".as_ptr(), 11) }
}
