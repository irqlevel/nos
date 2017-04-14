#include "dmesg.h"

namespace Kernel
{

Dmesg::Dmesg()
    : Printer(nullptr)
{
}

Dmesg::~Dmesg()
{
}

void Dmesg::AppendMsg(const DmesgMsg& msg)
{
    if (MsgBuf.IsFull())
    {
        MsgBuf.PopHead();
    }

    if (!MsgBuf.Put(msg))
        return;
}

void Dmesg::VPrintf(const char *fmt, va_list args)
{
    Shared::AutoLock lock(Lock);
	DmesgMsg msg;

	int size = Shared::VsnPrintf(msg.Str, sizeof(msg.Str), fmt, args);
    if (size < 0)
		return;

    AppendMsg(msg);
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
    Shared::AutoLock lock(Lock);
	DmesgMsg msg;

    Shared::StrnCpy(msg.Str, s, Shared::ArraySize(msg.Str));
    msg.Str[Shared::ArraySize(msg.Str) - 1] = '\0';

    AppendMsg(msg);
}

void Dmesg::PrintElement(const DmesgMsg& msg)
{
    Printer->PrintString(msg.Str);
}

void Dmesg::Dump(Shared::Printer& printer)
{
    Shared::AutoLock lock(Lock);
    Printer = &printer;
    MsgBuf.Print(*this);
    Printer = nullptr;
}

}