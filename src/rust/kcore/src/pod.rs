//! Structures as they lie on a disk or a wire: read out of a buffer of bytes
//! and written back into one.
//!
//! Doing that is two raw pointer casts, and whether they are sound depends on
//! the *type* -- that any bytes at all make a valid one, and that it has no
//! padding to leak -- not on the place it is done. So the type says so, once,
//! with `unsafe impl Pod`, and every read and write of it after that is an
//! ordinary bounds-checked call.

/// Plain old data.
///
/// # Safety
/// The type is `#[repr(C)]`, made of integers and arrays of them and nothing
/// else, so that every bit pattern is a valid value; and it has no padding,
/// between fields or at the end, so that every byte of a value is
/// initialised. A `const` assertion of its size against the sum of its
/// fields is the way to be sure of the second.
pub unsafe trait Pod: Copy + 'static {}

/// A value with every byte zero.
pub fn zeroed<T: Pod>() -> T {
    /* Every bit pattern is a valid `T`; all zeros is one. */
    unsafe { core::mem::zeroed() }
}

/// The bytes of `value`, every one of them: a `T` has no padding, so all of
/// them are initialised.
pub fn bytes_of<T: Pod>(value: &T) -> &[u8] {
    /* A `T` is exactly `size_of::<T>()` initialised bytes, and a `u8` has
     * no alignment to keep. */
    unsafe { core::slice::from_raw_parts(value as *const T as *const u8, core::mem::size_of::<T>()) }
}

/// The bytes of `value`, to be written: whatever goes into them is still a
/// `T`, since any bytes are one.
pub fn bytes_of_mut<T: Pod>(value: &mut T) -> &mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(value as *mut T as *mut u8, core::mem::size_of::<T>()) }
}

/// The `T` at `off` in `buf`, wherever that falls: nothing is assumed of its
/// alignment. None when the buffer ends before the value does.
pub fn read<T: Pod>(buf: &[u8], off: usize) -> Option<T> {
    let bytes = buf.get(off..off.checked_add(core::mem::size_of::<T>())?)?;
    /* `bytes` is exactly a `T` long, and any bytes are a `T`. */
    Some(unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const T) })
}

/// `value` into `buf` at `off`. False, and nothing written, when it does not
/// fit.
pub fn write<T: Pod>(buf: &mut [u8], off: usize, value: &T) -> bool {
    let end = match off.checked_add(core::mem::size_of::<T>()) {
        Some(end) => end,
        None => return false,
    };
    match buf.get_mut(off..end) {
        Some(bytes) => {
            /* `bytes` is exactly a `T` long, and a `T` has no padding. */
            unsafe { core::ptr::write_unaligned(bytes.as_mut_ptr() as *mut T, *value) };
            true
        }
        None => false,
    }
}

/* The integers themselves: what a descriptor's fields are. */
unsafe impl Pod for u8 {}
unsafe impl Pod for u16 {}
unsafe impl Pod for u32 {}
unsafe impl Pod for u64 {}
