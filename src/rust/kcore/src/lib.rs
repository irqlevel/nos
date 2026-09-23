#![no_std]

extern crate alloc;

pub mod barrier;
mod callback;
pub mod const_init;
pub mod consts;
pub mod crc32;
pub mod error;
pub mod trace;
pub mod time;
pub mod sync;
pub mod task;
pub mod tcp;
pub mod io;
pub mod dma;
pub mod frame;
pub mod entropy;
pub mod procinfo;
pub mod random;
pub mod once;
pub mod pci;
pub mod percpu;
pub mod pod;
pub mod msix;
pub mod input;
pub mod interrupt;
pub mod softirq;
pub mod timer;
pub mod cpu;
pub mod block;
pub mod net;
pub mod bitmap;
pub mod ring;
pub mod static_ring;
pub mod hpet;
pub mod acpi;
pub mod cmd;
pub mod fs;
pub mod vnic;
#[cfg(target_arch = "x86_64")]
pub mod tco_wdt;

#[macro_export]
macro_rules! trace {
    ($level:expr, $($arg:tt)*) => {{
        use core::fmt::Write;
        let mut buf = $crate::trace::__TraceBuf::new();
        let f = file!();
        let fname = match f.rfind('/') {
            Some(i) => &f[i + 1..],
            None => f,
        };
        let _ = write!(buf, "{}(),{},{}: ", module_path!(), fname, line!());
        let _ = write!(buf, $($arg)*);
        $crate::trace::trace($level, buf.as_str());
    }};
}
