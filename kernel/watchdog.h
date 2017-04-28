#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>
#include <lib/printer.h>

#include "spin_lock.h"
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

    void Dump(Shared::Printer& printer);

private:
    Watchdog(const Watchdog& other) = delete;
    Watchdog(Watchdog&& other) = delete;
    Watchdog& operator=(const Watchdog& other) = delete;
    Watchdog& operator=(Watchdog&& other) = delete;

    class Lock final
    {
    public:
        Lock();
        ~Lock();
        void Acquire();
        void Release();
    private:
        Lock(const Lock& other) = delete;
        Lock(Lock&& other) = delete;
        Lock& operator=(const Lock& other) = delete;
        Lock& operator=(Lock&& other) = delete;

        ulong Flags;
        Atomic RawLock;
    };

    static const size_t SpinLockHashSize = 512;

    Shared::ListEntry SpinLockList[SpinLockHashSize];
    Lock SpinLockListLock[SpinLockHashSize];

    Atomic CheckCounter;
    Atomic SpinLockCounter;

    Watchdog();
    ~Watchdog();
};

}