#pragma once

#include "stdlib.h"

namespace Kernel
{

void PreemptOn();

void PreemptOnWait();

void PreemptOff();

bool PreemptIsOn();

void PreemptDisable();

void PreemptEnable();

class Task;

/* The preemption half of a spinlock (RawSpinLock::Lock). Disables preemption
   for the current task and returns it, or returns nullptr when there is none
   to disable: preemption not on yet, or a stack that is not a task's (early
   boot, an AP on its way up). The pointer goes back to PreemptEnableTask(),
   because the count belongs to the task that took it -- a lock may be
   released by another task (the scheduler's are, across a context switch),
   and a nullptr from before PreemptOn() stays a no-op after it. */
Task* PreemptDisableTask();
void PreemptEnableTask(Task* task);

/* The task running, as the lock path counts and records it -- PreemptDisable,
   PreemptEnable, a SpinLock's owner: Task::GetCurrentTask(), whose BugOn is
   the tripwire at the bottom of every task's stack and a stack pointer that
   has left its task's stack, until a panic has begun. From then on it is
   nullptr where there is no task: a panic reported from a stack of its own
   -- x86's double fault's, a static one -- takes its locks there, and the
   BugOn would be a panic inside the panic, which only spins: the report
   stopped at its first lock, before the other CPUs' stacks, the netconsole
   and the reset. A panic switches to no task again, so there is nothing to
   count. */
Task* PreemptCurrentTask();

/* May the caller block -- wait for a completion, or Schedule() away? Not
   with interrupts off; and once preemption is on, not off a task stack, and
   not with preemption disabled -- which every spinlock (RawSpinLock,
   SpinLock, RawRwSpinLock) keeps it for as long as it is held. */
bool PreemptCanBlock();

/*
 * Save rflags, conditionally disable preemption, disable interrupts.
 * Bit 63 of the returned flags records whether PreemptDisable() was
 * actually called, so PreemptIrqRestore() can balance it correctly
 * even if PreemptIsOn() changes between the two calls.
 */
ulong PreemptIrqSave();
void PreemptIrqRestore(ulong flags);

}