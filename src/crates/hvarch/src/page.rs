//! The page a CPU needs before its extension can be turned on.

use kcore::dma::DmaBuffer;

use crate::{Error, Result};

/// One page of physically contiguous memory that belongs to a CPU for as
/// long as its virtualization extension is on: AMD's host state save area,
/// which `vmrun` writes the host's own state into, or Intel's VMXON region,
/// which the CPU keeps its root-operation state in. Neither is ever read
/// through this mapping -- only the physical address is handed over -- and
/// neither may be freed while the extension is on.
///
/// It exists as a type of its own for when it is allocated, not for what is
/// in it: the call that turns the extension on runs in interrupt context, on
/// the CPU it is for, where a page allocation would shoot down every other
/// CPU's TLB and wait for CPUs that have interrupts off and cannot answer.
/// So the page is made in task context first and the IPI carries nothing but
/// its address.
pub struct CpuPage {
    buf: DmaBuffer,
}

impl CpuPage {
    /// A zeroed page whose first word is `first_word` -- the VMX revision
    /// identifier, which the CPU checks against its own before it will enter
    /// root operation. Zero for an extension that wants nothing there.
    pub fn new(first_word: u32) -> Result<Self> {
        let mut buf = DmaBuffer::new(1).ok_or(Error::NoMemory)?;
        buf.as_mut_slice().fill(0);
        buf.as_mut_slice()[..4].copy_from_slice(&first_word.to_le_bytes());
        Ok(Self { buf })
    }

    /// What the CPU is told: the address, never the mapping.
    pub fn phys(&self) -> u64 {
        self.buf.phys()
    }
}
