#pragma once

#include "stdlib.h"

namespace kernel
{

void preempt_on();

void preempt_on_wait();

void preempt_off();

bool preempt_is_on();

void preempt_disable();

void preempt_enable();

class Task;

/* The preemption half of a spinlock (RawSpinLock::lock). Disables preemption
   for the current task and returns it, or returns nullptr when there is none
   to disable: preemption not on yet, or a stack that is not a task's (early
   boot, an AP on its way up). The pointer goes back to preempt_enable_task(),
   because the count belongs to the task that took it -- a lock may be
   released by another task (the scheduler's are, across a context switch),
   and a nullptr from before preempt_on() stays a no-op after it. */
Task* preempt_disable_task();
void preempt_enable_task(Task* task);

/* The task running, as the lock path counts and records it -- preempt_disable,
   preempt_enable, a SpinLock's owner: Task::get_current_task(), whose BUG_ON is
   the tripwire at the bottom of every task's stack and a stack pointer that
   has left its task's stack, until a panic has begun. From then on it is
   nullptr where there is no task: a panic reported from a stack of its own
   -- x86's double fault's, a static one -- takes its locks there, and the
   BUG_ON would be a panic inside the panic, which only spins: the report
   stopped at its first lock, before the other CPUs' stacks, the netconsole
   and the reset. A panic switches to no task again, so there is nothing to
   count. */
Task* preempt_current_task();

/* May the caller block -- wait for a completion, or schedule() away? Not
   with interrupts off; and once preemption is on, not off a task stack, and
   not with preemption disabled -- which every spinlock (RawSpinLock,
   SpinLock, RawRwSpinLock) keeps it for as long as it is held. */
bool preempt_can_block();

/*
 * Save rflags, conditionally disable preemption, disable interrupts.
 * Bit 63 of the returned flags records whether preempt_disable() was
 * actually called, so preempt_irq_restore() can balance it correctly
 * even if preempt_is_on() changes between the two calls.
 */
ulong preempt_irq_save();
void preempt_irq_restore(ulong flags);

}