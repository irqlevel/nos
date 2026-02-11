#include "virtio_pci.h"
#include "mmio.h"

#include <kernel/trace.h>
#include <mm/page_table.h>

namespace Kernel
{

VirtioPci::VirtioPci()
    : CommonCfg(nullptr)
    , NotifyBase(nullptr)
    , NotifyOffMultiplier(0)
    , IsrCfg(nullptr)
    , DeviceCfg(nullptr)
{
    for (ulong i = 0; i < MaxBars; i++)
        MappedBars[i] = 0;
}

VirtioPci::~VirtioPci()
{
}

ulong VirtioPci::MapBar(Pci::DeviceInfo* dev, u8 bar)
{
    if (bar >= MaxBars)
        return 0;

    if (MappedBars[bar] != 0)
        return MappedBars[bar];

    auto& pci = Pci::GetInstance();
    u32 barVal = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, bar);

    /* Must be MMIO BAR (bit 0 = 0) */
    if (barVal & 1)
    {
        Trace(0, "VirtioPci: BAR%u is I/O port, not MMIO", (ulong)bar);
        return 0;
    }

    ulong physAddr = barVal & ~0xFUL;

    /* Check if 64-bit BAR */
    if ((barVal & 0x6) == 0x4 && bar + 1 < MaxBars)
    {
        u32 barHigh = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, bar + 1);
        physAddr |= ((ulong)barHigh << 32);
    }

    if (physAddr == 0)
        return 0;

    /* Determine BAR size by writing all-ones and reading back */
    pci.WriteDword(dev->Bus, dev->Slot, dev->Func, 0x10 + bar * 4, 0xFFFFFFFF);
    u32 sizeMask = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, bar);
    pci.WriteDword(dev->Bus, dev->Slot, dev->Func, 0x10 + bar * 4, barVal);

    u32 sizeMask32 = sizeMask & ~0xFU;
    ulong barSize = (ulong)(~sizeMask32) + 1;
    if (barSize == 0)
        barSize = Const::PageSize;

    Trace(0, "VirtioPci: BAR%u phys 0x%p size 0x%p", (ulong)bar, physAddr, barSize);

    auto& pt = Mm::PageTable::GetInstance();
    ulong va = pt.MapMmioRegion(physAddr, barSize);
    if (va == 0)
    {
        Trace(0, "VirtioPci: failed to map BAR%u", (ulong)bar);
        return 0;
    }

    MappedBars[bar] = va;
    return va;
}

bool VirtioPci::Probe(Pci::DeviceInfo* dev)
{
    auto& pci = Pci::GetInstance();

    u8 capOffset = pci.FindCapability(dev->Bus, dev->Slot, dev->Func, PciCapIdVndr);
    if (capOffset == 0)
    {
        Trace(0, "VirtioPci: no vendor capabilities found");
        return false;
    }

    while (capOffset != 0)
    {
        u8 cfgType = pci.ReadByte(dev->Bus, dev->Slot, dev->Func, capOffset + 3);
        u8 bar     = pci.ReadByte(dev->Bus, dev->Slot, dev->Func, capOffset + 4);
        u32 offset = pci.ReadDword(dev->Bus, dev->Slot, dev->Func, capOffset + 8);
        u32 length = pci.ReadDword(dev->Bus, dev->Slot, dev->Func, capOffset + 12);

        Trace(0, "VirtioPci: cap type %u bar %u offset 0x%p len 0x%p",
            (ulong)cfgType, (ulong)bar, (ulong)offset, (ulong)length);

        ulong barVirt = MapBar(dev, bar);
        if (barVirt == 0 && cfgType != CapPciCfg)
        {
            Trace(0, "VirtioPci: failed to map BAR for cap type %u", (ulong)cfgType);
            return false;
        }

        switch (cfgType)
        {
        case CapCommonCfg:
            CommonCfg = (volatile u8*)(barVirt + offset);
            break;
        case CapNotifyCfg:
            NotifyBase = (volatile u8*)(barVirt + offset);
            NotifyOffMultiplier = pci.ReadDword(dev->Bus, dev->Slot, dev->Func, capOffset + 16);
            Trace(0, "VirtioPci: notify_off_multiplier %u", NotifyOffMultiplier);
            break;
        case CapIsrCfg:
            IsrCfg = (volatile u8*)(barVirt + offset);
            break;
        case CapDeviceCfg:
            DeviceCfg = (volatile u8*)(barVirt + offset);
            break;
        case CapPciCfg:
            /* PCI cfg access capability -- not needed */
            break;
        default:
            break;
        }

        capOffset = pci.FindCapability(dev->Bus, dev->Slot, dev->Func, PciCapIdVndr, capOffset);
    }

    if (!CommonCfg)
    {
        Trace(0, "VirtioPci: common config capability not found");
        return false;
    }

    if (!NotifyBase)
    {
        Trace(0, "VirtioPci: notify capability not found");
        return false;
    }

    /* Disable MSI-X -- use legacy INTx */
    MmioWrite16(CommonCfg + CfgMsixConfig, 0xFFFF);

    return true;
}

void VirtioPci::Reset()
{
    MmioWrite8(CommonCfg + CfgDeviceStatus, 0);
    /* Read back to ensure the write has completed */
    (void)MmioRead8(CommonCfg + CfgDeviceStatus);
}

u8 VirtioPci::GetStatus()
{
    return MmioRead8(CommonCfg + CfgDeviceStatus);
}

void VirtioPci::SetStatus(u8 s)
{
    MmioWrite8(CommonCfg + CfgDeviceStatus, s);
}

u32 VirtioPci::ReadDeviceFeature(ulong index)
{
    MmioWrite32(CommonCfg + CfgDeviceFeatureSelect, (u32)index);
    return MmioRead32(CommonCfg + CfgDeviceFeature);
}

void VirtioPci::WriteDriverFeature(ulong index, u32 val)
{
    MmioWrite32(CommonCfg + CfgDriverFeatureSelect, (u32)index);
    MmioWrite32(CommonCfg + CfgDriverFeature, val);
}

u16 VirtioPci::GetNumQueues()
{
    return MmioRead16(CommonCfg + CfgNumQueues);
}

u8 VirtioPci::ReadISR()
{
    return MmioRead8(IsrCfg);
}

u8 VirtioPci::ReadConfigGeneration()
{
    return MmioRead8(CommonCfg + CfgConfigGeneration);
}

void VirtioPci::SelectQueue(u16 idx)
{
    MmioWrite16(CommonCfg + CfgQueueSelect, idx);
}

u16 VirtioPci::GetQueueSize()
{
    return MmioRead16(CommonCfg + CfgQueueSize);
}

u16 VirtioPci::GetQueueNotifyOff()
{
    return MmioRead16(CommonCfg + CfgQueueNotifyOff);
}

void VirtioPci::SetQueueDesc(u64 physAddr)
{
    MmioWrite64(CommonCfg + CfgQueueDesc, physAddr);
}

void VirtioPci::SetQueueDriver(u64 physAddr)
{
    MmioWrite64(CommonCfg + CfgQueueDriver, physAddr);
}

void VirtioPci::SetQueueDevice(u64 physAddr)
{
    MmioWrite64(CommonCfg + CfgQueueDevice, physAddr);
}

void VirtioPci::EnableQueue()
{
    /* Disable MSI-X for this queue -- use legacy INTx */
    MmioWrite16(CommonCfg + CfgQueueMsixVector, 0xFFFF);
    MmioWrite16(CommonCfg + CfgQueueEnable, 1);
}

volatile void* VirtioPci::GetNotifyAddr(u16 queueIdx)
{
    SelectQueue(queueIdx);
    u16 notifyOff = GetQueueNotifyOff();
    return NotifyBase + (ulong)notifyOff * NotifyOffMultiplier;
}

u8 VirtioPci::ReadDevCfg8(ulong offset)
{
    if (!DeviceCfg)
        return 0;
    return MmioRead8(DeviceCfg + offset);
}

u32 VirtioPci::ReadDevCfg32(ulong offset)
{
    if (!DeviceCfg)
        return 0;
    return MmioRead32(DeviceCfg + offset);
}

u64 VirtioPci::ReadDevCfg64(ulong offset)
{
    if (!DeviceCfg)
        return 0;
    return MmioRead64(DeviceCfg + offset);
}

}
