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

/* The tick: every CPU's, 100 times a second on both architectures. The
   round robin, the watchdog, the profiler and the software timers run on
   it; a task asleep until a deadline does not wait for it (Cpu::LocalTimer*). */
const ulong TickHz = 100;

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

    /* True once Park() has stopped this CPU for good. Written once by the
       parking CPU and read by whichever one panics, without the lock. */
    bool IsParked();

    void IPI(Context* ctx);
    void TimerTick(Context* ctx);

    /* This CPU's local timer, one-shot: armed for the earlier of its next
       tick, on a TickHz grid, and the soonest deadline of a task asleep on
       it -- so a Sleep() or an Event::WaitFor() ends at its deadline, and
       not at the next tick after it. Each is called on this CPU only, with
       interrupts off.

       LocalTimerStart: the hardware set up one-shot by the arch, the grid
       begins and the first tick is armed. LocalTimerFired: from the
       timer's interrupt, whether the tick is due -- the grid moved on if so
       -- with the timer armed again for the next one; the soonest deadline,
       if it is sooner, is the scheduler's to bring forward
       (LocalTimerTakeWalk, TaskQueue::Schedule). LocalTimerWake: the timer
       brought forward to a deadline sooner than the one it has; nothing for
       0, for a later one, or where the timer is not one-shot.
       LocalTimerTick: the next tick armed and nothing else, for a CPU whose
       queue may not be looked at -- a panic's. */
    void LocalTimerStart();
    bool LocalTimerOneShot() const { return TimerOneShot; }
    bool LocalTimerFired(ulong now);
    bool LocalTimerTakeWalk();
    void LocalTimerWake(ulong deadline);
    void LocalTimerTick();

    static const ulong StateInited = 0x1;
    static const ulong StateRunning = 0x2;
    static const ulong StateExiting = 0x4;
    static const ulong StateExited = 0x8;

    bool Run(Task::Func func, void *ctx);

    /* The task Run() made for this CPU: the BSP's runs BpStartup and then
       the shutdown watch, an AP's its idle loop */
    Task* GetIdleTask() { return IdleTaskPtr; }

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
    void __attribute__((noreturn)) Park();
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

    /* Set by Park() before StateExited; see IsParked */
    Atomic Parked;

    void LocalTimerArm(ulong when, ulong now);

    /* The local timer (LocalTimer*): one-shot here, the next tick's due
       time, when the timer is armed to fire (~0 when it is not), and that
       it fired since the scheduler last looked for the soonest deadline.
       This CPU's alone, with interrupts off. */
    bool TimerOneShot;
    ulong TickDue;
    ulong TimerArmedAt;
    bool TimerWalk;

    /* Ticks are a period apart; an interrupt this close before the next
       is that tick, rather than one more interrupt a moment later. */
    static constexpr ulong TickPeriodNs = Const::NanoSecsInSec / TickHz;
    static constexpr ulong TickSlackNs = 20 * Const::NanoSecsInUsec;
    /* The nearest the timer is armed: one already due fires at once. */
    static constexpr ulong TimerMinNs = Const::NanoSecsInUsec;

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