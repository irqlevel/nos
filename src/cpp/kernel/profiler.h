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
 
   Samples are taken on the per-CPU tick, so the rate is the tick rate --
   100 Hz per CPU, which finds a hot function but will not show fine
   structure. Sampling at kilohertz needs the performance counters and an
   NMI, which is a separate piece of work; this one costs no new interrupt.
 
   Each CPU writes only its own buffer, from its own tick, with interrupts
   already off -- so there is no lock here and nothing to contend for. */
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

    /* Called from the per-CPU tick, interrupts already off. */
    void Sample(Context* ctx);

    bool Start();
    void Stop();

    /* Report every task, rather than one. */
    static const ulong NoPidFilter = (ulong)-1;

    void Report(Stdlib::Printer& printer, ulong pidFilter);

private:
    Profiler();
    ~Profiler();
    Profiler(const Profiler& other) = delete;
    Profiler& operator=(const Profiler& other) = delete;

    /* 2.5 seconds of headroom per CPU at the tick rate. */
    static const ulong SamplesPerCpu = 256;

    /* How many distinct symbols a report can name, and how many it prints. */
    static const ulong MaxSymbols = 128;
    static const ulong TopSymbols = 12;
    static const ulong TopCallers = 3;

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
    };

    bool Allocate();
    void ReportCallers(Stdlib::Printer& printer, const char* leaf, ulong pidFilter);

    volatile bool Active;
    bool Allocated;

    PerCpu Cpu_[MaxCpus];

    static const ulong Tag = 'Prof';
};

}
