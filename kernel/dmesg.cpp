#include "dmesg.h"
#include "panic.h"

namespace Kernel
{

Dmesg::Dmesg()
    : Active(false)
{
}

Dmesg::~Dmesg()
{
}

bool Dmesg::Setup()
{
    if (Active)
        return false;

    Active = MsgBuf.Setup((ulong)&Buf[0], (ulong)&Buf[0] + sizeof(Buf), sizeof(DmesgMsg));
    return Active;
}

void Dmesg::VPrintf(const char *fmt, va_list args)
{
    if (!Active)
        return;

restart:
    DmesgMsg* msg = (DmesgMsg*)MsgBuf.Alloc();
    if (msg == nullptr)
    {
        Stdlib::AutoLock lock(Lock);
        if (MsgList.IsEmpty())
            return;

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
            goto restart;
        }
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

void Dmesg::Dump(Stdlib::Printer& printer)
{
    for (DmesgMsg* msg = Next(nullptr); msg != nullptr; msg = Next(msg))
    {
        printer.PrintString(msg->Str);
    }
}

}