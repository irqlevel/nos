#include "profiler.h"
#include "stack_trace.h"
#include "symtab.h"
#include "task.h"
#include "trace.h"

#include <hal/irqchip.h>
#include <hal/pmu.h>
#include <mm/new.h>

namespace kernel
{

Profiler::Profiler()
    : active_(false)
    , use_pmu_(false)
    , allocated_(false)
{
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        cpu_[i].records = nullptr;
        cpu_[i].count = 0;
        cpu_[i].dropped = 0;
        cpu_[i].pmu_armed = false;
    }
}

Profiler::~Profiler()
{
}

bool Profiler::allocate()
{
    if (allocated_)
        return true;

    /* Only the CPUs that are up: a buffer for every slot MaxCpus allows
       would be megabytes held for the life of the kernel to serve cores this
       machine does not have. */
    ulong running = CpuTable::get_instance().get_running_cpus();

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (!(running & (1UL << i)))
            continue;

        cpu_[i].records = (Record*)mm::alloc(SAMPLES_PER_CPU * sizeof(Record), TAG);
        if (cpu_[i].records == nullptr)
        {
            trace(0, "Profiler: no memory for cpu %lu buffer", i);
            return false;
        }
    }

    /* Kept for the life of the kernel rather than freed on Stop. A CPU can
       be inside Sample() having already passed the Active check when another
       CPU stops the run, and freeing underneath it would be a use-after-free
       for the sake of reclaiming a megabyte. */
    allocated_ = true;
    return true;
}

bool Profiler::start()
{
    if (active_)
        return false;

    if (!allocate())
        return false;

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        cpu_[i].count = 0;
        cpu_[i].dropped = 0;
    }

    /* Asked once, on the CPU running the shell: the answer is a property of
       the part, and the cycle counter is the one thing the two core types of
       a hybrid CPU do agree about. */
    use_pmu_ = hal::pmu_available();

    active_ = true;
    return true;
}

void Profiler::stop()
{
    active_ = false;

    /* Each CPU disarms its own counter on its next tick -- the control
       registers are per CPU and cannot be written from here. Until then it
       keeps overflowing, and Sample() drops what arrives because Active is
       already false. */
}

void Profiler::tick(Context* ctx)
{
    ulong index = hal::get_current_cpu_hw_id();
    if (index >= MAX_CPUS)
        return;

    PerCpu& cpu = cpu_[index];

    if (!active_)
    {
        if (cpu.pmu_armed)
        {
            hal::pmu_stop();
            cpu.pmu_armed = false;
        }
        return;
    }

    if (use_pmu_)
    {
        if (!cpu.pmu_armed)
            cpu.pmu_armed = hal::pmu_start();

        /* Armed: the samples arrive as counter overflows, and taking one here
           as well would count this tick twice. */
        if (cpu.pmu_armed)
            return;
    }

    sample(ctx);
}

void Profiler::sample(Context* ctx)
{
    if (!active_)
        return;

    ulong index = hal::get_current_cpu_hw_id();
    if (index >= MAX_CPUS)
        return;

    PerCpu& cpu = cpu_[index];
    if (cpu.records == nullptr)
        return;

    if (cpu.count >= SAMPLES_PER_CPU)
    {
        cpu.dropped++;
        return;
    }

    Record& record = cpu.records[cpu.count];

    Task* task = Task::try_get_current_task();
    record.pid = (task != nullptr) ? task->pid_ : 0;

    /* The interrupted instruction, exactly, from the frame the tick pushed --
       not a walk from in here, which would have to guess how many of its own
       frames to skip. */
    record.frame[0] = ctx->get_ret_rip();
    record.depth = 1 + StackTrace::capture_from_sp(ctx->get_frame_pointer(),
        ctx->get_orig_rsp(), &record.frame[1], MAX_DEPTH - 1);

    cpu.count++;
}

static const char* symbol_name(const char* name)
{
    return (name != nullptr) ? name : "(unknown)";
}

void Profiler::report(stdlib::Printer& printer, ulong pid_filter, ulong chain_limit)
{
    /* Whole chains, not leaves. Every sample already carries MaxDepth frames;
       counting them by leaf alone threw away all but the first two. */
    struct Chain
    {
        ulong depth;
        ulong count;

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
        const char* leaf_name;
        ulong leaf_low;
        ulong leaf_high;

        ulong frame[MAX_DEPTH];
    };

    /* On the heap rather than on the stack: Report runs in task context,
       where allocating is fine, and a table this size is a third of the
       headroom `stacks` measures on a task stack. */
    Chain* chains = (Chain*)mm::alloc(MAX_CHAINS * sizeof(Chain), TAG);
    if (chains == nullptr)
    {
        printer.printf("profile: no memory for the chain table\n");
        return;
    }

    auto& symtab = SymbolTable::get_instance();

    ulong chain_count = 0;
    ulong total = 0;
    ulong dropped = 0;
    ulong cpus = 0;
    ulong unfolded = 0;

    /* One pass over every sample. The old report walked them all again for
       each hot symbol it wanted callers for; this costs one walk in total. */
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        PerCpu& cpu = cpu_[i];
        if (cpu.records == nullptr)
            continue;

        if (cpu.count != 0)
            cpus++;

        dropped += cpu.dropped;

        for (ulong s = 0; s < cpu.count; s++)
        {
            Record& record = cpu.records[s];
            if (pid_filter != NO_PID_FILTER && record.pid != pid_filter)
                continue;

            total++;

            ulong depth = record.depth;
            if (depth > MAX_DEPTH)
                depth = MAX_DEPTH;

            /* Resolved here rather than at sample time: a symbol lookup has
               no place in an interrupt handler, and the addresses keep just
               as well. Only the leaf is resolved during the walk -- the
               callers fold as raw addresses and are named at print time,
               for the few chains that actually get printed. */
            const char* leaf_name;
            ulong leaf_offset;
            if (!symtab.resolve(record.frame[0], leaf_name, leaf_offset))
            {
                leaf_name = nullptr;
                leaf_offset = 0;
            }

            ulong c = 0;
            for (; c < chain_count; c++)
            {
                if (chains[c].depth != depth)
                    continue;

                /* Resolve hands back a pointer into the static table, so the
                   same symbol is the same pointer. */
                if (chains[c].leaf_name != leaf_name)
                    continue;

                /* No name to fold by -- a loadable module's code, or nobody's
                   -- and the address is all there is */
                if (leaf_name == nullptr && chains[c].frame[0] != record.frame[0])
                    continue;

                ulong f = 1;
                for (; f < depth; f++)
                {
                    if (chains[c].frame[f] != record.frame[f])
                        break;
                }

                if (f == depth)
                    break;
            }

            if (c == chain_count)
            {
                if (chain_count == MAX_CHAINS)
                {
                    unfolded++;
                    continue;
                }

                chains[c].depth = depth;
                chains[c].leaf_name = leaf_name;
                for (ulong f = 0; f < depth; f++)
                    chains[c].frame[f] = record.frame[f];
                chains[c].count = 0;
                chains[c].leaf_low = leaf_offset;
                chains[c].leaf_high = leaf_offset;
                chain_count++;
            }

            if (leaf_offset < chains[c].leaf_low)
                chains[c].leaf_low = leaf_offset;
            if (leaf_offset > chains[c].leaf_high)
                chains[c].leaf_high = leaf_offset;

            chains[c].count++;
        }
    }

    /* Which clock these came off matters for reading them: at 100 Hz a
       function has to be very hot to appear at all, and nothing that runs
       with interrupts off appears ever. */
    printer.printf("%lu samples on %lu cpus, %lu dropped, source %s\n",
        total, cpus, dropped, use_pmu_ ? hal::pmu_name() : "tick");

    if (total == 0)
    {
        if (pid_filter != NO_PID_FILTER)
            printer.printf("(no sample landed in pid %lu -- it may have been "
                "asleep the whole window, which is what a task that is not "
                "running looks like)\n", pid_filter);
        else if (use_pmu_)
            /* The counter counts unhalted cycles, so a machine that spent the
               window in `hlt` produces nothing at all -- where the tick would
               have shown a stack full of idle. */
            printer.printf("(nothing sampled -- every cpu was halted for the "
                "whole window, or the window was too short)\n");
        else
            printer.printf("(nothing sampled -- was the window too short?)\n");

        mm::free(chains);
        return;
    }

    if (unfolded != 0)
        printer.printf("%lu samples past the %lu chains this report holds\n",
            unfolded, (ulong)MAX_CHAINS);

    for (ulong i = 0; i < chain_count && i < chain_limit; i++)
    {
        ulong best = i;
        for (ulong j = i + 1; j < chain_count; j++)
        {
            if (chains[j].count > chains[best].count)
                best = j;
        }

        if (best != i)
        {
            Chain tmp = chains[i];
            chains[i] = chains[best];
            chains[best] = tmp;
        }

        ulong permille = (chains[i].count * 1000) / total;

        if (chains[i].leaf_name == nullptr)
        {
            /* Not the kernel's: named now, if a module still has it */
            char where[SymbolTable::DESCRIBE_MAX];
            if (!symtab.describe(chains[i].frame[0], where, sizeof(where)))
                stdlib::sn_printf(where, sizeof(where), "0x%lX", chains[i].frame[0]);
            printer.printf("%lu.%lu%% %lu %s\n", permille / 10, permille % 10,
                chains[i].count, where);
        }
        else if (chains[i].leaf_low == chains[i].leaf_high)
            printer.printf("%lu.%lu%% %lu %s+0x%lX\n", permille / 10, permille % 10,
                chains[i].count, symbol_name(chains[i].leaf_name),
                chains[i].leaf_low);
        else
            printer.printf("%lu.%lu%% %lu %s+0x%lX..0x%lX\n",
                permille / 10, permille % 10, chains[i].count,
                symbol_name(chains[i].leaf_name),
                chains[i].leaf_low, chains[i].leaf_high);

        /* The offset is into the caller and points just past the call that
           led here -- these are return addresses, the same ones the panic
           backtrace prints, so the two read alike. */
        for (ulong f = 1; f < chains[i].depth; f++)
        {
            char where[SymbolTable::DESCRIBE_MAX];
            if (symtab.describe_return(chains[i].frame[f], where, sizeof(where)))
                printer.printf("        <- %s\n", where);
            else
                printer.printf("        <- 0x%lX\n", chains[i].frame[f]);
        }
    }

    mm::free(chains);
}

}
