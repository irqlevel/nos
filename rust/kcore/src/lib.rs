#![no_std]

extern crate alloc;

pub mod consts;
pub mod trace;
pub mod time;
pub mod sync;
pub mod task;
pub mod io;
pub mod dma;
pub mod random;
pub mod pci;
pub mod msix;
pub mod interrupt;
pub mod softirq;
pub mod timer;
pub mod cpu;
pub mod block;
pub mod net;

#[macro_export]
macro_rules! trace {
    ($level:expr, $($arg:tt)*) => {{
        use core::fmt::Write;
        let mut buf = $crate::trace::__TraceBuf::new();
        let _ = write!(buf, $($arg)*);
        $crate::trace::trace($level, buf.as_str());
    }};
}
