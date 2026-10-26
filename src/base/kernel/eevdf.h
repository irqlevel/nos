#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>

namespace kernel
{

/* What the scheduler knows of a task, and the rules it decides by:
   EEVDF -- earliest eligible virtual deadline first (Stoica and Abdel-Wahab,
   1995; Linux's fair class since 6.6).

   Each task is owed a share of its CPU in proportion to its weight, and
   virtual time is what it has had of it: a nanosecond on the CPU advances a
   task's VRuntime by 1024/Weight virtual nanoseconds, so a heavier task's
   moves slower. V, the queue's weighted average of VRuntime, is where a task
   would be had it had exactly its share; its lag, V - VRuntime, is what it is
   owed. A task is eligible while its lag is not negative. Each asks for a
   slice of the CPU at a time, and its virtual deadline is where its
   VRuntime will be when it has had the slice: the next to run is the
   eligible task whose deadline comes first. So the weight decides how much
   of the CPU a task gets, and the slice how soon after it is eligible it
   gets it -- a short slice is an early deadline, latency without a larger
   share. Lag is kept across a sleep and a move to another CPU, so neither
   is a way to be owed more or to owe less.

   Everything here is integers -- the kernel has no floating point -- and
   the 128-bit products are multiplications only, never a division the
   compiler would turn into a library call. It knows nothing of tasks,
   locks or CPUs: an entity and the queue it is on, so that it can be held to
   a model of its own on the host (fuzz/cpp/targets/eevdf.cpp). */
struct SchedEntity
{
    SchedEntity();

    /* Its share, from its nice: 1024 at nice 0, each step 1.25 times the
       next (Linux's sched_prio_to_weight) */
    ulong weight;

    /* Virtual nanoseconds per nanosecond on the CPU, times 2^32:
       (1024 << 32) / Weight */
    ulong vmult;

    long nice;

    /* How long it asks to run at a time: its deadline is a slice past its
       VRuntime when it is placed or its slice is done */
    ulong slice_ns;

    /* The virtual time it has had, and the virtual time its request is due
       by. Compared as differences, so that they may wrap. */
    u64 vruntime;
    u64 deadline;

    /* What it was owed (positive) or owed (negative) as it left its queue --
       asleep, or moved to another CPU -- clamped to lag_limit: where it is
       placed when it comes back */
    s64 vlag;

    /* Placed once: a new task is placed with no lag and half a slice, to
       run soon but not to take the CPU for a whole slice ahead of everybody */
    bool placed;

    /* The lag it was last placed with, as it brought it: for `sched probe` */
    s64 placed_lag;

    /* When it was last charged for the CPU: when it started running, or
       when it was last charged since */
    u64 exec_start;

    /* On its queue's run list */
    stdlib::ListEntry entry;

    /* constexpr, so inline: stdlib::min takes them by reference */
    static constexpr long NICE_MIN = -20;
    static constexpr long NICE_MAX = 19;
    static constexpr ulong NICE0_WEIGHT = 1024;
    static constexpr ulong DEFAULT_SLICE_NS = 3 * consts::NANO_SECS_IN_MS;
    static constexpr ulong MIN_SLICE_NS = 100 * consts::NANO_SECS_IN_USEC;
    static constexpr ulong MAX_SLICE_NS = 100 * consts::NANO_SECS_IN_MS;

    /* Its weight and VMult for nice, clamped to [NICE_MIN, NICE_MAX] */
    void set_nice(long value);

    /* Clamped to [MIN_SLICE_NS, MAX_SLICE_NS] */
    void set_slice(ulong value);

    /* Virtual nanoseconds that ns on the CPU are worth to it, and the
       nanoseconds on the CPU v virtual ones take it */
    u64 virtual_of(ulong ns) const;
    ulong real_of(u64 v) const;

    /* Its slice in virtual time */
    u64 vslice() const;

    /* The most lag it keeps, either way: two slices or a tick, the longer,
       in its virtual time (Linux's limit in update_entity_lag) */
    s64 lag_limit() const;

private:
    SchedEntity(const SchedEntity& other) = delete;
    SchedEntity(SchedEntity&& other) = delete;
    SchedEntity& operator=(const SchedEntity& other) = delete;
    SchedEntity& operator=(SchedEntity&& other) = delete;
};

/* The run list of one CPU, the entity on the CPU, and what is decided over
   them. An entity on the queue is on the list or current -- on the CPU,
   running or about to go to sleep -- and counted in V either way; nothing is
   current while the CPU runs nothing counted (the idle task idling). It
   joins the list or the CPU, goes from one to the other, and leaves from
   either, and is never on the queue but on one of them: so V, reckoned from
   the list and the current one, counts everything on the queue, whatever
   order the calls come in. Take refuses the next while the last is still
   current, which the order that once left one out needed: the next taken off
   the list before the last left the CPU, the lag the last went to sleep with
   was reckoned from a V without the next in it, and a waiter beside one
   spinner went with none, ever, to wait out what was left of a slice at
   every wakeup (test_eevdf). Every decision is made over the queue as it is,
   from scratch: V and every eligibility are recomputed, so nothing kept can
   drift. That makes a decision O(n) in the tasks that can run on the CPU,
   which are few. */
class EevdfQueue
{
public:
    EevdfQueue();

    /* Nothing waits on the list */
    bool is_empty();

    /* The entity on the CPU, nullptr for none */
    SchedEntity* current();

    /* se joins the list -- new, woken, or moved here: its VRuntime at V less
       the lag it brings, and its deadline a slice past that -- half a slice
       for one new. Its own weight moves V as it joins, by w * lag / (W + w),
       and so takes that much of its lag away; the lag is made larger by
       (W + w) / W first, so that once it is in, V less its VRuntime is the
       lag it left with (Linux's place_entity has the derivation). */
    void join(SchedEntity* se);

    /* se joins as Join places it, onto the CPU: the idle task starting to
       run something. Nothing is current. */
    void join_current(SchedEntity* se);

    /* se leaves -- current, going to sleep or exited or the idle task
       idling again; or on the list, moving to another CPU: the lag it takes
       with it, from a V that counts it */
    void leave(SchedEntity* se);

    /* The current one back on the list, displaced and able to run: it keeps
       where it is */
    void put_back();

    /* se, on the list, takes the CPU. Nothing is current: the last one has
       left or gone back, and whatever it took was reckoned with se counted. */
    void take(SchedEntity* se);

    /* The current one has run until now: its VRuntime advanced by what that
       is worth to it, and its deadline renewed if it has been reached. True
       when it was: its slice is done, and another may be due the CPU. */
    bool charge(u64 now);

    /* The eligible entity whose deadline is soonest, the current one among
       them when with_current; the one whose deadline is soonest if none is
       eligible, which is only possible with the current one left out.
       nullptr for nothing to choose. */
    SchedEntity* pick(bool with_current);

    /* The current one asks to let the others go first: its deadline at
       least a slice past V. Bounded, so that a task yielding without end
       does not push its deadline without end. */
    void yield();

    /* Nanoseconds on the CPU until the current one's deadline, 0 if it is
       past */
    ulong real_until_deadline();

    /* The queue's average VRuntime, the current one's included; where an
       entity joining an empty queue starts (the last V there was) if there
       is none */
    u64 v();

    /* Whether se, current or on the list, is at or before V */
    bool is_eligible(const SchedEntity* se);

    /* How many entities are on the list */
    ulong get_count();

    /* The list itself, for its owner to look through (SchedEntity::entry) --
       and not to change but by the calls above */
    stdlib::ListEntry& entries() { return list_; }

    /* Everything off the queue: the list unlinked, and nothing current */
    void clear();

private:
    EevdfQueue(const EevdfQueue& other) = delete;
    EevdfQueue(EevdfQueue&& other) = delete;
    EevdfQueue& operator=(const EevdfQueue& other) = delete;
    EevdfQueue& operator=(EevdfQueue&& other) = delete;

    /* se at V less the lag it brings (Join) */
    void place(SchedEntity* se);

    /* V's parts, over the list and the current one: the smallest VRuntime,
       the sum of weight times VRuntime past it, and the sum of the weights */
    struct Sums
    {
        u64 base;
        u64 weighted;
        u64 load;
    };
    void sum(Sums& sums);
    static u64 average(const Sums& sums);
    static bool eligible(const SchedEntity* se, const Sums& sums);

    /* A VRuntime this far past the base is taken as this far in the sums,
       and counted: a sum that would overflow is a schedule gone wrong, and
       it is not made worse by wrapping */
    static constexpr u64 MAX_SPAN = 1UL << 40;

    stdlib::ListEntry list_;
    ulong count_;
    SchedEntity* curr_;

    /* V when the queue last had something counted */
    u64 last_v_;
};

}
