use core::alloc::{GlobalAlloc, Layout};

extern "C" {
    fn kernel_alloc(size: usize, align: usize) -> *mut u8;
    fn kernel_free(ptr: *mut u8, size: usize, align: usize);
}

pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { kernel_alloc(layout.size(), layout.align()) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        /* align tells kernel_free whether the allocation was over-aligned
         * (align > 8) and carries a stashed original pointer */
        unsafe { kernel_free(ptr, layout.size(), layout.align()) }
    }
}
