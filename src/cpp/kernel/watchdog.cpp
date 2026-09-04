#include "watchdog.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "time.h"
#include "panic.h"
#include "trace.h"
#include "cpu.h"

static_assert(Kernel::Watchdog::SliceCpus == Kernel::MaxCpus,
    "SliceCpus must track MaxCpus");

namespace Kernel
{

ulong Watchdog::ListLock::LockIrqSave()
{
    ulong flags = PreemptIrqSave();
    for (;;)
    {
        if (Value.Cmpxchg(1, 0) == 0)
            break;

        Pause();
    }
    return flags;
}

void Watchdog::ListLock::UnlockIrqRestore(ulong flags)
{
    Value.Set(0);
    PreemptIrqRestore(flags);
}

namespace
{

/* Kernighan: one iteration per set bit, so at most the CPU count. Called once
   per walk, not per lock. */
ulong PopCount(ulong value)
{
    ulong count = 0;
    while (value != 0)
    {
        value &= value - 1;
        count++;
    }
    return count;
}

}

Watchdog::Watchdog()
{
    for (ulong i = 0; i < SliceCpus; i++)
    {
        CpuSlice[i].First = 0;
        CpuSlice[i].Stride = 0;
    }
}

Watchdog::~Watchdog()
{
}

void Watchdog::Check()
{
    /* Same counter SpinLock::Lock stamps with. A lock taken on another CPU
       is timed against this CPU's counter: on a machine whose cycle
       counters do not run together that can over- or under-state the hold
       time, which only ever costs this trace a false or missed report. */
    u64 now = Hal::ReadCycleCounter();
    const ulong timeoutNs = 25 * Const::NanoSecsInMs;

    /* Past this the lock is not slow, it is stuck: nothing in this kernel
       legitimately holds a spinlock for ten seconds. Reporting it as one more
       trace line is not enough -- the line goes into rings whose way out may
       be the very lock that is stuck, which is exactly how the machine dies
       in silence. Panic instead: the panic path is allowed to send without
       that lock, so the report actually leaves. */
    const ulong stuckNs = 10 * Const::NanoSecsInSec;

    /* Divide the table among the CPUs instead of having each of them walk
       all of it. The list is global and a bucket needs looking at once, so
       twenty CPUs each walking all 512 buckets a hundred times a second was
       doing the same work twenty times over -- and contending on the same
       bucket locks to do it. Every bucket is still visited every tick, by
       exactly one CPU: the coverage and the detection latency do not change,
       only the redundancy. */
    ulong first = 0;
    ulong stride = 1;

    auto& table = CpuTable::GetInstance();
    ulong mask = table.GetRunningCpusNoLock();
    ulong index = table.GetCurrentCpu().GetIndex();

    if (index < MaxCpus && (mask & (1UL << index)) != 0)
    {
        /* Rank among the running CPUs rather than the index itself: on x86
           the index is the APIC ID, which a hybrid part leaves full of holes
           (0,1,8,9,...,62 for twenty threads). Ranks are dense, so every
           bucket falls to exactly one CPU. */
        stride = PopCount(mask);
        first = PopCount(mask & ((1UL << index) - 1));
    }

    /* Recorded so `watchdog` can show how the table is divided. Written only
       when the split changes, which after bring-up is never: a store every
       tick would put every CPU back on a shared line. */
    if (index < SliceCpus &&
        (CpuSlice[index].First != first || CpuSlice[index].Stride != stride))
    {
        CpuSlice[index].First = first;
        CpuSlice[index].Stride = stride;
    }

    /* Once per walk. This used to be incremented for every lock examined --
       one global atomic, touched by every CPU, as many times a second as
       there are watched locks times the tick rate. */
    CheckCounter.Inc();

    RawSpinLock* stuck = nullptr;
    ulong stuckHeldNs = 0;

    for (size_t i = first; i < Stdlib::ArraySize(SpinLockList); i += stride)
    {
        auto& listLock = SpinLockListLock[i];
        auto& list = SpinLockList[i];

        if (list.IsEmpty())
            continue;

        ulong flags = listLock.LockIrqSave();
        for (Stdlib::ListEntry* entry = list.Flink;
            entry != &list;
            entry = entry->Flink)
        {
            RawSpinLock* lock = CONTAINING_RECORD(entry, RawSpinLock, WatchdogListEntry);
            u64 lockTime = lock->WatchdogLockTime.Get();
            if (lockTime != 0 && now > lockTime)
            {
                /* 0 until the counter's rate is known -- no report then,
                   rather than a report built on a guessed frequency. */
                ulong deltaNs = CycleCounterDeltaToNs(now - lockTime);
                if (deltaNs > timeoutNs && lock->WatchdogReported.Cmpxchg(1, 0) == 0)
                {
                    Trace(0, "Spinlock 0x%p is held too long %u", lock, deltaNs);
                }

                if (deltaNs > stuckNs && stuck == nullptr)
                {
                    stuck = lock;
                    stuckHeldNs = deltaNs;
                }
            }
        }
        listLock.UnlockIrqRestore(flags);

        /* Outside the list lock: Panic prints, and printing must not run with
           this held. */
        if (stuck != nullptr)
            Panic("Spinlock 0x%p stuck, held %u ns", (ulong)stuck, stuckHeldNs);
    }
}

void Watchdog::RegisterSpinLock(RawSpinLock& lock)
{
    size_t i = Stdlib::HashPtr(&lock) % Stdlib::ArraySize(SpinLockList);
    auto& listLock = SpinLockListLock[i];
    auto& list = SpinLockList[i];

    ulong flags = listLock.LockIrqSave();
    list.InsertTail(&lock.WatchdogListEntry);
    SpinLockCounter.Inc();
    listLock.UnlockIrqRestore(flags);
}

void Watchdog::UnregisterSpinLock(RawSpinLock& lock)
{
    size_t i = Stdlib::HashPtr(&lock) % Stdlib::ArraySize(SpinLockList);
    auto& listLock = SpinLockListLock[i];

    ulong flags = listLock.LockIrqSave();
    if (!lock.WatchdogListEntry.IsEmpty())
    {
        lock.WatchdogListEntry.Remove();
        SpinLockCounter.Dec();
    }
    listLock.UnlockIrqRestore(flags);
}

void Watchdog::Dump(Stdlib::Printer& printer)
{
    printer.Printf("locks watched: %u\n", SpinLockCounter.Get());
    printer.Printf("table walks:   %u\n", CheckCounter.Get());
    printer.Printf("buckets:       %u\n", (ulong)Stdlib::ArraySize(SpinLockList));

    /* Which slice of the table each CPU walks. The slices have to be dense
       and disjoint, or a bucket belongs to nobody and a lock stuck in it is
       never noticed. */
    for (ulong i = 0; i < SliceCpus; i++)
    {
        if (CpuSlice[i].Stride == 0)
            continue;

        printer.Printf("cpu %u: %u buckets, from %u every %u\n", i,
            (ulong)((Stdlib::ArraySize(SpinLockList) - CpuSlice[i].First +
                CpuSlice[i].Stride - 1) / CpuSlice[i].Stride),
            CpuSlice[i].First, CpuSlice[i].Stride);
    }
}

}