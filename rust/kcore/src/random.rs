use core::mem::MaybeUninit;
use ffi::random;

pub fn fill_random(buf: &mut [u8]) -> bool {
    if buf.is_empty() {
        return false;
    }
    unsafe { random::kernel_get_random(buf.as_mut_ptr(), buf.len()) != 0 }
}

pub fn random_u64() -> Option<u64> {
    let mut v = MaybeUninit::<u64>::uninit();
    let b = unsafe {
        random::kernel_get_random(v.as_mut_ptr().cast::<u8>(), core::mem::size_of::<u64>())
    };
    if b != 0 {
        Some(unsafe { v.assume_init() })
    } else {
        None
    }
}
