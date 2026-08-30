#pragma once

#include <kernel/spin_lock.h>
#include <lib/stdlib.h>

namespace Kernel
{

class IoApic final
{
public:
    static IoApic& GetInstance()
    {
        static IoApic Instance;
        return Instance;
    }

    void Enable();

    void SetIrq(u8 irq, u64 apicId, u8 vector);
    void SetIrqLevel(u8 irq, u64 apicId, u8 vector, bool activeHigh);

    /* Rewrite only the destination of an already programmed entry */
    void SetIrqDestination(u8 irq, u64 apicId);

    /* Physical destination mode names the target in bits 59:56 of the
       redirection entry -- four bits, so only apic ids 0..15 are
       addressable. Bits 63:60 are reserved and do not extend the field, so a
       higher id silently aliases: 24 lands on 8, and 50 lands on 2. On a
       hybrid Intel part, whose apic ids run 0,1,8,9,16,17,...,48,49,...,
       several of those aliases name an id no CPU has, and the line is then
       never delivered at all -- an IRQ that simply stops arriving.
       IrqBalance picks IOAPIC destinations through this. */
    static const ulong MaxPhysicalDest = 15;

    static bool CanTarget(ulong apicId)
    {
        return apicId <= MaxPhysicalDest;
    }

private:
    IoApic();
    ~IoApic();
    IoApic(const IoApic& other) = delete;
    IoApic(IoApic&& other) = delete;
    IoApic& operator=(const IoApic& other) = delete;
    IoApic& operator=(IoApic&& other) = delete;

    u32 ReadRegister(u8 reg);
    void WriteRegister(u8 reg, u32 value);

    void SetEntry(u8 index, u64 data);

    /* Backstop for a destination the entry cannot encode; the caller is
       supposed to have filtered it out with CanTarget() already. */
    u64 CheckDest(u8 irq, u64 apicId);

    static const ulong RegSel = 0x0;
    static const ulong RegWin = 0x10;

    static const ulong ApicId = 0x0;
    static const ulong ApicVer = 0x1;
    static const ulong ApicArb = 0x2;
    static const ulong RedTbl = 0x10;

    static const ulong DelivModeShift = 8;
    static const ulong DestModeShift = 11;
    static const ulong DelivStatusShift = 12;
    static const ulong PolarityShift = 13;
    static const ulong RemoteIrrShift = 14;
    static const ulong TriggerModeShift = 15;
    static const ulong MaskedShift = 16;
    static const ulong DestShift = 56;

    static const ulong TriggerEdge = 0;
    static const ulong TriggerLevel = 1;

    static const ulong DmFixed = 0x0;

    void *BaseAddress;
    SpinLock OpLock;
};

}