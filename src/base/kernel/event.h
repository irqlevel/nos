#pragma once

#include "atomic.h"

namespace kernel
{

/* One task waiting for others to have something for it: an auto-reset
   event. signal() comes from anywhere, a hard IRQ handler included; the one
   task that calls wait() comes back once for every run of signals since it
   last did.

   wait() takes the task out of the scheduler's walk (Task::block) rather
   than spinning on schedule() the way WaitGroup does, so a waiter costs its
   CPU nothing while there is nothing to do; and a signal() that finds it
   blocked sends that CPU an IPI, so it runs now rather than at the next tick.
   The handshake is SoftIrq::run's: the waiter publishes itself and raises its
   blocked flag before it looks at Signaled one last time, and signal() sets
   Signaled before it looks for a waiter -- every step a sequentially
   consistent atomic, so a signal landing anywhere in the waiter's window is
   either seen by the re-check or clears the flag before schedule() can act
   on it. Unlike SoftIrq::run, the window runs with interrupts off: a tick or
   an IPI in it would switch the blocked task out before its re-check, and an
   event has no tick_kick to come back for it.

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
    void wait();

    /* wait() for at most timeout_ns: true when signalled, false when the
       time ran out first. The deadline is sleep()'s -- the scheduler wakes a
       sleeping task whose sleep_until_ has passed at its CPU's next scheduling
       point, which that CPU's timer, armed for the deadline as the task goes
       to sleep, makes then (TaskQueue::wake_due) -- so a waiter costs its CPU
       nothing either way, ends at its deadline and not at the next tick
       after it, and a signal() still wakes it at once: of the two, exactly
       one does (Task::run_state_). */
    bool wait_for(unsigned long long timeout_ns);

    /* Any context. A signaller is done with the event only when signal()
       returns -- the waiter having taken the signal says nothing about that
       -- so the event, and the waiting task, must outlive every signal()
       that may still be running: whoever frees them has to know that no
       signaller is left, not merely that the waiter woke. */
    void signal();

private:
    Event(const Event& other) = delete;
    Event(Event&& other) = delete;
    Event& operator=(const Event& other) = delete;
    Event& operator=(Event&& other) = delete;

    Atomic signaled_;
    Atomic waiter_;      /* the Task in wait(), 0 when none is */
    Atomic waiter_cpu_;
};

}
