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
        {
            /* A writer may have started waiting between the check above
               and the cmpxchg; back out so it isn't starved */
            if (WriterWaiting.Get() != 0)
            {
                Value.Dec();
                Schedule();
                continue;
            }
            break;
        }

        Schedule();
    }
}

void RwMutex::ReadUnlock()
{
    Value.Dec();
}

void RwMutex::WriteLock()
{
    WriterWaiting.Inc();
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
    WriterWaiting.Dec();
}

}
