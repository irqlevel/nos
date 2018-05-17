#include "pci.h"

#include <kernel/trace.h>
#include <kernel/asm.h>

Pci::Pci()
{
    Trace(0, "Pci 0x%p", this);
}

Pci::~Pci()
{
}

const char* Pci::ClassToStr(u16 cls)
{
    switch (cls)
    {
    case ClsUnclassified:
        return "Unclassified";
    case ClsMassStorageController:
        return "Storage";
    case ClsNetworkController:
        return "Network";
    case ClsDisplayController:
        return "Display";
    case ClsMultimediaController:
        return "Multimedia";
    case ClsMemoryController:
        return "Memory";
    case ClsBridgeDevice:
        return "Bridge";
    default:
        return "Unknown";
    }
}

const char* Pci::SubClassToStr(u16 cls, u16 subcls)
{
    switch (cls)
    {
    case ClsUnclassified:
        return "Unknown";
    case ClsMassStorageController:
        switch (subcls)
        {
        case SubClsSCSIBusController:
            return "SCSI";
        case SubClsIDEController:
            return "IDE";
        default:
            return "Unknown";
        }
    case ClsNetworkController:
        switch (subcls)
        {
        case SubClsEthernetController:
            return "Ethernet";
        default:
            return "Unknown";
        }
    case ClsDisplayController:
        switch (subcls)
        {
        case SubClsVgaCompatibleController:
            return "VGA";
        default:
            return "Unknown";
        }
    case ClsMultimediaController:
        return "Unknown";
    case ClsMemoryController:
        return "Unknown";
    case ClsBridgeDevice:
        switch (subcls)
        {
        case SubClsHostBridge:
            return "Host";
        case SubClsISABridge:
            return "ISA";
        case SubClsOtherBridge:
            return "Other";
        default:
            return "Unknown";
        }
    default:
        return "Unknown";
    }
}

const char* Pci::VendorToStr(u16 vendor)
{
    switch (vendor)
    {
    case VendorIntel:
        return "Intel";
    case VendorBochs:
        return "Bochs";
    case VendorVirtio:
        return "Virtio";
    default:
        return "Unknown";
    }
}

const char* Pci::DeviceToStr(u16 vendor, u16 dev)
{
    switch (vendor)
    {
    case VendorIntel:
        return "Unknown";
    case VendorBochs:
        return "Unknown";
    case VendorVirtio:
        switch (dev)
        {
        case DevVirtioBlk:
            return "Blk";
        case DevVirtioRng:
            return "Rng";
        case DevVirtioNetwork:
            return "Network";
        case DevVirtioScsi:
            return "Scsi";
        default:
            return "Unknown";
        }
    default:
        return "Unknown";
    }
}

u16 Pci::ReadWord(u16 bus, u16 slot, u16 func, u16 offset)
{
    u64 address;
    u64 lbus = (u64)bus;
    u64 lslot = (u64)slot;
    u64 lfunc = (u64)func;
    u16 tmp = 0;

    address = (u64)((lbus << 16) | (lslot << 11) | (lfunc << 8) | (offset & 0xfc) | ((u32)0x80000000));
    Out(0xCF8, address);
    tmp = (u16)((In(0xCFC) >> ((offset & 2) * 8)) & 0xffff);
    return (tmp);
}

u16 Pci::GetVendorID(u16 bus, u16 device, u16 function)
{
    return ReadWord(bus, device, function, 0);
}

u16 Pci::GetDeviceID(u16 bus, u16 device, u16 function)
{
    return ReadWord(bus, device, function, 0x2);
}

u8 Pci::GetClassId(u16 bus, u16 device, u16 function)
{
    u16 r0 = ReadWord(bus, device, function, 0xA);
    return (r0 & 0xFF00) >> 8;
}

u8 Pci::GetSubClassId(u16 bus, u16 device, u16 function)
{
    u16 r0 = ReadWord(bus, device, function, 0xA);
    return (r0 & 0x00FF);
}

u8 Pci::GetProgIF(u16 bus, u16 device, u16 function)
{
    u16 r0 = ReadWord(bus, device, function, 0x8);
    return (r0 & 0xFF00);
}

u8 Pci::GetRevisionID(u16 bus, u16 device, u16 function)
{
    u16 r0 = ReadWord(bus, device, function, 0x8);
    return (r0 & 0x00FF);
}

void Pci::Scan()
{
	for(u32 bus = 0; bus < 256; bus++)
    {
        for(u32 slot = 0; slot < 32; slot++)
        {
            for(u32 function = 0; function < 8; function++)
            {
                u16 vendor = GetVendorID(bus, slot, function);
                if(vendor == 0xffff)
                    continue;

                u16 device = GetDeviceID(bus, slot, function);
                u16 cls = GetClassId(bus, slot, function);
                u16 scls = GetSubClassId(bus, slot, function);
                u16 progIf = GetProgIF(bus, slot, function);
                u16 revId = GetRevisionID(bus, slot, function);

                Trace(0, "vnd %s(0x%p) dev %s(0x%p) %s(0x%p) %s(0x%p) p 0x%p r 0x%p",
                    VendorToStr(vendor), (ulong)vendor, DeviceToStr(vendor, device), (ulong)device, ClassToStr(cls), (ulong)cls, SubClassToStr(cls, scls), (ulong)scls, (ulong)progIf, (ulong)revId);
            }
        }
    }
}

void Pci::Dump(Stdlib::Printer& printer)
{
	for(u32 bus = 0; bus < 256; bus++)
    {
        for(u32 slot = 0; slot < 32; slot++)
        {
            for(u32 function = 0; function < 8; function++)
            {
                u16 vendor = GetVendorID(bus, slot, function);
                if(vendor == 0xffff)
                    continue;

                u16 device = GetDeviceID(bus, slot, function);
                u16 cls = GetClassId(bus, slot, function);
                u16 scls = GetSubClassId(bus, slot, function);
                u16 progIf = GetProgIF(bus, slot, function);
                u16 revId = GetRevisionID(bus, slot, function);

                printer.Printf("vnd %s(0x%p) dev %s(0x%p) %s(0x%p) %s(0x%p) p 0x%p r 0x%p\n",
                    VendorToStr(vendor), (ulong)vendor, DeviceToStr(vendor, device), (ulong)device, ClassToStr(cls), (ulong)cls, SubClassToStr(cls, scls), (ulong)scls, (ulong)progIf, (ulong)revId);
            }
        }
    }
}