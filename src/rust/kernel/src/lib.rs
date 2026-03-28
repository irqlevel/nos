#![no_std]
#![feature(alloc_error_handler)]

extern crate alloc;

use ffi::alloc::KernelAllocator;

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    ffi::panic::panic_handler(info)
}

#[alloc_error_handler]
fn alloc_error(_layout: core::alloc::Layout) -> ! {
    ffi::panic::alloc_error()
}

#[no_mangle]
pub extern "C" fn rust_init() {
    hello::hello();
}

#[no_mangle]
pub extern "C" fn rust_test() {
    hello::test();
}
