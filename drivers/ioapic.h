#pragma once

#include <kernel/spin_lock.h>
#include <lib/stdlib.h>

namespace Kernel
{


namespace Core
{

class IoApic final
{
public:
    static IoApic& GetInstance()
    {
        static IoApic instance;
        return instance;
    }

    void Enable();

    void SetIrq(u8 irq, u64 apicId, u8 vector);

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

    void *BaseAddress;
    SpinLock OpLock;
};

}
}