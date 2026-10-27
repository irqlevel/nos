//! The hypervisor's devices, from their own files (hv/src/devices.rs has them
//! for x86-64 only; these are built for whichever host runs the fuzzer).

#[path = "../../../src/crates/hv/src/devices/blk.rs"]
pub mod blk;
#[path = "../../../src/crates/hv/src/devices/ioapic.rs"]
pub mod ioapic;
#[path = "../../../src/crates/hv/src/devices/net.rs"]
pub mod net;
#[path = "../../../src/crates/hv/src/devices/offload.rs"]
pub mod offload;
#[path = "../../../src/crates/hv/src/devices/pci.rs"]
pub mod pci;
#[path = "../../../src/crates/hv/src/devices/pic.rs"]
pub mod pic;
#[path = "../../../src/crates/hv/src/devices/pit.rs"]
pub mod pit;
#[path = "../../../src/crates/hv/src/devices/pm.rs"]
pub mod pm;
#[path = "../../../src/crates/hv/src/devices/pvpanic.rs"]
pub mod pvpanic;
#[path = "../../../src/crates/hv/src/devices/rtc.rs"]
pub mod rtc;
#[path = "../../../src/crates/hv/src/devices/uart.rs"]
pub mod uart;
#[path = "../../../src/crates/hv/src/devices/virtio.rs"]
pub mod virtio;

pub use ioapic::IoApic;
pub use pic::Pic;
pub use pit::Pit;
pub use pm::Pm;
pub use rtc::Rtc;
pub use uart::Uart;
