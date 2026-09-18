//! virtio-pci: the bus x86 finds its virtio devices on, in both layouts a
//! device here can speak.
//!
//! **Modern** puts its registers in BARs, pointed at by vendor-specific PCI
//! capabilities: a common configuration block, a notify window, the ISR byte
//! and the device's own configuration. **Legacy** (transitional) puts all of
//! it behind BAR0 as I/O ports, takes one page frame number per queue
//! instead of three ring addresses, and has neither a FEATURES_OK step nor
//! feature bits above 31. QEMU serves both: the smoke boot attaches a modern
//! virtio-blk and a legacy virtio-scsi, so both paths are walked every time.

use core::cell::Cell;

use kcore::barrier::dma_wmb;
use kcore::consts::PAGE_SIZE;
use kcore::dma::PhysMapping;
use kcore::io::{MmioRegion, Port};
use kcore::msix::MsixTable;
use kcore::pci::PciDevice;
use kcore::trace;

use crate::queue::MAX_DESCRIPTORS;
use crate::transport::{QueueLayout, Transport};

/// PCI capability id for a vendor-specific capability: what virtio's are.
const CAP_ID_VENDOR: u8 = 0x09;

/* cfg_type, at byte 3 of a virtio PCI capability */
const CAP_COMMON: u8 = 1;
const CAP_NOTIFY: u8 = 2;
const CAP_ISR: u8 = 3;
const CAP_DEVICE: u8 = 4;
const CAP_PCI: u8 = 5;

/* The common configuration block of a modern device */
const CFG_DEVICE_FEATURE_SELECT: usize = 0x00;
const CFG_DEVICE_FEATURE: usize = 0x04;
const CFG_DRIVER_FEATURE_SELECT: usize = 0x08;
const CFG_DRIVER_FEATURE: usize = 0x0C;
const CFG_MSIX_CONFIG: usize = 0x10;
const CFG_NUM_QUEUES: usize = 0x12;
const CFG_DEVICE_STATUS: usize = 0x14;
const CFG_CONFIG_GENERATION: usize = 0x15;
const CFG_QUEUE_SELECT: usize = 0x16;
const CFG_QUEUE_SIZE: usize = 0x18;
const CFG_QUEUE_MSIX_VECTOR: usize = 0x1A;
const CFG_QUEUE_ENABLE: usize = 0x1C;
const CFG_QUEUE_NOTIFY_OFF: usize = 0x1E;
const CFG_QUEUE_DESC: usize = 0x20;
const CFG_QUEUE_DRIVER: usize = 0x28;
const CFG_QUEUE_DEVICE: usize = 0x30;

/* Legacy I/O ports, from BAR0 */
const LEG_DEVICE_FEATURES: u16 = 0x00;
const LEG_DRIVER_FEATURES: u16 = 0x04;
const LEG_QUEUE_ADDRESS: u16 = 0x08;
const LEG_QUEUE_SIZE: u16 = 0x0C;
const LEG_QUEUE_SELECT: u16 = 0x0E;
const LEG_QUEUE_NOTIFY: u16 = 0x10;
const LEG_DEVICE_STATUS: u16 = 0x12;
const LEG_ISR_STATUS: u16 = 0x13;
const LEG_DEVICE_CONFIG: u16 = 0x14;

/// No MSI-X vector serves this queue (or the configuration change).
const MSIX_NO_VECTOR: u16 = 0xFFFF;

/// Notify addresses are worked out at setup and kept for the queues a device
/// here actually has; past that the address is derived again per notify.
const MAX_CACHED_QUEUES: usize = 4;

const MAX_BARS: usize = 6;
/// 256 bytes of configuration space hold at most this many capabilities, and
/// a device whose list is cyclic must not spin the walk.
const MAX_CAPS: usize = 48;

enum Regs {
    Modern {
        common: MmioRegion,
        /// VA of the notify window, and the multiplier a queue's notify
        /// offset is scaled by
        notify_base: usize,
        notify_multiplier: u32,
        isr: MmioRegion,
        device: Option<MmioRegion>,
    },
    Legacy {
        io: u16,
    },
}

pub struct PciTransport {
    regs: Regs,
    /// The MSI-X table, when the device has one and it could be set up
    msix: Option<MsixTable>,
    /// Whether a vector has been handed to the device for configuration
    /// changes, which is what says MSI-X is in use at all
    msix_active: Cell<bool>,
    notify: [Cell<usize>; MAX_CACHED_QUEUES],
    /// Kept so the windows outlive the transport's use of them. The MSI-X
    /// table took the addresses it needed at probe.
    _mappings: [Option<PhysMapping>; MAX_BARS],
}

impl PciTransport {
    /// Take a virtio device on the PCI bus: modern if it says so, legacy
    /// otherwise. Bus mastering is the caller's to enable.
    pub fn probe(dev: &PciDevice) -> Option<Self> {
        match Self::probe_modern(dev) {
            Some(transport) => Some(transport),
            None => Self::probe_legacy(dev),
        }
    }

    fn probe_modern(dev: &PciDevice) -> Option<Self> {
        let mut bars = [0u64; MAX_BARS];
        let mut mappings: [Option<PhysMapping>; MAX_BARS] = [None, None, None, None, None, None];

        let mut common = None;
        let mut notify: Option<(usize, u32)> = None;
        let mut isr = None;
        let mut device = None;

        let mut cap = dev.find_capability(CAP_ID_VENDOR);
        let mut seen = 0;

        while let Some(offset) = cap {
            seen += 1;
            if seen > MAX_CAPS {
                trace!(0, "virtio-pci: the capability list is cyclic or too long");
                return None;
            }

            let cfg_type = dev.read_config8(offset as u16 + 3);
            let bar = dev.read_config8(offset as u16 + 4);
            let at = dev.read_config32(offset as u16 + 8) as usize;
            let length = dev.read_config32(offset as u16 + 12) as usize;

            if cfg_type != CAP_PCI {
                let base = map_bar(dev, bar, &mut bars, &mut mappings);
                match base {
                    Some(base) => {
                        let region = MmioRegion::new((base + at as u64) as *mut u8, length.max(1));
                        match cfg_type {
                            CAP_COMMON => common = Some(region),
                            CAP_NOTIFY => {
                                let multiplier = dev.read_config32(offset as u16 + 16);
                                notify = Some(((base + at as u64) as usize, multiplier));
                            }
                            CAP_ISR => isr = Some(region),
                            CAP_DEVICE => device = Some(region),
                            _ => {}
                        }
                    }
                    None => {
                        trace!(0, "virtio-pci: no BAR {} for capability type {}", bar, cfg_type);
                        return None;
                    }
                }
            }

            cap = dev.find_capability_from(CAP_ID_VENDOR, offset);
        }

        let common = match common {
            Some(common) => common,
            None => return None,
        };
        let (notify_base, notify_multiplier) = match notify {
            Some(notify) => notify,
            None => {
                trace!(0, "virtio-pci: a common config with no notify window");
                return None;
            }
        };
        /* Without an ISR block there is nowhere to acknowledge a line
         * interrupt; a device that offers neither it nor MSI-X is not one
         * this can drive. */
        let isr = match isr {
            Some(isr) => isr,
            None => {
                trace!(0, "virtio-pci: a common config with no ISR block");
                return None;
            }
        };

        let msix = MsixTable::new_with_bars(dev, &bars);
        if msix.is_none() {
            /* INTx then: tell the device no vector serves config changes. */
            common.write16(CFG_MSIX_CONFIG, MSIX_NO_VECTOR);
        }

        Some(Self {
            regs: Regs::Modern { common, notify_base, notify_multiplier, isr, device },
            msix,
            msix_active: Cell::new(false),
            notify: [const { Cell::new(0) }; MAX_CACHED_QUEUES],
            _mappings: mappings,
        })
    }

    fn probe_legacy(dev: &PciDevice) -> Option<Self> {
        let bar0 = dev.get_bar(0);
        if bar0 & 1 == 0 {
            trace!(0, "virtio-pci: BAR0 is not an I/O port region");
            return None;
        }

        let io = (bar0 & !0x3) as u16;
        if io == 0 {
            return None;
        }

        trace!(0, "virtio-pci: legacy transport at I/O {:#x}", io);
        Some(Self {
            regs: Regs::Legacy { io },
            msix: None,
            msix_active: Cell::new(false),
            notify: [const { Cell::new(0) }; MAX_CACHED_QUEUES],
            _mappings: [None, None, None, None, None, None],
        })
    }

    fn port8(&self, offset: u16) -> Port<u8> {
        match &self.regs {
            Regs::Legacy { io } => Port::new(io + offset),
            _ => Port::new(0),
        }
    }

    fn port16(&self, offset: u16) -> Port<u16> {
        match &self.regs {
            Regs::Legacy { io } => Port::new(io + offset),
            _ => Port::new(0),
        }
    }

    fn port32(&self, offset: u16) -> Port<u32> {
        match &self.regs {
            Regs::Legacy { io } => Port::new(io + offset),
            _ => Port::new(0),
        }
    }

    /// Where a queue's doorbell is, from the offset the device reports.
    fn notify_addr(&self, index: u16) -> usize {
        match &self.regs {
            Regs::Modern { common, notify_base, notify_multiplier, .. } => {
                common.write16(CFG_QUEUE_SELECT, index);
                let offset = common.read16(CFG_QUEUE_NOTIFY_OFF) as usize;
                notify_base + offset * *notify_multiplier as usize
            }
            Regs::Legacy { .. } => 0,
        }
    }
}

/// Map a BAR once, remembering where it went. Returns the kernel VA.
fn map_bar(
    dev: &PciDevice, bar: u8, bars: &mut [u64; MAX_BARS],
    mappings: &mut [Option<PhysMapping>; MAX_BARS],
) -> Option<u64> {
    let index = bar as usize;
    if index >= MAX_BARS {
        return None;
    }
    if bars[index] != 0 {
        return Some(bars[index]);
    }

    let low = dev.get_bar(bar);
    if low & 1 != 0 {
        trace!(0, "virtio-pci: BAR {} is an I/O port region, not memory", bar);
        return None;
    }

    let is64 = low & 0x6 == 0x4 && index + 1 < MAX_BARS;
    let phys = if is64 { dev.get_bar64(bar) & !0xF } else { (low & !0xF) as u64 };
    if phys == 0 {
        return None;
    }

    /* The size is what the BAR reads back as after all-ones is written to
     * it: the bits it refuses to keep are the ones it does not decode. Both
     * halves of a 64-bit BAR are probed -- sizing only the low half gives a
     * nonsense answer for a BAR above 4 GiB. */
    let reg = 0x10 + index as u16 * 4;
    let high = if is64 { dev.read_config32(reg + 4) } else { 0 };

    dev.write_config32(reg, 0xFFFF_FFFF);
    let mask_low = dev.read_config32(reg);
    dev.write_config32(reg, low);

    let mask = if is64 {
        dev.write_config32(reg + 4, 0xFFFF_FFFF);
        let mask_high = dev.read_config32(reg + 4);
        dev.write_config32(reg + 4, high);
        ((mask_high as u64) << 32) | (mask_low & !0xF) as u64
    } else {
        0xFFFF_FFFF_0000_0000 | (mask_low & !0xF) as u64
    };

    let size = (!mask).wrapping_add(1);
    let size = if size == 0 { PAGE_SIZE as u64 } else { size };
    let pages = ((size as usize) + PAGE_SIZE - 1) / PAGE_SIZE;

    let mapping = PhysMapping::map(phys, pages)?;
    let va = mapping.as_mut_ptr() as u64;

    trace!(0, "virtio-pci: BAR {} at {:#x}, {} pages, mapped at {:#x}", bar, phys, pages, va);

    bars[index] = va;
    mappings[index] = Some(mapping);
    Some(va)
}

impl Transport for PciTransport {
    fn reset(&self) {
        match &self.regs {
            Regs::Modern { common, .. } => {
                common.write8(CFG_DEVICE_STATUS, 0);
                /* The reset is done when the status reads back as zero. */
                let _ = common.read8(CFG_DEVICE_STATUS);
            }
            Regs::Legacy { .. } => {
                self.port8(LEG_DEVICE_STATUS).write(0);
                let _ = self.port8(LEG_DEVICE_STATUS).read();
            }
        }
    }

    fn status(&self) -> u8 {
        match &self.regs {
            Regs::Modern { common, .. } => common.read8(CFG_DEVICE_STATUS),
            Regs::Legacy { .. } => self.port8(LEG_DEVICE_STATUS).read(),
        }
    }

    fn set_status(&self, status: u8) {
        match &self.regs {
            Regs::Modern { common, .. } => common.write8(CFG_DEVICE_STATUS, status),
            Regs::Legacy { .. } => self.port8(LEG_DEVICE_STATUS).write(status),
        }
    }

    fn device_features(&self, word: u32) -> u32 {
        match &self.regs {
            Regs::Modern { common, .. } => {
                common.write32(CFG_DEVICE_FEATURE_SELECT, word);
                common.read32(CFG_DEVICE_FEATURE)
            }
            /* Legacy has the low 32 bits and nothing else. */
            Regs::Legacy { .. } if word == 0 => self.port32(LEG_DEVICE_FEATURES).read(),
            Regs::Legacy { .. } => 0,
        }
    }

    fn set_driver_features(&self, word: u32, value: u32) {
        match &self.regs {
            Regs::Modern { common, .. } => {
                common.write32(CFG_DRIVER_FEATURE_SELECT, word);
                common.write32(CFG_DRIVER_FEATURE, value);
            }
            Regs::Legacy { .. } if word == 0 => self.port32(LEG_DRIVER_FEATURES).write(value),
            Regs::Legacy { .. } => {}
        }
    }

    fn num_queues(&self) -> u16 {
        match &self.regs {
            Regs::Modern { common, .. } => common.read16(CFG_NUM_QUEUES),
            Regs::Legacy { .. } => {
                /* Legacy does not say; ask each queue until one has no size */
                for index in 0..16u16 {
                    self.port16(LEG_QUEUE_SELECT).write(index);
                    if self.port16(LEG_QUEUE_SIZE).read() == 0 {
                        return index;
                    }
                }
                16
            }
        }
    }

    fn read_isr(&self) -> u8 {
        match &self.regs {
            Regs::Modern { isr, .. } => isr.read8(0),
            Regs::Legacy { .. } => self.port8(LEG_ISR_STATUS).read(),
        }
    }

    fn select_queue(&self, index: u16) -> u16 {
        let size = match &self.regs {
            Regs::Modern { common, .. } => {
                common.write16(CFG_QUEUE_SELECT, index);
                common.read16(CFG_QUEUE_SIZE)
            }
            Regs::Legacy { .. } => {
                self.port16(LEG_QUEUE_SELECT).write(index);
                self.port16(LEG_QUEUE_SIZE).read()
            }
        };

        /* A modern device takes the smaller size written back at setup; a
         * legacy one cannot be negotiated down, so a queue larger than this
         * driver's bookkeeping is refused by the caller. */
        if self.is_legacy() {
            size
        } else {
            core::cmp::min(size, MAX_DESCRIPTORS)
        }
    }

    fn setup_queue(&self, index: u16, layout: &QueueLayout) {
        match &self.regs {
            Regs::Modern { common, .. } => {
                common.write16(CFG_QUEUE_SELECT, index);
                common.write16(CFG_QUEUE_SIZE, layout.size);
                common.write64(CFG_QUEUE_DESC, layout.desc);
                common.write64(CFG_QUEUE_DRIVER, layout.driver);
                common.write64(CFG_QUEUE_DEVICE, layout.device);

                match layout.msix {
                    Some(entry) if self.msix_active.get() => {
                        common.write16(CFG_QUEUE_MSIX_VECTOR, entry)
                    }
                    _ => common.write16(CFG_QUEUE_MSIX_VECTOR, MSIX_NO_VECTOR),
                }

                /* The rings are in place before the queue is enabled. */
                dma_wmb();
                common.write16(CFG_QUEUE_ENABLE, 1);

                if (index as usize) < MAX_CACHED_QUEUES {
                    self.notify[index as usize].set(self.notify_addr(index));
                }
            }
            Regs::Legacy { .. } => {
                /* One page frame number covers the whole queue -- descriptor
                 * table, available and used rings -- and writing it is what
                 * starts the queue. */
                self.port16(LEG_QUEUE_SELECT).write(index);
                dma_wmb();
                self.port32(LEG_QUEUE_ADDRESS).write((layout.desc / PAGE_SIZE as u64) as u32);
            }
        }
    }

    fn notify(&self, index: u16) {
        /* Everything on the ring must be visible before the doorbell. */
        dma_wmb();

        match &self.regs {
            Regs::Legacy { .. } => self.port16(LEG_QUEUE_NOTIFY).write(index),
            Regs::Modern { .. } => {
                let cached = if (index as usize) < MAX_CACHED_QUEUES {
                    self.notify[index as usize].get()
                } else {
                    0
                };
                let addr = if cached != 0 { cached } else { self.notify_addr(index) };
                if addr != 0 {
                    unsafe { (addr as *mut u16).write_volatile(index) };
                }
            }
        }
    }

    fn config_read8(&self, offset: usize) -> u8 {
        match &self.regs {
            Regs::Modern { device: Some(device), .. } => device.read8(offset),
            Regs::Modern { device: None, .. } => 0,
            Regs::Legacy { .. } => self.port8(LEG_DEVICE_CONFIG + offset as u16).read(),
        }
    }

    fn config_read32(&self, offset: usize) -> u32 {
        match &self.regs {
            Regs::Modern { device: Some(device), .. } => device.read32(offset),
            Regs::Modern { device: None, .. } => 0,
            Regs::Legacy { .. } => self.port32(LEG_DEVICE_CONFIG + offset as u16).read(),
        }
    }

    fn config_read64(&self, offset: usize) -> u64 {
        match &self.regs {
            Regs::Modern { device: Some(device), .. } => device.read64(offset),
            Regs::Modern { device: None, .. } => 0,
            Regs::Legacy { .. } => {
                let low = self.config_read32(offset) as u64;
                let high = self.config_read32(offset + 4) as u64;
                (high << 32) | low
            }
        }
    }

    fn is_legacy(&self) -> bool {
        matches!(self.regs, Regs::Legacy { .. })
    }

    fn msix_table(&self) -> Option<&MsixTable> {
        self.msix.as_ref()
    }

    fn use_msix(&self, entry: u16) {
        if let Regs::Modern { common, .. } = &self.regs {
            /* The vector for configuration changes, and from here on the
             * queues are told theirs as they are enabled. */
            common.write16(CFG_MSIX_CONFIG, entry);
            self.msix_active.set(true);
        }
    }
}

/// The configuration generation counter, for a driver reading a multi-word
/// configuration that the device may change underneath it. Legacy has none
/// and answers 0.
pub fn config_generation(transport: &PciTransport) -> u8 {
    match &transport.regs {
        Regs::Modern { common, .. } => common.read8(CFG_CONFIG_GENERATION),
        Regs::Legacy { .. } => 0,
    }
}
