//! What a guest reaches instead of the host's hardware: the devices this
//! hypervisor emulates over port I/O and MSR exits.
//!
//! Each is plain data -- a handful of registers -- because stage 5's live
//! update needs every device to be serialisable, and because a device that
//! held a pointer into kernel memory would be a way for a guest to reach it.
//! The guest touches none of them directly: it stops at an intercept, and
//! the run loop hands the exit to the device.

pub mod pic;
pub mod pit;
pub mod rtc;
pub mod uart;

pub use pic::Pic;
pub use pit::Pit;
pub use rtc::Rtc;
pub use uart::Uart;
