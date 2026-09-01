#include "dmesg.h"
#include "panic.h"

namespace Kernel
{

Dmesg::Dmesg()
    : Active(false)
    , Lost(0)
{
}

Dmesg::~Dmesg()
{
    Reset();
}

bool Dmesg::Setup()
{
    if (Active)
        return false;

    Lost = 0;
    Active = MsgBuf.Setup((ulong)&Buf[0], (ulong)&Buf[0] + sizeof(Buf), sizeof(DmesgMsg));
    return Active;
}

void Dmesg::Reset()
{
    Active = false;
    Stdlib::AutoLock lock(Lock);
    while (!MsgList.IsEmpty())
    {
        DmesgMsg *msg = CONTAINING_RECORD(MsgList.RemoveHead(), DmesgMsg, ListEntry);
        MsgBuf.Free(msg);
    }
}

void Dmesg::VPrintf(const char *fmt, va_list args)
{
    if (!Active)
        return;

    ulong retries = 0;

restart:
    DmesgMsg* msg = (DmesgMsg*)MsgBuf.Alloc();
    if (msg == nullptr)
    {
        Stdlib::AutoLock lock(Lock);
        if (MsgList.IsEmpty())
        {
            Lost++;
            return;
        }

        for (auto listEntry = MsgList.Flink; listEntry != &MsgList; listEntry = listEntry->Flink)
        {
            auto candMsg = CONTAINING_RECORD(listEntry, DmesgMsg, ListEntry);
            if (candMsg->Usage.Get() == 0)
            {
                msg = candMsg;
                msg->ListEntry.RemoveInit();
                break;
            }
        }

        if (msg == nullptr)
        {
            /* Nothing free and nothing recyclable: every message in the list
               is pinned by a walk. Retrying bets on a walk stepping on within
               the next few instructions -- past that this is a spin in the
               trace path, and a CPU stuck printing is worse than a lost line. */
            if (++retries < MaxRecycleRetries)
                goto restart;

            Lost++;
            return;
        }

        Lost++;
    }
    else
    {
        msg->Init();
    }

	int size = Stdlib::VsnPrintf(msg->Str, sizeof(msg->Str), fmt, args);
    if (size < 0)
    {
        MsgBuf.Free(msg);
		return;
    }

    Stdlib::AutoLock lock(Lock);
    BugOn(msg->Usage.Get() != 0);
    MsgList.InsertTail(&msg->ListEntry);
}

void Dmesg::Printf(const char *fmt, ...)
{
    va_list args;

    va_start(args, fmt);
    VPrintf(fmt, args);
    va_end(args);
}

void Dmesg::PrintString(const char *s)
{
    Printf("%s", s);
}

DmesgMsg* Dmesg::Next(DmesgMsg* current)
{
    BugOn(current != nullptr && current->Usage.Get() == 0);

    Stdlib::AutoLock lock(Lock);

    Stdlib::ListEntry* nextListEntry;
    if (current == nullptr)
    {
        nextListEntry = MsgList.Flink;
    }
    else
    {
        nextListEntry = current->ListEntry.Flink;
        BugOn(current->Usage.Get() <= 0);
        current->Usage.Dec();
    }

    if (nextListEntry == &MsgList)
    {
        return nullptr;
    }

    DmesgMsg* next = CONTAINING_RECORD(nextListEntry, DmesgMsg, ListEntry);
    next->Usage.Inc();
    return next;
}

DmesgMsg* Dmesg::Prev(DmesgMsg* current)
{
    BugOn(current != nullptr && current->Usage.Get() == 0);

    Stdlib::AutoLock lock(Lock);

    Stdlib::ListEntry* prevListEntry;
    if (current == nullptr)
    {
        prevListEntry = MsgList.Blink;
    }
    else
    {
        prevListEntry = current->ListEntry.Blink;
        BugOn(current->Usage.Get() <= 0);
        current->Usage.Dec();
    }

    if (prevListEntry == &MsgList)
    {
        return nullptr;
    }

    DmesgMsg* prev = CONTAINING_RECORD(prevListEntry, DmesgMsg, ListEntry);
    prev->Usage.Inc();
    return prev;
}

void Dmesg::Release(DmesgMsg* msg)
{
    if (msg == nullptr)
        return;

    Stdlib::AutoLock lock(Lock);
    BugOn(msg->Usage.Get() <= 0);
    msg->Usage.Dec();
}

ulong Dmesg::GetLost()
{
    Stdlib::AutoLock lock(Lock);
    return Lost;
}

DmesgMsg* Dmesg::TailStart(ulong lastLines)
{
    DmesgMsg* msg = Prev(nullptr);

    for (ulong i = 1; msg != nullptr && i < lastLines; i++)
    {
        DmesgMsg* prev = Prev(msg);
        if (prev == nullptr)
        {
            /* The log is shorter than the walk asked for. Prev() released the
               pin on its way off the head, so start over from the head rather
               than try to resurrect it. */
            return Next(nullptr);
        }

        msg = prev;
    }

    return msg;
}

void Dmesg::Dump(Stdlib::Printer& printer, ulong lastLines, const char* filter)
{
    if (lastLines > MaxMsgs)
        lastLines = MaxMsgs;

    /* A walk longer than the list can be is following a tail another CPU keeps
       extending, so it would never return. Bound both flavours. */
    const ulong limit = (lastLines != 0) ? lastLines : MaxMsgs;

    ulong lost = GetLost();
    if (lost != 0)
        printer.Printf("dmesg: %u earlier lines lost\n", lost);

    DmesgMsg* msg = (lastLines != 0) ? TailStart(lastLines) : Next(nullptr);

    for (ulong visited = 0; msg != nullptr; visited++)
    {
        if (filter == nullptr || Stdlib::StrStr(msg->Str, filter) != nullptr)
            printer.PrintString(msg->Str);

        if (visited + 1 == limit)
        {
            Release(msg);
            break;
        }

        msg = Next(msg);
    }
}

}