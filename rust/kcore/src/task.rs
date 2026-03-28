use alloc::boxed::Box;
use crate::time::Duration;
use ffi::task;

struct RustSpawnCtx {
    f: fn(),
}

extern "C" fn rust_spawn_trampoline(ctx: *mut u8) {
    let b = unsafe { Box::from_raw(ctx.cast::<RustSpawnCtx>()) };
    (b.f)();
}

pub struct TaskHandle {
    handle: usize,
}

impl TaskHandle {
    pub fn wait(&self) {
        unsafe {
            task::kernel_task_wait(self.handle);
        }
    }

    pub fn request_stop(&self) {
        unsafe {
            task::kernel_task_set_stopping(self.handle);
        }
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe {
                task::kernel_task_wait(self.handle);
                task::kernel_task_put(self.handle);
            }
        }
    }
}

pub fn spawn(f: fn()) -> Option<TaskHandle> {
    let boxed = Box::new(RustSpawnCtx { f });
    let ptr = Box::into_raw(boxed).cast::<u8>();
    let h = unsafe { task::kernel_task_spawn(rust_spawn_trampoline, ptr) };
    if h == 0 {
        unsafe {
            drop(Box::from_raw(ptr.cast::<RustSpawnCtx>()));
        }
        return None;
    }
    Some(TaskHandle { handle: h })
}

pub fn sleep(dur: Duration) {
    unsafe {
        task::kernel_sleep_ns(dur.as_nanos());
    }
}

pub fn sleep_ms(ms: u64) {
    sleep(Duration::from_millis(ms));
}

pub fn cpu_id() -> u32 {
    unsafe { task::kernel_get_cpu_id() }
}
