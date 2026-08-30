#include "irq_balance.h"
#include "cpu.h"
#include "trace.h"

#include <arch/x86_64/ioapic.h>
#include <drivers/msix.h>

namespace Kernel
{

/* Running-CPU bits an IOAPIC redirection entry can name, for the diagnostic
   below: everything from apic id 0 up to IoApic::MaxPhysicalDest. */
static const ulong IoApicTargetMask =
    (IoApic::MaxPhysicalDest >= MaxCpus - 1)
        ? ~0UL
        : ((1UL << (IoApic::MaxPhysicalDest + 1)) - 1);

IrqBalance::IrqBalance()
    : EntryCount(0)
    , NextCpu(0)
    , NextIoApicCpu(0)
    , Balanced(false)
{
    Stdlib::MemSet(Entries, 0, sizeof(Entries));
}

IrqBalance::~IrqBalance()
{
}

ulong IrqBalance::NextCpuLockHeld(bool ioApic)
{
    auto& cpus = CpuTable::GetInstance();

    /* An IOAPIC line that cannot be delivered is worse than one that is not
       balanced, so its fallback is the BSP rather than whichever CPU happens
       to be running this. */
    ulong fallback = ioApic ? cpus.GetBspIndex() : cpus.GetCurrentCpuId();

    ulong cpuMask = cpus.GetRunningCpus();
    if (cpuMask == 0)
        return fallback;

    ulong& cursor = ioApic ? NextIoApicCpu : NextCpu;

    /* Cycle through running CPUs starting after the last assigned one */
    for (ulong i = 1; i <= MaxCpus; i++)
    {
        ulong cpu = (cursor + i) % MaxCpus;

        if (!(cpuMask & (1UL << cpu)))
            continue;

        /* On x86 the CPU index is the apic id, and an IOAPIC entry in
           physical mode can only name the low ones -- see IoApic::CanTarget.
           MSI-X has the full 8-bit field and takes any of them. */
        if (ioApic && !IoApic::CanTarget(cpu))
            continue;

        cursor = cpu;
        return cpu;
    }

    return fallback;
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
    entry.Cpu = Balanced ? NextCpuLockHeld(entry.Kind == KindIoApic)
                         : CpuTable::GetInstance().GetCurrentCpuId();

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
    NextIoApicCpu = NextCpu;

    for (ulong i = 0; i < EntryCount; i++)
    {
        Entries[i].Cpu = NextCpuLockHeld(Entries[i].Kind == KindIoApic);
        ApplyLockHeld(Entries[i]);
    }

    ulong cpuMask = CpuTable::GetInstance().GetRunningCpus();
    ulong ioApicMask = cpuMask & IoApicTargetMask;

    Trace(0, "IrqBalance: %u irqs balanced over cpu mask 0x%p, ioapic-reachable 0x%p",
        EntryCount, cpuMask, ioApicMask);
}

}
