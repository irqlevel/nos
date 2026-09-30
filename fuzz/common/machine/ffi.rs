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
pub unsafe extern "C" fn kernel_trace(level: u32, msg: *const u8, len: usize) {
    // SAFETY: the caller's message, `kcore::trace`'s buffer.
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

/* ---- the lockless ring ---- */

#[no_mangle]
pub extern "C" fn kernel_ring_create(capacity: usize) -> usize {
    sched::harness(|| super::ring_create(capacity))
}

#[no_mangle]
pub extern "C" fn kernel_ring_destroy(ring: usize) {
    sched::harness(|| super::ring_destroy(ring))
}

#[no_mangle]
pub extern "C" fn kernel_ring_push(ring: usize, value: usize) -> i32 {
    sched::harness(|| super::ring_push(ring, value)) as i32
}

/// # Safety
/// `value` is writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_ring_pop(ring: usize, value: *mut usize) -> i32 {
    match sched::harness(|| super::ring_pop(ring)) {
        Some(v) => {
            // SAFETY: the caller's local, `LocklessRing::pop`'s.
            unsafe { *value = v };
            1
        }
        None => 0,
    }
}

#[no_mangle]
pub extern "C" fn kernel_ring_count(ring: usize) -> usize {
    sched::harness(|| super::ring_count(ring))
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
/// `line` is `len` bytes; `sink(ctx, ...)` is sound to call while this runs.
#[no_mangle]
pub unsafe extern "C" fn kernel_cmd_dispatch(line: *const u8, len: usize, sink: ffi::cmd::CmdSink, ctx: *mut c_void) {
    // SAFETY: the caller's line.
    let line = unsafe { core::slice::from_raw_parts(line, len) }.to_vec();
    cmd::dispatch(&line, sink, None, ctx as usize)
}

/// # Safety
/// As `kernel_cmd_dispatch`, and `source(ctx, ...)` too.
#[no_mangle]
pub unsafe extern "C" fn kernel_cmd_dispatch_io(
    line: *const u8, len: usize, sink: ffi::cmd::CmdSink, source: ffi::cmd::CmdSource, ctx: *mut c_void,
) {
    // SAFETY: the caller's line.
    let line = unsafe { core::slice::from_raw_parts(line, len) }.to_vec();
    cmd::dispatch(&line, sink, Some(source), ctx as usize)
}
