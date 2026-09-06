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
       the part, and the cycle counter is the one thing the two core types of
       a hybrid CPU do agree about. */
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
    record.Depth = 1 + StackTrace::CaptureFromSp(ctx->GetFramePointer(),
        ctx->GetOrigRsp(), &record.Frame[1], MaxDepth - 1);

    cpu.Count++;
}

static const char* SymbolName(const char* name)
{
    return (name != nullptr) ? name : "(unknown)";
}

void Profiler::Report(Stdlib::Printer& printer, ulong pidFilter, ulong chainLimit)
{
    /* Whole chains, not leaves. Every sample already carries MaxDepth frames;
       counting them by leaf alone threw away all but the first two. */
    struct Chain
    {
        ulong Depth;
        ulong Count;

        /* The leaf folds by symbol, the callers by exact address.
 
           A sample can land on any instruction of the function it
           interrupted, so folding the leaf by address would shatter one hot
           function into a chain per instruction; it folds by name instead
           and carries the span of offsets seen, which says whether the
           samples sat on one instruction or spread across the body.
 
           A return address is not like that. Each call site has exactly one,
           so folding the callers by address does not shatter anything -- it
           separates paths that really are different, and keeps the offset
           that names which call in the function this came through. */
        const char* LeafName;
        ulong LeafLow;
        ulong LeafHigh;

        ulong Frame[MaxDepth];
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
               as well. Only the leaf is resolved during the walk -- the
               callers fold as raw addresses and are named at print time,
               for the few chains that actually get printed. */
            const char* leafName;
            ulong leafOffset;
            if (!symtab.Resolve(record.Frame[0], leafName, leafOffset))
            {
                leafName = nullptr;
                leafOffset = 0;
            }

            ulong c = 0;
            for (; c < chainCount; c++)
            {
                if (chains[c].Depth != depth)
                    continue;

                /* Resolve hands back a pointer into the static table, so the
                   same symbol is the same pointer. */
                if (chains[c].LeafName != leafName)
                    continue;

                ulong f = 1;
                for (; f < depth; f++)
                {
                    if (chains[c].Frame[f] != record.Frame[f])
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
                chains[c].LeafName = leafName;
                for (ulong f = 1; f < depth; f++)
                    chains[c].Frame[f] = record.Frame[f];
                chains[c].Count = 0;
                chains[c].LeafLow = leafOffset;
                chains[c].LeafHigh = leafOffset;
                chainCount++;
            }

            if (leafOffset < chains[c].LeafLow)
                chains[c].LeafLow = leafOffset;
            if (leafOffset > chains[c].LeafHigh)
                chains[c].LeafHigh = leafOffset;

            chains[c].Count++;
        }
    }

    /* Which clock these came off matters for reading them: at 100 Hz a
       function has to be very hot to appear at all, and nothing that runs
       with interrupts off appears ever. */
    printer.Printf("%u samples on %u cpus, %u dropped, source %s\n",
        total, cpus, dropped, UsePmu ? Hal::PmuName() : "tick");

    if (total == 0)
    {
        if (pidFilter != NoPidFilter)
            printer.Printf("(no sample landed in pid %u -- it may have been "
                "asleep the whole window, which is what a task that is not "
                "running looks like)\n", pidFilter);
        else if (UsePmu)
            /* The counter counts unhalted cycles, so a machine that spent the
               window in `hlt` produces nothing at all -- where the tick would
               have shown a stack full of idle. */
            printer.Printf("(nothing sampled -- every cpu was halted for the "
                "whole window, or the window was too short)\n");
        else
            printer.Printf("(nothing sampled -- was the window too short?)\n");

        Mm::Free(chains);
        return;
    }

    if (unfolded != 0)
        printer.Printf("%u samples past the %u chains this report holds\n",
            unfolded, (ulong)MaxChains);

    for (ulong i = 0; i < chainCount && i < chainLimit; i++)
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

        if (chains[i].LeafLow == chains[i].LeafHigh)
            printer.Printf("%u.%u%% %u %s+0x%p\n", permille / 10, permille % 10,
                chains[i].Count, SymbolName(chains[i].LeafName),
                chains[i].LeafLow);
        else
            printer.Printf("%u.%u%% %u %s+0x%p..0x%p\n",
                permille / 10, permille % 10, chains[i].Count,
                SymbolName(chains[i].LeafName),
                chains[i].LeafLow, chains[i].LeafHigh);

        /* The offset is into the caller and points just past the call that
           led here -- these are return addresses, the same ones the panic
           backtrace prints, so the two read alike. */
        for (ulong f = 1; f < chains[i].Depth; f++)
        {
            const char* name;
            ulong offset;
            if (symtab.Resolve(chains[i].Frame[f], name, offset))
                printer.Printf("        <- %s+0x%p\n", name, offset);
            else
                printer.Printf("        <- 0x%p\n", chains[i].Frame[f]);
        }
    }

    Mm::Free(chains);
}

}
