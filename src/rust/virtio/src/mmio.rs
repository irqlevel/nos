//! virtio-mmio, version 2: the bus arm64 finds its devices on, described by
//! the device tree rather than enumerated.
//!
//! Version 1 (legacy mmio) is deliberately not supported -- the run scripts
//! pass `-global virtio-mmio.force-legacy=false`, and a v1 device is refused
//! with a line saying so rather than half driven.

use kcore::barrier::dma_wmb;
use kcore::io::MmioRegion;
use kcore::trace;

use crate::queue::MAX_DESCRIPTORS;
use crate::transport::{QueueLayout, Transport};

const MAGIC: u32 = 0x7472_6976; /* "virt" */
const VERSION: u32 = 2;

const REG_MAGIC: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;
const REG_DEVICE_FEATURES: usize = 0x010;
const REG_DEVICE_FEATURES_SEL: usize = 0x014;
const REG_DRIVER_FEATURES: usize = 0x020;
const REG_DRIVER_FEATURES_SEL: usize = 0x024;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_READY: usize = 0x044;
const REG_QUEUE_NOTIFY: usize = 0x050;
const REG_INTERRUPT_STATUS: usize = 0x060;
const REG_INTERRUPT_ACK: usize = 0x064;
const REG_STATUS: usize = 0x070;
const REG_QUEUE_DESC_LOW: usize = 0x080;
const REG_QUEUE_DESC_HIGH: usize = 0x084;
const REG_QUEUE_DRIVER_LOW: usize = 0x090;
const REG_QUEUE_DRIVER_HIGH: usize = 0x094;
const REG_QUEUE_DEVICE_LOW: usize = 0x0A0;
const REG_QUEUE_DEVICE_HIGH: usize = 0x0A4;
const REG_CONFIG: usize = 0x100;

/// A virtio-mmio device the device tree pointed at. The C++ boot code hands
/// these over (`VirtioMmioSlot` in drivers/virtio_mmio.h), already mapped:
/// the window is inside the device GiB the arm64 boot premaps.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Slot {
    /// Kernel virtual address of the register window
    pub base: usize,
    pub size: usize,
    pub int_id: u32,
    /// 1 net, 2 blk, 4 rng, 8 scsi
    pub device_id: u32,
}

pub struct MmioTransport {
    regs: MmioRegion,
}

impl MmioTransport {
    /// Take the window if it holds a version 2 virtio device.
    pub fn probe(slot: &Slot) -> Option<Self> {
        let regs = MmioRegion::new(slot.base as *mut u8, slot.size);

        if regs.read32(REG_MAGIC) != MAGIC {
            return None;
        }

        let version = regs.read32(REG_VERSION);
        if version != VERSION {
            trace!(0, "virtio-mmio {:#x}: version {} is not supported (needs \
                -global virtio-mmio.force-legacy=false)", slot.base, version);
            return None;
        }

        if regs.read32(REG_DEVICE_ID) == 0 {
            return None;
        }

        Some(Self { regs })
    }

    /// What kind of device a window holds, without taking it: 0 for none.
    pub fn device_id(slot: &Slot) -> u32 {
        let regs = MmioRegion::new(slot.base as *mut u8, slot.size);
        if regs.read32(REG_MAGIC) != MAGIC || regs.read32(REG_VERSION) != VERSION {
            return 0;
        }
        regs.read32(REG_DEVICE_ID)
    }
}

impl Transport for MmioTransport {
    fn reset(&self) {
        self.regs.write32(REG_STATUS, 0);
    }

    fn status(&self) -> u8 {
        self.regs.read32(REG_STATUS) as u8
    }

    fn set_status(&self, status: u8) {
        self.regs.write32(REG_STATUS, status as u32);
    }

    fn device_features(&self, word: u32) -> u32 {
        self.regs.write32(REG_DEVICE_FEATURES_SEL, word);
        self.regs.read32(REG_DEVICE_FEATURES)
    }

    fn set_driver_features(&self, word: u32, value: u32) {
        self.regs.write32(REG_DRIVER_FEATURES_SEL, word);
        self.regs.write32(REG_DRIVER_FEATURES, value);
    }

    fn num_queues(&self) -> u16 {
        /* No register says how many: ask each in turn until one has no size */
        let mut count = 0;
        for index in 0..8 {
            self.regs.write32(REG_QUEUE_SEL, index);
            if self.regs.read32(REG_QUEUE_NUM_MAX) == 0 {
                break;
            }
            count += 1;
        }
        count
    }

    fn read_isr(&self) -> u8 {
        let status = self.regs.read32(REG_INTERRUPT_STATUS);
        if status != 0 {
            self.regs.write32(REG_INTERRUPT_ACK, status);
        }
        status as u8
    }

    fn select_queue(&self, index: u16) -> u16 {
        self.regs.write32(REG_QUEUE_SEL, index as u32);

        /* mmio reports up to 1024; a queue here holds fewer, and the size
         * written back at setup is what the device then uses. */
        let max = self.regs.read32(REG_QUEUE_NUM_MAX);
        core::cmp::min(max, MAX_DESCRIPTORS as u32) as u16
    }

    fn setup_queue(&self, index: u16, layout: &QueueLayout) {
        self.regs.write32(REG_QUEUE_SEL, index as u32);
        self.regs.write32(REG_QUEUE_NUM, layout.size as u32);

        self.regs.write32(REG_QUEUE_DESC_LOW, layout.desc as u32);
        self.regs.write32(REG_QUEUE_DESC_HIGH, (layout.desc >> 32) as u32);
        self.regs.write32(REG_QUEUE_DRIVER_LOW, layout.driver as u32);
        self.regs.write32(REG_QUEUE_DRIVER_HIGH, (layout.driver >> 32) as u32);
        self.regs.write32(REG_QUEUE_DEVICE_LOW, layout.device as u32);
        self.regs.write32(REG_QUEUE_DEVICE_HIGH, (layout.device >> 32) as u32);

        /* The rings have to be in place before the device is told the queue
         * is ready to be used. */
        dma_wmb();
        self.regs.write32(REG_QUEUE_READY, 1);
    }

    fn notify(&self, index: u16) {
        /* Everything the driver put on the ring must be visible first. */
        dma_wmb();
        self.regs.write32(REG_QUEUE_NOTIFY, index as u32);
    }

    fn config_read8(&self, offset: usize) -> u8 {
        self.regs.read8(REG_CONFIG + offset)
    }

    fn config_read32(&self, offset: usize) -> u32 {
        self.regs.read32(REG_CONFIG + offset)
    }

    fn config_read64(&self, offset: usize) -> u64 {
        let low = self.config_read32(offset) as u64;
        let high = self.config_read32(offset + 4) as u64;
        (high << 32) | low
    }
}
