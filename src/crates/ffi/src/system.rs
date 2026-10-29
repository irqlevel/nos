use core::ffi::c_void;

use crate::cmd::CmdSink;

/* What the kernel's C++ holds of itself, for the management bus's `system`
   provider (src/crates/kernel): its tables copied into the caller's buffers,
   each under the lock the table keeps and with nothing allocated, and the few
   things about it the provider may change. src/base/kernel/rust_ffi.cpp
   defines these; the structs are its layout, which both sides hold to by
   size. */

/// A task, as `kernel_task_snapshot` copies it (TaskTable::TaskSnapshot).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskInfo {
    pub pid: u64,
    pub runtime_ns: u64,
    pub context_switches: u64,
    /// `TASK_WAITING`, `TASK_RUNNING` or `TASK_EXITED`.
    pub state: u32,
    /// `TASK_STOPPING`, `TASK_IDLE`, `TASK_BLOCKED`, `TASK_IDLING`.
    pub flags: u32,
    /// NUL-terminated.
    pub name: [u8; 32],
}

impl TaskInfo {
    pub const ZERO: TaskInfo = TaskInfo { pid: 0, runtime_ns: 0, context_switches: 0, state: 0, flags: 0, name: [0; 32] };
}

const _: () = assert!(core::mem::size_of::<TaskInfo>() == 64);

pub const TASK_WAITING: u32 = 1;
pub const TASK_RUNNING: u32 = 2;
pub const TASK_EXITED: u32 = 3;

/// Asked to stop.
pub const TASK_STOPPING: u32 = 1;
/// A CPU's idle task.
pub const TASK_IDLE: u32 = 2;
/// Waiting to be woken.
pub const TASK_BLOCKED: u32 = 4;
/// An idle task in its CPU's idle loop.
pub const TASK_IDLING: u32 = 8;

/// A loaded module, as `kernel_module_list` copies it
/// (ModuleTable::ModuleSnapshot): not where it is -- the API says no kernel
/// address.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ModuleInfo {
    /// NUL-terminated.
    pub name: [u8; 32],
    pub size: u64,
    /// The kernel functions it binds to.
    pub imports: u64,
    /// `MODULE_LIVE`, `MODULE_LOADING` or `MODULE_UNLOADING`.
    pub phase: u32,
    /// 1 when what it imports keeps it from being unloaded.
    pub permanent: u32,
}

impl ModuleInfo {
    pub const ZERO: ModuleInfo = ModuleInfo { name: [0; 32], size: 0, imports: 0, phase: 0, permanent: 0 };
}

const _: () = assert!(core::mem::size_of::<ModuleInfo>() == 56);

pub const MODULE_LIVE: u32 = 0;
pub const MODULE_LOADING: u32 = 1;
pub const MODULE_UNLOADING: u32 = 2;

/// The page allocator's pages and the huge pool's blocks.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MemInfo {
    pub page_bytes: u64,
    pub total_pages: u64,
    pub free_pages: u64,
    pub huge_block_bytes: u64,
    pub huge_blocks: u64,
    pub huge_free: u64,
}

/// How `kernel_module_command` ended.
pub const MODULE_DONE: i32 = 0;
pub const MODULE_FAILED: i32 = 1;
/// Still going after the shell's wait: lsmod shows where, the log will say
/// how it ended.
pub const MODULE_PENDING: i32 = 2;
/// Never started: a path, name or parameters too long, empty or not
/// printable ASCII, or no memory.
pub const MODULE_NOT_STARTED: i32 = 3;

/// What `kernel_version_field` gives.
pub const VERSION: u32 = 0;
pub const REVISION: u32 = 1;

/// What `kernel_power_request` asks for.
pub const POWER_OFF: u32 = 0;
pub const REBOOT: u32 = 1;

/// A log record's header, before its text: its number (8 bytes) and the
/// text's length (2), least significant byte first.
pub const DMESG_RECORD_HEADER: usize = 10;

unsafe extern "C" {
    /// Every task, at most `max` of them, into `out`: how many were copied.
    pub fn kernel_task_snapshot(out: *mut TaskInfo, max: usize) -> usize;
    /// How long the CPU numbered `cpu` has spent in its idle task, the
    /// stretch it is in now included; 0 for one that is not running. Looks
    /// `cpu` up among the running CPUs first. Task context: it takes the
    /// idle task's lock.
    pub safe fn kernel_cpu_idle_ns(cpu: u32) -> u64;
    pub fn kernel_mem_info(out: *mut MemInfo);
    /// What the CPU is called, into `buf`: the bytes written.
    pub fn kernel_cpu_model(buf: *mut u8, len: usize) -> usize;
    /// `VERSION` or `REVISION`, into `buf`: the bytes written.
    pub fn kernel_version_field(which: u32, buf: *mut u8, len: usize) -> usize;
    pub safe fn kernel_trace_level() -> u32;
    /// 1 set; 0 -- nothing changed -- for a level past the highest.
    pub safe fn kernel_trace_set_level(level: u32) -> i32;
    /// A task's nice, looked up by its pid: 0 set, 1 no such task, 2 out of
    /// range. Task context.
    pub safe fn kernel_task_set_nice(pid: u64, nice: i64) -> i32;
    /// Every loaded module, at most `max` of them, into `out`: how many
    /// were copied.
    pub fn kernel_module_list(out: *mut ModuleInfo, max: usize) -> usize;
    /// insmod (`unload` 0) of the module at the path `arg`, its init handed
    /// `params`, or rmmod (1) of the module named `arg`, as the shell runs
    /// them -- what they said handed to `sink(ctx, ...)` -- answering
    /// `MODULE_DONE` or the rest. Task context: it sleeps.
    pub fn kernel_module_command(
        unload: i32,
        arg: *const u8,
        arg_len: usize,
        params: *const u8,
        params_len: usize,
        sink: CmdSink,
        ctx: *mut c_void,
    ) -> i32;
    /// The log's messages numbered after `since`, as records
    /// (`DMESG_RECORD_HEADER`, then the text) for as many as fit whole in
    /// `buf`: the bytes written, and the number of the last one copied --
    /// `since` for none -- into `last`.
    pub fn kernel_dmesg_read(since: u64, buf: *mut u8, len: usize, last: *mut u64) -> usize;
    /// The number the next message logged will have.
    pub safe fn kernel_dmesg_next_seq() -> u64;
    /// Messages the log no longer has: recycled to make room, or dropped.
    pub safe fn kernel_dmesg_lost() -> u64;
    /// `POWER_OFF` or `REBOOT` asked for, as the shell's commands ask: the
    /// halt runs on the shell's watch after this returns; anything else is
    /// nothing.
    pub safe fn kernel_power_request(what: u32);
}
