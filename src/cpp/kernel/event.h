#pragma once

#include "atomic.h"

namespace Kernel
{

/* One task waiting for others to have something for it: an auto-reset
   event. Signal() comes from anywhere, a hard IRQ handler included; the one
   task that calls Wait() comes back once for every run of signals since it
   last did.

   Wait() takes the task out of the scheduler's walk (Task::Block) rather
   than spinning on Schedule() the way WaitGroup does, so a waiter costs its
   CPU nothing while there is nothing to do; and a Signal() that finds it
   blocked sends that CPU an IPI, so it runs now rather than at the next tick.
   The handshake is SoftIrq::Run's: the waiter publishes itself and raises its
   blocked flag before it looks at Signaled one last time, and Signal() sets
   Signaled before it looks for a waiter -- every step a sequentially
   consistent atomic, so a signal landing anywhere in the waiter's window is
   either seen by the re-check or clears the flag before Schedule() can act
   on it. Unlike SoftIrq::Run, the window runs with interrupts off: a tick or
   an IPI in it would switch the blocked task out before its re-check, and an
   event has no TickKick to come back for it.

   The IPI goes to the CPU the waiter blocked on. A waiter free to move may
   be moved as it blocks, and is then woken at its new CPU's next scheduling
   point instead: pin a waiter whose wakeups have to be prompt. */
class Event final
{
public:
    Event();
    ~Event();

    /* Task context, one waiter at a time (a second panics) -- and never a
       CPU's idle task, which is the scheduler's last resort and must stay
       runnable: a task exiting on a CPU whose idle task is blocked has
       nothing to switch to. */
    void Wait();

    /* Wait() for at most timeoutNs: true when signalled, false when the
       time ran out first. The deadline is Sleep()'s -- the scheduler runs a
       blocked task whose SleepUntil has passed at its CPU's next scheduling
       point, the tick at the latest -- so a waiter costs its CPU nothing
       either way, and a Signal() still wakes it at once. */
    bool WaitFor(unsigned long long timeoutNs);

    /* Any context. A signaller is done with the event only when Signal()
       returns -- the waiter having taken the signal says nothing about that
       -- so the event, and the waiting task, must outlive every Signal()
       that may still be running: whoever frees them has to know that no
       signaller is left, not merely that the waiter woke. */
    void Signal();

private:
    Event(const Event& other) = delete;
    Event(Event&& other) = delete;
    Event& operator=(const Event& other) = delete;
    Event& operator=(Event&& other) = delete;

    Atomic Signaled;
    Atomic Waiter;      /* the Task in Wait(), 0 when none is */
    Atomic WaiterCpu;
};

}
