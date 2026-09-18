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

/// Register a source of raw entropy.
///
/// `name` must be NUL-terminated and live as long as the kernel; so must
/// `ctx`. `get_random(ctx, buf, len)` fills the buffer and answers 0, or
/// anything else if it could not -- the pool then falls back on its other
/// sources. It is called in task context and may be slow.
pub fn register(
    name: *const u8,
    get_random: extern "C" fn(ctx: *mut u8, buf: *mut u8, len: usize) -> i32,
    ctx: *mut u8,
) -> Option<EntropySource> {
    let handle = unsafe { entropy::kernel_entropy_source_register(name, get_random, ctx) };
    if handle == 0 {
        None
    } else {
        Some(EntropySource { handle })
    }
}
