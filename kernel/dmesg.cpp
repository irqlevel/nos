#include "dmesg.h"

namespace Kernel
{

Dmesg::Dmesg()
    : CurrentDumper(nullptr)
    , MsgBufDumper(*this)

{
}

Dmesg::~Dmesg()
{
}

void Dmesg::Vprintf(const char *fmt, va_list args)
{
    Shared::AutoLock lock(Lock);
	Msg msg;

	int size = Shared::VsnPrintf(msg.Str, sizeof(msg.Str), fmt, args);
    if (size < 0)
		return;

    if (MsgBuf.IsFull())
    {
        MsgBuf.PopHead();
    }

    if (!MsgBuf.Put(msg))
        return;
}

void Dmesg::Printf(const char *fmt, ...)
{
    va_list args;

    va_start(args, fmt);
    Vprintf(fmt, args);
    va_end(args);
}

void Dmesg::PrintString(const char *s)
{
    Printf("%s", s);
}

void Dmesg::Dump(Dumper& dumper)
{
    Shared::AutoLock lock(Lock);
    CurrentDumper = &dumper;
    MsgBuf.Dump(MsgBufDumper);
    CurrentDumper = nullptr;
}

}