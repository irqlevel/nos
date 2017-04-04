#pragma once

#include "types.h"
#include "idt.h"
#include "timer.h"
#include "ring_buffer.h"
#include "spin_lock.h"

namespace Kernel
{

namespace Core
{

class IO8042Observer
{
public:
    virtual void OnChar(char c) = 0;
};

class IO8042 : public TimerCallback
{
public:
    static IO8042& GetInstance()
    {
        static IO8042 instance;
        return instance;
    }

    void RegisterInterrupt(int intNum);
    void UnregisterInterrupt();

    void OnInterrupt();

    static void Interrupt();

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
    Shared::RingBuffer<u8, Shared::PageSize> Buf;

    int IntNum;
    u8 Mod;

    static const size_t MaxObserver = 16;
    IO8042Observer* Observer[MaxObserver];
};

}
}
