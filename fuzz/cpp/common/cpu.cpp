// The CPU table, the scheduler and the tasks, for a target whose kernel code
// reaches them: a table of CPUs nobody runs on, and no task to start -- the
// fuzzed code runs in the one there is. What it may not call on here is a
// finding.
#include "host.h"

#include <kernel/cpu.h>
#include <kernel/preempt.h>
#include <kernel/sched.h>
#include <kernel/task.h>

#include "fuzz.h"

namespace kernel
{

Cpu::Cpu()
{
}

Cpu::~Cpu()
{
}

CpuTable::CpuTable()
{
}

CpuTable::~CpuTable()
{
}

TaskQueue::TaskQueue()
{
}

TaskQueue::~TaskQueue()
{
}

/* No task is started here: the fuzzed code runs in the one there is. */
Task::Task(const char* name)
{
    (void)name;
    fuzz::host_hal_unreachable("Task::Task");
}

bool Task::start(Func func, void* ctx)
{
    (void)func;
    (void)ctx;
    fuzz::host_hal_unreachable("Task::start");
}

void Task::get()
{
    fuzz::host_hal_unreachable("Task::get");
}

void Task::put()
{
    fuzz::host_hal_unreachable("Task::put");
}

void sleep(ulong nano_secs)
{
    (void)nano_secs;
    fuzz::host_hal_unreachable("sleep");
}

/* Interrupts off and back, as the kernel's: what a caller that needs them
   off for its CPU's frame slot (PageTable::map_frame_slot) is checked for.
   One task at a time, so there is no preemption to hold off. */
ulong preempt_irq_save()
{
    return hal::irq_save();
}

void preempt_irq_restore(ulong flags)
{
    hal::irq_restore(flags);
}

}
