//! What the kernel's C++ holds of itself, copied out for whoever reports it
//! -- the management bus's `system` provider: its tasks, how long each CPU
//! has idled, its memory, what its CPU is called, its modules, its log by
//! number -- and the few things about it that may be changed from there:
//! the trace level, a task's nice, a module loaded or unloaded, the power.
//!
//! The copies are made into buffers of the caller's, under the lock each
//! table keeps: an answer is how the table stood at one moment, never a
//! pointer into it.

use alloc::vec::Vec;
use core::ffi::c_void;
use ffi::system;

pub use ffi::system::{
    MemInfo, ModuleInfo, TaskInfo, MODULE_LIVE, MODULE_LOADING, MODULE_UNLOADING, TASK_BLOCKED, TASK_EXITED,
    TASK_IDLE, TASK_IDLING, TASK_RUNNING, TASK_STOPPING, TASK_WAITING,
};

/// Every task, at most `max`: None with no memory for them. As many as
/// `max` may mean there were more.
pub fn tasks(max: usize) -> Option<Vec<TaskInfo>> {
    let mut out = Vec::new();
    out.try_reserve_exact(max).ok()?;
    out.resize(max, TaskInfo::ZERO);
    /* `out` holds `max` of them, each written whole or not at all */
    let n = unsafe { system::kernel_task_snapshot(out.as_mut_ptr(), max) };
    out.truncate(n);
    Some(out)
}

/// Every loaded module, at most `max`, as `tasks`.
pub fn modules(max: usize) -> Option<Vec<ModuleInfo>> {
    let mut out = Vec::new();
    out.try_reserve_exact(max).ok()?;
    out.resize(max, ModuleInfo::ZERO);
    /* As tasks' */
    let n = unsafe { system::kernel_module_list(out.as_mut_ptr(), max) };
    out.truncate(n);
    Some(out)
}

/// A name the C++ copied, up to its NUL.
pub fn name_of(name: &[u8]) -> &str {
    let len = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    match core::str::from_utf8(&name[..len]) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&name[..e.valid_up_to()]).unwrap_or(""),
    }
}

/// How long the CPU numbered `cpu` has idled since it started, the stretch
/// it idles now included; 0 for one that is not running.
pub fn cpu_idle_ns(cpu: u32) -> u64 {
    system::kernel_cpu_idle_ns(cpu)
}

pub fn memory() -> MemInfo {
    let mut info = MemInfo::default();
    unsafe { system::kernel_mem_info(&mut info) };
    info
}

/// What the CPU is called, into `buf`: the bytes written.
pub fn cpu_model(buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    unsafe { system::kernel_cpu_model(buf.as_mut_ptr(), buf.len()) }.min(buf.len())
}

/// The kernel's version, into `buf`: the bytes written.
pub fn version(buf: &mut [u8]) -> usize {
    field(system::VERSION, buf)
}

/// The git revision the kernel was built from, into `buf`: the bytes
/// written.
pub fn revision(buf: &mut [u8]) -> usize {
    field(system::REVISION, buf)
}

fn field(which: u32, buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    unsafe { system::kernel_version_field(which, buf.as_mut_ptr(), buf.len()) }.min(buf.len())
}

pub fn trace_level() -> u32 {
    system::kernel_trace_level()
}

/// The trace level set: false -- nothing changed -- past the highest.
pub fn set_trace_level(level: u32) -> bool {
    system::kernel_trace_set_level(level) == 1
}

/// Why a nice was not set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NiceError {
    NoTask,
    OutOfRange,
}

/// The nice of the task `pid`.
pub fn set_nice(pid: u64, nice: i64) -> Result<(), NiceError> {
    match system::kernel_task_set_nice(pid, nice) {
        0 => Ok(()),
        1 => Err(NiceError::NoTask),
        _ => Err(NiceError::OutOfRange),
    }
}

/// How an insmod or an rmmod ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleOutcome {
    Done,
    /// What it said says why.
    Failed,
    /// Still going after the shell's wait: the log will say how it ended.
    Pending,
    /// Never started: too long, empty, not printable ASCII, or no memory.
    NotStarted,
}

/// insmod of the module at `path`, its init handed `params`; what it said
/// handed to `sink`. Sleeps for as long as the shell's insmod waits.
pub fn load_module(path: &str, params: &str, sink: &mut dyn FnMut(&[u8])) -> ModuleOutcome {
    module_command(0, path, params, sink)
}

/// rmmod of the module `name`, as `load_module`.
pub fn unload_module(name: &str, sink: &mut dyn FnMut(&[u8])) -> ModuleOutcome {
    module_command(1, name, "", sink)
}

fn module_command(unload: i32, arg: &str, params: &str, sink: &mut dyn FnMut(&[u8])) -> ModuleOutcome {
    let mut sink = sink;
    let ctx = &mut sink as *mut &mut dyn FnMut(&[u8]) as *mut c_void;
    /* `ctx` is `sink`, alive until this returns, which is when the C++ is
     * done with it; the strings are valid for their lengths */
    let code = unsafe {
        system::kernel_module_command(unload, arg.as_ptr(), arg.len(), params.as_ptr(), params.len(),
                                      module_sink, ctx)
    };
    match code {
        system::MODULE_DONE => ModuleOutcome::Done,
        system::MODULE_FAILED => ModuleOutcome::Failed,
        system::MODULE_PENDING => ModuleOutcome::Pending,
        _ => ModuleOutcome::NotStarted,
    }
}

/// # Safety
/// `ctx` is the `&mut &mut dyn FnMut(&[u8])` `module_command` passed, and
/// `buf` is `len` bytes to read.
unsafe extern "C" fn module_sink(ctx: *mut c_void, buf: *const u8, len: usize) {
    let sink = unsafe { &mut *(ctx as *mut &mut dyn FnMut(&[u8])) };
    sink(unsafe { core::slice::from_raw_parts(buf, len) });
}

/// The log's messages numbered after `since`, as records for as many as fit
/// whole in `buf` -- `records` reads them -- and the number of the last one
/// copied (`since` for none).
pub fn log_since(since: u64, buf: &mut [u8]) -> (usize, u64) {
    let mut last = since;
    if buf.is_empty() {
        return (0, last);
    }
    let n = unsafe { system::kernel_dmesg_read(since, buf.as_mut_ptr(), buf.len(), &mut last) };
    (n.min(buf.len()), last)
}

/// The records `log_since` wrote: each message's number and text.
pub fn records(mut buf: &[u8]) -> impl Iterator<Item = (u64, &[u8])> {
    core::iter::from_fn(move || {
        let header = buf.get(..system::DMESG_RECORD_HEADER)?;
        let mut seq = [0u8; 8];
        seq.copy_from_slice(&header[..8]);
        let len = u16::from_le_bytes([header[8], header[9]]) as usize;
        let text = buf.get(system::DMESG_RECORD_HEADER..system::DMESG_RECORD_HEADER + len)?;
        buf = &buf[system::DMESG_RECORD_HEADER + len..];
        Some((u64::from_le_bytes(seq), text))
    })
}

/// The number the next message logged will have.
pub fn log_next_seq() -> u64 {
    system::kernel_dmesg_next_seq()
}

/// Messages the log no longer has.
pub fn log_lost() -> u64 {
    system::kernel_dmesg_lost()
}

/// A power-off asked for: the halt runs on the shell's watch, after this
/// returns.
pub fn power_off() {
    system::kernel_power_request(system::POWER_OFF);
}

/// A reboot asked for, as `power_off`.
pub fn reboot() {
    system::kernel_power_request(system::REBOOT);
}
