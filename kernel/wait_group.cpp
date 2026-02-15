#include "wait_group.h"
#include "panic.h"
#include "sched.h"

namespace Kernel
{

WaitGroup::WaitGroup()
    : Counter(0)
{
}

WaitGroup::WaitGroup(long count)
    : Counter(count)
{
    BugOn(count < 0);
}

WaitGroup::~WaitGroup()
{
    BugOn(Counter.Get() != 0);
}

void WaitGroup::Add(long delta)
{
    BugOn(delta <= 0);
    Counter.Add(delta);
}

void WaitGroup::Done()
{
    BugOn(Counter.Get() <= 0);
    Counter.Dec();
}

void WaitGroup::Wait()
{
    while (Counter.Get() != 0)
    {
        Schedule();
    }
}

long WaitGroup::GetCounter()
{
    return Counter.Get();
}

}
