#pragma once

#include "spin_lock.h"
#include "task.h"

#include <lib/stdlib.h>
#include <lib/list_entry.h>

namespace kernel
{

/* What one CPU's scheduler did, counted as it does it: how long a task that
   was woken waited for the CPU, how long one displaced while it could still
   run waited to run again, what a decision cost, how many tasks it touched,
   and how many tasks could run each time the CPU's timer had the queue looked
   at. Written only by the CPU that owns the queue, under its lock; read
   from anywhere with no lock, a counter at a time (`sched`), so a reading
   is a moment's and not a consistent whole. */
class SchedStats final
{
public:
    /* Log2 buckets of microseconds: bucket 0 is under 1 us, bucket k is
       [2^(k-1), 2^k) us, and the last holds everything from 2^(Buckets-2) us
       up -- about 4 s */
    static const ulong BUCKETS = 24;

    struct Snapshot
    {
        /* Woken: unblocked by someone, or asleep until a deadline that
           came -- from then until it ran */
        ulong wake[BUCKETS];
        ulong wake_max_ns;
        /* Displaced while it could still run, until it ran again */
        ulong delay[BUCKETS];
        ulong decisions;
        ulong decision_cycles;
        ulong decision_max_cycles;
        /* Tasks a decision touched -- itself, and each it put on the run
           list -- summed over the decisions */
        ulong touched;
        ulong touched_max;
        /* Looks at the queue the timer made, the tasks they found able to
           run -- the one on the CPU included, the idle task not -- summed,
           the most, and how many found two or more */
        ulong looks;
        ulong runnable;
        ulong runnable_max;
        ulong crowded;
    };

    constexpr SchedStats() = default;
    ~SchedStats() = default;

    void wake(ulong ns);
    void delay(ulong ns);
    void decision(ulong cycles, ulong touched);
    void look(ulong runnable);

    void read(Snapshot& out);

    static ulong bucket(ulong ns);
    /* What bucket's entries are under, in microseconds */
    static ulong bucket_limit_us(ulong bucket);

private:
    SchedStats(const SchedStats& other) = delete;
    SchedStats(SchedStats&& other) = delete;
    SchedStats& operator=(const SchedStats& other) = delete;
    SchedStats& operator=(SchedStats&& other) = delete;

    /* One writer, so a relaxed load and a relaxed store and no locked
       instruction -- Atomic's Set is a lock xchg on x86, five of them a
       decision: a reader on another CPU sees the value before or the value
       after, never a torn one */
    static void bump(ulong& counter, ulong by);
    static void raise(ulong& max, ulong value);
    static ulong load(const ulong& counter);

    ulong wake_hist_[BUCKETS] = {};
    ulong wake_max_ = 0;
    ulong delay_hist_[BUCKETS] = {};
    ulong decisions_ = 0;
    ulong decision_cycles_ = 0;
    ulong decision_max_ = 0;
    ulong touched_ = 0;
    ulong touched_max_ = 0;
    ulong looks_ = 0;
    ulong runnable_sum_ = 0;
    ulong runnable_max_ = 0;
    ulong crowded_ = 0;
};

/* Why a decision is made: a voluntary schedule() -- curr blocking, exiting,
   or letting others run; yield_to_runnable() -- curr polling, and giving the
   CPU to whoever else can use it, never to the idle task; Preempt() -- an
   interrupt's, the tick, a slice's end, an IPI for a wakeup, where curr
   keeps the CPU unless another is due it */
enum class ScheduleReason
{
    Voluntary,
    Yield,
    Preempt,
};

class TaskQueue
{
public:
    TaskQueue();
    ~TaskQueue();

    void insert(Task* task);
    void remove(Task* task);

    void schedule(Task* curr, ScheduleReason why);

    /* A task's nice and slice (SchedEntity), set under this queue's lock --
       false when the task is not this queue's, and the caller looks again
       (set_task_nice, set_task_slice) */
    bool set_nice(Task* task, long nice);
    bool set_slice(Task* task, ulong slice_ns);

    /* What EEVDF has of one task on this queue: `sched queue` */
    struct EntityView
    {
        char name[TASK_NAME_LEN];
        ulong pid;
        long nice;
        ulong slice_ns;
        /* V less its VRuntime, and its deadline less V: virtual ns */
        s64 lag;
        s64 deadline;
        bool eligible;
        bool on_cpu;
    };

    /* The queue as it is, copied under its lock into out -- the one on the
       CPU first, if it is counted -- for printing after: at most max, the
       count returned */
    ulong view(EntityView* out, ulong max);

    void clear();

    long get_switch_context_counter();

    /* How many tasks this queue is home to -- runnable, running, asleep, the
       idle task -- maintained by every change of home */
    long get_task_count();

    /* How many of them could run now: the one on the CPU included, the idle
       task not. What the load balancer weighs (Task::select_next_task_queue). */
    long get_runnable_count();

    /* Free tasks that exited on this queue's CPU. Must be called with
       interrupts enabled (it frees stacks, which triggers a blocking TLB
       shootdown) -- see switch_complete. */
    void reap_exited();

    void read_stats(SchedStats::Snapshot& out);

    /* A task this queue is home to, found asleep by an unblock() that has
       just made it Task::RUN_WAKING: handed back from any CPU and any
       context, a hard IRQ handler's included, without a lock -- pushed onto
       a stack this queue's CPU takes whole at its next schedule() and puts
       on the run list (drain_wakes). That schedule() is the waker's to bring
       about, as it always was: Event::signal and SoftIrq::raise send the IPI. */
    void push_wake(Task* task);

private:
    TaskQueue(const TaskQueue &other) = delete;
    TaskQueue(TaskQueue&& other) = delete;
    TaskQueue& operator=(const TaskQueue& other) = delete;
    TaskQueue& operator=(TaskQueue&& other) = delete;

    /* Everything below is under Lock, interrupts off, on this queue's CPU */

    /* The tasks woken since the last look (push_wake), onto the run list in
       the order they were woken; touched counts them, for SchedStats */
    void drain_wakes(ulong& touched);

    /* The sleepers whose deadline is not after now, onto the run list */
    void wake_due(ulong now, ulong& touched);

    /* A task that could not run and now can -- new, woken, moved here:
       placed by its lag (EevdfQueue::join), onto the run list, and counted */
    void enqueue(Task* task);

    /* curr, current and displaced while it can still run: back on the run
       list where its virtual runtime and deadline say */
    void requeue(Task* task);

    /* A task has gone on the run list now: what check_stalls times */
    void note_queued(Task* task);

    /* curr, gone to sleep: its lag kept, no longer counted, and on the
       sleep list if it is asleep until a deadline */
    void asleep(Task* task);

    /* curr as a decision finds it: charged for what it has run at its
       weight if it is current, gone from the queue if it has exited, and
       the idle task in the count or out of it (count_idle) */
    void account(Task* curr, ulong now, bool exited);

    /* The idle task joins the count when it stops idling -- placed as a
       task waking, from the lag it left with -- and leaves it, its lag kept,
       when it idles again: idling, it is not one of the tasks that share the
       CPU, only what runs when none of them can */
    void count_idle(Task* curr, ulong now);

    static Task* task_of(SchedEntity* se);

    /* The slice the current one is given, when another waits for the CPU:
       this CPU's timer brought forward to when its deadline comes, so that
       its slice ends then and not at the next tick. No sooner than MIN_ARM_NS
       from now. */
    void arm_slice(ulong now);

    static const ulong MIN_ARM_NS = 50 * consts::NANO_SECS_IN_USEC;

    /* The idle task made an ordinary one (Task::clear_idle, at a shutdown):
       out of its place as the last resort, and into its turn */
    void release_idle(Task* curr);

    /* A runnable task that has waited STALL_REPORT_NS for this CPU is a
       report, once a wait: a scheduler that can starve one says so. Noted
       under the lock, and said by report_stall once this CPU holds none of
       the scheduler's locks -- the keep path's end, or switch_complete. */
    void check_stalls(ulong now);
    void report_stall();

    static const ulong STALL_REPORT_NS = 5 * consts::NANO_SECS_IN_SEC;

    /* The stall noted and not yet said: this CPU's alone */
    bool stall_pending_;
    char stall_name_[TASK_NAME_LEN];
    ulong stall_pid_;
    ulong stall_waited_ms_;
    long stall_runnable_;

    /* Spins allowed in Schedule while an exited task waits for a
       runnable candidate before declaring the queue broken */
    static const ulong MAX_EXITED_RETRIES = 100000000;

    void switch_to(Task* next, Task* curr);

    void switch_complete(Task* curr);

    static void switch_complete(void* ctx);

    using ListEntry = stdlib::ListEntry;

    /* Every task this queue is home to (Task::list_entry_) */
    ListEntry task_list_;

    /* The runnable ones (Task::sched_.Entry), the one on the CPU among them
       as the queue's current, and the rules a decision over them is made
       by: EEVDF (kernel/eevdf.h). The idle task is one of them only while it
       runs something other than its idling. Only runnable tasks: one blocked
       is off it until woken, so a decision never passes over a task that
       cannot run. */
    EevdfQueue eevdf_;

    /* The idle task is counted -- it runs something other than its idling,
       or waits for its turn to (count_idle) */
    bool idle_counted_;

    /* The tasks asleep until a deadline -- sleep(), Event::wait_for() --
       soonest first (Task::sleep_entry_): what this CPU's timer is armed for,
       and what wake_due looks at the head of */
    ListEntry sleep_list_;

    /* The CPU's idle task, kept apart from the run list: the last resort,
       taken only when nothing else can run */
    Task* idle_task_;

    /* push_wake's stack, a Task* and 0 for none, linked by Task::wake_next_ */
    Atomic wake_head_;

    SpinLock lock_;

    /* Tasks that exited on this CPU, pending a stack free with interrupts
       enabled (reap_exited), rather than in the IRQs-off switch path. */
    ListEntry exited_list_;
    SpinLock exited_lock_;

    Atomic schedule_counter_;
    Atomic switch_context_counter_;
    Atomic task_count_;
    Atomic runnable_count_;

    /* The waits next has just ended, as it is switched in at now: woken,
       or displaced while runnable (SchedStats). Under Lock. */
    void note_run(Task* next, ulong now);

    SchedStats stats_;
};


/* The reschedule for a task that is taking itself off the run list:
   blocked (Event::wait), exited (Task::exit), or sleeping long enough that
   halting the CPU cannot overshoot the deadline (Sleep). When nothing else
   is runnable the CPU goes to the idle task, which halts it until the next
   interrupt that CPU itself takes -- often nothing nearer than its tick.

   A task that is still runnable must not poll here. Nothing wakes it: the
   thing it waits for is a store or a counter in somebody else's interrupt
   handler, with no waiter to unblock and no IPI to send, so it waits out the
   tick with its work already done. Poll with yield_to_runnable(), which gives
   the CPU to any other runnable task but never to the idle one. Every wait
   primitive here does -- WaitGroup, Mutex, RwMutex -- and the one time each
   of them did not, it cost a tick per wait (see WaitGroup::wait). */
void schedule();

/* The reschedule an interrupt asks for -- the tick's and every IPI's: the CPU
   goes to another task runnable on it, if there is one, and otherwise stays
   with the task the interrupt landed on. Never to the idle task while that
   task can still run, as schedule() would: the idle task only halts, and the
   task it displaced, runnable rather than blocked, gets no IPI from whoever
   has work for it -- it waited for the next tick. On the AX41 that took
   netblk's polling worker off its CPU some 25 times a second, a tick each:
   a quarter of the CPU, and a p99 of 10 ms. A task with preemption off keeps
   the CPU, and the reschedule is made when its preempt_enable() brings the
   count back to zero -- deferred, not dropped (see schedule_current). */
void preempt();

/* Gives the CPU to another task runnable on it, if there is one, and returns
   at once if there is not: never to the idle task, as schedule() would, which
   halts the CPU until the next interrupt that CPU takes. The way to poll for
   work another CPU's interrupt will bring, without keeping whatever else is
   runnable here -- a softirq task the tick preempted mid-handler among them
   -- off the CPU the way a plain spin does. */
void yield_to_runnable();

void sleep(ulong nano_secs);

/* Tasks moved between CPU queues since boot. */
long get_task_migration_count();

/* Reschedules since boot that found preemption off on the task they landed
   on, and how many of those landed on an idle task (see schedule_current). */
long get_preempt_deferred_count();
long get_preempt_deferred_idle_count();

/* Runnable tasks reported since boot for having waited too long for their
   CPU (TaskQueue::check_stalls) */
long get_stall_report_count();

/* A task's nice (its share of its CPU, SchedEntity::set_nice) and slice (how
   soon after it can it runs, and for how long at a time), wherever it is:
   under its queue's lock, the queue looked up again if it moved meanwhile.
   From task context. */
void set_task_nice(Task* task, long nice);
void set_task_slice(Task* task, ulong slice_ns);

}