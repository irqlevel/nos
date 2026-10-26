#include "mutex.h"
#include "sched.h"

namespace kernel
{

Mutex::Mutex()
    : value_(0)
{
}

Mutex::~Mutex()
{
}

void Mutex::lock()
{
    /* yield_to_runnable, not Schedule: unlock() is a store and nothing else,
       so a waiter the idle task displaced is woken by no one before the next
       tick -- see WaitGroup::wait for what that costs. Keeping the CPU costs
       nothing that was going to be used: the walk hands it to any other
       runnable task first, and only a CPU with none at all spins. */
    while (value_.cmpxchg(1, 0) != 0)
    {
        yield_to_runnable();
    }
}

void Mutex::unlock()
{
    value_.set(0);
}

void Mutex::lock(ulong& flags)
{
    flags = 0;
    lock();
}

void Mutex::unlock(ulong flags)
{
    (void)flags;
    unlock();
}

}
