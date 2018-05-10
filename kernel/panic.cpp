#include "panic.h"
#include "preempt.h"
#include "asm.h"
#include "cpu.h"
#include "parameters.h"
#include "trace.h"
#include "stack_trace.h"

#include <drivers/vga.h>

namespace Kernel
{

Panicker::Panicker()
    : FrameCount(0)
{
    Message[0] = '\0';
    Trace(0, "Panicker 0x%p", this);
}

Panicker::~Panicker()
{
}

bool Panicker::IsActive()
{
    return (Active.Get() != 0) ? true : false;
}

void Panicker::DoPanic(const char *fmt, ...)
{
    InterruptDisable();

    if (Active.Cmpxchg(1, 0) == 0)
    {
        va_list args;

        va_start(args, fmt);
        Stdlib::VsnPrintf(Message, sizeof(Message), fmt, args);
        va_end(args);
        FrameCount = StackTrace::Capture(4096, Frame, Stdlib::ArraySize(Frame));

        auto& cpus = CpuTable::GetInstance();
        if (cpus.Ready()) {
            Cpu& cpu = CpuTable::GetInstance().GetCurrentCpu();
            cpus.SendIPIAllExclude(cpu.GetIndex());
        }
    }

    for (;;)
    {
        Pause();
    }
}

}
