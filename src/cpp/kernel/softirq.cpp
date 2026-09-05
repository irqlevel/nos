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
            task->Wait();
            task->Put();
            CpuStates[i].TaskPtr = nullptr;
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

    /* The softirq task raising work for its own CPU needs no kick. It is
       running; its loop looks at Pending again the moment the handler
       returns.

       The IPI would be worse than useless there. It is delivered as a
       reschedule -- Cpu::IPI ends in Schedule() -- so it takes the CPU away
       from the one task that has work to do, and that task does not run
       again until the next tick. A handler that asks for another pass, which
       is how the receive poll yields on its budget, was therefore getting
       exactly one pass per tick: 64 frames at 100 Hz, a ceiling of 6600
       packets a second no matter how many arrived. */
    if (Task::TryGetCurrentTask() == CpuStates[cpu].TaskPtr)
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
                if (c != self && CpuStates[c].Pending.TestBit(i))
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

            /* Nothing pending -- sleep briefly to yield CPU */
            Sleep(1 * Const::NanoSecsInMs);
        }
    }
}

}
