#pragma once

#include "atomic.h"
#include <include/const.h>

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
   point instead: pin a waiter whose wakeups have to be prompt.

   What signal() does to the waiting task it does inside a window: counted
   (signallers_), with interrupts off, and the waiter looked at again inside
   it. A wait() lets go of its task only once no signal() is left in the
   window (let_go), so the task it returns to may exit and be freed, or block
   on something else, at once: no signal() of this event touches it again.
   Before, one that had read the waiter as its wait ended went on to unblock
   the task after it had exited -- a task freed, or one by then asleep on
   something else. */
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

    /* A signal looked for for up to poll_ns, spinning rather than blocking:
       true when one came -- taken, as wait() takes it -- and false when the
       time ran out first. Task context, the one waiter's, interrupts on, so
       that a signal() from this CPU's own interrupt is seen as one from any
       other: it finds no waiter blocked, and only sets the flag the spin
       reads. What a waiter that expects its signal within microseconds
       does before wait_for(), so as not to pay a sleep and a wake -- a
       schedule, an idle, the IPI -- for it. A plain read a time round, so
       that the spin keeps the flag's line shared until the signal. */
    bool poll_for(unsigned long long poll_ns);

    /* Any context. A signaller is done with the event only when signal()
       returns -- the waiter having taken the signal says nothing about that
       -- so the event must outlive every signal() that may still be running:
       whoever frees it has to know that no signaller is left, not merely
       that the waiter woke. The waiting task need not: its wait() returns
       only once no signal() may touch it (let_go).

       What the caller wrote for the waiter before the call is there for the
       waiter that takes the signal, whatever it was written with -- plain
       stores included, a lock-free ring's push: signal() makes a full
       barrier between them and its look at the flag (signal_holding). */
    void signal();

    /* signal(), holding the waiter it found for hold_ns inside its window --
       interrupts off -- before it touches it: what the boot's test widens
       the window with, so that a wait() letting go of its task meets a
       signal() still in it at every boot (test_event_let_go). Nothing else
       is to call it. */
    void signal_holding(ulong hold_ns);

    /* The waits that, letting go of their task, met a signal() still inside
       its window and waited it out: what that test holds to having
       happened. */
    static ulong let_go_waits();

private:
    Event(const Event& other) = delete;
    Event(Event&& other) = delete;
    Event& operator=(const Event& other) = delete;
    Event& operator=(Event&& other) = delete;

    /* The waiter's slot emptied, and its wait() done with its task once no
       signal() is left in its window -- every way out of a wait() */
    void let_go();

    /* A signal() in its window that has not left it after this long is one
       wedged with its interrupts off: said, not waited for */
    static constexpr ulong LET_GO_STUCK_NS = 10 * consts::NANO_SECS_IN_SEC;
    /* How many turns of the spin between two looks at the clock */
    static constexpr ulong LET_GO_CLOCK_EVERY = 1024;

    Atomic signaled_;
    Atomic waiter_;      /* the Task in wait(), 0 when none is */
    Atomic waiter_cpu_;
    /* signal()s inside their window: from before their look at waiter_ to
       after their last touch of the task it named */
    Atomic signallers_;
};

}
