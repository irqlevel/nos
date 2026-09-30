// What the stand-ins for the kernel (kernel.cpp, heap.cpp) tell a target
// about the state an input left behind.
#pragma once

#include <stddef.h>

namespace Fuzz
{

/* Back to the state a process of the runner's starts with: between two
   inputs of a batch, before the target's own Reset. */
void ResetKernel();

/* Spin locks held right now. */
long HeldSpinLocks();

/* A finding if any spin lock is still held: when says at what point. */
void CheckNoLocksHeld(const char* when);

/* The kernel heap (heap.cpp): what is allocated right now, in blocks and
   bytes. A target compares them before and after the work it hands the
   code, for what an error path forgot to give back. */
size_t HeapBlocks();
size_t HeapBytes();

/* From now on, the next n kernel allocations succeed and the ones after
   fail (-1: none fails): for the error paths. */
void HeapFailAfter(long n);

/* No allocation set to fail. What an input left allocated stays so: a
   target counts what its own work leaves, as a difference. */
void HeapReset();

}
