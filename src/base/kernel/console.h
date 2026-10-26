#pragma once

#include <lib/printer.h>
#include <lib/stdlib.h>
#include <hal/console.h>

namespace kernel
{

class Console : public stdlib::Printer
{
public:
    static Console& get_instance()
    {
        static Console instance;
        return instance;
    }

    virtual PRINTF_FORMAT(2, 3) void printf(const char *fmt, ...) override
    {
        va_list args;
        va_start(args, fmt);
        v_printf(fmt, args);
        va_end(args);
    }

    virtual PRINTF_FORMAT(2, 0) void v_printf(const char *fmt, va_list args) override
    {
        char str[256];
        if (stdlib::vsn_printf(str, sizeof(str), fmt, args) < 0)
            return;

        hal::console_out(str);
    }

    virtual void print_string(const char *s) override
    {
        hal::console_out(s);
    }

    virtual void backspace() override
    {
        hal::console_out_backspace();
    }

    void cls()
    {
        hal::console_out_clear();
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
