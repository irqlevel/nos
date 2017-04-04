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

    bool Put(u8 code);
    u8 Get();

    static void Interrupt();

    virtual void OnTick(TimerCallback& callback) override;

private:
    IO8042();
    virtual ~IO8042();

    IO8042(const IO8042& other) = delete;
    IO8042(IO8042&& other) = delete;
    IO8042& operator=(const IO8042& other) = delete;
    IO8042& operator=(IO8042&& other) = delete;

    static const ulong Port = 0x60;

    RingBuffer<u8, 16, SpinLock> Buf;
    int IntNum;
    u8 Mod;
};

}
}
