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

pub fn spawn_on(affinity_mask: u64, f: fn()) -> Option<TaskHandle> {
    let boxed = Box::new(RustSpawnCtx { f });
    let ptr = Box::into_raw(boxed).cast::<u8>();
    let h = unsafe {
        task::kernel_task_spawn_on(rust_spawn_trampoline, ptr, affinity_mask as usize)
    };
    if h == 0 {
        unsafe { drop(Box::from_raw(ptr.cast::<RustSpawnCtx>())); }
        return None;
    }
    Some(TaskHandle { handle: h })
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

/// Spawn a task that receives a raw context pointer.
/// The caller is responsible for the lifetime and safety of `ctx`.
pub fn spawn_with_ctx(
    func: extern "C" fn(*mut u8), ctx: *mut u8,
) -> Option<TaskHandle> {
    let h = unsafe { task::kernel_task_spawn_ctx(func, ctx) };
    if h == 0 { None } else { Some(TaskHandle { handle: h }) }
}

/// Spawn a task with a raw context pointer, bound to `affinity_mask` CPUs.
pub fn spawn_on_with_ctx(
    affinity_mask: u64,
    func: extern "C" fn(*mut u8), ctx: *mut u8,
) -> Option<TaskHandle> {
    let h = unsafe {
        task::kernel_task_spawn_on_ctx(func, ctx, affinity_mask as usize)
    };
    if h == 0 { None } else { Some(TaskHandle { handle: h }) }
}

pub fn cpu_id() -> u32 {
    crate::cpu::id()
}
