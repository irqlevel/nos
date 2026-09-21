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
    /* YieldToRunnable, not Schedule: the counter is brought down by whoever
       the waiter waits for -- an interrupt handler, for every user this has
       -- and Done() only decrements. It has no waiter to unblock and no IPI
       to send, so a waiter that Schedule() handed to the idle task is woken
       by nothing before its CPU's own next tick.

       That is not a tail: it is every wait on a CPU with nothing else
       runnable. A synchronous block read on an otherwise idle machine cost
       one whole tick, and the loop is self-clocking -- submit, halt, wake on
       the tick, submit again -- so the cost did not average out. Measured on
       the AX41 (2026-09-21, master 9c08e8b), blkload over an NVMe that
       answers in 11 us: qd=1 got 99 IOPS, every I/O 10.2 ms, p50 = p99.9;
       qd=8 got 156k because the workers kept each other off the idle task,
       and the ~5.7 of 8 that did not still showed as p99.9 10.2 ms. The same
       qd=1 run with the machine kept busy: 49k IOPS, p50 16.8 us -- 498x, for
       nothing but having something else to run. */
    while (Counter.Get() != 0)
    {
        YieldToRunnable();
    }
}

long WaitGroup::GetCounter()
{
    return Counter.Get();
}

}
