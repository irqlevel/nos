#include "softirq.h"
#include "trace.h"
#include "sched.h"

#include <mm/new.h>
#include <lib/stdlib.h>
#include <include/const.h>

namespace Kernel
{

SoftIrq::SoftIrq()
    : Pending(0)
    , TaskPtr(nullptr)
{
    Stdlib::MemSet(Handlers, 0, sizeof(Handlers));
}

SoftIrq::~SoftIrq()
{
}

bool SoftIrq::Init()
{
    TaskPtr = Mm::TAlloc<Task, Tag>("softirq");
    if (!TaskPtr)
        return false;

    if (!TaskPtr->Start(&SoftIrq::TaskFunc, this))
    {
        TaskPtr->Put();
        TaskPtr = nullptr;
        return false;
    }

    Trace(0, "SoftIrq: initialized");
    return true;
}

void SoftIrq::Stop()
{
    if (TaskPtr)
    {
        TaskPtr->SetStopping();
        TaskPtr->Wait();
        TaskPtr->Put();
        TaskPtr = nullptr;
    }
}

void SoftIrq::Raise(ulong type)
{
    if (type >= MaxTypes)
        return;

    Pending.SetBit(type);
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
    SoftIrq* self = static_cast<SoftIrq*>(ctx);
    self->Run();
}

void SoftIrq::Run()
{
    auto* task = Task::GetCurrentTask();

    while (!task->IsStopping())
    {
        bool handled = false;

        for (ulong i = 0; i < MaxTypes; i++)
        {
            if (Pending.TestBit(i))
            {
                Pending.ClearBit(i);

                if (Handlers[i].Func)
                {
                    Handlers[i].Func(Handlers[i].Ctx);
                    handled = true;
                }
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
