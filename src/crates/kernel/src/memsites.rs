//! The boot test of memsites, the allocation-site tracker (mm/alloc_sites.h,
//! kernel/memsites.h), its Rust half: blocks Rust's allocator takes for one
//! function of a known name, `memsites_probe`, which test_alloc_sites finds
//! the tracker gave that function as their site -- through `Vec`'s growth,
//! Rust's allocator shims and the kernel's `GlobalAlloc` -- and none of them
//! there once they are freed.

use alloc::vec::Vec;
use kcore::sync::PreemptSpinLock;

/// The probe's blocks, from their allocation to their release. Taken out
/// under the lock and dropped after it: nothing is freed with a spin lock
/// held.
static HELD: PreemptSpinLock<Option<Vec<Vec<u8>>>> = PreemptSpinLock::new(None);

/// `count` blocks of `size` bytes and the one holding them, every one taken
/// here: the site test_alloc_sites looks for by this function's name.
#[inline(never)]
fn memsites_probe(count: usize, size: usize) -> Option<Vec<Vec<u8>>> {
    let mut blocks = Vec::new();
    blocks.try_reserve_exact(count).ok()?;
    for _ in 0..count {
        let mut block = Vec::new();
        block.try_reserve_exact(size).ok()?;
        block.resize(size, 0x5A);
        blocks.push(block);
    }
    Some(blocks)
}

/// The probe's blocks made and kept until `rust_memsites_release`: false
/// when there was no memory for them, or some are kept already.
#[no_mangle]
pub extern "C" fn rust_memsites_probe(count: usize, size: usize) -> bool {
    let Some(blocks) = memsites_probe(count, size) else {
        return false;
    };
    let refused = {
        let mut held = HELD.lock();
        if held.is_some() {
            Some(blocks)
        } else {
            *held = Some(blocks);
            None
        }
    };
    let made = refused.is_none();
    drop(refused);
    made
}

/// The probe's blocks freed, if it has any.
#[no_mangle]
pub extern "C" fn rust_memsites_release() {
    let blocks = HELD.lock().take();
    drop(blocks);
}
