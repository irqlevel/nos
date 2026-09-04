#include "watchdog.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "time.h"
#include "panic.h"
#include "trace.h"

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

Watchdog::Watchdog()
{
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

    RawSpinLock* stuck = nullptr;
    ulong stuckHeldNs = 0;

    for (size_t i = 0; i < Stdlib::ArraySize(SpinLockList); i++)
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
            CheckCounter.Inc();
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
    printer.Printf("%u %u\n", SpinLockCounter.Get(), CheckCounter.Get());
}

}