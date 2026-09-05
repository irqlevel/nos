#include "softirq.h"
#include "task.h"
#include "trace.h"
#include "sched.h"
#include "cpu.h"

#include <hal/irqchip.h>
#include <mm/new.h>
#include <lib/stdlib.h>
#include <include/const.h>

namespace Kernel
{

SoftIrq::SoftIrq()
    : Running(0)
    , Ready(0)
{
    Stdlib::MemSet(Handlers, 0, sizeof(Handlers));
    for (ulong i = 0; i < MaxCpus; i++)
    {
        CpuStates[i].Pending.Set(0);
        CpuStates[i].TaskPtr = nullptr;
    }
}

SoftIrq::~SoftIrq()
{
}

bool SoftIrq::Init()
{
    ulong cpuMask = CpuTable::GetInstance().GetRunningCpus();

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (!(cpuMask & (1UL << i)))
            continue;

        Task* task = Mm::TAlloc<Task, Tag>("softirq/%u", i);
        if (!task)
        {
            Stop();
            return false;
        }

        task->SetCpuAffinity(1UL << i);
        if (!task->Start(&SoftIrq::TaskFunc, &CpuStates[i]))
        {
            task->Put();
            Stop();
            return false;
        }

        CpuStates[i].TaskPtr = task;
    }

    Ready.Set(1);

    Trace(0, "SoftIrq: initialized");
    return true;
}

void SoftIrq::Stop()
{
    Ready.Set(0);

    for (ulong i = 0; i < MaxCpus; i++)
    {
        Task* task = CpuStates[i].TaskPtr;
        if (task)
        {
            task->SetStopping();

            /* Publish-then-wake, as everywhere else here: a blocked task is
               out of the scheduler's walk and would never reach the top of
               its loop to notice the flag, and the Wait() below would then
               spin for good. */
            task->Unblock();

            task->Wait();

            /* Unpublished before the reference goes: Raise() and TickKick()
               both dereference this pointer without a lock, and Put() may be
               the last one. */
            CpuStates[i].TaskPtr = nullptr;
            task->Put();
        }
    }
}

bool SoftIrq::IsPending(ulong type)
{
    if (type >= MaxTypes)
        return false;

    ulong cpu = CpuTable::GetInstance().GetCurrentCpuId();
    if (cpu >= MaxCpus)
        return false;

    return CpuStates[cpu].Pending.TestBit(type);
}

void SoftIrq::Raise(ulong type)
{
    if (type >= MaxTypes)
        return;

    ulong cpu = CpuTable::GetInstance().GetCurrentCpuId();
    if (BugOn(cpu >= MaxCpus))
        return;

    if (CpuStates[cpu].Pending.SetBit(type))
        return; /* was already pending */

    if (!Ready.Get())
        return;

    /* The pending bit is up; now make sure the task will be looked at. This
       is unconditional and comes before the check below: between the Block()
       in Run() and the context switch that follows it the task is already out
       of the scheduler's walk while still being the current task, so a raise
       landing in that window would take the early return below and leave the
       work to sit until the tick repaired it. */
    Task* target = CpuStates[cpu].TaskPtr;
    bool wasBlocked = (target != nullptr) && target->IsBlocked();
    if (target != nullptr)
        target->Unblock();

    /* The softirq task raising work for its own CPU needs no kick -- unless
       it was blocked, which is the one case where "it is running" is true and
       useless. In the few instructions between Block() and the Schedule()
       that follows it, the task is the current task and about to hand the CPU
       to idle; skipping the kick there would leave it runnable but
       unscheduled until the next tick, which at line rate is ten milliseconds
       of receive going nowhere. Blocked means kick, whoever is current.

       Otherwise it really is running; its loop looks at Pending again the
       moment the handler returns.

       The IPI would be worse than useless there. It is delivered as a
       reschedule -- Cpu::IPI ends in Schedule() -- so it takes the CPU away
       from the one task that has work to do, and that task does not run
       again until the next tick. A handler that asks for another pass, which
       is how the receive poll yields on its budget, was therefore getting
       exactly one pass per tick: 64 frames at 100 Hz, a ceiling of 6600
       packets a second no matter how many arrived. */
    if (!wasBlocked && Task::TryGetCurrentTask() == target)
        return;

    /* Otherwise the kick is what gets the task scheduled: without it an idle
       CPU would not run its softirq task until the next timer tick. Sent
       directly -- CpuTable::SendIPI takes spinlocks that are not safe in
       hard IRQ context. */
    Hal::SendIpi(cpu, CpuTable::IPIVector);
}

void SoftIrq::Register(ulong type, void (*handler)(void* ctx), void* ctx)
{
    if (type >= MaxTypes)
        return;

    Handlers[type].Func = handler;
    Handlers[type].Ctx = ctx;
}

void SoftIrq::TaskFunc(void* ctx)
{
    SoftIrq::GetInstance().Run(*static_cast<CpuState*>(ctx));
}

void SoftIrq::TickKick(ulong cpu)
{
    if (cpu >= MaxCpus)
        return;

    /* Only when there is something to wake for: a CPU with nothing pending
       should stay in the idle task's Hlt, which is the whole point of the
       task blocking rather than spinning. */
    if (CpuStates[cpu].Pending.Get() == 0)
        return;

    Task* target = CpuStates[cpu].TaskPtr;
    if (target != nullptr)
        target->Unblock();
}

bool SoftIrq::HasRunnableWork(CpuState& state)
{
    for (ulong i = 0; i < MaxTypes; i++)
    {
        if (state.Pending.TestBit(i) && !Running.TestBit(i))
            return true;
    }

    return false;
}

void SoftIrq::Run(CpuState& state)
{
    auto* task = Task::GetCurrentTask();
    ulong self = (ulong)(&state - &CpuStates[0]);

    while (!task->IsStopping())
    {
        bool handled = false;

        for (ulong i = 0; i < MaxTypes; i++)
        {
            if (!state.Pending.TestBit(i))
                continue;

            if (Running.SetBit(i))
            {
                /* Type is being handled on another CPU: keep the
                   pending bit set and retry on the next pass. */
                continue;
            }

            state.Pending.ClearBit(i);

            if (Handlers[i].Func)
            {
                Handlers[i].Func(Handlers[i].Ctx);
                handled = true;
            }

            Running.ClearBit(i);

            /* Kick CPUs which lost the Running race while we held it,
               so their pending work is not delayed until the next
               timer tick. */
            for (ulong c = 0; c < MaxCpus; c++)
            {
                if (c == self || !CpuStates[c].Pending.TestBit(i))
                    continue;

                /* Clear the block before the kick, in the same order Raise
                   uses: that CPU's task may be sitting out of the scheduler's
                   walk, and an IPI alone would not put it back in -- the work
                   would then wait for the tick to repair it. */
                if (CpuStates[c].TaskPtr != nullptr)
                    CpuStates[c].TaskPtr->Unblock();

                Hal::SendIpi(c, CpuTable::IPIVector);
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
            CpuTable::GetInstance().GetCurrentCpu().GetTaskQueue().ReapExited();

            /* Nothing pending: ask the scheduler to pass over this task
               until someone has work for it. This used to be Sleep(1ms),
               which despite the name never blocked -- it spun on
               GetBootTime() and Schedule() for a whole millisecond without
               ever looking at Pending again, so a frame arriving just after
               it started waited out the rest, and the CPU never reached the
               idle task's Hlt.

               The order below is what makes it safe. The flag goes up
               first and the re-check reads only after; both CPUs that can
               wake this task do the mirror image, publishing the thing worth
               waking for before clearing the flag. Raise() sets the pending
               bit, then unblocks. A CPU finishing a handler clears the
               Running bit, then unblocks every CPU still holding that type
               pending. Either way a wakeup landing anywhere in this window is
               seen by the re-check or clears the flag before Schedule() can
               act on it. Both sides store sequentially consistent (lock bts
               on x86, stlr on arm64), which is what the pair needs -- relaxed
               ordering would hold on x86 and fail on arm64 only.

               And the question the re-check asks is whether there is work
               this CPU can actually take, not merely whether Pending is
               empty: a type held by another CPU leaves the bit set here, and
               testing Pending alone would refuse to sleep and spin for the
               length of that handler. */
            task->Block();

            if (!HasRunnableWork(state))
                Schedule();

            /* Running again, so the flag has no business still being up. The
               waker normally cleared it; this covers the re-check above
               finding work, and the case where Schedule() had no one else to
               run and returned to us still blocked. */
            task->Unblock();
        }
    }
}

}
