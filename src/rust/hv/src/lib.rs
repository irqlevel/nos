#![no_std]

//! The hypervisor, less the CPU.
//!
//! `hvarch` below this crate is the CPU's virtualization extension and the
//! only `unsafe` in the hypervisor; everything here is ordinary safe Rust,
//! and that is deliberate rather than incidental. The long-term goal of this
//! kernel is other people's Linux guests on this machine, and the bug class
//! that goal cannot survive is a guest reaching host memory. So the line is
//! drawn once, in the crate graph, where a script can count it:
//!
//! ```text
//! scripts/unsafe-count.py hv hvarch
//! ```
//!
//! Three sites are left here: the two IPI handlers that turn the extension
//! on and off, and the one call that enters a guest (`Vm::enter`), whose
//! preconditions are two invariants this crate keeps by construction --
//! guest memory owns every page its nested table maps, and the machine
//! never names a host save area that has been freed.
//!
//! What is here is the first two of the four steps
//! [`plans/03-hypervisor.md`](../../../plans/03-hypervisor.md) lays out:
//! the machine's extension, turned on for the CPUs that will run a guest and
//! off again when the module is taken out; and a VM -- guest memory behind a
//! nested page table, one CPU under AMD-V, the exits decoded -- that runs
//! the built-in guests of [`guests`]. Intel's VMCS and Arm's EL2 come later,
//! under the same names.

extern crate alloc;

mod machine;
mod memory;
mod devices;
#[cfg(target_arch = "x86_64")]
mod npt;
#[cfg(target_arch = "x86_64")]
pub mod svm;
#[cfg(target_arch = "x86_64")]
pub mod vm;
#[cfg(target_arch = "x86_64")]
pub mod guests;

pub use hvarch::{Caps, Error, Ext, Result, Vendor};
pub use machine::{Machine, Refused};
pub use devices::Uart;
pub use memory::GuestMemory;

/// No guest runs on this architecture yet: the extension is EL2, which this
/// kernel leaves in its first instructions (`hvarch::arm64`).
#[cfg(not(target_arch = "x86_64"))]
pub mod guests {
    use core::fmt::Write;

    use crate::Machine;

    pub fn names() -> impl Iterator<Item = &'static str> {
        core::iter::empty()
    }

    pub fn run_one(_machine: &Machine, _name: &str, _out: &mut dyn Write) -> Option<bool> {
        None
    }
}
