#![no_std]

extern crate alloc;
use alloc::vec;
use core::fmt::Write;

pub fn hello() {
    ffi::trace::trace(0, "Hello from Rust!");

    let v = vec![1u32, 2, 3];
    let sum: u32 = v.iter().sum();

    let mut buf = ffi::trace::__TraceBuf::new();
    let _ = write!(buf, "Rust alloc works, vec sum = {}", sum);
    ffi::trace::trace(0, buf.as_str());
}
