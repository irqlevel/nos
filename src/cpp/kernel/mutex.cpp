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
    while (Value.Cmpxchg(1, 0) != 0)
    {
        Schedule();
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
