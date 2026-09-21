#include "mutex.h"
#include "sched.h"

namespace Kernel
{

Mutex::Mutex()
    : Value(0)
{
}

Mutex::~Mutex()
{
}

void Mutex::Lock()
{
    /* YieldToRunnable, not Schedule: Unlock() is a store and nothing else,
       so a waiter the idle task displaced is woken by no one before the next
       tick -- see WaitGroup::Wait for what that costs. Keeping the CPU costs
       nothing that was going to be used: the walk hands it to any other
       runnable task first, and only a CPU with none at all spins. */
    while (Value.Cmpxchg(1, 0) != 0)
    {
        YieldToRunnable();
    }
}

void Mutex::Unlock()
{
    Value.Set(0);
}

void Mutex::Lock(ulong& flags)
{
    flags = 0;
    Lock();
}

void Mutex::Unlock(ulong flags)
{
    (void)flags;
    Unlock();
}

}
