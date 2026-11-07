#![no_std]

extern crate alloc;

pub mod barrier;
mod callback;
pub mod const_init;
pub mod consts;
pub mod error;
pub mod klog;
pub mod time;
pub mod sync;
pub mod task;
pub mod tcp;
pub mod http;
pub mod io;
pub mod dma;
pub mod frame;
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
pub mod trace;
pub mod cpu;
pub mod block;
pub mod net;
pub mod bitmap;
pub mod ring;
pub mod static_ring;
pub mod hpet;
pub mod acpi;
pub mod cmd;
pub mod mgmt;
pub mod ipc;
pub mod events;
pub mod system;
pub mod fs;
mod lent;
pub mod vnic;
#[cfg(target_arch = "x86_64")]
pub mod tco_wdt;

#[macro_export]
macro_rules! klog {
    ($level:expr, $($arg:tt)*) => {{
        let level: u32 = $level;
        /* The level first: a line is formatted -- core::fmt, into 512 bytes
         * of stack -- only when the log keeps it. A klog!(3, ..) on a hot
         * path used to be formatted at every level and thrown away by the
         * C++ side's own look at it. */
        if $crate::klog::wants(level) {
            use core::fmt::Write;
            let mut buf = $crate::klog::__KlogBuf::new();
            let f = file!();
            let fname = match f.rfind('/') {
                Some(i) => &f[i + 1..],
                None => f,
            };
            let _ = write!(buf, "{}(),{},{}: ", module_path!(), fname, line!());
            let _ = write!(buf, $($arg)*);
            $crate::klog::klog(level, buf.as_str());
        }
    }};
}
