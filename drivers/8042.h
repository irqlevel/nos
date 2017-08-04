#pragma once

#include <include/types.h>
#include <kernel/idt.h>
#include <kernel/timer.h>
#include <kernel/spin_lock.h>
#include <kernel/interrupt.h>
#include <kernel/asm.h>
#include <kernel/atomic.h>
#include <lib/ring_buffer.h>

namespace Kernel
{

class IO8042Observer
{
public:
    virtual void OnChar(char c) = 0;
};

class IO8042 : public TimerCallback, public InterruptHandler
{
public:
    static IO8042& GetInstance()
    {
        static IO8042 Instance;
        return Instance;
    }

    virtual void OnInterruptRegister(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn GetHandlerFn() override;

    void Interrupt(Context* ctx);

    virtual void OnTick(TimerCallback& callback) override;

    char GetCmd();

    bool RegisterObserver(IO8042Observer& observer);
    void UnregisterObserver(IO8042Observer& observer);

private:
    IO8042();
    virtual ~IO8042();

    IO8042(const IO8042& other) = delete;
    IO8042(IO8042&& other) = delete;
    IO8042& operator=(const IO8042& other) = delete;
    IO8042& operator=(IO8042&& other) = delete;

    static const ulong Port = 0x60;

    SpinLock Lock;
    Stdlib::RingBuffer<u8, Const::PageSize> Buf;

    int IntVector;
    u8 Mod;

    static const size_t MaxObserver = 16;
    IO8042Observer* Observer[MaxObserver];
    Atomic InterruptCounter;
};

}
