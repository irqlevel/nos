extern "C" {
    pub fn kernel_get_random(buf: *mut u8, len: usize) -> i32;
}
