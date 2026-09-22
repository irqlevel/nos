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
//! What is here today is the first of the four steps
//! [`plans/03-hypervisor.md`](../../../plans/03-hypervisor.md) lays out: the
//! machine's extension, turned on for the CPUs that will run a guest and off
//! again when the module is taken out. The VM, its nested page tables, its
//! devices and the exit dispatcher come next, and go here.

extern crate alloc;

mod machine;

pub use hvarch::{Caps, Error, Ext, Result, Vendor};
pub use machine::{Machine, Refused};
