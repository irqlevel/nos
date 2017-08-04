#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>
#include <lib/printer.h>

#include "spin_lock.h"
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

    void RegisterSpinLock(SpinLock& lock);
    void UnregisterSpinLock(SpinLock& lock);

    void Check();

    void Dump(Stdlib::Printer& printer);

private:
    Watchdog(const Watchdog& other) = delete;
    Watchdog(Watchdog&& other) = delete;
    Watchdog& operator=(const Watchdog& other) = delete;
    Watchdog& operator=(Watchdog&& other) = delete;


    static const size_t SpinLockHashSize = 512;

    Stdlib::ListEntry SpinLockList[SpinLockHashSize];
    RawSpinLock SpinLockListLock[SpinLockHashSize];

    Atomic CheckCounter;
    Atomic SpinLockCounter;

    Watchdog();
    ~Watchdog();
};

}