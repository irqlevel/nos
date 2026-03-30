#pragma once

#include "atomic.h"

namespace Kernel
{

/*
 * Reader-writer mutex with writer priority.
 *
 * Like RawRwSpinLock but yields the CPU (Schedule()) when contending
 * instead of busy-spinning.  Use in task context only — must not be held
 * across IRQ handlers or with preemption/interrupts disabled.
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
