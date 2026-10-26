//! Being called back by the kernel, without a `ctx: *mut u8` at the call
//! site.
//!
//! Everything the kernel calls back -- an interrupt handler, a timer, a soft
//! IRQ, a driver's entry points -- is a C function and a word to hand it.
//! Written by hand that is a cast of the device to a pointer where it is
//! registered and a cast back, under `unsafe`, in every handler -- and a
//! device kept as a `*mut` throughout, because that is the shape the casts
//! leave it in.
//!
//! What is called back here is a *target* that lives for good -- a device is
//! registered for the life of the kernel -- and a *handler* that is a
//! function item, or a closure that captures nothing. Such a handler has no
//! bytes: its type alone says what to call. So the word the kernel carries
//! can be the target itself, the C function can be stamped out per handler
//! type, and neither side of the registration sees a pointer.

/// That `F` is a function item or a captureless closure -- a fn *pointer*,
/// or a closure with captures, has bytes, and is refused when the kernel is
/// built. Used as `const { assert_stateless::<F>() }`.
pub(crate) const fn assert_stateless<F>() {
    assert!(
        core::mem::size_of::<F>() == 0,
        "a handler must be a function item or a closure that captures nothing",
    );
}

/// The target, as the word the kernel hands back.
pub(crate) fn ctx_of<T: Sync + 'static>(target: &'static T) -> *mut u8 {
    target as *const T as *mut u8
}

/// # Safety
/// `ctx` is what `ctx_of::<T>` made of a `&'static T`.
pub(crate) unsafe fn target_of<T: Sync + 'static>(ctx: *mut u8) -> &'static T {
    unsafe { &*(ctx as *const T) }
}

/// Another of a handler that was shown when it was registered.
///
/// # Safety
/// `assert_stateless::<F>()` holds, and whoever registered the handler held
/// a value of `F`: it is `Copy`, and a copy of something with no bytes is
/// exactly this.
pub(crate) unsafe fn handler<F: Copy>() -> F {
    unsafe { core::mem::MaybeUninit::<F>::uninit().assume_init() }
}

/// What the kernel is given to call: `handler(target)`.
pub(crate) extern "C" fn trampoline<T, F>(ctx: *mut u8)
where
    T: Sync + 'static,
    F: Fn(&'static T) + Copy + 'static,
{
    /* Registered by a `*_for` function of this crate, which checked `F`,
     * was shown one, and passed `ctx_of` its target. */
    let handler = unsafe { handler::<F>() };
    handler(unsafe { target_of::<T>(ctx) });
}
