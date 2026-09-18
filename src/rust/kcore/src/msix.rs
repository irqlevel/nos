use crate::callback;
use crate::pci::PciDevice;
use ffi::msix;

pub struct MsixTable {
    handle: usize,
}

impl MsixTable {
    pub fn new(dev: &PciDevice) -> Option<Self> {
        let h = unsafe {
            msix::kernel_msix_create(dev.bus, dev.slot, dev.func, core::ptr::null())
        };
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    pub fn new_with_bars(dev: &PciDevice, mapped_bars: &[u64; 6]) -> Option<Self> {
        let h = unsafe {
            msix::kernel_msix_create(
                dev.bus, dev.slot, dev.func, mapped_bars.as_ptr())
        };
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    pub fn mask(&self, index: u16) {
        unsafe { msix::kernel_msix_mask(self.handle, index) }
    }

    pub fn unmask(&self, index: u16) {
        unsafe { msix::kernel_msix_unmask(self.handle, index) }
    }

    pub fn table_size(&self) -> u16 {
        unsafe { msix::kernel_msix_table_size(self.handle) }
    }

    pub fn is_ready(&self) -> bool {
        unsafe { msix::kernel_msix_is_ready(self.handle) != 0 }
    }
}

impl Drop for MsixTable {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { msix::kernel_msix_destroy(self.handle) }
        }
    }
}

/// A registered MSI-X interrupt backed by a kernel callback slot.
///
/// The assembly stub handles register save/restore and LAPIC EOI.
/// The interrupt slot is freed when this handle is dropped.
pub struct MsixInterrupt {
    slot_handle: usize,
    vector: u8,
}

impl MsixInterrupt {
    /// Have `handler(target)` called for MSI-X table entry `msix_index`: from
    /// the assembly stub's ISR context, registers saved and the EOI sent
    /// after it returns. The target is something that lives for good -- a
    /// device, registered for the life of the kernel -- and the handler a
    /// function item, so there is no context pointer to cast either way.
    ///
    /// An interrupt runs on whichever CPU it is routed to, and that can move:
    /// what the handler touches of the target it must be able to touch with
    /// anything else running, itself included -- registers, atomics, and
    /// locks that take interrupts off.
    ///
    /// Returns `None` if all 16 MSI-X callback slots are in use or
    /// if `MsixTable::EnableVector` fails (no free CPU vectors).
    pub fn register_for<T, F>(
        table: &MsixTable, msix_index: u16, target: &'static T, handler: F,
    ) -> Option<Self>
    where
        T: Sync + 'static,
        F: Fn(&'static T) + Copy + 'static,
    {
        const { callback::assert_stateless::<F>() };
        let _shown = handler;
        Self::register(table, msix_index, callback::trampoline::<T, F>, callback::ctx_of(target))
    }

    fn register(
        table: &MsixTable,
        msix_index: u16,
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
    ) -> Option<Self> {
        let mut vector: u8 = 0;
        let h = unsafe {
            msix::kernel_msix_register_handler(
                table.handle, msix_index, handler, ctx, &mut vector,
            )
        };
        if h == 0 { None } else { Some(Self { slot_handle: h, vector }) }
    }

    pub fn vector(&self) -> u8 {
        self.vector
    }
}

impl Drop for MsixInterrupt {
    fn drop(&mut self) {
        if self.slot_handle != 0 {
            unsafe { msix::kernel_msix_unregister_handler(self.slot_handle) }
        }
    }
}
