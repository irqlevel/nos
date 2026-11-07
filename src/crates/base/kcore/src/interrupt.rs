use crate::callback;
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
    /// Have `handler(target)` called on every interrupt of `dev`'s line,
    /// level-triggered. The line may be shared: the handler is called for
    /// its neighbours' interrupts as well, and says by what it reads of its
    /// own device whether this one was its. It must NOT send an EOI -- the
    /// assembly stub does that. See `MsixInterrupt::register_for` for what a
    /// target and a handler are.
    ///
    /// Returns `None` if all 8 legacy slots are already in use.
    pub fn register_level_for<T, F>(
        dev: &PciDevice, target: &'static T, handler: F,
    ) -> Option<Self>
    where
        T: Sync + 'static,
        F: Fn(&'static T) + Copy + 'static,
    {
        Self::register_irq_for(dev.irq_line, target, handler)
    }

    /// The same by the interrupt's number, for a device found somewhere
    /// other than the PCI bus -- a virtio-mmio window, whose interrupt the
    /// device tree names.
    pub fn register_irq_for<T, F>(irq: u8, target: &'static T, handler: F) -> Option<Self>
    where
        T: Sync + 'static,
        F: Fn(&'static T) + Copy + 'static,
    {
        const { callback::assert_stateless::<F>() };
        let _shown = handler;

        let mut vector: u8 = 0;
        let handle = unsafe {
            interrupt::kernel_interrupt_register_level(
                irq, callback::trampoline::<T, F>, callback::ctx_of(target), &mut vector,
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
