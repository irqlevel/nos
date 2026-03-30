use crate::pci::PciDevice;
use ffi::interrupt;

/// A registered legacy level-triggered PCI interrupt (INTx).
///
/// The interrupt is unregistered (slot freed) when this handle is dropped.
pub struct LegacyInterrupt {
    handle: usize,
    vector: u8,
}

impl LegacyInterrupt {
    /// Construct a no-op placeholder handle (handle == 0).
    /// Drop of an empty interrupt is a no-op.
    /// Use as a field initialiser when the real interrupt is registered later.
    pub fn empty() -> Self {
        Self { handle: 0, vector: 0 }
    }

    /// Register a level-triggered PCI interrupt for `dev`.
    ///
    /// `handler` is called on every interrupt with `ctx` as its argument.
    /// The handler must NOT send a LAPIC EOI — the assembly stub does that.
    ///
    /// Returns `None` if all 8 legacy slots are already in use.
    pub fn register_level(
        dev: &PciDevice,
        handler: extern "C" fn(*mut u8),
        ctx: *mut u8,
    ) -> Option<Self> {
        let mut vector: u8 = 0;
        let handle = unsafe {
            interrupt::kernel_interrupt_register_level(
                dev.irq_line,
                handler,
                ctx,
                &mut vector,
            )
        };
        if handle == 0 {
            None
        } else {
            Some(Self { handle, vector })
        }
    }

    /// The CPU interrupt vector assigned to this slot.
    pub fn vector(&self) -> u8 {
        self.vector
    }
}

impl Drop for LegacyInterrupt {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { interrupt::kernel_interrupt_unregister(self.handle) }
        }
    }
}
