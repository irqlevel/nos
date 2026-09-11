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