//! The machine the channel runs on: the fuzzers' common one
//! (`common::machine`: the kernel's C++ half, the CPUs that run its tasks
//! one at a time, its allocator), and what the channel adds to it -- which
//! module holds an address (`kernel_module_name_at`), answered from a table
//! of made-up modules at made-up addresses.

pub use crate::common::machine::*;

/// The made-up modules: where each one's image is, how long, its name.
pub const MODULES: [(usize, usize, &str); 3] =
    [(0x1000_0000, 0x1000, "nbs-mds"), (0x2000_0000, 0x1000, "nbs-cs"), (0x3000_0000, 0x1000, "modtest")];

/// The name of the made-up module whose image holds `addr`: what the module
/// table answers the channel (ModuleTable::name_at).
///
/// # Safety
/// `buf` is writable for `size` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_module_name_at(addr: usize, buf: *mut u8, size: usize) -> usize {
    sched::harness(|| {
        for (base, len, name) in MODULES {
            if addr >= base && addr - base < len {
                let n = name.len().min(size);
                // SAFETY: `buf` takes `size` bytes, and `n` is no more.
                unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), buf, n) };
                return n;
            }
        }
        0
    })
}

/// Boots the machine, once, in the parent every input's process is forked
/// from: the channel's table made and its command in the table, as
/// `rust_init` does it. No thread is made and none parked: see
/// `sched::boot`.
pub fn boot() {
    /* Every big allocation of the fuzzer's own is marked as its own: the
     * kernel's code is held to what the kernel's heap gives at once. */
    heap::hold_to_ceiling();
    sched::boot();
    ipc::init();
}
