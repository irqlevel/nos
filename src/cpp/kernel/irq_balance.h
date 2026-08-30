#pragma once

#include <lib/stdlib.h>
#include "spin_lock.h"

namespace Kernel
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
    static IrqBalance& GetInstance()
    {
        static IrqBalance instance;
        return instance;
    }

    /* Record a level-triggered IOAPIC device IRQ. Returns the
       destination CPU to program into the redirection entry. */
    ulong AssignIoApicIrq(u8 gsi);

    /* Record an MSI-X vector. Returns the destination CPU to program
       into the table entry. */
    ulong AssignMsix(MsixTable* table, u16 index);

    /* Forget the MSI-X vectors of a table being destroyed. */
    void RemoveMsix(MsixTable* table);

    /* Called once after SMP bringup: spread recorded IRQs across CPUs. */
    void Balance();

private:
    IrqBalance();
    ~IrqBalance();
    IrqBalance(const IrqBalance& other) = delete;
    IrqBalance(IrqBalance&& other) = delete;
    IrqBalance& operator=(const IrqBalance& other) = delete;
    IrqBalance& operator=(IrqBalance&& other) = delete;

    static const u8 KindIoApic = 0;
    static const u8 KindMsix = 1;

    struct Entry
    {
        u8 Kind;
        u8 Gsi;
        u16 Index;
        MsixTable* Table;
        ulong Cpu;
    };

    ulong Assign(Entry entry);
    ulong NextCpuLockHeld(bool ioApic);
    void ApplyLockHeld(Entry& entry);

    static const ulong MaxEntries = 64;

    Entry Entries[MaxEntries];
    ulong EntryCount;
    ulong NextCpu;       /* round-robin cursor for MSI-X vectors */
    ulong NextIoApicCpu; /* and for IOAPIC lines, over a smaller CPU set */
    bool Balanced;
    SpinLock Lock;
};

}
