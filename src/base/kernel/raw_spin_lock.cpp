#include "raw_spin_lock.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "watchdog.h"

namespace kernel
{

RawSpinLock::RawSpinLock(bool watched)
    : watched_(watched)
    , preempt_task_(nullptr)
    , watchdog_reported_(0)
    , watchdog_lock_time_(0)
{
    if (watched_)
        Watchdog::get_instance().register_spin_lock(*this);
}

RawSpinLock::~RawSpinLock()
{
    if (watched_)
        Watchdog::get_instance().unregister_spin_lock(*this);
}

void RawSpinLock::unwatch()
{
    if (!watched_)
        return;

    Watchdog::get_instance().unregister_spin_lock(*this);
    watched_ = false;
    watchdog_lock_time_.set(0);
}

/* Raw cycle counter, not get_boot_time(): this runs on every lock in the
   kernel. 0 is the "not held" marker the watchdog reads, so a counter that
   happens to read exactly 0 is nudged rather than losing the timestamp. */
void RawSpinLock::stamp()
{
    if (!watched_)
        return;

    u64 now = hal::read_cycle_counter();
    watchdog_lock_time_.set_relaxed((now != 0) ? now : 1);
}

void RawSpinLock::acquire()
{
    /* Free: the one cmpxchg is the whole of it */
    if (likely(value_.cmpxchg(1, 0) == 0))
    {
        stamp();
        return;
    }

    contend();
    stamp();
}

/* Out of line, past the first try. A waiter that spins on cmpxchg takes the
   lock's line for itself at every try, from the holder too, whose every
   write to it then waits its turn behind the waiters': on Newton's two
   sockets of 48 CPUs all taking one lock at each allocation and free, a
   64 KiB block's pair cost 330 us where it cost 0.5 us on one CPU
   (heapbench). Read instead: a read shares the line with every other
   waiter, and the line moves when the holder writes it, not when a waiter
   looks -- 94 us there, the same lock. When it reads free, one cmpxchg;
   lost to another waiter that saw it free too, back to reading. A pause
   after a lost cmpxchg, doubling up to 16, 64 or 256 pauses, measured no
   different there, and is not here. */
void RawSpinLock::contend()
{
    for (;;)
    {
        while (value_.get_relaxed() != 0)
            cpu_relax();

        if (value_.cmpxchg(1, 0) == 0)
            return;
    }
}

bool RawSpinLock::try_acquire()
{
    if (value_.cmpxchg(1, 0) != 0)
        return false;

    stamp();
    return true;
}

/* Stores, not lock xchg: the release's store orders everything before it
   -- the stamp's clearing too, which the next holder's stamp then follows --
   and a report flag that is already clear is not written at all, the line
   being the one every waiter reads. */
void RawSpinLock::release()
{
    if (watched_)
    {
        if (watchdog_reported_.get_relaxed() != 0)
            watchdog_reported_.set_relaxed(0);
        watchdog_lock_time_.set_relaxed(0);
    }
    value_.set_release(0);
}

void RawSpinLock::lock()
{
    /* Before the acquire, not after: a tick landing between the two would
       switch the new holder away with the lock taken. */
    Task* task = preempt_disable_task();
    acquire();
    preempt_task_ = task;
}

void RawSpinLock::unlock()
{
    /* Read and cleared while the lock is still ours: once release() runs,
       the next holder writes its own. */
    Task* task = preempt_task_;
    preempt_task_ = nullptr;
    release();
    preempt_enable_task(task);
}

bool RawSpinLock::try_lock()
{
    Task* task = preempt_disable_task();
    if (!try_acquire())
    {
        preempt_enable_task(task);
        return false;
    }

    preempt_task_ = task;
    return true;
}

/* The IRQ-saving forms leave preempt_task_ alone: preempt_irq_save disables
   preemption along with interrupts, and the flags carry it to the restore. */
ulong RawSpinLock::lock_irq_save()
{
    ulong flags = preempt_irq_save();
    acquire();
    return flags;
}

ulong RawSpinLock::try_lock_irq_save(bool& acquired)
{
    ulong flags = preempt_irq_save();
    acquired = try_acquire();
    return flags;
}

void RawSpinLock::unlock_irq_restore(ulong flags)
{
    release();
    preempt_irq_restore(flags);
}

}
