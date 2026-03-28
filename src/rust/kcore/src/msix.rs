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

    /// Program MSI-X table entry `index` and install `isr_fn` into the IDT.
    /// Returns the allocated CPU vector, or `None` on failure.
    ///
    /// # Safety
    ///
    /// `isr_fn` must be a valid ISR entry point that saves/restores
    /// registers, sends LAPIC EOI, and returns via `iretq`.
    pub unsafe fn enable_vector(
        &self, index: u16, isr_fn: unsafe extern "C" fn(),
    ) -> Option<u8> {
        let v = msix::kernel_msix_enable_vector(self.handle, index, isr_fn);
        if v == 0 { None } else { Some(v) }
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

    pub fn disarm(&mut self) {
        if self.handle != 0 {
            unsafe { msix::kernel_msix_destroy(self.handle) }
            self.handle = 0;
        }
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
    /// An empty handle that owns no interrupt slot.
    /// Drop is a safe no-op (slot_handle == 0).
    pub fn empty() -> Self {
        Self { slot_handle: 0, vector: 0 }
    }

    /// Register a Rust callback for MSI-X table entry `msix_index`.
    ///
    /// `handler(ctx)` will be called from the assembly stub's ISR context
    /// (registers saved, LAPIC EOI sent automatically after return).
    ///
    /// Returns `None` if all 16 MSI-X callback slots are in use or
    /// if `MsixTable::EnableVector` fails (no free CPU vectors).
    pub fn register(
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

    /// Unregister the interrupt and disarm the implicit Drop.
    pub fn disarm(&mut self) {
        if self.slot_handle != 0 {
            unsafe { msix::kernel_msix_unregister_handler(self.slot_handle) }
            self.slot_handle = 0;
        }
    }
}

impl Drop for MsixInterrupt {
    fn drop(&mut self) {
        if self.slot_handle != 0 {
            unsafe { msix::kernel_msix_unregister_handler(self.slot_handle) }
        }
    }
}
