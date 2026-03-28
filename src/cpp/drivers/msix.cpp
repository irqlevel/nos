#include "msix.h"

#include "mmio.h"

#include <include/const.h>
#include <kernel/idt.h>
#include <kernel/idt_descriptor.h>
#include <kernel/trace.h>

#include "lapic.h"
#include <lib/stdlib.h>
#include <mm/new.h>
#include <mm/page_table.h>

namespace Kernel
{

static const u8 PciCapIdMsix = 0x11;

static const u16 PciCommandIntxDisable = (u16)(1 << 10);

static const u16 MsixControlEnable = (u16)(1 << 15);

static const u32 MsixAddrBase = 0xFEE00000U;

u8 MsixTable::NextVector = MsixVectorBase;

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
    if (Dev && CapOffset != 0 && PciMsixEnabled)
    {
        auto& pci = Pci::GetInstance();
        u16 mc = pci.ReadWord(Dev->Bus, Dev->Slot, Dev->Func, (u16)(CapOffset + 2));
        mc = (u16)(mc & ~MsixControlEnable);
        pci.WriteWord(Dev->Bus, Dev->Slot, Dev->Func, (u16)(CapOffset + 2), mc);

        u16 cmd = pci.ReadWord(Dev->Bus, Dev->Slot, Dev->Func, 0x04);
        cmd = (u16)(cmd & ~PciCommandIntxDisable);
        pci.WriteWord(Dev->Bus, Dev->Slot, Dev->Func, 0x04, cmd);
    }

    if (EntryVector)
    {
        Mm::Free(EntryVector);
        EntryVector = nullptr;
    }

    Table = nullptr;
    Count = 0;
    Dev = nullptr;
    CapOffset = 0;
    PciMsixEnabled = false;
}

bool MsixTable::MapBarForTable(Pci::DeviceInfo* dev, u8 bar, ulong offsetInBar,
    ulong tableBytes)
{
    if (bar >= 6)
        return false;

    auto& pci = Pci::GetInstance();
    u32 barVal = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, bar);

    if (barVal & 1)
    {
        Trace(0, "MsixTable: BAR%u is I/O, not MMIO", (ulong)bar);
        return false;
    }

    ulong physAddr = barVal & ~0xFUL;

    if ((barVal & 0x6) == 0x4 && bar + 1 < 6)
    {
        u32 barHigh = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, (u8)(bar + 1));
        physAddr |= ((ulong)barHigh << 32);
    }

    if (physAddr == 0)
        return false;

    pci.WriteDword(dev->Bus, dev->Slot, dev->Func, (u16)(0x10 + bar * 4), 0xFFFFFFFF);
    u32 sizeMask = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, bar);
    pci.WriteDword(dev->Bus, dev->Slot, dev->Func, (u16)(0x10 + bar * 4), barVal);

    u32 sizeMask32 = sizeMask & ~0xFU;
    ulong barSize = (ulong)(~sizeMask32) + 1;
    if (barSize == 0)
        barSize = Const::PageSize;

    if (offsetInBar + tableBytes > barSize)
    {
        Trace(0, "MsixTable: table past BAR end");
        return false;
    }

    Trace(0, "MsixTable: BAR%u phys 0x%p size 0x%p table off 0x%p",
        (ulong)bar, physAddr, barSize, offsetInBar);

    auto& pt = Mm::PageTable::GetInstance();
    ulong va = pt.MapMmioRegion(physAddr, barSize);
    if (va == 0)
    {
        Trace(0, "MsixTable: MapMmioRegion failed");
        return false;
    }

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
    {
        Trace(0, "MsixTable: no MSI-X capability");
        Table = nullptr;
        Count = 0;
        CapOffset = 0;
        return false;
    }

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
        Trace(0, "MsixTable: reusing BAR%u VA 0x%p off 0x%p",
            (ulong)bir, mappedBars[bir], offInBar);
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

    Trace(0, "MsixTable: %u entries cap 0x%p", (ulong)Count, (ulong)cap);
    return true;
}

u8 MsixTable::AllocVector()
{
    if (NextVector > MsixVectorLimit)
        return 0;
    return NextVector++;
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

    u8 vector = AllocVector();
    if (vector == 0)
    {
        Trace(0, "MsixTable: out of vectors");
        return 0;
    }

    volatile u8* entry = Table + (ulong)index * 16;
    u32 apicId = Lapic::GetApicId();
    u32 addrLow = MsixAddrBase | (apicId << 12);

    MmioWrite32(entry + 0, addrLow);
    MmioWrite32(entry + 4, 0);
    MmioWrite32(entry + 8, (u32)vector);
    MmioWrite32(entry + 12, 1);

    Idt::GetInstance().SetDescriptor(vector, IdtDescriptor::Encode(handler.GetHandlerFn()));
    handler.OnInterruptRegister(0, vector);

    MmioWrite32(entry + 12, 0);

    if (!PciMsixEnabled)
    {
        DisableLegacyIntx(Dev);
        auto& pci = Pci::GetInstance();
        u16 mc = pci.ReadWord(Dev->Bus, Dev->Slot, Dev->Func, (u16)(CapOffset + 2));
        mc = (u16)(mc | MsixControlEnable);
        pci.WriteWord(Dev->Bus, Dev->Slot, Dev->Func, (u16)(CapOffset + 2), mc);
        PciMsixEnabled = true;
    }

    EntryVector[index] = vector;
    return vector;
}

void MsixTable::Mask(u16 index)
{
    if (!Table || index >= Count)
        return;

    volatile u8* entry = Table + (ulong)index * 16;
    u32 ctrl = MmioRead32(entry + 12);
    MmioWrite32(entry + 12, ctrl | 1);
}

void MsixTable::Unmask(u16 index)
{
    if (!Table || index >= Count)
        return;

    volatile u8* entry = Table + (ulong)index * 16;
    u32 ctrl = MmioRead32(entry + 12);
    MmioWrite32(entry + 12, ctrl & ~1u);
}

}
