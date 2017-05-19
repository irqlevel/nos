#include "dmesg.h"

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

    DmesgMsg* msg = (DmesgMsg*)MsgBuf.Alloc();
    if (msg == nullptr)
    {
        Shared::AutoLock lock(Lock);
        if (MsgList.IsEmpty())
            return;

        msg = CONTAINING_RECORD(MsgList.RemoveHead(), DmesgMsg, ListEntry);
    }

	int size = Shared::VsnPrintf(msg->Str, sizeof(msg->Str), fmt, args);
    if (size < 0)
    {
        MsgBuf.Free(msg);
		return;
    }

    Shared::AutoLock lock(Lock);
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
    Shared::AutoLock lock(Lock);

    if (MsgList.IsEmpty())
        return nullptr;

    Shared::ListEntry* nextListEntry;
    if (current == nullptr)
    {
        nextListEntry = MsgList.Flink;
    }
    else
    {
        nextListEntry = current->ListEntry.Flink;
    }

    if (nextListEntry == &MsgList)
    {
        return nullptr;
    }

    return CONTAINING_RECORD(nextListEntry, DmesgMsg, ListEntry);
}

void Dmesg::Dump(Shared::Printer& printer)
{
    for (DmesgMsg* msg = Next(nullptr); msg != nullptr; msg = Next(msg))
    {
        printer.PrintString(msg->Str);
    }
}

}