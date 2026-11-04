//! The kernel's C++ half, as the `ffi` crate declares it: each function here
//! is one of its `extern "C"` declarations, defined -- the linker takes it
//! for the C++ one -- over the fuzzer's machine. Only what some fuzzer's
//! linked code reaches is here, and only what every machine has: what one
//! layer's kernel alone reaches -- the network's command line, the disk
//! log's -- is that fuzzer's own. A declaration nothing calls needs no
//! definition.
//!
//! Every one runs as the fuzzer's own code (`sched::harness`): what it
//! allocates is not the kernel's.

use core::ffi::c_void;

use super::{cmd, sched};

/* ---- the clock ---- */

#[no_mangle]
pub extern "C" fn kernel_get_boot_time_ns() -> u64 {
    sched::harness(sched::read_clock)
}

/// # Safety
/// Both pointers are writable, as the declaration's callers pass them.
#[no_mangle]
pub unsafe extern "C" fn kernel_get_boot_time(secs: *mut u64, usecs: *mut u64) {
    let ns = sched::harness(sched::read_clock);
    // SAFETY: the caller's pointers, `kcore::time::boot_time`'s locals.
    unsafe {
        *secs = ns / 1_000_000_000;
        *usecs = (ns % 1_000_000_000) / 1_000;
    }
}

#[no_mangle]
pub extern "C" fn kernel_get_wall_time_secs() -> u64 {
    sched::harness(sched::wall_secs)
}

#[no_mangle]
pub extern "C" fn kernel_cycle_counter_hz() -> u64 {
    0
}

/* ---- the CPU ---- */

#[no_mangle]
pub extern "C" fn kernel_get_cpu_id() -> u32 {
    sched::harness(sched::cpu)
}

#[no_mangle]
pub extern "C" fn kernel_cpu_count() -> u32 {
    4
}

#[no_mangle]
pub extern "C" fn kernel_cpu_online_mask() -> usize {
    0xF
}

#[no_mangle]
pub extern "C" fn kernel_irq_save() -> usize {
    sched::harness(sched::irq_save)
}

#[no_mangle]
pub extern "C" fn kernel_irq_restore(flags: usize) {
    sched::harness(|| sched::irq_restore(flags))
}

#[no_mangle]
pub extern "C" fn kernel_preempt_disable() {
    sched::harness(sched::preempt_disable)
}

#[no_mangle]
pub extern "C" fn kernel_preempt_enable() {
    sched::harness(sched::preempt_enable)
}

#[no_mangle]
pub extern "C" fn kernel_preempt_disable_task() -> usize {
    sched::harness(|| {
        sched::preempt_disable();
        sched::me()
    })
}

#[no_mangle]
pub extern "C" fn kernel_preempt_enable_task(_task: usize) {
    sched::harness(sched::preempt_enable)
}

#[no_mangle]
pub extern "C" fn kernel_preempt_is_on() -> i32 {
    1
}

#[no_mangle]
pub extern "C" fn kernel_preempt_can_block() -> i32 {
    sched::harness(|| (sched::ctx() == sched::Ctx::Task && sched::irq_off() == 0 && sched::preempt_off() == 0) as i32)
}

#[no_mangle]
pub extern "C" fn kernel_interrupts_enabled() -> i32 {
    sched::harness(|| (sched::ctx() != sched::Ctx::Irq && sched::irq_off() == 0) as i32)
}

/* ---- locks and events ---- */

#[no_mangle]
pub extern "C" fn kernel_mutex_create() -> usize {
    sched::harness(sched::mutex_create)
}

#[no_mangle]
pub extern "C" fn kernel_mutex_destroy(handle: usize) {
    sched::harness(|| sched::lock_destroy(handle))
}

#[no_mangle]
pub extern "C" fn kernel_mutex_lock(handle: usize) {
    sched::harness(|| sched::mutex_lock(handle))
}

#[no_mangle]
pub extern "C" fn kernel_mutex_unlock(handle: usize) {
    sched::harness(|| sched::mutex_unlock(handle))
}

#[no_mangle]
pub extern "C" fn kernel_spinlock_create() -> usize {
    sched::harness(sched::spin_create)
}

#[no_mangle]
pub extern "C" fn kernel_spinlock_destroy(handle: usize) {
    sched::harness(|| sched::lock_destroy(handle))
}

#[no_mangle]
pub extern "C" fn kernel_spinlock_lock(handle: usize) -> u64 {
    sched::harness(|| sched::spin_lock(handle))
}

#[no_mangle]
pub extern "C" fn kernel_spinlock_unlock(handle: usize, flags: u64) {
    sched::harness(|| sched::spin_unlock(handle, flags))
}

#[no_mangle]
pub extern "C" fn kernel_event_create() -> usize {
    sched::harness(sched::event_create)
}

#[no_mangle]
pub extern "C" fn kernel_event_destroy(handle: usize) {
    sched::harness(|| sched::event_destroy(handle))
}

#[no_mangle]
pub extern "C" fn kernel_event_wait(handle: usize) {
    sched::harness(|| sched::event_wait(handle, u64::MAX));
}

#[no_mangle]
pub extern "C" fn kernel_event_wait_for(handle: usize, timeout_ns: u64) -> i32 {
    sched::harness(|| {
        let until = sched::now().saturating_add(timeout_ns).min(sched::END);
        sched::event_wait(handle, until) as i32
    })
}

#[no_mangle]
pub extern "C" fn kernel_event_signal(handle: usize) {
    sched::harness(|| sched::event_signal(handle))
}

/* ---- tasks ---- */

/// # Safety
/// `name` points at `name_len` bytes; `func(ctx)` is the task.
#[no_mangle]
pub unsafe extern "C" fn kernel_task_spawn(
    name: *const u8, name_len: usize, func: extern "C" fn(*mut u8), ctx: *mut u8,
) -> usize {
    // SAFETY: the caller's name, as `kcore::task` passes it.
    let name = String::from_utf8_lossy(unsafe { core::slice::from_raw_parts(name, name_len) }).into_owned();
    let ctx = ctx as usize;
    sched::harness(|| {
        let cpu = super::next_cpu();
        sched::spawn(&name, sched::Kind::Kernel, cpu, Box::new(move || func(ctx as *mut u8)))
    })
}

/// # Safety
/// As `kernel_task_spawn`; the affinity is the fuzzer's to ignore.
#[no_mangle]
pub unsafe extern "C" fn kernel_task_spawn_on(
    name: *const u8, name_len: usize, func: extern "C" fn(*mut u8), ctx: *mut u8, _affinity: usize,
) -> usize {
    // SAFETY: the caller's contract, passed on.
    unsafe { kernel_task_spawn(name, name_len, func, ctx) }
}

#[no_mangle]
pub extern "C" fn kernel_task_wait(handle: usize) {
    sched::harness(|| sched::join(handle))
}

#[no_mangle]
pub extern "C" fn kernel_task_set_stopping(handle: usize) {
    sched::harness(|| sched::set_stopping(handle))
}

#[no_mangle]
pub extern "C" fn kernel_task_put(handle: usize) {
    sched::harness(|| sched::put(handle))
}

#[no_mangle]
pub extern "C" fn kernel_task_stopping() -> i32 {
    sched::harness(sched::stopping) as i32
}

#[no_mangle]
pub extern "C" fn kernel_sleep_ns(ns: u64) {
    sched::harness(|| sched::sleep_ns(ns))
}

#[no_mangle]
pub extern "C" fn kernel_task_yield_to_runnable() {
    sched::harness(sched::yield_now)
}

#[no_mangle]
pub extern "C" fn kernel_task_current() -> usize {
    sched::harness(|| {
        if sched::ctx() != sched::Ctx::Task {
            panic!("invariant: the current task asked for outside a task's context");
        }
        sched::me()
    })
}

#[no_mangle]
pub extern "C" fn kernel_task_current_or_none() -> usize {
    sched::harness(|| if sched::ctx() == sched::Ctx::Task { sched::me() } else { 0 })
}

/* ---- the tracepoints ---- */

/// The flags `kcore::trace` reads, a byte an event: every one on for an
/// input whose header says so (`runner::begin`), off for the rest -- so that
/// the layers' sites build their records, from values a disk or a peer chose,
/// under the overflow checks too.
static TP_FLAGS: [core::sync::atomic::AtomicU8; kcore::trace::ID_LIMIT] =
    [const { core::sync::atomic::AtomicU8::new(0) }; kcore::trace::ID_LIMIT];

/// Every event on, or off.
pub fn tp_set_all(on: bool) {
    for f in &TP_FLAGS {
        f.store(u8::from(on), core::sync::atomic::Ordering::Relaxed);
    }
}

#[no_mangle]
pub extern "C" fn kernel_tp_flags() -> *const u8 {
    TP_FLAGS.as_ptr() as *const u8
}

#[no_mangle]
pub extern "C" fn kernel_tp_count() -> usize {
    TP_FLAGS.len()
}

/// A record, held to what kcore declares of its event -- the kernel's
/// description, which every boot holds kcore's to: its size, each enumerated
/// field's value one its names cover, its padding zero, and the bytes of the
/// slot's words past it zero, as `kcore::trace::emit` lays a payload into
/// them. Values only, as the kernel's is.
#[no_mangle]
pub extern "C" fn kernel_tp_record(id_size: u64, w0: u64, w1: u64, w2: u64, w3: u64, w4: u64) {
    const RECORD_SIZE_SHIFT: u32 = 16;
    let id = (id_size & 0xFFFF) as u16;
    let size = usize::try_from(id_size >> RECORD_SIZE_SHIFT).unwrap_or(usize::MAX);
    let Some(e) = kcore::trace::EVENTS.iter().find(|e| e.id as u16 == id) else {
        panic!("invariant: a record of event {}, which kcore declares no payload of", id);
    };
    if size != e.size {
        panic!("invariant: a record of {} of {} bytes, its payload's being {}", e.name, size, e.size);
    }
    let mut slot = [0u8; 40];
    for (chunk, word) in slot.chunks_exact_mut(8).zip([w0, w1, w2, w3, w4]) {
        chunk.copy_from_slice(&word.to_ne_bytes());
    }
    let Some((bytes, past)) = slot.split_at_checked(size) else {
        panic!("invariant: a record of {} of {} bytes, past a slot's", e.name, size);
    };
    if past.iter().any(|&b| b != 0) {
        panic!("invariant: {}'s slot words past its {} bytes are not zero: {:?}", e.name, size, past);
    }
    for f in e.fields {
        let Some(field) = bytes.get(f.offset..f.offset + f.size) else {
            panic!("invariant: {}'s field {} past its payload", e.name, f.name);
        };
        if f.name.starts_with('_') && field.iter().any(|&b| b != 0) {
            panic!("invariant: {}'s {} is not zero: {:?}", e.name, f.name, field);
        }
        if !f.names.is_empty() && (f.size != 1 || usize::from(field[0]) >= f.names.len()) {
            panic!("invariant: {}'s {} is {:?}, which none of its {} names is", e.name, f.name, field, f.names.len());
        }
    }
    /* What the stats count: which events the inputs had records of -- the
     * fuzzer's own work, whose allocation is not the kernel's, wherever the
     * record was made: under a spin lock, in an interrupt. */
    sched::harness(|| super::super::runner::reached(e.name));
}

#[no_mangle]
pub extern "C" fn kernel_tp_pid() -> u32 {
    sched::harness(|| if sched::ctx() == sched::Ctx::Task { u32::try_from(sched::me()).unwrap_or(u32::MAX) } else { u32::MAX })
}

/* ---- soft IRQs and timers ---- */

#[no_mangle]
pub extern "C" fn kernel_softirq_raise(typ: usize) {
    sched::harness(|| sched::softirq_raise(typ))
}

#[no_mangle]
pub extern "C" fn kernel_softirq_pending(typ: usize) -> i32 {
    sched::harness(|| sched::softirq_pending(typ)) as i32
}

#[no_mangle]
pub extern "C" fn kernel_softirq_register(typ: usize, handler: extern "C" fn(*mut u8), ctx: *mut u8) {
    let ctx = ctx as usize;
    sched::harness(|| sched::softirq_register(typ, handler, ctx))
}

#[no_mangle]
pub extern "C" fn kernel_timer_start(handler: extern "C" fn(*mut u8), ctx: *mut u8, period_ns: u64) -> usize {
    let ctx = ctx as usize;
    sched::harness(|| sched::timer_start(handler, ctx, period_ns))
}

#[no_mangle]
pub extern "C" fn kernel_timer_stop(handle: usize) {
    sched::harness(|| sched::timer_stop(handle))
}

/* ---- the entropy pool ---- */

/// # Safety
/// `buf` is `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_get_random(buf: *mut u8, len: usize) -> i32 {
    // SAFETY: the caller's buffer, as `kcore::random::fill_random` passes it.
    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    sched::harness(|| super::random(buf)) as i32
}

/* ---- the log, and a panic ---- */

/// # Safety
/// `msg` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_klog(level: u32, msg: *const u8, len: usize) {
    // SAFETY: the caller's message, `kcore::klog`'s buffer.
    let msg = unsafe { core::slice::from_raw_parts(msg, len) };
    sched::harness(|| super::trace(level, msg))
}

#[no_mangle]
pub extern "C" fn kernel_panic_active() -> i32 {
    0
}

/// # Safety
/// `msg` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_panic(msg: *const u8, len: usize) -> ! {
    // SAFETY: the caller's message.
    let msg = String::from_utf8_lossy(unsafe { core::slice::from_raw_parts(msg, len) }).into_owned();
    panic!("the kernel panicked: {}", msg)
}

/* ---- memory ---- */

/// Pages for a device to reach: the host's, page-aligned and zeroed, their
/// address their "physical" one. Allocated as the kernel allocates them,
/// which interrupts off would deadlock (`heap::check`).
#[no_mangle]
pub unsafe extern "C" fn kernel_alloc_dma_pages(count: usize, phys_out: *mut u64, actual_pages_out: *mut usize)
    -> *mut u8 {
    const PAGE: usize = 4096;
    super::heap::check("allocates DMA pages of", count.saturating_mul(PAGE));
    sched::harness(|| {
        let Some(size) = count.checked_mul(PAGE).filter(|&s| s != 0) else { return core::ptr::null_mut() };
        let Ok(layout) = std::alloc::Layout::from_size_align(size, PAGE) else { return core::ptr::null_mut() };
        // SAFETY: a layout of a non-zero size.
        let p = unsafe { std::alloc::alloc_zeroed(layout) };
        if !p.is_null() {
            DMA.lock().unwrap_or_else(|e| e.into_inner()).push((p as usize, layout));
            // SAFETY: the caller's two words to write.
            unsafe {
                *phys_out = p as u64;
                *actual_pages_out = count;
            }
        }
        p
    })
}

/// What `kernel_alloc_dma_pages` gave out, and how: to give back.
static DMA: std::sync::Mutex<Vec<(usize, std::alloc::Layout)>> = std::sync::Mutex::new(Vec::new());

/// Whether `len` bytes from `addr` lie inside DMA pages given out and not
/// given back: what a device that is code may reach -- its I/O into
/// anything else a finding, memory the kernel freed under it above all.
pub fn dma_live(addr: usize, len: usize) -> bool {
    let d = DMA.lock().unwrap_or_else(|e| e.into_inner());
    d.iter().any(|&(at, layout)| addr >= at && addr.checked_add(len).is_some_and(|end| end <= at + layout.size()))
}

#[no_mangle]
pub unsafe extern "C" fn kernel_free_dma_pages(ptr: *mut u8) {
    super::heap::check("frees DMA pages at", ptr as usize);
    sched::harness(|| {
        let mut d = DMA.lock().unwrap_or_else(|e| e.into_inner());
        let Some(at) = d.iter().position(|e| e.0 == ptr as usize) else {
            panic!("invariant: DMA pages at {:p} freed that were never allocated, or twice", ptr);
        };
        let (_, layout) = d.swap_remove(at);
        // SAFETY: allocated above with this layout, and not freed since.
        unsafe { std::alloc::dealloc(ptr, layout) };
    })
}

/// A frame's buffer is where the NIC reads and writes it: in the fuzzer,
/// its address, which is as good as a physical one to a NIC that is code.
#[no_mangle]
pub extern "C" fn kernel_virt_to_phys(virt: *const u8) -> u64 {
    virt as u64
}

/// Every line the kernel log holds, oldest first: what the fuzzer traced
/// before netconsole was set up.
#[no_mangle]
pub extern "C" fn kernel_dmesg_replay(line: extern "C" fn(ctx: *mut u8, s: *const u8, len: usize), ctx: *mut u8) {
    let lines = sched::harness(super::dmesg);
    for l in &lines {
        sched::kernel(|| line(ctx, l.as_ptr(), l.len()));
    }
}

/* ---- what the kernel holds of itself: the system provider's ---- */

/// `s`, as much of it as fits in the caller's `len` bytes at `buf`: the
/// bytes written.
///
/// # Safety
/// `buf` is `len` writable bytes.
unsafe fn put(buf: *mut u8, len: usize, s: &[u8]) -> usize {
    let n = s.len().min(len);
    if n != 0 {
        // SAFETY: the caller's buffer, `n <= len` bytes of it.
        unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n) };
    }
    n
}

/// # Safety
/// `buf` is `len` writable bytes and `last` writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_dmesg_read(since: u64, buf: *mut u8, len: usize, last: *mut u64) -> usize {
    let (records, got) = sched::harness(|| super::dmesg_since(since, len));
    // SAFETY: the caller's buffer and its `last`, `kcore::system::log_since`'s.
    unsafe {
        *last = got;
        put(buf, len, &records)
    }
}

#[no_mangle]
pub extern "C" fn kernel_dmesg_next_seq() -> u64 {
    sched::harness(super::dmesg_counts).0
}

#[no_mangle]
pub extern "C" fn kernel_dmesg_lost() -> u64 {
    sched::harness(super::dmesg_counts).1
}

/// # Safety
/// `buf` is `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_version_field(which: u32, buf: *mut u8, len: usize) -> usize {
    let s: &[u8] = if which == ffi::system::VERSION { b"0.0.0-fuzz" } else { b"fuzz" };
    // SAFETY: the caller's buffer.
    unsafe { put(buf, len, s) }
}

/// # Safety
/// `buf` is `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_cpu_model(buf: *mut u8, len: usize) -> usize {
    // SAFETY: the caller's buffer.
    unsafe { put(buf, len, b"the fuzzer's CPU") }
}

/// The machine's memory, as a machine of a gigabyte with half of it free
/// would say it.
///
/// # Safety
/// `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_mem_info(out: *mut ffi::system::MemInfo) {
    let info = ffi::system::MemInfo { page_bytes: 4096, total_pages: 1 << 18, free_pages: 1 << 17,
                                      huge_block_bytes: 2 << 20, huge_blocks: 0, huge_free: 0 };
    // SAFETY: the caller's struct.
    unsafe { *out = info };
}

#[no_mangle]
pub extern "C" fn kernel_cpu_idle_ns(_cpu: u32) -> u64 {
    0
}

/// The machine's tasks, as the kernel's table copies them.
///
/// # Safety
/// `out` is `max` writable `TaskInfo`s.
#[no_mangle]
pub unsafe extern "C" fn kernel_task_snapshot(out: *mut ffi::system::TaskInfo, max: usize) -> usize {
    let tasks = sched::harness(sched::tasks);
    let mut n = 0;
    for (id, name, _, st, _) in tasks.iter().take(max) {
        let mut info = ffi::system::TaskInfo::ZERO;
        info.pid = *id as u64;
        info.state = match st {
            sched::St::Done => ffi::system::TASK_EXITED,
            sched::St::Blocked(_) => ffi::system::TASK_WAITING,
            sched::St::Ready | sched::St::Running => ffi::system::TASK_RUNNING,
        };
        let len = name.len().min(info.name.len() - 1);
        info.name[..len].copy_from_slice(&name.as_bytes()[..len]);
        // SAFETY: the caller's array, `n < max` of it.
        unsafe { *out.add(n) = info };
        n += 1;
    }
    n
}

#[no_mangle]
pub extern "C" fn kernel_klog_level() -> u32 {
    super::trace_level()
}

/// Every line under `--trace`, which echoes each whatever its level; the
/// log's own otherwise.
#[no_mangle]
pub extern "C" fn kernel_klog_wants(level: u32) -> i32 {
    (super::ECHO_TRACE.load(std::sync::atomic::Ordering::Relaxed) || level <= super::trace_level()) as i32
}

/// Taken as the kernel takes one, and nothing changed: the fuzzer's log
/// keeps its level.
#[no_mangle]
pub extern "C" fn kernel_klog_set_level(level: u32) -> i32 {
    (level <= 9) as i32
}

/// No task is the shell's to renice here: none is found.
#[no_mangle]
pub extern "C" fn kernel_task_set_nice(_pid: u64, _nice: i64) -> i32 {
    1
}

#[no_mangle]
pub extern "C" fn kernel_module_list(_out: *mut ffi::system::ModuleInfo, _max: usize) -> usize {
    0
}

/// No module is loaded through the loader on the fuzzer's machine: a target
/// loads its module's source itself.
#[no_mangle]
pub extern "C" fn kernel_module_command(_unload: i32, _arg: *const u8, _arg_len: usize, _params: *const u8,
                                        _params_len: usize, _sink: ffi::cmd::CmdSink, _ctx: *mut c_void) -> i32 {
    ffi::system::MODULE_NOT_STARTED
}

/// Power-offs and reboots asked for: counted, and nothing done -- a target
/// that never asks for one holds this to 0.
pub static POWER_REQUESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[no_mangle]
pub extern "C" fn kernel_power_request(_what: u32) {
    POWER_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/* ---- the shell's command table ---- */

/// # Safety
/// `name` and `help` point at their lengths' bytes; `handler(ctx, ...)` is
/// sound to call until the unregister.
#[no_mangle]
pub unsafe extern "C" fn kernel_cmd_register(
    name: *const u8, name_len: usize, help: *const u8, help_len: usize, handler: ffi::cmd::CmdHandler,
    ctx: *mut c_void,
) -> usize {
    // SAFETY: the caller's strings.
    let (name, help) = unsafe {
        (core::slice::from_raw_parts(name, name_len).to_vec(), core::slice::from_raw_parts(help, help_len).to_vec())
    };
    let ctx = ctx as usize;
    sched::harness(|| cmd::register(name, help, handler, ctx))
}

#[no_mangle]
pub extern "C" fn kernel_cmd_unregister(handle: usize) {
    sched::harness(|| cmd::unregister(handle))
}

/// # Safety
/// `out` is a printer this fuzzer handed a command; `buf` is `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_printer_write(out: *mut c_void, buf: *const u8, len: usize) {
    // SAFETY: the command's output, as `kcore::cmd::Output` passes it.
    let bytes = unsafe { core::slice::from_raw_parts(buf, len) };
    cmd::printer_write(out as usize, bytes)
}

/// # Safety
/// As `kernel_printer_write`, `buf` `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_printer_read(out: *mut c_void, buf: *mut u8, len: usize, timeout_ns: u64) -> isize {
    // SAFETY: the command's buffer.
    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    cmd::printer_read(out as usize, buf, timeout_ns)
}

/// # Safety
/// `out` is a printer this fuzzer handed a command; `term` is a
/// `CmdTerminal` to fill.
#[no_mangle]
pub unsafe extern "C" fn kernel_printer_terminal(out: *mut c_void, term: *mut ffi::cmd::CmdTerminal) -> bool {
    if term.is_null() {
        return false;
    }
    match cmd::printer_terminal(out as usize) {
        Some(t) => {
            // SAFETY: the caller's struct, checked not null.
            unsafe { term.write(t) };
            true
        }
        None => false,
    }
}

/// # Safety
/// `line` is `len` bytes; `sink(ctx, ...)` is sound to call while this runs.
#[no_mangle]
pub unsafe extern "C" fn kernel_cmd_dispatch(line: *const u8, len: usize, sink: ffi::cmd::CmdSink, ctx: *mut c_void) {
    // SAFETY: the caller's line.
    let line = unsafe { core::slice::from_raw_parts(line, len) }.to_vec();
    cmd::dispatch(&line, sink, None, None, ctx as usize)
}

/// # Safety
/// As `kernel_cmd_dispatch`, and `source(ctx, ...)` and `terminal(ctx, ...)`
/// too.
#[no_mangle]
pub unsafe extern "C" fn kernel_cmd_dispatch_io(
    line: *const u8, len: usize, sink: ffi::cmd::CmdSink, source: ffi::cmd::CmdSource,
    terminal: ffi::cmd::CmdTerminalOf, ctx: *mut c_void,
) {
    // SAFETY: the caller's line.
    let line = unsafe { core::slice::from_raw_parts(line, len) }.to_vec();
    cmd::dispatch(&line, sink, Some(source), Some(terminal), ctx as usize)
}
