#pragma once

#include <lib/stdlib.h>
#include <lib/printer.h>
#include <hal/context.h>

#include "cpu.h"

namespace Kernel
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
    static Profiler& GetInstance()
    {
        static Profiler Instance;
        return Instance;
    }

    static const ulong MaxDepth = 8;

    /* Cheap enough to call from every tick: one read of a flag. */
    bool IsActive() { return Active; }

    /* Called from the per-CPU tick, interrupts already off. Arms and disarms
       this CPU's performance counter, and takes the sample itself when there
       is no counter to do it. */
    void Tick(Context* ctx);

    /* Called from the NMI handler once the counter overflow has been
       acknowledged as this profiler's. */
    void Sample(Context* ctx);

    bool Start();
    void Stop();

    /* Report every task, rather than one. */
    static const ulong NoPidFilter = (ulong)-1;

    /* `chains` caps how many are printed. The default is TopChains; a
       smaller number is what makes the report readable on a console with no
       scrollback, where the hottest chain is the one that scrolls away. */
    void Report(Stdlib::Printer& printer, ulong pidFilter, ulong chains = TopChains);

private:
    Profiler();
    ~Profiler();
    Profiler(const Profiler& other) = delete;
    Profiler& operator=(const Profiler& other) = delete;

    /* One second per CPU at the counter rate, ten at the tick rate. Buffers
       are allocated for the CPUs that are actually running, so the cost is
       the machine's real width rather than MaxCpus. */
    static const ulong SamplesPerCpu = 1024;

    /* How many distinct call chains a report can hold, and how many it
       prints. A chain that does not fit is counted, not silently lost. */
    static const ulong MaxChains = 128;

public:
    static const ulong TopChains = 8;
private:

    struct Record
    {
        ulong Pid;
        ulong Depth;
        ulong Frame[MaxDepth];
    };

    struct PerCpu
    {
        Record* Records;
        ulong Count;
        ulong Dropped;

        /* Written only by its own CPU, from its own tick. */
        bool PmuArmed;
    };

    bool Allocate();

    volatile bool Active;

    /* Set at Start from CPUID; read by every tick. */
    volatile bool UsePmu;

    bool Allocated;

    PerCpu Cpu_[MaxCpus];

    static const ulong Tag = 'Prof';
};

}
