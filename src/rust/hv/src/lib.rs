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
//! What is here: the machine's extension, turned on for the CPUs that will
//! run a guest and off again when the module is taken out; a VM -- guest
//! memory behind a nested page table, a CPU under AMD-V or Intel VT-x, the
//! exits decoded -- that runs the built-in guests of [`guests`]; and a PC
//! for a Linux guest of one CPU or several ([`run`]), with the devices it
//! boots with, a local APIC for each CPU ([`lapic`]), and what its CPUs
//! reach each other by ([`smp`]). Arm's EL2 comes later, under the same
//! names.

extern crate alloc;

mod machine;
mod memory;
mod devices;
#[cfg(target_arch = "x86_64")]
pub mod lapic;
#[cfg(target_arch = "x86_64")]
pub mod smp;
#[cfg(target_arch = "x86_64")]
mod npt;
#[cfg(target_arch = "x86_64")]
mod ept;
#[cfg(target_arch = "x86_64")]
pub mod svm;
#[cfg(target_arch = "x86_64")]
pub mod vmx;
#[cfg(target_arch = "x86_64")]
pub mod vm;
#[cfg(target_arch = "x86_64")]
pub mod linux;
#[cfg(target_arch = "x86_64")]
pub mod policy;
#[cfg(target_arch = "x86_64")]
pub mod run;
#[cfg(target_arch = "x86_64")]
pub mod guests;

/// A guest's disks: what stores one (`Backend`), and what it counts.
#[cfg(target_arch = "x86_64")]
pub use devices::blk as disk;
/// A guest's NICs: what carries one's frames (`Backend`), and what it counts.
#[cfg(target_arch = "x86_64")]
pub use devices::net as nic;
/// How what the host hands a running guest -- a frame -- gets it to leave
/// its guest and take it, rather than wait for the host's next interrupt.
#[cfg(target_arch = "x86_64")]
pub use hvarch::x86::svm::Kick;
/// What wakes each CPU of a guest: its task out of a wait, its guest out of
/// the CPU (`Kick`).
#[cfg(target_arch = "x86_64")]
pub use smp::{Doorbell, Doorbells, MAX_VCPUS};

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

    pub fn run_one(_machine: &alloc::sync::Arc<Machine>, _name: &str, _out: &mut dyn Write) -> Option<bool> {
        None
    }
}
