//! ATA disks behind a PCI IDE controller: SATA in IDE mode -- an Intel ICH's,
//! as on the ASUS RS500 whose BIOS left its SATA there -- and the parallel
//! ATA of a PIIX, which is what QEMU's `pc` machine gives a guest.
//!
//! A controller is driven through its taskfile: each channel's command block
//! of eight registers and its device control register, one command at a time
//! per channel, the two devices on it -- master and slave -- taking turns
//! under the channel's lock. Disks are named by where they are connected,
//! `hda` the first controller's primary master to `hdd` its secondary slave,
//! `hde` on for the next controller's, so that a name says the same port
//! whatever else is or is not plugged in.
//!
//! Data moves by PIO, polled, with the devices' interrupt off (nIEN): an IDE
//! function has no MSI, and the line its PCI header names is the one a PIC
//! would take -- the ICH10R's says 11 where its IO-APIC pin is 18 -- which
//! only ACPI's `_PRT` tells, and nothing here reads it yet. Polling needs
//! neither, and a disk is a disk before an interrupt is anything.
//!
//! A command that fails is said, and the channel reset (SRST, both its
//! devices). A device that does not come back from the reset, or
//! `ataproto::MAX_UNFINISHED` commands running that did not finish, is
//! failed: said once, every request of it refused at once from then on, and
//! no longer waited for by the channel's resets -- rather than each request
//! waiting out a timeout and a reset of its own. At power-off each disk is
//! flushed and sent STANDBY IMMEDIATE.
//!
//! Left for later: bus-master DMA (its PRD table addresses 32 bits, so its
//! buffers have to be below 4 GiB, which the allocator has no way to ask
//! for yet), the asynchronous path (`submit`), TRIM, the interrupt once
//! `_PRT` is read.
//!
//! x86-64 only: the registers are I/O ports. On arm64 the entry points do
//! nothing.

#![no_std]

extern crate alloc;

#[cfg(target_arch = "x86_64")]
mod x86;

/// The PCI IDE controllers, their channels and the disks on them, taken and
/// registered with the block layer. From `rust_init`, before the second
/// partition probe.
pub fn init() {
    #[cfg(target_arch = "x86_64")]
    x86::init();
}

/// On the way down, from `rust_fini`: every disk's write cache flushed, and
/// with `power_off` each disk put in standby for the power to go.
pub fn shutdown(power_off: bool) {
    #[cfg(target_arch = "x86_64")]
    x86::shutdown(power_off);
    #[cfg(not(target_arch = "x86_64"))]
    let _ = power_off;
}
