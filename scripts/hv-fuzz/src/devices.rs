//! The hypervisor's devices, from their own files (hv/src/devices.rs has them
//! for x86-64 only; these are built for whichever host runs the fuzzer).

#[path = "../../../src/rust/hv/src/devices/blk.rs"]
pub mod blk;
#[path = "../../../src/rust/hv/src/devices/ioapic.rs"]
pub mod ioapic;
#[path = "../../../src/rust/hv/src/devices/net.rs"]
pub mod net;
#[path = "../../../src/rust/hv/src/devices/pci.rs"]
pub mod pci;
#[path = "../../../src/rust/hv/src/devices/pic.rs"]
pub mod pic;
#[path = "../../../src/rust/hv/src/devices/pit.rs"]
pub mod pit;
#[path = "../../../src/rust/hv/src/devices/pm.rs"]
pub mod pm;
#[path = "../../../src/rust/hv/src/devices/rtc.rs"]
pub mod rtc;
#[path = "../../../src/rust/hv/src/devices/uart.rs"]
pub mod uart;
#[path = "../../../src/rust/hv/src/devices/virtio.rs"]
pub mod virtio;

pub use ioapic::IoApic;
pub use pic::Pic;
pub use pit::Pit;
pub use pm::Pm;
pub use rtc::Rtc;
pub use uart::Uart;
