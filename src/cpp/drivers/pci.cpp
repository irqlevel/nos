#include "pci.h"

#include <kernel/trace.h>
#include <kernel/asm.h>

Pci::Pci()
    : DeviceCount(0)
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
    case ClsSimpleComm:
        return "Comm";
    case ClsBaseSystemPeripheral:
        return "System";
    case ClsInputDevice:
        return "Input";
    case ClsDockingStation:
        return "Dock";
    case ClsProcessor:
        return "Processor";
    case ClsSerialBus:
        return "SerialBus";
    case ClsWireless:
        return "Wireless";
    case ClsIntelligentIO:
        return "I2O";
    case ClsSatelliteComm:
        return "Satellite";
    case ClsEncryption:
        return "Crypto";
    case ClsSignalProcessing:
        return "DSP";
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
        case SubClsSATA:
            return "SATA";
        case SubClsNVMe:
            return "NVMe";
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
        switch (subcls)
        {
        case 0x0:
            return "RAM";
        case 0x1:
            return "Flash";
        default:
            return "Unknown";
        }
    case ClsBridgeDevice:
        switch (subcls)
        {
        case SubClsHostBridge:
            return "Host";
        case SubClsISABridge:
            return "ISA";
        case 0x2:
            return "EISA";
        case 0x3:
            return "MCA";
        case SubClsPCIBridge:
            return "PCI";
        case 0x5:
            return "PCMCIA";
        case SubClsOtherBridge:
            return "Other";
        default:
            return "Unknown";
        }
    case ClsSimpleComm:
        switch (subcls)
        {
        case 0x0:
            return "Serial";
        case 0x1:
            return "Parallel";
        default:
            return "Unknown";
        }
    case ClsBaseSystemPeripheral:
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
    case ClsInputDevice:
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
    case ClsSerialBus:
        switch (subcls)
        {
        case SubClsFireWire:
            return "FireWire";
        case SubClsUSB:
            return "USB";
        case SubClsSMBus:
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
    case VendorRedHat:
        return "RedHat";
    case VendorAMD:
        return "AMD";
    case VendorRealtek:
        return "Realtek";
    default:
        return "Unknown";
    }
}

const char* Pci::DeviceToStr(u16 vendor, u16 dev)
{
    switch (vendor)
    {
    case VendorIntel:
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
    case VendorBochs:
        switch (dev)
        {
        case 0x1111:
            return "VGA";
        default:
            return "Unknown";
        }
    case VendorVirtio:
        switch (dev)
        {
        case DevVirtioBlk:
            return "Blk";
        case DevVirtioBalloon:
            return "Balloon";
        case DevVirtioConsole:
            return "Console";
        case DevVirtioRng:
            return "Rng";
        case DevVirtioNetwork:
            return "Network";
        case DevVirtioScsi:
            return "Scsi";
        case DevVirtioGpu:
            return "Gpu";
        case DevVirtioInput:
            return "Input";
        case DevVirtioSocket:
            return "Socket";
        case DevVirtioNetModern:
            return "Network";
        case DevVirtioBlkModern:
            return "Blk";
        default:
            return "Unknown";
        }
    case VendorRedHat:
        switch (dev)
        {
        case DevRedHatPcieBridge:
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

u32 Pci::ReadDword(u16 bus, u16 slot, u16 func, u16 offset)
{
    u64 address;
    u64 lbus = (u64)bus;
    u64 lslot = (u64)slot;
    u64 lfunc = (u64)func;

    address = (u64)((lbus << 16) | (lslot << 11) | (lfunc << 8) | (offset & 0xfc) | ((u32)0x80000000));
    Out(0xCF8, address);
    return In(0xCFC);
}

void Pci::WriteDword(u16 bus, u16 slot, u16 func, u16 offset, u32 value)
{
    u64 address;
    u64 lbus = (u64)bus;
    u64 lslot = (u64)slot;
    u64 lfunc = (u64)func;

    address = (u64)((lbus << 16) | (lslot << 11) | (lfunc << 8) | (offset & 0xfc) | ((u32)0x80000000));
    Out(0xCF8, address);
    Out(0xCFC, value);
}

void Pci::WriteWord(u16 bus, u16 slot, u16 func, u16 offset, u16 value)
{
    u32 dword = ReadDword(bus, slot, func, offset);
    u16 shift = (offset & 2) * 8;
    dword &= ~(0xFFFF << shift);
    dword |= ((u32)value << shift);
    WriteDword(bus, slot, func, offset, dword);
}

u8 Pci::ReadByte(u16 bus, u16 slot, u16 func, u16 offset)
{
    u32 dword = ReadDword(bus, slot, func, offset);
    return (u8)((dword >> ((offset & 3) * 8)) & 0xFF);
}

void Pci::WriteByte(u16 bus, u16 slot, u16 func, u16 offset, u8 value)
{
    u32 dword = ReadDword(bus, slot, func, offset);
    u16 shift = (offset & 3) * 8;
    dword &= ~(0xFF << shift);
    dword |= ((u32)value << shift);
    WriteDword(bus, slot, func, offset, dword);
}

u8 Pci::FindCapability(u16 bus, u16 slot, u16 func, u8 capId, u8 startOffset)
{
    /* Check status register bit 4 (Capabilities List) */
    u16 status = ReadWord(bus, slot, func, 0x06);
    if (!(status & (1 << 4)))
        return 0;

    u8 offset;
    if (startOffset != 0)
    {
        /* Resume from a previous capability's next pointer */
        offset = ReadByte(bus, slot, func, startOffset + 1) & 0xFC;
    }
    else
    {
        /* Start from the capabilities pointer at offset 0x34 */
        offset = ReadByte(bus, slot, func, 0x34) & 0xFC;
    }

    for (int i = 0; i < 48 && offset != 0; i++)
    {
        u8 id = ReadByte(bus, slot, func, offset);
        if (id == capId)
            return offset;
        offset = ReadByte(bus, slot, func, offset + 1) & 0xFC;
    }

    return 0;
}

u32 Pci::GetBAR(u16 bus, u16 slot, u16 func, u8 bar)
{
    return ReadDword(bus, slot, func, 0x10 + bar * 4);
}

u8 Pci::GetInterruptLine(u16 bus, u16 slot, u16 func)
{
    return ReadWord(bus, slot, func, 0x3C) & 0xFF;
}

u8 Pci::GetInterruptPin(u16 bus, u16 slot, u16 func)
{
    return (ReadWord(bus, slot, func, 0x3C) >> 8) & 0xFF;
}

void Pci::EnableBusMastering(u16 bus, u16 slot, u16 func)
{
    u16 cmd = ReadWord(bus, slot, func, 0x04);
    cmd |= (1 << 2); /* Bus Master Enable */
    WriteWord(bus, slot, func, 0x04, cmd);
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
    return (r0 >> 8) & 0xFF;
}

u8 Pci::GetRevisionID(u16 bus, u16 device, u16 function)
{
    u16 r0 = ReadWord(bus, device, function, 0x8);
    return (r0 & 0x00FF);
}

void Pci::Scan()
{
    DeviceCount = 0;
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

                if (DeviceCount < MaxDevices)
                {
                    auto& d = Devices[DeviceCount];
                    d.Bus = bus;
                    d.Slot = slot;
                    d.Func = function;
                    d.Vendor = vendor;
                    d.Device = device;
                    d.Class = cls;
                    d.SubClass = scls;
                    d.ProgIF = progIf;
                    d.RevisionID = revId;
                    d.InterruptLine = GetInterruptLine(bus, slot, function);
                    d.InterruptPin = GetInterruptPin(bus, slot, function);
                    d.Valid = true;
                    DeviceCount++;
                }
            }
        }
    }
}

Pci::DeviceInfo* Pci::FindDevice(u16 vendor, u16 device, ulong startIndex)
{
    for (ulong i = startIndex; i < DeviceCount; i++)
    {
        if (Devices[i].Valid && Devices[i].Vendor == vendor && Devices[i].Device == device)
            return &Devices[i];
    }
    return nullptr;
}

Pci::DeviceInfo* Pci::GetDevice(ulong index)
{
    if (index >= DeviceCount)
        return nullptr;
    return &Devices[index];
}

ulong Pci::GetDeviceCount()
{
    return DeviceCount;
}

void Pci::Dump(Stdlib::Printer& printer)
{
    for (ulong i = 0; i < DeviceCount; i++)
    {
        auto& d = Devices[i];
        if (!d.Valid)
            continue;

        printer.Printf("vnd %s(0x%p) dev %s(0x%p) %s(0x%p) %s(0x%p) p 0x%p r 0x%p\n",
            VendorToStr(d.Vendor), (ulong)d.Vendor, DeviceToStr(d.Vendor, d.Device), (ulong)d.Device, ClassToStr(d.Class), (ulong)d.Class, SubClassToStr(d.Class, d.SubClass), (ulong)d.SubClass, (ulong)d.ProgIF, (ulong)d.RevisionID);
    }
}