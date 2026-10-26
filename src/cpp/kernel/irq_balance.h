#pragma once

#include <lib/stdlib.h>
#include "spin_lock.h"

namespace kernel
{

class MsixTable;

/*
 * Simple irqbalance: device IRQs (level-triggered IOAPIC lines and MSI-X
 * vectors) are recorded at registration time and spread round-robin
 * across running CPUs once SMP bringup completes (Balance()).
 * Devices which register after Balance() get the next CPU immediately.
 * System IRQs (PIT/HPET/8042/serial) are not recorded and stay on the BSP.
 *
 * The two kinds do not share a cursor, because they cannot address the same
 * set of CPUs: an MSI-X entry carries a full 8-bit destination, while an
 * IOAPIC redirection entry in physical mode has four bits and reaches apic
 * ids 0..15 only (IoApic::CanTarget). Handing an IOAPIC line a higher id
 * does not spread it, it aliases it -- onto another CPU, or on a hybrid part
 * onto an id no CPU has, which loses the interrupt for good.
 */
class IrqBalance
{
public:
    static IrqBalance& get_instance()
    {
        static IrqBalance instance;
        return instance;
    }

    /* Record a level-triggered IOAPIC device IRQ. Returns the
       destination CPU to program into the redirection entry. */
    ulong assign_io_apic_irq(u8 gsi);

    /* Record an MSI-X vector. Returns the destination CPU to program
       into the table entry. */
    ulong assign_msix(MsixTable* table, u16 index);

    /* Forget the MSI-X vectors of a table being destroyed. */
    void remove_msix(MsixTable* table);

    /* Called once after SMP bringup: spread recorded IRQs across CPUs. */
    void balance();

private:
    IrqBalance();
    ~IrqBalance();
    IrqBalance(const IrqBalance& other) = delete;
    IrqBalance(IrqBalance&& other) = delete;
    IrqBalance& operator=(const IrqBalance& other) = delete;
    IrqBalance& operator=(IrqBalance&& other) = delete;

    static const u8 KIND_IO_APIC = 0;
    static const u8 KIND_MSIX = 1;

    struct Entry
    {
        u8 kind;
        u8 gsi;
        u16 index;
        MsixTable* table;
        ulong cpu;
    };

    ulong assign(Entry entry);
    ulong next_cpu_lock_held(bool io_apic);
    void apply_lock_held(Entry& entry);

    static const ulong MAX_ENTRIES = 64;

    Entry entries_[MAX_ENTRIES];
    ulong entry_count_;
    ulong next_cpu_;       /* round-robin cursor for MSI-X vectors */
    ulong next_io_apic_cpu_; /* and for IOAPIC lines, over a smaller CPU set */
    bool balanced_;
    SpinLock lock_;
};

}
