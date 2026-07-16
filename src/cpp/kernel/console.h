#pragma once

#include <lib/printer.h>
#include <lib/stdlib.h>
#include <hal/console.h>

namespace Kernel
{

class Console : public Stdlib::Printer
{
public:
    static Console& GetInstance()
    {
        static Console Instance;
        return Instance;
    }

    virtual void Printf(const char *fmt, ...) override
    {
        va_list args;
        va_start(args, fmt);
        VPrintf(fmt, args);
        va_end(args);
    }

    virtual void VPrintf(const char *fmt, va_list args) override
    {
        char str[256];
        if (Stdlib::VsnPrintf(str, sizeof(str), fmt, args) < 0)
            return;

        Hal::ConsoleOut(str);
    }

    virtual void PrintString(const char *s) override
    {
        Hal::ConsoleOut(s);
    }

    virtual void Backspace() override
    {
        Hal::ConsoleOutBackspace();
    }

    void Cls()
    {
        Hal::ConsoleOutClear();
    }

private:
    Console() {}
    ~Console() {}
    Console(const Console& other) = delete;
    Console(Console&& other) = delete;
    Console& operator=(const Console& other) = delete;
    Console& operator=(Console&& other) = delete;
};

}
