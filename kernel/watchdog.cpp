#include "watchdog.h"
#include "asm.h"
#include "preempt.h"
#include "time.h"
#include "panic.h"
#include "trace.h"

namespace Kernel
{

Watchdog::Lock::Lock()
    : Flags(0)
    , RawLock(0)
{
}

Watchdog::Lock::~Lock()
{
}

void Watchdog::Lock::Acquire()
{
    PreemptDisable();
    Flags = GetRflags();
    InterruptDisable();
    for (;;)
    {
        if (RawLock.Cmpxchg(1, 0) == 0)
            break;

        Pause();
    }
}

void Watchdog::Lock::Release()
{
    RawLock.Set(0);
    SetRflags(Flags);
    PreemptEnable();
}

Watchdog::Watchdog()
{
}

Watchdog::~Watchdog()
{
}

void Watchdog::Check()
{
    Shared::Time now = GetBootTime();
    Shared::Time timeout(20 * Shared::NanoSecsInMs);

    for (size_t i = 0; i < Shared::ArraySize(SpinLockList); i++)
    {
        auto& listLock = SpinLockListLock[i];
        auto& list = SpinLockList[i];

        listLock.Acquire();

        for (Shared::ListEntry* entry = list.Flink;
            entry != &list;
            entry = entry->Flink)
        {
            SpinLock* lock = CONTAINING_RECORD(entry, SpinLock, ListEntry);
            CheckCounter.Inc();
            Shared::Time lockTime(lock->LockTime.Get());
            if (lockTime.GetValue() != 0)
            {
                Shared::Time delta = now - lockTime;
                if (delta > timeout)
                {
                    Trace(0, "Spinlock 0x%p is held too long %u", lock, delta.GetValue());
                }
            }
        }
        listLock.Release();
    }
}

void Watchdog::RegisterSpinLock(SpinLock& lock)
{
    size_t i = 0;
    auto& listLock = SpinLockListLock[i];
    auto& list = SpinLockList[i];

    listLock.Acquire();
    list.InsertTail(&lock.ListEntry);
    SpinLockCounter.Inc();
    listLock.Release();
}

void Watchdog::UnregisterSpinLock(SpinLock& lock)
{
    size_t i = 0;
    auto& listLock = SpinLockListLock[i];

    listLock.Acquire();
    lock.ListEntry.Remove();
    SpinLockCounter.Dec();
    listLock.Release();
}

void Watchdog::Dump(Shared::Printer& printer)
{
    printer.Printf("%u %u\n", SpinLockCounter.Get(), CheckCounter.Get());
}

}