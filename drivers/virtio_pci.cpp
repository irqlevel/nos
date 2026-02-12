#include "virtio_pci.h"
#include "mmio.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <mm/page_table.h>

namespace Kernel
{

VirtioPci::VirtioPci()
    : Legacy(false)
    , IoBase(0)
    , CommonCfg(nullptr)
    , NotifyBase(nullptr)
    , NotifyOffMultiplier(0)
    , IsrCfg(nullptr)
    , DeviceCfg(nullptr)
{
    for (ulong i = 0; i < MaxCachedQueues; i++)
        NotifyAddr[i] = nullptr;
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

bool VirtioPci::ProbeModern(Pci::DeviceInfo* dev)
{
    auto& pci = Pci::GetInstance();

    u8 capOffset = pci.FindCapability(dev->Bus, dev->Slot, dev->Func, PciCapIdVndr);
    if (capOffset == 0)
        return false;

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

    Legacy = false;
    Trace(0, "VirtioPci: modern transport probed");
    return true;
}

bool VirtioPci::ProbeLegacy(Pci::DeviceInfo* dev)
{
    auto& pci = Pci::GetInstance();

    /* Legacy virtio uses BAR0 as I/O port region */
    u32 bar0 = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, 0);
    if (!(bar0 & 1))
    {
        Trace(0, "VirtioPci: BAR0 is not I/O port for legacy");
        return false;
    }

    IoBase = (u16)(bar0 & ~0x3U);
    if (IoBase == 0)
    {
        Trace(0, "VirtioPci: BAR0 I/O base is 0");
        return false;
    }

    Legacy = true;
    Trace(0, "VirtioPci: legacy transport probed, iobase 0x%p", (ulong)IoBase);
    return true;
}

bool VirtioPci::Probe(Pci::DeviceInfo* dev)
{
    /* Try modern transport first */
    if (ProbeModern(dev))
        return true;

    /* Fall back to legacy transport */
    Trace(0, "VirtioPci: modern probe failed, trying legacy");
    return ProbeLegacy(dev);
}

void VirtioPci::Reset()
{
    if (Legacy)
    {
        Outb(IoBase + LegDeviceStatus, 0);
        (void)Inb(IoBase + LegDeviceStatus);
    }
    else
    {
        MmioWrite8(CommonCfg + CfgDeviceStatus, 0);
        (void)MmioRead8(CommonCfg + CfgDeviceStatus);
    }
}

u8 VirtioPci::GetStatus()
{
    if (Legacy)
        return Inb(IoBase + LegDeviceStatus);
    return MmioRead8(CommonCfg + CfgDeviceStatus);
}

void VirtioPci::SetStatus(u8 s)
{
    if (Legacy)
        Outb(IoBase + LegDeviceStatus, s);
    else
        MmioWrite8(CommonCfg + CfgDeviceStatus, s);
}

u32 VirtioPci::ReadDeviceFeature(ulong index)
{
    if (Legacy)
    {
        /* Legacy only supports 32-bit feature set (index 0) */
        if (index != 0)
            return 0;
        return In(IoBase + LegDeviceFeatures);
    }
    MmioWrite32(CommonCfg + CfgDeviceFeatureSelect, (u32)index);
    return MmioRead32(CommonCfg + CfgDeviceFeature);
}

void VirtioPci::WriteDriverFeature(ulong index, u32 val)
{
    if (Legacy)
    {
        if (index != 0)
            return;
        Out(IoBase + LegDriverFeatures, val);
        return;
    }
    MmioWrite32(CommonCfg + CfgDriverFeatureSelect, (u32)index);
    MmioWrite32(CommonCfg + CfgDriverFeature, val);
}

u16 VirtioPci::GetNumQueues()
{
    if (Legacy)
    {
        /* Legacy doesn't expose num_queues directly;
           probe by selecting queues until size == 0 */
        for (u16 i = 0; i < 16; i++)
        {
            Outw(IoBase + LegQueueSelect, i);
            if (Inw(IoBase + LegQueueSize) == 0)
                return i;
        }
        return 16;
    }
    return MmioRead16(CommonCfg + CfgNumQueues);
}

u8 VirtioPci::ReadISR()
{
    if (Legacy)
        return Inb(IoBase + LegISRStatus);
    return MmioRead8(IsrCfg);
}

u8 VirtioPci::ReadConfigGeneration()
{
    if (Legacy)
        return 0; /* Legacy has no config generation counter */
    return MmioRead8(CommonCfg + CfgConfigGeneration);
}

void VirtioPci::SelectQueue(u16 idx)
{
    if (Legacy)
        Outw(IoBase + LegQueueSelect, idx);
    else
        MmioWrite16(CommonCfg + CfgQueueSelect, idx);
}

u16 VirtioPci::GetQueueSize()
{
    if (Legacy)
        return Inw(IoBase + LegQueueSize);
    return MmioRead16(CommonCfg + CfgQueueSize);
}

u16 VirtioPci::GetQueueNotifyOff()
{
    if (Legacy)
        return 0; /* Legacy doesn't have per-queue notify offsets */
    return MmioRead16(CommonCfg + CfgQueueNotifyOff);
}

void VirtioPci::SetQueueDesc(u64 physAddr)
{
    if (Legacy)
    {
        /* Legacy uses a single PFN (page frame number) for the queue.
           The PFN covers the entire queue (desc + avail + used).
           Write the PFN = physAddr / 4096 to the queue address register. */
        u32 pfn = (u32)(physAddr / Const::PageSize);
        Out(IoBase + LegQueueAddress, pfn);
        return;
    }
    MmioWrite64(CommonCfg + CfgQueueDesc, physAddr);
}

void VirtioPci::SetQueueDriver(u64 physAddr)
{
    if (Legacy)
        return; /* Legacy uses single PFN set via SetQueueDesc */
    MmioWrite64(CommonCfg + CfgQueueDriver, physAddr);
}

void VirtioPci::SetQueueDevice(u64 physAddr)
{
    if (Legacy)
        return; /* Legacy uses single PFN set via SetQueueDesc */
    MmioWrite64(CommonCfg + CfgQueueDevice, physAddr);
}

void VirtioPci::EnableQueue()
{
    if (Legacy)
        return; /* Legacy queues are enabled by setting the PFN */
    /* Disable MSI-X for this queue -- use legacy INTx */
    MmioWrite16(CommonCfg + CfgQueueMsixVector, 0xFFFF);
    MmioWrite16(CommonCfg + CfgQueueEnable, 1);

    /* Cache the notify address for this queue */
    u16 queueIdx = MmioRead16(CommonCfg + CfgQueueSelect);
    if (queueIdx < MaxCachedQueues)
    {
        u16 notifyOff = MmioRead16(CommonCfg + CfgQueueNotifyOff);
        NotifyAddr[queueIdx] = NotifyBase + (ulong)notifyOff * NotifyOffMultiplier;
    }
}

volatile void* VirtioPci::GetNotifyAddr(u16 queueIdx)
{
    if (Legacy)
        return nullptr; /* Legacy uses I/O port, not MMIO; use NotifyQueue() */
    SelectQueue(queueIdx);
    u16 notifyOff = GetQueueNotifyOff();
    return NotifyBase + (ulong)notifyOff * NotifyOffMultiplier;
}

void VirtioPci::NotifyQueue(u16 queueIdx)
{
    Barrier();
    if (Legacy)
    {
        Outw(IoBase + LegQueueNotify, queueIdx);
    }
    else if (queueIdx < MaxCachedQueues && NotifyAddr[queueIdx])
    {
        MmioWrite16(NotifyAddr[queueIdx], queueIdx);
    }
    else
    {
        /* Fallback: derive address on the fly */
        SelectQueue(queueIdx);
        u16 notifyOff = GetQueueNotifyOff();
        volatile u8* addr = NotifyBase + (ulong)notifyOff * NotifyOffMultiplier;
        MmioWrite16(addr, queueIdx);
    }
}

u8 VirtioPci::ReadDevCfg8(ulong offset)
{
    if (Legacy)
        return Inb(IoBase + LegDeviceConfig + (u16)offset);
    if (!DeviceCfg)
        return 0;
    return MmioRead8(DeviceCfg + offset);
}

u32 VirtioPci::ReadDevCfg32(ulong offset)
{
    if (Legacy)
        return In(IoBase + LegDeviceConfig + (u16)offset);
    if (!DeviceCfg)
        return 0;
    return MmioRead32(DeviceCfg + offset);
}

u64 VirtioPci::ReadDevCfg64(ulong offset)
{
    if (Legacy)
    {
        u32 lo = In(IoBase + LegDeviceConfig + (u16)offset);
        u32 hi = In(IoBase + LegDeviceConfig + (u16)offset + 4);
        return ((u64)hi << 32) | lo;
    }
    if (!DeviceCfg)
        return 0;
    return MmioRead64(DeviceCfg + offset);
}

}
