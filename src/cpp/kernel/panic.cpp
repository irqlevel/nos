#include "panic.h"
#include "preempt.h"
#include <hal/cpu.h>
#include <hal/context.h>
#include "cpu.h"
#include "parameters.h"
#include "trace.h"
#include "stack_trace.h"
#include "symtab.h"
#include "module.h"
#include "task.h"

#include <hal/irqchip.h>
#include <hal/barrier.h>
#include <hal/console.h>

#include <net/netconsole.h>
#include "disklog.h"

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
    Hal::ConsolePanicWrite(str);

    /* Also into the netconsole ring; PanicFlush() below pushes it out while
       the machine still can. */
    Netconsole::GetInstance().Log(str);

    /* And into the disk log's, for its PanicFlush(): on a machine with no
       serial port and no network yet the disk is the only place the report
       can go. Log() takes no lock, which is what makes it safe here. */
    DiskLog::GetInstance().Log(str);
}

void Panicker::DumpContext()
{
    char buf[128];

    /* CPU ID — safe if LAPIC is mapped */
    if (Hal::IrqChipReady())
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
    char where[SymbolTable::DescribeMax];
    char buf[SymbolTable::DescribeMax + 32];
    auto& symtab = SymbolTable::GetInstance();

    PrintOutput("Backtrace:\n");
    for (size_t i = 0; i < count; i++)
    {
        if (symtab.Describe(frames[i], where, sizeof(where)))
            Stdlib::SnPrintf(buf, sizeof(buf), "  [%u] 0x%p %s\n",
                (ulong)i, frames[i], where);
        else
            Stdlib::SnPrintf(buf, sizeof(buf), "  [%u] 0x%p\n",
                (ulong)i, frames[i]);
        PrintOutput(buf);
    }
}

/* Where each loaded module sits, so a frame nothing could name -- or one read
   off a log later -- can still be placed */
void Panicker::DumpModules()
{
    static const ulong LineMax = 256;
    char list[LineMax];
    char buf[LineMax + 16];

    if (!ModuleTable::GetInstance().DescribeAll(list, sizeof(list)))
        return;

    Stdlib::SnPrintf(buf, sizeof(buf), "Modules: %s\n", list);
    PrintOutput(buf);
}

bool Panicker::CollectRemoteStack()
{
    if (Collecting.Get() == 0)
        return false;

    ulong index = Hal::GetCurrentCpuHwId();
    if (index >= RemoteCpuCount)
        return false;

    if (RemoteDone[index].Cmpxchg(1, 0) != 0)
        return true; /* already recorded; still not our NMI to panic about */

    /* Walked from here: the frame pointer chain crosses the interrupt entry
       (the kernel is built with -fno-omit-frame-pointer), so this stack ends
       in whatever the CPU was doing when the NMI arrived -- which is the
       whole point of asking. */
    size_t count = StackTrace::Capture(RemoteFrame[index], RemoteFrameCount);
    RemoteCount[index] = (u8)count;

    Hal::SmpWmb();
    RemoteDone[index].Set(2);
    return true;
}

void Panicker::CollectRemoteStacks()
{
    static_assert(RemoteCpuCount == MaxCpus, "RemoteCpuCount is out of step");

    if (!Hal::IrqChipReady() || !Hal::NmiIpiSupported())
        return;

    auto& table = CpuTable::GetInstance();
    ulong self = table.GetCurrentCpu().GetIndex();
    ulong mask = table.GetRunningCpus();

    for (ulong i = 0; i < RemoteCpuCount; i++)
        RemoteDone[i].Set(0);

    Collecting.Set(1);

    ulong asked = 0;
    ulong parked = 0;
    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (!(mask & (1UL << i)) || i == self)
            continue;

        /* Stopped for good in the halt path (Cpu::Park): it has nothing to
           tell, and the stack it stopped on may be freed by now. Delivering
           an NMI pushes a frame onto that stack, and the fault that follows
           would bury this report under one of its own. */
        if (table.GetCpu(i).IsParked())
        {
            parked |= (1UL << i);
            continue;
        }

        asked |= (1UL << i);
        Hal::SendNmiIpi(i);
    }

    /* Bounded by iterations, not by the clock: a panic must not depend on a
       clock source that may be part of what broke. */
    static const ulong SpinBudget = 20000000;
    for (ulong spin = 0; spin < SpinBudget; spin++)
    {
        ulong done = 0;
        for (ulong i = 0; i < MaxCpus; i++)
        {
            if ((asked & (1UL << i)) && RemoteDone[i].Get() == 2)
                done |= (1UL << i);
        }

        if (done == asked)
            break;

        Pause();
    }

    Collecting.Set(0);

    char buf[64];
    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (parked & (1UL << i))
        {
            Stdlib::SnPrintf(buf, sizeof(buf), "Cpu %u parked\n", i);
            PrintOutput(buf);
            continue;
        }

        if (!(asked & (1UL << i)))
            continue;

        if (RemoteDone[i].Get() != 2)
        {
            Stdlib::SnPrintf(buf, sizeof(buf), "Cpu %u did not answer the NMI\n", i);
            PrintOutput(buf);
            continue;
        }

        Stdlib::SnPrintf(buf, sizeof(buf), "Cpu %u backtrace:\n", i);
        PrintOutput(buf);
        DumpBacktrace(RemoteFrame[i], RemoteCount[i]);
    }
}

void Panicker::DoPanic(const char *fmt, ...)
{
    InterruptDisable();
    if (Active.Cmpxchg(1, 0) == 0)
    {
        /* Before the first PrintOutput: the report is appended behind whatever
           backlog the drain task still owes the collector, and PanicFlush()
           needs to know how much of it to skip. */
        Netconsole::GetInstance().PanicMark();

        va_list args;

        va_start(args, fmt);
        Stdlib::VsnPrintf(Message, sizeof(Message), fmt, args);
        va_end(args);

        PrintOutput(Message);
        DumpContext();

        ulong frames[16];
        size_t count = StackTrace::Capture(frames, Stdlib::ArraySize(frames));
        DumpBacktrace(frames, count);
        DumpModules();

        /* Before the halting IPI: the CPU worth asking has interrupts off and
           would never take that one. */
        CollectRemoteStacks();

        if (Hal::IrqChipReady())
        {
            Cpu& cpu = CpuTable::GetInstance().GetCurrentCpu();
            CpuTable::GetInstance().SendIPIAllExclude(cpu.GetIndex());
        }

        /* Last thing done, and best-effort by construction: the console
           already has the whole report, so a TX path that turns out to be
           wedged costs nothing that was still needed. */
        Netconsole::GetInstance().PanicFlush();
        DiskLog::GetInstance().PanicFlush();
    }

    for (;;)
    {
        Pause();
    }
}

void Panicker::DoPanicCtx(Context* ctx, bool hasErrorCode, const char *fmt, ...)
{
    InterruptDisable();
    if (Active.Cmpxchg(1, 0) == 0)
    {
        /* Before the first PrintOutput: the report is appended behind whatever
           backlog the drain task still owes the collector, and PanicFlush()
           needs to know how much of it to skip. */
        Netconsole::GetInstance().PanicMark();

        va_list args;

        va_start(args, fmt);
        Stdlib::VsnPrintf(Message, sizeof(Message), fmt, args);
        va_end(args);

        PrintOutput(Message);
        DumpContext();

        /* Walk from the faulting code's RBP, over the stack its own RSP
           names: #DF arrives on a separate IST stack, and the report wanted
           is about the stack it came from. */
        ulong frames[16];
        size_t count = StackTrace::CaptureFromSp(ctx->GetFramePointer(),
            ctx->GetOrigRsp(hasErrorCode), frames, Stdlib::ArraySize(frames));
        DumpBacktrace(frames, count);
        DumpModules();

        CollectRemoteStacks();

        if (Hal::IrqChipReady())
        {
            Cpu& cpu = CpuTable::GetInstance().GetCurrentCpu();
            CpuTable::GetInstance().SendIPIAllExclude(cpu.GetIndex());
        }

        /* Last thing done, and best-effort by construction: the console
           already has the whole report, so a TX path that turns out to be
           wedged costs nothing that was still needed. */
        Netconsole::GetInstance().PanicFlush();
        DiskLog::GetInstance().PanicFlush();
    }

    for (;;)
    {
        Pause();
    }
}

}