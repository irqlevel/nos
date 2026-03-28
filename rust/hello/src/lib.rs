#![no_std]

extern crate alloc;

use alloc::vec;
use core::fmt::Write;

pub fn hello() {
    kcore::trace!(0, "Hello from Rust!");

    let v = vec![1u32, 2, 3];
    let sum: u32 = v.iter().sum();
    kcore::trace!(0, "Rust alloc works, vec sum = {}", sum);
}

fn test_time() {
    let boot = kcore::time::boot_time();
    kcore::trace!(0, "rust_test: boot_time={} ns wall={}",
        boot.as_nanos(), kcore::time::wall_clock_secs());
}

fn test_sync() {
    if let Some(m) = kcore::sync::Mutex::new() {
        let _g = m.lock();
        kcore::trace!(0, "rust_test: mutex acquired");
    }
}

fn test_worker() {
    kcore::trace!(0, "rust_test: task cpu_id={}", kcore::task::cpu_id());
    kcore::task::sleep_ms(50);
    kcore::trace!(0, "rust_test: task done");
}

fn test_task() {
    if let Some(h) = kcore::task::spawn(test_worker) {
        kcore::task::sleep_ms(10);
        drop(h);
    }
    kcore::trace!(0, "rust_test: task+sleep ok");
}

fn test_dma() {
    if let Some(mut dma) = kcore::dma::DmaBuffer::new(1) {
        let n = dma.len();
        {
            let sl = dma.as_mut_slice();
            sl[0] = 0xAB;
            sl[n - 1] = 0xCD;
        }
        let slro = dma.as_slice();
        kcore::trace!(0, "rust_test: dma pages={} first=0x{:02x} last=0x{:02x}",
            dma.pages(), slro[0], slro[n - 1]);
    }
}

fn test_random() {
    let mut rnd = [0u8; 4];
    if kcore::random::fill_random(&mut rnd) {
        kcore::trace!(0, "rust_test: random {:02x}{:02x}{:02x}{:02x}",
            rnd[0], rnd[1], rnd[2], rnd[3]);
    }
}

pub fn test() {
    kcore::trace!(0, "rust_test: begin");
    test_time();
    test_sync();
    test_task();
    test_dma();
    test_random();
    kcore::trace!(0, "rust_test: passed");
}
