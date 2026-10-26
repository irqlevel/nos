use crate::callback;
use crate::time::Duration;

pub struct Timer {
    handle: usize,
}

impl Timer {
    /// Start a periodic timer: `handler(target)` is called every `period`,
    /// from IPI context on CPU 0. See `MsixInterrupt::register_for` for what
    /// a target and a handler are. None if no timer slots are available or
    /// the period is zero.
    pub fn start_for<T, F>(period: Duration, target: &'static T, handler: F) -> Option<Self>
    where
        T: Sync + 'static,
        F: Fn(&'static T) + Copy + 'static,
    {
        const { callback::assert_stateless::<F>() };
        let _shown = handler;

        let h = unsafe {
            ffi::timer::kernel_timer_start(
                callback::trampoline::<T, F>, callback::ctx_of(target), period.as_nanos())
        };
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    /// Stop the timer explicitly before it is dropped.
    pub fn stop(self) {
        /* Drop handles the call. */
    }

    /// Leak the handle so the timer keeps firing for the kernel's lifetime.
    /// Dropping the returned `Timer` would stop it, so callers that want a
    /// permanent periodic timer must call this (or store the handle).
    pub fn leak(self) {
        core::mem::forget(self);
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { ffi::timer::kernel_timer_stop(self.handle) }
        }
    }
}
