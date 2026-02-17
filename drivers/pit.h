#pragma once

#include <include/types.h>
#include <kernel/atomic.h>
#include <kernel/seq_lock.h>
#include <kernel/interrupt.h>
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

    /* I/O ports */
    static const u16 Channel0Port = 0x40;
    static const u16 Channel2Port = 0x42;
    static const u16 ModePort     = 0x43;

    /* Base oscillator frequency in Hz */
    static const u32 BaseFrequency = 1193182;

    /* Desired tick rate and derived reload value */
    static const u32 DesiredHz = 100;
    static const u16 DesiredReload = (BaseFrequency + DesiredHz / 2) / DesiredHz;

    /* Mode command: channel 0, lobyte/hibyte access, mode 2 (rate generator), binary */
    static const u8 ModeCh0RateGen = 0x34;

    ulong TimeMs;
    ulong TimeMsNs;
    ulong TickMs;
    ulong TickMsNs;
    u16 ReloadValue;

    SeqLock TimeLock;
};

}