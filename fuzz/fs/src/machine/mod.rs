//! The machine the storage layers run on: the fuzzers' common one
//! (`common::machine`: the kernel's C++ half, the CPUs that run its tasks
//! one at a time, its allocator), and what storage adds to it -- its disks
//! (`disk`), the command line's storage parameters, what procfs reports,
//! and the disk log, which the C++ tracer hands every line to.

pub mod disk;

pub use crate::common::machine::*;

use std::sync::Mutex;

/// The wall clock when an input starts: in the past of whoever runs
/// e2fsck on an image the fuzzer made, which calls a superblock written in
/// its future damaged.
const WALL_BASE: u64 = 1_750_000_000;

extern "C" {
    /* The disk log's C ABI (block::disklog is private to its crate): what
     * the C++ tracer, the boot path and the panic path call. */
    pub fn rust_disklog_log(line: *const u8);
    pub fn rust_disklog_setup() -> i32;
    pub fn rust_disklog_stop();
}

/// Boots the machine, once, in the parent every input's process is forked
/// from: the block layer and the filesystem layer set up and their
/// commands in the table, as `rust_init` does it. The disks are each
/// input's own. No thread is made and none parked: see `sched::boot`.
pub fn boot() {
    sched::boot();
    sched::set_wall_base(WALL_BASE);
    set_trace_sink(disklog_line);
    block::init();
    fs::init();
}

/// Every line the tracer makes, to the disk log -- NUL-terminated, as the
/// C++ tracer hands it on -- from whatever context traced it; and to
/// `LOGGED`, while a target keeps it.
fn disklog_line(line: &[u8]) {
    {
        let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lines) = logged.as_mut() {
            lines.push((sched::me(), line.to_vec()));
        }
    }
    let mut text = line.to_vec();
    text.push(0);
    // SAFETY: a NUL-terminated line.
    sched::kernel(|| unsafe { rust_disklog_log(text.as_ptr()) });
}

/// The lines handed to the disk log, and the task each came from, in the
/// order they came: kept once `keep_logged` is called.
static LOGGED: Mutex<Option<Vec<(usize, Vec<u8>)>>> = Mutex::new(None);

pub fn keep_logged() {
    *LOGGED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Vec::new());
}

pub fn logged() -> Vec<(usize, Vec<u8>)> {
    LOGGED.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

/* ---- the kernel command line, and what procfs reports ---- */

#[derive(Clone)]
pub struct Params {
    /// `kernel_disklog_wanted`: -1 before the command line is read, then 1
    /// for `disklog=on` and 0 without it.
    pub disklog: i32,
    /// `root=`: 0 none, 1 auto, 2 a device, 3 a label, 4 a UUID
    pub root_mode: i32,
    pub root_value: Vec<u8>,
    pub root_uuid: [u8; 16],
    pub root_ro: bool,
    pub fstest: bool,
    pub cmdline: Vec<u8>,
    /// The interrupt counters procfs renders: a name and a count each.
    pub interrupts: Vec<(Vec<u8>, i64)>,
}

static PARAMS: Mutex<Params> = Mutex::new(Params {
    disklog: 0,
    root_mode: 0,
    root_value: Vec::new(),
    root_uuid: [0; 16],
    root_ro: false,
    fstest: false,
    cmdline: Vec::new(),
    interrupts: Vec::new(),
});

pub fn params() -> Params {
    PARAMS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn set_params(p: Params) {
    *PARAMS.lock().unwrap_or_else(|e| e.into_inner()) = p;
}

/// `s` into the C buffer `buf` of `len` bytes as `SnPrintf("%s")` puts it:
/// cut to fit, NUL-terminated; the bytes written, the NUL aside.
///
/// # Safety
/// `buf` is `len` writable bytes.
unsafe fn put_c_string(buf: *mut u8, len: usize, s: &[u8]) -> usize {
    if buf.is_null() || len == 0 {
        return 0;
    }
    let n = s.len().min(len - 1);
    // SAFETY: the caller's buffer, `n + 1 <= len` bytes of it.
    unsafe {
        core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n);
        *buf.add(n) = 0;
    }
    n
}

#[no_mangle]
pub extern "C" fn kernel_disklog_wanted() -> i32 {
    params().disklog
}

/// # Safety
/// `value` is `value_len` writable bytes, `uuid` `uuid_len`.
#[no_mangle]
pub unsafe extern "C" fn kernel_root_spec(value: *mut u8, value_len: usize, uuid: *mut u8, uuid_len: usize) -> i32 {
    let p = params();
    // SAFETY: the caller's buffers, as `kcore::procinfo::root_spec` passes
    // them.
    unsafe {
        put_c_string(value, value_len, &p.root_value);
        if !uuid.is_null() && uuid_len >= 16 {
            core::ptr::copy_nonoverlapping(p.root_uuid.as_ptr(), uuid, 16);
        }
    }
    p.root_mode
}

#[no_mangle]
pub extern "C" fn kernel_root_read_only() -> i32 {
    params().root_ro as i32
}

#[no_mangle]
pub extern "C" fn kernel_root_fstest() -> i32 {
    params().fstest as i32
}

/// # Safety
/// `buf` is `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_version_string(buf: *mut u8, len: usize) -> usize {
    // SAFETY: the caller's buffer.
    unsafe { put_c_string(buf, len, b"nos fuzz (fs-fuzz)") }
}

/// # Safety
/// `buf` is `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_cmdline_string(buf: *mut u8, len: usize) -> usize {
    let cmdline = params().cmdline;
    // SAFETY: the caller's buffer.
    unsafe { put_c_string(buf, len, &cmdline) }
}

#[no_mangle]
pub extern "C" fn kernel_interrupt_source_count() -> usize {
    params().interrupts.len()
}

/// # Safety
/// `name` is `name_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_interrupt_source(index: usize, name: *mut u8, name_len: usize) -> isize {
    let p = params();
    match p.interrupts.get(index) {
        Some((n, count)) => {
            // SAFETY: the caller's buffer.
            unsafe { put_c_string(name, name_len, n) };
            *count as isize
        }
        None => -1,
    }
}
