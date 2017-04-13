#pragma once

#include <include/types.h>
#include <kernel/spin_lock.h>
#include <kernel/interrupt.h>
#include <lib/ring_buffer.h>

namespace Kernel
{

class Serial final : public InterruptHandler
{
public:
    static Serial& GetInstance()
    {
        static Serial instance;

        return instance;
    }

    void PrintString(const char *str);

    void Vprintf(const char *fmt, va_list args);
    void Printf(const char *fmt, ...);

    virtual void OnInterruptRegister(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn GetHandlerFn() override;

    void Interrupt(Context* ctx);

private:
    Serial();
    ~Serial();

    void WriteChar(char c);
    void Send();
    void Wait(size_t pauseCount);

    Serial(const Serial& other) = delete;
    Serial(Serial&& other) = delete;
    Serial& operator=(const Serial& other) = delete;
    Serial& operator=(Serial&& other) = delete;

    bool IsTransmitEmpty();
    int IntVector;

    Shared::RingBuffer<char, Shared::PageSize> Buf;
    SpinLock Lock;

    static const int Port = 0x3F8;
};

}