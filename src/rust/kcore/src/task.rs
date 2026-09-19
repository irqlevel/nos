use alloc::boxed::Box;
use crate::time::Duration;
use ffi::task;

/// What a spawned task starts from: boxed on its way through the kernel,
/// which carries it as a word.
struct Start<C> {
    ctx: C,
    entry: fn(C),
}

/// A task that is handed `ctx` by value, on every CPU `affinity` names (any,
/// when it names none). The one place a start is boxed, and the one place it
/// is unboxed: in the task, or here if no task was made to hand it to. `ctx`
/// is the task's from then on, and dropped when `entry` returns.
fn spawn_owned<C: Send + 'static>(
    name: &str, affinity: Option<u64>, ctx: C, entry: fn(C),
) -> Option<TaskHandle> {
    extern "C" fn trampoline<C>(raw: *mut u8) {
        /* Made below and handed over exactly once: to this task. */
        let start = unsafe { Box::from_raw(raw.cast::<Start<C>>()) };
        let Start { ctx, entry } = *start;
        entry(ctx);
    }

    let raw = Box::into_raw(Box::new(Start { ctx, entry })).cast::<u8>();
    let h = unsafe {
        match affinity {
            None => task::kernel_task_spawn(name.as_ptr(), name.len(), trampoline::<C>, raw),
            Some(mask) => task::kernel_task_spawn_on(
                name.as_ptr(), name.len(), trampoline::<C>, raw, mask as usize),
        }
    };
    if h == 0 {
        /* No task to hand it to: still this function's. */
        drop(unsafe { Box::from_raw(raw.cast::<Start<C>>()) });
        return None;
    }
    Some(TaskHandle { handle: h })
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

/// Whether the calling task has been asked to stop -- what a loop that runs
/// for the life of the kernel checks each time round, so that a shutdown or
/// a `stop` gets it out rather than leaving it sleeping.
pub fn stopping() -> bool {
    task::kernel_task_stopping() != 0
}

/// The calling task: what `TaskHandle::id` says for it. For telling
/// whether a call comes from one of a server's own tasks -- which must not
/// wait for itself.
pub fn current_id() -> usize {
    task::kernel_task_current()
}

/// The calling task, or 0 when the stack is not a task's. For code that runs
/// wherever it is called from -- an interrupt handler, the tracer, the panic
/// path -- where asking the other way would itself complain, and a complaint
/// traces.
pub fn current_id_or_none() -> usize {
    task::kernel_task_current_or_none()
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

/// Spawn a task running `f`. `name`, which every spawn here takes, is what
/// `ps` and `top` show for the task -- cut to the 31 bytes a task has room
/// for.
pub fn spawn(name: &str, f: fn()) -> Option<TaskHandle> {
    spawn_owned(name, None, f, |f| f())
}

pub fn sleep(dur: Duration) {
    task::kernel_sleep_ns(dur.as_nanos());
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
    task::kernel_task_yield_to_runnable()
}

/// Spawn a task that runs `entry` over something that lives for good -- a
/// service's one instance, a `static`. This is what almost every task in the
/// kernel is, and it needs no raw pointer at the call site: that `target`
/// outlives the task is what `'static` says, and that the task may share it
/// is what `Sync` says.
pub fn spawn_for<T: Sync + 'static>(
    name: &str, target: &'static T, entry: fn(&'static T),
) -> Option<TaskHandle> {
    spawn_owned(name, None, (target, entry), |(target, entry)| entry(target))
}

/// Spawn a task that owns what it starts from: `ctx` is moved into the task
/// and dropped when `entry` returns. What a loadable module's tasks are --
/// nothing of a module lives for good, so its task holds an `Arc` of the
/// state it works on, and the state outlives the task however the two end.
pub fn spawn_with<C: Send + 'static>(name: &str, ctx: C, entry: fn(C)) -> Option<TaskHandle> {
    spawn_owned(name, None, ctx, entry)
}

/// As `spawn_with`, bound to the CPUs `affinity_mask` names.
pub fn spawn_on_with<C: Send + 'static>(
    name: &str, affinity_mask: u64, ctx: C, entry: fn(C),
) -> Option<TaskHandle> {
    spawn_owned(name, Some(affinity_mask), ctx, entry)
}

/// Spawn a task that receives a raw context pointer.
/// The caller is responsible for the lifetime and safety of `ctx`.
pub fn spawn_with_ctx(
    name: &str, func: extern "C" fn(*mut u8), ctx: *mut u8,
) -> Option<TaskHandle> {
    let h = unsafe { task::kernel_task_spawn(name.as_ptr(), name.len(), func, ctx) };
    if h == 0 { None } else { Some(TaskHandle { handle: h }) }
}

/// Spawn a task with a raw context pointer, bound to `affinity_mask` CPUs.
pub fn spawn_on_with_ctx(
    name: &str, affinity_mask: u64,
    func: extern "C" fn(*mut u8), ctx: *mut u8,
) -> Option<TaskHandle> {
    let h = unsafe {
        task::kernel_task_spawn_on(name.as_ptr(), name.len(), func, ctx, affinity_mask as usize)
    };
    if h == 0 { None } else { Some(TaskHandle { handle: h }) }
}

pub fn cpu_id() -> u32 {
    crate::cpu::id()
}
