#include "irq_balance.h"
#include "cpu.h"
#include "trace.h"

#include <arch/x86_64/ioapic.h>
#include <drivers/msix.h>

namespace Kernel
{

IrqBalance::IrqBalance()
    : EntryCount(0)
    , NextCpu(0)
    , Balanced(false)
{
    Stdlib::MemSet(Entries, 0, sizeof(Entries));
}

IrqBalance::~IrqBalance()
{
}

ulong IrqBalance::NextCpuLockHeld()
{
    ulong cpuMask = CpuTable::GetInstance().GetRunningCpus();
    if (cpuMask == 0)
        return CpuTable::GetInstance().GetCurrentCpuId();

    /* Cycle through running CPUs starting after the last assigned one */
    for (ulong i = 1; i <= MaxCpus; i++)
    {
        ulong cpu = (NextCpu + i) % MaxCpus;
        if (cpuMask & (1UL << cpu))
        {
            NextCpu = cpu;
            return cpu;
        }
    }

    return CpuTable::GetInstance().GetCurrentCpuId();
}

void IrqBalance::ApplyLockHeld(Entry& entry)
{
    if (entry.Kind == KindIoApic)
    {
        IoApic::GetInstance().SetIrqDestination(entry.Gsi, entry.Cpu);
        Trace(0, "IrqBalance: gsi 0x%p -> cpu %u", (ulong)entry.Gsi, entry.Cpu);
    }
    else
    {
        entry.Table->Retarget(entry.Index, (u32)entry.Cpu);
        Trace(0, "IrqBalance: msix 0x%p[%u] -> cpu %u",
            (ulong)entry.Table, (ulong)entry.Index, entry.Cpu);
    }
}

ulong IrqBalance::Assign(Entry entry)
{
    Stdlib::AutoLock lock(Lock);

    /* Before Balance() all IRQs stay on the registering CPU (the BSP)
       and get spread once SMP bringup completes; afterwards new IRQs
       join the round-robin immediately. */
    entry.Cpu = Balanced ? NextCpuLockHeld() : CpuTable::GetInstance().GetCurrentCpuId();

    if (EntryCount < MaxEntries)
        Entries[EntryCount++] = entry;
    else
        Trace(0, "IrqBalance: entry table full, irq not balanced");

    return entry.Cpu;
}

ulong IrqBalance::AssignIoApicIrq(u8 gsi)
{
    Entry entry;
    entry.Kind = KindIoApic;
    entry.Gsi = gsi;
    entry.Table = nullptr;
    entry.Index = 0;
    entry.Cpu = 0;
    return Assign(entry);
}

ulong IrqBalance::AssignMsix(MsixTable* table, u16 index)
{
    Entry entry;
    entry.Kind = KindMsix;
    entry.Gsi = 0;
    entry.Table = table;
    entry.Index = index;
    entry.Cpu = 0;
    return Assign(entry);
}

void IrqBalance::RemoveMsix(MsixTable* table)
{
    Stdlib::AutoLock lock(Lock);

    ulong dst = 0;
    for (ulong src = 0; src < EntryCount; src++)
    {
        if (Entries[src].Kind == KindMsix && Entries[src].Table == table)
            continue;

        if (dst != src)
            Entries[dst] = Entries[src];
        dst++;
    }
    EntryCount = dst;
}

void IrqBalance::Balance()
{
    Stdlib::AutoLock lock(Lock);

    if (Balanced)
        return;
    Balanced = true;

    /* Start the round-robin after the BSP so device IRQs prefer
       the other CPUs (the BSP keeps the system IRQs) */
    NextCpu = CpuTable::GetInstance().GetBspIndex();

    for (ulong i = 0; i < EntryCount; i++)
    {
        Entries[i].Cpu = NextCpuLockHeld();
        ApplyLockHeld(Entries[i]);
    }

    Trace(0, "IrqBalance: %u irqs balanced over cpu mask 0x%p",
        EntryCount, CpuTable::GetInstance().GetRunningCpus());
}

}
