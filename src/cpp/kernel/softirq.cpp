#include "softirq.h"
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

void SoftIrq::Raise(ulong type)
{
    if (type >= MaxTypes)
        return;

    ulong cpu = CpuTable::GetInstance().GetCurrentCpuId();
    if (BugOn(cpu >= MaxCpus))
        return;

    if (CpuStates[cpu].Pending.SetBit(type))
        return; /* was already pending */

    /* Scheduling is IPI-driven: without a kick an idle CPU would not
       run its softirq task until the next timer tick IPI. Send the
       IPI directly -- CpuTable::SendIPI takes spinlocks which are not
       safe in hard IRQ context. */
    if (Ready.Get())
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
            /* Nothing pending -- sleep briefly to yield CPU */
            Sleep(1 * Const::NanoSecsInMs);
        }
    }
}

}
