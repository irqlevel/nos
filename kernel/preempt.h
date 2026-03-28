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

/*
 * Save rflags, conditionally disable preemption, disable interrupts.
 * Bit 63 of the returned flags records whether PreemptDisable() was
 * actually called, so PreemptIrqRestore() can balance it correctly
 * even if PreemptIsOn() changes between the two calls.
 */
ulong PreemptIrqSave();
void PreemptIrqRestore(ulong flags);

}