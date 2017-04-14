#pragma once

#include "spin_lock.h"

#include <lib/stdlib.h>
#include <lib/ring_buffer.h>
#include <lib/printer.h>

namespace Kernel
{

struct DmesgMsg final
{
    char Str[256];

    DmesgMsg& operator=(const DmesgMsg& other)
    {
        if (this != &other)
        {
            Shared::MemCpy(Str, other.Str, sizeof(other.Str));
        }
        return *this;
    }
};

class Dmesg final : public Shared::TypePrinter<DmesgMsg>
{
public:

    static Dmesg& GetInstance()
    {
        static Dmesg instance;
        return instance;
    }

    void VPrintf(const char *fmt, va_list args);
    void Printf(const char *fmt, ...);
    void PrintString(const char *s);

    void Dump(Shared::Printer& printer);

    void PrintElement(const DmesgMsg& element) override;

private:
    Dmesg();
    ~Dmesg();

    void AppendMsg(const DmesgMsg& msg);

    Dmesg(const Dmesg& other) = delete;
    Dmesg(Dmesg&& other) = delete;
    Dmesg& operator=(const Dmesg& other) = delete;
    Dmesg& operator=(Dmesg&& other) = delete;

    Shared::RingBuffer<DmesgMsg, 1000> MsgBuf;
    SpinLock Lock;
    Shared::Printer* Printer;
};

}