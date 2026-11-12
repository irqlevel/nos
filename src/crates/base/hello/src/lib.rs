#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicBool, Ordering};

pub fn hello() {
    kcore::klog!(0, "Hello from Rust!");

    let v = vec![1u32, 2, 3];
    let sum: u32 = v.iter().sum();
    kcore::klog!(0, "Rust alloc works, vec sum = {}", sum);
}

/* Each check a panic naming what failed, as the other self-tests rust_test
 * runs are: this one once logged and said "passed" whatever happened -- a
 * Mutex, a spawn or a DMA buffer that could not be had, or bytes written
 * and not read back, all read as a pass. */

fn fail(what: &str) -> ! {
    panic!("rust_test: {}", what);
}

fn test_time() {
    let boot = kcore::time::boot_time();
    if boot.as_nanos() == 0 {
        fail("boot_time is 0");
    }
    kcore::klog!(0, "rust_test: boot_time={} ns wall={}",
        boot.as_nanos(), kcore::time::wall_clock_secs());
}

fn test_sync() {
    let Some(m) = kcore::sync::Mutex::new(0u32) else { fail("no Mutex") };
    {
        let mut g = m.lock();
        *g += 1;
    }
    if *m.lock() != 1 {
        fail("a Mutex's data not as left");
    }
}

/// What the spawned task sets: that it ran.
struct Ran(AtomicBool);

fn test_worker(ran: Arc<Ran>) {
    kcore::task::sleep_ms(10);
    ran.0.store(true, Ordering::Release);
}

fn test_task() {
    let ran = Arc::new(Ran(AtomicBool::new(false)));
    let Some(h) = kcore::task::spawn_with("rust_test", ran.clone(), test_worker) else { fail("no task") };
    /* The handle's drop joins the task */
    drop(h);
    if !ran.0.load(Ordering::Acquire) {
        fail("the task joined and never ran");
    }
}

fn test_dma() {
    let Some(mut dma) = kcore::dma::DmaBuffer::new(1) else { fail("no DMA buffer") };
    let n = dma.len();
    if n == 0 || dma.pages() != 1 {
        fail("a DMA buffer of one page is not one");
    }
    {
        let sl = dma.as_mut_slice();
        sl[0] = 0xAB;
        sl[n - 1] = 0xCD;
    }
    let slro = dma.as_slice();
    if slro[0] != 0xAB || slro[n - 1] != 0xCD {
        fail("a DMA buffer's bytes not as written");
    }
}

fn test_random() {
    let mut rnd = [0u8; 16];
    /* A pool nothing has seeded gives nothing, which the boot has said
     * already ("Random: unseeded at boot"): no bytes to look at */
    if !kcore::random::fill_random(&mut rnd) {
        kcore::klog!(0, "rust_test: the random pool is unseeded -- its bytes not looked at");
        return;
    }
    if rnd.iter().all(|&b| b == rnd[0]) {
        fail("the random pool gave sixteen bytes of one value");
    }
}

pub fn test() {
    kcore::klog!(0, "rust_test: begin");
    test_time();
    test_sync();
    test_task();
    test_dma();
    test_random();
    kcore::klog!(0, "rust_test: passed");
}
