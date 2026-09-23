#pragma once

#include "stdlib.h"

namespace Stdlib
{

class Printer
{
public:
    virtual void Printf(const char *fmt, ...) = 0;
    virtual void VPrintf(const char *fmt, va_list args) = 0;
    virtual void PrintString(const char *s) = 0;
    virtual void Backspace() = 0;

    /* What the person at the other end types, for a command that reads it
       while it runs: up to len bytes, waiting up to timeoutNs for some.
       0 when the time passed with nothing, -1 when no one can type here --
       the console, the UDP shell, /etc/rc -- or no more will come. Only an
       SSH session's printer has anyone to ask (kernel_cmd_dispatch_io). */
    virtual long ReadInput(unsigned char* buf, unsigned long len, unsigned long long timeoutNs)
    {
        (void)buf;
        (void)len;
        (void)timeoutNs;
        return -1;
    }
};

/* A Printer into a buffer its owner provides: what a command printed, kept
   to be read back or passed on. What does not fit is dropped, and the text
   is NUL-terminated whatever happens. */
class BufferPrinter final : public Printer
{
public:
    BufferPrinter(char* buf, size_t size)
        : Buf(buf)
        , Size(size)
        , Pos(0)
    {
        if (Size != 0)
            Buf[0] = '\0';
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
        if (Pos + 1 >= Size)
            return;

        VsnPrintf(Buf + Pos, Size - Pos, fmt, args);
        Buf[Size - 1] = '\0';
        Pos += StrLen(Buf + Pos);
    }

    virtual void PrintString(const char *s) override
    {
        if (Pos + 1 >= Size)
            return;

        size_t len = StrLen(s);
        if (len > Size - 1 - Pos)
            len = Size - 1 - Pos;
        MemCpy(Buf + Pos, s, len);
        Pos += len;
        Buf[Pos] = '\0';
    }

    virtual void Backspace() override
    {
    }

    const char* Get() const
    {
        return Buf;
    }

    void Reset()
    {
        Pos = 0;
        if (Size != 0)
            Buf[0] = '\0';
    }

private:
    char* Buf;
    size_t Size;
    size_t Pos;
};

template <typename T>
class TypePrinter
{
public:
    virtual void PrintElement(const T& element) = 0;
};

};
