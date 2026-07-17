#include <drivers/msix.h>

#include "its.h"

#include <drivers/mmio.h>
#include <kernel/trace.h>
#include <lib/stdlib.h>
#include <mm/new.h>
#include <mm/page_table.h>
#include <include/const.h>

/* arm64 MsixTable: same class ABI as drivers/msix.cpp (x86), but MSI is
   routed through the GICv3 ITS instead of a LAPIC-addressed message. The
   MSI-X table setup / BAR mapping is identical; only the message
   composition (address = GITS_TRANSLATER, data = ITS eventId) and the
   interrupt install (an LPI handler, not an IDT vector) differ. This is a
   per-arch TU of the shared class, like CpuTable::StartAll. */

namespace Kernel
{

static const u8 PciCapIdMsix = 0x11;
static const u16 PciCommandIntxDisable = (u16)(1 << 10);
static const u16 MsixControlEnable = (u16)(1 << 15);
static const u16 MsixControlFuncMask = (u16)(1 << 14);

Atomic MsixTable::NextVectorOffset;

MsixTable::MsixTable()
    : Table(nullptr)
    , Count(0)
    , CapOffset(0)
    , Dev(nullptr)
    , PciMsixEnabled(false)
    , EntryVector(nullptr)
{
}

MsixTable::~MsixTable()
{
    if (EntryVector)
    {
        Mm::Free(EntryVector);
        EntryVector = nullptr;
    }
    Table = nullptr;
    Count = 0;
    Dev = nullptr;
}

bool MsixTable::MapBarForTable(Pci::DeviceInfo* dev, u8 bar, ulong offsetInBar,
    ulong tableBytes)
{
    if (bar >= 6)
        return false;

    auto& pci = Pci::GetInstance();
    u32 barVal = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, bar);
    if (barVal & 1)
        return false;

    ulong physAddr = barVal & ~0xFUL;
    bool is64 = ((barVal & 0x6) == 0x4) && (bar + 1 < 6);
    if (is64)
        physAddr |= ((ulong)pci.GetBAR(dev->Bus, dev->Slot, dev->Func, (u8)(bar + 1)) << 32);
    if (physAddr == 0)
        return false;

    ulong mapBytes = offsetInBar + tableBytes;
    mapBytes = (mapBytes + Const::PageSize - 1) & ~(Const::PageSize - 1);

    ulong va = Mm::PageTable::GetInstance().MapMmioRegion(physAddr, mapBytes);
    if (va == 0)
        return false;

    Table = (volatile u8*)(va + offsetInBar);
    return true;
}

bool MsixTable::Setup(Pci::DeviceInfo* dev, const ulong* mappedBars)
{
    if (!dev || !dev->Valid)
        return false;

    Dev = dev;

    auto& pci = Pci::GetInstance();
    u8 cap = pci.FindCapability(dev->Bus, dev->Slot, dev->Func, PciCapIdMsix);
    if (cap == 0)
        return false;

    CapOffset = cap;

    u32 capDw0 = pci.ReadDword(dev->Bus, dev->Slot, dev->Func, cap);
    u16 msgCtrl = (u16)((capDw0 >> 16) & 0xFFFF);
    Count = (u16)((msgCtrl & 0x7FFu) + 1);
    if (Count == 0)
        return false;

    u32 tblInfo = pci.ReadDword(dev->Bus, dev->Slot, dev->Func, (u16)(cap + 4));
    u8 bir = (u8)(tblInfo & 7);
    ulong offInBar = tblInfo & ~7UL;
    ulong tableBytes = (ulong)Count * 16;

    EntryVector = (u8*)Mm::Alloc((size_t)Count, 0);
    if (!EntryVector)
        return false;
    Stdlib::MemSet(EntryVector, 0, (size_t)Count);

    bool mapped = false;
    if (mappedBars && bir < 6 && mappedBars[bir] != 0)
    {
        Table = (volatile u8*)(mappedBars[bir] + offInBar);
        mapped = true;
    }
    if (!mapped && !MapBarForTable(dev, bir, offInBar, tableBytes))
    {
        Mm::Free(EntryVector);
        EntryVector = nullptr;
        Table = nullptr;
        Count = 0;
        return false;
    }

    for (u16 i = 0; i < Count; i++)
        Mask(i);

    /* Map the device with the ITS up front (RID = deviceId on QEMU virt) */
    u32 deviceId = ((u32)dev->Bus << 8) | ((u32)dev->Slot << 3) | dev->Func;
    Its::GetInstance().MapDevice(deviceId, Count);

    Trace(0, "MsixTable(its): %u entries cap 0x%p dev 0x%p",
        (ulong)Count, (ulong)cap, (ulong)deviceId);
    return true;
}

u8 MsixTable::AllocVector()
{
    /* Unused on arm64 (LPIs are allocated by the ITS); kept for ABI. */
    return 0;
}

void MsixTable::DisableLegacyIntx(Pci::DeviceInfo* dev)
{
    auto& pci = Pci::GetInstance();
    u16 cmd = pci.ReadWord(dev->Bus, dev->Slot, dev->Func, 0x04);
    cmd = (u16)(cmd | PciCommandIntxDisable);
    pci.WriteWord(dev->Bus, dev->Slot, dev->Func, 0x04, cmd);
}

u8 MsixTable::EnableVector(u16 index, InterruptHandler& handler)
{
    if (!Table || !Dev || index >= Count || CapOffset == 0)
        return 0;
    if (EntryVector[index] != 0)
        return EntryVector[index];

    u32 deviceId = ((u32)Dev->Bus << 8) | ((u32)Dev->Slot << 3) | Dev->Func;

    u64 msiAddr = 0;
    u32 msiData = Its::GetInstance().MapEvent(deviceId, index, handler, msiAddr);
    if (msiAddr == 0)
    {
        Trace(0, "MsixTable(its): MapEvent failed");
        return 0;
    }

    volatile u8* entry = Table + (ulong)index * 16;
    {
        Stdlib::AutoLock lock(EntryLock);
        MmioWrite32(entry + 0, (u32)(msiAddr & 0xFFFFFFFF));
        MmioWrite32(entry + 4, (u32)(msiAddr >> 32));
        MmioWrite32(entry + 8, msiData);
        MmioWrite32(entry + 12, 0); /* unmasked */
    }

    if (!PciMsixEnabled)
    {
        DisableLegacyIntx(Dev);
        auto& pci = Pci::GetInstance();
        u16 mc = pci.ReadWord(Dev->Bus, Dev->Slot, Dev->Func, (u16)(CapOffset + 2));
        mc = (u16)((mc | MsixControlEnable) & ~MsixControlFuncMask);
        pci.WriteWord(Dev->Bus, Dev->Slot, Dev->Func, (u16)(CapOffset + 2), mc);
        PciMsixEnabled = true;
    }

    /* Non-zero token so re-enable is idempotent; the actual routing token
       is the ITS LPI, not a CPU vector. */
    EntryVector[index] = (u8)(index + 1);
    return EntryVector[index];
}

void MsixTable::Mask(u16 index)
{
    if (!Table || index >= Count)
        return;
    Stdlib::AutoLock lock(EntryLock);
    volatile u8* entry = Table + (ulong)index * 16;
    u32 ctrl = MmioRead32(entry + 12);
    MmioWrite32(entry + 12, ctrl | 1);
}

void MsixTable::Unmask(u16 index)
{
    if (!Table || index >= Count)
        return;
    Stdlib::AutoLock lock(EntryLock);
    volatile u8* entry = Table + (ulong)index * 16;
    u32 ctrl = MmioRead32(entry + 12);
    MmioWrite32(entry + 12, ctrl & ~1u);
}

void MsixTable::Retarget(u16 index, u32 apicId)
{
    /* No MSI balancing on arm64 yet (all LPIs on the boot collection) */
    (void)index;
    (void)apicId;
}

}
