#include "irq_balance.h"
#include "cpu.h"
#include "trace.h"

#include <arch/x86_64/ioapic.h>
#include <drivers/msix.h>

namespace kernel
{

/* Running-CPU bits an IOAPIC redirection entry can name, for the diagnostic
   below: everything from apic id 0 up to IoApic::MAX_PHYSICAL_DEST. */
static const ulong IO_APIC_TARGET_MASK =
    (IoApic::MAX_PHYSICAL_DEST >= MAX_CPUS - 1)
        ? ~0UL
        : ((1UL << (IoApic::MAX_PHYSICAL_DEST + 1)) - 1);

IrqBalance::IrqBalance()
    : entry_count_(0)
    , next_cpu_(0)
    , next_io_apic_cpu_(0)
    , balanced_(false)
{
    stdlib::mem_set(entries_, 0, sizeof(entries_));
}

IrqBalance::~IrqBalance()
{
}

ulong IrqBalance::next_cpu_lock_held(bool io_apic)
{
    auto& cpus = CpuTable::get_instance();

    /* An IOAPIC line that cannot be delivered is worse than one that is not
       balanced, so its fallback is the BSP rather than whichever CPU happens
       to be running this. */
    ulong fallback = io_apic ? cpus.get_bsp_index() : cpus.get_current_cpu_id();

    ulong cpu_mask = cpus.get_running_cpus();
    if (cpu_mask == 0)
        return fallback;

    ulong& cursor = io_apic ? next_io_apic_cpu_ : next_cpu_;

    /* Cycle through running CPUs starting after the last assigned one */
    for (ulong i = 1; i <= MAX_CPUS; i++)
    {
        ulong cpu = (cursor + i) % MAX_CPUS;

        if (!(cpu_mask & (1UL << cpu)))
            continue;

        /* On x86 the CPU index is the apic id, and an IOAPIC entry in
           physical mode can only name the low ones -- see IoApic::can_target.
           MSI-X has the full 8-bit field and takes any of them. */
        if (io_apic && !IoApic::can_target(cpu))
            continue;

        cursor = cpu;
        return cpu;
    }

    return fallback;
}

void IrqBalance::apply_lock_held(Entry& entry)
{
    /* A line's destination as the IOAPIC took it, so that trace_entry says
       where the line really goes */
    if (entry.kind == KIND_IO_APIC)
        entry.cpu = IoApic::get_instance().set_irq_destination(entry.gsi, entry.cpu);
    else
        entry.table->retarget(entry.index, (u32)entry.cpu);
}

void IrqBalance::trace_entry(const Entry& entry)
{
    if (entry.kind == KIND_IO_APIC)
    {
        trace(0, "IrqBalance: gsi 0x%lX -> cpu %lu", (ulong)entry.gsi, entry.cpu);
    }
    else
    {
        trace(0, "IrqBalance: msix 0x%lX[%lu] -> cpu %lu",
            (ulong)entry.table, (ulong)entry.index, entry.cpu);
    }
}

ulong IrqBalance::assign(Entry entry)
{
    bool recorded = false;
    {
        stdlib::AutoLock lock(lock_);

        /* Before balance() all IRQs stay on the registering CPU (the BSP)
           and get spread once SMP bringup completes; afterwards new IRQs
           join the round-robin immediately. */
        entry.cpu = balanced_ ? next_cpu_lock_held(entry.kind == KIND_IO_APIC)
                             : CpuTable::get_instance().get_current_cpu_id();

        if (entry_count_ < MAX_ENTRIES)
        {
            entries_[entry_count_++] = entry;
            recorded = true;
        }
    }

    if (!recorded)
        trace(0, "IrqBalance: entry table full, irq not balanced");

    return entry.cpu;
}

ulong IrqBalance::assign_io_apic_irq(u8 gsi)
{
    Entry entry;
    entry.kind = KIND_IO_APIC;
    entry.gsi = gsi;
    entry.table = nullptr;
    entry.index = 0;
    entry.cpu = 0;
    return assign(entry);
}

ulong IrqBalance::assign_msix(MsixTable* table, u16 index)
{
    Entry entry;
    entry.kind = KIND_MSIX;
    entry.gsi = 0;
    entry.table = table;
    entry.index = index;
    entry.cpu = 0;
    return assign(entry);
}

void IrqBalance::remove_msix(MsixTable* table)
{
    stdlib::AutoLock lock(lock_);

    ulong dst = 0;
    for (ulong src = 0; src < entry_count_; src++)
    {
        if (entries_[src].kind == KIND_MSIX && entries_[src].table == table)
            continue;

        if (dst != src)
            entries_[dst] = entries_[src];
        dst++;
    }
    entry_count_ = dst;
}

void IrqBalance::balance()
{
    /* What was done, said once the lock is down: the entries as they were
       applied. A table's address is only printed, never followed, so one
       removed meanwhile is no matter. */
    Entry applied[MAX_ENTRIES];
    ulong count = 0;
    {
        stdlib::AutoLock lock(lock_);

        if (balanced_)
            return;
        balanced_ = true;

        /* Start the round-robin after the BSP so device IRQs prefer
           the other CPUs (the BSP keeps the system IRQs) */
        next_cpu_ = CpuTable::get_instance().get_bsp_index();
        next_io_apic_cpu_ = next_cpu_;

        for (ulong i = 0; i < entry_count_; i++)
        {
            entries_[i].cpu = next_cpu_lock_held(entries_[i].kind == KIND_IO_APIC);
            apply_lock_held(entries_[i]);
            applied[i] = entries_[i];
        }
        count = entry_count_;
    }

    for (ulong i = 0; i < count; i++)
        trace_entry(applied[i]);

    ulong cpu_mask = CpuTable::get_instance().get_running_cpus();
    ulong io_apic_mask = cpu_mask & IO_APIC_TARGET_MASK;

    trace(0, "IrqBalance: %lu irqs balanced over cpu mask 0x%lX, ioapic-reachable 0x%lX",
        count, cpu_mask, io_apic_mask);
}

}
