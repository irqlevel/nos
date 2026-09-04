#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>
#include <lib/printer.h>

#include "raw_spin_lock.h"
#include "atomic.h"

namespace Kernel
{

class Watchdog final
{
public:
    static Watchdog& GetInstance()
    {
        static Watchdog Instance;
        return Instance;
    }

    void RegisterSpinLock(RawSpinLock& lock);
    void UnregisterSpinLock(RawSpinLock& lock);

    void Check();

    void Dump(Stdlib::Printer& printer);

    /* Checked against MaxCpus in the .cpp; kept here so this header does not
       have to pull in cpu.h. */
    static const ulong SliceCpus = 64;

private:
    Watchdog(const Watchdog& other) = delete;
    Watchdog(Watchdog&& other) = delete;
    Watchdog& operator=(const Watchdog& other) = delete;
    Watchdog& operator=(Watchdog&& other) = delete;


    static const size_t SpinLockHashSize = 512;

    /* The watchdog's own locks cannot be RawSpinLocks: a RawSpinLock
       registers itself with the watchdog when it is constructed, and these
       are constructed as part of constructing the watchdog. Same two
       instructions, no registration. */
    class ListLock final
    {
    public:
        ulong LockIrqSave();
        void UnlockIrqRestore(ulong flags);

    private:
        Atomic Value;
    };

    Stdlib::ListEntry SpinLockList[SpinLockHashSize];
    ListLock SpinLockListLock[SpinLockHashSize];

    /* The slice of the bucket table each CPU walks, for Dump. */
    struct Slice
    {
        ulong First;
        ulong Stride;
    };

    Slice CpuSlice[SliceCpus];

    Atomic CheckCounter;
    Atomic SpinLockCounter;

    Watchdog();
    ~Watchdog();
};

}