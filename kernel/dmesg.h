#pragma once

#include "spin_lock.h"
#include "atomic.h"

#include <lib/stdlib.h>
#include <lib/printer.h>
#include <mm/block_allocator.h>

namespace Kernel
{

struct DmesgMsg final
{
    char Str[256 - sizeof(Stdlib::ListEntry) - sizeof(Atomic)];
    Stdlib::ListEntry ListEntry;
    Atomic Usage;

    void Init()
    {
        ListEntry.Init();
        Usage.Set(0);
    }
};

static_assert(sizeof(DmesgMsg) == 256, "Invalid size");

class Dmesg final
{
public:

    static Dmesg& GetInstance()
    {
        static Dmesg Instance;
        return Instance;
    }

    bool Setup();

    void VPrintf(const char *fmt, va_list args);
    void Printf(const char *fmt, ...);
    void PrintString(const char *s);

    void Dump(Stdlib::Printer& printer);

    DmesgMsg* Next(DmesgMsg* current);

private:
    Dmesg();
    ~Dmesg();

    Dmesg(const Dmesg& other) = delete;
    Dmesg(Dmesg&& other) = delete;
    Dmesg& operator=(const Dmesg& other) = delete;
    Dmesg& operator=(Dmesg&& other) = delete;

    char Buf[32 * Const::PageSize]  __attribute__((aligned(sizeof(DmesgMsg))));

    Mm::BlockAllocatorImpl MsgBuf;

    Stdlib::ListEntry MsgList;

    SpinLock Lock;

    volatile bool Active;
};

}