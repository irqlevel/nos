use crate::time::Duration;

pub struct Timer {
    handle: usize,
}

impl Timer {
    /// Start a periodic timer. `handler(ctx)` is called every `period` from IPI context on CPU 0.
    /// Returns None if no timer slots are available or period is zero.
    pub fn start(
        period: Duration,
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
    ) -> Option<Self> {
        let h = unsafe {
            ffi::timer::kernel_timer_start(handler, ctx, period.as_nanos())
        };
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    /// Stop the timer explicitly before it is dropped.
    pub fn stop(self) {
        /* Drop handles the call. */
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { ffi::timer::kernel_timer_stop(self.handle) }
        }
    }
}
