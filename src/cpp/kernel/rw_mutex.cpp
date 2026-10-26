#include "rw_mutex.h"
#include "sched.h"

namespace kernel
{

RwMutex::RwMutex()
{
}

RwMutex::~RwMutex()
{
}

void RwMutex::read_lock()
{
    for (;;)
    {
        if (writer_waiting_.get() != 0)
        {
            yield_to_runnable();
            continue;
        }

        long v = value_.get();
        if (v >= 0 && value_.cmpxchg(v + 1, v) == v)
        {
            /* A writer may have started waiting between the check above
               and the cmpxchg; back out so it isn't starved */
            if (writer_waiting_.get() != 0)
            {
                value_.dec();
                yield_to_runnable();
                continue;
            }
            break;
        }

        yield_to_runnable();
    }
}

void RwMutex::read_unlock()
{
    value_.dec();
}

void RwMutex::write_lock()
{
    writer_waiting_.inc();
    for (;;)
    {
        if (value_.cmpxchg(-1, 0) == 0)
            break;

        yield_to_runnable();
    }
}

void RwMutex::write_unlock()
{
    value_.set(0);
    writer_waiting_.dec();
}

}
