#pragma once

#include <lib/printer.h>
#include <lib/stdlib.h>
#include <drivers/vga.h>
#include <drivers/serial.h>
#include <kernel/parameters.h>

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

    bool UseVga()
    {
        return !Parameters::GetInstance().IsConsoleSerial();
    }

    bool UseSerial()
    {
        return !Parameters::GetInstance().IsConsoleVga();
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

        if (UseVga())
            VgaTerm::GetInstance().PrintString(str);
        if (UseSerial())
            Serial::GetInstance().PrintString(str);
    }

    virtual void PrintString(const char *s) override
    {
        if (UseVga())
            VgaTerm::GetInstance().PrintString(s);
        if (UseSerial())
            Serial::GetInstance().PrintString(s);
    }

    virtual void Backspace() override
    {
        if (UseVga())
            VgaTerm::GetInstance().Backspace();
        if (UseSerial())
            Serial::GetInstance().Backspace();
    }

    void Cls()
    {
        if (UseVga())
            VgaTerm::GetInstance().Cls();
        if (UseSerial())
        {
            /* ANSI escape: clear screen and move cursor home */
            Serial::GetInstance().PrintString("\033[2J\033[H");
        }
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
