#include "pci.h"

#include <kernel/trace.h>
#include <hal/pci.h>
#include <kernel/raw_spin_lock.h>

Pci::Pci()
    : device_count_(0)
{
    trace(0, "Pci 0x%p", this);
}

Pci::~Pci()
{
}

const char* Pci::class_to_str(u16 cls)
{
    switch (cls)
    {
    case CLS_UNCLASSIFIED:
        return "Unclassified";
    case CLS_MASS_STORAGE_CONTROLLER:
        return "Storage";
    case CLS_NETWORK_CONTROLLER:
        return "Network";
    case CLS_DISPLAY_CONTROLLER:
        return "Display";
    case CLS_MULTIMEDIA_CONTROLLER:
        return "Multimedia";
    case CLS_MEMORY_CONTROLLER:
        return "Memory";
    case CLS_BRIDGE_DEVICE:
        return "Bridge";
    case CLS_SIMPLE_COMM:
        return "Comm";
    case CLS_BASE_SYSTEM_PERIPHERAL:
        return "System";
    case CLS_INPUT_DEVICE:
        return "Input";
    case CLS_DOCKING_STATION:
        return "Dock";
    case CLS_PROCESSOR:
        return "Processor";
    case CLS_SERIAL_BUS:
        return "SerialBus";
    case CLS_WIRELESS:
        return "Wireless";
    case CLS_INTELLIGENT_IO:
        return "I2O";
    case CLS_SATELLITE_COMM:
        return "Satellite";
    case CLS_ENCRYPTION:
        return "Crypto";
    case CLS_SIGNAL_PROCESSING:
        return "DSP";
    default:
        return "Unknown";
    }
}

const char* Pci::sub_class_to_str(u16 cls, u16 subcls)
{
    switch (cls)
    {
    case CLS_UNCLASSIFIED:
        return "Unknown";
    case CLS_MASS_STORAGE_CONTROLLER:
        switch (subcls)
        {
        case SUB_CLS_SCSI_BUS_CONTROLLER:
            return "SCSI";
        case SUB_CLS_IDE_CONTROLLER:
            return "IDE";
        case SUB_CLS_SATA:
            return "SATA";
        case SUB_CLS_NV_ME:
            return "NVMe";
        default:
            return "Unknown";
        }
    case CLS_NETWORK_CONTROLLER:
        switch (subcls)
        {
        case SUB_CLS_ETHERNET_CONTROLLER:
            return "Ethernet";
        default:
            return "Unknown";
        }
    case CLS_DISPLAY_CONTROLLER:
        switch (subcls)
        {
        case SUB_CLS_VGA_COMPATIBLE_CONTROLLER:
            return "VGA";
        default:
            return "Unknown";
        }
    case CLS_MULTIMEDIA_CONTROLLER:
        return "Unknown";
    case CLS_MEMORY_CONTROLLER:
        switch (subcls)
        {
        case 0x0:
            return "RAM";
        case 0x1:
            return "Flash";
        default:
            return "Unknown";
        }
    case CLS_BRIDGE_DEVICE:
        switch (subcls)
        {
        case SUB_CLS_HOST_BRIDGE:
            return "Host";
        case SUB_CLS_ISA_BRIDGE:
            return "ISA";
        case 0x2:
            return "EISA";
        case 0x3:
            return "MCA";
        case SUB_CLS_PCI_BRIDGE:
            return "PCI";
        case 0x5:
            return "PCMCIA";
        case SUB_CLS_OTHER_BRIDGE:
            return "Other";
        default:
            return "Unknown";
        }
    case CLS_SIMPLE_COMM:
        switch (subcls)
        {
        case 0x0:
            return "Serial";
        case 0x1:
            return "Parallel";
        default:
            return "Unknown";
        }
    case CLS_BASE_SYSTEM_PERIPHERAL:
        switch (subcls)
        {
        case 0x0:
            return "PIC";
        case 0x1:
            return "DMA";
        case 0x2:
            return "Timer";
        case 0x3:
            return "RTC";
        case 0x80:
            return "Other";
        default:
            return "Unknown";
        }
    case CLS_INPUT_DEVICE:
        switch (subcls)
        {
        case 0x0:
            return "Keyboard";
        case 0x1:
            return "Pen";
        case 0x2:
            return "Mouse";
        case 0x3:
            return "Scanner";
        case 0x4:
            return "Gameport";
        case 0x80:
            return "Other";
        default:
            return "Unknown";
        }
    case CLS_SERIAL_BUS:
        switch (subcls)
        {
        case SUB_CLS_FIRE_WIRE:
            return "FireWire";
        case SUB_CLS_USB:
            return "USB";
        case SUB_CLS_SM_BUS:
            return "SMBus";
        case 0x80:
            return "Other";
        default:
            return "Unknown";
        }
    default:
        return "Unknown";
    }
}

const char* Pci::vendor_to_str(u16 vendor)
{
    switch (vendor)
    {
    case VENDOR_INTEL:
        return "Intel";
    case VENDOR_BOCHS:
        return "Bochs";
    case VENDOR_VIRTIO:
        return "Virtio";
    case VENDOR_RED_HAT:
        return "RedHat";
    case VENDOR_AMD:
        return "AMD";
    case VENDOR_REALTEK:
        return "Realtek";
    default:
        return "Unknown";
    }
}

const char* Pci::device_to_str(u16 vendor, u16 dev)
{
    switch (vendor)
    {
    case VENDOR_INTEL:
        switch (dev)
        {
        case 0x29C0:
            return "82G33 MCH";
        case 0x2918:
            return "ICH9 LPC";
        case 0x2930:
            return "ICH9 SMBus";
        default:
            return "Unknown";
        }
    case VENDOR_BOCHS:
        switch (dev)
        {
        case 0x1111:
            return "VGA";
        default:
            return "Unknown";
        }
    case VENDOR_VIRTIO:
        switch (dev)
        {
        case DEV_VIRTIO_BLK:
            return "Blk";
        case DEV_VIRTIO_BALLOON:
            return "Balloon";
        case DEV_VIRTIO_CONSOLE:
            return "Console";
        case DEV_VIRTIO_RNG:
            return "Rng";
        case DEV_VIRTIO_NETWORK:
            return "Network";
        case DEV_VIRTIO_SCSI:
            return "Scsi";
        case DEV_VIRTIO_GPU:
            return "Gpu";
        case DEV_VIRTIO_INPUT:
            return "Input";
        case DEV_VIRTIO_SOCKET:
            return "Socket";
        case DEV_VIRTIO_NET_MODERN:
            return "Network";
        case DEV_VIRTIO_BLK_MODERN:
            return "Blk";
        default:
            return "Unknown";
        }
    case VENDOR_RED_HAT:
        switch (dev)
        {
        case DEV_RED_HAT_PCIE_BRIDGE:
            return "PCIe Bridge";
        case 0x000D:
            return "XHCI USB";
        case 0x0010:
            return "QXL GPU";
        case 0x0100:
            return "QXL VGA";
        default:
            return "Unknown";
        }
    default:
        return "Unknown";
    }
}

namespace {

/* Serializes config access. Config access is BSP-serialized at boot, but
   the surface is exported to Rust drivers that run post-SMP, where two CPUs
   could otherwise interleave a read-modify-write. The RMW helpers (word/byte
   writes) hold the lock across the whole read-modify-write. The raw dword
   accessors are the arch backend (x86: port CAM; arm64: ECAM).

   A function-local static, not a global: its constructor registers it with
   the watchdog, and a global's constructor is one this kernel never runs --
   as a global it went unwatched from the day it was written. */
kernel::RawSpinLock& config_lock()
{
    static kernel::RawSpinLock lock;
    return lock;
}

u32 config_read_dword_raw(u16 bus, u16 slot, u16 func, u16 offset)
{
    return hal::pci_config_read32(bus, slot, func, offset);
}

void config_write_dword_raw(u16 bus, u16 slot, u16 func, u16 offset, u32 value)
{
    hal::pci_config_write32(bus, slot, func, offset, value);
}

}

u16 Pci::read_word(u16 bus, u16 slot, u16 func, u16 offset)
{
    ulong flags = config_lock().lock_irq_save();
    u32 dword = config_read_dword_raw(bus, slot, func, offset);
    config_lock().unlock_irq_restore(flags);
    return (u16)((dword >> ((offset & 2) * 8)) & 0xffff);
}

u32 Pci::read_dword(u16 bus, u16 slot, u16 func, u16 offset)
{
    ulong flags = config_lock().lock_irq_save();
    u32 dword = config_read_dword_raw(bus, slot, func, offset);
    config_lock().unlock_irq_restore(flags);
    return dword;
}

void Pci::write_dword(u16 bus, u16 slot, u16 func, u16 offset, u32 value)
{
    ulong flags = config_lock().lock_irq_save();
    config_write_dword_raw(bus, slot, func, offset, value);
    config_lock().unlock_irq_restore(flags);
}

void Pci::write_word(u16 bus, u16 slot, u16 func, u16 offset, u16 value)
{
    ulong flags = config_lock().lock_irq_save();
    u32 dword = config_read_dword_raw(bus, slot, func, offset);
    u16 shift = (offset & 2) * 8;
    dword &= ~(0xFFFF << shift);
    dword |= ((u32)value << shift);
    config_write_dword_raw(bus, slot, func, offset, dword);
    config_lock().unlock_irq_restore(flags);
}

u8 Pci::read_byte(u16 bus, u16 slot, u16 func, u16 offset)
{
    u32 dword = read_dword(bus, slot, func, offset);
    return (u8)((dword >> ((offset & 3) * 8)) & 0xFF);
}

void Pci::write_byte(u16 bus, u16 slot, u16 func, u16 offset, u8 value)
{
    ulong flags = config_lock().lock_irq_save();
    u32 dword = config_read_dword_raw(bus, slot, func, offset);
    u16 shift = (offset & 3) * 8;
    dword &= ~(0xFF << shift);
    dword |= ((u32)value << shift);
    config_write_dword_raw(bus, slot, func, offset, dword);
    config_lock().unlock_irq_restore(flags);
}

u8 Pci::find_capability(u16 bus, u16 slot, u16 func, u8 cap_id, u8 start_offset)
{
    /* Check status register bit 4 (Capabilities List) */
    u16 status = read_word(bus, slot, func, 0x06);
    if (!(status & (1 << 4)))
        return 0;

    u8 offset;
    if (start_offset != 0)
    {
        /* Resume from a previous capability's next pointer */
        offset = read_byte(bus, slot, func, start_offset + 1) & 0xFC;
    }
    else
    {
        /* Start from the capabilities pointer at offset 0x34 */
        offset = read_byte(bus, slot, func, 0x34) & 0xFC;
    }

    for (int i = 0; i < 48 && offset != 0; i++)
    {
        u8 id = read_byte(bus, slot, func, offset);
        if (id == cap_id)
            return offset;
        offset = read_byte(bus, slot, func, offset + 1) & 0xFC;
    }

    return 0;
}

u32 Pci::get_bar(u16 bus, u16 slot, u16 func, u8 bar)
{
    return read_dword(bus, slot, func, 0x10 + bar * 4);
}

u8 Pci::get_interrupt_line(u16 bus, u16 slot, u16 func)
{
    return read_word(bus, slot, func, 0x3C) & 0xFF;
}

u8 Pci::get_interrupt_pin(u16 bus, u16 slot, u16 func)
{
    return (read_word(bus, slot, func, 0x3C) >> 8) & 0xFF;
}

void Pci::enable_bus_mastering(u16 bus, u16 slot, u16 func)
{
    u16 cmd = read_word(bus, slot, func, 0x04);
    cmd |= (1 << 2); /* Bus Master Enable */
    write_word(bus, slot, func, 0x04, cmd);
}

u16 Pci::get_vendor_id(u16 bus, u16 device, u16 function)
{
    return read_word(bus, device, function, 0);
}

u16 Pci::get_device_id(u16 bus, u16 device, u16 function)
{
    return read_word(bus, device, function, 0x2);
}

u8 Pci::get_class_id(u16 bus, u16 device, u16 function)
{
    u16 r0 = read_word(bus, device, function, 0xA);
    return (r0 & 0xFF00) >> 8;
}

u8 Pci::get_sub_class_id(u16 bus, u16 device, u16 function)
{
    u16 r0 = read_word(bus, device, function, 0xA);
    return (r0 & 0x00FF);
}

u8 Pci::get_prog_if(u16 bus, u16 device, u16 function)
{
    u16 r0 = read_word(bus, device, function, 0x8);
    return (r0 >> 8) & 0xFF;
}

u8 Pci::get_revision_id(u16 bus, u16 device, u16 function)
{
    u16 r0 = read_word(bus, device, function, 0x8);
    return (r0 & 0x00FF);
}

void Pci::scan()
{
    device_count_ = 0;
	for(u32 bus = 0; bus < 256; bus++)
    {
        for(u32 slot = 0; slot < 32; slot++)
        {
            for(u32 function = 0; function < 8; function++)
            {
                u16 vendor = get_vendor_id(bus, slot, function);
                if(vendor == 0xffff)
                    continue;

                u16 device = get_device_id(bus, slot, function);
                u16 cls = get_class_id(bus, slot, function);
                u16 scls = get_sub_class_id(bus, slot, function);
                u16 prog_if = get_prog_if(bus, slot, function);
                u16 rev_id = get_revision_id(bus, slot, function);

                trace(0, "vnd %s(0x%lX) dev %s(0x%lX) %s(0x%lX) %s(0x%lX) p 0x%lX r 0x%lX",
                    vendor_to_str(vendor), (ulong)vendor, device_to_str(vendor, device), (ulong)device, class_to_str(cls), (ulong)cls, sub_class_to_str(cls, scls), (ulong)scls, (ulong)prog_if, (ulong)rev_id);

                if (device_count_ < MAX_DEVICES)
                {
                    auto& d = devices_[device_count_];
                    d.bus = bus;
                    d.slot = slot;
                    d.func = function;
                    d.vendor = vendor;
                    d.device = device;
                    d.base_class = cls;
                    d.sub_class = scls;
                    d.prog_if = prog_if;
                    d.revision_id = rev_id;
                    d.interrupt_line = get_interrupt_line(bus, slot, function);
                    d.interrupt_pin = get_interrupt_pin(bus, slot, function);
                    d.valid = true;
                    device_count_++;
                }
            }
        }
    }

    /* Firmware programs BARs on x86; bare -kernel arm64 must assign them. */
    hal::pci_assign_resources(devices_, device_count_, sizeof(DeviceInfo));
}

Pci::DeviceInfo* Pci::find_device(u16 vendor, u16 device, ulong start_index)
{
    for (ulong i = start_index; i < device_count_; i++)
    {
        if (devices_[i].valid && devices_[i].vendor == vendor && devices_[i].device == device)
            return &devices_[i];
    }
    return nullptr;
}

Pci::DeviceInfo* Pci::get_device(ulong index)
{
    if (index >= device_count_)
        return nullptr;
    return &devices_[index];
}

ulong Pci::get_device_count()
{
    return device_count_;
}

void Pci::dump(stdlib::Printer& printer)
{
    for (ulong i = 0; i < device_count_; i++)
    {
        auto& d = devices_[i];
        if (!d.valid)
            continue;

        printer.printf("vnd %s(0x%lX) dev %s(0x%lX) %s(0x%lX) %s(0x%lX) p 0x%lX r 0x%lX\n",
            vendor_to_str(d.vendor), (ulong)d.vendor, device_to_str(d.vendor, d.device), (ulong)d.device, class_to_str(d.base_class), (ulong)d.base_class, sub_class_to_str(d.base_class, d.sub_class), (ulong)d.sub_class, (ulong)d.prog_if, (ulong)d.revision_id);
    }
}