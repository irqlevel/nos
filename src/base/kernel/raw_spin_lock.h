#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>
#include "atomic.h"

namespace kernel
{

class Task;

class RawSpinLock final
{
public:
    /* watched = false only for the watchdog's own locks: registering those
       would recurse through Watchdog::get_instance() while it is being built.
       Everything else is watched, which is the point -- the locks that
       actually wedge a machine are the hot raw ones in the driver and net
       paths, and they used to be invisible because only SpinLock was
       instrumented. */
    explicit RawSpinLock(bool watched = true);
    ~RawSpinLock();

    /* lock() disables preemption until unlock(), as Linux's spin_lock does.
       The holder cannot be switched away with the lock taken -- a timer tick
       would otherwise leave every other taker spinning for a whole time
       slice -- and a caller that asks whether it may block
       (preempt_can_block) is told no. Interrupts stay on: lock_irq_save is for a
       lock an interrupt handler takes too. */
    void lock();
    void unlock();

    /* One attempt, no spin. For paths that must not block on a lock whose
       holder may never release it -- the panic path, above all. Preemption
       stays disabled only if the lock was taken. */
    bool try_lock();

	ulong lock_irq_save();
	void unlock_irq_restore(ulong flags);

	/* Disables interrupts either way; acquired says whether the lock was
	   actually taken, and the caller must decide what to do if it was not. */
	ulong try_lock_irq_save(bool& acquired);

private:
    RawSpinLock(const RawSpinLock& other) = delete;
    RawSpinLock(RawSpinLock&& other) = delete;
    RawSpinLock& operator=(const RawSpinLock& other) = delete;
    RawSpinLock& operator=(RawSpinLock&& other) = delete;

    void acquire();
    bool try_acquire();
    void release();
    void stamp();

    Atomic value_;
    bool watched_;

    /* Whose preemption lock() disabled, for unlock() to enable again -- not
       necessarily the task calling unlock(): the scheduler takes its locks
       in one task and releases them in the next, across the context switch.
       nullptr when lock() had nothing to disable, and whenever the lock is
       free or held through lock_irq_save, whose flags carry that instead. */
    Task* preempt_task_;

public:
    /* Set once the watchdog has complained about this lock, cleared on
       release: one line per stuck episode instead of one per check pass,
       and the watchdog checks over a thousand times a second per CPU. */
    Atomic watchdog_reported_;

private:

public:
    stdlib::ListEntry watchdog_list_entry_;
    Atomic watchdog_lock_time_;
};
}