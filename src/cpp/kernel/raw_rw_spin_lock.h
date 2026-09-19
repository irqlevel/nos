#pragma once

#include <lib/stdlib.h>
#include "atomic.h"

namespace Kernel
{

class Task;

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
 *
 * Holding either side disables preemption, as RawSpinLock::Lock() does. The
 * writer is one, and the lock keeps whose count it raised. Readers are many
 * at once, so that cannot live in the lock: ReadLock() hands it back and
 * ReadUnlock() takes it, the way WriteLockIrqSave() hands back its flags.
 */
class RawRwSpinLock final
{
public:
    /* constexpr with a trivial destructor, for the same reason as Atomic's:
       the static instances (rust_ffi.cpp's) are initialised by the compiler. */
    constexpr RawRwSpinLock()
        : WriterPreemptTask(nullptr)
    {
    }

    ~RawRwSpinLock() = default;

    /* The returned task goes back to ReadUnlock(). It is nullptr when there
       was nothing to disable -- preemption not on yet, or no task on this
       stack -- and ReadUnlock(nullptr) then leaves the count alone, whatever
       the global gate did in between. */
    Task* ReadLock();
    void ReadUnlock(Task* preemptTask);

    void WriteLock();
    void WriteUnlock();

    /* IRQ-save variants for writers running in task/preemptible context.
       Preemption goes off with interrupts, and the flags carry it. */
    ulong WriteLockIrqSave();
    void WriteUnlockIrqRestore(ulong flags);

private:
    RawRwSpinLock(const RawRwSpinLock& other) = delete;
    RawRwSpinLock(RawRwSpinLock&& other) = delete;
    RawRwSpinLock& operator=(const RawRwSpinLock& other) = delete;
    RawRwSpinLock& operator=(RawRwSpinLock&& other) = delete;

    void AcquireWrite();
    void ReleaseWrite();

    Atomic Value;
    Atomic WriterWaiting;

    /* Whose preemption WriteLock() disabled, for WriteUnlock() to enable
       again. nullptr when it had nothing to disable, and whenever the lock is
       not held through WriteLock() -- WriteLockIrqSave's flags carry that
       instead. */
    Task* WriterPreemptTask;
};

}
