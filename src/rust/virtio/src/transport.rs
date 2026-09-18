//! What a virtio driver needs of the bus under it, once the bus-specific
//! probe has found the device. Two buses implement it: virtio-pci (modern
//! and legacy alike) and virtio-mmio v2.
//!
//! The calls take `&self`: a transport is a window onto device registers,
//! and what changes behind it is the device's state, not the driver's. The
//! queue setup is one call rather than the spec's dance of select-and-write,
//! so the bus decides how a queue is described -- three addresses on a
//! modern device, one page frame number on a legacy one.

/// Device status bits, written in this order during bring-up.
pub const STATUS_ACKNOWLEDGE: u8 = 1;
pub const STATUS_DRIVER: u8 = 2;
pub const STATUS_DRIVER_OK: u8 = 4;
pub const STATUS_FEATURES_OK: u8 = 8;
pub const STATUS_FAILED: u8 = 128;

/// VIRTIO_F_VERSION_1 -- bit 32, which is bit 0 of feature word 1. A modern
/// device offers it and refuses to leave FEATURES_OK set without it.
pub const FEATURE_VERSION_1: u64 = 1 << 32;

/// Where a queue's rings are, and which MSI-X vector serves it.
pub struct QueueLayout {
    pub size: u16,
    pub desc: u64,
    pub driver: u64,
    pub device: u64,
    /// The MSI-X entry the device should raise for this queue, if any
    pub msix: Option<u16>,
}

pub trait Transport {
    fn reset(&self);
    fn status(&self) -> u8;
    fn set_status(&self, status: u8);

    /// One 32-bit word of what the device offers (word 0 is bits 0..31).
    fn device_features(&self, word: u32) -> u32;
    fn set_driver_features(&self, word: u32, value: u32);

    fn num_queues(&self) -> u16;

    /// The interrupt status, read and acknowledged. Not used by a driver
    /// that polls or that takes its interrupts through MSI-X.
    fn read_isr(&self) -> u8;

    /// Make `index` the queue the next setup_queue call is about, and answer
    /// with the largest size the device will give it -- 0 when there is no
    /// such queue. Already clamped to what a Queue here can hold.
    fn select_queue(&self, index: u16) -> u16;

    /// Point the device at a queue's rings and start it.
    fn setup_queue(&self, index: u16, layout: &QueueLayout);

    /// Tell the device a queue has something on its available ring.
    fn notify(&self, index: u16);

    /// The device-specific configuration space, past the transport's own.
    fn config_read8(&self, offset: usize) -> u8;
    fn config_read32(&self, offset: usize) -> u32;
    fn config_read64(&self, offset: usize) -> u64;

    /// The legacy (transitional) virtio-pci layout: 32 bits of features, one
    /// page frame number per queue, and no FEATURES_OK step.
    fn is_legacy(&self) -> bool {
        false
    }

    /// The MSI-X table this bus has, for a driver that wants its completions
    /// as messages rather than on a line. None where there is no MSI-X --
    /// virtio-mmio, and a legacy virtio-pci device.
    fn msix_table(&self) -> Option<&kcore::msix::MsixTable> {
        None
    }

    /// Say that MSI-X entry `entry` serves this device, which is what makes
    /// the queues be told their vectors as they are enabled. A driver calls
    /// it once its handler is registered and before the queue is set up.
    fn use_msix(&self, entry: u16) {
        let _ = entry;
    }
}

/// The bring-up every virtio driver does before it touches a queue: reset,
/// say the driver is here, agree on the features, and -- on a modern device
/// -- check the device stands by the agreement.
///
/// `wanted` is what the driver would like on top of VERSION_1, as a mask of
/// feature bits; what both sides agree on comes back. On failure the device
/// is left FAILED, and the driver has nothing to undo.
pub fn negotiate(transport: &dyn Transport, wanted: u64) -> Option<u64> {
    transport.reset();
    transport.set_status(STATUS_ACKNOWLEDGE);
    transport.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    let offered = if transport.is_legacy() {
        transport.device_features(0) as u64
    } else {
        transport.device_features(0) as u64 | (transport.device_features(1) as u64) << 32
    };

    /* A modern device is only spoken to as a modern device. */
    let mut agreed = offered & wanted;
    if !transport.is_legacy() {
        if offered & FEATURE_VERSION_1 == 0 {
            transport.set_status(STATUS_FAILED);
            return None;
        }
        agreed |= FEATURE_VERSION_1;
    }

    transport.set_driver_features(0, agreed as u32);
    if !transport.is_legacy() {
        transport.set_driver_features(1, (agreed >> 32) as u32);

        transport.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
        if transport.status() & STATUS_FEATURES_OK == 0 {
            transport.set_status(STATUS_FAILED);
            return None;
        }
    }

    Some(agreed)
}

/// The last step of bring-up: the device may start using its queues.
pub fn driver_ok(transport: &dyn Transport) {
    let mut status = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK;
    if !transport.is_legacy() {
        status |= STATUS_FEATURES_OK;
    }
    transport.set_status(status);
}

/// Give up on a device that is half set up.
pub fn failed(transport: &dyn Transport) {
    transport.set_status(STATUS_FAILED);
}
