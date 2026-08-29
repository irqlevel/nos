#include "watchdog.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "time.h"
#include "panic.h"
#include "trace.h"

namespace Kernel
{

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
            SpinLock* lock = CONTAINING_RECORD(entry, SpinLock, WatchdogListEntry);
            CheckCounter.Inc();
            u64 lockTime = lock->WatchdogLockTime.Get();
            if (lockTime != 0 && now > lockTime)
            {
                /* 0 until the counter's rate is known -- no report then,
                   rather than a report built on a guessed frequency. */
                ulong deltaNs = CycleCounterDeltaToNs(now - lockTime);
                if (deltaNs > timeoutNs)
                {
                    Trace(0, "Spinlock 0x%p is held too long %u", lock, deltaNs);
                }
            }
        }
        listLock.UnlockIrqRestore(flags);
    }
}

void Watchdog::RegisterSpinLock(SpinLock& lock)
{
    size_t i = Stdlib::HashPtr(&lock) % Stdlib::ArraySize(SpinLockList);
    auto& listLock = SpinLockListLock[i];
    auto& list = SpinLockList[i];

    ulong flags = listLock.LockIrqSave();
    list.InsertTail(&lock.WatchdogListEntry);
    SpinLockCounter.Inc();
    listLock.UnlockIrqRestore(flags);
}

void Watchdog::UnregisterSpinLock(SpinLock& lock)
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