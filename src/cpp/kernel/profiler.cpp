#include "profiler.h"
#include "stack_trace.h"
#include "symtab.h"
#include "task.h"
#include "trace.h"

#include <hal/irqchip.h>
#include <hal/pmu.h>
#include <mm/new.h>

namespace Kernel
{

Profiler::Profiler()
    : Active(false)
    , UsePmu(false)
    , Allocated(false)
{
    for (ulong i = 0; i < MaxCpus; i++)
    {
        Cpu_[i].Records = nullptr;
        Cpu_[i].Count = 0;
        Cpu_[i].Dropped = 0;
        Cpu_[i].PmuArmed = false;
    }
}

Profiler::~Profiler()
{
}

bool Profiler::Allocate()
{
    if (Allocated)
        return true;

    /* Only the CPUs that are up: a buffer for every slot MaxCpus allows
       would be megabytes held for the life of the kernel to serve cores this
       machine does not have. */
    ulong running = CpuTable::GetInstance().GetRunningCpus();

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (!(running & (1UL << i)))
            continue;

        Cpu_[i].Records = (Record*)Mm::Alloc(SamplesPerCpu * sizeof(Record), Tag);
        if (Cpu_[i].Records == nullptr)
        {
            Trace(0, "Profiler: no memory for cpu %u buffer", i);
            return false;
        }
    }

    /* Kept for the life of the kernel rather than freed on Stop. A CPU can
       be inside Sample() having already passed the Active check when another
       CPU stops the run, and freeing underneath it would be a use-after-free
       for the sake of reclaiming a megabyte. */
    Allocated = true;
    return true;
}

bool Profiler::Start()
{
    if (Active)
        return false;

    if (!Allocate())
        return false;

    for (ulong i = 0; i < MaxCpus; i++)
    {
        Cpu_[i].Count = 0;
        Cpu_[i].Dropped = 0;
    }

    /* Asked once, on the CPU running the shell: the answer is a property of
       the part, and the fixed counters are the one thing the two core types
       of a hybrid CPU do agree about. */
    UsePmu = Hal::PmuAvailable();

    Active = true;
    return true;
}

void Profiler::Stop()
{
    Active = false;

    /* Each CPU disarms its own counter on its next tick -- the control
       registers are per CPU and cannot be written from here. Until then it
       keeps overflowing, and Sample() drops what arrives because Active is
       already false. */
}

void Profiler::Tick(Context* ctx)
{
    ulong index = Hal::GetCurrentCpuHwId();
    if (index >= MaxCpus)
        return;

    PerCpu& cpu = Cpu_[index];

    if (!Active)
    {
        if (cpu.PmuArmed)
        {
            Hal::PmuStop();
            cpu.PmuArmed = false;
        }
        return;
    }

    if (UsePmu)
    {
        if (!cpu.PmuArmed)
            cpu.PmuArmed = Hal::PmuStart();

        /* Armed: the samples arrive as counter overflows, and taking one here
           as well would count this tick twice. */
        if (cpu.PmuArmed)
            return;
    }

    Sample(ctx);
}

void Profiler::Sample(Context* ctx)
{
    if (!Active)
        return;

    ulong index = Hal::GetCurrentCpuHwId();
    if (index >= MaxCpus)
        return;

    PerCpu& cpu = Cpu_[index];
    if (cpu.Records == nullptr)
        return;

    if (cpu.Count >= SamplesPerCpu)
    {
        cpu.Dropped++;
        return;
    }

    Record& record = cpu.Records[cpu.Count];

    Task* task = Task::TryGetCurrentTask();
    record.Pid = (task != nullptr) ? task->Pid : 0;

    /* The interrupted instruction, exactly, from the frame the tick pushed --
       not a walk from in here, which would have to guess how many of its own
       frames to skip. */
    record.Frame[0] = ctx->GetRetRip();
    record.Depth = 1 + StackTrace::CaptureFrom(ctx->GetFramePointer(),
        &record.Frame[1], MaxDepth - 1);

    cpu.Count++;
}

void Profiler::Report(Stdlib::Printer& printer, ulong pidFilter)
{
    struct Bucket
    {
        const char* Name;
        ulong Addr;
        ulong Count;
    };

    Bucket buckets[MaxSymbols];
    ulong bucketCount = 0;

    auto& symtab = SymbolTable::GetInstance();

    ulong total = 0;
    ulong dropped = 0;
    ulong cpus = 0;

    /* Resolve here rather than at sample time: a symbol lookup has no place
       in an interrupt handler, and the addresses keep just as well. */
    for (ulong i = 0; i < MaxCpus; i++)
    {
        PerCpu& cpu = Cpu_[i];
        if (cpu.Records == nullptr)
            continue;

        if (cpu.Count != 0)
            cpus++;

        dropped += cpu.Dropped;

        for (ulong s = 0; s < cpu.Count; s++)
        {
            Record& record = cpu.Records[s];
            if (pidFilter != NoPidFilter && record.Pid != pidFilter)
                continue;

            total++;

            const char* name;
            ulong offset;
            if (!symtab.Resolve(record.Frame[0], name, offset))
                name = nullptr;

            ulong b = 0;
            for (; b < bucketCount; b++)
            {
                /* Resolve hands back a pointer into the static table, so the
                   same symbol is the same pointer. */
                if (buckets[b].Name == name)
                    break;
            }

            if (b == bucketCount)
            {
                if (bucketCount == MaxSymbols)
                    continue;

                buckets[bucketCount].Name = name;
                buckets[bucketCount].Addr = record.Frame[0];
                buckets[bucketCount].Count = 0;
                bucketCount++;
            }

            buckets[b].Count++;
        }
    }

    /* Which clock these came off matters for reading them: at 100 Hz a
       function has to be very hot to appear at all, and nothing that runs
       with interrupts off appears ever. */
    printer.Printf("%u samples on %u cpus, %u dropped, source %s\n",
        total, cpus, dropped, UsePmu ? "pmu/nmi" : "tick");

    if (total == 0)
    {
        if (pidFilter != NoPidFilter)
            printer.Printf("(no sample landed in pid %u -- it may have been "
                "asleep the whole window, which is what a task that is not "
                "running looks like)\n", pidFilter);
        else
            printer.Printf("(nothing sampled -- was the window too short?)\n");
        return;
    }

    for (ulong i = 0; i < bucketCount && i < TopSymbols; i++)
    {
        ulong best = i;
        for (ulong j = i + 1; j < bucketCount; j++)
        {
            if (buckets[j].Count > buckets[best].Count)
                best = j;
        }

        if (best != i)
        {
            Bucket tmp = buckets[i];
            buckets[i] = buckets[best];
            buckets[best] = tmp;
        }

        ulong permille = (buckets[i].Count * 1000) / total;
        printer.Printf("%u.%u%% %u %s\n", permille / 10, permille % 10,
            buckets[i].Count,
            (buckets[i].Name != nullptr) ? buckets[i].Name : "(unknown)");

        ReportCallers(printer, buckets[i].Name, pidFilter);
    }
}

void Profiler::ReportCallers(Stdlib::Printer& printer, const char* leaf,
    ulong pidFilter)
{
    struct Caller
    {
        const char* Name;
        ulong Count;
    };

    Caller callers[TopCallers];
    ulong callerCount = 0;

    auto& symtab = SymbolTable::GetInstance();

    for (ulong i = 0; i < MaxCpus; i++)
    {
        PerCpu& cpu = Cpu_[i];
        if (cpu.Records == nullptr)
            continue;

        for (ulong s = 0; s < cpu.Count; s++)
        {
            Record& record = cpu.Records[s];
            if (pidFilter != NoPidFilter && record.Pid != pidFilter)
                continue;

            if (record.Depth < 2)
                continue;

            const char* name;
            ulong offset;
            if (!symtab.Resolve(record.Frame[0], name, offset))
                name = nullptr;

            if (name != leaf)
                continue;

            const char* caller;
            if (!symtab.Resolve(record.Frame[1], caller, offset))
                continue;

            ulong c = 0;
            for (; c < callerCount; c++)
            {
                if (callers[c].Name == caller)
                    break;
            }

            if (c == callerCount)
            {
                /* Only the few most common matter here; a caller that never
                   makes the first three is noise in a line of output. */
                if (callerCount == TopCallers)
                    continue;

                callers[callerCount].Name = caller;
                callers[callerCount].Count = 0;
                callerCount++;
            }

            callers[c].Count++;
        }
    }

    for (ulong c = 0; c < callerCount; c++)
        printer.Printf("        <- %u %s\n", callers[c].Count, callers[c].Name);
}

}
