#pragma once

#include "stdlib.h"
#include "spin_lock.h"
#include "raw_spin_lock.h"
#include "wait_group.h"
#include "task.h"
#include "sched.h"
#include <hal/cpu.h>
#include <hal/irqchip.h>
#include <hal/context.h>

namespace Kernel
{

/* Upper bound on the CPUs the kernel tracks.  64 is a ceiling rather than a
   tuning knob: CPU sets travel as a ulong bitmask (GetRunningCpus,
   Task::CpuAffinity, SoftIrq), so index 63 is the last one that fits.

   On x86 the index *is* the LAPIC ID, not a dense counter, so this bound has
   to clear the largest APIC ID the firmware reports rather than the CPU
   count: a 14-core Raptor Lake numbers its 20 threads 0,1,8,9,...,62, and at
   MaxCpus = 8 everything above the first two threads was dropped.  Anything
   sized by this constant is per-CPU static storage (kernel/main.cpp Stack[],
   gdt.h DfStack[], arm64 ApBootStack[]), so raising it further costs BSS in
   proportion. */
const ulong MaxCpus = 64;

struct IPITask
{
    using Func = void (*)(void* ctx, Context* ipiCtx);

    IPITask();
    IPITask(Func func, void* ctx);

    Func Function;
    void* Ctx;
    WaitGroup Completion;
    Stdlib::ListEntry ListEntry;
};

class Cpu final
{
public:
    Cpu();
    ~Cpu();

    void Init(ulong index);

    void SetRunning();
    void SetExiting();

    ulong GetIndex();

    void Idle();

    ulong GetState();

    void IPI(Context* ctx);
    void TimerTick(Context* ctx);

    static const ulong StateInited = 0x1;
    static const ulong StateRunning = 0x2;
    static const ulong StateExiting = 0x4;
    static const ulong StateExited = 0x8;

    bool Run(Task::Func func, void *ctx);

    void SendIPISelf();

    void QueueIPITask(IPITask& task);
    void QueueIPITaskAsync(IPITask& task);

    TaskQueue& GetTaskQueue();

    void Reset();

private:
    Cpu(const Cpu& other) = delete;
    Cpu(Cpu&& other) = delete;
    Cpu& operator=(const Cpu& other) = delete;
    Cpu& operator=(Cpu&& other) = delete;

    void OnPanic();
    void ProcessIPITasks(Context* ctx);
    void DrainAndCloseIPITasks(Context* ctx);

    ulong Index;
    ulong State;
    SpinLock Lock;
    Task* IdleTaskPtr;
    TaskQueue TaskQueue;
    Atomic IPIConter;

    RawSpinLock IPITaskLock;
    Stdlib::ListEntry IPITaskList;
    bool IPITasksClosed; /* set on exit: queuers self-complete instead of waiting */

    static const ulong Tag = 'Cpu ';
};

class CpuTable final
{
public:
    static CpuTable& GetInstance()
    {
        static CpuTable Instance;
        return Instance;
    }

    bool InsertCpu(ulong index);

    Cpu& GetCpu(ulong index);

    bool StartAll();

    ulong GetBspIndex();
    bool SetBspIndex(ulong index);

    /* Lock-free BSP index for IRQ context (e.g. the tick IPI) */
    ulong GetBspIndexNoLock();

    ulong GetCurrentCpuId();

    Cpu& GetCurrentCpu();

    static const u8 IPIVector = Hal::IpiVector;

    /* The local APIC timer's own vector, below the IPI's. */
    static const u8 TimerVector = 0xFD;

    /* True once every CPU is ticking off its own local timer; until then the
       tick arrives as a broadcast IPI from whichever CPU owns the HPET. */
    bool HasPerCpuTimer();
    void SetPerCpuTimer();

    void SendIPI(ulong index);

    ulong GetRunningCpus();

    /* Mirror of the above, readable without taking Lock. The watchdog reads
       it from every CPU's tick, and taking the table lock there would be both
       the contention the reader exists to remove and a spinlock acquired
       from an interrupt with interrupts off -- the shape that wedges
       machines. Published by NoteCpuRunning as each CPU comes up, and
       refreshed by every locked call above. Nothing clears StateRunning
       today, so nothing takes a bit back out. */
    ulong GetRunningCpusNoLock();

    void NoteCpuRunning(ulong index);

    void ExitAllExceptSelf();

    void SendIPIAllExclude(ulong excludeIndex);

    void SendIPIAll();

    void InvalidateTlbAll();
    void InvalidateTlbAddress(ulong virtAddr);
    void InvalidateTlbRange(ulong virtAddr, ulong count);

    void Reset();

private:
    CpuTable();
    ~CpuTable();
    CpuTable(const CpuTable& other) = delete;
    CpuTable(CpuTable&& other) = delete;
    CpuTable& operator=(const CpuTable& other) = delete;
    CpuTable& operator=(CpuTable&& other) = delete;

    ulong GetBspIndexLockHeld();
    ulong GetRemoteCpuMask();
    void SendTlbIPI(ulong cpuMask, IPITask tasks[MaxCpus]);

    SpinLock Lock;
    Cpu CpuArray[MaxCpus];

    ulong BspIndex;
    /* Mirror of BspIndex readable without taking Lock */
    Atomic BspIndexCached;

    /* Mirror of the running-CPU set, read on the tick path */
    Atomic RunningMaskCached;


    /* Read on the tick path from every CPU; written once, at boot. */
    volatile bool PerCpuTimer;
};

static inline Cpu& GetCpu()
{
    return CpuTable::GetInstance().GetCurrentCpu();
}

}