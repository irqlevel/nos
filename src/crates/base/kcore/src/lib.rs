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
pub mod chunked;
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

#[doc(hidden)]
pub use ffi as __ffi;

/// A line of the kernel log, `klog!(level, "fmt {}", arg)`: `ffi::klog!`,
/// whose body is the one copy -- this was a second, word for word.
#[macro_export]
macro_rules! klog {
    ($($arg:tt)*) => {
        $crate::__ffi::klog!($($arg)*)
    };
}
