#pragma once

#include <include/types.h>
#include "atomic.h"
#include "task.h"
#include "cpu.h"

namespace Kernel
{

class SoftIrq
{
public:
    static SoftIrq& GetInstance()
    {
        static SoftIrq instance;
        return instance;
    }

    /* Called during boot: creates a soft IRQ task per running CPU */
    bool Init();
    void Stop();

    /* Called from hard IRQ handler to schedule deferred work.
       The work runs on the CPU which raised it. */
    void Raise(ulong type);

    /* Register a handler for a soft IRQ type */
    void Register(ulong type, void (*handler)(void* ctx), void* ctx);

    static const ulong TypeNetRx = 0;
    static const ulong TypeBlkIo = 1;
    static const ulong TypeNetTx = 2;
    static const ulong TypeTcpTimer = 3;
    static const ulong MaxTypes = 8;

private:
    SoftIrq();
    ~SoftIrq();
    SoftIrq(const SoftIrq& other) = delete;
    SoftIrq(SoftIrq&& other) = delete;
    SoftIrq& operator=(const SoftIrq& other) = delete;
    SoftIrq& operator=(SoftIrq&& other) = delete;

    struct Handler
    {
        void (*Func)(void* ctx);
        void* Ctx;
    };

    struct CpuState
    {
        Atomic Pending; /* bitmask of pending soft IRQ types */
        Task* TaskPtr;
    };

    Handler Handlers[MaxTypes];
    CpuState CpuStates[MaxCpus];

    /* Handlers are not reentrant: a type runs on at most one CPU
       at a time. A CPU which loses the race keeps its pending bit
       set and retries. */
    Atomic Running; /* bitmask of soft IRQ types being handled */

    Atomic Ready; /* set once the per-CPU tasks are started */

    static void TaskFunc(void* ctx);
    void Run(CpuState& state);

    static const ulong Tag = 'SIrq';
};

}
