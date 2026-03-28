#pragma once

#include <lib/stdlib.h>
#include "atomic.h"

namespace Kernel
{

/*
 * Lightweight reader-writer spinlock with writer priority.
 *
 * Value encoding:
 *   0   = unlocked
 *  >0   = N concurrent readers hold the lock
 *  -1   = one writer holds the lock
 *
 * WriterWaiting: when non-zero, new readers back off and let existing
 * readers drain so the writer can acquire without starvation.
 */
class RawRwSpinLock final
{
public:
    RawRwSpinLock();
    ~RawRwSpinLock();

    void ReadLock();
    void ReadUnlock();

    void WriteLock();
    void WriteUnlock();

    /* IRQ-save variants for writers running in task/preemptible context. */
    ulong WriteLockIrqSave();
    void WriteUnlockIrqRestore(ulong flags);

private:
    RawRwSpinLock(const RawRwSpinLock& other) = delete;
    RawRwSpinLock(RawRwSpinLock&& other) = delete;
    RawRwSpinLock& operator=(const RawRwSpinLock& other) = delete;
    RawRwSpinLock& operator=(RawRwSpinLock&& other) = delete;

    Atomic Value;
    Atomic WriterWaiting;
};

}
