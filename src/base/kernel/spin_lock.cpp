#include "spin_lock.h"
#include "task.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "watchdog.h"

namespace kernel
{

SpinLock::SpinLock()
    : owner_(nullptr)
{
}

SpinLock::~SpinLock()
{
}

void SpinLock::unwatch()
{
    Watchdog::get_instance().unregister_spin_lock(raw_lock_);
}

void SpinLock::lock()
{
    raw_lock_.lock();
    owner_ = (preempt_is_on()) ? preempt_current_task() : nullptr;
}

void SpinLock::unlock()
{
    owner_ = nullptr;
    raw_lock_.unlock();
}

/* Through the raw lock's IRQ-saving form: preemption goes off with
   interrupts and the flags carry it, instead of lock() counting it a second
   time. */
void SpinLock::lock(ulong& flags)
{
    flags = raw_lock_.lock_irq_save();
    owner_ = (preempt_is_on()) ? preempt_current_task() : nullptr;
}

void SpinLock::unlock(ulong flags)
{
    owner_ = nullptr;
    raw_lock_.unlock_irq_restore(flags);
}

void SpinLock::shared_lock(ulong& flags)
{
    lock(flags);
}

void SpinLock::shared_unlock(ulong flags)
{
    unlock(flags);
}

}