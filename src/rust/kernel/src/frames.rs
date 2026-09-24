//! The boot's check of frames -- pages mapped nowhere, which a guest's memory
//! is made of -- written and read back through the kernel's temporary window:
//! what every copy of a guest's memory goes through (`FrameCopy` in
//! kernel/rust_ffi.cpp), and each CPU with a slot of the window of its own for
//! it (`PageTable::MapFrameSlot`). So every CPU reads the frame back through
//! its own slot. A failure panics, as the boot's other self-tests do.

use core::sync::atomic::{AtomicU64, Ordering};

use kcore::consts::{MAX_CPUS, PAGE_SIZE};
use kcore::frame::Frame;
use kcore::trace;

/// The byte the test writes at `at`: a different run on every line of the
/// page, and never zero, which is what a fresh frame holds.
fn pattern(at: usize) -> u8 {
    (at % 251) as u8 + 1
}

/// Piece sizes the page is written in, round and round: a byte, words
/// across and off a word's edge, and a run longer than the word loop's.
const PIECES: [usize; 7] = [1, 7, 8, 63, 64, 100, 511];

/// What each CPU reads back, and which of them found something else.
struct Check<'a> {
    frame: &'a Frame,
    bad: AtomicU64,
}

/// Read the whole frame back on this CPU and compare: in interrupt context,
/// on the stack.
fn check_here(check: &Check<'_>) {
    let mut buf = [0u8; 256];
    let mut at = 0;
    while at < PAGE_SIZE {
        let n = buf.len().min(PAGE_SIZE - at);
        let read = check.frame.read(at, &mut buf[..n]);
        if !read || buf[..n].iter().enumerate().any(|(i, &b)| b != pattern(at + i)) {
            check.bad.fetch_or(1u64 << (kcore::cpu::id() % u64::BITS), Ordering::Relaxed);
            return;
        }
        at += n;
    }
}

pub fn selftest() {
    let Some(mut frame) = Frame::new() else {
        panic!("frame selftest: no frame to test with");
    };

    /* Handed out zeroed. */
    let mut probe = [0xFFu8; 64];
    for at in [0, PAGE_SIZE / 2, PAGE_SIZE - probe.len()] {
        assert!(frame.read(at, &mut probe) && probe.iter().all(|&b| b == 0),
            "frame selftest: a fresh frame is not zeroed at {}", at);
    }

    /* The page written a piece at a time, at every alignment the sizes
     * make. */
    let mut piece = [0u8; 512];
    let mut at = 0;
    let mut k = 0;
    while at < PAGE_SIZE {
        let n = PIECES[k % PIECES.len()].min(PAGE_SIZE - at);
        for (i, b) in piece[..n].iter_mut().enumerate() {
            *b = pattern(at + i);
        }
        assert!(frame.write(at, &piece[..n]), "frame selftest: a write of {} at {} refused", n, at);
        at += n;
        k += 1;
    }

    /* Past the page's end is refused whole: nothing of it written. */
    assert!(!frame.write(PAGE_SIZE - 1, &[0, 0]), "frame selftest: a write past the page taken");
    let mut last = [0u8; 1];
    assert!(frame.read(PAGE_SIZE - 1, &mut last) && last[0] == pattern(PAGE_SIZE - 1),
        "frame selftest: a refused write changed the page");

    /* Read back on every running CPU, each through its own slot. */
    let check = Check { frame: &frame, bad: AtomicU64::new(0) };
    let online = kcore::cpu::online_mask();
    for cpu in 0..MAX_CPUS as u32 {
        if online & (1u64 << cpu) != 0 {
            kcore::cpu::run_on_with(cpu, &check, check_here);
        }
    }
    let bad = check.bad.load(Ordering::Relaxed);
    assert!(bad == 0, "frame selftest: cpus {:#x} read the frame back wrong", bad);

    trace!(0, "frame selftest: passed on cpus {:#x}", online);
}
