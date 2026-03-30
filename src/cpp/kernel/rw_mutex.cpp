#include "rw_mutex.h"
#include "sched.h"

namespace Kernel
{

RwMutex::RwMutex()
{
}

RwMutex::~RwMutex()
{
}

void RwMutex::ReadLock()
{
    for (;;)
    {
        if (WriterWaiting.Get() != 0)
        {
            Schedule();
            continue;
        }

        long v = Value.Get();
        if (v >= 0 && Value.Cmpxchg(v + 1, v) == v)
            break;

        Schedule();
    }
}

void RwMutex::ReadUnlock()
{
    Value.Dec();
}

void RwMutex::WriteLock()
{
    WriterWaiting.Set(1);
    for (;;)
    {
        if (Value.Cmpxchg(-1, 0) == 0)
            break;

        Schedule();
    }
}

void RwMutex::WriteUnlock()
{
    Value.Set(0);
    WriterWaiting.Set(0);
}

}
