#include "sched.h"
#include "panic.h"
#include "trace.h"
#include "preempt.h"
#include "time.h"
#include <hal/cpu.h>
#include "debug.h"
#include "cpu.h"

namespace kernel
{

/* Tasks moved from one CPU's queue to another. This used to happen on every
   context switch by construction; it is now supposed to be rare, and a
   number is the only way to know whether it is. */
static Atomic task_migrations;

/* Reschedules that found the task they landed on with preemption off, and
   of those, how many landed on an idle task. */
static Atomic preempts_deferred;
static Atomic preempts_deferred_idle;

long get_task_migration_count()
{
    return task_migrations.get();
}

long get_preempt_deferred_count()
{
    return preempts_deferred.get();
}

long get_preempt_deferred_idle_count()
{
    return preempts_deferred_idle.get();
}

/* Runnable tasks reported for having waited TaskQueue::STALL_REPORT_NS for
   their CPU (TaskQueue::check_stalls) */
static Atomic stall_reports;

long get_stall_report_count()
{
    return stall_reports.get();
}

void SchedStats::bump(ulong& counter, ulong by)
{
    __atomic_store_n(&counter, __atomic_load_n(&counter, __ATOMIC_RELAXED) + by, __ATOMIC_RELAXED);
}

void SchedStats::raise(ulong& max, ulong value)
{
    if (__atomic_load_n(&max, __ATOMIC_RELAXED) < value)
        __atomic_store_n(&max, value, __ATOMIC_RELAXED);
}

ulong SchedStats::load(const ulong& counter)
{
    return __atomic_load_n(&counter, __ATOMIC_RELAXED);
}

ulong SchedStats::bucket(ulong ns)
{
    ulong us = ns / consts::NANO_SECS_IN_USEC;
    if (us == 0)
        return 0;

    ulong bucket = stdlib::size_of_in_bits<ulong>() - (ulong)__builtin_clzl(us);
    return stdlib::min(bucket, BUCKETS - 1);
}

ulong SchedStats::bucket_limit_us(ulong bucket)
{
    return 1UL << stdlib::min(bucket, BUCKETS - 1);
}

void SchedStats::wake(ulong ns)
{
    bump(wake_hist_[bucket(ns)], 1);
    raise(wake_max_, ns);
}

void SchedStats::delay(ulong ns)
{
    bump(delay_hist_[bucket(ns)], 1);
}

void SchedStats::decision(ulong cycles, ulong touched)
{
    bump(decisions_, 1);
    bump(decision_cycles_, cycles);
    raise(decision_max_, cycles);
    bump(touched_, touched);
    raise(touched_max_, touched);
}

void SchedStats::look(ulong runnable)
{
    bump(looks_, 1);
    bump(runnable_sum_, runnable);
    raise(runnable_max_, runnable);
    if (runnable >= 2)
        bump(crowded_, 1);
}

void SchedStats::read(Snapshot& out)
{
    for (ulong i = 0; i < BUCKETS; i++)
    {
        out.wake[i] = load(wake_hist_[i]);
        out.delay[i] = load(delay_hist_[i]);
    }
    out.wake_max_ns = load(wake_max_);
    out.decisions = load(decisions_);
    out.decision_cycles = load(decision_cycles_);
    out.decision_max_cycles = load(decision_max_);
    out.touched = load(touched_);
    out.touched_max = load(touched_max_);
    out.looks = load(looks_);
    out.runnable = load(runnable_sum_);
    out.runnable_max = load(runnable_max_);
    out.crowded = load(crowded_);
}

void TaskQueue::read_stats(SchedStats::Snapshot& out)
{
    stats_.read(out);
}

TaskQueue::TaskQueue()
    : stall_pending_(false)
    , stall_name_{}
    , stall_pid_(0)
    , stall_waited_ms_(0)
    , stall_runnable_(0)
    , idle_counted_(false)
    , idle_task_(nullptr)
{
    stdlib::AutoLock lock(lock_);
    task_list_.init();
    sleep_list_.init();
    exited_list_.init();

    wake_head_.set(0);
    switch_context_counter_.set(0);
    schedule_counter_.set(0);
    task_count_.set(0);
    runnable_count_.set(0);
}

void TaskQueue::reap_exited()
{
    for (;;)
    {
        Task* task = nullptr;
        {
            stdlib::AutoLock lock(exited_lock_);
            if (exited_list_.is_empty())
                break;
            stdlib::ListEntry* entry = exited_list_.remove_head();
            task = CONTAINING_RECORD(entry, Task, list_entry_);
        }
        /* put() here (interrupts enabled) may free the 32KB stack via a
           blocking cross-CPU TLB shootdown; that is safe now but was not in
           the IRQs-off switch_complete path where the task was enqueued. */
        task->put();
    }
}

void TaskQueue::switch_complete(Task* curr)
{
    Task* prev = curr->prev_;

    curr->prev_ = nullptr;
    curr->lock_.unlock();
    prev->lock_.unlock();
    lock_.unlock();

    report_stall();

    if (prev->state_.get() != Task::STATE_EXITED)
    {
        /* Only one that can still run -- displaced, not asleep -- is
           weighed for a lighter CPU. One asleep stays on the queue its
           wakeups go to (Task::unblock) and whose timer is armed for its
           deadline; it moves, if it should, once it runs. Its state cannot
           change under this: a Queued task is moved from there only by
           itself, and it is not running. */
        bool runnable = prev->run_state_.get() == Task::RUN_QUEUED && !prev->is_idle();
        auto task_queue = runnable ? prev->select_next_task_queue() : nullptr;

        /* schedule()'s count should be the only one left: the locks
           TaskQueue::schedule took in prev were released above, in this
           task, and handed their counts back to prev -- a RawSpinLock
           remembers whose preemption it disabled. Anything more would be a
           lock prev still holds, which schedule() never switches away from,
           or a count gone astray. Checked while that last count still keeps
           prev off every CPU. */
        BUG_ON(prev->preempt_disable_counter_.get() != 1);

        if (task_queue != nullptr)
        {
            /* Off this queue, its count down while it is on none, and only
               then onto the other: a decision never finds a task whose count
               is up -- here, prev's comes down before this CPU decides
               again -- so none has one to pass over. It used to be inserted
               there first, and every walk had to skip a task whose count was
               up. */
            task_migrations.inc();
            prev->get();
            prev->task_queue_->remove(prev);
            prev->preempt_disable_counter_.dec();
            task_queue->insert(prev);
            prev->put();
        }
        else
        {
            prev->preempt_disable_counter_.dec();
        }
    } else {

        prev->preempt_disable_counter_.dec();
        /* Defer the final put() -- which frees the 32KB stack via a blocking
           cross-CPU TLB shootdown -- out of this IRQs-disabled context. Two
           CPUs freeing exited stacks here concurrently could deadlock on each
           other's shootdown IPI. Enqueue the task and let reap_exited() free it
           from the idle loop with interrupts enabled. prev->ListEntry was
           remove_init'd in Schedule, so it is free to reuse. */
        stdlib::AutoLock lock(exited_lock_);
        exited_list_.insert_tail(&prev->list_entry_);
    }
}

void TaskQueue::switch_complete(void* ctx)
{
    Task* curr = static_cast<Task*>(ctx);
    curr->task_queue_->switch_complete(curr);
}

void TaskQueue::note_run(Task* next, ulong now)
{
    /* The idle task waits for nothing: it runs when nobody else can */
    if (next->is_idle())
        return;

    /* When it left the CPU, or started: update_runtime stamped it then, and
       it has not run since */
    ulong out = next->run_start_time_.get_value();

    if (next->switched_out_runnable_)
    {
        if (now > out)
            stats_.delay(now - out);
        return;
    }

    /* Whoever woke it said when: an unblock(), or its deadline (wake_due) --
       unless that was before it left the CPU, a wakeup for the wait before
       this one */
    ulong woke = (ulong)next->wake_time_.get();
    if (woke != 0 && woke >= out && now >= woke)
        stats_.wake(now - woke);
}

void TaskQueue::switch_to(Task* next, Task* curr)
{
    switch_context_counter_.inc();

    BUG_ON(curr == next);
    BUG_ON(next->prev_ != nullptr);
    BUG_ON(next->rsp_ == 0);

    if (curr->state_.get() != Task::STATE_EXITED)
        curr->state_.set(Task::STATE_WAITING);

    curr->context_switches_.inc();
    curr->update_runtime();

    BUG_ON(next->state_.get() == Task::STATE_EXITED);
    next->state_.set(Task::STATE_RUNNING);
    stdlib::Time now = get_boot_time();
    note_run(next, now.get_value());
    next->run_start_time_ = now;
    next->sched_.exec_start = now.get_value();
    next->prev_ = curr;
    switch_context(next->rsp_, &curr->rsp_, &TaskQueue::switch_complete, next);
}

/* A Treiber stack that is only ever pushed onto and taken whole, never
   popped: there is no ABA -- a task pushed again after the take is a new
   entry on what was an empty stack -- and the push is all a waker does, from
   whatever context it is in. The compare-and-swap that publishes the entry
   is sequentially consistent, so the wake_next_ written before it is seen by
   the CPU that takes the stack. A task is on one stack at a time: only the
   unblock() that made it Waking pushes it, and only drain_wakes takes it off
   that state. */
void TaskQueue::push_wake(Task* task)
{
    long head = wake_head_.get();
    for (;;)
    {
        task->wake_next_ = reinterpret_cast<Task*>(head);
        long seen = wake_head_.cmpxchg(reinterpret_cast<long>(task), head);
        if (seen == head)
            return;
        head = seen;
    }
}

Task* TaskQueue::task_of(SchedEntity* se)
{
    return (se != nullptr) ? CONTAINING_RECORD(se, Task, sched_) : nullptr;
}

void TaskQueue::account(Task* curr, ulong now, bool exited)
{
    /* On the CPU is curr, or nothing counted: the idle task idling, or an
       exited task gone already, at a look before this one */
    SchedEntity* se = &curr->sched_;
    BUG_ON(eevdf_.current() != nullptr && eevdf_.current() != se);

    if (eevdf_.current() == se)
    {
        if (exited)
            eevdf_.leave(se);
        else
            eevdf_.charge(now);
    }
    count_idle(curr, now);
}

void TaskQueue::count_idle(Task* curr, ulong now)
{
    if (curr != idle_task_)
        return;

    if (curr->is_idling() && idle_counted_)
    {
        eevdf_.leave(&curr->sched_);
        idle_counted_ = false;
    }
    else if (!curr->is_idling() && !idle_counted_)
    {
        eevdf_.join_current(&curr->sched_);
        curr->sched_.exec_start = now;
        idle_counted_ = true;
    }
}

void TaskQueue::arm_slice(ulong now)
{
    /* Alone on the CPU, the current one's slice ends deciding nothing: it
       would keep the CPU. A task that joins later is a decision of its own
       -- a wakeup's, a deadline's, the tick's -- which arms it then. A
       guest's vCPU on a core of its own would otherwise leave its guest
       every slice for nothing. */
    if (eevdf_.current() == nullptr || eevdf_.is_empty())
        return;

    ulong ns = stdlib::max(eevdf_.real_until_deadline(), MIN_ARM_NS);
    CpuTable::get_instance().get_current_cpu().local_timer_wake(now + ns);
}

void TaskQueue::enqueue(Task* task)
{
    runnable_count_.inc();
    eevdf_.join(&task->sched_);
    note_queued(task);
}

void TaskQueue::requeue(Task* task)
{
    BUG_ON(eevdf_.current() != &task->sched_);
    eevdf_.put_back();
    note_queued(task);
}

void TaskQueue::note_queued(Task* task)
{
    task->queued_at_ = get_boot_time().get_value();
    task->stall_reported_ = false;
}

void TaskQueue::asleep(Task* task)
{
    /* The lag it goes with, V counting it still: it is current */
    BUG_ON(eevdf_.current() != &task->sched_);
    eevdf_.leave(&task->sched_);

    runnable_count_.dec();
    BUG_ON(runnable_count_.get() < 0);

    ulong until = (ulong)task->sleep_until_.get();
    if (until == 0)
        return;

    /* Soonest first. From the tail, where a later deadline mostly goes; the
       list is as long as this CPU has sleepers, a handful */
    auto* pos = sleep_list_.blink;
    while (pos != &sleep_list_ &&
           (ulong)CONTAINING_RECORD(pos, Task, sleep_entry_)->sleep_until_.get() > until)
        pos = pos->blink;

    /* After pos, which is before the one that follows it */
    pos->flink->insert_tail(&task->sleep_entry_);
}

void TaskQueue::drain_wakes(ulong& touched)
{
    if (wake_head_.get() == 0)
        return;

    /* The whole stack at once. Only this CPU takes it to 0, so it is not 0
       again until it is taken. */
    long head = wake_head_.get();
    for (;;)
    {
        long seen = wake_head_.cmpxchg(0, head);
        if (seen == head)
            break;
        head = seen;
    }

    /* The one pushed last is on top: turned over, they take their turns in
       the order they were woken */
    Task* order = nullptr;
    for (Task* task = reinterpret_cast<Task*>(head); task != nullptr;)
    {
        Task* below = task->wake_next_;
        task->wake_next_ = order;
        order = task;
        task = below;
    }

    while (order != nullptr)
    {
        Task* task = order;
        order = task->wake_next_;
        task->wake_next_ = nullptr;
        touched++;

        /* Off its CPU for good before it can be pushed: it was made
           Sleeping by the schedule() that switched it out on this CPU, whose
           next one this is */
        BUG_ON(task->task_queue_ != this);
        BUG_ON(task->state_.get() == Task::STATE_RUNNING);

        /* Woken before its deadline: off the sleep list too */
        if (!task->sleep_entry_.is_empty())
            task->sleep_entry_.remove_init();

        long was = task->run_state_.cmpxchg(Task::RUN_QUEUED, Task::RUN_WAKING);
        BUG_ON(was != Task::RUN_WAKING);
        enqueue(task);
    }
}

void TaskQueue::wake_due(ulong now, ulong& touched)
{
    while (!sleep_list_.is_empty())
    {
        Task* task = CONTAINING_RECORD(sleep_list_.flink, Task, sleep_entry_);
        ulong until = (ulong)task->sleep_until_.get();
        if (until > now)
            return;

        task->sleep_entry_.remove_init();
        touched++;

        /* Unless an unblock() won it first: then it is Waking, on the wake
           stack, and the next drain puts it on the run list */
        if (task->run_state_.cmpxchg(Task::RUN_QUEUED, Task::RUN_SLEEPING) == Task::RUN_SLEEPING)
        {
            task->wake_time_.set((long)until);
            enqueue(task);
        }
    }
}

void TaskQueue::release_idle(Task* curr)
{
    if (idle_task_ == nullptr || idle_task_->is_idle())
        return;

    /* Counted from now on, as any task that can run. Counted already -- on
       the CPU or on the run list, running something other than its idling
       -- it stays where it is; idling on the CPU, it is placed as a task
       waking, and takes the run list when it leaves the CPU; in its place
       as the last resort, it is placed and on the run list now. */
    Task* task = idle_task_;
    idle_task_ = nullptr;
    bool counted = idle_counted_;
    idle_counted_ = false;

    if (counted)
    {
        runnable_count_.inc();
    }
    else if (task == curr)
    {
        runnable_count_.inc();
        eevdf_.join_current(&task->sched_);
        task->sched_.exec_start = get_boot_time().get_value();
    }
    else
    {
        enqueue(task);
    }
}

void TaskQueue::check_stalls(ulong now)
{
    /* The run list is in no order of waiting: the one waiting longest is
       looked for, the list as long as this CPU has tasks that can run */
    Task* task = nullptr;
    auto& list = eevdf_.entries();
    for (auto entry = list.flink; entry != &list; entry = entry->flink)
    {
        Task* cand = task_of(CONTAINING_RECORD(entry, SchedEntity, entry));
        if (!cand->stall_reported_ && (task == nullptr || cand->queued_at_ < task->queued_at_))
            task = cand;
    }
    if (task == nullptr || now < task->queued_at_ + STALL_REPORT_NS)
        return;

    /* Said once the locks are down (report_stall): the trace path ends in the
       console and, with disklog=on, in a disk write, and none of that is for
       under the queue's lock */
    task->stall_reported_ = true;
    stall_reports.inc();
    stdlib::sn_printf(stall_name_, sizeof(stall_name_), "%s", task->get_name());
    stall_pid_ = task->pid_;
    stall_waited_ms_ = (now - task->queued_at_) / consts::NANO_SECS_IN_MS;
    stall_runnable_ = runnable_count_.get();
    stall_pending_ = true;
}

void TaskQueue::report_stall()
{
    if (likely(!stall_pending_))
        return;

    stall_pending_ = false;
    trace(0, "sched: %s (pid %lu) could run, and has waited %lu ms for cpu %lu, with %ld that can run there",
        stall_name_, stall_pid_, stall_waited_ms_, CpuTable::get_instance().get_current_cpu_id(), stall_runnable_);
}

void TaskQueue::schedule(Task* curr, ScheduleReason why)
{
    schedule_counter_.inc();

    ulong flags = hal::irq_save();
    u64 began = hal::read_cycle_counter();

    /* Whatever reschedule was put off until now, this is it */
    curr->preempt_pending_.set(0);
    lock_.lock();

    /* The timer fired since the last look: the queue is counted for
       SchedStats and looked at for a task left waiting too long */
    auto& cpu = CpuTable::get_instance().get_current_cpu();
    bool fired = cpu.local_timer_take_walk();

    /* Only the idle task to hand the CPU to: a task polling with
       yield_to_runnable, or one an interrupt landed on, keeps it instead */
    bool keep_over_idle = (why != ScheduleReason::Voluntary);

    Task* next = nullptr;
    bool next_queued = false;
    bool exited = false;
    long run = Task::RUN_QUEUED;
    ulong now = 0;
    ulong exited_retries = 0;
    ulong touched = 1;
    for (;;) {
        curr->lock_.lock();
        BUG_ON(task_list_.is_empty());

        /* curr charged for what it ran, gone from the count if it has
           exited, and the idle task in it or out of it as it runs something
           or idles: V is the queue's as it is now before anything joins it */
        now = get_boot_time().get_value();
        exited = (curr->state_.get() == Task::STATE_EXITED);
        release_idle(curr);
        account(curr, now, exited);

        /* What can run that could not at the last look: the woken, and the
           sleepers whose time has come, each placed by the lag it brings */
        drain_wakes(touched);
        if (!sleep_list_.is_empty())
            wake_due(now, touched);

        if (exited && curr->task_queue_ != nullptr)
        {
            BUG_ON(curr->task_queue_ != this);
            BUG_ON(curr->list_entry_.is_empty());
            curr->task_queue_ = nullptr;
            curr->list_entry_.remove_init();
            task_count_.dec();
            BUG_ON(task_count_.get() < 0);
            runnable_count_.dec();
            BUG_ON(runnable_count_.get() < 0);
        }

        /* On its CPU, a task is Queued, or Blocking on its way to sleep:
           only this CPU makes it anything else, and only as it leaves */
        run = curr->run_state_.get();
        BUG_ON(!exited && run != Task::RUN_QUEUED && run != Task::RUN_BLOCKING);
        bool runnable = !exited && run == Task::RUN_QUEUED;

        SchedEntity* current = eevdf_.current();
        if (!eevdf_.is_empty())
        {
            SchedEntity* best;
            if (runnable && why == ScheduleReason::Preempt && current != nullptr)
            {
                /* An interrupt's: curr among the candidates, and it keeps
                   the CPU unless another eligible task's deadline comes
                   sooner -- its slice done, or a wakeup owed more */
                best = eevdf_.pick(true);
                next = (best == current) ? nullptr : task_of(best);
            }
            else
            {
                /* curr lets the others go -- yields, sleeps, exits -- or is
                   the idle task idling, which keeps nothing from anybody:
                   the eligible one whose deadline is soonest of the rest. A
                   task that can still run goes behind them first. */
                if (runnable && current != nullptr)
                    eevdf_.yield();
                best = eevdf_.pick(false);
                next = task_of(best);
            }
            /* On the run list until curr has left the CPU (below): the
               queue takes it only then */
            next_queued = (next != nullptr);
        }
        else if (!(runnable && keep_over_idle) && idle_task_ != nullptr && idle_task_ != curr)
        {
            /* The idle task takes its turn only when there is no other turn
               to take, and not over a task an interrupt landed on, or one
               polling with yield_to_runnable, that can still run itself. It
               used to sit in the walk as an equal, so a reschedule -- and
               every raise of a softirq from an interrupt handler forces one,
               by way of the IPI -- could pick it over the task that had just
               been given work. It then halted the CPU, and the work waited
               for whatever interrupt came next. */
            next = idle_task_;
        }

        /* Nothing to switch to: curr keeps the CPU -- runnable, or blocking
           with no idle task to go to, when it comes back from schedule()
           still Blocking, unblocks itself, and looks again */
        if (next != nullptr || !exited)
            break;

        /* An exited task can't be returned to: drop the locks and wait
           for a runnable candidate (the idle task at the latest) */
        curr->lock_.unlock();
        lock_.unlock();
        if (++exited_retries > MAX_EXITED_RETRIES)
            PANIC("Schedule: exited task but no runnable candidate");
        cpu_relax();
        lock_.lock();
    }

    if (next != nullptr)
    {
        next->lock_.lock();

        /* Nothing that could not run is ever on the run list, and no task
           with preemption disabled is ever on one when a decision is made
           (switch_complete): what was passed over is now a report */
        BUG_ON(next->run_state_.get() != Task::RUN_QUEUED);
        BUG_ON(next->preempt_disable_counter_.get() != 0);

        /* curr leaves the CPU. Blocking, it goes to sleep -- unless an
           unblock() made it Queued since it looked, and then it has not.
           Runnable, it goes back on the run list -- the idle task too, while
           it runs the boot or a test and is counted, and only while it idles
           is its place its own (Task::set_idling). Kept apart whatever it ran,
           it used to be the last resort with the boot in it: a task the boot
           started on its own CPU that never blocked -- the ring selftest's
           producer -- took that CPU at the first tick and kept it, and the
           boot never got as far as starting the consumer the producer waited
           for. */
        if (!exited)
        {
            if (run == Task::RUN_BLOCKING &&
                curr->run_state_.cmpxchg(Task::RUN_SLEEPING, Task::RUN_BLOCKING) == Task::RUN_BLOCKING)
            {
                asleep(curr);
                curr->switched_out_runnable_ = false;
            }
            else
            {
                if (eevdf_.current() != nullptr)
                    requeue(curr);
                curr->switched_out_runnable_ = true;
            }
        }

        /* The idle task, as the last resort, is not counted and takes
           nothing */
        if (next_queued)
            eevdf_.take(&next->sched_);
    }

    /* This CPU's timer at the soonest sleeper's deadline -- curr's, if it has
       just gone to sleep until one -- so that the sleeper runs then, and not
       at the next tick. A deadline later than the timer has is no change. */
    if (!sleep_list_.is_empty())
        cpu.local_timer_wake((ulong)CONTAINING_RECORD(sleep_list_.flink, Task, sleep_entry_)->sleep_until_.get());

    /* And at the end of the slice of whoever runs now, so that the next
       decision comes when its deadline does */
    arm_slice(now);

    if (fired)
    {
        stats_.look((ulong)runnable_count_.get());
        check_stalls(now);
    }

    /* What deciding cost, the locks' waits included, and not the switch */
    stats_.decision(hal::read_cycle_counter() - began, touched);

    if (next == nullptr)
    {
        curr->update_runtime();
        curr->lock_.unlock();
        lock_.unlock();

        report_stall();

        /* The count down before interrupts come back on, not after: one
           waiting to be taken -- the IPI for a task woken meanwhile -- would
           find it still up, and its reschedule would wait for a preempt_enable
           that a task which has just kept the CPU may be a long way from. */
        curr->preempt_disable_counter_.dec();
        hal::irq_restore(flags);
        BUG_ON(curr->state_.get() == Task::STATE_EXITED);
        return;
    }

    switch_to(next, curr);
    hal::irq_restore(flags);
}

void TaskQueue::insert(Task* task)
{
    task->get();

    stdlib::AutoLock lock(lock_);
    stdlib::AutoLock lock2(task->lock_);

    BUG_ON(task->task_queue_ != nullptr);
    BUG_ON(!(task->list_entry_.is_empty()));
    BUG_ON(!task->sched_.entry.is_empty());
    BUG_ON(task->run_state_.get() != Task::RUN_QUEUED);

    task->task_queue_ = this;
    task_list_.insert_tail(&task->list_entry_);
    task_count_.inc();

    /* The idle task has a place of its own, apart from the run list. It is
       made one before it is inserted (Cpu::run), and is already running:
       Task::run inserts it from its own stack. */
    if (task->is_idle())
    {
        BUG_ON(idle_task_ != nullptr);
        idle_task_ = task;
        return;
    }

    enqueue(task);
}

void TaskQueue::remove(Task* task)
{
    {
        stdlib::AutoLock lock(lock_);
        stdlib::AutoLock lock2(task->lock_);

        BUG_ON(task->task_queue_ != this);
        BUG_ON(task->list_entry_.is_empty());

        /* Only a task that can run is moved (switch_complete): one asleep
           stays where its wakeups and its deadline go, and the idle task
           where it is */
        BUG_ON(task->run_state_.get() != Task::RUN_QUEUED);
        BUG_ON(task == idle_task_);
        BUG_ON(!task->sleep_entry_.is_empty());

        /* It takes its lag with it, to be placed by on the queue it goes to.
           Not current: displaced, and back on the run list, where the
           decision that switched it out put it (switch_complete moves it). */
        BUG_ON(eevdf_.current() == &task->sched_);
        eevdf_.leave(&task->sched_);
        runnable_count_.dec();
        BUG_ON(runnable_count_.get() < 0);

        task->task_queue_ = nullptr;
        task->list_entry_.remove_init();
        task_count_.dec();
        BUG_ON(task_count_.get() < 0);
    }

    task->put();
}

void TaskQueue::clear()
{
    stdlib::ListEntry task_list;
    {
        stdlib::AutoLock lock(lock_);
        task_list.move_tail_list(&task_list_);
        task_count_.set(0);
        runnable_count_.set(0);
        idle_task_ = nullptr;
        wake_head_.set(0);

        /* What waits on the run and sleep lists is on the task list too, and
           goes with it: unlinked, so that no task is left pointing into
           lists that are gone */
        eevdf_.clear();
        idle_counted_ = false;
        while (!sleep_list_.is_empty())
            sleep_list_.remove_head()->init();
    }

    if (task_list.is_empty())
        return;

    while (!task_list.is_empty())
    {
        Task* task = CONTAINING_RECORD(task_list.remove_head(), Task, list_entry_);
        stdlib::AutoLock lock2(task->lock_);
        BUG_ON(task->task_queue_ != this);
        task->task_queue_ = nullptr;
        task->put();
    }

    trace(0, "TaskQueue 0x%p counters: sched %lu switch context %lu",
        this, schedule_counter_.get(), switch_context_counter_.get());
}

TaskQueue::~TaskQueue()
{
    clear();
}

long TaskQueue::get_task_count()
{
    return task_count_.get();
}

long TaskQueue::get_runnable_count()
{
    return runnable_count_.get();
}

long TaskQueue::get_switch_context_counter()
{
    return switch_context_counter_.get();
}

static void schedule_current(ScheduleReason why)
{
    if (unlikely(!preempt_is_on()))
    {
        return;
    }

    Task *curr = Task::get_current_task();
    if (!curr)
    {
        static long diag_once = 0;
        if (atomic_read_and_inc(&diag_once) == 0)
            Task::diagnose_get_current_task();
        return;
    }

    curr->preempt_disable_counter_.inc();
    if (curr->preempt_disable_counter_.get() > 1)
    {
        /* Preemption is off -- a spinlock, most likely -- so not now; but
           remembered, and made the moment the count is back to zero (see
           preempt_enable). It used to be dropped, and with it whatever the
           reschedule was for: the IPI SoftIrq::raise sends to run a softirq
           task it has just woken, landing on an idle task inside the lock
           reap_exited takes, left that idle task to halt with the softirq task
           runnable -- and every raise after it found the bit already up and
           sent nothing. The CPU slept on its work until its next tick. */
        curr->preempt_pending_.set(1);
        preempts_deferred.inc();
        if (curr->is_idle())
            preempts_deferred_idle.inc();

        stdlib::AutoLock lock(curr->lock_);
        curr->update_runtime();
        curr->preempt_disable_counter_.dec();
        return;
    }

    curr->task_queue_->schedule(curr, why);
}

void schedule()
{
    schedule_current(ScheduleReason::Voluntary);
}

void preempt()
{
    schedule_current(ScheduleReason::Preempt);
}

void yield_to_runnable()
{
    schedule_current(ScheduleReason::Yield);
}

bool TaskQueue::set_nice(Task* task, long nice)
{
    /* The task's lock too, in the scheduler's order: TaskQueue is written
       under it everywhere, Clear's included, which does not hold this one */
    stdlib::AutoLock lock(lock_);
    stdlib::AutoLock lock2(task->lock_);
    if (task->task_queue_ != this)
        return false;

    /* Its weight from now on: V and every eligibility are reckoned from
       scratch at the next decision, and it is charged at it from then */
    task->sched_.set_nice(nice);
    return true;
}

ulong TaskQueue::view(EntityView* out, ulong max)
{
    stdlib::AutoLock lock(lock_);

    SchedEntity* curr = eevdf_.current();
    u64 v = eevdf_.v();
    ulong count = 0;

    /* curr first, then the list */
    SchedEntity* first = curr;
    auto& list = eevdf_.entries();
    stdlib::ListEntry* entry = list.flink;
    for (;;)
    {
        SchedEntity* se;
        if (first != nullptr)
        {
            se = first;
            first = nullptr;
        }
        else if (entry != &list)
        {
            se = CONTAINING_RECORD(entry, SchedEntity, entry);
            entry = entry->flink;
        }
        else
        {
            break;
        }

        if (count == max)
            break;

        Task* task = task_of(se);
        EntityView& e = out[count++];
        stdlib::sn_printf(e.name, sizeof(e.name), "%s", task->get_name());
        e.pid = task->pid_;
        e.nice = se->nice;
        e.slice_ns = se->slice_ns;
        e.lag = (s64)(v - se->vruntime);
        e.deadline = (s64)(se->deadline - v);
        e.eligible = eevdf_.is_eligible(se);
        e.on_cpu = (se == curr);
    }
    return count;
}

bool TaskQueue::set_slice(Task* task, ulong slice_ns)
{
    stdlib::AutoLock lock(lock_);
    stdlib::AutoLock lock2(task->lock_);
    if (task->task_queue_ != this)
        return false;

    /* The next deadline it is given is a slice of this past its VRuntime */
    task->sched_.set_slice(slice_ns);
    return true;
}

/* The queue a task is on is read under the task's lock, and looked at again
   under both, the queue's first: the order the scheduler takes them in, so
   the task's is let go before the queue's is taken. A task on no queue --
   not started, or exited -- is set under its own lock, which Insert takes
   too. */
static TaskQueue* queue_of(Task* task)
{
    stdlib::AutoLock lock(task->lock_);
    return task->task_queue_;
}

void set_task_nice(Task* task, long nice)
{
    for (;;)
    {
        TaskQueue* queue = queue_of(task);
        if (queue == nullptr)
        {
            stdlib::AutoLock lock(task->lock_);
            if (task->task_queue_ == nullptr)
            {
                task->sched_.set_nice(nice);
                return;
            }
            continue;
        }
        if (queue->set_nice(task, nice))
            return;
    }
}

void set_task_slice(Task* task, ulong slice_ns)
{
    for (;;)
    {
        TaskQueue* queue = queue_of(task);
        if (queue == nullptr)
        {
            stdlib::AutoLock lock(task->lock_);
            if (task->task_queue_ == nullptr)
            {
                task->sched_.set_slice(slice_ns);
                return;
            }
            continue;
        }
        if (queue->set_slice(task, slice_ns))
            return;
    }
}

/* Sleep blocks. The task is off its queue's run list until its time has
   come, on the queue's sleep list, soonest first, and the scheduling point
   its CPU's timer makes at the deadline -- armed for it as the task goes to
   sleep -- or any sooner one that finds the time has come puts it back
   (TaskQueue::wake_due). That used to be the tick's, 100 times a second, at the
   latest, and a sleep of a millisecond took ten. It used to poll, too, a loop
   around schedule() that left it runnable: alone on its CPU it let the CPU
   halt until the tick, but two on one CPU handed it to each other without
   end and the idle task never ran. Two idle hypervisor guests, each vCPU
   asleep until its guest's next timer edge, kept a CPU of the AX41 busy that
   way between them, 46% each.

   Where a task cannot block -- before preemption is on, off a task's stack,
   in a CPU's idle task (which must stay runnable, see Event::wait), with
   preemption disabled -- it polls as it always did. */
void sleep(ulong nano_secs)
{
    ulong start = get_boot_time().get_value();
    ulong until = (nano_secs > ~0UL - start) ? ~0UL : start + nano_secs;

    Task* self = preempt_is_on() ? Task::try_get_current_task() : nullptr;
    bool block = (self != nullptr && !self->is_idle() &&
                  self->preempt_disable_counter_.get() == 0);

    while (get_boot_time().get_value() < until)
    {
        if (!block)
        {
            schedule();
            continue;
        }

        /* Interrupts off from the moment the task is marked until it is
           unmarked, as in Event::wait: a tick landing in between would
           switch it out as a sleeper -- harmless, it is one -- but then
           return it here to go through the schedule() below a second time.
           The task comes back from schedule() with interrupts still off,
           which the switch saves and restores per task. */
        ulong flags = hal::irq_save();
        self->sleep_until_.set((long)until);
        self->block();
        schedule();
        self->unblock();
        self->sleep_until_.set(0);
        hal::irq_restore(flags);
    }
}

}