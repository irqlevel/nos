#pragma once

#include <lib/stdlib.h>
#include "atomic.h"

namespace kernel
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
 * writer_waiting_: when non-zero, new readers back off and let existing
 * readers drain so the writer can acquire without starvation.
 *
 * Holding either side disables preemption, as RawSpinLock::lock() does. The
 * writer is one, and the lock keeps whose count it raised. Readers are many
 * at once, so that cannot live in the lock: read_lock() hands it back and
 * read_unlock() takes it, the way write_lock_irq_save() hands back its flags.
 */
class RawRwSpinLock final
{
public:
    /* constexpr with a trivial destructor, for the same reason as Atomic's:
       the static instances (rust_ffi.cpp's) are initialised by the compiler. */
    constexpr RawRwSpinLock()
        : writer_preempt_task_(nullptr)
    {
    }

    ~RawRwSpinLock() = default;

    /* The returned task goes back to read_unlock(). It is nullptr when there
       was nothing to disable -- preemption not on yet, or no task on this
       stack -- and read_unlock(nullptr) then leaves the count alone, whatever
       the global gate did in between. */
    Task* read_lock();
    void read_unlock(Task* preempt_task);

    void write_lock();
    void write_unlock();

    /* IRQ-save variants for writers running in task/preemptible context.
       Preemption goes off with interrupts, and the flags carry it. */
    ulong write_lock_irq_save();
    void write_unlock_irq_restore(ulong flags);

private:
    RawRwSpinLock(const RawRwSpinLock& other) = delete;
    RawRwSpinLock(RawRwSpinLock&& other) = delete;
    RawRwSpinLock& operator=(const RawRwSpinLock& other) = delete;
    RawRwSpinLock& operator=(RawRwSpinLock&& other) = delete;

    void acquire_write();
    void release_write();

    Atomic value_;
    Atomic writer_waiting_;

    /* Whose preemption write_lock() disabled, for write_unlock() to enable
       again. nullptr when it had nothing to disable, and whenever the lock is
       not held through write_lock() -- write_lock_irq_save's flags carry that
       instead. */
    Task* writer_preempt_task_;
};

}
