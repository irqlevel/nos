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

static const char* SymbolName(const char* name)
{
    return (name != nullptr) ? name : "(unknown)";
}

void Profiler::Report(Stdlib::Printer& printer, ulong pidFilter)
{
    /* Whole chains, not leaves. Every sample already carries MaxDepth frames;
       counting them by leaf alone threw away all but the first two. */
    struct Chain
    {
        ulong Depth;
        const char* Name[MaxDepth];
        ulong Count;
    };

    /* On the heap rather than on the stack: Report runs in task context,
       where allocating is fine, and a table this size is a third of the
       headroom `stacks` measures on a task stack. */
    Chain* chains = (Chain*)Mm::Alloc(MaxChains * sizeof(Chain), Tag);
    if (chains == nullptr)
    {
        printer.Printf("profile: no memory for the chain table\n");
        return;
    }

    auto& symtab = SymbolTable::GetInstance();

    ulong chainCount = 0;
    ulong total = 0;
    ulong dropped = 0;
    ulong cpus = 0;
    ulong unfolded = 0;

    /* One pass over every sample. The old report walked them all again for
       each hot symbol it wanted callers for; this costs one walk in total. */
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

            ulong depth = record.Depth;
            if (depth > MaxDepth)
                depth = MaxDepth;

            /* Resolved here rather than at sample time: a symbol lookup has
               no place in an interrupt handler, and the addresses keep just
               as well. */
            const char* name[MaxDepth];
            for (ulong f = 0; f < depth; f++)
            {
                ulong offset;
                if (!symtab.Resolve(record.Frame[f], name[f], offset))
                    name[f] = nullptr;
            }

            ulong c = 0;
            for (; c < chainCount; c++)
            {
                if (chains[c].Depth != depth)
                    continue;

                ulong f = 0;
                /* Resolve hands back a pointer into the static table, so the
                   same symbol is the same pointer and the whole chain
                   compares as pointers. Two addresses in one function fold
                   together, which is the point. */
                for (; f < depth; f++)
                {
                    if (chains[c].Name[f] != name[f])
                        break;
                }

                if (f == depth)
                    break;
            }

            if (c == chainCount)
            {
                if (chainCount == MaxChains)
                {
                    unfolded++;
                    continue;
                }

                chains[c].Depth = depth;
                for (ulong f = 0; f < depth; f++)
                    chains[c].Name[f] = name[f];
                chains[c].Count = 0;
                chainCount++;
            }

            chains[c].Count++;
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

        Mm::Free(chains);
        return;
    }

    if (unfolded != 0)
        printer.Printf("%u samples past the %u chains this report holds\n",
            unfolded, (ulong)MaxChains);

    for (ulong i = 0; i < chainCount && i < TopChains; i++)
    {
        ulong best = i;
        for (ulong j = i + 1; j < chainCount; j++)
        {
            if (chains[j].Count > chains[best].Count)
                best = j;
        }

        if (best != i)
        {
            Chain tmp = chains[i];
            chains[i] = chains[best];
            chains[best] = tmp;
        }

        ulong permille = (chains[i].Count * 1000) / total;
        printer.Printf("%u.%u%% %u %s\n", permille / 10, permille % 10,
            chains[i].Count, SymbolName(chains[i].Name[0]));

        for (ulong f = 1; f < chains[i].Depth; f++)
            printer.Printf("        <- %s\n", SymbolName(chains[i].Name[f]));
    }

    Mm::Free(chains);
}

}
