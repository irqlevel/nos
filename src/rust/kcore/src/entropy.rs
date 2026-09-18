//! Giving the kernel's random pool a source to reseed from.

use ffi::entropy;

/// A registered source. Registration is permanent (boot-lifetime): the
/// kernel's table has no way to take one back, so this has no Drop and a
/// driver keeps it only to say the registration happened.
pub struct EntropySource {
    handle: usize,
}

impl EntropySource {
    pub fn handle(&self) -> usize {
        self.handle
    }
}

/// What a source's name fits in, its terminator included.
const NAME_MAX: usize = 16;

/// Something the pool can ask for raw entropy: a hardware generator's
/// driver. It lives for good -- the pool's table has no way to give a source
/// back -- and is asked in task context, from a reseed, where it may be slow.
pub trait Source: Sync + 'static {
    /// Fill `buf`, or say it could not -- the pool then falls back on its
    /// other sources.
    fn fill(&'static self, buf: &mut [u8]) -> bool;
}

/// Register `source` with the pool under `name`. None when the table is full
/// or the name will not do.
pub fn register_source<S: Source>(name: &str, source: &'static S) -> Option<EntropySource> {
    if name.is_empty() || name.len() >= NAME_MAX || name.as_bytes().contains(&0) {
        return None;
    }

    /* The pool keeps the pointer it is given for as long as it keeps the
     * source, which is for good: so is this copy. */
    let mut c_name = alloc::vec::Vec::new();
    c_name.try_reserve_exact(name.len() + 1).ok()?;
    c_name.extend_from_slice(name.as_bytes());
    c_name.push(0);
    let c_name: &'static [u8] = c_name.leak();

    let handle = unsafe {
        entropy::kernel_entropy_source_register(
            c_name.as_ptr(), get_random::<S>, crate::callback::ctx_of(source))
    };
    if handle == 0 {
        None
    } else {
        Some(EntropySource { handle })
    }
}

extern "C" fn get_random<S: Source>(ctx: *mut u8, buf: *mut u8, len: usize) -> i32 {
    if buf.is_null() || len == 0 {
        return -1;
    }

    let source = unsafe { crate::callback::target_of::<S>(ctx) };
    /* The pool's own buffer, `len` bytes of it, for the length of the call. */
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    if source.fill(out) { 0 } else { -1 }
}
