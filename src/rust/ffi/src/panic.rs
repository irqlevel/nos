unsafe extern "C" {
    fn kernel_panic(msg: *const u8, len: usize) -> !;
    /// Whether a panic has started -- so code writes without taking a lock
    /// another CPU may hold on its way to a halt.
    pub safe fn kernel_panic_active() -> i32;
}

pub fn panic_handler(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let mut buf = crate::trace::__TraceBuf::new();
    let _ = write!(buf, "{}", info);
    unsafe { kernel_panic(buf.as_str().as_ptr(), buf.as_str().len()) }
}

/// The heap handed back null to an allocation that cannot fail -- a
/// `Box::new`, a `format!`, a `push` past capacity. The request's size and
/// alignment are what tell its refusals apart: a request past the heap's
/// largest block (`PageTable::MaxContiguousPages` pages) is refused however
/// much memory is free, one whose size class has used up its share of the
/// address space is refused with memory to spare, and neither reads like a
/// machine that has really run out. Formatted on the stack, as the panic
/// handler's message is: nothing on this path may allocate.
pub fn alloc_error(layout: core::alloc::Layout) -> ! {
    use core::fmt::Write;
    let mut buf = crate::trace::__TraceBuf::new();
    let _ = write!(buf, "alloc error: {} bytes, align {}", layout.size(), layout.align());
    unsafe { kernel_panic(buf.as_str().as_ptr(), buf.as_str().len()) }
}
