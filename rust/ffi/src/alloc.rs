use core::alloc::{GlobalAlloc, Layout};

extern "C" {
    fn kernel_alloc(size: usize, align: usize) -> *mut u8;
    fn kernel_free(ptr: *mut u8);
}

pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { kernel_alloc(layout.size(), layout.align()) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        unsafe { kernel_free(ptr) }
    }
}
