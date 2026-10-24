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
#include <hal/power.h>


/* The disk log is Rust (src/rust/block/src/disklog.rs). */
extern "C" {
void rust_disklog_log(const char* line);
void rust_netconsole_log(const char* line, unsigned long len);
void rust_netconsole_panic_mark();
void rust_netconsole_panic_flush();
void rust_disklog_panic_flush();
}

namespace Kernel
{

Panicker::Panicker()
    : ProbeCpu(-1)
{
}

Panicker::~Panicker()
{
}

bool Panicker::IsActive()
{
    return (Active.Get() != 0) ? true : false;
}

/* After the report: on the screen, the serial port and -- flushed by the
   caller -- the netconsole and the disk log. Timed on the cycle counter,
   which takes no lock and wants no interrupt; before the counter is
   calibrated, at once. Only the CPU whose report it is: the others are
   halted. */
void Panicker::ResetIfAsked()
{
    ulong secs = Parameters::GetInstance().GetPanicResetSecs();
    if (secs == 0)
        return;

    char buf[96];
    Stdlib::SnPrintf(buf, sizeof(buf), "panic: resetting the machine in %lu s (panic=%lu)\n", secs, secs);
    PrintOutput(buf);

    ulong hz = Hal::CycleCounterHz();
    if (hz != 0)
    {
        u64 start = Hal::ReadCycleCounter();
        u64 wait = (u64)secs * hz;
        while (Hal::ReadCycleCounter() - start < wait)
            Pause();
    }
    Hal::ResetNow();
}

void Panicker::PrintOutput(const char* str)
{
    Hal::ConsolePanicWrite(str);

    /* Also into the netconsole ring; PanicFlush() below pushes it out while
       the machine still can. */
    rust_netconsole_log(str, Stdlib::StrLen(str));

    /* And into the disk log's, for its PanicFlush(): on a machine with no
       serial port and no network yet the disk is the only place the report
       can go. Log() takes no lock, which is what makes it safe here. */
    rust_disklog_log(str);
}

void Panicker::DumpContext()
{
    char buf[128];

    /* CPU ID — safe if LAPIC is mapped */
    if (Hal::IrqChipReady())
    {
        ulong cpuId = CpuTable::GetInstance().GetCurrentCpuId();
        Stdlib::SnPrintf(buf, sizeof(buf), "CPU: %lu\n", cpuId);
        PrintOutput(buf);
    }

    /* Task — use TryGetCurrentTask to avoid BugOn/recursive panic */
    Task* task = Task::TryGetCurrentTask();
    if (task != nullptr)
    {
        Stdlib::SnPrintf(buf, sizeof(buf), "Task: pid %lu name %s\n",
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
        if (symtab.DescribeReturn(frames[i], where, sizeof(where)))
            Stdlib::SnPrintf(buf, sizeof(buf), "  [%lu] 0x%lX %s\n",
                (ulong)i, frames[i], where);
        else
            Stdlib::SnPrintf(buf, sizeof(buf), "  [%lu] 0x%lX\n",
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

void Panicker::RecoverFault()
{
    long probe = ProbeCpu.Get();
    if (probe < 0 || Active.Get() == 0)
        return;

    /* Which CPU this is, only once it can be asked: before the interrupt
       controller is up the boot CPU is the only one, and on x86 the asking
       is a read of the local APIC, not mapped yet -- a fault of its own */
    if (Hal::IrqChipReady() && probe != (long)Hal::GetCurrentCpuHwId())
        return;

    ProbeCpu.Set(-1);
    LongJmp(&ProbeJump, 1);
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
           tell -- what it runs is the halt, on a stack of its own -- and the
           report says so in a line. */
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
            Stdlib::SnPrintf(buf, sizeof(buf), "Cpu %lu parked\n", i);
            PrintOutput(buf);
            continue;
        }

        if (!(asked & (1UL << i)))
            continue;

        if (RemoteDone[i].Get() != 2)
        {
            Stdlib::SnPrintf(buf, sizeof(buf), "Cpu %lu did not answer the NMI\n", i);
            PrintOutput(buf);
            continue;
        }

        Stdlib::SnPrintf(buf, sizeof(buf), "Cpu %lu backtrace:\n", i);
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
        rust_netconsole_panic_mark();

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
        rust_netconsole_panic_flush();
        rust_disklog_panic_flush();

        ResetIfAsked();
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
        rust_netconsole_panic_mark();

        va_list args;

        va_start(args, fmt);
        Stdlib::VsnPrintf(Message, sizeof(Message), fmt, args);
        va_end(args);

        PrintOutput(Message);
        DumpContext();

        /* Walk from the faulting code's RBP, over the stack its own RSP
           names: #DF arrives on a separate IST stack, and the report wanted
           is about the stack it came from. Which is the likeliest thing
           there to be broken -- a double fault is a fault the CPU could not
           push onto that stack -- so the walk reads it as memory that may
           not be there: a fault in it comes back here (RecoverFault), and
           the report goes on without it. It used to fault itself, a panic
           inside the panic, and stop: no other CPU's stack, no netconsole,
           no reset. */
        if (SetJmp(&ProbeJump) == 0)
        {
            ProbeCpu.Set(Hal::IrqChipReady() ? (long)Hal::GetCurrentCpuHwId() : 0);
            ulong frames[16];
            size_t count = StackTrace::CaptureFromSp(ctx->GetFramePointer(),
                ctx->GetOrigRsp(hasErrorCode), frames, Stdlib::ArraySize(frames));
            ProbeCpu.Set(-1);
            DumpBacktrace(frames, count);
        }
        else
        {
            /* Then at least where it was, which needs no stack */
            char where[SymbolTable::DescribeMax];
            char buf[SymbolTable::DescribeMax + 64];
            if (SymbolTable::GetInstance().Describe(ctx->GetRetRip(hasErrorCode), where, sizeof(where)))
                Stdlib::SnPrintf(buf, sizeof(buf),
                    "Backtrace: none, the stack it came from cannot be read; at %s\n", where);
            else
                Stdlib::SnPrintf(buf, sizeof(buf),
                    "Backtrace: none, the stack it came from cannot be read\n");
            PrintOutput(buf);
        }
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
        rust_netconsole_panic_flush();
        rust_disklog_panic_flush();

        ResetIfAsked();
    }

    for (;;)
    {
        Pause();
    }
}

}