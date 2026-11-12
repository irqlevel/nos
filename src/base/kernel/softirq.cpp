#include "softirq.h"
#include "task.h"
#include "klog.h"
#include "sched.h"
#include "cpu.h"
#include "time.h"
#include "tracepoint.h"

#include <hal/cpu.h>
#include <hal/irqchip.h>
#include <mm/new.h>
#include <lib/stdlib.h>
#include <include/const.h>

namespace kernel
{

SoftIrq::SoftIrq()
    : running_(0)
    , ready_(0)
{
    stdlib::mem_set(handlers_, 0, sizeof(handlers_));
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        cpu_states_[i].pending.set(0);
        cpu_states_[i].task_ptr = nullptr;
        for (ulong type = 0; type < MAX_TYPES; type++)
            cpu_states_[i].raised_at[type] = 0;
        stopped_[i] = nullptr;
    }
}

SoftIrq::~SoftIrq()
{
    /* The references stop() kept, let go of here: the kernel's statics go
       with every other CPU parked and this one's interrupts off, so nothing
       is left that could be holding one of these pointers */
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (stopped_[i] != nullptr)
        {
            stopped_[i]->put();
            stopped_[i] = nullptr;
        }
    }
}

bool SoftIrq::init()
{
    ulong cpu_mask = CpuTable::get_instance().get_running_cpus();

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (!(cpu_mask & (1UL << i)))
            continue;

        char name[TASK_NAME_LEN];
        stdlib::sn_printf(name, sizeof(name), "softirq/%lu", i);
        Task* task = mm::talloc<Task, TAG>(name);
        if (!task)
        {
            stop();
            return false;
        }

        /* Its CPU's softirqs: on that CPU alone, for good */
        task->pin_to_cpu(i);
        if (!task->start(&SoftIrq::task_func, &cpu_states_[i]))
        {
            task->put();
            stop();
            return false;
        }

        cpu_states_[i].task_ptr = task;
    }

    ready_.set(1);

    /* What was raised while the tasks were being made is in the pending
       bits and woke nobody (raise_mask, before ready_) -- and a task that
       had already looked and blocked sleeps on it: every later raise of
       that type finds its bit up and wakes nobody either, until some other
       type is raised there. On arm64 a NIC is live long before this. Each
       CPU with something pending is looked at once, now that a raise
       wakes. */
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        Task* task = cpu_states_[i].task_ptr;
        if (task == nullptr || cpu_states_[i].pending.get() == 0)
            continue;

        task->unblock();
        hal::send_ipi(i, CpuTable::IPI_VECTOR);
    }

    klog(0, "SoftIrq: initialized");
    return true;
}

void SoftIrq::stop()
{
    ready_.set(0);

    /* Every task told before any is waited for: a task still running ends a
       handler by waking the others' (run), and is not to find one of them
       on its way out from under it. */
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        Task* task = cpu_states_[i].task_ptr;
        if (task == nullptr)
            continue;

        task->set_stopping();

        /* Publish-then-wake, as everywhere else here: a blocked task is out
           of the scheduler's walk and would never reach the top of its loop
           to notice the flag, and the wait() below would then spin for
           good. The task looks at the flag again after it blocks (run). */
        task->unblock();
    }

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        Task* task = cpu_states_[i].task_ptr;
        if (task != nullptr)
            task->wait();
    }

    /* Unpublished, and kept. raise_mask and tick_kick read these pointers
       with no lock -- from an interrupt, or from a task that may be switched
       away between its read and its use -- so there is no moment here at
       which this could be known to be the last look at a task: let go of
       here, as it used to be, the last reference could go under a CPU that
       had just read the pointer. The task has exited; its Task goes when
       the kernel's statics do (~SoftIrq). An unblock() of it until then
       finds it neither blocking nor asleep, and does nothing. */
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        Task* task = cpu_states_[i].task_ptr;
        if (task == nullptr)
            continue;

        cpu_states_[i].task_ptr = nullptr;
        BUG_ON(stopped_[i] != nullptr);
        stopped_[i] = task;
    }
}

bool SoftIrq::is_pending(ulong type)
{
    if (type >= MAX_TYPES)
        return false;

    ulong cpu = CpuTable::get_instance().get_current_cpu_id();
    if (cpu >= MAX_CPUS)
        return false;

    return cpu_states_[cpu].pending.test_bit(type);
}

namespace
{

/* softirq_raise's record of each type in `types`: what the raise did */
void emit_raised(ulong types, tp::SoftirqHow how)
{
    for (ulong rest = types; rest != 0; rest &= rest - 1)
        tp::emit_softirq_raise((ulong)__builtin_ctzl(rest), how);
}

}

void SoftIrq::raise_mask(ulong mask)
{
    /* A bit past the types is none */
    mask &= ALL_TYPES;
    if (mask == 0)
        return;

    ulong cpu = CpuTable::get_instance().get_current_cpu_id();
    if (BUG_ON(cpu >= MAX_CPUS))
        return;

    CpuState& state = cpu_states_[cpu];

    /* Each type's bit, and which of them this raise made pending: those a
       pass of the task is owed for. One already pending joins the pass it
       is owed already. */
    ulong fresh = 0;
    for (ulong rest = mask; rest != 0; rest &= rest - 1)
    {
        ulong type = (ulong)__builtin_ctzl(rest);
        if (make_pending(state, type))
            fresh |= 1UL << type;
    }
    if (fresh == 0)
        return;

    if (!ready_.get())
    {
        if (unlikely(tp::on(tp::Id::SoftirqRaise)))
            emit_raised(fresh, tp::SoftirqNone);
        return;
    }

    /* The pending bits are up; now make sure the task will be looked at. This
       is unconditional and comes before the check below: between the Block()
       in Run() and the context switch that follows it the task is already out
       of the scheduler's walk while still being the current task, so a raise
       landing in that window would take the early return below and leave the
       work to sit until the tick repaired it. */
    Task* target = state.task_ptr;
    bool was_blocked = (target != nullptr) && target->is_blocked();

    /* The raise's records before the hand-off: from the unblock on, the task
       may run a handler -- and write its softirq_entry -- before anything
       here does. What it will do, decided as below: left to the running
       task, or the task kicked. */
    if (unlikely(tp::on(tp::Id::SoftirqRaise)))
        emit_raised(fresh, (!was_blocked && Task::try_get_current_task() == target)
            ? tp::SoftirqRunning : tp::SoftirqKicked);

    if (target != nullptr)
        target->unblock();

    /* The softirq task raising work for its own CPU needs no kick -- unless
       it was blocked, which is the one case where "it is running" is true and
       useless. In the few instructions between Block() and the schedule()
       that follows it, the task is the current task and about to hand the CPU
       to idle; skipping the kick there would leave it runnable but
       unscheduled until the next tick, which at line rate is ten milliseconds
       of receive going nowhere. Blocked means kick, whoever is current.

       Otherwise it really is running; its loop looks at Pending again the
       moment the handler returns.

       The IPI would be useless there: an interrupt, for a reschedule the task
       does not need. It used to be worse than useless. Before Cpu::ipi ended
       in Preempt() rather than schedule(), it took the CPU away from the one
       task that had work to do and gave it to the idle task, and that task
       did not run again until the next tick. A handler that asks for another
       pass, which is how the receive poll yields on its budget, was getting
       exactly one pass per tick: 64 frames at 100 Hz, a ceiling of 6600
       packets a second no matter how many arrived. */
    if (!was_blocked && Task::try_get_current_task() == target)
        return;

    /* Otherwise the kick is what gets the task scheduled: without it an idle
       CPU would not run its softirq task until the next timer tick. Sent
       directly -- CpuTable::send_ipi takes spinlocks that are not safe in
       hard IRQ context. One for every type this raise made pending: a NIC's
       interrupt with work for both its queues raises both at once, where a
       raise each sent two -- the second after the first had unblocked the
       task, an IPI to this CPU it already had on its way. */
    hal::send_ipi(cpu, CpuTable::IPI_VECTOR);
}

bool SoftIrq::make_pending(CpuState& state, ulong type)
{
    /* When the work came, for the wait its run's softirq_entry records:
       only while that is on, by a raise that finds the type not pending --
       before its bit, so that the run that takes the bit finds the stamp --
       and only into a stamp the last run zeroed. `stamp` the one this raise
       wrote, 0 for none: the counter is never 0 once the machine is up. */
    u64 stamp = 0;
    if (unlikely(tp::on(tp::Id::SoftirqEntry)) && !state.pending.test_bit(type))
    {
        u64 none = 0;
        u64 now = hal::read_cycle_counter_unordered();
        if (__atomic_compare_exchange_n(&state.raised_at[type], &none, now, false,
                __ATOMIC_RELAXED, __ATOMIC_RELAXED))
            stamp = now;
    }

    if (!state.pending.set_bit(type))
        return true;

    /* Pending already, by a raise that came between this one's look and its
       bit -- an interrupt's, landing on a task's raise: the stamp is that
       one's to write, and this one's is taken back, so that only the raise
       that made the type pending leaves one, and no stamp outlives the
       pending it was for. One that found its own taken back is left as it
       is; the run says it was not timed. */
    if (unlikely(stamp != 0))
    {
        u64 mine = stamp;
        __atomic_compare_exchange_n(&state.raised_at[type], &mine, 0, false,
            __ATOMIC_RELAXED, __ATOMIC_RELAXED);
    }
    if (unlikely(tp::on(tp::Id::SoftirqRaise)))
        tp::emit_softirq_raise(type, tp::SoftirqPending);
    return false;
}

bool SoftIrq::register_handler(ulong type, void (*handler)(void* ctx), void* ctx)
{
    if (type >= MAX_TYPES || handler == nullptr)
        return false;

    /* The type taken, by one registration at most: the pair is then
       written, and only then published -- a pass on any CPU reads it only
       after it sees it ready (ready_handler) */
    Handler& h = handlers_[type];
    ulong expected = HANDLER_FREE;
    if (!__atomic_compare_exchange_n(&h.state, &expected, HANDLER_WRITING, false,
            __ATOMIC_ACQUIRE, __ATOMIC_RELAXED))
        return false;
    h.func = handler;
    h.ctx = ctx;
    __atomic_store_n(&h.state, HANDLER_READY, __ATOMIC_RELEASE);
    return true;
}

const SoftIrq::Handler* SoftIrq::ready_handler(ulong type) const
{
    if (type >= MAX_TYPES)
        return nullptr;
    const Handler& h = handlers_[type];
    if (__atomic_load_n(&h.state, __ATOMIC_ACQUIRE) != HANDLER_READY)
        return nullptr;
    return &h;
}

void SoftIrq::task_func(void* ctx)
{
    SoftIrq::get_instance().run(*static_cast<CpuState*>(ctx));
}

void SoftIrq::run_handler(CpuState& state, ulong type, u64 raised)
{
    u64 began = hal::read_cycle_counter_unordered();
    if (tp::on(tp::Id::SoftirqEntry))
    {
        /* 0 for none: raised while the event was off. The raise was read on
           this CPU, or on one whose counter keeps step with it -- never
           after this read, but held to it should one say otherwise */
        ulong wait_ns = (raised != 0 && began > raised) ? cycle_counter_delta_to_ns(began - raised) : 0;
        tp::emit_softirq_entry(type, wait_ns);
    }

    /* The caller found it ready (ready_handler), and a ready one stays */
    const Handler* h = ready_handler(type);
    if (h != nullptr)
        h->func(h->ctx);

    /* Raised again while it ran -- an interrupt's, or the handler's own on
       its budget -- so that this loop runs it again */
    if (tp::on(tp::Id::SoftirqExit))
    {
        u64 ended = hal::read_cycle_counter_unordered();
        tp::emit_softirq_exit(type, (ended > began) ? cycle_counter_delta_to_ns(ended - began) : 0,
            state.pending.test_bit(type));
    }
}

void SoftIrq::tick_kick(ulong cpu)
{
    if (cpu >= MAX_CPUS)
        return;

    /* Only when there is something to wake for: a CPU with nothing pending
       should stay in the idle task's Hlt, which is the whole point of the
       task blocking rather than spinning. */
    if (cpu_states_[cpu].pending.get() == 0)
        return;

    Task* target = cpu_states_[cpu].task_ptr;
    if (target != nullptr)
        target->unblock();
}

bool SoftIrq::has_runnable_work(CpuState& state)
{
    for (ulong i = 0; i < MAX_TYPES; i++)
    {
        if (state.pending.test_bit(i) && !running_.test_bit(i))
            return true;
    }

    return false;
}

void SoftIrq::run(CpuState& state)
{
    auto* task = Task::get_current_task();
    ulong self = (ulong)(&state - &cpu_states_[0]);

    while (!task->is_stopping())
    {
        bool handled = false;

        for (ulong i = 0; i < MAX_TYPES; i++)
        {
            if (!state.pending.test_bit(i))
                continue;

            if (running_.set_bit(i))
            {
                /* Type is being handled on another CPU: keep the
                   pending bit set and retry on the next pass. */
                continue;
            }

            /* The stamp of the raise that made it pending, if softirq_entry
               wants it -- and zeroed for the next raise whether it does or
               not, so that no stamp outlives its pending: read while the bit
               is still up, when no raise writes one (SoftIrq::raise_mask) */
            u64 raised = 0;
            if (unlikely(tp::on(tp::Id::SoftirqEntry)))
                raised = __atomic_load_n(&state.raised_at[i], __ATOMIC_RELAXED);
            __atomic_store_n(&state.raised_at[i], 0, __ATOMIC_RELAXED);

            state.pending.clear_bit(i);

            const Handler* h = ready_handler(i);
            if (h != nullptr)
            {
                if (unlikely(tp::on(tp::Id::SoftirqEntry) || tp::on(tp::Id::SoftirqExit)))
                    run_handler(state, i, raised);
                else
                    h->func(h->ctx);
                handled = true;
            }

            running_.clear_bit(i);

            /* Kick CPUs which lost the Running race while we held it,
               so their pending work is not delayed until the next
               timer tick. */
            for (ulong c = 0; c < MAX_CPUS; c++)
            {
                if (c == self || !cpu_states_[c].pending.test_bit(i))
                    continue;

                /* Clear the block before the kick, in the same order Raise
                   uses: that CPU's task may be sitting out of the scheduler's
                   walk, and an IPI alone would not put it back in -- the work
                   would then wait for the tick to repair it. */
                Task* other = cpu_states_[c].task_ptr;
                if (other != nullptr)
                    other->unblock();

                hal::send_ipi(c, CpuTable::IPI_VECTOR);
            }
        }

        if (!handled)
        {
            /* Reaping is the idle task's job, and the idle task is now the
               scheduler's last resort -- so on a CPU with something always
               runnable it may not get a turn for a while. This costs nothing
               here (the list is empty unless a task exited on this CPU) and
               it runs in task context with interrupts on, which is what
               freeing a stack requires: it triggers a TLB shootdown that
               waits for every other CPU to answer. */
            CpuTable::get_instance().get_current_cpu().get_task_queue().reap_exited();

            /* Nothing pending: ask the scheduler to pass over this task
               until someone has work for it. This used to be sleep(1ms),
               which despite the name never blocked -- it spun on
               get_boot_time() and schedule() for a whole millisecond without
               ever looking at Pending again, so a frame arriving just after
               it started waited out the rest, and the CPU never reached the
               idle task's Hlt.

               The order below is what makes it safe. The flag goes up
               first and the re-check reads only after; both CPUs that can
               wake this task do the mirror image, publishing the thing worth
               waking for before clearing the flag. raise() sets the pending
               bit, then unblocks. A CPU finishing a handler clears the
               Running bit, then unblocks every CPU still holding that type
               pending. Either way a wakeup landing anywhere in this window is
               seen by the re-check or clears the flag before schedule() can
               act on it. Both sides store sequentially consistent (lock bts
               on x86, stlr on arm64), which is what the pair needs -- relaxed
               ordering would hold on x86 and fail on arm64 only.

               And the question the re-check asks is whether there is work
               this CPU can actually take, not merely whether Pending is
               empty: a type held by another CPU leaves the bit set here, and
               testing Pending alone would refuse to sleep and spin for the
               length of that handler. */
            /* And interrupts off across it, as Event::wait has them: the tick
               and every IPI end in a schedule() of their own, and one landing
               between the flag going up and the re-check switches this task
               out blocked before it has looked -- while a raise that finds
               the bit already pending returns without a wakeup. Only
               tick_kick came back for it then, a tick later: ten milliseconds
               of a receive path going nowhere, a few times in a thousand
               round trips. schedule() keeps interrupts per task, so the task
               comes back with them still off. */
            ulong flags = hal::irq_save();
            task->block();

            /* And not asked to stop: stop() raises the flag and then
               unblocks, the same publish-then-wake, and a task that had
               passed the loop's test just before the two would otherwise
               sleep here on a flag already up -- its unblock having found
               it not yet blocked -- with stop() waiting for it for good. */
            if (!has_runnable_work(state) && !task->is_stopping())
                schedule();

            /* Running again, so the flag has no business still being up. The
               waker normally cleared it; this covers the re-check above
               finding work, and the case where schedule() had no one else to
               run and returned to us still blocked. Cleared before interrupts
               come back on: a tick held off until then would find it up. */
            task->unblock();

            hal::irq_restore(flags);
        }
    }
}

}
