extern "C" {
    fn kernel_panic(msg: *const u8, len: usize) -> !;
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
