#pragma once

#include <include/types.h>
#include <kernel/atomic.h>
#include <kernel/interrupt.h>
#include <kernel/spin_lock.h>
#include <kernel/asm.h>
#include <lib/stdlib.h>

namespace Kernel
{

class Pit final : public InterruptHandler
{
public:
    static Pit& GetInstance()
    {
        static Pit Instance;

        return Instance;
    }

    virtual void OnInterruptRegister(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn GetHandlerFn() override;

    void Interrupt(Context* ctx);

    void Setup();

    Stdlib::Time GetTime();

    void Wait(const Stdlib::Time& timeout);
    void Wait(ulong nanoSecs);

private:
    Pit();
    ~Pit();

    Pit(const Pit& other) = delete;
    Pit(Pit&& other) = delete;
    Pit& operator=(const Pit& other) = delete;
    Pit& operator=(Pit&& other) = delete;

    int IntVector;

    static const int Channel0Port = 0x40;
    static const int Channel1Port = 0x41;
    static const int Channel2Port = 0x42;
    static const int ModePort = 0x43;
    static const u32 HighestFrequency = 1193182;

    volatile ulong TimeMs;
    volatile ulong TimeMsNs;
    ulong TickMs;
    ulong TickMsNs;
    u16 ReloadValue;

    SpinLock Lock;
};

}