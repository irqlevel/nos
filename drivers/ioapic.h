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
    void SetIrqLevel(u8 irq, u64 apicId, u8 vector);

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