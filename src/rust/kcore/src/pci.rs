use core::mem::MaybeUninit;
use ffi::pci;

#[derive(Clone, Copy, Debug)]
pub struct PciDevice {
    pub bus: u16,
    pub slot: u16,
    pub func: u16,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub irq_line: u8,
    pub irq_pin: u8,
}

impl PciDevice {
    fn from_ffi(info: &pci::PciDeviceInfo) -> Self {
        Self {
            bus: info.bus,
            slot: info.slot,
            func: info.func,
            vendor: info.vendor,
            device: info.device,
            class: info.cls,
            subclass: info.subclass,
            prog_if: info.prog_if,
            revision: info.revision,
            irq_line: info.irq_line,
            irq_pin: info.irq_pin,
        }
    }

    pub fn read_config8(&self, offset: u16) -> u8 {
        unsafe { pci::kernel_pci_read_config8(self.bus, self.slot, self.func, offset) }
    }

    pub fn read_config16(&self, offset: u16) -> u16 {
        unsafe { pci::kernel_pci_read_config16(self.bus, self.slot, self.func, offset) }
    }

    pub fn read_config32(&self, offset: u16) -> u32 {
        unsafe { pci::kernel_pci_read_config32(self.bus, self.slot, self.func, offset) }
    }

    pub fn write_config8(&self, offset: u16, val: u8) {
        unsafe { pci::kernel_pci_write_config8(self.bus, self.slot, self.func, offset, val) }
    }

    pub fn write_config16(&self, offset: u16, val: u16) {
        unsafe { pci::kernel_pci_write_config16(self.bus, self.slot, self.func, offset, val) }
    }

    pub fn write_config32(&self, offset: u16, val: u32) {
        unsafe { pci::kernel_pci_write_config32(self.bus, self.slot, self.func, offset, val) }
    }

    pub fn get_bar(&self, bar: u8) -> u32 {
        unsafe { pci::kernel_pci_get_bar(self.bus, self.slot, self.func, bar) }
    }

    /* Read a 64-bit memory BAR.  `bar` must be even; `bar+1` holds the high 32 bits. */
    pub fn get_bar64(&self, bar: u8) -> u64 {
        let low = self.get_bar(bar) as u64;
        let high = self.get_bar(bar + 1) as u64;
        (high << 32) | (low & !0xF)
    }

    pub fn enable_bus_mastering(&self) {
        unsafe { pci::kernel_pci_enable_bus_mastering(self.bus, self.slot, self.func) }
    }

    pub fn find_capability(&self, cap_id: u8) -> Option<u8> {
        let off = unsafe {
            pci::kernel_pci_find_capability(self.bus, self.slot, self.func, cap_id, 0)
        };
        if off == 0 { None } else { Some(off) }
    }

    pub fn find_capability_from(&self, cap_id: u8, start_offset: u8) -> Option<u8> {
        let off = unsafe {
            pci::kernel_pci_find_capability(
                self.bus, self.slot, self.func, cap_id, start_offset)
        };
        if off == 0 { None } else { Some(off) }
    }
}

pub fn find_device(vendor: u16, device: u16) -> Option<PciDevice> {
    find_device_from(vendor, device, 0).map(|(_, d)| d)
}

pub fn find_device_from(vendor: u16, device: u16, start: usize)
    -> Option<(usize, PciDevice)>
{
    let mut info = MaybeUninit::<pci::PciDeviceInfo>::uninit();
    let idx = unsafe {
        pci::kernel_pci_find_device(vendor, device, start, info.as_mut_ptr())
    };
    if idx >= 0 {
        Some((idx as usize, PciDevice::from_ffi(unsafe { info.assume_init_ref() })))
    } else {
        None
    }
}

pub fn device_count() -> usize {
    unsafe { pci::kernel_pci_device_count() }
}

pub fn get_device(index: usize) -> Option<PciDevice> {
    let mut info = MaybeUninit::<pci::PciDeviceInfo>::uninit();
    let ok = unsafe { pci::kernel_pci_get_device(index, info.as_mut_ptr()) };
    if ok != 0 {
        Some(PciDevice::from_ffi(unsafe { info.assume_init_ref() }))
    } else {
        None
    }
}

pub mod vendor {
    pub const INTEL: u16 = 0x8086;
    pub const VIRTIO: u16 = 0x1AF4;
    pub const BOCHS: u16 = 0x1234;
    pub const RED_HAT: u16 = 0x1B36;
}

pub mod device {
    pub const VIRTIO_NET: u16 = 0x1000;
    pub const VIRTIO_BLK: u16 = 0x1001;
    pub const VIRTIO_CONSOLE: u16 = 0x1003;
    pub const VIRTIO_SCSI: u16 = 0x1004;
    pub const VIRTIO_RNG: u16 = 0x1005;
    pub const VIRTIO_NET_MODERN: u16 = 0x1041;
    pub const VIRTIO_BLK_MODERN: u16 = 0x1042;
    pub const VIRTIO_RNG_MODERN: u16 = 0x1044;
    pub const VIRTIO_SCSI_MODERN: u16 = 0x1048;
}
