#include "panic.h"
#include "preempt.h"
#include "asm.h"
#include "cpu.h"
#include "parameters.h"
#include "trace.h"
#include "stack_trace.h"
#include "symtab.h"
#include "task.h"

#include <drivers/acpi.h>
#include <drivers/serial.h>
#include <drivers/vga.h>

namespace Kernel
{

Panicker::Panicker()
{
}

Panicker::~Panicker()
{
}

bool Panicker::IsActive()
{
    return (Active.Get() != 0) ? true : false;
}

void Panicker::PrintOutput(const char* str)
{
    Serial::GetInstance().PanicPrintString(str);
    VgaTerm::GetInstance().PanicPrintString(str);
}

void Panicker::DumpContext()
{
    char buf[128];

    /* CPU ID — safe if LAPIC is mapped */
    if (Acpi::GetInstance().GetLapicAddress() != nullptr)
    {
        ulong cpuId = CpuTable::GetInstance().GetCurrentCpuId();
        Stdlib::SnPrintf(buf, sizeof(buf), "CPU: %u\n", cpuId);
        PrintOutput(buf);
    }

    /* Task — use TryGetCurrentTask to avoid BugOn/recursive panic */
    Task* task = Task::TryGetCurrentTask();
    if (task != nullptr)
    {
        Stdlib::SnPrintf(buf, sizeof(buf), "Task: pid %u name %s\n",
            task->Pid, task->GetName());
        PrintOutput(buf);
    }
}

void Panicker::DumpBacktrace(ulong* frames, size_t count)
{
    char buf[128];
    auto& symtab = SymbolTable::GetInstance();

    PrintOutput("Backtrace:\n");
    for (size_t i = 0; i < count; i++)
    {
        const char* name;
        ulong offset;
        if (symtab.Resolve(frames[i], name, offset))
            Stdlib::SnPrintf(buf, sizeof(buf), "  [%u] 0x%p %s+0x%p\n",
                (ulong)i, frames[i], name, offset);
        else
            Stdlib::SnPrintf(buf, sizeof(buf), "  [%u] 0x%p\n",
                (ulong)i, frames[i]);
        PrintOutput(buf);
    }
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

        PrintOutput(Message);
        DumpContext();

        ulong frames[16];
        size_t count = StackTrace::Capture(frames, Stdlib::ArraySize(frames));
        DumpBacktrace(frames, count);

        if (Acpi::GetInstance().GetLapicAddress() != nullptr)
        {
            Cpu& cpu = CpuTable::GetInstance().GetCurrentCpu();
            CpuTable::GetInstance().SendIPIAllExclude(cpu.GetIndex());
        }
    }

    for (;;)
    {
        Pause();
    }
}

void Panicker::DoPanicCtx(Context* ctx, bool hasErrorCode, const char *fmt, ...)
{
    (void)hasErrorCode;
    InterruptDisable();
    if (Active.Cmpxchg(1, 0) == 0)
    {
        va_list args;

        va_start(args, fmt);
        Stdlib::VsnPrintf(Message, sizeof(Message), fmt, args);
        va_end(args);

        PrintOutput(Message);
        DumpContext();

        /* Walk backtrace from the faulting code's RBP */
        ulong frames[16];
        size_t count = StackTrace::CaptureFrom(ctx->Rbp, frames, Stdlib::ArraySize(frames));
        DumpBacktrace(frames, count);

        if (Acpi::GetInstance().GetLapicAddress() != nullptr)
        {
            Cpu& cpu = CpuTable::GetInstance().GetCurrentCpu();
            CpuTable::GetInstance().SendIPIAllExclude(cpu.GetIndex());
        }
    }

    for (;;)
    {
        Pause();
    }
}

}