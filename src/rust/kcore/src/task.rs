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

    /// The task, as `current_id` names it from inside.
    pub fn id(&self) -> usize {
        self.handle
    }
}

/// The calling task: what `TaskHandle::id` says for it. For telling
/// whether a call comes from one of a server's own tasks -- which must not
/// wait for itself.
pub fn current_id() -> usize {
    unsafe { task::kernel_task_current() }
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

/// Gives the CPU to another task runnable on it, if there is one, and
/// returns at once if there is not -- never to the idle task, which would
/// halt the CPU until the next interrupt the CPU itself takes, often the
/// tick, 10 ms on. The way to poll for work another CPU's interrupt will
/// bring, without keeping whatever else is runnable here -- a softirq task
/// the tick preempted mid-handler among them -- off the CPU the way a plain
/// spin does.
#[inline]
pub fn yield_to_runnable() {
    unsafe { task::kernel_task_yield_to_runnable() }
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
