use ffi::random;

pub fn fill_random(buf: &mut [u8]) -> bool {
    if buf.is_empty() {
        return false;
    }
    unsafe { random::kernel_get_random(buf.as_mut_ptr(), buf.len()) != 0 }
}

pub fn random_u64() -> Option<u64> {
    let mut bytes = [0u8; core::mem::size_of::<u64>()];
    if fill_random(&mut bytes) {
        /* The bytes the pool wrote, as they lie: what writing through a
         * pointer to a u64 gave. */
        Some(u64::from_ne_bytes(bytes))
    } else {
        None
    }
}
