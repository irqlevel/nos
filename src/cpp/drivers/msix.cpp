#include "msix.h"

#include "mmio.h"

#include <include/const.h>
#include <kernel/idt.h>
#include <kernel/idt_descriptor.h>
#include <kernel/trace.h>

#include "lapic.h"
#include <kernel/irq_balance.h>
#include <lib/stdlib.h>
#include <mm/new.h>
#include <mm/page_table.h>

namespace Kernel
{

static const u8 PciCapIdMsix = 0x11;

static const u16 PciCommandIntxDisable = (u16)(1 << 10);

static const u16 MsixControlEnable = (u16)(1 << 15);
static const u16 MsixControlFuncMask = (u16)(1 << 14);

static const u32 MsixAddrBase = 0xFEE00000U;

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
    IrqBalance::GetInstance().RemoveMsix(this);

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

    bool is64 = ((barVal & 0x6) == 0x4) && (bar + 1 < 6);
    u32 barHigh = 0;
    if (is64)
    {
        barHigh = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, (u8)(bar + 1));
        physAddr |= ((ulong)barHigh << 32);
    }

    if (physAddr == 0)
        return false;

    /* Size-probe both halves of a 64-bit BAR; probing only the low half
       computes a bogus size for BARs >= 4 GB */
    pci.WriteDword(dev->Bus, dev->Slot, dev->Func, (u16)(0x10 + bar * 4), 0xFFFFFFFF);
    u32 sizeMaskLow = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, bar);
    u32 sizeMaskHigh = 0xFFFFFFFF;
    if (is64)
    {
        pci.WriteDword(dev->Bus, dev->Slot, dev->Func, (u16)(0x10 + (bar + 1) * 4), 0xFFFFFFFF);
        sizeMaskHigh = pci.GetBAR(dev->Bus, dev->Slot, dev->Func, (u8)(bar + 1));
        pci.WriteDword(dev->Bus, dev->Slot, dev->Func, (u16)(0x10 + (bar + 1) * 4), barHigh);
    }
    pci.WriteDword(dev->Bus, dev->Slot, dev->Func, (u16)(0x10 + bar * 4), barVal);

    ulong sizeMask = ((ulong)sizeMaskHigh << 32) | (ulong)(sizeMaskLow & ~0xFU);
    ulong barSize = ~sizeMask + 1;
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
    for (;;)
    {
        long curr = NextVectorOffset.Get();
        ulong vector = MsixVectorBase + (ulong)curr;
        if (vector > MsixVectorLimit)
            return 0;
        if (NextVectorOffset.Cmpxchg(curr + 1, curr) == curr)
            return (u8)vector;
    }
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

    /* AssignMsix takes the IrqBalance lock; it must complete before
       EntryLock is taken because Balance() nests them the other way
       around (IrqBalance lock -> Retarget -> EntryLock). */
    u32 apicId = (u32)IrqBalance::GetInstance().AssignMsix(this, index);
    u32 addrLow = MsixAddrBase | (apicId << 12);

    volatile u8* entry = Table + (ulong)index * 16;

    {
        Stdlib::AutoLock lock(EntryLock);

        MmioWrite32(entry + 0, addrLow);
        MmioWrite32(entry + 4, 0);
        MmioWrite32(entry + 8, (u32)vector);
        MmioWrite32(entry + 12, 1);
    }

    Idt::GetInstance().SetDescriptor(vector, IdtDescriptor::Encode(handler.GetHandlerFn()));
    handler.OnInterruptRegister(0, vector);

    {
        Stdlib::AutoLock lock(EntryLock);
        MmioWrite32(entry + 12, 0);
    }

    if (!PciMsixEnabled)
    {
        DisableLegacyIntx(Dev);
        auto& pci = Pci::GetInstance();
        u16 mc = pci.ReadWord(Dev->Bus, Dev->Slot, Dev->Func, (u16)(CapOffset + 2));
        /* Clear Function Mask too: firmware may have left it set, which
           would keep every vector masked after enable */
        mc = (u16)((mc | MsixControlEnable) & ~MsixControlFuncMask);
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
    if (!Table || index >= Count || EntryVector[index] == 0)
        return;

    Stdlib::AutoLock lock(EntryLock);

    volatile u8* entry = Table + (ulong)index * 16;

    /* The address must not change while the entry is unmasked */
    u32 ctrl = MmioRead32(entry + 12);
    MmioWrite32(entry + 12, ctrl | 1);

    MmioWrite32(entry + 0, MsixAddrBase | (apicId << 12));

    MmioWrite32(entry + 12, ctrl);
}

}
