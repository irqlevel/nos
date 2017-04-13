#pragma once

#include "spin_lock.h"

#include <lib/stdlib.h>
#include <lib/ring_buffer.h>

namespace Kernel
{

class Dmesg final
{
public:

    static Dmesg& GetInstance()
    {
        static Dmesg instance;
        return instance;
    }

    class Dumper
    {
    public:
        virtual void OnMessage(const char *msg) = 0;
    };

    void Vprintf(const char *fmt, va_list args);
    void Printf(const char *fmt, ...);
    void PrintString(const char *s);

    void Dump(Dumper& dumper);

private:
    Dmesg();
    ~Dmesg();

    Dmesg(const Dmesg& other) = delete;
    Dmesg(Dmesg&& other) = delete;
    Dmesg& operator=(const Dmesg& other) = delete;
    Dmesg& operator=(Dmesg&& other) = delete;

    struct Msg final
    {
        char Str[256];

        Msg& operator=(const Msg& other)
        {
            if (this != &other)
            {
                Shared::MemCpy(Str, other.Str, sizeof(other.Str));
            }
            return *this;
        }
    };

    using MessageBuffer = Shared::RingBuffer<Msg, 1000>;

    MessageBuffer MsgBuf;
    SpinLock Lock;

    class MessageBufferDumper final : public MessageBuffer::Dumper
    {
    public:
        MessageBufferDumper(Dmesg& dmesg)
            : Parent(dmesg)
        {
        }

        ~MessageBufferDumper() {}
        virtual void OnElement(const Msg& msg) override
        {
            if (Parent.CurrentDumper != nullptr)
                Parent.CurrentDumper->OnMessage(msg.Str);
        }
    private:
        Dmesg& Parent;
    };

    Dumper* CurrentDumper;
    MessageBufferDumper MsgBufDumper;
};

}