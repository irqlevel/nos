#pragma once

#include "atomic.h"

namespace Kernel
{

/*
 * Reader-writer mutex with writer priority.
 *
 * Like RawRwSpinLock but yields the CPU (YieldToRunnable()) when contending
 * instead of busy-spinning.  Use in task context only — must not be held
 * across IRQ handlers or with preemption/interrupts disabled.
 *
 * Yielding is to another runnable task, never to the idle task: unlocking
 * is a store, with no waiter to unblock and no IPI to send, so a waiter
 * parked behind a halted CPU would wait out the tick (see WaitGroup::Wait).
 *
 * Value encoding:
 *   0   = unlocked
 *  >0   = N concurrent readers hold the lock
 *  -1   = one writer holds the lock
 *
 * WriterWaiting: when non-zero, new readers back off and yield so the
 * writer can acquire without starvation.
 */
class RwMutex final
{
public:
    RwMutex();
    ~RwMutex();

    void ReadLock();
    void ReadUnlock();

    void WriteLock();
    void WriteUnlock();

private:
    RwMutex(const RwMutex& other) = delete;
    RwMutex(RwMutex&& other) = delete;
    RwMutex& operator=(const RwMutex& other) = delete;
    RwMutex& operator=(RwMutex&& other) = delete;

    Atomic Value;
    Atomic WriterWaiting;
};

}
