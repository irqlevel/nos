#include "watchdog.h"
#include "asm.h"
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
    Shared::Time now = GetBootTime();
    Shared::Time timeout(25 * Shared::NanoSecsInMs);

    for (size_t i = 0; i < Shared::ArraySize(SpinLockList); i++)
    {
        auto& listLock = SpinLockListLock[i];
        auto& list = SpinLockList[i];

        if (list.IsEmpty())
            continue;

        ulong flags = listLock.LockIrqSave();
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
        listLock.UnlockIrqRestore(flags);
    }
}

void Watchdog::RegisterSpinLock(SpinLock& lock)
{
    size_t i = Shared::HashPtr(&lock) % Shared::ArraySize(SpinLockList);
    auto& listLock = SpinLockListLock[i];
    auto& list = SpinLockList[i];

    ulong flags = listLock.LockIrqSave();
    list.InsertTail(&lock.ListEntry);
    SpinLockCounter.Inc();
    listLock.UnlockIrqRestore(flags);
}

void Watchdog::UnregisterSpinLock(SpinLock& lock)
{
    size_t i = Shared::HashPtr(&lock) % Shared::ArraySize(SpinLockList);
    auto& listLock = SpinLockListLock[i];

    ulong flags = listLock.LockIrqSave();
    lock.ListEntry.Remove();
    SpinLockCounter.Dec();
    listLock.UnlockIrqRestore(flags);
}

void Watchdog::Dump(Shared::Printer& printer)
{
    printer.Printf("%u %u\n", SpinLockCounter.Get(), CheckCounter.Get());
}

}