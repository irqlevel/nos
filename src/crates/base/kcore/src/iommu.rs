//! The IOMMU as hv reaches it (plans/gpu-passthrough.md): a domain a PCI
//! function is moved to -- an address space of its own, a guest's RAM as
//! the guest sees it -- through which every DMA of the function's goes.
//!
//! A [`Domain`] holds every [`Run`] it maps, and its `Drop` destroys the
//! domain -- every device in it blocked, its tables freed -- before it lets
//! go of them: whatever the owner gets wrong, no page a device may still
//! reach goes back to the free list while it can.

use alloc::vec::Vec;

use ffi::iommu;

use crate::consts::PAGE_SIZE;
use crate::frame::Run;

/// A function's requester id, as the IOMMU knows it: bus, device, function.
#[inline]
pub fn sid(bus: u16, slot: u16, func: u16) -> u16 {
    ((bus & 0xFF) << 8) | ((slot & 0x1F) << 3) | (func & 0x7)
}

/// The memory the firmware reserved for `sid`'s DMA, where it did (a DMAR
/// RMRR, whose scope is it or a bridge above it): its first byte and its
/// last. Such a device's DMA has to keep reaching the host's memory there,
/// and it is no guest's.
pub fn reserved(sid: u16) -> Option<(u64, u64)> {
    let (mut base, mut limit) = (0u64, 0u64);
    // SAFETY: both are locals, writable for the call.
    (unsafe { iommu::kernel_iommu_reserved(sid, &mut base, &mut limit) } != 0).then_some((base, limit))
}

/// Whether DMA is translated: `iommu=on`, and every unit came up.
#[inline]
pub fn on() -> bool {
    iommu::kernel_iommu_on() != 0
}

/// Whether interrupts are remapped as well: every interrupt is an entry of
/// its unit's table, verified to come from its own device, and a device's
/// DMA to the interrupt range raises its own vectors and no other.
#[inline]
pub fn remapping() -> bool {
    iommu::kernel_iommu_remapping() != 0
}

/// Why a domain did not do what it was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No domain to be had: DMA not translated, or no room.
    NoDomain,
    /// A page mapped already, past the domain's width, or no memory for a
    /// table: the domain is not to be used.
    Map,
    /// The function is not the domain's unit's, or in a domain already.
    Attach,
    /// No memory for what the domain holds.
    NoMemory,
}

/// A domain, and the pages it maps -- held until it is gone.
pub struct Domain {
    handle: usize,
    /// Every run mapped, a share of each held: dropped after the domain is
    /// destroyed, as the fields of a value are dropped after its `drop`.
    held: Vec<Run>,
    devices: Vec<u16>,
}

impl Domain {
    /// A domain for the unit that translates `sid`, the function it is
    /// made for -- which is not in it until [`Domain::attach`].
    pub fn new(sid: u16) -> Result<Domain, Error> {
        match iommu::kernel_iommu_domain_create(sid) {
            0 => Err(Error::NoDomain),
            handle => Ok(Domain { handle, held: Vec::new(), devices: Vec::new() }),
        }
    }

    /// `run`'s pages mapped in order from `iova`: a block of the huge pool
    /// one 2 MiB leaf where the unit walks those, a frame a 4 KiB one. A
    /// share of the run held first, so that what is mapped is never
    /// freed before the domain is gone.
    pub fn map(&mut self, iova: u64, run: &Run) -> Result<(), Error> {
        self.held.try_reserve(1).map_err(|_| Error::NoMemory)?;
        self.held.push(run.clone());
        let mut at = iova;
        for e in run.extents() {
            /* The run is held above until after the domain is destroyed in
             * `drop`: every page of it stays allocated for as long as a
             * device of the domain's can reach it. */
            if unsafe { iommu::kernel_iommu_domain_map(self.handle, at, e.phys, e.pages) } == 0 {
                return Err(Error::Map);
            }
            at = at.checked_add((e.pages * PAGE_SIZE) as u64).ok_or(Error::Map)?;
        }
        Ok(())
    }

    /// The function's DMA translated through the domain from now on.
    pub fn attach(&mut self, sid: u16) -> Result<(), Error> {
        self.devices.try_reserve(1).map_err(|_| Error::NoMemory)?;
        if iommu::kernel_iommu_domain_attach(self.handle, sid) == 0 {
            return Err(Error::Attach);
        }
        self.devices.push(sid);
        Ok(())
    }

    /// The function out of the domain, its DMA blocked -- not passed
    /// through: until it is reset it may hold a guest's addresses.
    pub fn detach(&mut self, sid: u16) -> bool {
        match self.devices.iter().position(|&d| d == sid) {
            Some(i) => {
                self.devices.swap_remove(i);
                iommu::kernel_iommu_domain_detach(self.handle, sid) != 0
            }
            None => false,
        }
    }

    /// The functions in the domain.
    pub fn devices(&self) -> &[u16] {
        &self.devices
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        /* Every device blocked and the tables freed here; `held` is dropped
         * after this body, and with it the domain's shares of its runs. */
        iommu::kernel_iommu_domain_destroy(self.handle);
    }
}

/// A function in no domain, blocked, passed through again: given back to
/// the host. False where it is a domain's, or the unit did not take it.
///
/// # Safety
/// The function must hold no address it may no longer reach -- it was
/// reset since it was a domain's, or never told one: passed through, it
/// reaches every byte of the machine.
pub unsafe fn release(sid: u16) -> bool {
    unsafe { iommu::kernel_iommu_release(sid) != 0 }
}
