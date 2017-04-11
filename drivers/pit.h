#pragma once

#include <include/types.h>
#include <kernel/atomic.h>
#include <kernel/interrupt.h>
#include <kernel/spin_lock.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace Core
{

class Pit final : public InterruptHandler
{
public:
    static Pit& GetInstance()
    {
        static Pit instance;

        return instance;
    }

    virtual void OnInterruptRegister(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn GetHandlerFn() override;

    void Interrupt(Context* ctx);

    void Setup();

    Shared::Time GetTime();

    void Wait(const Shared::Time& timeout);
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

    u32 TimeMs;
    u32 TimeMsNs;
    u32 TickMs;
    u32 TickMsNs;
    u16 ReloadValue;

    SpinLock Lock;
};


};

}