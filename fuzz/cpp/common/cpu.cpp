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

namespace Kernel
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
Task::Task(const char* fmt, ...)
{
    (void)fmt;
    Fuzz::HostHalUnreachable("Task::Task");
}

bool Task::Start(Func func, void* ctx)
{
    (void)func;
    (void)ctx;
    Fuzz::HostHalUnreachable("Task::Start");
}

void Task::Get()
{
    Fuzz::HostHalUnreachable("Task::Get");
}

void Task::Put()
{
    Fuzz::HostHalUnreachable("Task::Put");
}

void Sleep(ulong nanoSecs)
{
    (void)nanoSecs;
    Fuzz::HostHalUnreachable("Sleep");
}

ulong PreemptIrqSave()
{
    return 0;
}

void PreemptIrqRestore(ulong flags)
{
    (void)flags;
}

}
