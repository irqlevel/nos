#include "softirq.h"
#include "task.h"
#include "trace.h"
#include "sched.h"
#include "cpu.h"

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
    }
}

SoftIrq::~SoftIrq()
{
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

        task->set_cpu_affinity(1UL << i);
        if (!task->start(&SoftIrq::task_func, &cpu_states_[i]))
        {
            task->put();
            stop();
            return false;
        }

        cpu_states_[i].task_ptr = task;
    }

    ready_.set(1);

    trace(0, "SoftIrq: initialized");
    return true;
}

void SoftIrq::stop()
{
    ready_.set(0);

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        Task* task = cpu_states_[i].task_ptr;
        if (task)
        {
            task->set_stopping();

            /* Publish-then-wake, as everywhere else here: a blocked task is
               out of the scheduler's walk and would never reach the top of
               its loop to notice the flag, and the wait() below would then
               spin for good. */
            task->unblock();

            task->wait();

            /* Unpublished before the reference goes: raise() and tick_kick()
               both dereference this pointer without a lock, and put() may be
               the last one. */
            cpu_states_[i].task_ptr = nullptr;
            task->put();
        }
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

void SoftIrq::raise(ulong type)
{
    if (type >= MAX_TYPES)
        return;

    ulong cpu = CpuTable::get_instance().get_current_cpu_id();
    if (BUG_ON(cpu >= MAX_CPUS))
        return;

    if (cpu_states_[cpu].pending.set_bit(type))
        return; /* was already pending */

    if (!ready_.get())
        return;

    /* The pending bit is up; now make sure the task will be looked at. This
       is unconditional and comes before the check below: between the Block()
       in Run() and the context switch that follows it the task is already out
       of the scheduler's walk while still being the current task, so a raise
       landing in that window would take the early return below and leave the
       work to sit until the tick repaired it. */
    Task* target = cpu_states_[cpu].task_ptr;
    bool was_blocked = (target != nullptr) && target->is_blocked();
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
       hard IRQ context. */
    hal::send_ipi(cpu, CpuTable::IPI_VECTOR);
}

void SoftIrq::register_handler(ulong type, void (*handler)(void* ctx), void* ctx)
{
    if (type >= MAX_TYPES)
        return;

    handlers_[type].func = handler;
    handlers_[type].ctx = ctx;
}

void SoftIrq::task_func(void* ctx)
{
    SoftIrq::get_instance().run(*static_cast<CpuState*>(ctx));
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

            state.pending.clear_bit(i);

            if (handlers_[i].func)
            {
                handlers_[i].func(handlers_[i].ctx);
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
                if (cpu_states_[c].task_ptr != nullptr)
                    cpu_states_[c].task_ptr->unblock();

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

            if (!has_runnable_work(state))
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
