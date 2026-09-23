//! What a guest reaches instead of the host's hardware: the devices this
//! hypervisor emulates over port I/O and MSR exits.
//!
//! Each is plain data -- a handful of registers -- because stage 5's live
//! update needs every device to be serialisable, and because a device that
//! held a pointer into kernel memory would be a way for a guest to reach it.
//! The guest touches none of them directly: it stops at an intercept, and
//! the run loop hands the exit to the device.

/* The PC chipset's devices -- the 8259, the 8254, the MC146818 -- are an x86
 * guest's; an arm64 one would have a GIC, a generic timer and a PL011. The
 * 8250 is anybody's serial port. */
#[cfg(target_arch = "x86_64")]
pub mod pic;
#[cfg(target_arch = "x86_64")]
pub mod pit;
#[cfg(target_arch = "x86_64")]
pub mod rtc;
pub mod uart;

#[cfg(target_arch = "x86_64")]
pub use pic::Pic;
#[cfg(target_arch = "x86_64")]
pub use pit::Pit;
#[cfg(target_arch = "x86_64")]
pub use rtc::Rtc;
pub use uart::Uart;
