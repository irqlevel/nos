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
        return;

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

void Dmesg::Dump(Shared::Printer& printer)
{
    Shared::ListEntry msgList;

    {
        Shared::AutoLock lock(Lock);
        msgList.MoveTailList(&MsgList);
    }

    for (Shared::ListEntry* entry = msgList.Flink;
        entry != &msgList;
        entry = entry->Flink)
    {
        DmesgMsg* msg = CONTAINING_RECORD(entry, DmesgMsg, ListEntry);
        printer.PrintString(msg->Str);
    }

    {
        Shared::AutoLock lock(Lock);
        while (!msgList.IsEmpty())
        {
            DmesgMsg* msg = CONTAINING_RECORD(msgList.RemoveHead(), DmesgMsg, ListEntry);
            MsgList.InsertTail(&msg->ListEntry);
        }
    }
}

}