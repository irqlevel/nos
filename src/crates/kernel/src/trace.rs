//! The boot's check of Rust's tracepoints (`kcore::trace`): every payload this
//! side declares held to the kernel's description of its event -- its name,
//! its size, every field's name, offset and size, both ways -- on every boot
//! of either architecture, so that a field added to one side alone is a
//! failed boot rather than records printed wrong. A failure panics, as the
//! boot's other self-tests do.
//!
//! And the records themselves: kernel/test.cpp's test_tracepoints has Rust
//! record a hundred of the test event (`rust_trace_emit_test`) into the ring
//! it reads them back from -- the path a module's sites take, through the
//! flags' address kcore asks once and the kernel's emit().

use kcore::klog;
use kcore::trace::{self, Test};

/// What the value of a test record Rust writes is: its sequence number with
/// this, so that it is told from the C++ side's (kernel/test.cpp says the same).
const TEST_SALT: u64 = 0xA5A5_A5A5_A5A5_A5A5;

pub fn selftest() {
    match trace::check() {
        Ok(()) => klog!(0, "trace: {} Rust payloads as the kernel describes their events", trace::EVENTS.len()),
        Err((name, mismatch)) => {
            panic!("trace: Rust's payload of {} is not as the kernel describes the event: {:?}", name, mismatch)
        }
    }
}

/// `n` records of the test event from Rust, the `i`th of sequence number
/// `base + i` and value `(base + i) ^ TEST_SALT`: how many it wrote -- none
/// once the event is off. The caller has interrupts off, so that they all go
/// into its CPU's ring.
#[no_mangle]
pub extern "C" fn rust_trace_emit_test(base: u64, n: u64) -> u64 {
    let mut emitted = 0;
    for i in 0..n {
        if !trace::on::<Test>() {
            break;
        }
        let seq = base.wrapping_add(i);
        trace::emit(&Test { seq, value: seq ^ TEST_SALT });
        emitted += 1;
    }
    emitted
}
