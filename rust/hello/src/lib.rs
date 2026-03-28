#![no_std]

extern crate alloc;

use alloc::vec;
use core::fmt::Write;

fn kcore_demo_worker() {
    kcore::trace!(0, "kcore task: cpu_id={}", kcore::task::cpu_id());
    kcore::task::sleep_ms(50);
    kcore::trace!(0, "kcore task: done");
}

pub fn hello() {
    kcore::trace!(0, "Hello from Rust!");

    let boot = kcore::time::boot_time();
    let mut tb = kcore::trace::__TraceBuf::new();
    let _ = write!(
        tb,
        "kcore boot_time={} ns wall={}",
        boot.as_nanos(),
        kcore::time::wall_clock_secs()
    );
    kcore::trace::trace(0, tb.as_str());

    if let Some(m) = kcore::sync::Mutex::new() {
        let _g = m.lock();
        let mut buf2 = kcore::trace::__TraceBuf::new();
        let _ = write!(buf2, "kcore mutex acquired");
        kcore::trace::trace(0, buf2.as_str());
    }

    if let Some(h) = kcore::task::spawn(kcore_demo_worker) {
        kcore::task::sleep_ms(10);
        drop(h);
    }

    if let Some(mut dma) = kcore::dma::DmaBuffer::new(1) {
        let n = dma.len();
        {
            let sl = dma.as_mut_slice();
            sl[0] = 0xAB;
            sl[n - 1] = 0xCD;
        }
        let slro = dma.as_slice();
        let mut buf3 = kcore::trace::__TraceBuf::new();
        let _ = write!(
            buf3,
            "kcore dma pages={} first=0x{:02x} last=0x{:02x}",
            dma.pages(),
            slro[0],
            slro[n - 1]
        );
        kcore::trace::trace(0, buf3.as_str());
    }

    let mut rnd = [0u8; 4];
    if kcore::random::fill_random(&mut rnd) {
        let mut buf4 = kcore::trace::__TraceBuf::new();
        let _ = write!(
            buf4,
            "kcore random {:02x}{:02x}{:02x}{:02x}",
            rnd[0], rnd[1], rnd[2], rnd[3]
        );
        kcore::trace::trace(0, buf4.as_str());
    }

    let v = vec![1u32, 2, 3];
    let sum: u32 = v.iter().sum();

    let mut buf = kcore::trace::__TraceBuf::new();
    let _ = write!(buf, "Rust alloc works, vec sum = {}", sum);
    kcore::trace::trace(0, buf.as_str());
}
