//! virtio: the queue, the bus contracts, and the bring-up every virtio
//! driver does before it can talk to its device.
//!
//! A device driver on top of this deals in buffers and completions; which
//! bus it is on -- virtio-pci modern, virtio-pci legacy, virtio-mmio -- is
//! behind [`Transport`], and the handshake that gets a device from reset to
//! DRIVER_OK is [`negotiate`] plus [`driver_ok`].

#![no_std]

extern crate alloc;

pub mod mmio;
#[cfg(target_arch = "x86_64")]
pub mod pci;
pub mod queue;
pub mod transport;

use kcore::trace;

pub use queue::{Buf, Queue, MAX_DESCRIPTORS};
pub use transport::{
    driver_ok, failed, negotiate, QueueLayout, Transport, FEATURE_VERSION_1, STATUS_ACKNOWLEDGE,
    STATUS_DRIVER, STATUS_DRIVER_OK, STATUS_FAILED, STATUS_FEATURES_OK,
};

/// What a virtio device says it is -- on PCI through its device id, on mmio
/// through the DeviceID register.
pub mod device {
    pub const NET: u32 = 1;
    pub const BLK: u32 = 2;
    pub const CONSOLE: u32 = 3;
    pub const RNG: u32 = 4;
    pub const SCSI: u32 = 8;
}

/// Lay out queue `index` and point the device at it: ask how large the
/// device will have it, build the rings, and hand over their addresses.
///
/// `msix` is the MSI-X entry the device should raise for this queue, for a
/// driver that has registered one; None leaves the queue on the device's
/// interrupt line.
pub fn setup_queue(transport: &dyn Transport, index: u16, msix: Option<u16>) -> Option<Queue> {
    let size = transport.select_queue(index);
    if size == 0 {
        trace!(0, "virtio: queue {} is not there", index);
        return None;
    }

    /* A modern device has already been negotiated down to what a Queue
     * holds; a legacy one cannot be, so a larger queue is refused rather
     * than driven with bookkeeping that does not cover it. */
    let queue = Queue::new(size)?;

    transport.setup_queue(index, &QueueLayout {
        size: queue.size(),
        desc: queue.desc_phys(),
        driver: queue.avail_phys(),
        device: queue.used_phys(),
        msix,
    });

    Some(queue)
}
