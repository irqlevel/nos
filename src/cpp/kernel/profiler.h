#pragma once

#include <lib/stdlib.h>
#include <lib/printer.h>
#include <hal/context.h>

#include "cpu.h"

namespace kernel
{

/* Sampling profiler: where the kernel actually spends its time, asked of a
   running machine over the shell.
 
   The expensive parts of a profiler are already in this kernel and are
   reused rather than rebuilt: SymbolTable::Resolve turns an address into a
   name from the table the two-pass link bakes into the image, and
   StackTrace::CaptureFrom walks the frame-pointer chain, which is intact
   because the whole kernel is built with -fno-omit-frame-pointer. What is
   left is to take a sample on each tick and count them up.
 
   Samples come from one of two places. Where the hardware has a usable
   performance counter -- Intel's fixed counter 1 or AMD's PMCx076, both
   counting unhalted core cycles -- it is set to overflow into an NMI about
   every millisecond: ten times the tick rate, and -- because it arrives as
   an NMI -- it also samples code running with interrupts disabled, which a
   tick by construction never catches. Everywhere else the tick itself takes
   the sample, at 100 Hz per CPU: enough to find a hot function, not enough
   to show its shape. Report() names the counter the numbers came off.

   The two differ in one way worth knowing before reading a report: the
   counter counts unhalted cycles, so an idle CPU contributes nothing at all,
   where the tick would have filled the profile with `Hlt`.
 
   Each CPU writes only its own buffer, from its own interrupt, with
   interrupts already off -- so there is no lock here and nothing to contend
   for. An NMI cannot be nested by another NMI on the same CPU, so that
   holds for the counter path too. */
class Profiler final
{
public:
    static Profiler& get_instance()
    {
        static Profiler instance;
        return instance;
    }

    static const ulong MAX_DEPTH = 8;

    /* Cheap enough to call from every tick: one read of a flag. */
    bool is_active() { return active_; }

    /* Called from the per-CPU tick, interrupts already off. Arms and disarms
       this CPU's performance counter, and takes the sample itself when there
       is no counter to do it. */
    void tick(Context* ctx);

    /* Called from the NMI handler once the counter overflow has been
       acknowledged as this profiler's. */
    void sample(Context* ctx);

    bool start();
    void stop();

    /* Report every task, rather than one. */
    static const ulong NO_PID_FILTER = (ulong)-1;

    /* `chains` caps how many are printed. The default is TopChains; a
       smaller number is what makes the report readable on a console with no
       scrollback, where the hottest chain is the one that scrolls away. */
    void report(stdlib::Printer& printer, ulong pid_filter, ulong chains = TOP_CHAINS);

private:
    Profiler();
    ~Profiler();
    Profiler(const Profiler& other) = delete;
    Profiler& operator=(const Profiler& other) = delete;

    /* One second per CPU at the counter rate, ten at the tick rate. Buffers
       are allocated for the CPUs that are actually running, so the cost is
       the machine's real width rather than MaxCpus. */
    static const ulong SAMPLES_PER_CPU = 1024;

    /* How many distinct call chains a report can hold, and how many it
       prints. A chain that does not fit is counted, not silently lost. */
    static const ulong MAX_CHAINS = 128;

public:
    static const ulong TOP_CHAINS = 8;
private:

    struct Record
    {
        ulong pid;
        ulong depth;
        ulong frame[MAX_DEPTH];
    };

    struct PerCpu
    {
        Record* records;
        ulong count;
        ulong dropped;

        /* Written only by its own CPU, from its own tick. */
        bool pmu_armed;
    };

    bool allocate();

    volatile bool active_;

    /* Set at Start from CPUID; read by every tick. */
    volatile bool use_pmu_;

    bool allocated_;

    PerCpu cpu_[MAX_CPUS];

    static const ulong TAG = 'Prof';
};

}
