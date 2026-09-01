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

    /* Print the log. lastLines == 0 is the whole of it; otherwise only the
       newest lastLines messages, found by walking back from the tail. filter,
       when set, prints only the messages that contain it.

       The tail is what a remote shell wants: its reply buffer holds a few
       kilobytes and drops everything past that, so a dump from the head of a
       log that has been growing for an hour returns the first minute of boot
       and nothing else. */
    void Dump(Stdlib::Printer& printer, ulong lastLines = 0,
              const char* filter = nullptr);

    /* Step a walk one message towards the tail (Next) or the head (Prev).
       nullptr starts at the head resp. the tail; the returned message is
       pinned so VPrintf cannot recycle it under the walk, and the step
       releases the message it came from. */
    DmesgMsg* Next(DmesgMsg* current);
    DmesgMsg* Prev(DmesgMsg* current);

    /* Release the message a walk is standing on. A walk that stops before it
       runs off the end must call this: a pinned message is skipped by the
       recycler forever, so an abandoned pin permanently costs the log a slot. */
    void Release(DmesgMsg* msg);

    /* Messages the log no longer has: recycled to make room, or dropped. */
    ulong GetLost();

    void Reset();

    /* The list cannot hold more messages than the buffer has slots, so a walk
       that visits more than this is not walking a log any more -- it is
       following a tail another CPU keeps extending, and it is not obliged to
       ever end. Every walk is capped here. */
    static const ulong BufSize = 128 * Const::PageSize;
    static const ulong MaxMsgs = BufSize / sizeof(DmesgMsg);

private:
    Dmesg();
    ~Dmesg();

    Dmesg(const Dmesg& other) = delete;
    Dmesg(Dmesg&& other) = delete;
    Dmesg& operator=(const Dmesg& other) = delete;
    Dmesg& operator=(Dmesg&& other) = delete;

    /* The message a lastLines dump starts printing at, pinned, or nullptr
       when the log is empty. */
    DmesgMsg* TailStart(ulong lastLines);

    /* How many times VPrintf retries when the buffer is full and every listed
       message is pinned by a reader. Retrying is a bet that a reader steps on
       within the next few instructions; past that it is an unbounded spin in
       the trace path, which costs a CPU to save one line of log. */
    static const ulong MaxRecycleRetries = 16;

    char Buf[BufSize]  __attribute__((aligned(sizeof(DmesgMsg))));

    Mm::BlockAllocatorImpl MsgBuf;

    Stdlib::ListEntry MsgList;

    SpinLock Lock;

    volatile bool Active;

    /* Guarded by Lock. */
    ulong Lost;
};

}